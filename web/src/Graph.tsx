// Routing map: items | branches | components, with routed wires between neighbouring columns
// and a time strip underneath. Items link to the branches that implement them; branches link to
// the parts of the codebase they change since their merge-base. A component two or more open
// branches change is a hot spot (likely conflict) and carries the one accent colour.
// Layout is O(V + E): fixed row height, one barycentre pass per column, no force simulation.
//
// v3 adds lenses over the same data: Work (default: items by stage and who they wait on), Code
// (the routing map above) and Time (the activity strip at full height), plus an explain mode
// that captions every column, glyph and the strip in place.
import { useEffect, useMemo, useState, type MouseEvent } from 'react';
import { State, Who } from './glyph';
import { components, lanes, markSeen, nameOf, timeLabel, WORK_LANES, type Component, type GraphNode, type Snapshot, type WorkLane } from './api';
import { Segmented } from './astryx/Segmented';

type Lens = 'work' | 'code' | 'time';
const LENSES = [
  { value: 'work' as const, label: 'Work', hint: 'What is happening, and who it is waiting on' },
  { value: 'code' as const, label: 'Code', hint: 'Where in the code the work lands' },
  { value: 'time' as const, label: 'Time', hint: 'When each step happened, per participant' },
];
const DONE_HOURS = 24;
const LANE_CAP = 12;

function stored<T extends string>(key: string, fallback: T, ok: readonly T[]): T {
  try {
    const v = localStorage.getItem(key) as T | null;
    return v !== null && ok.includes(v) ? v : fallback;
  } catch {
    return fallback;
  }
}
function store(key: string, v: string) {
  try { localStorage.setItem(key, v); } catch { /* private window: the choice lasts this session */ }
}

const ROW = 30;
const GAP = 56; // gutter width, matches .cols grid
const ACTIVE_HOURS = 36;
const MAX_COMPONENTS = 28;
const PRIORITY = ['changes', 'blocked', 'review', 'claimed', 'open', 'pass'];

export type Pick = { kind: 'item' | 'branch'; id: string } | { kind: 'component'; id: string };

function ms(at: string): number {
  const t = Date.parse(at);
  return Number.isNaN(t) ? 0 : t;
}

const bary = (idx: number[]) => (idx.length ? idx.reduce((a, b) => a + b, 0) / idx.length : Infinity);

function wire(y1: number, y2: number) {
  const m = GAP / 2;
  return `M0,${y1} C${m},${y1} ${m},${y2} ${GAP},${y2}`;
}

export function Graph({ snap, selected, onSelect }: { snap: Snapshot; selected: string | null; onSelect: (p: Pick, el: Element, pointer: boolean) => void }) {
  const [scope, setScope] = useState<'active' | 'all'>('active');
  const [hover, setHover] = useState<string | null>(null);
  const [lens, setLensState] = useState<Lens>(() => stored('trio.lens', 'work', ['work', 'code', 'time']));
  const [explainPref, setExplainPref] = useState<'on' | 'off' | ''>(() => stored('trio.explain', '', ['on', 'off', '']));
  const setLens = (l: Lens) => { setLensState(l); store('trio.lens', l); };
  // Explain is on by default while the room is empty: the empty state is where people learn.
  const explain = explainPref === '' ? snap.graph.items.length === 0 : explainPref === 'on';
  useEffect(() => {
    const on = (e: KeyboardEvent) => {
      const el = e.target as HTMLElement | null;
      if (e.key !== '?' || el?.closest('input, textarea, [contenteditable="true"]')) return;
      e.preventDefault();
      const v = explain ? 'off' : 'on';
      setExplainPref(v);
      store('trio.explain', v);
    };
    window.addEventListener('keydown', on);
    return () => window.removeEventListener('keydown', on);
  }, [explain]);
  const toggleExplain = () => { const v = explain ? 'off' : 'on'; setExplainPref(v); store('trio.explain', v); };
  const me = snap.me;
  const touches = useMemo(() => snap.graph.touches ?? {}, [snap.graph.touches]);
  const merged = useMemo(() => new Set(snap.graph.merged ?? []), [snap.graph.merged]);

  const model = useMemo(() => {
    const all = snap.graph.items;
    const implemented = new Map<string, string[]>(); // item -> branches
    const implementsOf = new Map<string, string[]>(); // branch -> items
    for (const e of snap.graph.edges) {
      if (e.kind !== 'implements') continue;
      implemented.set(e.to, [...(implemented.get(e.to) ?? []), e.from]);
      implementsOf.set(e.from, [...(implementsOf.get(e.from) ?? []), e.to]);
    }
    const newest = all.reduce((m, n) => Math.max(m, ms(n.last)), 0);
    const cutoff = newest - ACTIVE_HOURS * 3600_000;

    const branches = all
      .filter((n) => n.kind === 'branch' && (scope === 'all' || !merged.has(n.id)))
      .sort((a, b) => b.last.localeCompare(a.last));
    const bIndex = new Map(branches.map((b, i) => [b.id, i]));

    const items = all.filter((n) => {
      if (n.kind !== 'item') return false;
      if (scope === 'all') return true;
      if ((implemented.get(n.id) ?? []).some((b) => bIndex.has(b))) return true;
      return ms(n.last) >= cutoff && n.state !== 'pass' && (n.touches > 1 || n.state !== 'open');
    });
    // Each item's barycentre once, not inside the comparator (O(n) instead of O(n log n) recomputes).
    const ibOf = new Map(items.map((n) => [n.id, bary((implemented.get(n.id) ?? []).flatMap((b) => (bIndex.has(b) ? [bIndex.get(b)!] : [])))]));
    const ib = (n: GraphNode) => ibOf.get(n.id)!;
    items.sort((a, b) => {
      const d = ib(a) - ib(b);
      if (d && Number.isFinite(d)) return d;
      if (Number.isFinite(ib(a)) !== Number.isFinite(ib(b))) return Number.isFinite(ib(a)) ? -1 : 1;
      const p = PRIORITY.indexOf(a.state) - PRIORITY.indexOf(b.state);
      return p || b.last.localeCompare(a.last) || a.id.localeCompare(b.id);
    });

    const comps = [...components(touches, branches.filter((b) => !merged.has(b.id)).map((b) => b.id)).values()];
    const cb = (c: Component) => {
      const w = c.branches.reduce((a, b) => a + b.lines, 0) || 1;
      return c.branches.reduce((a, b) => a + (bIndex.get(b.branch) ?? 0) * b.lines, 0) / w;
    };
    const shown = comps.sort((a, b) => Number(b.hot) - Number(a.hot) || b.lines - a.lines).slice(0, MAX_COMPONENTS).sort((a, b) => cb(a) - cb(b));
    return { items, branches, comps: shown, implemented, implementsOf, hot: comps.filter((c) => c.hot).length, hiddenComps: comps.length - shown.length };
  }, [snap.graph.items, snap.graph.edges, touches, merged, scope]);

  const { items, branches, comps, implemented, implementsOf } = model;
  const iy = new Map(items.map((n, i) => [n.id, i * ROW + ROW / 2]));
  const by = new Map(branches.map((n, i) => [n.id, i * ROW + ROW / 2]));
  const cy = new Map(comps.map((c, i) => [c.part, i * ROW + ROW / 2]));
  const left = [...implementsOf].flatMap(([b, its]) => its.filter((i) => iy.has(i) && by.has(b)).map((i) => ({ b, i })));
  const right = comps.flatMap((c) => c.branches.filter((x) => by.has(x.branch)).map((x) => ({ b: x.branch, c: c.part, lines: x.lines, hot: c.hot })));

  // What lights up around the hovered or selected node: one hop each way, two for items.
  const focus = hover ?? selected;
  const lit = useMemo(() => {
    const s = new Set<string>();
    if (!focus) return s;
    const addBranch = (b: string, withItems: boolean) => {
      s.add(`b:${b}`);
      for (const t of touches[b] ?? []) s.add(`c:${t.part}`);
      if (withItems) for (const i of implementsOf.get(b) ?? []) s.add(`i:${i}`);
    };
    const id = focus.slice(2);
    if (focus.startsWith('c:')) {
      s.add(focus);
      for (const x of comps.find((c) => c.part === id)?.branches ?? []) {
        s.add(`b:${x.branch}`);
        for (const i of implementsOf.get(x.branch) ?? []) s.add(`i:${i}`);
      }
    } else if (focus.startsWith('b:')) {
      addBranch(id, true);
    } else {
      s.add(focus);
      for (const b of implemented.get(id) ?? []) addBranch(b, false);
    }
    return s;
  }, [focus, touches, implementsOf, implemented, comps]);

  const height = Math.max(items.length, branches.length, comps.length, 1) * ROW;
  const pick = (p: Pick) => (e: MouseEvent<HTMLButtonElement>) => onSelect(p, e.currentTarget, e.detail > 0);
  const hoverProps = (key: string) => ({ onMouseEnter: () => setHover(key), onMouseLeave: () => setHover(null), onFocus: () => setHover(key), onBlur: () => setHover(null) });
  const cls = (key: string, extra = '') => `node ${lit.has(key) ? 'lit' : ''} ${selected === key ? 'on' : ''} ${extra}`;
  const total = (b: string) => (touches[b] ?? []).reduce((a, t) => a + t.lines, 0);
  const empty = items.length + branches.length === 0;

  return (
    <div className="map">
      <div className="map-bar">
        <div className="lens-row">
          <Segmented label="Lens" value={lens} segments={LENSES} onChange={setLens} />
          <div className="scope" role="tablist" aria-label="Scope">
            {(['active', 'all'] as const).map((s) => (
              <button key={s} role="tab" aria-selected={scope === s} onClick={() => setScope(s)}>
                {s === 'active' ? 'In flight' : 'Everything'}
              </button>
            ))}
          </div>
          <button className={`explain-toggle ${explain ? 'on' : ''}`} aria-pressed={explain} onClick={toggleExplain} title="Explain every column, glyph and the strip (?)">
            <kbd>?</kbd>Explain
          </button>
        </div>
        <div className="key" aria-label="Legend">
          <span><Who who={me} me={me} />you</span><span><Who who="claude" me={me} />Claude</span><span><Who who="codex" me={me} />Codex</span>
          {lens === 'code' && ['open', 'claimed', 'review', 'changes', 'pass'].map((s) => <span key={s}><State state={s} />{s}</span>)}
          {lens === 'code' && <span><State state="branch" />branch</span>}
          {lens === 'code' && <span><State state="component" hot />hot spot</span>}
          {lens === 'work' && <span><i className="you-edge" />waiting on you</span>}
        </div>
      </div>
      {explain && <p className="explain lens-explain">{LENS_EXPLAIN[lens]}</p>}

      {lens === 'work' && snap.catchup && <CatchupStrip snap={snap} onSelect={onSelect} />}
      {lens === 'work' && <WorkLens snap={snap} scope={scope} explain={explain} selected={selected} onSelect={onSelect} onHover={setHover} />}
      {lens === 'time' && <div className="time-lens"><TimeStrip snap={snap} nodes={snap.graph.items} lit={lit} tall /></div>}
      {lens === 'code' && <div className="map-scroll">
        {empty ? (
          <p className="empty">No work in flight. Items appear when a message names one (P0a, XF29); branches when <code>repo</code> is set in room.toml.</p>
        ) : (
          <div className={`cols ${focus ? 'dim' : ''}`}>
            <div className="col-h">Items<span className="n">{items.length}</span>{explain && <small className="explain">Work named in chat (P0a, XF29)</small>}</div>
            <div className="col-h" />
            <div className="col-h">Branches<span className="n">{branches.length}</span>{explain && <small className="explain">Git branches whose commits name an item</small>}</div>
            <div className="col-h" />
            <div className="col-h">Components<span className="n">{comps.length}{model.hiddenComps > 0 ? `+${model.hiddenComps}` : ''}</span>{model.hot > 0 && <span className="hot-count">{model.hot} hot</span>}{explain && <small className="explain">Folders each branch changes, in lines</small>}</div>

            <div className="col">
              {items.map((n) => (
                <button key={n.id} className={cls(`i:${n.id}`)} data-morph={`i:${n.id}`} onClick={pick({ kind: 'item', id: n.id })} {...hoverProps(`i:${n.id}`)} title={n.summary}>
                  <State state={n.state} />
                  <span className="id">{n.id}<small>{n.summary}</small></span>
                  <span className="aux"><Who who={n.lane} me={me} title={nameOf(n.lane, me)} /></span>
                </button>
              ))}
            </div>
            <svg className="wires" width={GAP} height={height} aria-hidden>
              {left.map(({ b, i }) => (
                <path key={`${b}>${i}`} d={wire(iy.get(i)!, by.get(b)!)} className={`wire ${lit.has(`i:${i}`) && lit.has(`b:${b}`) ? 'lit' : ''}`} strokeWidth={1.2} />
              ))}
            </svg>
            <div className="col">
              {branches.map((n) => {
                const isMerged = merged.has(n.id);
                return (
                  <button key={n.id} className={cls(`b:${n.id}`, isMerged ? 'merged' : '')} data-morph={`b:${n.id}`} onClick={pick({ kind: 'branch', id: n.id })} {...hoverProps(`b:${n.id}`)} title={n.summary}>
                    <State state={isMerged ? 'merged' : n.state} />
                    <span className="id">{n.id}</span>
                    <span className="aux">{total(n.id) > 0 && <span>{total(n.id)}</span>}<Who who={n.lane} me={me} title={nameOf(n.lane, me)} /></span>
                  </button>
                );
              })}
            </div>
            <svg className="wires" width={GAP} height={height} aria-hidden>
              {right.map(({ b, c, lines, hot }) => (
                <path key={`${b}>${c}`} d={wire(by.get(b)!, cy.get(c)!)} className={`wire ${hot ? 'hot' : ''} ${lit.has(`b:${b}`) && lit.has(`c:${c}`) ? 'lit' : ''}`} strokeWidth={1 + Math.min(2.5, Math.log10(lines + 1))} />
              ))}
            </svg>
            <div className="col">
              {comps.map((c) => (
                <button key={c.part} className={cls(`c:${c.part}`, c.hot ? 'hot' : '')} data-morph={`c:${c.part}`} onClick={pick({ kind: 'component', id: c.part })} {...hoverProps(`c:${c.part}`)} title={`${c.files} files, ${c.lines} lines, ${c.branches.length} branch${c.branches.length > 1 ? 'es' : ''}`}>
                  <State state="component" hot={c.hot} />
                  <span className="id">{c.part}</span>
                  <span className="aux">{c.hot && <span>×{c.branches.length}</span>}<span>{c.lines}</span></span>
                </button>
              ))}
              {comps.length === 0 && branches.length > 0 && <p className="muted" style={{ fontSize: 12.5 }}>Set <code>repo</code> in room.toml to see what each branch changes.</p>}
            </div>
          </div>
        )}
      </div>}

      {lens !== 'time' && <TimeStrip snap={snap} nodes={lens === 'code' ? [...items, ...branches] : snap.graph.items} lit={lit} explain={explain} />}
    </div>
  );
}

const LENS_EXPLAIN: Record<Lens, string> = {
  work: 'Work: every item by stage. A card waiting on you has a carmine edge and sorts first. Click a card for its story; nothing here costs a wake.',
  code: 'Code: items (left) link to the branches that implement them (middle); branches link to the parts of the code they change since their merge-base (right). A part that two open branches change is a hot spot, a likely conflict.',
  time: 'Time: one row per participant, one tick per recorded step (a claim, commit, verdict or message). Hover a node in another lens to light its ticks.',
};

const LANE_TITLE = Object.fromEntries(WORK_LANES.map((l) => [l.lane, l.title])) as Record<WorkLane, string>;
const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

/** Since you left: what moved while you were away (including work done outside Trio). */
function CatchupStrip({ snap, onSelect }: { snap: Snapshot; onSelect: (p: Pick, el: Element, pointer: boolean) => void }) {
  const c = snap.catchup!;
  const facts = [
    c.moved.length + c.more > 0 && plural(c.moved.length + c.more, 'item moved', 'items moved'),
    c.waiting_on_you > 0 && `${c.waiting_on_you} wait${c.waiting_on_you === 1 ? 's' : ''} on you`,
    c.commits > 0 && `${plural(c.commits, 'commit')} on ${plural(c.branches, 'branch', 'branches')}`,
    c.verdicts > 0 && plural(c.verdicts, 'verdict'),
    c.messages > 0 && plural(c.messages, 'message'),
  ].filter(Boolean);
  return (
    <section className="catchup" aria-label="Since you left">
      <div className="catchup-h">
        <b>Since {timeLabel(c.since)}</b>
        <span>{facts.join(' · ')}</span>
        <button className="catchup-x" onClick={() => markSeen(snap.token)}>Dismiss</button>
      </div>
      {c.moved.length > 0 && (
        <ul className="catchup-list">
          {c.moved.map((m) => (
            <li key={m.id}>
              <button className={m.yours ? 'yours' : ''} onClick={(e) => onSelect({ kind: 'item', id: m.id }, e.currentTarget, e.detail > 0)}>
                <span className="id-tag mono">{m.id}</span>
                <span className="catchup-t">{m.title || m.id}</span>
                <span className="catchup-move">{m.from ? `${LANE_TITLE[m.from]} → ` : 'new · '}{LANE_TITLE[m.to]}</span>
              </button>
            </li>
          ))}
          {c.more > 0 && <li className="muted">+{c.more} more</li>}
        </ul>
      )}
    </section>
  );
}

/** When a card last moved: branch activity counts, so a fresh branch verdict is not stale. */
const moved = (n: GraphNode) => n.work?.last || n.last;

/** Work lens: items in five stage lanes; cards waiting on you sort first. */
function WorkLens({ snap, scope, explain, selected, onSelect, onHover }: { snap: Snapshot; scope: 'active' | 'all'; explain: boolean; selected: string | null; onSelect: (p: Pick, el: Element, pointer: boolean) => void; onHover: (k: string | null) => void }) {
  const me = snap.me;
  const byLane = useMemo(() => {
    const items = snap.graph.items.filter((n) => n.kind === 'item' && n.work);
    const newest = items.reduce((m, n) => Math.max(m, ms(moved(n))), 0);
    const out = new Map<WorkLane, GraphNode[]>(WORK_LANES.map((l) => [l.lane, []]));
    for (const n of items) {
      const lane = n.work!.lane;
      const age = newest - ms(moved(n));
      if (scope === 'active' && lane === 'done' && age > DONE_HOURS * 3600_000) continue;
      if (scope === 'active' && lane === 'asked' && age > ACTIVE_HOURS * 3600_000) continue;
      out.get(lane)?.push(n);
    }
    const mine = (n: GraphNode) => Number(n.work?.waiting_on === me);
    for (const list of out.values()) list.sort((a, b) => mine(b) - mine(a) || moved(b).localeCompare(moved(a)) || a.id.localeCompare(b.id));
    return out;
  }, [snap.graph.items, scope, me]);
  const total = [...byLane.values()].reduce((a, l) => a + l.length, 0);
  if (total === 0 && snap.graph.items.some((n) => n.kind === 'item') && !snap.graph.items.some((n) => n.work)) {
    return <p className="empty">This server predates the Work lens. Update Trio, or use the Code lens.</p>;
  }
  if (total === 0) {
    return <p className="empty">No work yet. When a message names an item (P0a, XF29), it shows here in its stage: asked, in progress, in review, changes or done.</p>;
  }
  return (
    <div className="work">
      {WORK_LANES.map(({ lane, title, explain: caption }) => {
        const list = byLane.get(lane) ?? [];
        return (
          <section key={lane} className="work-lane" aria-label={title}>
            <h4>{title}{lane === 'done' && scope === 'active' ? ' · 24h' : ''}<span className="n">{list.length}</span></h4>
            {explain && <p className="explain">{caption}</p>}
            {list.slice(0, scope === 'all' ? list.length : LANE_CAP).map((n) => {
              const w = n.work!;
              const key = `i:${n.id}`;
              return (
                <button key={n.id} className={`card ${w.waiting_on === me ? 'you' : ''} ${selected === key ? 'on' : ''}`} data-morph={key} title={w.story}
                  onClick={(e) => onSelect({ kind: 'item', id: n.id }, e.currentTarget, e.detail > 0)}
                  onMouseEnter={() => onHover(key)} onMouseLeave={() => onHover(null)} onFocus={() => onHover(key)} onBlur={() => onHover(null)}>
                  <span className="card-t">{n.summary || <b className="mono">{n.id}</b>}</span>
                  <span className="card-m">
                    <span>{n.summary && <span className="id-tag mono">{n.id}</span>}{w.waiting_on ? <><Who who={w.waiting_on} me={me} />{w.waiting_on === me ? 'waiting on you' : nameOf(w.waiting_on, me)}</> : w.lane === 'done' ? <><State state="pass" />done</> : <><State state={n.state} />{n.state}</>}</span>
                    <span>{timeLabel(moved(n))}</span>
                  </span>
                </button>
              );
            })}
            {scope === 'active' && list.length > LANE_CAP && <p className="muted more">+{list.length - LANE_CAP} more in Everything</p>}
          </section>
        );
      })}
    </div>
  );
}

/** Every recorded step of the visible work, one row per participant, on a shared time axis.
 * The hovered or selected node's steps stand out, so you can see when its work happened. */
function TimeStrip({ snap, nodes, lit, tall = false, explain = false }: { snap: Snapshot; nodes: GraphNode[]; lit: Set<string>; tall?: boolean; explain?: boolean }) {
  const me = snap.me;
  const rows = lanes(me);
  const ticks = nodes.flatMap((n) => n.steps.map((s) => ({ t: ms(s.at), row: Math.max(0, rows.indexOf(s.who)), on: lit.has(`i:${n.id}`) || lit.has(`b:${n.id}`), key: `${n.id}-${s.event}` })));
  const ts = ticks.map((x) => x.t).filter(Boolean);
  if (ts.length < 2) return null;
  // A loop, not Math.min(...ts): spreading a large room's ticks overflows the argument limit.
  let lo = Infinity, hi = -Infinity;
  for (const t of ts) { if (t < lo) lo = t; if (t > hi) hi = t; }
  const RH = tall ? 40 : 14;
  const W = 1000, H = rows.length * RH;
  const x = (t: number) => (hi === lo ? W / 2 : ((t - lo) / (hi - lo)) * W);
  const iso = (t: number) => new Date(t - new Date(t).getTimezoneOffset() * 60_000).toISOString().slice(0, 16);
  return (
    <div className={`strip ${tall ? 'tall' : ''}`} aria-label="Activity over time">
      {explain && <p className="explain strip-explain">Activity: one row per participant, one tick per recorded step. Hover or select a node to light its ticks.</p>}
      <div className="lbls">{rows.map((r) => <span key={r}><Who who={r} me={me} />{nameOf(r, me)}</span>)}</div>
      <div>
        <svg className="plot" viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" height={H} aria-hidden>
          {rows.map((_, i) => <line key={i} x1={0} x2={W} y1={i * RH + RH / 2} y2={i * RH + RH / 2} className="rule" vectorEffect="non-scaling-stroke" />)}
          {ticks.filter((k) => k.t).sort((a, b) => Number(a.on) - Number(b.on)).map((k) => (
            <line key={k.key} x1={x(k.t)} x2={x(k.t)} y1={k.row * RH + 2} y2={k.row * RH + RH - 2} className={`tick ${k.on ? 'lit' : ''}`} vectorEffect="non-scaling-stroke" />
          ))}
        </svg>
        <div style={{ display: 'flex', justifyContent: 'space-between' }} className="axis mono muted">
          <small>{timeLabel(iso(lo))}</small><small>{timeLabel(iso(hi))}</small>
        </div>
      </div>
    </div>
  );
}
