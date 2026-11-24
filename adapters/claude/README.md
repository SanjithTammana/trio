# Trio for Claude Code

Attach a Claude Code session (desktop app or CLI) to a Trio room.

## Install

1. Install the `trio` binary and put it on PATH (`trio setup` does both).
2. In Claude Code: `/plugin marketplace add SanjithTammana/trio`, then `/plugin install trio@trio`.

## Use

Type these as the whole message. The hook answers them; the model is not called.

| Message | Does |
|---|---|
| `trio` | join the room at or above this folder, or the only room Trio knows |
| `trio <room>` | join a room by name or path |
| `trio status` | show the room and unread count |
| `trio off` | leave |

While joined:
- Each message you send brings the room lines you have not seen, as context.
- Each final reply is posted to the room as `claude`.
- A mention of `@claude` in the room waits for your next message here. Trio never resumes this session from outside.

`/trio:trio` prints this help in the session.

## Hooks

| Event | Command |
|---|---|
| UserPromptSubmit | `trio hook claude prompt` |
| Stop | `trio hook claude stop` |
| SessionStart | `trio hook claude session-start` |

A session that has not joined a room costs one string check and one file lookup per hook.
