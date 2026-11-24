//! A room: its config, its sources, a bounded event window, the derived graph, and the agents.
//!
//! Cost model:
//! - tick: one `stat` per source, O(sources); bytes read only when a file grew, O(delta).
//! - apply: O(1) amortized per event (hash index into the window; the window is trimmed in
//!   batches, so the O(n) re-index runs once per `WINDOW / 4` events).
//! - memory: two independent windows of at most `WINDOW * 1.25` events, plus the graph.

use crate::agent::{wake_prompt, Agent, AgentConfig, CliAgent, Guard, WakeDecision};
use crate::graph::{derivers, Deriver, Graph};
use crate::model::{clip, split_reply, Body, Event, Summarized, SUMMARY_MAX};
use crate::source::{append, mentions_in, PostsFile, RoomLedger, Source};
use crate::work::cap;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const WINDOW: usize = 1500;

#[derive(Debug, Deserialize, Default)]
pub struct RoomCfg {
    #[serde(default)]
    pub name: String,
    /// The human's handle in this room (`trio say --as <me>`, `@<me>`). Default: the OS user name.
    #[serde(default)]
    pub me: String,
    #[serde(default = "default_budget")]
    pub hop_budget: u32,
    #[serde(default)]
    pub paused: bool,
    /// Existing thread files to show read-only: `{ file = "codex-posts.md", from = "codex" }`.
    #[serde(default)]
    pub import: Vec<Import>,
    /// Git repo whose branch tips appear in the graph. Unset: the work tree the room sits in.
    /// `repo = ""` turns git off.
    #[serde(default)]
    pub repo: Option<PathBuf>,
    /// Branch that work merges into (default: `main`, else `master`).
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub agents: HashMap<String, AgentConfig>,
    /// Daily spend caps in dollars: `[budget] claude_usd_day = 5.0`, `codex_usd_day = 3.0`.
    #[serde(default)]
    pub budget: Budget,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Budget {
    #[serde(default)]
    pub claude_usd_day: Option<f64>,
    #[serde(default)]
    pub codex_usd_day: Option<f64>,
}

impl Budget {
    pub fn of(&self, agent: &str) -> Option<f64> {
        match agent {
            "claude" => self.claude_usd_day,
            "codex" => self.codex_usd_day,
            _ => None,
        }
    }
}

/// A wake waiting for its agent to be available again.
#[derive(Debug, Clone)]
struct Held {
    agent: String,
    event: Event,
}

/// Held wakes kept at most (the oldest go first).
const HELD_MAX: usize = 20;
/// How often usage readings are re-read from disk.
const READ_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

fn default_budget() -> u32 {
    3
}

/// The other agent, for rerouting.
fn other_agent(a: &str) -> Option<&'static str> {
    match a {
        "claude" => Some("codex"),
        "codex" => Some("claude"),
        _ => None,
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Import {
    pub file: String,
    pub from: String,
}

#[derive(Default)]
struct Window {
    rows: Vec<(u64, Event)>,
    index: HashMap<String, usize>,
    dropped: u64,
}

impl Window {
    fn get(&self, id: &str) -> Option<&Event> {
        self.index.get(id).map(|&i| &self.rows[i].1)
    }

    fn push(&mut self, e: Event, arrival: u64) {
        if let Some(&i) = self.index.get(&e.id) {
            self.rows[i] = (arrival, e);
            return;
        }
        self.index.insert(e.id.clone(), self.rows.len());
        self.rows.push((arrival, e));
        if self.rows.len() > WINDOW + WINDOW / 4 {
            let cut = self.rows.len() - WINDOW;
            for (arrival, _) in self.rows.drain(..cut) {
                self.dropped = self.dropped.max(arrival);
            }
            self.index = self.rows.iter().enumerate().map(|(i, (_, e))| (e.id.clone(), i)).collect();
        }
    }

    fn events(&self) -> impl DoubleEndedIterator<Item = &Event> {
        self.rows.iter().map(|(_, e)| e)
    }
}

pub struct Room {
    pub root: PathBuf,
    pub ledger: PathBuf,
    pub cfg: RoomCfg,
    sources: Vec<Box<dyn Source + Send>>,
    chat: Window,
    git_events: Window,
    arrival: u64,
    pub graph: Graph,
    derivers: Vec<Box<dyn Deriver>>,
    pub rev: u64,
    /// Which sessions hold the agents, as last seen; a change is published like an event.
    presence: String,
    pub agents: HashMap<String, CliAgent>,
    started: HashMap<String, Instant>,
    hop: u32,
    live: bool,
    /// Branch tips for the map, read off the room lock (None without `repo`).
    git: Option<crate::gitwatch::GitWatch>,
    git_seeded: bool,
    /// Failed writes stay in order for the next tick.
    pending_git: VecDeque<Event>,
    /// The mark saved by an earlier run, until the first ingest turns it into this run's mark.
    restored: Option<crate::seen::Seen>,
    /// Per source: how many ids it has yielded, for stable positions.
    counts: Vec<u64>,
    trio_exe: String,
    /// When the human was last here, for the catch-up (`seen.rs`).
    pub seen: Option<crate::seen::Seen>,
    /// Latest window reading per agent, and when it was read (`usage.rs`).
    readings: HashMap<String, crate::usage::Reading>,
    read_at: Option<Instant>,
    /// Agents stopped by a usage limit, until the reset their run named.
    stops: HashMap<String, crate::usage::Stop>,
    /// Wakes waiting for their agent to come back.
    held: Vec<Held>,
    /// The message each running agent was woken for, so a limit hit can hand it on.
    triggers: HashMap<String, Event>,
}

#[derive(Serialize)]
pub struct MessageView<'a> {
    id: &'a str,
    at: &'a str,
    from: &'a str,
    kind: &'static str,
    summary: String,
    has_detail: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    items: Vec<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    mentions: Vec<&'a str>,
}

impl Room {
    pub fn open(root: &Path) -> io::Result<Self> {
        let root = root.canonicalize()?;
        let dir = root.join(".trio");
        let cfg: RoomCfg = match std::fs::read_to_string(dir.join("room.toml")) {
            Ok(t) => toml::from_str(&t).map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => RoomCfg { hop_budget: 3, ..Default::default() },
            Err(e) => return Err(e),
        };
        let ledger = dir.join("room.jsonl");
        let mut sources: Vec<Box<dyn Source + Send>> = Vec::new();
        for imp in &cfg.import {
            sources.push(Box::new(PostsFile::new(root.join(&imp.file), &imp.from, &human(&cfg))));
        }
        sources.push(Box::new(RoomLedger::new(ledger.clone())));
        let hues = [("claude", "clay"), ("codex", "moss")];
        let agents = cfg
            .agents
            .iter()
            .map(|(name, c)| {
                let hue = hues.iter().find(|(n, _)| n == name).map(|(_, h)| *h).unwrap_or("dusk");
                (name.clone(), CliAgent::new(name, hue, c.clone()))
            })
            .collect();
        let trio_exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "trio".into());
        let repo = match &cfg.repo {
            Some(r) if r.as_os_str().is_empty() => None,
            Some(r) => Some(root.join(r)),
            None => git_root(&root),
        };
        let git = repo.map(|r| crate::gitwatch::GitWatch::start(r, cfg.base.clone()));
        let restored = crate::seen::load(&root);
        Ok(Self {
            root,
            ledger,
            cfg,
            sources,
            chat: Window::default(),
            git_events: Window::default(),
            arrival: 0,
            graph: Graph::default(),
            derivers: derivers(),
            rev: 0,
            presence: String::new(),
            agents,
            started: HashMap::new(),
            hop: 0,
            live: false,
            git,
            git_seeded: false,
            pending_git: VecDeque::new(),
            restored,
            counts: Vec::new(),
            trio_exe,
            seen: None,
            readings: HashMap::new(),
            read_at: None,
            stops: HashMap::new(),
            held: Vec::new(),
            triggers: HashMap::new(),
        })
    }

    fn activity(&self) -> impl Iterator<Item = &(u64, Event)> + Clone {
        self.chat.rows.iter().chain(&self.git_events.rows)
    }

    pub fn catchup(&self, work: &HashMap<String, crate::work::Work>) -> Option<crate::seen::Catchup> {
        let seen = self.seen.as_ref()?;
        let partial = self.chat.dropped.max(self.git_events.dropped) > seen.position;
        crate::seen::catchup(seen, &self.graph, work, self.activity(), &self.me(), partial)
    }

    /// Move the seen mark to now, with every item's current lane, and publish the change.
    pub fn mark_seen(&mut self) -> io::Result<()> {
        let s = self.seen_now();
        crate::seen::save(&self.root, &s)?;
        self.seen = Some(s);
        self.rev += 1;
        Ok(())
    }

    fn seen_now(&self) -> crate::seen::Seen {
        let sources = self.counts.iter().enumerate().filter(|(_, c)| **c > 0).map(|(i, c)| (self.source_key(i), *c)).collect();
        crate::seen::mark(&now(), self.arrival, &crate::work::all(&self.graph, &self.me()), sources)
    }

    /// A name for source `i` that survives reordering the imports in room.toml.
    fn source_key(&self, i: usize) -> String {
        match self.cfg.import.get(i) {
            Some(imp) => format!("import:{}", imp.file),
            None if i + 1 == self.sources.len() => "ledger".into(),
            None => "git".into(),
        }
    }

    /// Whether the event at `pos` was already there at a saved mark. A mark from before
    /// per-source counts falls back to its time.
    fn before_mark(&self, mark: &crate::seen::Seen, e: &Event, pos: u64) -> bool {
        if mark.sources.is_empty() {
            return e.at <= mark.at;
        }
        let i = ((pos >> 40) as usize).saturating_sub(1);
        let n = pos & ((1u64 << 40) - 1);
        n <= mark.sources.get(&self.source_key(i)).copied().unwrap_or(0)
    }

    /// The human's handle: `me` from room.toml when usable, else the OS user name.
    pub fn me(&self) -> String {
        human(&self.cfg)
    }

    pub fn name(&self) -> String {
        if self.cfg.name.is_empty() {
            self.root.file_name().and_then(|n| n.to_str()).unwrap_or("room").to_string()
        } else {
            self.cfg.name.clone()
        }
    }

    /// Poll every source, apply new events, route mentions, reap finished runs.
    /// Returns true when anything changed. The first call loads history without waking anyone.
    pub fn tick(&mut self) -> bool {
        let mut changed = self.ingest();
        // A reaped reply was just appended to the ledger. Read and route it now: the loop may
        // sleep after this tick (no page, no live run), and a mention in it must still wake the
        // other agent. Bounded: each pass needs a run to finish, and there are two agents.
        while self.reap() {
            changed = true;
            self.ingest();
        }
        self.refresh_usage();
        if self.release_held() {
            changed = true;
        }
        // Joining or leaving writes no ledger line, so compare the attachment markers too.
        let presence = self.presence();
        if presence != self.presence {
            self.presence = presence;
            changed = true;
        }
        self.live = true;
        if changed {
            self.rev += 1;
        }
        changed
    }

    /// Stable position for an event from source `i`: sources in
    /// config order, then each source's own order. Same-minute events from different sources
    /// therefore order the same way live and on replay; within a file, file order decides.
    fn position(&mut self, i: usize, id: &str) -> u64 {
        if let Some(pos) = self.graph.position(id) {
            return pos;
        }
        if self.counts.len() <= i {
            self.counts.resize(i + 1, 0);
        }
        self.counts[i] += 1;
        ((i as u64 + 1) << 40) | self.counts[i]
    }

    /// Poll every source, apply new events and route new ledger messages.
    fn ingest(&mut self) -> bool {
        let mut fresh: Vec<(Event, bool, u64)> = Vec::new();
        let n = self.sources.len();
        for i in 0..n {
            self.poll_source(i, &mut fresh);
        }
        let mut changed;
        if self.seen.is_none() {
            // Arrival positions start over in each process. With a saved mark, history up to it
            // arrives first and the rest after the new mark, so what came in while Trio was
            // closed is the catch-up. Without one, all history is the baseline.
            let restored = self.restored.take();
            let (old, new): (Vec<_>, Vec<_>) = fresh.into_iter().partition(|(e, _, pos)| restored.as_ref().is_none_or(|m| self.before_mark(m, e, *pos)));
            changed = self.apply_batch(old);
            let mut mark = self.seen_now();
            if let Some(m) = restored {
                (mark.at, mark.lanes, mark.sources) = (m.at, m.lanes, m.sources);
            }
            self.seen = Some(mark);
            changed |= self.apply_batch(new);
        } else {
            changed = self.apply_batch(fresh);
        }
        let seed = (self.git.is_some() && !self.git_seeded).then(|| crate::gitwatch::Seed {
            serials: self.graph.git_serials.clone(),
            tips: self.graph.items.values().filter(|b| !b.tip_sha.is_empty())
                .map(|b| (b.id.clone(), b.tip_sha.clone(), b.title.clone(), self.graph.merged.contains(&b.id))).collect(),
        });
        self.git_seeded |= seed.is_some();
        let polls = self.git.as_mut().map(|g| g.take(seed)).unwrap_or_default();
        for p in polls {
            changed |= self.apply_git_poll(p);
        }
        let mut fresh = Vec::new();
        self.flush_git(n - 1, &mut fresh);
        changed | self.apply_batch(fresh)
    }

    fn apply_git_poll(&mut self, mut poll: crate::gitwatch::Poll) -> bool {
        // A branch without an agent prefix is the human's.
        let me = self.me();
        for e in poll.durable.iter_mut().chain(poll.tips.iter_mut()).filter(|e| e.from.is_empty()) {
            e.from = me.clone();
        }
        self.pending_git.extend(poll.durable);
        let mut changed = false;
        for e in poll.tips {
            let pos = self.position(self.sources.len(), &e.id);
            changed |= self.apply(e, pos);
        }
        changed
    }

    fn flush_git(&mut self, ledger_index: usize, fresh: &mut Vec<(Event, bool, u64)>) {
        let mut appended = std::collections::HashSet::new();
        while let Some(e) = self.pending_git.front() {
            if !self.graph.has_event(&e.id) && !appended.contains(&e.id) {
                if let Err(err) = append(&self.ledger, e) {
                    eprintln!("trio: could not record {}: {err}", e.id);
                    break;
                }
                appended.insert(e.id.clone());
            }
            self.pending_git.pop_front();
        }
        if !appended.is_empty() {
            self.poll_source(ledger_index, fresh);
        }
    }

    fn apply_batch(&mut self, mut fresh: Vec<(Event, bool, u64)>) -> bool {
        let mut changed = false;
        fresh.sort_by(|a, b| crate::gitwatch::replay_at(&a.0).cmp(crate::gitwatch::replay_at(&b.0)).then(a.2.cmp(&b.2)));
        for (e, from_ledger, pos) in fresh {
            let route = self.live && from_ledger && matches!(e.body, Body::Message { .. }) && !self.graph.has_event(&e.id);
            if route {
                changed |= self.apply(e.clone(), pos);
                self.route(&e);
            } else {
                changed |= self.apply(e, pos);
            }
        }
        changed
    }

    fn poll_source(&mut self, i: usize, fresh: &mut Vec<(Event, bool, u64)>) {
        let from_ledger = i == self.sources.len() - 1;
        if let Ok(evs) = self.sources[i].poll() {
            for e in evs {
                let pos = self.position(i, &e.id);
                fresh.push((e, from_ledger, pos));
            }
        }
    }

    fn apply(&mut self, e: Event, pos: u64) -> bool {
        if let Body::Commit { branch, sha, touches, .. } = &e.body
            && crate::gitwatch::is_refresh(&e)
        {
            if self.graph.items.get(branch).is_none_or(|b| b.tip_sha != *sha || b.merged_at.is_some())
                || self.graph.touches.get(branch) == Some(touches)
            {
                return false;
            }
            self.graph.apply_at(&e, Some(pos), &self.derivers);
            return true;
        }
        let window = if matches!(e.body, Body::Commit { .. }) {
            if crate::gitwatch::is_transition(&e) && self.graph.has_event(&e.id) {
                return false;
            }
            &mut self.git_events
        } else {
            &mut self.chat
        };
        if window.get(&e.id) == Some(&e) {
            return false;
        }
        self.graph.apply_at(&e, Some(pos), &self.derivers);
        self.arrival += 1;
        window.push(e, self.arrival);
        true
    }

    /// Append to the ledger (the file is the source of truth; the next tick reads it back).
    pub fn say(&self, from: &str, summary: &str, detail: Option<String>) -> Result<(), String> {
        let summary = summary.trim();
        if summary.is_empty() {
            return Err("empty message".into());
        }
        if summary.chars().count() > SUMMARY_MAX {
            return Err(format!("summary is over {SUMMARY_MAX} chars; put the rest in detail"));
        }
        let text = format!("{summary}\n{}", detail.as_deref().unwrap_or(""));
        let e = Event {
            id: String::new(),
            at: now(),
            from: from.to_string(),
            body: Body::Message { summary: summary.to_string(), detail, mentions: mentions_in(&text, &self.me()), items: crate::model::item_ids(&text) },
        };
        append(&self.ledger, &e).map_err(|e| e.to_string())
    }

    fn record(&self, from: &str, body: Body) {
        let _ = append(&self.ledger, &Event { id: String::new(), at: now(), from: from.to_string(), body });
    }

    fn route(&mut self, e: &Event) {
        let Body::Message { mentions, .. } = &e.body else { return };
        let me = self.me();
        if e.from == me {
            self.hop = 0;
        } else {
            self.hop += 1;
        }
        let mut targets: Vec<String> = mentions.iter().filter(|m| m.as_str() != "both" && m.as_str() != me).cloned().collect();
        if mentions.iter().any(|m| m == "both") {
            targets.extend(["claude".to_string(), "codex".to_string()]);
        }
        targets.retain(|t| t != &e.from);
        targets.dedup();
        let recent: Vec<String> = self
            .chat.events()
            .rev()
            .filter(|x| matches!(x.body, Body::Message { .. }))
            .take(8)
            .map(|x| format!("{}: {}", x.from, x.summary()))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let detail = match &e.body {
            Body::Message { detail, .. } => detail.as_deref(),
            _ => None,
        };
        for t in &targets {
            // Attached agents are pull-on-turn: the mention reaches them with their next message.
            if let Some(a) = crate::hook::attached(&self.root, t) {
                self.record("trio", Body::Status { text: format!("{t} is attached: waiting on you in {}", a.surface.label()) });
                continue;
            }
            self.dispatch(t, e, &recent, detail, &targets);
        }
    }

    /// Wake `t` for `e`, by its usage. Out (limit, used-up window or spent budget): the other
    /// agent takes it, with the item history, unless that would have the author review its own
    /// work; then the wake is held until `t` is back. Low: woken anyway, with a warning line.
    fn dispatch(&mut self, t: &str, e: &Event, recent: &[String], detail: Option<&str>, targets: &[String]) {
        let u = self.usage(t);
        if !u.out() {
            if u.state == "low" {
                self.record("trio", Body::Status { text: clip(&format!("{t} is low ({}); woken anyway", u.reason.as_deref().unwrap_or("")), 120) });
            }
            self.wake(t, e, recent, detail, &[]);
            return;
        }
        let other = other_agent(t).filter(|o| self.agents.get(*o).is_some_and(|a| a.cfg.enabled) && !self.usage(o).out());
        match other {
            Some(o) if targets.iter().any(|x| x == o) => {
                self.record("trio", Body::Status { text: clip(&format!("{t} is {}; {} already has this", u.why(), cap(o)), 120) });
            }
            Some(o) if !self.reviews_own_work(e, o) => {
                self.record("trio", Body::Status { text: clip(&format!("routed to {}: {} is {}", cap(o), cap(t), u.why()), 120) });
                let note = self.handoff(t, &u, e);
                self.wake(o, e, recent, detail, &note);
            }
            Some(o) => {
                self.hold(t, e);
                self.record("trio", Body::Status { text: clip(&format!("held for {}: {} wrote it, so its review waits until {} is back ({})", cap(t), cap(o), cap(t), u.why()), 120) });
            }
            None => {
                self.hold(t, e);
                self.record("trio", Body::Status { text: clip(&format!("held for {}: {} is {}", cap(t), cap(t), u.why()), 120) });
            }
        }
    }

    fn wake(&mut self, t: &str, e: &Event, recent: &[String], detail: Option<&str>, handoff: &[String]) {
        let me = self.me();
        let room = self.root.display().to_string();
        let guard = Guard { paused: self.cfg.paused, hop: self.hop, budget: self.cfg.hop_budget, from_human: e.from == me };
        let mut since = handoff.to_vec();
        since.extend(crate::seen::digest(self.activity(), t));
        let prompt = wake_prompt(t, &me, &room, recent, &since, detail);
        let decision = match self.agents.get_mut(t) {
            Some(a) => a.wake_if_allowed(&prompt, &guard),
            None => WakeDecision::Disabled,
        };
        match decision {
            WakeDecision::Started => {
                self.started.insert(t.to_string(), Instant::now());
                self.triggers.insert(t.to_string(), e.clone());
                self.record(&e.from, Body::RunStarted { agent: t.to_string(), hop: self.hop, trigger: e.id.clone() });
            }
            other => self.record("trio", Body::Status { text: clip(&format!("{t} not woken: {other:?}"), 120) }),
        }
    }

    fn hold(&mut self, t: &str, e: &Event) {
        if !self.held.iter().any(|h| h.agent == t && h.event.id == e.id) {
            self.held.push(Held { agent: t.to_string(), event: e.clone() });
            if self.held.len() > HELD_MAX {
                self.held.remove(0);
            }
        }
    }

    /// Wake held agents that are back. True when any was woken.
    fn release_held(&mut self) -> bool {
        let back: Vec<bool> = self.held.iter().map(|h| !self.usage(&h.agent).out()).collect();
        if !back.contains(&true) {
            return false;
        }
        let mut ready: Vec<Held> = Vec::new();
        for (h, is_back) in std::mem::take(&mut self.held).into_iter().zip(back) {
            if is_back { ready.push(h) } else { self.held.push(h) }
        }
        let recent: Vec<String> = self.recent();
        for h in ready {
            self.record("trio", Body::Status { text: format!("{} is back: waking it for the held message", cap(&h.agent)) });
            let detail = h.event.detail().map(str::to_string);
            self.wake(&h.agent, &h.event, &recent, detail.as_deref(), &[]);
        }
        true
    }

    /// The newest eight chat lines, oldest first, for a wake prompt.
    fn recent(&self) -> Vec<String> {
        let mut v: Vec<String> = self.chat.events().rev().filter(|x| matches!(x.body, Body::Message { .. })).take(8).map(|x| format!("{}: {}", x.from, x.summary())).collect();
        v.reverse();
        v
    }

    /// True when `e` asks for a review of an item that `author` wrote.
    fn reviews_own_work(&self, e: &Event, author: &str) -> bool {
        let Body::Message { items, .. } = &e.body else { return false };
        items.iter().any(|id| {
            self.graph.items.get(id).is_some_and(|it| it.review_request(&e.id))
                && crate::work::author_of(&self.graph, id).as_deref() == Some(author)
        })
    }

    /// For a rerouted wake: who it was for, why they are out, and each item's story so far.
    fn handoff(&self, t: &str, u: &crate::usage::Usage, e: &Event) -> Vec<String> {
        let mut out = vec![format!("This message was for {}, who is {}. You are taking it instead.", cap(t), u.why())];
        if let Body::Message { items, .. } = &e.body {
            let work = crate::work::all(&self.graph, &self.me());
            for id in items.iter().take(3) {
                if let (Some(it), Some(w)) = (self.graph.items.get(id), work.get(id)) {
                    out.push(clip(&format!("{id} ({}): {}", it.title, w.story), 400));
                }
            }
        }
        out
    }

    /// Re-read usage sources at most every READ_EVERY (two small reads, off the hot path).
    fn refresh_usage(&mut self) {
        if cfg!(test) || self.read_at.is_some_and(|t| t.elapsed() < READ_EVERY) {
            return;
        }
        self.read_at = Some(Instant::now());
        if let Some(r) = crate::usage::claude_file().and_then(|p| crate::usage::claude_reading(&p)) {
            self.readings.insert("claude".into(), r);
        }
        let sessions = crate::usage::home().map(|h| h.join(".codex").join("sessions"));
        if let Some(r) = sessions.and_then(|s| crate::usage::codex_latest_log(&s)).and_then(|l| crate::usage::codex_reading(&l)) {
            self.readings.insert("codex".into(), r);
        }
    }

    /// Dollars this room's runs of `agent` cost today (estimated for Codex).
    fn spent_today(&self, agent: &str) -> f64 {
        let today = &now()[..10];
        self.chat.events()
            .filter(|e| e.at.starts_with(today))
            .filter_map(|e| match &e.body {
                Body::RunFinished { agent: a, cost: Some(c), .. } if a == agent => Some(*c),
                _ => None,
            })
            .sum()
    }

    /// `agent`'s availability now: ok, low or out, with why and until when.
    pub fn usage(&self, agent: &str) -> crate::usage::Usage {
        crate::usage::assess(self.readings.get(agent), self.stops.get(agent), self.spent_today(agent), self.cfg.budget.of(agent), crate::usage::epoch_now())
    }

    /// Wakes held, per agent, for the snapshot.
    pub fn held_for(&self, agent: &str) -> usize {
        self.held.iter().filter(|h| h.agent == agent).count()
    }

    fn reap(&mut self) -> bool {
        let mut changed = false;
        let names: Vec<String> = self.agents.keys().cloned().collect();
        for n in names {
            let a = self.agents.get_mut(&n).expect("agent");
            let secs = self.started.get(&n).map(|t| t.elapsed().as_secs()).unwrap_or(0);
            if a.is_running() && secs > 20 * 60 {
                a.kill();
            }
            if let Some(r) = a.reap() {
                self.started.remove(&n);
                let trigger = self.triggers.remove(&n);
                self.record(&n, Body::RunFinished { agent: n.clone(), exit: r.exit, secs, cost: r.cost });
                // A usage limit: the agent is out until its reset, and the message it was woken
                // for goes on by the same rules as a fresh one (rerouted or held).
                if let Some(stop) = r.stop.clone() {
                    self.stops.insert(n.clone(), stop);
                    let u = self.usage(&n);
                    self.record("trio", Body::Status { text: clip(&format!("{n} hit a usage limit: {}", u.why()), 120) });
                    if let Some(e) = trigger {
                        let recent = self.recent();
                        let detail = e.detail().map(str::to_string);
                        self.dispatch(&n, &e, &recent, detail.as_deref(), &[]);
                    }
                    changed = true;
                    continue;
                }
                // Only a successful, complete final answer becomes the agent's chat message (and
                // can wake the next agent). Anything else is a status line, never a message.
                match r.reply.as_deref().and_then(split_reply) {
                    Some((summary, detail)) => {
                        if let Err(err) = self.say(&n, &summary, detail) {
                            self.record("trio", Body::Status { text: clip(&format!("{n} reply dropped: {err}"), 120) });
                        }
                    }
                    None => {
                        let why = r.problem.unwrap_or_else(|| "finished without a reply".into());
                        self.record("trio", Body::Status { text: clip(&format!("{n} {why}"), 120) });
                    }
                }
                changed = true;
            }
        }
        changed
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.cfg.paused = paused;
        self.rev += 1;
    }

    pub fn detail(&self, id: &str) -> Option<&str> {
        self.chat.get(id).and_then(Event::detail)
    }

    /// The chat view: the newest `limit` messages and run events, oldest first.
    pub fn messages(&self, limit: usize) -> Vec<MessageView<'_>> {
        let mut out: Vec<&Event> = self.chat.events().filter(|e| match &e.body {
            Body::Claim { .. } | Body::Verdict { .. } => false,
            Body::Status { text } if text.is_empty() && e.id.ends_with("#status") => false,
            _ => true,
        }).collect();
        out.sort_by(|a, b| a.at.cmp(&b.at));
        let skip = out.len().saturating_sub(limit);
        out[skip..]
            .iter()
            .map(|e| MessageView {
                id: &e.id,
                at: &e.at,
                from: &e.from,
                kind: crate::model::Entity::kind(*e),
                summary: match &e.body {
                    Body::RunStarted { agent, hop, .. } => format!("woke {agent} (hop {hop})"),
                    Body::RunFinished { agent, exit, secs, cost } => match cost {
                        Some(c) => format!("{agent} finished in {secs}s (exit {exit}, ${c:.2})"),
                        None => format!("{agent} finished in {secs}s (exit {exit})"),
                    },
                    _ => e.summary(),
                },
                has_detail: e.detail().is_some_and(|d| !d.trim().is_empty()),
                items: match &e.body {
                    Body::Message { items, .. } => items.iter().map(String::as_str).collect(),
                    _ => Vec::new(),
                },
                mentions: match &e.body {
                    Body::Message { mentions, .. } => mentions.iter().map(String::as_str).collect(),
                    _ => Vec::new(),
                },
            })
            .collect()
    }

    fn presence(&self) -> String {
        ["claude", "codex"]
            .iter()
            .map(|n| crate::hook::attached(&self.root, n).and_then(|a| serde_json::to_string(&a).ok()).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("|")
    }

    pub fn agent_states(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        for (n, a) in &self.agents {
            m.insert(n.clone(), serde_json::json!({ "enabled": a.cfg.enabled, "running": a.is_running(), "hue": crate::agent::Participant::hue(a) }));
        }
        // Attached sessions show up whether or not a headless agent is configured.
        for n in ["claude", "codex"] {
            let attached = crate::hook::attached(&self.root, n).map(|a| a.surface);
            let entry = m.entry(n.to_string()).or_insert_with(|| serde_json::json!({ "enabled": false, "running": false, "hue": "dusk" }));
            entry["attached"] = serde_json::json!(attached);
        }
        serde_json::Value::Object(m)
    }

    pub fn any_running(&self) -> bool {
        self.agents.values().any(|a| a.is_running())
    }

    pub fn hop(&self) -> u32 {
        self.hop
    }
}

/// The room's human handle. Agent names and `trio` are refused so routing stays unambiguous.
/// The top of the git work tree `dir` is in, if any.
pub fn git_root(dir: &Path) -> Option<PathBuf> {
    crate::gitwatch::git(dir, &["rev-parse", "--show-toplevel"]).filter(|s| !s.is_empty()).map(PathBuf::from)
}

pub fn human(cfg: &RoomCfg) -> String {
    let clean = |s: &str| -> Option<String> {
        let s: String = s.trim().to_ascii_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        (!s.is_empty() && !["claude", "codex", "both", "trio"].contains(&s.as_str())).then_some(s)
    };
    clean(&cfg.me)
        .or_else(|| std::env::var("USERNAME").ok().or_else(|| std::env::var("USER").ok()).and_then(|u| clean(&u)))
        .unwrap_or_else(|| "you".into())
}

/// Local wall-clock time as `YYYY-MM-DDTHH:MM`, matching the thread headers.
pub fn now() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_room(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("trio-room-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(root.join(".trio")).unwrap();
        root
    }

    fn message(id: &str, at: &str, from: &str, summary: &str) -> Event {
        Event { id: id.into(), at: at.into(), from: from.into(), body: Body::Message {
            summary: summary.into(), detail: None, mentions: vec![], items: crate::model::item_ids(summary),
        } }
    }

    fn commit(id: &str, at: &str, branch: &str, sha: &str, subject: &str, merged: bool, gone: bool) -> Event {
        Event { id: id.into(), at: at.into(), from: "claude".into(), body: Body::Commit {
            branch: branch.into(), sha: sha.into(), subject: subject.into(), touches: vec![], merged, gone,
        } }
    }

    #[test]
    fn a_growing_import_retracts_its_ask_live_and_on_cold_replay() {
        use std::io::Write;
        let root = temp_room("grow");
        std::fs::write(root.join(".trio/room.toml"), "me = 'sanjith'\n[[import]]\nfile = 'claude-posts.md'\nfrom = 'claude'\n").unwrap();
        let file = root.join("claude-posts.md");
        std::fs::write(&file, "## 2026-11-23 09:00 @codex please review XF1\nStatus: awaiting review\n").unwrap();
        let mut live = Room::open(&root).unwrap();
        live.tick();
        assert_eq!(crate::work::all(&live.graph, "sanjith")["XF1"].lane, "review");
        assert_eq!(live.graph.now["claude"].text, "awaiting review");
        writeln!(std::fs::OpenOptions::new().append(true).open(file).unwrap(), "verdict XF2: pass").unwrap();
        live.tick();
        let mut cold = Room::open(&root).unwrap();
        cold.tick();
        for room in [&live, &cold] {
            let work = crate::work::all(&room.graph, "sanjith");
            assert_eq!((work["XF1"].lane, work["XF2"].lane), ("asked", "done"));
            assert!(!room.graph.now.contains_key("claude"));
        }
        assert_eq!(serde_json::to_value(&live.graph).unwrap(), serde_json::to_value(&cold.graph).unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_seen_mark_keeps_same_minute_and_backdated_arrivals() {
        let root = temp_room("arrival");
        let mut room = Room::open(&root).unwrap();
        room.tick();
        room.mark_seen().unwrap();
        let at = room.seen.as_ref().unwrap().at.clone();
        append(&room.ledger, &message("same", &at, "claude", "XF1 just arrived")).unwrap();
        append(&room.ledger, &message("late", "2026-01-01T09:00", "claude", "XF2 was imported late")).unwrap();
        room.tick();
        let work = crate::work::all(&room.graph, &room.me());
        let c = room.catchup(&work).unwrap();
        assert_eq!((c.messages, c.moved.len(), c.partial), (2, 2, false));
        assert_eq!(room.seen.as_ref().unwrap().position, 0, "ingest cannot advance the mark");
        room.mark_seen().unwrap();
        assert!(room.catchup(&work).is_none());
        let mut cold = Room::open(&root).unwrap();
        cold.tick();
        assert_eq!(cold.seen.as_ref().unwrap().position, 2);
        assert!(cold.catchup(&work).is_none(), "a restart keeps the saved mark");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn work_done_while_trio_was_closed_is_the_catch_up() {
        let root = temp_room("away");
        let mut room = Room::open(&root).unwrap();
        append(&room.ledger, &message("before", "2026-11-23T09:00", "claude", "XF1 started")).unwrap();
        room.tick();
        room.mark_seen().unwrap();
        drop(room);
        // Trio is closed; an agent keeps working, one row backdated before the mark's time.
        let ledger = root.join(".trio").join("room.jsonl");
        append(&ledger, &message("away1", "2026-11-23T08:00", "codex", "XF2 opened while you were out")).unwrap();
        append(&ledger, &message("away2", "2099-01-01T09:00", "claude", "XF1 ready for review")).unwrap();
        let mut back = Room::open(&root).unwrap();
        back.tick();
        let work = crate::work::all(&back.graph, &back.me());
        let c = back.catchup(&work).expect("what happened while Trio was closed");
        assert_eq!(c.messages, 2, "both rows written while closed, whatever their times");
        assert!(c.moved.iter().any(|m| m.id == "XF2"), "{c:?}");
        back.mark_seen().unwrap();
        drop(back);
        let mut again = Room::open(&root).unwrap();
        again.tick();
        assert!(again.catchup(&crate::work::all(&again.graph, &again.me())).is_none(), "nothing new since the last mark");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn git_churn_keeps_chat_and_runs_and_marks_catchup_partial() {
        let root = temp_room("windows");
        let mut room = Room::open(&root).unwrap();
        room.tick();
        let at = "2026-11-23T09:00";
        room.apply(message("chat", at, "claude", "still here"), 1);
        room.apply(Event { id: "run".into(), at: at.into(), from: "claude".into(),
            body: Body::RunStarted { agent: "codex".into(), hop: 1, trigger: "chat".into() },
        }, 2);
        room.mark_seen().unwrap();
        for i in 1..=WINDOW + WINDOW / 4 + 1 {
            room.apply(commit(&format!("git:claude/x:tip:{i}"), at, "claude/x", &i.to_string(), "work", false, false), i as u64 + 2);
        }
        assert_eq!(room.messages(10).iter().map(|m| m.id).collect::<Vec<_>>(), ["chat", "run"]);
        let c = room.catchup(&crate::work::all(&room.graph, &room.me())).unwrap();
        assert_eq!((c.commits, c.branches, c.messages, c.partial), (WINDOW, 1, 0, true));
        assert_eq!(crate::seen::digest(room.activity(), "codex").len(), 6, "the run survived to anchor the digest");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn two_durable_merge_rows_use_the_second_sha_and_clock_on_replay() {
        let root = temp_room("second-merge");
        let mut room = Room::open(&root).unwrap();
        let ledger = room.ledger.clone();
        append(&ledger, &commit("git:claude/x:merged:old", "2026-11-23T09:00", "claude/x", "old", "XF1 work", true, false)).unwrap();
        append(&ledger, &Event { id: "changes".into(), at: "2026-11-23T10:00".into(), from: "codex".into(),
            body: Body::Verdict { item: "XF1".into(), outcome: "changes".into() },
        }).unwrap();
        append(&ledger, &commit("git:claude/x:merged:new", "2026-11-23T12:00", "claude/x", "new", "XF1 more", true, false)).unwrap();
        room.tick();
        assert_eq!(room.graph.items["claude/x"].merged_at.as_ref().unwrap().0, "2026-11-23T12:00");
        assert_eq!(crate::work::all(&room.graph, &room.me())["XF1"].lane, "done");
        let _ = std::fs::remove_dir_all(root);
    }

    fn git(root: &Path, args: &[&str]) -> String {
        crate::gitwatch::git(root, args).unwrap_or_else(|| panic!("git {args:?}"))
    }

    fn git_room(name: &str) -> PathBuf {
        let root = temp_room(name);
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        git(&root, &["commit", "--allow-empty", "-qm", "base"]);
        let repo = toml::Value::String(root.display().to_string());
        std::fs::write(root.join(".trio/room.toml"), format!("repo = {repo}\nme = 'sanjith'\n")).unwrap();
        root
    }

    fn poll_git(room: &mut Room) -> Vec<Event> {
        let poll = room.git.as_mut().unwrap().wait(Duration::from_secs(20)).unwrap();
        let durable = poll.durable.clone();
        room.apply_git_poll(poll);
        room.tick();
        durable
    }

    fn ledger_rows(room: &Room) -> Vec<Event> {
        RoomLedger::new(room.ledger.clone()).poll().unwrap()
    }

    #[test]
    fn a_first_sight_merge_is_durable_through_deletion_and_restart() {
        let root = git_room("first-merge");
        git(&root, &["checkout", "-qb", "claude/x"]);
        let output = std::process::Command::new("git").arg("-C").arg(&root)
            .env("GIT_COMMITTER_DATE", "2026-01-02T12:00:00+00:00")
            .args(["commit", "--allow-empty", "-qm", "XF1 implemented"]).output().unwrap();
        assert!(output.status.success());
        git(&root, &["checkout", "-q", "main"]);
        git(&root, &["merge", "-q", "--ff-only", "claude/x"]);
        let mut live = Room::open(&root).unwrap();
        live.tick();
        let rows = poll_git(&mut live);
        assert_eq!(rows.len(), 1);
        assert!(crate::gitwatch::is_merge(&rows[0]));
        let stamp = git(&root, &["for-each-ref", "--format=%(committerdate:format-local:%Y-%m-%dT%H:%M)", "refs/heads/claude/x"]);
        assert_eq!(rows[0].at, stamp);
        git(&root, &["branch", "-D", "claude/x"]);
        assert_eq!(poll_git(&mut live).len(), 1);
        assert_eq!(ledger_rows(&live).len(), 2);
        drop(live);
        let mut cold = Room::open(&root).unwrap();
        cold.tick();
        assert!(poll_git(&mut cold).is_empty());
        assert!(!cold.graph.items.contains_key("claude/x"));
        let work = crate::work::all(&cold.graph, &cold.me());
        assert_eq!((work["XF1"].lane, work["XF1"].last.as_str()), ("done", stamp.as_str()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_second_observed_merge_after_changes_survives_cold_replay() {
        let root = git_room("merge-again");
        git(&root, &["checkout", "-qb", "claude/x"]);
        git(&root, &["commit", "--allow-empty", "-qm", "XF1 work"]);
        git(&root, &["checkout", "-q", "main"]);
        let mut live = Room::open(&root).unwrap();
        live.tick();
        poll_git(&mut live);
        git(&root, &["merge", "-q", "--ff-only", "claude/x"]);
        let first = poll_git(&mut live).pop().unwrap();
        assert!(crate::gitwatch::is_merge(&first));
        append(&live.ledger, &Event { id: "changes".into(), at: first.at.clone(), from: "codex".into(),
            body: Body::Verdict { item: "XF1".into(), outcome: "changes".into() },
        }).unwrap();
        live.tick();
        assert_eq!(crate::work::all(&live.graph, &live.me())["XF1"].lane, "changes");
        git(&root, &["checkout", "-q", "claude/x"]);
        git(&root, &["commit", "--allow-empty", "-qm", "XF1 fixes"]);
        git(&root, &["checkout", "-q", "main"]);
        poll_git(&mut live);
        git(&root, &["merge", "-q", "--ff-only", "claude/x"]);
        let second = poll_git(&mut live).pop().unwrap();
        assert!(crate::gitwatch::is_merge(&second));
        assert_ne!(first.id, second.id);
        assert_eq!(crate::work::all(&live.graph, &live.me())["XF1"].lane, "done");
        let mut cold = Room::open(&root).unwrap();
        cold.tick();
        assert_eq!(crate::work::all(&cold.graph, &cold.me())["XF1"].lane, "done");
        assert_eq!(cold.graph.items["claude/x"].merged_at, live.graph.items["claude/x"].merged_at);
        let _ = poll_git(&mut cold);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_recreated_ref_retires_its_old_generation_and_keeps_ledger_ids_unique() {
        let root = git_room("recreate");
        let base = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["checkout", "-qb", "claude/x"]);
        git(&root, &["commit", "--allow-empty", "-qm", "XF1 implemented"]);
        git(&root, &["checkout", "-q", "main"]);
        git(&root, &["merge", "-q", "--ff-only", "claude/x"]);
        let old_sha = git(&root, &["rev-parse", "claude/x"]);
        let mut first = Room::open(&root).unwrap();
        first.tick();
        poll_git(&mut first);
        append(&first.ledger, &Event { id: "branch-pass".into(), at: now(), from: "codex".into(),
            body: Body::Verdict { item: "claude/x".into(), outcome: "pass".into() },
        }).unwrap();
        first.tick();
        drop(first);

        git(&root, &["branch", "-D", "claude/x"]);
        git(&root, &["checkout", "-qb", "claude/x", &base]);
        git(&root, &["commit", "--allow-empty", "-qm", "XF2 implemented"]);
        git(&root, &["checkout", "-q", "main"]);
        let mut second = Room::open(&root).unwrap();
        second.tick();
        let rows = poll_git(&mut second);
        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[0].body, Body::Commit { gone: true, .. }));
        assert!(crate::gitwatch::is_tip(&rows[1]));
        let work = crate::work::all(&second.graph, &second.me());
        assert_eq!((work["XF1"].lane, work["XF2"].lane), ("done", "progress"));
        assert_eq!(work["XF1"].state, "pass");
        assert!(second.graph.items["claude/x"].verdicts.is_empty());
        let targets: Vec<_> = second.graph.edges.iter().filter(|e| e.kind == "implements").map(|e| e.to.as_str()).collect();
        assert_eq!(targets, ["XF2"]);
        second.apply_git_poll(crate::gitwatch::Poll { durable: rows, ..Default::default() });
        second.tick();
        let count = ledger_rows(&second).len();
        assert_eq!(count, 4, "a repeated poll must not append known rows");
        drop(second);

        let mut cold = Room::open(&root).unwrap();
        cold.tick();
        assert!(poll_git(&mut cold).is_empty());
        assert_eq!(ledger_rows(&cold).len(), count, "an unchanged restart must not append rows");
        let work = crate::work::all(&cold.graph, &cold.me());
        assert_eq!((work["XF1"].lane, work["XF2"].lane), ("done", "progress"));
        git(&root, &["branch", "-D", "claude/x"]);
        poll_git(&mut cold);
        for _ in 0..2 {
            git(&root, &["branch", "claude/x", &old_sha]);
            poll_git(&mut cold);
            git(&root, &["branch", "-D", "claude/x"]);
            poll_git(&mut cold);
        }
        let git_rows = ledger_rows(&cold);
        let ids: std::collections::HashSet<_> = git_rows.iter().map(|e| &e.id).collect();
        assert_eq!(ids.len(), git_rows.len());
        assert_eq!(git_rows.iter().filter(|e| matches!(&e.body, Body::Commit { sha, gone: true, .. } if sha == &old_sha)).count(), 3);
        let _ = std::fs::remove_dir_all(root);
    }

    /// A room whose claude agent is a shell command printing `reply` and exiting with `code`.
    fn room_with_claude(name: &str, reply: &str, code: i32) -> (PathBuf, Room) {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("trio-room-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(root.join(".trio")).unwrap();
        #[cfg(windows)]
        let wake = ["cmd".to_string(), "/C".into(), format!("echo {reply}& exit {code}")];
        #[cfg(unix)]
        let wake = ["sh".to_string(), "-c".into(), format!("echo '{reply}'; exit {code}")];
        let q = |s: &str| toml::Value::String(s.into()).to_string();
        let cfg = format!(
            "hop_budget = 3\n[agents.claude]\nenabled = true\nsession = \"s\"\ncwd = {}\nwake = [{}]\n",
            q(&root.join("cw").display().to_string()),
            wake.iter().map(|a| q(a)).collect::<Vec<_>>().join(", ")
        );
        std::fs::write(root.join(".trio").join("room.toml"), cfg).unwrap();
        let room = Room::open(&root).unwrap();
        (root, room)
    }

    /// Tick until the claude run started by a mention has been reaped; returns the ledger text
    /// as it stood right after that tick (no further ticks).
    async fn wake_and_reap(root: &Path, room: &mut Room) -> String {
        room.tick();
        room.say(&room.me(), "@claude go", None).unwrap();
        room.tick();
        assert!(room.any_running(), "the mention should have started a run");
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            room.tick();
            if !room.any_running() {
                return std::fs::read_to_string(root.join(".trio").join("room.jsonl")).unwrap();
            }
        }
        panic!("run never finished");
    }

    #[test]
    fn the_human_handle_is_configured_cleaned_and_never_an_agent() {
        let cfg = |me: &str| RoomCfg { me: me.into(), ..Default::default() };
        assert_eq!(human(&cfg("Sanjith")), "sanjith");
        assert_eq!(human(&cfg(" Ana Lee ")), "analee");
        for bad in ["claude", "Codex", "both", "trio", "  ", "@!"] {
            assert!(!["claude", "codex", "both", "trio", ""].contains(&human(&cfg(bad)).as_str()), "{bad}");
        }
    }

    #[tokio::test]
    async fn a_reaped_reply_is_routed_in_the_same_tick() {
        let (root, mut room) = room_with_claude("route", "@codex ping", 0);
        let ledger = wake_and_reap(&root, &mut room).await;
        assert!(ledger.contains("\"from\":\"claude\",\"kind\":\"message\",\"summary\":\"@codex ping\""), "{ledger}");
        // codex is not configured, so routing records why it was not woken: proof it was routed.
        assert!(ledger.contains("codex not woken: Disabled"), "{ledger}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A room with both agents as shell commands: (name, printed reply, exit code).
    fn room_with(name: &str, agents: &[(&str, &str, i32)]) -> (PathBuf, Room) {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("trio-room-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(root.join(".trio")).unwrap();
        let q = |s: &str| toml::Value::String(s.into()).to_string();
        let mut cfg = "hop_budget = 3\n".to_string();
        for (agent, reply, code) in agents {
            #[cfg(windows)]
            let wake = ["cmd".to_string(), "/C".into(), format!("echo {reply}& exit {code}")];
            #[cfg(unix)]
            let wake = ["sh".to_string(), "-c".into(), format!("echo '{reply}'; exit {code}")];
            cfg.push_str(&format!(
                "[agents.{agent}]\nenabled = true\nsession = \"s\"\ncwd = {}\nwake = [{}]\n",
                q(&root.join(format!("cw-{agent}")).display().to_string()),
                wake.iter().map(|a| q(a)).collect::<Vec<_>>().join(", ")
            ));
        }
        std::fs::write(root.join(".trio").join("room.toml"), cfg).unwrap();
        let room = Room::open(&root).unwrap();
        (root, room)
    }

    async fn settle(room: &mut Room) {
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            room.tick();
            if !room.any_running() {
                return;
            }
        }
        panic!("runs never finished");
    }

    #[tokio::test]
    async fn a_usage_limit_hands_the_message_to_the_other_agent() {
        let (root, mut room) = room_with("limit", &[("claude", "You've hit your session limit - resets 3:45pm", 1), ("codex", "Taking it.", 0)]);
        room.tick();
        room.say(&room.me(), "@claude look at XF1", None).unwrap();
        room.tick();
        settle(&mut room).await;
        let ledger = std::fs::read_to_string(root.join(".trio").join("room.jsonl")).unwrap();
        assert!(ledger.contains("claude hit a usage limit: out until 15:45"), "{ledger}");
        assert!(ledger.contains("routed to Codex: Claude is out until 15:45"), "{ledger}");
        assert!(ledger.contains("\"agent\":\"codex\""), "codex was woken: {ledger}");
        assert!(ledger.contains("\"summary\":\"Taking it.\""), "{ledger}");
        assert!(room.usage("claude").out() && !room.usage("codex").out());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_review_of_the_other_agents_work_waits_for_the_reviewer() {
        let (root, mut room) = room_with("hold", &[("claude", "Reviewed.", 0), ("codex", "Not me.", 0)]);
        room.tick();
        // Codex wrote XF1 on its branch; Claude is out.
        let branch = Event { id: "git:codex/x".into(), at: now(), from: "codex".into(), body: Body::Commit { branch: "codex/x".into(), sha: "abc12345".into(), subject: "XF1 work".into(), touches: vec![], merged: false, gone: false } };
        append(&room.ledger, &branch).unwrap();
        room.stops.insert("claude".into(), crate::usage::Stop { until: crate::usage::epoch_now() + 3600, reason: "session limit".into() });
        room.say(&room.me(), "@claude please review XF1", None).unwrap();
        room.tick();
        let ledger = std::fs::read_to_string(root.join(".trio").join("room.jsonl")).unwrap();
        assert!(ledger.contains("held for Claude: Codex wrote it"), "{ledger}");
        assert!(!ledger.contains("\"agent\":\"codex\""), "never rerouted to the author: {ledger}");
        assert_eq!(room.held_for("claude"), 1);
        // Claude's limit resets: the held review goes to Claude.
        room.stops.clear();
        room.tick();
        settle(&mut room).await;
        let ledger = std::fs::read_to_string(root.join(".trio").join("room.jsonl")).unwrap();
        assert!(ledger.contains("Claude is back"), "{ledger}");
        assert!(ledger.contains("\"summary\":\"Reviewed.\""), "{ledger}");
        assert_eq!(room.held_for("claude"), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_daily_budget_puts_an_agent_out_until_midnight() {
        let (root, mut room) = room_with("budget", &[("claude", "x", 0)]);
        room.cfg.budget.claude_usd_day = Some(1.0);
        room.tick();
        append(&room.ledger, &Event { id: String::new(), at: now(), from: "claude".into(), body: Body::RunFinished { agent: "claude".into(), exit: 0, secs: 9, cost: Some(1.25) } }).unwrap();
        room.tick();
        let u = room.usage("claude");
        assert_eq!((u.state, u.spent_today), ("out", 1.25));
        assert!(u.why().contains("today's $1.00 budget is spent"), "{}", u.why());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_failed_run_never_posts_as_the_agent() {
        let (root, mut room) = room_with_claude("fail", "@codex half an answer", 3);
        let ledger = wake_and_reap(&root, &mut room).await;
        assert!(!ledger.contains("half an answer"), "{ledger}");
        assert!(ledger.contains("claude run failed (exit 3); output not posted"), "{ledger}");
        assert!(!ledger.contains("codex not woken"), "a failed run must not wake anyone: {ledger}");
        let _ = std::fs::remove_dir_all(root);
    }
}
