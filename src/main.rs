//! trio: a local group chat for you, Claude and Codex, plus a project graph.
//!
//!   trio open <room-dir> [--port 8964] [--no-browser] [--web]   native window (trio-app), or the browser with --web
//!   trio say  --room <room-dir> --as <who> "<summary, 140 chars max>" [--detail "<markdown>"]
//!   trio init <room-dir>
//!   trio bind <room-dir> <claude|codex>   create the agent's Trio-owned session and enable it
//!   trio hook <claude|codex> <prompt|stop|session-start>   attached mode (called by the plugins)
//!   trio setup [--bin-dir <dir>] [--no-path]   install trio where the plugin hooks can run it
#![allow(dead_code)] // interface methods kept for implementors and future views

mod agent;
mod gitwatch;
mod graph;
mod hook;
mod oslock;
mod proctree;
mod model;
mod room;
mod roomid;
mod server;
mod setup;
mod source;
mod seen;
mod usage;
mod work;

use std::hash::{BuildHasher, RandomState};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("open") => cmd_open(&args[1..]),
        Some("say") => cmd_say(&args[1..]),
        Some("init") => cmd_init(&args[1..]),
        Some("bind") => cmd_bind(&args[1..]),
        Some("setup") => setup::cmd_setup(&args[1..]),
        // Claude Code's status line: never fail it, whatever happens.
        Some("statusline") => {
            cmd_statusline(&args[1..]);
            Ok(())
        }
        Some("hook") => match (args.get(1), args.get(2)) {
            (Some(s), Some(e)) => hook::run(s, e).or_else(|err| {
                // Never fail the user's turn over Trio: report and exit 0.
                eprintln!("trio: {err}");
                Ok(())
            }),
            _ => Err("usage: trio hook <claude|codex> <prompt|stop|session-start>".into()),
        },
        _ => Err("usage: trio open <room-dir> | trio say --room <dir> --as <who> \"<summary>\" [--detail ..] | trio init <room-dir> | trio bind <room-dir> <claude|codex> | trio hook <claude|codex> <event> | trio setup | trio statusline [-- \"<your status line command>\"]".into()),
    };
    if let Err(e) = code {
        eprintln!("trio: {e}");
        std::process::exit(2);
    }
}

/// Claude Code's `statusLine` command: copy the documented `rate_limits` from the status-line
/// JSON to `~/.trio/usage/claude.json`, where rooms read Claude's windows. With
/// `-- "<command>"` the existing status line runs on the same input and its output is shown;
/// without one, a short window summary is.
fn cmd_statusline(args: &[String]) {
    use std::io::{Read, Write};
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let v: Option<serde_json::Value> = serde_json::from_str(&input).ok();
    let limits = v.as_ref().and_then(|v| v.get("rate_limits")).cloned();
    if let (Some(rate_limits), Some(path)) = (limits.clone(), usage::claude_file()) {
        let copy = usage::ClaudeCopy { at: usage::epoch_now(), rate_limits };
        if let (Some(dir), Ok(bytes)) = (path.parent(), serde_json::to_vec(&copy)) {
            let tmp = path.with_extension("json.tmp");
            let _ = std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&tmp, bytes)).and_then(|_| std::fs::rename(&tmp, &path));
        }
    }
    if let Some(cmd) = args.iter().position(|a| a == "--").and_then(|i| args.get(i + 1..)).filter(|c| !c.is_empty()) {
        let line = cmd.join(" ");
        #[cfg(windows)]
        let mut c = std::process::Command::new("cmd");
        #[cfg(windows)]
        c.args(["/C", &line]);
        #[cfg(not(windows))]
        let mut c = std::process::Command::new("sh");
        #[cfg(not(windows))]
        c.args(["-c", &line]);
        if let Ok(mut child) = c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn() {
            if let Some(mut si) = child.stdin.take() {
                let _ = si.write_all(input.as_bytes());
            }
            if let Ok(out) = child.wait_with_output() {
                let _ = std::io::stdout().write_all(&out.stdout);
            }
        }
        return;
    }
    let windows = limits.as_ref().map(usage::claude_windows).unwrap_or_default();
    let text: Vec<String> = windows.iter().map(|w| format!("{} {:.0}%", w.label, w.used)).collect();
    println!("{}", if text.is_empty() { "trio".to_string() } else { text.join(" \u{b7} ") });
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn positional(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i].starts_with("--") {
            i += if args[i] == "--no-browser" || args[i] == "--web" { 1 } else { 2 };
        } else {
            out.push(args[i].clone());
            i += 1;
        }
    }
    out
}

/// 128 bits from two independently keyed std hashers; no RNG crate needed.
fn token() -> String {
    let a = RandomState::new().hash_one(std::process::id());
    let b = RandomState::new().hash_one(std::time::SystemTime::now());
    format!("{a:016x}{b:016x}")
}

fn cmd_say(args: &[String]) -> Result<(), String> {
    let dir = flag(args, "--room").ok_or("--room <dir> is required")?;
    let who = flag(args, "--as").ok_or("--as <claude|codex|your name> is required")?;
    let text = positional(args).into_iter().next().ok_or("missing summary text")?;
    let room = room::Room::open(&PathBuf::from(dir)).map_err(|e| e.to_string())?;
    room.say(&who.to_ascii_lowercase(), &text, flag(args, "--detail"))
}

fn cmd_init(args: &[String]) -> Result<(), String> {
    let dir = PathBuf::from(positional(args).into_iter().next().ok_or("missing room dir")?);
    let cfg = dir.join(".trio").join("room.toml");
    if cfg.exists() {
        return Err(format!("{} already exists", cfg.display()));
    }
    let mut imports = String::new();
    let me = room::human(&room::RoomCfg::default());
    let mine = format!("{me}-posts.md");
    for (file, from) in [("claude-posts.md", "claude"), ("codex-posts.md", "codex"), (mine.as_str(), me.as_str())] {
        if dir.join(file).exists() {
            imports.push_str(&format!("[[import]]\nfile = \"{file}\"\nfrom = \"{from}\"\n\n"));
        }
    }
    let body = format!(
        "# Trio room. Agents stay off until you enable them and bind a session.\nme = \"{me}\"\nhop_budget = 3\npaused = false\n# repo = \"C:/path/to/repo\"\n\n{imports}[agents.claude]\nenabled = false\nsession = \"\"\nwake = []\n\n[agents.codex]\nenabled = false\nsession = \"\"\nwake = []\n"
    );
    std::fs::create_dir_all(cfg.parent().expect("parent")).map_err(|e| e.to_string())?;
    std::fs::write(&cfg, body).map_err(|e| e.to_string())?;
    register(&dir);
    println!("wrote {}", cfg.display());
    Ok(())
}

/// Create the agent's Trio-owned session and write its binding into room.toml.
///
/// The session lives outside any workspace (~/.trio/sessions/<room>/<agent>): the desktop apps
/// never open it, so resuming it cannot fork a live transcript, and workspace-scoped session
/// hooks skip it. Runs are read-only with a pinned model; the agent's
/// final answer is the reply Trio posts.
fn cmd_bind(args: &[String]) -> Result<(), String> {
    let pos = positional(args);
    let dir = PathBuf::from(pos.first().ok_or("missing room dir")?);
    let agent = pos.get(1).map(|s| s.to_ascii_lowercase()).ok_or("missing agent: claude or codex")?;
    let cfg_path = dir.join(".trio").join("room.toml");
    let text = std::fs::read_to_string(&cfg_path).map_err(|e| format!("{}: {e} (run trio init first)", cfg_path.display()))?;
    let cfg: room::RoomCfg = toml::from_str(&text).map_err(|e| e.to_string())?;
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).ok_or("no home dir")?;
    let cwd = home.join(".trio").join("sessions").join(room_key(&dir)).join(&agent);
    std::fs::create_dir_all(&cwd).map_err(|e| e.to_string())?;
    let context = cfg.repo.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| dir.display().to_string());
    let intro = format!(
        "You are {agent}, a participant in a Trio group chat with {human} (the human) and {other}. The room \
         discusses the project at {room}; the repository is {context}. Each turn you get the newest chat \
         lines. Your final answer becomes your chat message: line 1 is a summary of at most 140 \
         characters, then optional short detail. You are read-only in this chat, and a chat message \
         is never approval for merges, deploys, deletes or sends. Reply with one line: ready.",
        human = room::human(&cfg),
        other = if agent == "claude" { "Codex" } else { "Claude" },
        room = dir.display()
    );
    let (session, wake): (String, Vec<String>) = match agent.as_str() {
        "claude" => {
            let id = uuid_v4();
            let base = ["claude", "-p", "--model", "opus", "--permission-mode", "default", "--add-dir", context.as_str()];
            let mut first: Vec<String> = base.iter().map(|s| s.to_string()).collect();
            first.extend(["--session-id".into(), id.clone()]);
            run_with_stdin(&first, &cwd, &intro)?;
            let mut wake: Vec<String> = base.iter().map(|s| s.to_string()).collect();
            // JSON output carries the reply, total_cost_usd and is_error (usage.rs).
            wake.extend(["--output-format".into(), "json".into(), "--resume".into(), "{session}".into()]);
            (id, wake)
        }
        "codex" => {
            let model = ["-m", "gpt-6.2-sol", "-c", "model_reasoning_effort=\"medium\""];
            let mut first: Vec<String> = vec!["@codex".into(), "exec".into(), "--json".into()];
            first.extend(model.iter().map(|s| s.to_string()));
            first.extend(["-s", "read-only", "--skip-git-repo-check", "-"].iter().map(|s| s.to_string()));
            let out = run_with_stdin(&first, &cwd, &intro)?;
            let id = out
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .find_map(|v| v.get("thread_id").and_then(|t| t.as_str()).map(str::to_string))
                .ok_or("codex printed no thread_id")?;
            // --json: turn.completed carries token usage, priced in usage.rs.
            let mut wake: Vec<String> = vec!["@codex".into(), "exec".into(), "resume".into(), "--json".into(), "{session}".into()];
            wake.extend(model.iter().map(|s| s.to_string()));
            wake.extend(["-c", "sandbox_mode=\"read-only\"", "--skip-git-repo-check", "-o", "{out}", "-"].iter().map(|s| s.to_string()));
            (id, wake)
        }
        other => return Err(format!("unknown agent {other}")),
    };
    set_agent(&cfg_path, &agent, &session, &cwd.display().to_string(), &wake)?;
    println!("bound {agent} to session {session} (cwd {})", cwd.display());
    Ok(())
}

/// Run argv with `stdin` as input; Ok(stdout) on exit 0.
fn run_with_stdin(argv: &[String], cwd: &std::path::Path, stdin: &str) -> Result<String, String> {
    use std::io::Write;
    let bin = agent::resolve_bin(&argv[0])?;
    let mut child = std::process::Command::new(&bin)
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    child.stdin.take().expect("stdin").write_all(stdin.as_bytes()).map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{} exited {:?}: {}", argv[0], out.status.code(), err.lines().rev().take(3).collect::<Vec<_>>().join(" | ")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Session dir key for a room: its folder name plus a hash of its full canonical path, so two
/// rooms with the same folder name never share sessions or reply files.
fn room_key(dir: &std::path::Path) -> String {
    let canon = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let name = canon.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "room".into());
    // FNV-1a 64: stable across builds and platforms (std's hasher makes no such promise).
    let h = canon.to_string_lossy().bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
    format!("{name}-{:08x}", h >> 32)
}

/// Set enabled/session/cwd/wake under `[agents.<agent>]` in room.toml, keeping every other key,
/// table and comment. Writers hold an OS lock on `room.toml.lock` for the whole read-edit-rename,
/// so two binds at once apply one after the other and neither edit is lost.
fn set_agent(path: &std::path::Path, agent: &str, session: &str, cwd: &str, wake: &[String]) -> Result<(), String> {
    let _lock = oslock::hold(&path.with_extension("toml.lock"), Duration::from_secs(5))?;
    let before = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let after = with_agent(&before, agent, session, cwd, wake).map_err(|e| format!("{}: {e}", path.display()))?;
    let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    std::fs::write(&tmp, after).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

fn with_agent(text: &str, agent: &str, session: &str, cwd: &str, wake: &[String]) -> Result<String, String> {
    use toml_edit::{table, value, Array, DocumentMut};
    let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| e.to_string())?;
    let agents = doc.entry("agents").or_insert(table()).as_table_mut().ok_or("`agents` is not a table")?;
    agents.set_implicit(true);
    let t = agents.entry(agent).or_insert(table()).as_table_mut().ok_or_else(|| format!("`agents.{agent}` is not a table"))?;
    t["enabled"] = value(true);
    t["session"] = value(session);
    t["cwd"] = value(cwd);
    t["wake"] = value(wake.iter().map(String::as_str).collect::<Array>());
    Ok(doc.to_string())
}

/// Open the room in the native window (`trio-app` next to this exe). False when it is missing
/// or would not start, so the caller can fall back to the browser.
fn open_app(room: &std::path::Path) -> bool {
    let name = format!("trio-app{}", std::env::consts::EXE_SUFFIX);
    let Some(app) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(&name))).filter(|p| p.is_file()) else { return false };
    std::process::Command::new(app)
        .arg(room)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

fn server_lock_path(root: &std::path::Path) -> PathBuf {
    root.join(".trio").join("server.lock")
}

/// The URL of the server that `server.json` names, if it answers as this room's own server
/// (a poke carrying this room's id gets 204; another room's server answers 403).
fn running_url(root: &std::path::Path) -> Option<String> {
    hook::poke(root).map(|port| format!("http://127.0.0.1:{port}/"))
}

/// Open `url` in the default browser. Best effort.
fn open_browser(url: &str) {
    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else {
        std::process::Command::new("xdg-open")
    };
    let _ = cmd.arg(url).spawn();
}

/// Remember the room so `trio <name>` finds it from any session. Best effort.
fn register(dir: &std::path::Path) {
    if let Ok(store) = hook::Store::from_env() {
        let _ = store.register_room(dir);
    }
}

fn uuid_v4() -> String {
    let s = token(); // 32 hex chars
    format!("{}-{}-4{}-a{}-{}", &s[0..8], &s[8..12], &s[13..16], &s[17..20], &s[20..32])
}

fn cmd_open(args: &[String]) -> Result<(), String> {
    let dir = positional(args).into_iter().next().ok_or("missing room dir")?;
    let port: u16 = flag(args, "--port").map(|p| p.parse().map_err(|_| "bad --port")).transpose()?.unwrap_or(8964);
    let no_browser = args.iter().any(|a| a == "--no-browser");
    let web = args.iter().any(|a| a == "--web");
    let mut room = room::Room::open(&PathBuf::from(&dir)).map_err(|e| format!("{dir}: {e}"))?;
    // One server per room, held for this process's lifetime and taken before the first tick:
    // two servers on one ledger would both route each message and wake agents twice.
    let Some(_serving) = oslock::try_hold(&server_lock_path(&room.root))? else {
        let url = running_url(&room.root).ok_or("another trio server is starting for this room; try again in a moment")?;
        println!("trio: {url} (already running)");
        if !no_browser && (web || !open_app(&room.root)) {
            open_browser(&url);
        }
        return Ok(());
    };
    room.tick();
    let app_root = room.root.clone();
    register(&app_root);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        // Hooks read this to poke the server after they append (see hook::poke).
        let info = hook::ServerInfo { port, pid: std::process::id() };
        let root = app_root.clone();
        let _ = std::fs::write(hook::server_info_path(&root), serde_json::to_vec(&info).unwrap_or_default());
        let (tx, _) = tokio::sync::broadcast::channel(16);
        let app = Arc::new(server::App {
            room: Mutex::new(room),
            token: token(),
            hosts: vec![format!("127.0.0.1:{port}"), format!("localhost:{port}")],
            tx,
            wake: tokio::sync::Notify::new(),
            room_id: hook::room_id(&root),
            snapshot: Mutex::new(None),
        });
        let ticker = app.clone();
        // Idle unless used: poll only while a visible page is connected or an agent run is live;
        // otherwise sleep until a page connects or a message arrives (no stat, no git, no CPU).
        tokio::spawn(async move {
            // A visit is the time a page or window holds the change stream. When one of at least
            // a minute ends, the seen mark moves there: the next visit opens on what changed since.
            let mut visit: Option<std::time::Instant> = None;
            loop {
                let watching = ticker.tx.receiver_count() > 0;
                match (watching, visit) {
                    (true, None) => visit = Some(std::time::Instant::now()),
                    (false, Some(started)) => {
                        visit = None;
                        if started.elapsed().as_secs() >= seen::VISIT_SECS {
                            let rev = {
                                let mut r = ticker.room.lock().expect("room lock");
                                r.mark_seen();
                                r.rev
                            };
                            let _ = ticker.tx.send(rev);
                        }
                    }
                    _ => {}
                }
                if ticker.active() {
                    tokio::time::sleep(Duration::from_millis(700)).await;
                } else {
                    ticker.wake.notified().await;
                }
                let changed = {
                    let mut r = ticker.room.lock().expect("room lock");
                    r.tick().then_some(r.rev)
                };
                if let Some(rev) = changed {
                    let _ = ticker.tx.send(rev);
                }
            }
        });
        let url = format!("http://127.0.0.1:{port}/");
        println!("trio: {url}");
        if !no_browser && (web || !open_app(&root)) {
            open_browser(&url);
        }
        axum::serve(listener, server::router(app)).await.map_err(|e| e.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rooms_with_one_folder_name_get_separate_session_keys() {
        let a = room_key(std::path::Path::new("Z:/one/refactor"));
        let b = room_key(std::path::Path::new("Z:/two/refactor"));
        assert!(a.starts_with("refactor-") && b.starts_with("refactor-"));
        assert_ne!(a, b);
        assert_eq!(a, room_key(std::path::Path::new("Z:/one/refactor")));
    }

    #[test]
    fn bind_edit_keeps_other_settings_and_comments() {
        let before = "# my room\nhop_budget = 5\nrepo = \"C:/r\"\n\n[agents.claude]\nenabled = false\nsession = \"\"\ntranscript = \"C:/t.jsonl\" # keep me\nwake = []\n\n[agents.codex]\nenabled = true\nsession = \"x\"\nwake = [\"codex\"]\n";
        let after = with_agent(before, "claude", "s-1", "C:/w", &["claude".into(), "--resume".into(), "{session}".into()]).unwrap();
        assert!(after.starts_with("# my room\nhop_budget = 5\nrepo = \"C:/r\""), "{after}");
        assert!(after.contains("transcript = \"C:/t.jsonl\" # keep me"));
        assert!(after.contains("session = \"x\"\nwake = [\"codex\"]"), "codex untouched: {after}");
        let cfg: room::RoomCfg = toml::from_str(&after).unwrap();
        let c = &cfg.agents["claude"];
        assert!(c.enabled && c.session == "s-1" && c.wake == ["claude", "--resume", "{session}"]);
        assert_eq!(c.transcript.as_deref(), Some(std::path::Path::new("C:/t.jsonl")));
        // A room with no agents table yet.
        let fresh = with_agent("hop_budget = 3\n", "codex", "t", "C:/w", &[]).unwrap();
        assert!(toml::from_str::<room::RoomCfg>(&fresh).unwrap().agents["codex"].enabled);
    }

    #[test]
    fn concurrent_binds_both_land() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("trio-bind-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("room.toml");
        std::fs::write(&path, "hop_budget = 3\n").unwrap();
        for round in 0..20 {
            let handles: Vec<_> = ["claude", "codex"]
                .into_iter()
                .map(|agent| {
                    let path = path.clone();
                    std::thread::spawn(move || set_agent(&path, agent, &format!("{agent}-{round}"), "C:/w", &[]).unwrap())
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
            let cfg: room::RoomCfg = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(cfg.agents["claude"].session, format!("claude-{round}"), "round {round}");
            assert_eq!(cfg.agents["codex"].session, format!("codex-{round}"), "round {round}");
        }
        assert!(oslock::hold(&path.with_extension("toml.lock"), Duration::ZERO).is_ok(), "the lock outlived its writers");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_second_server_for_a_room_defers_to_the_first() {
        let dir = std::env::temp_dir().join(format!("trio-serve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".trio")).unwrap();
        let held = oslock::try_hold(&server_lock_path(&dir)).unwrap().expect("free lock");
        // A stand-in for the running server answers the identity poke with `status`: 204 is this
        // room's server, 403 another room's (server.json is stale and its port was reused).
        let second_open = |reply: &'static str| {
            use std::io::{Read, Write};
            let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let info = hook::ServerInfo { port: first.local_addr().unwrap().port(), pid: 1 };
            std::fs::write(hook::server_info_path(&dir), serde_json::to_vec(&info).unwrap()).unwrap();
            std::thread::spawn(move || {
                if let Some(Ok(mut s)) = first.incoming().next() {
                    let mut buf = [0u8; 512];
                    let _ = s.read(&mut buf);
                    let _ = s.write_all(reply.as_bytes());
                }
            });
            let (tx, rx) = std::sync::mpsc::channel();
            let args = vec![dir.display().to_string(), "--no-browser".into(), "--port".into(), "0".into()];
            std::thread::spawn(move || tx.send(cmd_open(&args)));
            rx.recv_timeout(Duration::from_secs(5)).expect("a second server started instead of deferring to the first")
        };
        let ours = second_open("HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n");
        assert!(ours.is_ok(), "{ours:?}");
        let theirs = second_open("HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\n\r\n");
        assert!(theirs.is_err(), "pointed at another room's server");
        let cut = second_open("HTTP/1.1 204 No Content\r\n");
        assert!(cut.is_err(), "accepted an incomplete 204");
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
