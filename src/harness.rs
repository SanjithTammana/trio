//! Harness presets: how trio drives one kind of agent CLI.
//!
//! A room lists its agents under `[agents.<handle>]`, each with a `harness`. The preset supplies
//! what differs between CLIs: how `trio bind` creates the agent's session and its wake command,
//! where the account's usage limits can be read, and whether attached-mode hooks exist. An agent
//! whose harness has no preset runs as `command`: its own `wake` argv, its stdout (or its `{out}`
//! file) posted as the reply. Supporting another CLI is one more entry in `PRESETS`.

use crate::usage::Reading;
use std::path::Path;

/// What `trio bind` knows when it creates a session.
pub struct Bind<'a> {
    pub agent: &'a str,
    pub model: Option<&'a str>,
    /// The agent's own working folder (session files, reply files).
    pub cwd: &'a Path,
    /// The repository or room folder the agent reads.
    pub context: &'a str,
    /// The first message: who the agent is and the reply contract.
    pub intro: &'a str,
}

/// Creates an agent's session: (session id, wake argv).
pub type BindFn = fn(&Bind) -> Result<(String, Vec<String>), String>;

pub struct Preset {
    pub name: &'static str,
    /// How the product calls itself, for "attached in …" and the agent list.
    pub label: &'static str,
    /// Plugin hooks exist for an attached session (`trio hook <name> …`).
    pub attach: bool,
    /// The model `bind` uses when the agent names none.
    pub model: Option<&'static str>,
    /// Create the agent's session: (session id, wake argv).
    pub bind: Option<BindFn>,
    /// The account's current usage windows, when the CLI exposes them.
    pub usage: Option<fn() -> Option<Reading>>,
}

pub const PRESETS: &[Preset] = &[
    Preset { name: "claude", label: "Claude Code", attach: true, model: Some("opus"), bind: Some(bind_claude), usage: Some(usage_claude) },
    Preset { name: "codex", label: "Codex", attach: true, model: Some("gpt-6.2-sol"), bind: Some(bind_codex), usage: Some(usage_codex) },
    Preset { name: "antigravity", label: "Antigravity", attach: false, model: Some("gemini-4-hydrogen-high"), bind: Some(bind_antigravity), usage: None },
    Preset { name: "command", label: "Command", attach: false, model: None, bind: None, usage: None },
];

pub fn get(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name == name)
}

/// The harness an agent runs on: its `harness`, else its handle when that names a preset (an
/// agent called `claude` is Claude Code), else `command`.
pub fn of(handle: &str, harness: Option<&str>) -> &'static Preset {
    harness.and_then(get).or_else(|| get(handle)).unwrap_or(&PRESETS[PRESETS.len() - 1])
}

/// Every harness name, for messages.
pub fn names() -> String {
    PRESETS.iter().map(|p| p.name).collect::<Vec<_>>().join(", ")
}

fn usage_claude() -> Option<Reading> {
    crate::usage::claude_file().and_then(|p| crate::usage::claude_reading(&p))
}

fn usage_codex() -> Option<Reading> {
    let sessions = crate::usage::home()?.join(".codex").join("sessions");
    crate::usage::codex_latest_log(&sessions).and_then(|l| crate::usage::codex_reading(&l))
}

fn bind_claude(b: &Bind) -> Result<(String, Vec<String>), String> {
    let id = crate::uuid_v4();
    let model = b.model.or(get("claude").and_then(|p| p.model)).unwrap_or("opus");
    let base = ["claude", "-p", "--model", model, "--permission-mode", "default", "--add-dir", b.context];
    let mut first: Vec<String> = base.iter().map(|s| s.to_string()).collect();
    first.extend(["--session-id".into(), id.clone()]);
    run(&first, b.cwd, Some(b.intro))?;
    let mut wake: Vec<String> = base.iter().map(|s| s.to_string()).collect();
    // JSON output carries the reply, total_cost_usd and is_error (usage.rs).
    wake.extend(["--output-format".into(), "json".into(), "--resume".into(), "{session}".into()]);
    Ok((id, wake))
}

fn bind_codex(b: &Bind) -> Result<(String, Vec<String>), String> {
    let model = b.model.or(get("codex").and_then(|p| p.model)).unwrap_or("gpt-6.2-sol");
    let model = ["-m", model, "-c", "model_reasoning_effort=\"medium\""];
    let mut first: Vec<String> = vec!["@codex".into(), "exec".into(), "--json".into()];
    first.extend(model.iter().map(|s| s.to_string()));
    first.extend(["-s", "read-only", "--skip-git-repo-check", "-"].iter().map(|s| s.to_string()));
    let out = run(&first, b.cwd, Some(b.intro))?;
    let id = out
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|v| v.get("thread_id").and_then(|t| t.as_str()).map(str::to_string))
        .ok_or("codex printed no thread_id")?;
    // --json: turn.completed carries token usage, priced in usage.rs.
    let mut wake: Vec<String> = vec!["@codex".into(), "exec".into(), "resume".into(), "--json".into(), "{session}".into()];
    wake.extend(model.iter().map(|s| s.to_string()));
    wake.extend(["-c", "sandbox_mode=\"read-only\"", "--skip-git-repo-check", "-o", "{out}", "-"].iter().map(|s| s.to_string()));
    Ok((id, wake))
}

/// Antigravity's CLI takes the prompt only as the value of `-p`, and resumes a conversation with
/// `-r <conversation_id>`. Plan mode keeps it read-only; headless runs refuse shell commands
/// unless the room's settings allow them.
fn bind_antigravity(b: &Bind) -> Result<(String, Vec<String>), String> {
    let model = b.model.or(get("antigravity").and_then(|p| p.model)).unwrap_or("gemini-4-hydrogen-high");
    let argv = |prompt: &str| -> Vec<String> {
        ["agy", &format!("-p={prompt}"), "--output-format", "json", "--mode", "plan", "--model", model, "--add-dir", b.context].iter().map(|s| s.to_string()).collect()
    };
    let out = run(&argv(b.intro), b.cwd, None)?;
    let id = serde_json::from_str::<serde_json::Value>(out.trim())
        .ok()
        .and_then(|v| v.get("conversation_id").and_then(|t| t.as_str()).map(str::to_string))
        .ok_or("agy printed no conversation_id")?;
    let mut wake = argv("{prompt}");
    wake.extend(["-r".into(), "{session}".into()]);
    Ok((id, wake))
}

/// Run argv in `cwd`, with `stdin` as input when given; Ok(stdout) on exit 0.
pub fn run(argv: &[String], cwd: &Path, stdin: Option<&str>) -> Result<String, String> {
    use std::io::Write;
    let bin = crate::agent::resolve_bin(&argv[0])?;
    let mut child = std::process::Command::new(&bin)
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(if stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    if let (Some(text), Some(mut si)) = (stdin, child.stdin.take()) {
        si.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{} exited {:?}: {}", argv[0], out.status.code(), err.lines().rev().take(3).collect::<Vec<_>>().join(" | ")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Shapes for agents beyond Claude (circle) and Codex (triangle), in the order agents are listed.
/// The clients draw each by name; you are the square.
pub const GLYPHS: &[&str] = &["diamond", "down", "plus", "ring", "bar", "pent", "hex"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handle_that_names_a_preset_is_that_harness() {
        assert_eq!(of("claude", None).name, "claude");
        assert_eq!(of("gemini", Some("antigravity")).name, "antigravity");
        assert_eq!(of("helper", None).name, "command");
        assert_eq!(of("x", Some("nope")).name, "command");
    }

    #[test]
    fn antigravity_output_is_a_reply_or_an_error() {
        let ok = crate::usage::parse_output(r#"{"conversation_id":"c","status":"SUCCESS","response":"done\n","usage":{"input_tokens":5}}"#, None);
        assert_eq!((ok.structured, ok.reply.as_deref(), ok.error), (true, Some("done\n"), false));
        let bad = crate::usage::parse_output(r#"{"conversation_id":"c","status":"ERROR","response":"","error":"RESOURCE_EXHAUSTED: quota"}"#, None);
        assert!(bad.error && bad.reply.is_none());
        assert!(crate::usage::limit_stop(&bad.error_text, chrono::Local::now()).is_some());
    }
}
