# Performance

What each part of Trio costs, what was cut, and what is left. The measurements were taken
2026-11-24 on Windows 11 with release builds.

Notation: n = ledger events, m = messages, I = graph items, S = steps, E = edges,
d = the mean number of edges per item, c = connected windows.

## Costs by component

| Path | Before | Now | Note |
|---|---|---|---|
| `/api/state` per request | O(m + I + S + E) build and serialize | O(1) when the revision is unchanged | The snapshot bytes are cached per revision and shared by every client. |
| `/api/state` payload | all edges (158 in the live room) | drawn edges only (`implements`) | `touched` edges were 135 of 158 and were never drawn. |
| Step insert (`graph.rs`) | push then re-sort, O(S log S) | binary search, O(log S) plus the shift | In-order appends do no shifting. |
| SSE fan-out | O(c) per revision | same | Each client refetches once per burst (120 ms debounce in the native client). |
| `room.tick()` | O(new bytes) | same | Reads only past the last offset. |
| `messages()` | O(m log m) per revision | same, cached per revision | Rust's sort is adaptive, so near O(m) on presorted input. |
| Map ordering (web `Graph.tsx`) | barycentre recomputed in the comparator, O(I log I · d) | O(I·d + I log I) | Barycentres are computed once, before the sort. |
| Map ordering (native) | — | O(I·d + I log I), cached per (revision, token, scope) | Hover tracing is O(E) per hover change, not per frame. |
| Chat frame (native) | — | O(m) layout with egui's galley cache; paints only visible rows | No work at all while idle: 0 frames counted over 10 s. |

## Native window vs browser window

The same room, idle, measured over 10 s. The browser figures are Edge in app mode with a
fresh profile.

| | trio-app (egui) | Edge app window |
|---|---|---|
| Processes | 1 | 11 |
| Private memory | 49–50 MB | 322 MB |
| Working set | ~101 MB | 638 MB |
| Idle CPU | ~1.6 ms/s | not measurable (process churn) |
| Binary | 7.7 MB | — |

The idle CPU was 4.7–7 ms/s at first. A frame counter (`TRIO_FRAMES=<file>`) showed no idle
frames, so the cost came from the sync thread waking on 100 ms socket timeouts. That thread now
blocks for up to 1 s and only shortens the wait while a refetch is due. A minimized window
drops the change stream entirely.

## Left on purpose (needs design, not a patch)

Each item below is grounded in recent work found through alphaXiv. None is a drop-in.

1. **Delta sync instead of full refetch.** Every change still costs each client one full
   snapshot (about 104 KB in the scratch room). Delta-state CRDTs send only the join-delta
   since a client's last revision
   ([1603.01529](https://arxiv.org/abs/1603.01529),
   [1803.02750](https://arxiv.org/abs/1803.02750)). Because the ledger is append-only,
   "events after revision r" is already a valid delta. The client-side merge is the work.
2. **Incremental snapshot maintenance.** The snapshot is rebuilt in full once per revision.
   DBSP ([2203.16684](https://arxiv.org/abs/2203.16684)) maintains a view as the integral of
   its input deltas, so an append updates only the rows it touches. This is worth doing once
   rebuilds show up in a profile; they do not yet (9–22 ms per uncached request).
3. **Ledger compaction and graph eviction.** The ledger and the in-memory graph grow without
   bound. Windowing over a tiered append log (Cascade Log,
   [2606.05467](https://arxiv.org/abs/2606.05467)) suggests checkpointing old events and keeping
   a hot window. The open question is what the map's "all time" scope should mean afterwards.
4. **Lazy steps.** Steps are about 40% of the live snapshot and appear only in the side panel.
   Fetching them per item on demand, through the same path as message detail, would cut the
   payload without a protocol change.
5. **Map crossing reduction.** Barycentre ordering is the standard heuristic. Exact one-sided
   crossing minimization is fast in practice
   ([2412.06361](https://arxiv.org/abs/2412.06361), [2510.00331](https://arxiv.org/abs/2510.00331)),
   and few-gap layered drawing ([2502.20896](https://arxiv.org/abs/2502.20896)) targets layouts
   like this one. At 28 components the heuristic is already adequate. Revisit only if crossings
   are visibly bad.
6. **Presence.** `presence()` reads two small files on every tick. Caching by file mtime would
   remove that, but the cost is too small to measure today.
7. **Chat virtualization.** The native chat lays out every row each frame and relies on the
   galley cache. Cached row heights would make a frame O(visible rows). This matters past a
   few thousand messages.

8. **Minimize detection off Windows.** The window drops the change stream when the OS reports
   it minimized. egui does not update that state at runtime on macOS, and winit reports none on
   Wayland, so on those platforms the stream stays open while minimized: one idle socket, a
   keep-alive every 5 s, and the server keeps its 700 ms change poll running as it does for any
   connected page. A snapshot refresh already in flight also finishes before the stream
   notices a minimize; it normally takes milliseconds, with a 15 s cap on a stalled server.

On energy, measurements of background and idle processes
([2509.11738](https://arxiv.org/abs/2509.11738), [2401.06482](https://arxiv.org/abs/2401.06482))
point the same way as the table above. Wakeups, not frames, dominate an idle client's cost, so
the work went into making the client stop waking up when nothing has changed.
