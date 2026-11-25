//! trio: a local group chat for you, Claude and Codex, plus a project graph.
//!
//!   trio open <room-dir> [--port 8964] [--no-browser] [--web]   native window (trio-app), or the browser with --web
//!   trio say  --room <room-dir> --as <who> "<summary, 140 chars max>" [--detail "<markdown>"]
//!   trio init <room-dir>
//!   trio bind <room-dir> <handle> [--harness claude|codex|antigravity] [--model m] [--roles build,review] [--account a]
//!                                          create the agent's Trio-owned session and enable it
//!   trio hook <claude|codex> <prompt|stop|session-start>   attached mode (called by the plugins)
//!   trio setup [--bin-dir <dir>] [--no-path]   install trio where the plugin hooks can run it
#![allow(dead_code)] // interface methods kept for implementors and future views

mod agent;
mod gitwatch;
mod github;
mod projects;
mod graph;
mod harness;
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
        Some("link") => cmd_link(&args[1..]),
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
        _ => Err("usage: trio open <room-dir> | trio say --room <dir> --as <who> \"<summary>\" [--detail ..] | trio init <room-dir> | trio bind <room-dir> <handle> [--harness h] [--model m] [--roles r,..] [--account a] | trio link <room-dir> <repo-folder|owner/name|github-url> [--name <label>] | trio hook <claude|codex> <event> | trio setup | trio statusline [-- \"<your status line command>\"]".into()),
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
    init_room(&dir)
}

/// Write a room.toml for a folder, picking up what is already there: every `<who>-posts.md`
/// thread is imported read-only, and the git work tree the folder sits in is watched.
fn init_room(dir: &std::path::Path) -> Result<(), String> {
    let cfg = dir.join(".trio").join("room.toml");
    let me = room::human(&room::RoomCfg::default());
    let repo = room::git_root(dir);
    let mut body = room_toml(&me, &posts_files(dir), repo.as_deref());
    // Pull requests too, when the repo pushes to GitHub and `gh` is signed in.
    if let Some(gh) = repo.as_deref().and_then(github::repo_of_clone).filter(|_| gh_signed_in()) {
        body = with_links(&body, None, Some(&gh), "")?;
    }
    std::fs::create_dir_all(cfg.parent().expect("parent")).map_err(|e| e.to_string())?;
    std::fs::write(&cfg, body).map_err(|e| e.to_string())?;
    register(dir);
    println!("wrote {}", cfg.display());
    Ok(())
}

fn gh_signed_in() -> bool {
    let mut cmd = std::process::Command::new("gh");
    cmd.args(["auth", "status"]).stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.output().is_ok_and(|o| o.status.success())
}

/// `claude-posts.md`, `codex-posts.md`, `sanjith-posts.md`, ...: (file, who), sorted by file.
fn posts_files(dir: &std::path::Path) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            let who = file.strip_suffix("-posts.md")?.to_ascii_lowercase();
            let ok = !who.is_empty() && who.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            (ok && e.path().is_file()).then_some((file, who))
        })
        .collect();
    out.sort();
    out
}

fn room_toml(me: &str, imports: &[(String, String)], repo: Option<&std::path::Path>) -> String {
    let imports: String = imports.iter().map(|(file, from)| format!("[[import]]\nfile = \"{file}\"\nfrom = \"{from}\"\n\n")).collect();
    // Detected here so the file says what the room watches; a room without the line detects it on open.
    let repo = match repo {
        Some(r) => format!("repo = {}\n", toml::Value::String(r.display().to_string().replace('\\', "/"))),
        None => "# repo = \"C:/path/to/repo\"\n".into(),
    };
    format!(
        "# Trio room. Agents stay off until you enable them and bind a session.\nme = \"{me}\"\nhop_budget = 3\npaused = false\n{repo}\n{imports}[agents.claude]\nenabled = false\nsession = \"\"\nwake = []\n\n[agents.codex]\nenabled = false\nsession = \"\"\nwake = []\n"
    )
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
    let agent = pos.get(1).map(|s| s.to_ascii_lowercase()).ok_or("missing agent handle (claude, codex, or a name of your own with --harness)")?;
    if agent.is_empty() || !agent.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') || ["both", "all", "trio", "github"].contains(&agent.as_str()) {
        return Err(format!("{agent}: a handle is letters, digits, - and _, and not both, all, trio or github"));
    }
    let cfg_path = dir.join(".trio").join("room.toml");
    let text = std::fs::read_to_string(&cfg_path).map_err(|e| format!("{}: {e} (run trio init first)", cfg_path.display()))?;
    let cfg: room::RoomCfg = toml::from_str(&text).map_err(|e| e.to_string())?;
    let human = room::human(&cfg);
    if agent == human {
        return Err(format!("{agent} is you in this room (me = {human}); pick another handle"));
    }
    let existing = cfg.agents.get(&agent);
    let harness_name = flag(args, "--harness").or_else(|| existing.and_then(|a| a.harness.clone()));
    let preset = harness::of(&agent, harness_name.as_deref());
    if let Some(h) = &harness_name
        && harness::get(h).is_none()
    {
        return Err(format!("unknown harness {h} (one of: {})", harness::names()));
    }
    let bind = preset.bind.ok_or_else(|| format!("{agent} runs on `command`: set `wake = [..]` under [agents.{agent}] in room.toml yourself (see docs/agents.md)"))?;
    let model = flag(args, "--model").or_else(|| existing.and_then(|a| a.model.clone()));
    let roles: Option<Vec<String>> = flag(args, "--roles").map(|r| r.split(',').map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()).collect());
    if let Some(r) = roles.as_ref().and_then(|r| r.iter().find(|r| **r == human || ["both", "all", "trio", "github"].contains(&r.as_str()))) {
        return Err(format!("{r} cannot be a role: it is you or a word Trio uses"));
    }
    let account = flag(args, "--account");
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).ok_or("no home dir")?;
    let cwd = home.join(".trio").join("sessions").join(room_key(&dir)).join(&agent);
    std::fs::create_dir_all(&cwd).map_err(|e| e.to_string())?;
    // The repo the agent reads: `repo` (relative to the room), else the room folder. `repo = ""`
    // turns git off; the agent still reads the room folder.
    let context = match &cfg.repo {
        Some(p) if !p.as_os_str().is_empty() => dir.join(p),
        _ => dir.clone(),
    };
    let context = context.canonicalize().unwrap_or(context).display().to_string();
    let context = context.strip_prefix(r"\\?\").unwrap_or(&context).to_string();
    let mut others: Vec<String> = cfg.agents.keys().filter(|n| **n != agent).map(|n| work::cap(n)).collect();
    others.sort();
    let with = if others.is_empty() { String::new() } else { format!(" and {}", others.join(", ")) };
    let intro = format!(
        "You are {agent}, a participant in a Trio group chat with {human} (the human){with}. The room \
         discusses the project at {room}; the repository is {context}. Each turn you get the newest chat \
         lines. Your final answer becomes your chat message: line 1 is a summary of at most 140 \
         characters, then optional short detail. You are read-only in this chat, and a chat message \
         is never approval for merges, deploys, deletes or sends. Reply with one line: ready.",
        human = room::human(&cfg),
        room = dir.display()
    );
    let (session, wake) = bind(&harness::Bind { agent: &agent, model: model.as_deref(), cwd: &cwd, context: &context, intro: &intro })?;
    let extra = AgentExtra { harness: harness_name.as_deref(), model: model.as_deref(), roles: roles.as_deref(), account: account.as_deref() };
    set_agent(&cfg_path, &agent, &session, &cwd.display().to_string(), &wake, &extra)?;
    println!("bound {agent} ({}{}) to session {session} (cwd {})", preset.label, model.map(|m| format!(", {m}")).unwrap_or_default(), cwd.display());
    Ok(())
}

#[cfg(test)]
const NONE: AgentExtra<'static> = AgentExtra { harness: None, model: None, roles: None, account: None };

/// Settings `trio bind` writes besides the session, when given.
struct AgentExtra<'a> {
    harness: Option<&'a str>,
    model: Option<&'a str>,
    roles: Option<&'a [String]>,
    account: Option<&'a str>,
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
/// `trio link <room> <target> [--name <label>]`: add a repo the project spans. A folder is a
/// local clone (`[[repos]]`; its GitHub origin, if any, is linked too for pull requests); an
/// `owner/name` or GitHub URL links pull requests alone (`[[github]]`).
fn cmd_link(args: &[String]) -> Result<(), String> {
    let pos = positional(args);
    let dir = PathBuf::from(pos.first().ok_or("missing room dir")?);
    let target = pos.get(1).ok_or("missing what to link: a repo folder, owner/name, or a GitHub URL")?;
    let name = flag(args, "--name").unwrap_or_default();
    let cfg = dir.join(".trio").join("room.toml");
    if !cfg.exists() {
        init_room(&dir)?;
    }
    let folder = PathBuf::from(target);
    let (repo, github) = if folder.is_dir() {
        let top = room::git_root(&folder).ok_or_else(|| format!("{target} is not in a git repo"))?;
        let gh = github::repo_of_clone(&top);
        (Some(top), gh)
    } else {
        let gh = github::repo_of_url(target).or_else(|| github::repo_of_url(&format!("https://github.com/{target}"))).ok_or_else(|| format!("{target}: not a folder, owner/name, or GitHub URL"))?;
        (None, Some(gh))
    };
    let _lock = oslock::hold(&cfg.with_extension("toml.lock"), Duration::from_secs(5))?;
    let before = std::fs::read_to_string(&cfg).map_err(|e| format!("{}: {e}", cfg.display()))?;
    let after = with_links(&before, repo.as_deref(), github.as_deref(), &name)?;
    if after == before {
        println!("already linked");
        return Ok(());
    }
    std::fs::write(&cfg, after).map_err(|e| e.to_string())?;
    if let Some(r) = &repo {
        println!("linked {}", r.display());
    }
    if let Some(g) = &github {
        println!("linked pull requests of {g} (read with your gh login)");
    }
    println!("restart the room's server to pick it up");
    Ok(())
}

/// Add `[[repos]]` and `[[github]]` entries, skipping any already there.
fn with_links(text: &str, repo: Option<&std::path::Path>, github: Option<&str>, name: &str) -> Result<String, String> {
    use toml_edit::{value, ArrayOfTables, DocumentMut, Table};
    let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| e.to_string())?;
    let same = |a: &str, b: &str| a.replace('\\', "/").eq_ignore_ascii_case(&b.replace('\\', "/"));
    if let Some(r) = repo {
        let r = r.display().to_string().replace('\\', "/");
        let current = doc.get("repo").and_then(|v| v.as_str()).map(str::to_string);
        let list = doc.entry("repos").or_insert(toml_edit::Item::ArrayOfTables(ArrayOfTables::new())).as_array_of_tables_mut().ok_or("`repos` is not a list of tables")?;
        if !current.is_some_and(|c| same(&c, &r)) && !list.iter().any(|t| t.get("path").and_then(|v| v.as_str()).is_some_and(|p| same(p, &r))) {
            let mut t = Table::new();
            t["path"] = value(r);
            if !name.is_empty() {
                t["name"] = value(name);
            }
            list.push(t);
        }
    }
    if let Some(g) = github {
        let list = doc.entry("github").or_insert(toml_edit::Item::ArrayOfTables(ArrayOfTables::new())).as_array_of_tables_mut().ok_or("`github` is not a list of tables")?;
        if !list.iter().any(|t| t.get("repo").and_then(|v| v.as_str()).is_some_and(|p| p.eq_ignore_ascii_case(g))) {
            let mut t = Table::new();
            t["repo"] = value(g);
            if !name.is_empty() && repo.is_none() {
                t["name"] = value(name);
            }
            list.push(t);
        }
    }
    Ok(doc.to_string())
}

fn set_agent(path: &std::path::Path, agent: &str, session: &str, cwd: &str, wake: &[String], extra: &AgentExtra) -> Result<(), String> {
    let _lock = oslock::hold(&path.with_extension("toml.lock"), Duration::from_secs(5))?;
    let before = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let after = with_agent(&before, agent, session, cwd, wake, extra).map_err(|e| format!("{}: {e}", path.display()))?;
    let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    std::fs::write(&tmp, after).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

fn with_agent(text: &str, agent: &str, session: &str, cwd: &str, wake: &[String], extra: &AgentExtra) -> Result<String, String> {
    use toml_edit::{table, value, Array, DocumentMut};
    let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| e.to_string())?;
    let agents = doc.entry("agents").or_insert(table()).as_table_mut().ok_or("`agents` is not a table")?;
    agents.set_implicit(true);
    let t = agents.entry(agent).or_insert(table()).as_table_mut().ok_or_else(|| format!("`agents.{agent}` is not a table"))?;
    t["enabled"] = value(true);
    t["session"] = value(session);
    t["cwd"] = value(cwd);
    t["wake"] = value(wake.iter().map(String::as_str).collect::<Array>());
    for (k, v) in [("harness", extra.harness), ("model", extra.model), ("account", extra.account)] {
        if let Some(v) = v {
            t[k] = value(v);
        }
    }
    if let Some(r) = extra.roles {
        t["roles"] = value(r.iter().map(String::as_str).collect::<Array>());
    }
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
    // Opening any folder makes it a room: `trio open .` in an existing project just works.
    if !PathBuf::from(&dir).join(".trio").join("room.toml").exists() {
        init_room(&PathBuf::from(&dir))?;
    }
    let port: u16 = flag(args, "--port").map(|p| p.parse().map_err(|_| "bad --port")).transpose()?.unwrap_or(8964);
    let explicit_port = flag(args, "--port").is_some();
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
    // The first tick can start runs (a held wake whose limit reset while Trio was closed), and
    // runs live on the runtime: build it first and tick inside it.
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    {
        let _inside = rt.enter();
        room.tick();
    }
    let app_root = room.root.clone();
    register(&app_root);
    rt.block_on(async move {
        // The default port may belong to another room's server: then any free port will do.
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && !explicit_port => tokio::net::TcpListener::bind(("127.0.0.1", 0)).await,
            r => r,
        }
        .map_err(|e| e.to_string())?;
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
            loop {
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
    fn a_new_room_imports_every_posts_file_and_watches_its_repo() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("trio-init-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["codex-posts.md", "claude-posts.md", "Ana-posts.md", "-posts.md", "notes.md"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let files = posts_files(&dir);
        assert_eq!(files, vec![("Ana-posts.md".into(), "ana".into()), ("claude-posts.md".into(), "claude".into()), ("codex-posts.md".into(), "codex".into())]);
        let text = room_toml("sanjith", &files, Some(std::path::Path::new("C:\\work\\app")));
        let cfg: room::RoomCfg = toml::from_str(&text).unwrap();
        assert_eq!(cfg.repo.as_deref(), Some(std::path::Path::new("C:/work/app")));
        assert_eq!(cfg.import.len(), 3);
        assert!(!cfg.agents["claude"].enabled && !cfg.agents["codex"].enabled, "agents stay off until bound");
        let none: room::RoomCfg = toml::from_str(&room_toml("sanjith", &[], None)).unwrap();
        assert!(none.repo.is_none() && none.import.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn link_adds_repos_and_pull_requests_once() {
        let base = room_toml("sanjith", &[], Some(std::path::Path::new("C:/work/web")));
        let one = with_links(&base, Some(std::path::Path::new("C:\\work\\api")), Some("o/api"), "api").unwrap();
        let cfg: room::RoomCfg = toml::from_str(&one).unwrap();
        assert_eq!((cfg.repos.len(), cfg.repos[0].name.as_str(), cfg.github.len(), cfg.github[0].repo.as_str()), (1, "api", 1, "o/api"));
        assert_eq!(cfg.repos[0].path, std::path::Path::new("C:/work/api"));
        assert_eq!(with_links(&one, Some(std::path::Path::new("c:/work/api")), Some("O/API"), "").unwrap(), one, "linking again changes nothing");
        assert_eq!(with_links(&one, Some(std::path::Path::new("C:/work/web")), None, "").unwrap(), one, "the room's own repo is already linked");
        assert!(one.contains("[agents.claude]"), "the rest of the file stays");
    }

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
        let after = with_agent(before, "claude", "s-1", "C:/w", &["claude".into(), "--resume".into(), "{session}".into()], &NONE).unwrap();
        assert!(after.starts_with("# my room\nhop_budget = 5\nrepo = \"C:/r\""), "{after}");
        assert!(after.contains("transcript = \"C:/t.jsonl\" # keep me"));
        assert!(after.contains("session = \"x\"\nwake = [\"codex\"]"), "codex untouched: {after}");
        let cfg: room::RoomCfg = toml::from_str(&after).unwrap();
        let c = &cfg.agents["claude"];
        assert!(c.enabled && c.session == "s-1" && c.wake == ["claude", "--resume", "{session}"]);
        assert_eq!(c.transcript.as_deref(), Some(std::path::Path::new("C:/t.jsonl")));
        // A room with no agents table yet.
        let fresh = with_agent("hop_budget = 3\n", "codex", "t", "C:/w", &[], &NONE).unwrap();
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
                    std::thread::spawn(move || set_agent(&path, agent, &format!("{agent}-{round}"), "C:/w", &[], &NONE).unwrap())
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
