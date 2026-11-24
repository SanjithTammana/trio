// Small glyphs instead of colour: participants by shape (you ■, Claude ●, Codex ▲, Trio ◦),
// state by outline (open ○, claimed ◐, review ◇, changes ◆ accent, pass ✓, blocked ⊘ accent,
// branch ⑂). One 10×10 box each, drawn the same in HTML and inside the map's SVG.
import type { ReactNode } from 'react';
import { STATE_LABEL } from './api';

function shape(kind: string, filled: boolean): ReactNode {
  const c = filled ? 'f' : 's';
  switch (kind) {
    case 'me': return <rect className={c} x={1} y={1} width={8} height={8} />;
    case 'claude': return <circle className={c} cx={5} cy={5} r={4.2} />;
    case 'codex': return <path className={c} d="M5 0.8 L9.4 9 L0.6 9 Z" />;
    case 'trio': return <circle className="s" cx={5} cy={5} r={2.4} />;
    case 'both': return <><circle className={c} cx={3.2} cy={5} r={2.6} /><path className={c} d="M7.2 2.2 L9.6 7.8 L4.8 7.8 Z" /></>;
    default: return <circle className={c} cx={5} cy={5} r={3} />;
  }
}

/** A participant's glyph: `me` is the room's human handle. Hollow when `to` (addressed, not sender). */
export function Who({ who, me, to = false, title }: { who: string; me: string; to?: boolean; title?: string }) {
  const kind = who === me ? 'me' : who;
  return (
    <svg className="g" viewBox="0 0 10 10" aria-hidden={title ? undefined : true} role={title ? 'img' : undefined}>
      {title && <title>{title}</title>}
      {shape(kind, !to)}
    </svg>
  );
}

const STATE: Record<string, ReactNode> = {
  open: <circle className="s" cx={5} cy={5} r={4} />,
  claimed: <><circle className="s" cx={5} cy={5} r={4} /><path className="f" d="M5 1 A4 4 0 0 0 5 9 Z" /></>,
  review: <path className="s" d="M5 0.8 L9.2 5 L5 9.2 L0.8 5 Z" />,
  changes: <path className="f" d="M5 0.6 L9.4 5 L5 9.4 L0.6 5 Z" />,
  pass: <path className="s" d="M1.2 5.4 L4 8.2 L9 1.8" />,
  blocked: <><circle className="s" cx={5} cy={5} r={4} /><path className="s" d="M2.2 7.8 L7.8 2.2" /></>,
  branch: <><circle className="f" cx={2.6} cy={2} r={1.5} /><circle className="f" cx={2.6} cy={8.4} r={1.5} /><circle className="f" cx={7.8} cy={3.6} r={1.5} /><path className="s" d="M2.6 2 V8.4 M2.6 7 C2.6 5 7.8 6 7.8 3.6" /></>,
  merged: <><circle className="s" cx={5} cy={5} r={4} /><circle className="f" cx={5} cy={5} r={1.6} /></>,
  component: <rect className="s" x={1} y={2} width={8} height={6} rx={1} />,
};


export function State({ state, hot = false }: { state: string; hot?: boolean }) {
  return (
    <svg className={`g st-${state} ${hot ? 'hot' : ''}`} viewBox="0 0 10 10" role="img" aria-label={STATE_LABEL[state] ?? state}>
      <title>{STATE_LABEL[state] ?? state}</title>
      {STATE[state] ?? STATE.open}
    </svg>
  );
}
