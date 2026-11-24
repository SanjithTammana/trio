# Extending Trio

Trio is a few small parts joined by one event format. Everything a room shows comes from **events**, and everything else is derived from them. To add an integration, write a module that turns what it reads into events. The graph, the Work lanes, the catch-up, both clients and replay after a restart then work for it with no further changes.

## How the parts fit

```
 sources (read files)          watchers (background threads)
   posts files, ledger           git repos, GitHub pull requests, ...
          │                                │ durable rows appended to the ledger
          ▼                                ▼
                     events  ──►  derivers  ──►  graph  ──►  work::all  ──►  snapshot  ──►  web / native
                                                   │
                                    seen mark  ──► catch-up ("since you left")
```

| Part | File | Job |
|---|---|---|
| Event | `src/model.rs` (`Event`, `Body`) | one fact: a message, claim, verdict, commit, run, status |
| Source | `src/source.rs` (`Source`) | reads a file and returns new or replaced events |
| Watcher | `src/gitwatch.rs`, `src/github.rs` | polls something slow on its own thread; `take()` never blocks |
| Deriver | `src/graph.rs` (`Deriver`) | turns events of one kind into graph facts |
| Work | `src/work.rs` | the one place a card's lane and who it waits on are decided |
| Room | `src/room.rs` | wires the parts together, routes mentions, wakes agents, tracks usage |
| Projects | `src/projects.rs` | every room you have, read on demand |

## Rules every integration keeps

1. **Stable ids.** The same fact always gets the same id: `gh:owner/name#12`, or `git:<branch>:tip:<sha>:...`. Re-emitting an id replaces what it said before. That is how a pull request's ask is withdrawn when it merges, and why live tailing and a cold replay agree.
2. **Durable or derived, never both.** A fact you cannot read again later, such as a merge seen once or a pull request's state on a given day, goes to the ledger with `source::append`. Append a row only when it differs from the last one recorded for that id. A fact you can always recompute stays out of the ledger.
3. **Never block the room.** Network and process calls run on a worker thread. The room collects finished results on its next tick.
4. **Never wake an agent by accident.** Only `mentions` wake agents, so an integration should not mention `claude` or `codex` unless the user asked it to.
5. **Times are local `YYYY-MM-DDTHH:MM`.** Convert at the edge; see `github::local`.

## Adding an integration, step by step

GitLab merge requests are the example here. GitHub support (`src/github.rs`) is the model to copy.

1. **Module.** Create `src/gitlab.rs` with:
   - a pure `fn events(&self, mr: &Mr) -> Vec<Event>`, so the same input always gives the same events;
   - a `read()` that calls the CLI or API;
   - a `GitLabWatch { start(link), take() -> Vec<Event> }` built on a thread and two channels, as `GitHubWatch` is.
2. **Map onto existing bodies.** Reuse what the derivers already understand:
   - a review request is a `Body::Message` whose summary says "ready for review" and whose `items` is the branch;
   - an approval is a `Body::Verdict` with outcome `pass` or `changes`;
   - a branch with no local clone is a `Body::Commit`.

   Add a new `Body` variant only for a fact none of these can say.
3. **Config.** Add `pub gitlab: Vec<GitLabCfg>` to `RoomCfg` in `src/room.rs`, with `#[serde(default)]` and a doc comment that shows the `room.toml` line.
4. **Wire it.** In `Room::open_with`, start the watchers when `watch` is true. In `ingest`, append changed rows to the ledger, as `flush_github` does.
5. **Link command.** Teach `trio link` (`with_links` in `src/main.rs`) to recognise the URL.
6. **Tests.** Write a pure test of `events()` and a room test that appends the events, ticks, and checks the Work lane. Also check that a cold `Room::open` replays to the same lane.

## room.toml

```toml
me = "sanjith"                 # your handle; default: the OS user name
hop_budget = 3                 # how many times agents may wake each other in a row
paused = false

repo = "C:/work/web"           # the main repo; unset = the git work tree the room sits in; "" = no git
base = "main"                  # what work merges into; default main, else master

[[repos]]                      # more repos the project spans; branches show as <name>:<branch>
path = "../api"
name = "api"                   # default: the folder name
base = "develop"

[[github]]                     # pull requests, read with your gh login every two minutes
repo = "owner/web"             # a matching local clone shares its branch names
name = "web"                   # label when there is no local clone

item_prefixes = ["WEB", "ENG"] # your own ids: WEB-12, ENG42 (built in: P, XF, H, A, L, S, B, M)

[budget]                       # daily dollars; an agent over its budget is out until midnight
claude_usd_day = 5.0
codex_usd_day = 3.0

[[import]]                     # an existing thread file, shown read-only
file = "codex-posts.md"
from = "codex"

[agents.claude]                # written by `trio bind <room> claude`
enabled = true
session = "..."
wake = ["claude", "..."]
```
