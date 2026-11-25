// The group chat as a lane log: a three-column gutter (you, Claude, Codex) carries each line's
// sender as a solid glyph in its own lane and anyone it addresses as a hollow one, so who is
// talking to whom reads down the page without colour. Click a line for its detail.
import { useEffect, useLayoutEffect, useMemo, useRef, useState, type MouseEvent } from 'react';
import { Who } from './glyph';
import { agentNames, lanes, nameOf, roleNames, rolesOf, send, timeLabel, type Message, type Snapshot } from './api';

const MAX = 140;

function Gutter({ from, to, me }: { from: string; to: string[]; me: string }) {
  return (
    <div className="gut" aria-hidden>
      {lanes(me).map((l) => {
        const sender = from === l;
        const addressed = !sender && (to.includes(l) || (l !== me && (to.includes('both') || to.includes('all') || rolesOf(l).some((r) => to.includes(r)))));
        return <span key={l} className={addressed ? 'to' : ''}>{(sender || addressed) && <Who who={l} me={me} to={addressed} />}</span>;
      })}
    </div>
  );
}

export function Chat({ snap, onOpen, composerRef }: {
  snap: Snapshot;
  onOpen: (m: Message, el: Element, pointer: boolean) => void;
  composerRef: React.RefObject<HTMLTextAreaElement | null>;
}) {
  const [draft, setDraft] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const log = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const me = snap.me;
  const rows = useMemo(() => snap.messages.filter((m) => !(m.kind === 'status' && m.from !== 'trio')), [snap.messages]);

  // Follow the bottom unless the reader has scrolled up. Keyed on the newest row, not the count:
  // once the 400-row window is full, the count stops changing while new rows still arrive.
  const newest = rows.length ? `${rows[rows.length - 1].id}@${rows[rows.length - 1].at}` : '';
  useLayoutEffect(() => {
    const el = log.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [newest]);
  useEffect(() => {
    const el = log.current;
    if (!el) return;
    const on = () => { stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40; };
    el.addEventListener('scroll', on, { passive: true });
    return () => el.removeEventListener('scroll', on);
  }, []);

  const submit = async () => {
    const value = draft.trim();
    if (!value || busy) return;
    setBusy(true);
    setError(null);
    const err = await send(snap.token, value);
    setBusy(false);
    if (err) setError(err);
    else { setDraft(''); stick.current = true; }
  };
  const mention = (who: string) => {
    setDraft((d) => (d.includes(`@${who}`) ? d : `@${who} ${d}`.trimEnd() + ' '));
    composerRef.current?.focus();
  };
  const open = (m: Message) => (e: MouseEvent<HTMLButtonElement>) => onOpen(m, e.currentTarget, e.detail > 0);

  return (
    <div className="chat" style={{ '--lanes': lanes(me).length } as React.CSSProperties}>
      <div className="log" ref={log} aria-live="polite">
        <div className="lanes-head">
          <div className="gut">
            {lanes(me).map((l) => <span key={l} title={nameOf(l, me)}><Who who={l} me={me} title={nameOf(l, me)} /></span>)}
          </div>
          <span>{rows.length === 0 ? '' : lanes(me).map((l) => nameOf(l, me)).join(' · ')}</span>
        </div>
        {rows.length === 0 && <p className="empty">Nothing yet. Say something, or type <code>trio</code> in a Claude Code or Codex session in this folder.</p>}
        {rows.map((m, i) => {
          const to = m.mentions ?? [];
          if (m.kind !== 'message' || m.from === 'trio') {
            return (
              <div key={m.id} className="sys">
                <Gutter from={m.from} to={to} me={me} />
                <div className="txt">{!m.summary.toLowerCase().startsWith(m.from) && <b>{nameOf(m.from, me)} </b>}{m.summary} · {timeLabel(m.at)}</div>
              </div>
            );
          }
          const prev = rows[i - 1];
          const head = !prev || prev.from !== m.from || prev.kind !== 'message' || prev.at.slice(0, 15) !== m.at.slice(0, 15);
          return (
            <div key={m.id} className={`msg ${head ? 'head' : ''} ${m.from === me ? 'mine' : ''}`}>
              <Gutter from={m.from} to={to} me={me} />
              <div>
                {head && <div className="hdr"><b>{nameOf(m.from, me)}</b><time>{timeLabel(m.at)}</time></div>}
                <button className="body" data-morph={m.id} onClick={open(m)} aria-label={`${nameOf(m.from, me)}: ${m.summary}. Open detail`}>
                  {m.summary}
                  {(m.items?.length ?? 0) > 0 && <span className="chips">{m.items!.slice(0, 4).map((it) => <span key={it} className="chip">{it}</span>)}</span>}
                  {m.has_detail && <span className="more">more ›</span>}
                </button>
              </div>
            </div>
          );
        })}
      </div>

      <div className="composer">
        <div className="composer-in">
          <div className="tools">
            {[...new Set([...agentNames(), ...roleNames(), 'all'])].map((w) => (
              <button key={w} onClick={() => mention(w)} title={`Mention ${nameOf(w, me)}`}>@{w}</button>
            ))}
            <span className={`count ${draft.length > MAX ? 'over' : ''}`}>
              {draft.length > MAX ? 'long: the first sentence becomes the line, the rest its detail' : `${draft.length}/${MAX}`}
            </span>
          </div>
          <div className="box">
            <textarea
              ref={composerRef}
              rows={1}
              value={draft}
              placeholder="Message the room   ( / to focus, Enter to send, Shift+Enter for a new line )"
              aria-label="Message"
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) { e.preventDefault(); submit(); }
                if (e.key === 'Escape') (e.target as HTMLTextAreaElement).blur();
              }}
            />
            <button className="send" onClick={submit} disabled={!draft.trim() || busy}>Send</button>
          </div>
          {error && <div className="error" role="alert">{error}</div>}
        </div>
      </div>
    </div>
  );
}
