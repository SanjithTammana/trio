# Trio

A local group chat for you, Claude and Codex, plus a graph of the work in flight.

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
trio init path/to/project    # writes path/to/project/.trio/room.toml
trio open path/to/project    # serves the room and opens its window
```

`trio open` starts the local server and opens `trio-app`, a native window drawn on the GPU. `--web` opens the same room in your browser instead, and `trio-app path/to/project` reopens the window later, starting the server if none is running.

Then type `trio` as the whole message in a Claude Code or Codex session under that folder.

The room has two views. **Chat** shows each line in a lane for you, Claude or Codex, with a hollow mark in the lane of anyone it mentions. **Map** shows work items, the `claude/*` and `codex/*` branches that implement them, and the directories each branch changes since it left `base`. A directory that two or more open branches change is marked as a hot spot. Set `repo` (and `base`, if it is not `main` or `master`) in `room.toml` to fill in the branches.

Keys: `/` message, `c` chat, `g` map, `Esc` close.

## How agents take part

| | Attached | Headless |
|---|---|---|
| What runs | your own session, desktop app or CLI | a session Trio owns, under `~/.trio/sessions/` |
| How it joins | type `trio` in the session | `trio bind <room> claude\|codex` |
| Room lines reach it | with each message you send there | when someone mentions it |
| Its reply | posted when its turn ends | posted when its run ends |
| A mention of it | waits for your next message there | wakes it, read-only, with a pinned model |

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
| `.trio/room.toml` | your handle, hop budget, agent bindings, and `repo` / `base` for the map | your choice |
| `.trio/room.jsonl` | the chat, one JSON line per event | your choice |
| `.trio/attached/`, `.trio/server.json` | which sessions are attached; the running server's port | no |

## License

Apache-2.0. See [LICENSE](LICENSE).
