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

/** One agent of the room: `glyph` is the shape it is drawn with, `order` its place in room.toml. */
export interface AgentState {
  enabled: boolean; running: boolean; hue: string; attached?: string | null;
  glyph?: string; harness?: string; model?: string | null; roles?: string[]; order?: number;
}

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
  /** Older history fell out of the server's window: the counts may be short. */
  partial?: boolean;
  /** The room's position when this was built; Dismiss sends it back. */
  upto?: number;
}

/** One agent's availability (server `usage.rs`): its limit windows, spend and state. */
export interface UsageWindow { label: string; used: number; resets?: string }
export interface Usage {
  state: 'ok' | 'low' | 'out';
  until?: string;
  reason?: string;
  windows: UsageWindow[];
  source?: string;
  observed?: string;
  spent_today: number;
  budget?: number;
  held?: number;
}

/** `2026-11-24T15:00` -> `15:00`. */
export const hhmm = (at?: string) => (at ? at.split('T')[1] ?? at : '');

/** "out until 15:00 (5h window used up)", the same words the server's status lines use. */
export function usageWhy(u: Usage): string {
  const until = u.until ? ` until ${hhmm(u.until)}` : '';
  return `${u.state}${until}${u.reason ? ` (${u.reason})` : ''}`;
}

/** A one-line account of where a usage reading came from, for hover text. */
export function usageSource(u: Usage): string {
  const parts = [u.source ? `From the ${u.source}${u.observed ? `, read ${timeLabel(u.observed)}` : ''}.` : 'No window reading yet.'];
  if (u.spent_today > 0 || u.budget) parts.push(`$${u.spent_today.toFixed(2)} today${u.budget ? ` of $${u.budget.toFixed(2)}` : ''}.`);
  return parts.join(' ');
}

export interface Snapshot {
  catchup?: Catchup;
  usage?: Record<string, Usage>;
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
          if (r.ok && alive) {
            const s: Snapshot = await r.json();
            setRoster(s.agents ?? {});
            setSnap(s);
          }
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

/** One of your rooms (server `projects.rs`), for the project switcher. */
export interface RoomSummary {
  name: string;
  path: string;
  current: boolean;
  running: boolean;
  waiting_on_you: number;
  in_flight: number;
  new: number;
  last: string;
}

export async function fetchRooms(): Promise<RoomSummary[]> {
  const r = await fetch('/api/rooms');
  if (!r.ok) throw new Error(`could not read your rooms (${r.status})`);
  return r.json();
}

/** Start (or find) another room's server; resolves to its URL. */
export async function openRoom(token: string, path: string): Promise<string> {
  const r = await fetch('/api/rooms/open', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ token, path, web: true }) });
  if (!r.ok) throw new Error((await r.text()) || `error ${r.status}`);
  return (await r.json()).url;
}

/** Dismiss the catch-up built at `upto`: the seen mark moves to now. The server refuses (409)
 *  when rows arrived since, and the next snapshot shows the newer strip. */
export async function markSeen(token: string, upto?: number) {
  await fetch('/api/seen', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ token, upto }) });
}

export async function setPaused(token: string, paused: boolean) {
  await fetch('/api/pause', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ token, paused }) });
}

const NAMES: Record<string, string> = { claude: 'Claude', codex: 'Codex', trio: 'Trio', both: 'Both', all: 'All', github: 'GitHub' };

/** The room's agents in room.toml order, their glyphs and roles: set from each snapshot. */
let ROSTER: { name: string; glyph: string; roles: string[] }[] = [];

export function setRoster(agents: Record<string, AgentState>) {
  ROSTER = Object.entries(agents)
    .filter(([, a]) => a.enabled || a.attached || a.order !== undefined && a.order < 1000)
    .sort(([an, a], [bn, b]) => (a.order ?? 1000) - (b.order ?? 1000) || an.localeCompare(bn))
    .map(([name, a]) => ({ name, glyph: a.glyph ?? name, roles: a.roles ?? [] }));
}

const capital = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

/** Display name; the room's human (`snap.me`) is always "You". */
export function nameOf(who: string, me: string): string {
  return who === me ? 'You' : NAMES[who] ?? capital(who);
}

/** The shape an agent is drawn with (Claude a circle, Codex a triangle, others by position). */
export function glyphOf(who: string): string {
  return ROSTER.find((r) => r.name === who)?.glyph ?? who;
}

/** The room's agent handles in order; Claude and Codex until a snapshot names the agents. */
export function agentNames(): string[] {
  return ROSTER.length > 0 ? ROSTER.map((r) => r.name) : ['claude', 'codex'];
}

/** The roles an agent holds; a `@<role>` mention addresses each of them. */
export function rolesOf(who: string): string[] {
  return ROSTER.find((r) => r.name === who)?.roles ?? [];
}

/** Every role some agent holds, for `@<role>` mentions. */
export function roleNames(): string[] {
  return [...new Set(ROSTER.flatMap((r) => r.roles))];
}

/** Lane order, used by the chat gutter, the island and the time strip: you, then the agents. */
export const lanes = (me: string) => [me, ...agentNames()];

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
