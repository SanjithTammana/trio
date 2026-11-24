# Codex attached mode

Trio uses two synchronous command hooks in a local Codex chat. The prompt hook handles Trio commands and adds unread room messages to ordinary turns. The Stop hook gives Trio the text to post to the room. Codex Desktop and the CLI share the wire format. [Codex hooks](https://learn.chatgpt.com/docs/hooks).

| Field or output | Behavior | Source | Verified |
| --- | --- | --- | --- |
| `session_id`, `cwd`, `hook_event_name` | Identifies the root session, its working directory and the event. | [Common inputs](https://learn.chatgpt.com/docs/hooks#common-input-fields) | Yes, captured on both surfaces |
| `UserPromptSubmit.prompt` | User text before generation. The Desktop samples end in `\n`. Trio passes the string through and the shared command parser splits on whitespace. | [UserPromptSubmit](https://learn.chatgpt.com/docs/hooks#userpromptsubmit) | Yes, both surfaces |
| `hookSpecificOutput.hookEventName = "UserPromptSubmit"` and `hookSpecificOutput.additionalContext` | Adds extra developer context to the same turn. Keep room messages bounded and clearly attributed. | [UserPromptSubmit](https://learn.chatgpt.com/docs/hooks#userpromptsubmit) | Yes. Desktop repeated a token present only in hook context. The CLI's model stub received it in the request. |
| `decision = "block"` and nonempty `reason` on `UserPromptSubmit` | Ends submission before generation. Desktop shows the reason under **Hook blocked this message**. This confirmation is hook feedback, not an assistant message. | [UserPromptSubmit](https://learn.chatgpt.com/docs/hooks#userpromptsubmit), [Codex upstream tests](https://github.com/openai/codex/blob/main/codex-rs/core/tests/suite/hooks.rs) | Yes. Desktop displayed the reason with no assistant reply. CLI completed with zero token usage and the stub counted zero requests. The tested `exec --json` stream did not print the reason. |
| `Stop.last_assistant_message` | Latest assistant text or `null`. Trio passes no reply if absent or null. The adapter does not open `transcript_path`. | [Stop](https://learn.chatgpt.com/docs/hooks#stop) | Yes, the captured text on both surfaces matched the normal turn's reply. Null handling is covered by a test. |
| `Stop.turn_id`, `Stop.stop_hook_active` | Identifies the turn and indicates whether a Stop hook already continued it. Another Stop hook can cause another Stop for the same turn. | [Stop continuation](https://learn.chatgpt.com/docs/hooks#stop) | Captured on both surfaces. Continuation was not exercised. |

`{}` lets the hook event finish without adding context or steering the turn. At Stop, `decision: "block"` would ask Codex for another continuation, so Trio always answers Stop with `{}`. `render_codex` produces prompt-specific output for `AddContext` and `Answer`. The package registers `prompt` and `stop` only. A `session-start` hook registered by hand gets the attachment reminder as SessionStart context.

The adapter carries `turn_id` into the core. A Stop is skipped only when it repeats the last posted turn with the same text. The same text on a later turn posts, and so does a changed answer after another Stop hook continued the turn. When a payload has no `turn_id`, a Stop is skipped if its trimmed text matches the last post.

The template's 15 s timeout leaves room for Trio's own waits: up to 5 s for a room's attachment lock, plus a short poke to the room's server.

## Install and review

Use the [hooks.json template](../adapters/codex/hooks.json) for the initial distribution. It invokes the installed Trio binary directly, and the same file works with both Codex surfaces. A plugin could bundle the same hooks and improve install and update discovery. It would still require hook review and a cross-platform binary delivery path. Keep one registration per event because Codex can run duplicates from multiple sources. [Hook discovery and trust](https://learn.chatgpt.com/docs/hooks#where-codex-looks-for-hooks) and [plugin packaging](https://developers.openai.com/plugins/build/plugins#bundle-hooks).

1. Install the Trio build that wires in the Codex adapter. Put the absolute paths to that executable in `command` and `commandWindows`. Quote paths with spaces for the shell. Keep the executable in a user-controlled installation directory.
2. Merge the two handler groups into `$CODEX_HOME/hooks.json` for all local chats, or `.codex/hooks.json` in a trusted project for project-only attachment. The default user directory is `~/.codex` on macOS and Linux and `%USERPROFILE%\.codex` on Windows. Preserve other hooks already in the destination file. Managed policy can exclude non-managed sources.
3. In Desktop, trust the project folder if using project scope. Open **Settings → Hooks** or use **Review hooks** when the composer offers it. Select the source and review the exact `UserPromptSubmit` and `Stop` commands before trusting them. In CLI, use `/hooks`. Codex skips new or changed untrusted definitions. Desktop folder trust and command trust were both present in the Windows probe. [Review and trust](https://learn.chatgpt.com/docs/hooks#review-and-trust-hooks).
4. In the local chat, submit `trio` or `trio <room>` as its own message. The hook feedback confirms the attachment. Submit `trio off` to leave. While attached, ordinary turns can include room messages in model context, and their replies are posted to the local room ledger.

## Reproducing the capture

The six fixtures contain JSON received on hook stdin. A disposable probe accepted a fixed plain-text context prompt and a fixed local bind prompt. It returned the token in `additionalContext` and used `decision: "block"` for bind. It saved only matched probe payloads after replacing session IDs, turn IDs, working directory and transcript path. Desktop produced the final token with the signed-in model. The `codex exec` probe used the installed Windows CLI (`0.171.0-alpha.13`), a disposable profile and a loopback Responses stub to measure requests directly. The workspace's managed policy remained enabled. The packet format and trailing Desktop newline are preserved.

## Headless mode

Keep `codex exec resume <id>` with stdin and `--output-last-message` for a Trio-owned, single-writer session. The existing wake probe appended a turn to its rollout and returned a room reply. App Server offers `thread/resume`, `turn/start`, streamed items and `turn/interrupt`, useful if Trio needs to own a persistent client or stream progress. A separate server does not establish that a live Desktop client shares its in-memory thread or can safely race another writer. Keep attached sessions pull-on-turn. [Non-interactive mode](https://learn.chatgpt.com/docs/non-interactive-mode) and [App Server turn lifecycle](https://learn.chatgpt.com/docs/app-server#turns).
