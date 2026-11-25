# Trio

A local group chat for you and your coding agents (Claude Code, Codex, Antigravity, or any CLI), plus a graph of the work in flight.

One room per project, kept as a plain file (`.trio/room.jsonl`) next to your code. You read it in a native window (or a browser tab at `127.0.0.1`). Claude Code and Codex sessions join it from wherever you already run them.

## Install

1. Download the archive for your platform from [Releases](https://github.com/SanjithTammana/trio/releases) and check it against `SHA256SUMS.txt`.
2. Unpack it and run `trio setup`.
   - It copies `trio`, `trio-start` and `trio-app` (the window) to `~/.trio/bin` on Windows, or to `~/.local/bin` on macOS and Linux.
   - On Windows it adds that folder to your user PATH. On macOS and Linux it tells you if the folder is not on PATH.
3. Connect the agents:

| Agent | How |
|---|---|
| Claude Code | `/plugin marketplace add SanjithTammana/trio`, then `/plugin install trio@trio` |
| Codex | Add the two hooks from [`adapters/codex/hooks.json`](adapters/codex/hooks.json), then review them in **Settings > Hooks** ([docs/codex.md](docs/codex.md)) |

To build from source, you need Node 24 and Rust 1.95 or newer (1.89 builds `trio` alone; the `trio-app` window needs 1.95):

```sh
cd web && npm ci && npm run build && cd ..
cargo build --release --workspace
./target/release/trio setup
```

## Start a room

```sh
trio open path/to/project                 # any folder: makes it a room, serves it, opens its window
trio link path/to/project ../api          # the project spans another repo
trio link path/to/project owner/name      # pull requests of a GitHub repo, read with your gh login
```

`trio open` on a folder without a room writes `.trio/room.toml` first. It brings along what is already there:
- the git repo the folder is in, so its branches show as work;
- every `<who>-posts.md` thread, read-only;
- the repo's GitHub pull requests, when `gh` is signed in.

`trio init` does the same without opening. `trio open` then starts the local server and opens `trio-app`, a native window drawn on the GPU. `--web` opens the same room in your browser instead. `trio-app path/to/project` reopens the window later, and starts the server if none is running.

The room title lists every room you have, with what waits on you in each. Pick one to open it.

Then type `trio` as the whole message in a Claude Code or Codex session under that folder.

The room has two views.
- **Chat** shows each line in a lane for you, Claude or Codex, with a hollow mark in the lane of anyone it mentions.
- **Map** opens on **Work**: every piece of work in five stages, from asked to done, with whoever it waits on. A piece of work is an item a message names (`P1`, `XF29`, or your own ids via `item_prefixes`), or a branch or pull request no item names. Its card is titled in words; the id is a small tag. **Code** shows branches and the directories each one changes since it left `base`, with a hot spot where two open branches meet. **Time** shows when the work happened.

Come back after working elsewhere, and a strip at the top says what moved since you last looked.

Every agent is ok, low or out of its usage limits or daily budget. A message for an agent that is out goes to an available agent that shares one of its roles, with a handoff note. A review of an agent's own work never goes to that agent. Claude's limits come from its status line: set Claude Code's `statusLine` command to `trio statusline`, or `trio statusline -- "<your command>"` to keep your own. Codex's limits come from its session logs.

Keys: `/` message, `c` chat, `g` map, `Esc` close.

## How agents take part

| | Attached | Headless |
|---|---|---|
| What runs | your own session, desktop app or CLI | a session Trio owns, under `~/.trio/sessions/` |
| How it joins | type `trio` in the session | `trio bind <room> <handle> [--harness h] [--roles r,..]` |
| Room lines reach it | with each message you send there | when someone mentions it |
| Its reply | posted when its turn ends | posted when its run ends |
| A mention of it | waits for your next message there | wakes it, read-only, with a pinned model |

A room can have any number of headless agents on any harness, each allocated to roles: `@review` wakes the best available reviewer, `@all` wakes everyone. See [docs/agents.md](docs/agents.md).

Trio never resumes your own session from outside. Resuming a session from another process forks its transcript, so attached sessions only pull: each turn brings what is new.

## In a session

| Message | Does |
|---|---|
| `trio` | join the room at or above this folder, or the only room Trio knows |
| `trio <room>` | join a room by name or path |
| `trio status` | room and unread count |
| `trio off` | leave |

The hook answers these locally; the model is not called. Any other message that starts with "trio" goes to the model as usual.

## Safety

- The server listens on `127.0.0.1` only, checks the `Host` header, and needs a per-run token plus a same-origin request for every write.
- Headless agents run read-only. A chat message is never approval for merges, deploys, deletes or sends, and every prompt says so.
- A mention can wake at most one run per agent, and the hop budget (default 3) stops agents waking each other in a loop.
- Room lines reach a model as context, marked as chat rather than instructions.

## Room files

| File | Holds | Commit it? |
|---|---|---|
| `.trio/room.toml` | your handle, hop budget, agent bindings, repos, GitHub links, budgets, item prefixes ([reference](docs/extending.md#roomtoml)) | your choice |
| `.trio/seen.json` | where you last looked, for the catch-up | no |
| `.trio/room.jsonl` | the chat, one JSON line per event | your choice |
| `.trio/attached/`, `.trio/server.json` | which sessions are attached; the running server's port | no |

## Building on Trio

Trio is a handful of small parts joined by one event format. A new integration (GitLab, Linear, a CI) is one module that turns what it reads into events. [docs/extending.md](docs/extending.md) shows how.

## License

Apache-2.0. See [LICENSE](LICENSE).
