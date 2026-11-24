// Types and data access for the Trio server. One SSE stream says "revision changed";
// the client refetches the snapshot, debounced, so a burst of events costs one fetch.
import { useEffect, useRef, useState } from 'react';

export type Who = 'claude' | 'codex' | 'trio' | string;

export interface Message {
  id: string;
  at: string;
  from: Who;
  kind: 'message' | 'run_started' | 'run_finished' | 'status';
  summary: string;
  has_detail: boolean;
  items?: string[];
  mentions?: string[];
}

export interface Step { at: string; who: Who; what: string; event: string }

export interface GraphNode {
  id: string;
  kind: 'item' | 'branch';
  lane: Who;
  state: 'open' | 'claimed' | 'review' | 'changes' | 'pass' | 'blocked' | 'branch';
  first: string;
  last: string;
  summary: string;
  touches: number;
  steps: Step[];
  /** Work lens reading, on items only: stage, who it waits on, and a story from the ledger. */
  work?: Work;
}

export type WorkLane = 'asked' | 'progress' | 'review' | 'changes' | 'done';
/** `last`: when the item or a branch implementing it last moved (absent on older servers). */
export interface Work { lane: WorkLane; waiting_on?: string; story: string; last?: string }

/** The Work lens columns, in board order, with what each one means. */
export const WORK_LANES: { lane: WorkLane; title: string; explain: string }[] = [
  { lane: 'asked', title: 'Asked', explain: 'Named in chat, nobody has picked it up yet.' },
  { lane: 'progress', title: 'In progress', explain: 'Someone claimed it, or a branch implements it.' },
  { lane: 'review', title: 'In review', explain: 'A review was asked for and no verdict has come back since.' },
  { lane: 'changes', title: 'Changes', explain: 'The last verdict asked for changes: the work is back with its author.' },
  { lane: 'done', title: 'Done', explain: 'Passed review, or every branch that implements it is merged.' },
];

export interface Edge { from: string; to: string; kind: string }

/** One component a branch changes since its merge-base: a path prefix and lines added + deleted. */
export interface Touch { part: string; lines: number; files: number }

export interface AgentState { enabled: boolean; running: boolean; hue: string; attached?: string | null }

/** What changed since the human was last here (server `seen.rs`). */
export interface Catchup {
  since: string;
  moved: { id: string; title: string; from?: WorkLane; to: WorkLane; yours: boolean }[];
  more: number;
  waiting_on_you: number;
  commits: number;
  branches: number;
  verdicts: number;
  messages: number;
}

export interface Snapshot {
  catchup?: Catchup;
  room: string;
  rev: number;
  paused: boolean;
  hop: number;
  budget: number;
  me: string;
  token: string;
  agents: Record<string, AgentState>;
  messages: Message[];
  graph: {
    items: GraphNode[];
    edges: Edge[];
    now: Record<string, { text: string; at: string; running: boolean }>;
    /** Unmerged branch -> components it changes, largest first. */
    touches?: Record<string, Touch[]>;
    merged?: string[];
  };
}

export function useSnapshot(): Snapshot | null {
  const [snap, setSnap] = useState<Snapshot | null>(null);
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => {
    let alive = true;
    const load = () => {
      window.clearTimeout(timer.current);
      timer.current = window.setTimeout(async () => {
        try {
          const r = await fetch('/api/state');
          if (r.ok && alive) setSnap(await r.json());
        } catch { /* the stream reconnects and triggers another load */ }
      }, 120);
    };
    // A hidden tab drops its stream, so the server can sleep; showing it again reconnects and
    // catches up in one fetch.
    let es: EventSource | null = null;
    const connect = () => {
      if (es || document.hidden) return;
      es = new EventSource('/api/events');
      es.onmessage = load;
    };
    const onVis = () => {
      if (document.hidden) { es?.close(); es = null; } else { connect(); load(); }
    };
    document.addEventListener('visibilitychange', onVis);
    connect();
    load();
    return () => {
      alive = false;
      es?.close();
      document.removeEventListener('visibilitychange', onVis);
      window.clearTimeout(timer.current);
    };
  }, []);
  return snap;
}

export async function fetchDetail(id: string): Promise<string> {
  const r = await fetch(`/api/detail?id=${encodeURIComponent(id)}`);
  return r.ok ? (await r.json()).detail : '';
}

/** Long input becomes summary + detail automatically: first sentence (<=140) up front. */
export function splitForSend(text: string): { text: string; detail?: string } {
  const t = text.trim();
  if (t.length <= 140) return { text: t };
  const m = /^(.{20,138}?[.!?])\s/.exec(t);
  const head = m ? m[1] : t.slice(0, 137).trimEnd() + '…';
  return { text: head, detail: t };
}

export async function send(token: string, text: string): Promise<string | null> {
  const body = { token, ...splitForSend(text) };
  const r = await fetch('/api/say', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
  return r.ok ? null : (await r.text()) || `error ${r.status}`;
}

/** Dismiss the catch-up: the seen mark moves to now. */
export async function markSeen(token: string) {
  await fetch('/api/seen', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ token }) });
}

export async function setPaused(token: string, paused: boolean) {
  await fetch('/api/pause', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ token, paused }) });
}

const AGENTS: Record<string, string> = { claude: 'Claude', codex: 'Codex', trio: 'Trio', both: 'Both' };

/** Display name; the room's human (`snap.me`) is always "You". */
export function nameOf(who: string, me: string): string {
  return who === me ? 'You' : AGENTS[who] ?? who;
}

/** Lane order, used by the chat gutter, the island and the time strip: you, Claude, Codex. */
export const lanes = (me: string) => [me, 'claude', 'codex'];

export function timeLabel(at: string): string {
  const [d, t] = at.split('T');
  const today = new Date().toLocaleDateString('sv'); // local YYYY-MM-DD, same clock as the ledger
  return d === today ? (t ?? '').slice(0, 5) : `${d.slice(5)} ${(t ?? '').slice(0, 5)}`;
}

export const STATE_LABEL: Record<string, string> = {
  open: 'open', claimed: 'claimed', review: 'in review', changes: 'changes requested', pass: 'passed',
  blocked: 'blocked', branch: 'branch', merged: 'merged', component: 'component',
};

export interface Component { part: string; lines: number; files: number; branches: { branch: string; lines: number; files: number }[]; hot: boolean }

/** Components across the given unmerged branches, with who touches each and how much. */
export function components(touches: Record<string, Touch[]>, branches: string[]): Map<string, Component> {
  const out = new Map<string, Component>();
  for (const b of branches) {
    for (const t of touches[b] ?? []) {
      const c = out.get(t.part) ?? { part: t.part, lines: 0, files: 0, branches: [], hot: false };
      c.lines += t.lines;
      c.files += t.files;
      c.branches.push({ branch: b, lines: t.lines, files: t.files });
      c.hot = c.branches.length >= 2;
      out.set(t.part, c);
    }
  }
  return out;
}
