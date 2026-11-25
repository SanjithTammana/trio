//! Participants and agents.
//!
//!   Entity <- Participant <- Agent
//! `Agent::wake_if_allowed` is the inherited template method: it runs every guard in order and
//! only then calls the implementor's `spawn`. Delivery itself is a `Waker` (strategy), so a new
//! harness route (Claude channels, Codex app-server, a plain command) is one more impl.

use crate::model::Entity;
use futures_util::FutureExt;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Bytes of a reply kept from stdout; the rest is drained and dropped so the child never blocks.
const OUT_CAP: u64 = 64 * 1024;

pub trait Participant: Entity {
    fn handle(&self) -> &str;
    /// Identity colour token name the UI maps to its palette.
    fn hue(&self) -> &'static str;
}

#[allow(dead_code)] // the human participant; used once Trio lists members
pub struct Human {
    pub name: String,
}
impl Entity for Human {
    fn id(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> &'static str {
        "human"
    }
}
impl Participant for Human {
    fn handle(&self) -> &str {
        &self.name
    }
    fn hue(&self) -> &'static str {
        "ink"
    }
}

/// Why a wake did or did not happen; recorded so the chat can show it.
#[derive(Debug, PartialEq, Eq)]
pub enum WakeDecision {
    Started,
    Disabled,
    Paused,
    Busy,
    Running,
    OverBudget { hop: u32, budget: u32 },
    Failed(String),
}

pub struct Guard {
    pub paused: bool,
    pub hop: u32,
    pub budget: u32,
    pub from_human: bool,
}

pub trait Agent: Participant {
    fn binding(&self) -> &AgentConfig;
    fn mirror(&self) -> &dyn Mirror;
    fn is_running(&self) -> bool;
    /// Start the run. Implementors only deliver; guards live in `wake_if_allowed`.
    fn spawn(&mut self, prompt: &str) -> Result<(), String>;

    /// Whether a wake would start now, guards aside: enabled, idle, and its transcript free.
    fn ready(&self) -> bool {
        self.binding().enabled && !self.is_running() && !self.mirror().busy()
    }

    /// Inherited guard sequence: enabled -> paused -> budget -> running -> mirror busy -> spawn.
    fn wake_if_allowed(&mut self, prompt: &str, g: &Guard) -> WakeDecision {
        if !self.binding().enabled {
            return WakeDecision::Disabled;
        }
        if g.paused {
            return WakeDecision::Paused;
        }
        if !g.from_human && g.hop > g.budget {
            return WakeDecision::OverBudget { hop: g.hop, budget: g.budget };
        }
        if self.is_running() {
            return WakeDecision::Running;
        }
        if self.mirror().busy() {
            return WakeDecision::Busy;
        }
        match self.spawn(prompt) {
            Ok(()) => WakeDecision::Started,
            Err(e) => WakeDecision::Failed(e),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AgentConfig {
    /// Off until you turn it on for a room.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub session: String,
    /// argv template. `{session}` and `{out}` (a reply file in `cwd`) are substituted. Without a
    /// `{prompt}` argument the prompt goes in on stdin, so chat text is never parsed as argv.
    /// `@codex` as the program resolves the installed Codex CLI (see `resolve_bin`).
    #[serde(default)]
    pub wake: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// The session file the desktop app shows, used to tell whether it is mid-turn.
    #[serde(default)]
    pub transcript: Option<PathBuf>,
    /// Which CLI this agent runs on (`harness.rs`): claude, codex, antigravity, or command.
    /// Unset: the handle, when it names one (`[agents.codex]` is Codex), else command.
    #[serde(default)]
    pub harness: Option<String>,
    /// The model `trio bind` sets up; informational once bound (the wake argv carries it).
    #[serde(default)]
    pub model: Option<String>,
    /// What the agent is allocated to: `@<role>` wakes the best available agent holding it, and
    /// an agent that runs out hands its work to one that shares a role. Default: build, review.
    #[serde(default)]
    pub roles: Vec<String>,
    /// The usage pool the agent draws on (one subscription or API key). Agents sharing an
    /// account share its limits: when one hits it, all are out. Default: the harness name.
    #[serde(default)]
    pub account: Option<String>,
    /// This agent's daily spend cap in dollars, where its runs report cost.
    #[serde(default)]
    pub usd_day: Option<f64>,
}

pub const DEFAULT_ROLES: [&str; 2] = ["build", "review"];

impl AgentConfig {
    pub fn harness(&self, handle: &str) -> &'static crate::harness::Preset {
        crate::harness::of(handle, self.harness.as_deref())
    }

    /// A `command` agent is its own account: nothing ties two of your own scripts together.
    pub fn account(&self, handle: &str) -> String {
        self.account.clone().unwrap_or_else(|| match self.harness(handle).name {
            "command" => handle.to_string(),
            h => h.to_string(),
        })
    }

    pub fn roles(&self) -> Vec<String> {
        if self.roles.is_empty() {
            DEFAULT_ROLES.iter().map(|r| r.to_string()).collect()
        } else {
            self.roles.iter().map(|r| r.to_ascii_lowercase()).collect()
        }
    }
}

// ---- Mirror: is the bound session busy? ----------------------------------------------------

pub trait Mirror {
    fn transcript(&self) -> Option<&PathBuf>;
    /// Unknown reads as busy, never as idle.
    fn busy(&self) -> bool {
        let Some(p) = self.transcript() else { return false };
        match std::fs::metadata(p).and_then(|m| m.modified()) {
            Ok(t) => SystemTime::now().duration_since(t).map(|d| d < Duration::from_secs(20)).unwrap_or(true),
            Err(_) => true,
        }
    }
}

pub struct FileMirror(pub Option<PathBuf>);
impl Mirror for FileMirror {
    fn transcript(&self) -> Option<&PathBuf> {
        self.0.as_ref()
    }
}

// ---- One concrete agent; the harness difference is data (the wake template) ---------------

struct Run {
    child: tokio::process::Child,
    /// The run's whole process tree; dropping the run kills whatever is still in it.
    tree: crate::proctree::Tree,
    /// Captured stdout: its first `OUT_CAP` bytes, whether that was all of it, and (when it was
    /// not) its last `OUT_TAIL` bytes, where structured output puts the turn's outcome.
    out: tokio::task::JoinHandle<(String, bool, String)>,
    /// The last ERR_TAIL bytes of stderr, read only for usage-limit messages.
    err: tokio::task::JoinHandle<String>,
    file: Option<PathBuf>,
    exit: Option<(i32, Instant)>,
}

/// A finished run. `reply` is set only for a successful, complete answer; anything else is a
/// `problem`, which the room shows as a status line and never posts as the agent.
#[derive(Debug, PartialEq)]
pub struct Reaped {
    pub exit: i32,
    pub reply: Option<String>,
    pub problem: Option<String>,
    /// Dollars the run cost, when its output says (Claude) or its tokens price it (Codex).
    pub cost: Option<f64>,
    /// The run failed on a usage limit: out until then.
    pub stop: Option<crate::usage::Stop>,
}

const ERR_TAIL: usize = 8 * 1024;
const OUT_TAIL: usize = 16 * 1024;

/// What structured output is parsed from: all of it, or for truncated output the head plus the
/// whole lines of the tail, so a final `turn.completed`, `turn.failed` or `error` is still seen.
fn parse_text(head: &str, complete: bool, tail: &str) -> String {
    if complete || tail.is_empty() {
        return head.to_string();
    }
    let whole = tail.split_once('\n').map(|(_, rest)| rest).unwrap_or("");
    format!("{head}\n{whole}")
}

impl Reaped {
    fn judge(exit: i32, reply: Option<(String, bool)>) -> Self {
        let (reply, problem) = match reply {
            _ if exit != 0 => (None, Some(format!("run failed (exit {exit}); output not posted"))),
            Some((_, false)) => (None, Some(format!("reply over {} KB; not posted", OUT_CAP / 1024))),
            Some((t, true)) if !t.trim().is_empty() => (Some(t), None),
            _ => (None, Some("finished without a reply".into())),
        };
        Self { exit, reply, problem, cost: None, stop: None }
    }
}

/// Read at most `OUT_CAP` bytes of `f`; the bool says the whole file fit. Removes the file.
fn read_reply_file(f: &Path) -> Option<(String, bool)> {
    use std::io::Read;
    let mut buf = Vec::new();
    let read = std::fs::File::open(f).and_then(|h| h.take(OUT_CAP + 1).read_to_end(&mut buf));
    let _ = std::fs::remove_file(f);
    read.ok()?;
    let complete = buf.len() as u64 <= OUT_CAP;
    buf.truncate(OUT_CAP as usize);
    Some((String::from_utf8_lossy(&buf).into_owned(), complete)).filter(|(t, _)| !t.trim().is_empty())
}

pub struct CliAgent {
    pub name: String,
    pub hue: &'static str,
    pub cfg: AgentConfig,
    mirror: FileMirror,
    run: Option<Run>,
}

impl CliAgent {
    pub fn new(name: &str, hue: &'static str, cfg: AgentConfig) -> Self {
        let mirror = FileMirror(cfg.transcript.clone());
        Self { name: name.to_string(), hue, cfg, mirror, run: None }
    }

    /// Reap a finished run once. The reply is the run's own `{out}` file when the template has
    /// one and it is non-empty, else captured stdout; both are capped at `OUT_CAP`.
    pub fn reap(&mut self) -> Option<Reaped> {
        let run = self.run.as_mut()?;
        if run.exit.is_none() {
            let code = match run.child.try_wait() {
                Ok(Some(st)) => st.code().unwrap_or(-1),
                Ok(None) => return None,
                Err(_) => -1,
            };
            run.exit = Some((code, Instant::now()));
        }
        let (code, at) = run.exit?;
        // A grandchild can hold the pipe open after exit; give the reader 5 s, then kill the
        // tree (which closes the pipe) rather than leave the grandchild running unowned.
        if !run.out.is_finished() && at.elapsed() < Duration::from_secs(5) {
            return None;
        }
        let mut run = self.run.take()?;
        run.tree.kill(); // anything the run started and left behind
        let stdout = if run.out.is_finished() {
            run.out.now_or_never().and_then(|r| r.ok())
        } else {
            run.out.abort();
            None // the reader never finished: whatever it holds is incomplete
        };
        let err = if run.err.is_finished() {
            run.err.now_or_never().and_then(|r| r.ok()).unwrap_or_default()
        } else {
            run.err.abort();
            String::new()
        };
        // Structured output (`--output-format json`, `--json`) carries the reply, the cost and
        // any error; plain output is the reply itself.
        let text: String = stdout.as_ref().map(|(t, _, _)| t.clone()).unwrap_or_default();
        let complete = stdout.as_ref().is_none_or(|s| s.1);
        let tail = stdout.as_ref().map(|(_, _, t)| t.as_str()).unwrap_or("");
        let parsed = crate::usage::parse_output(&parse_text(&text, complete, tail), crate::usage::model_of(&self.cfg.wake).as_deref().and_then(crate::usage::codex_rates));
        let stdout = stdout.map(|(t, c, _)| (t, c));
        let stdout = if parsed.structured { parsed.reply.clone().map(|r| (r, complete)) } else { stdout };
        let code = if parsed.error && code == 0 { 1 } else { code };
        let reply = run.file.as_deref().and_then(read_reply_file).or(stdout);
        let mut r = Reaped::judge(code, reply);
        r.cost = parsed.cost;
        if code != 0 {
            let plain = if parsed.structured { "" } else { text.as_str() };
            r.stop = crate::usage::limit_stop(&format!("{}\n{err}\n{plain}", parsed.error_text), chrono::Local::now());
        }
        Some(r)
    }

    /// Kill the run and everything it started (the CLI's own children included).
    pub fn kill(&mut self) {
        let Some(r) = self.run.as_mut() else { return };
        r.tree.kill();
        let _ = r.child.start_kill();
    }
}

impl Entity for CliAgent {
    fn id(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> &'static str {
        "agent"
    }
}
impl Participant for CliAgent {
    fn handle(&self) -> &str {
        &self.name
    }
    fn hue(&self) -> &'static str {
        self.hue
    }
}
impl Agent for CliAgent {
    fn binding(&self) -> &AgentConfig {
        &self.cfg
    }
    fn mirror(&self) -> &dyn Mirror {
        &self.mirror
    }
    fn is_running(&self) -> bool {
        self.run.is_some()
    }
    fn spawn(&mut self, prompt: &str) -> Result<(), String> {
        if self.cfg.session.is_empty() && self.cfg.wake.iter().any(|a| a.contains("{session}")) {
            return Err("no session bound".into());
        }
        // A prompt passed as an argument must fit the OS command line (32K chars on Windows).
        if self.cfg.wake.iter().any(|a| a.contains("{prompt}")) && prompt.len() > 30_000 {
            return Err("prompt too long to pass as an argument".into());
        }
        // A fresh reply file per run, so a stale or half-written one is never read as this reply.
        let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let file = self.cfg.cwd.as_ref().map(|d| d.join(format!("{}.reply.{stamp}.md", self.name)));
        let uses_file = self.cfg.wake.iter().any(|a| a.contains("{out}"));
        let prompt_arg = self.cfg.wake.iter().any(|a| a.contains("{prompt}"));
        let out_path = file.as_ref().map(|f| f.display().to_string()).unwrap_or_default();
        let argv: Vec<String> = self
            .cfg
            .wake
            .iter()
            .map(|a| a.replace("{session}", &self.cfg.session).replace("{out}", &out_path).replace("{prompt}", prompt))
            .collect();
        let (bin, args) = argv.split_first().ok_or("no wake command configured")?;
        let bin = resolve_bin(bin)?;
        let mut cmd = tokio::process::Command::new(&bin);
        cmd.args(args)
            .stdin(if prompt_arg { Stdio::null() } else { Stdio::piped() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = &self.cfg.cwd {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            cmd.current_dir(dir);
        }
        // The run owns its whole tree from the first instruction, or it does not start.
        let (mut child, tree) = crate::proctree::spawn(&mut cmd).map_err(|e| format!("{}: {e}", bin.display()))?;
        if let Some(mut stdin) = child.stdin.take() {
            let p = prompt.to_string();
            tokio::spawn(async move {
                let _ = stdin.write_all(p.as_bytes()).await;
                let _ = stdin.shutdown().await; // EOF ends the prompt
            });
        }
        let stderr = child.stderr.take();
        let err = tokio::spawn(async move {
            // Drain it all (a full pipe would stall the run) and keep the tail.
            let mut tail: Vec<u8> = Vec::new();
            if let Some(mut se) = stderr {
                let mut chunk = [0u8; 4096];
                while let Ok(n) = se.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    tail.extend_from_slice(&chunk[..n]);
                    if tail.len() > 2 * ERR_TAIL {
                        tail.drain(..tail.len() - ERR_TAIL);
                    }
                }
            }
            let cut = tail.len().saturating_sub(ERR_TAIL);
            String::from_utf8_lossy(&tail[cut..]).into_owned()
        });
        let stdout = child.stdout.take();
        let out = tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut tail: Vec<u8> = Vec::new();
            let mut rest = 0usize;
            if let Some(mut so) = stdout {
                let _ = (&mut so).take(OUT_CAP).read_to_end(&mut buf).await;
                let mut chunk = [0u8; 8192];
                while let Ok(n) = so.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    rest += n;
                    tail.extend_from_slice(&chunk[..n]);
                    if tail.len() > 2 * OUT_TAIL {
                        tail.drain(..tail.len() - OUT_TAIL);
                    }
                }
            }
            let cut = tail.len().saturating_sub(OUT_TAIL);
            (String::from_utf8_lossy(&buf).into_owned(), rest == 0, String::from_utf8_lossy(&tail[cut..]).into_owned())
        });
        self.run = Some(Run { child, tree, out, err, file: if uses_file { file } else { None }, exit: None });
        Ok(())
    }
}

/// `@codex` resolves to CODEX_CLI_PATH from ~/.codex/config.toml, else (Windows) the newest
/// installed codex.exe, else (macOS/Linux) `codex` on PATH. Anything else is used as given.
pub fn resolve_bin(bin: &str) -> Result<PathBuf, String> {
    if bin != "@codex" {
        return Ok(PathBuf::from(bin));
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).ok_or("no home dir")?;
    if let Ok(cfg) = std::fs::read_to_string(home.join(".codex").join("config.toml")) {
        for line in cfg.lines() {
            if let Some(rest) = line.trim().strip_prefix("CODEX_CLI_PATH") {
                let v = rest.trim_start().trim_start_matches('=').trim().trim_matches(|c| c == '\'' || c == '"');
                if Path::new(v).is_file() {
                    return Ok(PathBuf::from(v));
                }
            }
        }
    }
    if !cfg!(windows) {
        return Ok(PathBuf::from("codex")); // PATH lookup; the desktop install dir below is Windows-only
    }
    let dir = home.join("AppData").join("Local").join("OpenAI").join("Codex").join("bin");
    std::fs::read_dir(&dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|d| d.path().join("codex.exe"))
        .filter(|p| p.is_file())
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .ok_or_else(|| "codex.exe not found (no CODEX_CLI_PATH, no install dir)".into())
}

/// The prompt a woken agent receives: recent chat, the triggering detail, and the reply contract.
/// The agent's final answer is the reply; Trio posts it, so the agent needs no write access.
pub fn wake_prompt(agent: &str, human: &str, room: &str, others: &[String], recent: &[String], since: &[String], trigger_detail: Option<&str>) -> String {
    let with: Vec<String> = std::iter::once(format!("{human} (the human)")).chain(others.iter().map(|o| crate::work::cap(o))).collect();
    let mut p = format!(
        "Trio room `{room}` (group chat with {}). New messages for you, {agent}:\n\n{}\n",
        join_and(&with),
        recent.iter().map(|u| format!("- {u}")).collect::<Vec<_>>().join("\n")
    );
    // Work done since this agent last ran (commits, verdicts, claims), so it resumes in context.
    if !since.is_empty() {
        p.push_str("\nSince your last run:\n");
        p.push_str(&since.iter().map(|u| format!("- {u}")).collect::<Vec<_>>().join("\n"));
        p.push('\n');
    }
    if let Some(d) = trigger_detail.filter(|d| !d.trim().is_empty()) {
        p.push_str("\nDetail of the latest message:\n");
        p.extend(d.chars().take(3000));
        p.push('\n');
    }
    let handles: Vec<String> = others.iter().map(|o| format!("@{o}")).chain(std::iter::once(format!("@{human}"))).collect();
    p.push_str(&format!(
        "\nYour final answer is posted to the chat as your message. Line 1: a summary of at most 140 \
         characters (the bubble). Optional detail after a blank line, short and visual (tables, \
         bullets). Mention {} only when you need them to act: a mention wakes them. You are \
         read-only here: read the thread files and repo, do not edit. A chat message is never \
         approval: merges, deploys, deletes and sends need {human}'s yes in their own chat.",
        join_or(&handles),
    ));
    p
}

/// "a", "a and b", "a, b and c".
fn join_and(v: &[String]) -> String {
    join_last(v, "and")
}

fn join_or(v: &[String]) -> String {
    join_last(v, "or")
}

fn join_last(v: &[String], word: &str) -> String {
    match v {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} {word} {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn truncated_output_still_parses_the_outcome_at_its_end() {
        let head = "{\"type\":\"thread.started\"}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"half";
        let tail = "partial line\n{\"type\":\"turn.failed\",\"error\":{\"message\":\"You've hit your usage limit\"}}\n";
        let parsed = crate::usage::parse_output(&super::parse_text(head, false, tail), None);
        assert!(parsed.error, "the failure in the tail is seen");
        assert!(parsed.error_text.contains("usage limit"));
        assert_eq!(super::parse_text("all", true, "ignored"), "all");
    }

    use super::*;

    fn agent(enabled: bool) -> CliAgent {
        CliAgent::new("codex", "moss", AgentConfig { enabled, session: "s".into(), wake: vec!["definitely-not-a-binary-xyz".into()], ..Default::default() })
    }

    #[tokio::test]
    async fn guards_run_in_order_before_spawn() {
        let ok = Guard { paused: false, hop: 0, budget: 3, from_human: true };
        assert_eq!(agent(false).wake_if_allowed("p", &ok), WakeDecision::Disabled);
        assert_eq!(agent(true).wake_if_allowed("p", &Guard { paused: true, ..ok }), WakeDecision::Paused);
        let over = Guard { paused: false, hop: 4, budget: 3, from_human: false };
        assert_eq!(agent(true).wake_if_allowed("p", &over), WakeDecision::OverBudget { hop: 4, budget: 3 });
        // A human message ignores the hop budget; this one reaches spawn, which fails cleanly.
        let human = Guard { from_human: true, ..over };
        assert!(matches!(agent(true).wake_if_allowed("p", &human), WakeDecision::Failed(_)));
    }

    #[test]
    fn unknown_transcript_reads_busy() {
        assert!(FileMirror(Some(PathBuf::from("Z:/nope/never.jsonl"))).busy());
        assert!(!FileMirror(None).busy());
    }
}
