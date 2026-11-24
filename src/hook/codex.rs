//! Wire mapping for Codex command hooks. Room state belongs to the shared hook core.

use super::{HookEvent, HookIn, HookOut, Surface};
use serde_json::{Value, json};
use std::path::PathBuf;

pub fn parse_codex(event: &str, payload: &Value) -> Result<HookIn, String> {
    let (event, wire_name) = match event {
        "prompt" | "UserPromptSubmit" => (HookEvent::PromptSubmit, "UserPromptSubmit"),
        "stop" | "Stop" => (HookEvent::Stop, "Stop"),
        "session-start" | "SessionStart" => (HookEvent::SessionStart, "SessionStart"),
        _ => return Err(format!("unsupported Codex hook event: {event}")),
    };
    let fields = payload
        .as_object()
        .ok_or("Codex hook input must be an object")?;
    if fields.get("hook_event_name").and_then(Value::as_str) != Some(wire_name) {
        return Err("Codex hook_event_name does not match the invoked event".into());
    }
    // Subagent hook payloads can carry the parent session ID. Only the attached
    // root thread should consume unread room messages or publish its reply.
    if fields.get("agent_id").is_some_and(|value| !value.is_null()) {
        return Err("Codex Trio hooks only accept root-thread events".into());
    }

    let required_text = |field: &str| {
        fields
            .get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("Codex {field} must be a string"))
    };
    let session_id = required_text("session_id")?;
    let cwd = required_text("cwd")?;
    if session_id.trim().is_empty() || cwd.trim().is_empty() {
        return Err("Codex session_id and cwd must be non-empty".into());
    }

    let (prompt, reply) = match event {
        HookEvent::PromptSubmit => (Some(required_text("prompt")?.to_owned()), None),
        HookEvent::Stop => {
            let reply = match fields.get("last_assistant_message") {
                None | Some(Value::Null) => None,
                Some(Value::String(text)) => Some(text.clone()),
                Some(_) => {
                    return Err("Codex last_assistant_message must be a string or null".into());
                }
            };
            (None, reply)
        }
        HookEvent::SessionStart => (None, None),
    };
    Ok(HookIn {
        surface: Surface::Codex,
        event,
        session_id: session_id.to_owned(),
        cwd: PathBuf::from(cwd),
        prompt,
        reply,
        // Codex's id for the turn: a repeated Stop with the same turn and text is posted once.
        turn_id: fields.get("turn_id").and_then(Value::as_str).filter(|t| !t.is_empty()).map(str::to_owned),
    })
}

/// The response for `event`. Only a prompt may answer: on Codex, `decision: block` on Stop starts
/// a continuation. SessionStart may add context, in its own event's format; Stop never does.
pub fn respond(event: HookEvent, out: &HookOut) -> Value {
    match (event, out) {
        (HookEvent::PromptSubmit, _) => render_codex(out),
        (HookEvent::SessionStart, HookOut::AddContext(text)) => json!({
            "hookSpecificOutput": { "hookEventName": "SessionStart", "additionalContext": text }
        }),
        _ => json!({}),
    }
}

/// `AddContext` and `Answer` are only valid in response to `PromptSubmit`.
/// In particular, `decision: block` on `Stop` starts a continuation.
pub fn render_codex(out: &HookOut) -> Value {
    match out {
        HookOut::Continue => json!({}),
        HookOut::AddContext(text) => json!({
            "hookSpecificOutput": {
                "hookEventName": "UserPromptSubmit",
                "additionalContext": text,
            }
        }),
        HookOut::Answer(text) => {
            let reason = if text.trim().is_empty() {
                "Trio handled this command locally."
            } else {
                text
            };
            json!({ "decision": "block", "reason": reason })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLI_PROMPT: &str = include_str!("../../tests/fixtures/codex/cli-UserPromptSubmit.json");
    const CLI_STOP: &str = include_str!("../../tests/fixtures/codex/cli-Stop.json");
    const CLI_BLOCK: &str =
        include_str!("../../tests/fixtures/codex/cli-UserPromptSubmit-block.json");
    const DESKTOP_PROMPT: &str =
        include_str!("../../tests/fixtures/codex/desktop-UserPromptSubmit.json");
    const DESKTOP_STOP: &str = include_str!("../../tests/fixtures/codex/desktop-Stop.json");
    const DESKTOP_BLOCK: &str =
        include_str!("../../tests/fixtures/codex/desktop-UserPromptSubmit-block.json");

    #[test]
    fn reads_captured_prompts_and_final_reply() {
        for raw in [CLI_PROMPT, CLI_BLOCK, DESKTOP_PROMPT, DESKTOP_BLOCK] {
            let payload: Value = serde_json::from_str(raw).unwrap();
            let hook = parse_codex("prompt", &payload).unwrap();
            assert!(matches!(hook.event, HookEvent::PromptSubmit));
            assert!(matches!(hook.surface, Surface::Codex));
            assert_eq!(hook.session_id, "fixture-session");
            assert_eq!(hook.cwd, PathBuf::from(r"C:\example\trio-hook-probe"));
            assert_eq!(hook.prompt.as_deref(), payload["prompt"].as_str());
            assert!(hook.reply.is_none());
        }
        for raw in [CLI_STOP, DESKTOP_STOP] {
            let payload: Value = serde_json::from_str(raw).unwrap();
            let hook = parse_codex("stop", &payload).unwrap();
            assert!(matches!(hook.event, HookEvent::Stop));
            assert_eq!(hook.session_id, "fixture-session");
            assert_eq!(hook.reply.as_deref(), Some("TRIO-PROBE-CONTEXT-OK"));
            assert!(hook.prompt.is_none());
        }
    }

    #[test]
    fn validates_required_fields_and_accepts_nullable_reply() {
        let mut stop: Value = serde_json::from_str(CLI_STOP).unwrap();
        stop["last_assistant_message"] = Value::Null;
        assert!(parse_codex("Stop", &stop).unwrap().reply.is_none());
        stop["last_assistant_message"] = json!(["unexpected"]);
        assert!(parse_codex("Stop", &stop).is_err());
        assert!(parse_codex("UserPromptSubmit", &stop).is_err());

        let mut prompt: Value = serde_json::from_str(CLI_PROMPT).unwrap();
        prompt["prompt"] = json!(""); // Non-text submissions can have empty text.
        assert_eq!(
            parse_codex("UserPromptSubmit", &prompt)
                .unwrap()
                .prompt
                .as_deref(),
            Some("")
        );
        prompt["prompt"] = Value::Null;
        assert!(parse_codex("UserPromptSubmit", &prompt).is_err());
        prompt["prompt"] = json!("trio");
        prompt["agent_id"] = json!("subagent-1");
        assert!(parse_codex("UserPromptSubmit", &prompt).is_err());
        prompt["agent_id"] = Value::Null;
        prompt["session_id"] = json!("");
        assert!(parse_codex("UserPromptSubmit", &prompt).is_err());
    }

    #[test]
    fn maps_session_start_without_using_the_transcript() {
        let payload = json!({"hook_event_name": "SessionStart", "session_id": "fixture-session", "cwd": "/example", "transcript_path": null});
        let hook = parse_codex("SessionStart", &payload).unwrap();
        assert!(matches!(hook.event, HookEvent::SessionStart));
        assert!(hook.prompt.is_none() && hook.reply.is_none());
    }

    #[test]
    fn renders_continue_context_and_local_answer() {
        assert_eq!(render_codex(&HookOut::Continue), json!({}));
        assert_eq!(
            render_codex(&HookOut::AddContext("line one\nline two".into())),
            json!({
                "hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": "line one\nline two"}
            })
        );
        assert_eq!(
            render_codex(&HookOut::Answer("Joined Trio.".into())),
            json!({"decision": "block", "reason": "Joined Trio."})
        );
        assert_eq!(
            render_codex(&HookOut::Answer("  ".into())),
            json!({"decision": "block", "reason": "Trio handled this command locally."})
        );
    }

    #[test]
    fn only_a_prompt_can_block_or_add_context() {
        let answer = HookOut::Answer("x".into());
        assert_eq!(respond(HookEvent::Stop, &answer), json!({}), "a block on Stop would continue the turn");
        assert_eq!(respond(HookEvent::Stop, &HookOut::AddContext("x".into())), json!({}));
        assert_eq!(respond(HookEvent::SessionStart, &answer), json!({}));
        assert_eq!(
            respond(HookEvent::SessionStart, &HookOut::AddContext("x".into())),
            json!({"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": "x"}})
        );
        assert_eq!(respond(HookEvent::PromptSubmit, &answer), json!({"decision": "block", "reason": "x"}));
    }

    #[test]
    fn carries_the_turn_id() {
        let hook = parse_codex("stop", &serde_json::from_str(CLI_STOP).unwrap()).unwrap();
        assert_eq!(hook.turn_id.as_deref(), Some("fixture-turn"));
    }
}
