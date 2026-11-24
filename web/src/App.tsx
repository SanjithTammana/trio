// Shell: island top bar (presence + wake budget, expands in place), Chat or Map, and one detail
// panel that morphs out of whatever was clicked and collapses back into it.
// Keys: / composer, c chat, g map, esc close.
import { useEffect, useRef, useState } from 'react';
import { flushSync } from 'react-dom';
import { Chat } from './Chat';
import { Graph, type Pick } from './Graph';
import { Markdown } from './Detail';
import { State, Who } from './glyph';
import { morph, unmorph } from './motion';
import { STATE_LABEL, components, fetchDetail, lanes, nameOf, setPaused, timeLabel, useSnapshot, type Message, type Snapshot } from './api';

type Panel = { kind: 'message'; m: Message; key: string } | (Pick & { key: string }) | null;
type View = 'chat' | 'map';

const keyOf = (p: Pick) => `${p.kind === 'component' ? 'c' : p.kind === 'branch' ? 'b' : 'i'}:${p.id}`;

export default function App() {
  const snap = useSnapshot();
  const [view, setView] = useState<View>(() => (location.hash === '#map' ? 'map' : 'chat'));
  const [panel, setPanel] = useState<Panel>(null);
  const [island, setIsland] = useState(false);
  const composer = useRef<HTMLTextAreaElement>(null);
  const panelRef = useRef<Panel>(null);
  useEffect(() => { panelRef.current = panel; }, [panel]);

  useEffect(() => { history.replaceState(null, '', `#${view}`); }, [view]);
  useEffect(() => {
    const on = () => setView(location.hash === '#map' ? 'map' : 'chat');
    window.addEventListener('hashchange', on);
    return () => window.removeEventListener('hashchange', on);
  }, []);

  const open = (next: Panel, el: Element | null, pointer: boolean) => morph(el, pointer, () => flushSync(() => setPanel(next)));
  const close = (pointer: boolean) => {
    const p = panelRef.current;
    if (!p) return;
    unmorph(`[data-morph="${CSS.escape(p.key)}"]`, pointer, () => flushSync(() => setPanel(null)));
  };
  const toggleIsland = (pointer: boolean) => {
    const doc = document as Document & { startViewTransition?: (cb: () => void) => unknown };
    const flip = () => flushSync(() => setIsland((v) => !v));
    if (pointer && doc.startViewTransition && !matchMedia('(prefers-reduced-motion: reduce)').matches) doc.startViewTransition(flip);
    else flip();
  };

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement;
      const typing = t.tagName === 'TEXTAREA' || t.tagName === 'INPUT' || t.isContentEditable;
      if (e.key === 'Escape') {
        if (island) setIsland(false);
        else if (panelRef.current) close(false);
        return;
      }
      if (typing || e.metaKey || e.ctrlKey || e.altKey) return;
      if (e.key === '/') { e.preventDefault(); setView('chat'); requestAnimationFrame(() => composer.current?.focus()); }
      else if (e.key === 'g') setView('map');
      else if (e.key === 'c') setView('chat');
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [island]);

  if (!snap) return <div className="boot">connecting…</div>;
  const me = snap.me;

  return (
    <div className="app">
      <header className="top">
        <div className="room-name" title={snap.room}>trio / <b>{snap.room}</b></div>
        <div className="island-slot">
          <Island snap={snap} open={island} onToggle={toggleIsland} />
        </div>
        <nav className="views" role="tablist" aria-label="View">
          <button role="tab" aria-selected={view === 'chat'} onClick={() => setView('chat')}>Chat<kbd>c</kbd></button>
          <button role="tab" aria-selected={view === 'map'} onClick={() => setView('map')}>Map<kbd>g</kbd></button>
        </nav>
      </header>

      <main className={`main ${panel ? 'with-panel' : ''}`}>
        {view === 'chat' ? (
          <Chat snap={snap} composerRef={composer} onOpen={(m, el, pointer) => open({ kind: 'message', m, key: m.id }, el, pointer)} />
        ) : (
          <Graph snap={snap} selected={panel && panel.kind !== 'message' ? panel.key : null} onSelect={(p, el, pointer) => open({ ...p, key: keyOf(p) }, el, pointer)} />
        )}
        {panel && (
          <aside className="panel" aria-label="Details">
            <button className="close" onClick={(e) => close(e.detail > 0)} aria-label="Close (Esc)">×</button>
            <PanelBody snap={snap} panel={panel} me={me} go={(p) => setPanel({ ...p, key: keyOf(p) })} openMessage={(m) => setPanel({ kind: 'message', m, key: m.id })} />
          </aside>
        )}
      </main>
    </div>
  );
}

function Island({ snap, open, onToggle }: { snap: Snapshot; open: boolean; onToggle: (pointer: boolean) => void }) {
  const me = snap.me;
  const status = (w: string) => {
    const a = snap.agents[w];
    const now = snap.graph.now[w];
    if (a?.running || now?.running) return { live: true, text: now?.text || 'working…' };
    if (a?.attached) {
      const app = a.attached === 'codex' ? 'Codex' : a.attached === 'claude' ? 'Claude Code' : a.attached;
      return { live: false, text: `attached in ${app}${now?.text ? ` · ${now.text}` : ''}` };
    }
    if (w !== me && !a?.enabled) return { live: false, text: now?.text || 'not in the room' };
    return { live: false, text: now?.text || 'idle' };
  };
  const hop = snap.paused ? 'paused' : `${snap.hop}/${snap.budget}`;
  if (!open) {
    return (
      <button className="island" onClick={(e) => onToggle(e.detail > 0)} aria-expanded={false} aria-label="Room status">
        {lanes(me).map((w) => {
          const s = status(w);
          return <span key={w} className={`who ${s.live ? 'live' : ''}`} title={`${nameOf(w, me)}: ${s.text}`}><Who who={w} me={me} /><span className="nm">{nameOf(w, me)}</span></span>;
        })}
        <span className="sep" />
        <span className={`hop ${snap.paused ? 'paused' : ''}`} title="Agent wakes used of the hop budget">{hop}</span>
      </button>
    );
  }
  return (
    <div className="island open" role="dialog" aria-label="Room status">
      {lanes(me).map((w) => {
        const s = status(w);
        return (
          <div key={w} className="row">
            <Who who={w} me={me} />
            <span className="nm">{nameOf(w, me)}</span>
            <span className="st">{s.text}{snap.graph.now[w]?.at ? ` · ${timeLabel(snap.graph.now[w].at)}` : ''}</span>
          </div>
        );
      })}
      <div className="actions">
        <span className="mono">{snap.paused ? 'agents will not wake each other' : `wakes ${snap.hop} of ${snap.budget}`}</span>
        <span style={{ display: 'flex', gap: 6 }}>
          <button className={snap.paused ? 'on' : ''} onClick={() => setPaused(snap.token, !snap.paused)}>{snap.paused ? 'Resume' : 'Pause'}</button>
          <button onClick={(e) => onToggle(e.detail > 0)} aria-label="Collapse">Done</button>
        </span>
      </div>
    </div>
  );
}

function PanelBody({ snap, panel, me, go, openMessage }: {
  snap: Snapshot; panel: NonNullable<Panel>; me: string;
  go: (p: Pick) => void; openMessage: (m: Message) => void;
}) {
  const [loaded, setLoaded] = useState<{ id: string; text: string } | null>(null);
  const msgId = panel.kind === 'message' && panel.m.has_detail ? panel.m.id : null;
  useEffect(() => {
    if (!msgId) return;
    let live = true;
    fetchDetail(msgId).then((text) => { if (live) setLoaded({ id: msgId, text }); });
    return () => { live = false; };
  }, [msgId]);
  const detail = loaded && loaded.id === msgId ? loaded.text : null;

  const touches = snap.graph.touches ?? {};
  const merged = new Set(snap.graph.merged ?? []);
  const openStep = (eventId: string) => {
    const m = snap.messages.find((x) => x.id === eventId);
    if (m) openMessage(m);
  };

  if (panel.kind === 'message') {
    const m = panel.m;
    return (
      <>
        <div className="eyebrow"><Who who={m.from} me={me} />{nameOf(m.from, me)} · {timeLabel(m.at)}{(m.mentions?.length ?? 0) > 0 && <> · to {m.mentions!.map((w) => nameOf(w, me)).join(', ')}</>}</div>
        <h3>{m.summary}</h3>
        {(m.items?.length ?? 0) > 0 && (
          <div className="chips" style={{ marginBottom: 12 }}>{m.items!.map((it) => <button key={it} className="chip" onClick={() => go({ kind: 'item', id: it })}>{it}</button>)}</div>
        )}
        {m.has_detail ? (detail === null ? <p className="muted">loading…</p> : <Markdown text={detail} />) : <p className="muted">No more detail.</p>}
      </>
    );
  }

  if (panel.kind === 'component') {
    const branches = (snap.graph.items.filter((n) => n.kind === 'branch' && !merged.has(n.id))).map((n) => n.id);
    const c = components(touches, branches).get(panel.id);
    if (!c) return <p className="muted">No open branch changes {panel.id} now.</p>;
    const max = Math.max(...c.branches.map((b) => b.lines), 1);
    return (
      <>
        <div className="eyebrow"><State state="component" hot={c.hot} />{c.hot ? 'hot spot' : 'component'}</div>
        <h3><span className="mono">{c.part}</span></h3>
        <p className="lede">
          {c.lines} lines in {c.files} file{c.files === 1 ? '' : 's'} across {c.branches.length} open branch{c.branches.length === 1 ? '' : 'es'}.
          {c.hot && ' More than one open branch changes it, so whichever merges second may conflict.'}
        </p>
        <div className="section">Branches</div>
        <div className="bars">
          {c.branches.sort((a, b) => b.lines - a.lines).map((b) => (
            <button key={b.branch} className={`bar ${c.hot ? 'hot' : ''}`} onClick={() => go({ kind: 'branch', id: b.branch })}>
              <span className="lbl">{b.branch}</span><span className="n">{b.lines} · {b.files}f</span>
              <span className="fill"><i style={{ width: `${(b.lines / max) * 100}%` }} /></span>
            </button>
          ))}
        </div>
      </>
    );
  }

  const n = snap.graph.items.find((x) => x.id === panel.id);
  if (!n) return <p className="muted">{panel.id} is outside the current window.</p>;
  const isBranch = n.kind === 'branch';
  const linked = snap.graph.edges.filter((e) => e.kind === 'implements' && (isBranch ? e.from === n.id : e.to === n.id)).map((e) => (isBranch ? e.to : e.from));
  const ts = touches[n.id] ?? [];
  const max = Math.max(...ts.map((t) => t.lines), 1);
  const hotParts = new Set([...components(touches, snap.graph.items.filter((x) => x.kind === 'branch' && !merged.has(x.id)).map((x) => x.id)).values()].filter((c) => c.hot).map((c) => c.part));
  const state = isBranch && merged.has(n.id) ? 'merged' : n.state;
  return (
    <>
      <div className="eyebrow"><State state={state} />{STATE_LABEL[state] ?? state} · <Who who={n.lane} me={me} />{nameOf(n.lane, me)} · {n.touches} update{n.touches === 1 ? '' : 's'}</div>
      <h3><span className="mono">{n.id}</span></h3>
      {n.summary && <p className="lede">{n.summary}</p>}
      {n.work && <p className="work-story">{n.work.story}</p>}
      {linked.length > 0 && (
        <>
          <div className="section">{isBranch ? 'Implements' : 'Branches'}</div>
          <div className="chips">{linked.map((id) => <button key={id} className="chip" onClick={() => go({ kind: isBranch ? 'item' : 'branch', id })}>{id}</button>)}</div>
        </>
      )}
      {isBranch && ts.length > 0 && (
        <>
          <div className="section">Changes since merge-base</div>
          <div className="bars">
            {ts.map((t) => (
              <button key={t.part} className={`bar ${hotParts.has(t.part) ? 'hot' : ''}`} onClick={() => go({ kind: 'component', id: t.part })}>
                <span className="lbl">{hotParts.has(t.part) && <State state="component" hot />}{t.part}</span><span className="n">{t.lines} · {t.files}f</span>
                <span className="fill"><i style={{ width: `${(t.lines / max) * 100}%` }} /></span>
              </button>
            ))}
          </div>
        </>
      )}
      <div className="section">{n.work ? 'Steps' : 'Story'}</div>
      <ol className="story">
        {n.steps.slice().reverse().map((s, i) => (
          <li key={`${s.event}-${i}`}>
            <Who who={s.who} me={me} />
            <button className="step" onClick={() => openStep(s.event)}>
              <span className="step-what">{s.what}</span>
              <span className="step-meta">{nameOf(s.who, me)} · {timeLabel(s.at)}</span>
            </button>
          </li>
        ))}
      </ol>
    </>
  );
}
