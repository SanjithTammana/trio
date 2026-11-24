//! A room: its config, its sources, a bounded event window, the derived graph, and the agents.
//!
//! Cost model:
//! - tick: one `stat` per source, O(sources); bytes read only when a file grew, O(delta).
//! - apply: O(1) amortized per event (hash index into the window; the window is trimmed in
//!   batches, so the O(n) re-index runs once per `WINDOW / 4` events).
//! - memory: at most `WINDOW * 1.25` events plus the graph, O(V + E) with capped step lists.

use crate::agent::{wake_prompt, Agent, AgentConfig, CliAgent, Guard, WakeDecision};
use crate::graph::{derivers, Deriver, Graph};
use crate::model::{clip, split_reply, Body, Event, Summarized, SUMMARY_MAX};
use crate::source::{append, mentions_in, PostsFile, RoomLedger, Source};
use crate::work::cap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

pub struct Room {
    pub root: PathBuf,
    pub ledger: PathBuf,
    pub cfg: RoomCfg,
    sources: Vec<Box<dyn Source + Send>>,
    events: Vec<Event>,
    index: HashMap<String, usize>,
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
    /// The first git listing was compared with the replayed ledger.
    git_listed: bool,
    /// Per source (then git): how many events it has yielded, for stable positions.
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
        let seen = crate::seen::load(&root);
        Ok(Self {
            root,
            ledger,
            cfg,
            sources,
            events: Vec::new(),
            index: HashMap::new(),
            graph: Graph::default(),
            derivers: derivers(),
            rev: 0,
            presence: String::new(),
            agents,
            started: HashMap::new(),
            hop: 0,
            live: false,
            git,
            git_listed: false,
            counts: Vec::new(),
            trio_exe,
            seen,
            readings: HashMap::new(),
            read_at: None,
            stops: HashMap::new(),
            held: Vec::new(),
            triggers: HashMap::new(),
        })
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// Move the seen mark to now, with every item's current lane, and publish the change.
    pub fn mark_seen(&mut self) {
        let s = crate::seen::mark(&now(), &crate::work::all(&self.graph, &self.me()));
        if let Err(e) = crate::seen::save(&self.root, &s) {
            eprintln!("trio: could not save the seen mark: {e}");
        }
        self.seen = Some(s);
        self.rev += 1;
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

    /// Stable position for the next event from source `i` (git is `sources.len()`): sources in
    /// config order, then each source's own order. Same-minute events from different sources
    /// therefore order the same way live and on replay; within a file, file order decides.
    fn position(&mut self, i: usize) -> u64 {
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
        let polls = self.git.as_mut().map(|g| g.take()).unwrap_or_default();
        for p in polls {
            for e in p.tips {
                let pos = self.position(n);
                fresh.push((e, false, pos));
            }
            let mut durable = p.durable;
            if !self.git_listed {
                // Branches the ledger says merged but git no longer lists were deleted while the
                // room was closed: retire them, so their items keep Done through the archive.
                self.git_listed = true;
                for b in self.graph.merged.iter().filter(|b| !p.listed.contains(b)) {
                    durable.push(crate::gitwatch::gone_event(b, ""));
                }
            }
            if !durable.is_empty() {
                for e in &durable {
                    if let Err(err) = append(&self.ledger, e) {
                        eprintln!("trio: could not record {}: {err}", e.id);
                    }
                }
                self.poll_source(n - 1, &mut fresh);
            }
        }
        let changed = !fresh.is_empty();
        fresh.sort_by(|a, b| a.0.at.cmp(&b.0.at).then(a.2.cmp(&b.2)));
        for (e, from_ledger, pos) in fresh {
            let route = self.live && from_ledger && matches!(e.body, Body::Message { .. }) && !self.index.contains_key(&e.id);
            self.apply(e.clone(), pos);
            if route {
                self.route(&e);
            }
        }
        changed
    }

    fn poll_source(&mut self, i: usize, fresh: &mut Vec<(Event, bool, u64)>) {
        let from_ledger = i == self.sources.len() - 1;
        if let Ok(evs) = self.sources[i].poll() {
            for e in evs {
                let pos = self.position(i);
                fresh.push((e, from_ledger, pos));
            }
        }
    }

    fn apply(&mut self, e: Event, pos: u64) {
        self.graph.apply_at(&e, Some(pos), &self.derivers);
        if let Some(&i) = self.index.get(&e.id) {
            self.events[i] = e;
            return;
        }
        self.index.insert(e.id.clone(), self.events.len());
        self.events.push(e);
        if self.events.len() > WINDOW + WINDOW / 4 {
            let cut = self.events.len() - WINDOW;
            self.events.drain(..cut);
            self.index = self.events.iter().enumerate().map(|(i, e)| (e.id.clone(), i)).collect();
        }
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
            .events
            .iter()
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
        since.extend(crate::seen::digest(&self.events, t));
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
        let mut v: Vec<String> = self.events.iter().rev().filter(|x| matches!(x.body, Body::Message { .. })).take(8).map(|x| format!("{}: {}", x.from, x.summary())).collect();
        v.reverse();
        v
    }

    /// True when `e` asks for a review of an item that `author` wrote.
    fn reviews_own_work(&self, e: &Event, author: &str) -> bool {
        let Body::Message { items, .. } = &e.body else { return false };
        items.iter().any(|id| {
            self.graph.items.get(id).is_some_and(|it| it.review_asked.as_ref().is_some_and(|r| r.at == e.at && r.by == e.from))
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
        self.events
            .iter()
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
        self.index.get(id).and_then(|&i| self.events[i].detail())
    }

    /// The chat view: the newest `limit` messages and run events, oldest first.
    pub fn messages(&self, limit: usize) -> Vec<MessageView<'_>> {
        let mut out: Vec<&Event> = self.events.iter().filter(|e| !matches!(e.body, Body::Claim { .. } | Body::Verdict { .. } | Body::Commit { .. })).collect();
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
