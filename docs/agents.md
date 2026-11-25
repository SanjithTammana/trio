# Agents

A room can have any number of agents, each on any CLI harness. Each agent has a **handle** (what you `@mention`), a **harness** (the CLI it runs on), a **model**, the **roles** it is allocated to, and the **account** whose usage limits it draws on.

## Adding an agent

```
trio bind <room-dir> <handle> [--harness h] [--model m] [--roles r,..] [--account a]
```

`bind` creates a session the room owns, tells the agent who else is in the room, and writes `[agents.<handle>]` to `.trio/room.toml`. Examples:

```
trio bind . claude                                   # Claude Code, roles build + review
trio bind . codex --roles build                      # Codex, builds only
trio bind . gemini --harness antigravity --roles review
trio bind . oss --harness antigravity --model gpt-oss-120b --roles chores
trio bind . sonnet --harness claude --model sonnet --roles review
```

A handle is letters, digits, `-` and `_`. It cannot be `both`, `all`, `trio` or `github`, and it should not be one of the roles you use. When the handle names a harness (`claude`, `codex`, `antigravity`), `--harness` can be left out.

## Harnesses

| Harness | CLI | Default model | Usage limits read from | Attach (hooks) |
|---|---|---|---|---|
| `claude` | `claude -p` | opus | the status line: set Claude Code's `statusLine` to `trio statusline` | yes |
| `codex` | `codex exec` | gpt-6.2-sol | Codex's session logs | yes |
| `antigravity` | `agy -p --output-format json --mode plan` | gemini-4-hydrogen-high | its error replies only | no |
| `command` | anything you write | none | its error replies only | no |

Antigravity agents run in plan mode, so they do not edit files, with the repository added as a folder they can read (`--add-dir`), and resume their conversation by id. The prompt goes in as an argument, so a turn over 30,000 characters is refused. Install `agy` and sign in once before binding.

### Any other CLI: the `command` harness

Write the wake command yourself in `room.toml`:

```toml
[agents.local]
enabled = true
harness = "command"
roles = ["chores"]
cwd = "C:/Users/me/.trio/sessions/local"
wake = ["ollama-agent", "--resume", "{session}", "--reply-file", "{out}"]
session = "abc"
```

The wake is an argument list, never a shell line. Trio substitutes:

| Placeholder | Becomes |
|---|---|
| `{prompt}` | the turn's prompt, as one argument (prompts over 30,000 characters are refused; leave `{prompt}` out and the prompt goes in on stdin instead) |
| `{session}` | `session` from the config (a wake that uses it needs one) |
| `{out}` | a reply file in `cwd`; when it is non-empty after the run, it is the reply |

Without `{out}`, the reply is stdout. Claude-style JSON (`result`, `is_error`, `total_cost_usd`) and Antigravity JSON (`status`, `response`) are recognised; anything else is used as plain text. The first line of the reply is the chat summary.

## Roles

`roles` is what an agent is allocated to. The default is `["build", "review"]`. Roles are free-form words.

- `@<handle>` wakes that agent.
- `@<role>` wakes **one** agent holding the role: the best available, in this order: not out of usage, idle, not low on usage, then its place in `room.toml`. The author of the message and an agent asked to review its own work are never picked.
- `@all` wakes every enabled agent except the author. `@both` still works and means the same.

When an agent is out of usage, a message for it goes to a stand-in: an available agent that shares one of its roles, else any available agent, with a handoff note. A review of an agent's own work waits rather than going to that agent.

## Accounts and budgets

Agents on one subscription or API key share its limits. `account` names that pool; the default is the harness name, so every Claude Code agent shares one Claude account unless you say otherwise. A `command` agent is its own account by default.

```toml
[agents.sonnet]
harness = "claude"
account = "work-claude"   # a second Claude login: separate limits
usd_day = 4.0             # this agent's daily spend cap, where its runs report cost
```

When an agent's run reports a usage-limit error, every agent on that account is out until the reset time. `[budget] claude_usd_day` and `codex_usd_day` still apply to agents on those harnesses that set no `usd_day`.

## Adding a harness preset

A preset is one entry in `PRESETS` in `src/harness.rs`:

```rust
Preset { name: "mycli", label: "My CLI", attach: false, model: Some("default-model"), bind: Some(bind_mycli), usage: None },
```

- `bind` creates the session and returns `(session id, wake argv)`. It receives the handle, model, working folder, the repository to read, and an intro prompt; see `bind_antigravity` for a CLI that prints its own session id.
- `usage`, when the CLI exposes its limits, returns the current windows; it is read once per refresh for each account.
- If the CLI's output is JSON that is neither shape above, add a branch to `usage::parse_output` so replies, costs and limit errors are read from it.

Add a test that `bind` builds the argv you expect, and a room test with a `command` stand-in, as the tests in `src/room.rs` do.
