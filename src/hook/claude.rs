//! Claude Code hook payloads <-> `HookIn` / `HookOut`.
//!
//! Source: https://code.claude.com/docs/en/hooks (checked 2026-11-23).
//! - Every event carries `session_id`, `transcript_path`, `cwd`, `hook_event_name`.
//! - UserPromptSubmit adds `prompt`. Context goes in `hookSpecificOutput.additionalContext`.
//!   `decision: "block"` stops the prompt: the model is not called and `reason` is shown to the
//!   user only; `suppressOriginalPrompt` drops the echoed prompt from that message.
//! - Stop adds `last_assistant_message`, the turn's final text (the transcript can lag, so it is
//!   not read). Stop never blocks here.
//! - SessionStart adds `source` (startup|resume|clear|compact|fork); context as above.

use super::{HookEvent, HookIn, HookOut, Surface};
use serde_json::{json, Value};
use std::path::PathBuf;

pub fn parse(event: HookEvent, p: &Value) -> Result<HookIn, String> {
    let s = |k: &str| p.get(k).and_then(Value::as_str).map(str::to_string);
    let expected = match event {
        HookEvent::PromptSubmit => "UserPromptSubmit",
        HookEvent::Stop => "Stop",
        HookEvent::SessionStart => "SessionStart",
    };
    if let Some(name) = s("hook_event_name").filter(|n| n != expected) {
        return Err(format!("payload is {name}, hook was registered for {expected}"));
    }
    Ok(HookIn {
        surface: Surface::Claude,
        event,
        session_id: s("session_id").ok_or("payload has no session_id")?,
        cwd: s("cwd").map(PathBuf::from).unwrap_or_default(),
        prompt: s("prompt"),
        reply: s("last_assistant_message"),
        turn_id: s("prompt_id"),
    })
}

/// The JSON to print on stdout, or None to print nothing (exit 0: proceed unchanged).
pub fn render(event: HookEvent, out: &HookOut) -> Option<Value> {
    let name = match event {
        HookEvent::PromptSubmit => "UserPromptSubmit",
        HookEvent::SessionStart => "SessionStart",
        HookEvent::Stop => return None,
    };
    match out {
        HookOut::Continue => None,
        HookOut::AddContext(text) => Some(json!({ "hookSpecificOutput": { "hookEventName": name, "additionalContext": text } })),
        // Only UserPromptSubmit can answer locally; elsewhere the text is shown as context.
        HookOut::Answer(text) if event == HookEvent::PromptSubmit => Some(json!({
            "decision": "block",
            "reason": text,
            "hookSpecificOutput": { "hookEventName": name, "suppressOriginalPrompt": true }
        })),
        HookOut::Answer(text) => Some(json!({ "hookSpecificOutput": { "hookEventName": name, "additionalContext": text } })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude").join(name);
        serde_json::from_str(&std::fs::read_to_string(&path).expect("fixture")).expect("json")
    }

    #[test]
    fn prompt_payload_maps_session_cwd_and_text() {
        let h = parse(HookEvent::PromptSubmit, &fixture("user-prompt-submit.json")).unwrap();
        assert_eq!(h.surface, Surface::Claude);
        assert_eq!(h.session_id, "3f1c2a9e-5b7d-4e21-9c0a-6d8e2f4b1a73");
        assert_eq!(h.prompt.as_deref(), Some("trio"));
        assert!(h.reply.is_none());
        // Windows path captured on Windows; compare as text so the test also runs on Linux/macOS.
        assert!(h.cwd.to_string_lossy().ends_with(r"project\sub"));
        let turn = parse(HookEvent::PromptSubmit, &fixture("user-prompt-submit-turn.json")).unwrap();
        assert!(turn.prompt.unwrap().starts_with("Answer the question"));
    }

    #[test]
    fn stop_payload_carries_the_final_answer() {
        let h = parse(HookEvent::Stop, &fixture("stop.json")).unwrap();
        assert_eq!(h.reply.as_deref(), Some("The kettle is teal. Codex said: \"XF31 is fixed on codex/attach.\""));
        // Stop and the prompt that started the turn share prompt_id: the turn id.
        let p = parse(HookEvent::PromptSubmit, &fixture("user-prompt-submit-turn.json")).unwrap();
        assert!(h.turn_id.is_some() && h.turn_id == p.turn_id);
        assert!(h.prompt.is_none());
    }

    #[test]
    fn session_start_parses_and_mismatched_event_is_refused() {
        assert!(parse(HookEvent::SessionStart, &fixture("session-start.json")).is_ok());
        assert!(parse(HookEvent::Stop, &fixture("user-prompt-submit.json")).unwrap_err().contains("UserPromptSubmit"));
        assert!(parse(HookEvent::Stop, &json!({ "hook_event_name": "Stop" })).is_err());
    }

    #[test]
    fn render_matches_the_documented_shapes() {
        let ctx = render(HookEvent::PromptSubmit, &HookOut::AddContext("lines".into())).unwrap();
        assert_eq!(ctx["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
        assert_eq!(ctx["hookSpecificOutput"]["additionalContext"], "lines");
        assert!(ctx.get("decision").is_none());

        let ans = render(HookEvent::PromptSubmit, &HookOut::Answer("bound".into())).unwrap();
        assert_eq!(ans["decision"], "block");
        assert_eq!(ans["reason"], "bound");
        assert_eq!(ans["hookSpecificOutput"]["suppressOriginalPrompt"], true);

        assert!(render(HookEvent::PromptSubmit, &HookOut::Continue).is_none());
        assert!(render(HookEvent::Stop, &HookOut::AddContext("x".into())).is_none());
        let ss = render(HookEvent::SessionStart, &HookOut::AddContext("x".into())).unwrap();
        assert_eq!(ss["hookSpecificOutput"]["hookEventName"], "SessionStart");
    }
}
