//! Project graph, derived deterministically from events (no model calls).
//!
//! `Node` inherits `Timed + Summarized`. Each `Deriver` owns one kind of fact and updates the
//! graph incrementally, so applying an event costs O(1) amortized (hash lookups) and the whole
//! graph keeps contributions by id, with per-item step lists capped at `STEPS_MAX`.

use crate::model::{clip, definition, Body, Entity, Event, Summarized, Timed, Touch};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

const STEPS_MAX: usize = 60;

pub trait Node: Timed + Summarized {
    fn lane(&self) -> &str;
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub at: String,
    pub who: String,
    pub what: String,
    /// Event id, so the UI can open the message behind a step.
    pub event: String,
    #[serde(skip)]
    seq: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkItem {
    pub id: String,
    pub first: String,
    pub last: String,
    pub owner: String,
    pub title: String,
    pub touches: u32,
    pub steps: Vec<Step>,
    /// Requests keyed by the message that made them.
    #[serde(skip)]
    review_asks: HashMap<String, ReviewAsk>,
    #[serde(skip)]
    latest_ask: Option<String>,
    /// Every verdict on this node (newest kept, at most VERDICTS_MAX), with who gave it and when.
    #[serde(skip)]
    pub verdicts: Vec<Verdict>,
    /// Who first named it; `owner` moves to whoever claims it.
    #[serde(skip)]
    pub raised_by: String,
    #[serde(skip)]
    first_seq: u64,
    #[serde(skip)]
    pub claimed_by: Option<String>,
    #[serde(skip)]
    claim_key: Option<Key>,
    /// Branches: when the branch was first seen merged, as an order key.
    #[serde(skip)]
    pub merged_at: Option<Key>,
    #[serde(skip)]
    merge_sha: String,
    #[serde(skip)]
    pub tip_sha: String,
    /// Items: implementing branches that merged and were later deleted, so Done survives cleanup.
    #[serde(skip)]
    pub merged_gone: Vec<Gone>,
    #[serde(skip)]
    pub branches: BranchFacts,
    #[serde(skip)]
    work_lane: Option<&'static str>,
    #[serde(skip)]
    pub moved_by: String,
    /// Where the title's definition came from ("XF32 \u{2014} Limit the clean claim"): 0 none,
    /// 1 a message body, 2 a message summary or post title. A definition replaces the title
    /// only from a stronger place, so later status lines ("XF1: fixed, ready") never rename it,
    /// while a heading can still correct a definition guessed from a body.
    #[serde(skip)]
    pub defined: u8,
    #[serde(skip)]
    title_key: Key,
    #[serde(skip)]
    title_from_summary: bool,
}

/// Order of an event: its minute, then its stable position (`Graph::order`). Comparing keys
/// instead of timestamps keeps same-minute events in source order, live and on replay alike.
pub type Key = (String, u64);

/// Keep branch lookups out of the message path. Ref and branch review events update this.
#[derive(Debug, Clone, Default)]
pub struct BranchFacts {
    pub count: usize,
    pub unmerged: usize,
    pub verdict: Option<Verdict>,
    pub merged: Option<Key>,
}

#[derive(Debug, Clone)]
pub struct ReviewAsk {
    pub at: String,
    /// Stable position of the asking event, which breaks ties within a minute.
    pub seq: u64,
    pub by: String,
    pub of: Vec<String>,
}

impl ReviewAsk {
    pub fn key(&self) -> (&str, u64) {
        (&self.at, self.seq)
    }
}

#[derive(Debug, Clone)]
pub struct Verdict {
    pub at: String,
    pub seq: u64,
    pub by: String,
    /// pass | changes | blocked | review
    pub outcome: String,
    /// The node the verdict named (an item or a branch).
    pub on: String,
    /// The line it came from. One `verdict` line naming a branch and its items yields one event
    /// per node; they share this, so it counts as one review.
    pub line: String,
}

impl Verdict {
    pub fn key(&self) -> (&str, u64) {
        (self.at.as_str(), self.seq)
    }
}

#[derive(Debug, Clone)]
pub struct Gone {
    pub branch: String,
    pub merged: Key,
    pub last: String,
}

const VERDICTS_MAX: usize = 32;
/// Merged-and-deleted branches remembered per item.
const GONE_MAX: usize = 16;

/// Replace a verdict from the same line and keep the newest VERDICTS_MAX by key.
fn keep_verdict(list: &mut Vec<Verdict>, v: Verdict) {
    if let Some(old) = list.iter_mut().find(|x| x.line == v.line && x.on == v.on) {
        *old = v;
        return;
    }
    list.push(v);
    if list.len() > VERDICTS_MAX {
        let oldest = (0..list.len()).min_by(|&a, &b| list[a].key().cmp(&list[b].key())).expect("non-empty");
        list.remove(oldest);
    }
}

impl Entity for WorkItem {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &'static str {
        if self.id.contains('/') { "branch" } else { "item" }
    }
}
impl Timed for WorkItem {
    fn at(&self) -> &str {
        &self.first
    }
}
impl Summarized for WorkItem {
    fn raw_summary(&self) -> Option<&str> {
        Some(&self.title)
    }
}
impl Node for WorkItem {
    fn lane(&self) -> &str {
        &self.owner
    }
}

impl WorkItem {
    pub fn base_state(&self) -> &'static str {
        if self.kind() == "branch" && !self.tip_sha.is_empty() {
            "branch"
        } else if self.claimed_by.is_some() {
            "claimed"
        } else {
            "open"
        }
    }

    pub fn first_key(&self) -> (&str, u64) {
        (&self.first, self.first_seq)
    }

    pub fn claim_key(&self) -> Option<(&str, u64)> {
        self.claim_key.as_ref().map(|(at, seq)| (at.as_str(), *seq))
    }

    pub fn review_request(&self, id: &str) -> bool {
        self.review_asks.contains_key(id)
    }

    pub fn latest_request(&self) -> Option<&ReviewAsk> {
        self.latest_ask.as_ref().and_then(|id| self.review_asks.get(id))
    }

    fn add_ask(&mut self, id: &str, ask: ReviewAsk) {
        if self.latest_request().is_none_or(|old| old.key() <= ask.key()) {
            self.latest_ask = Some(id.into());
        } else if self.latest_ask.as_deref() == Some(id) {
            self.review_asks.insert(id.into(), ask);
            self.select_ask();
            return;
        }
        self.review_asks.insert(id.into(), ask);
    }

    fn remove_ask(&mut self, id: &str) {
        self.review_asks.remove(id);
        if self.latest_ask.as_deref() == Some(id) {
            self.select_ask();
        }
    }

    fn select_ask(&mut self) {
        self.latest_ask = self.review_asks.iter().max_by_key(|(_, ask)| ask.key()).map(|(id, _)| id.clone());
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: String,
}

#[derive(Debug, Default, Serialize)]
pub struct Graph {
    pub items: HashMap<String, WorkItem>,
    pub edges: Vec<Edge>,
    #[serde(skip)]
    edge_set: HashSet<Edge>,
    #[serde(skip)]
    ask_targets: HashMap<String, Vec<String>>,
    #[serde(skip)]
    review_events: HashMap<String, ReviewFact>,
    #[serde(skip)]
    review_edges: HashMap<Edge, usize>,
    #[serde(skip)]
    retired: HashMap<String, u64>,
    #[serde(skip)]
    step_events: HashSet<(String, String)>,
    #[serde(skip)]
    implementers: HashMap<String, HashSet<String>>,
    #[serde(skip)]
    implemented: HashMap<String, Vec<String>>,
    #[serde(skip)]
    dirty: Vec<String>,
    #[serde(skip)]
    pub git_serials: HashMap<String, u64>,
    /// Latest status line per participant.
    pub now: HashMap<String, Status>,
    #[serde(skip)]
    status_lines: HashMap<String, StatusLines>,
    #[serde(skip)]
    first_run: HashMap<String, String>,
    /// What each unmerged branch changes, replaced whenever its tip moves.
    pub touches: BTreeMap<String, Vec<Touch>>,
    /// Branches whose tip is already in the base branch.
    pub merged: BTreeSet<String>,
    /// Stable position of every event applied, by id. A re-emitted post keeps the position it
    /// was first given, so growing a post never reorders it against later events.
    #[serde(skip)]
    order: HashMap<String, u64>,
    /// Counter for events applied without a position (tests and one-off graphs).
    #[serde(skip)]
    next: u64,
    /// Position of the event being applied.
    #[serde(skip)]
    pub seq: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub text: String,
    pub at: String,
    pub running: bool,
}

#[derive(Debug, Default)]
struct StatusLines {
    lines: HashMap<String, (Key, String)>,
    latest: Option<String>,
}

impl StatusLines {
    fn replace(&mut self, id: &str, key: Key, text: &str) {
        let head = self.latest.as_ref().and_then(|id| self.lines.get(id));
        let newest = head.is_none_or(|(old, _)| old <= &key);
        if text.is_empty() && id.ends_with("#status") {
            self.lines.remove(id);
        } else {
            self.lines.insert(id.into(), (key, clip(text, 120)));
            if newest {
                self.latest = Some(id.into());
                return;
            }
        }
        if self.latest.as_deref() == Some(id) {
            self.latest = self.lines.iter().max_by_key(|(_, (key, _))| key).map(|(id, _)| id.clone());
        }
    }
}

#[derive(Debug, Clone)]
struct ReviewFact {
    edge: Edge,
    line: String,
    generation: u64,
}

impl Graph {
    fn item(&mut self, id: &str, at: &str, who: &str) -> &mut WorkItem {
        self.dirty.push(id.to_string());
        let it = self.items.entry(id.to_string()).or_insert_with(|| WorkItem {
            id: id.to_string(),
            first: at.to_string(),
            last: at.to_string(),
            owner: who.to_string(),
            title: String::new(),
            touches: 0,
            steps: Vec::new(),
            review_asks: HashMap::new(),
            latest_ask: None,
            verdicts: Vec::new(),
            raised_by: who.to_string(),
            first_seq: self.seq,
            claimed_by: None,
            claim_key: None,
            merged_at: None,
            merge_sha: String::new(),
            tip_sha: String::new(),
            merged_gone: Vec::new(),
            branches: BranchFacts::default(),
            work_lane: None,
            moved_by: String::new(),
            defined: 0,
            title_key: (String::new(), 0),
            title_from_summary: false,
        });
        if (at, self.seq) < it.first_key() {
            it.first = at.to_string();
            it.first_seq = self.seq;
            it.raised_by = who.to_string();
            if it.claimed_by.is_none() {
                it.owner = who.to_string();
            }
        }
        it
    }

    /// Forget an item and its edges (a deleted branch). A merged branch hands its merge and its
    /// verdicts to the items it implemented first, so their Done and story survive the cleanup.
    fn drop_item(&mut self, id: &str) {
        *self.retired.entry(id.into()).or_default() += 1;
        let targets = self.implemented.remove(id).unwrap_or_default();
        if let Some(b) = self.items.remove(id) {
            for target in &targets {
                if let Some(it) = self.items.get_mut(target) {
                    if let Some(merged) = &b.merged_at {
                        it.merged_gone.push(Gone { branch: id.to_string(), merged: merged.clone(), last: b.last.clone() });
                    }
                    if it.merged_gone.len() > GONE_MAX {
                        let oldest = (0..it.merged_gone.len()).min_by(|&a, &b| it.merged_gone[a].merged.cmp(&it.merged_gone[b].merged)).expect("non-empty");
                        it.merged_gone.remove(oldest);
                    }
                    for v in &b.verdicts {
                        keep_verdict(&mut it.verdicts, v.clone());
                    }
                }
            }
        }
        self.touches.remove(id);
        self.merged.remove(id);
        self.edges.retain(|e| e.from != id && e.to != id);
        self.edge_set.retain(|e| e.from != id && e.to != id);
        self.review_edges.retain(|e, _| e.to != id);
        for target in targets {
            if let Some(branches) = self.implementers.get_mut(&target) {
                branches.remove(id);
            }
            self.refresh_branches(&target);
        }
    }

    fn edge(&mut self, from: &str, to: &str, kind: &str) -> bool {
        let e = Edge { from: from.into(), to: to.into(), kind: kind.into() };
        if self.edge_set.insert(e.clone()) {
            self.edges.push(e);
            return true;
        }
        false
    }

    fn link(&mut self, branch: &str, item: &str) -> bool {
        if self.edge(branch, item, "implements") {
            self.implementers.entry(item.into()).or_default().insert(branch.into());
            self.implemented.entry(branch.into()).or_default().push(item.into());
            return true;
        }
        false
    }

    fn branch_changed(&mut self, branch: &str) {
        if let Some(targets) = self.implemented.get(branch).cloned() {
            for id in targets {
                self.refresh_branches(&id);
            }
        }
    }

    fn refresh_branches(&mut self, id: &str) {
        let mut facts = BranchFacts::default();
        for branch in self.implementers.get(id).into_iter().flatten().filter_map(|b| self.items.get(b)) {
            facts.count += 1;
            facts.unmerged += usize::from(branch.merged_at.is_none());
            if branch.merged_at > facts.merged {
                facts.merged.clone_from(&branch.merged_at);
            }
            let newest = branch.verdicts.iter().max_by_key(|v| v.key());
            if newest.map(Verdict::key) > facts.verdict.as_ref().map(Verdict::key) {
                facts.verdict = newest.cloned();
            }
        }
        self.items.get_mut(id).expect("linked item").branches = facts;
        self.dirty.push(id.into());
    }

    fn review(&mut self, e: &Event, item: &str, state: &str) -> bool {
        let generation = self.retired.get(item).copied().unwrap_or_default();
        let edge = Edge { from: e.from.clone(), to: item.into(), kind: format!("review:{state}") };
        if let Some(old) = self.review_events.get(&e.id).cloned() {
            if old.edge.to == item && old.generation != generation {
                return false; // This line belonged to the retired ref.
            }
            if old.edge == edge {
                return true;
            }
            self.drop_review(&old);
        }
        self.edge(&edge.from, &edge.to, &edge.kind);
        *self.review_edges.entry(edge.clone()).or_default() += 1;
        self.review_events.insert(e.id.clone(), ReviewFact { edge, line: e.verdict_line(item).into(), generation });
        true
    }

    fn drop_review(&mut self, fact: &ReviewFact) {
        let edge = &fact.edge;
        if self.retired.get(&edge.to).copied().unwrap_or_default() != fact.generation {
            return;
        }
        if let Some(count) = self.review_edges.get_mut(edge) {
            *count -= 1;
            if *count == 0 {
                self.review_edges.remove(edge);
                self.edge_set.remove(edge);
                self.edges.retain(|e| e != edge);
            }
        }
        if let Some(it) = self.items.get_mut(&edge.to) {
            it.verdicts.retain(|v| v.line != fact.line || v.on != edge.to);
            self.dirty.push(edge.to.clone());
            self.branch_changed(&edge.to);
        }
    }

    fn step(&mut self, item: &str, e: &Event, what: String) {
        let first = self.step_events.insert((e.id.clone(), item.to_string()));
        let it = self.items.get_mut(item).expect("item exists");
        if e.at.as_str() > it.last.as_str() {
            it.last = e.at.clone();
        }
        it.touches += u32::from(first);
        if let Some(s) = it.steps.iter_mut().find(|s| s.event == e.id) {
            s.what = what; // a re-emitted post updates its step in place
            return;
        }
        // Insert in time order: O(log n) search instead of re-sorting the list on every step.
        if !first {
            return;
        }
        let pos = it.steps.partition_point(|s| (&s.at, s.seq) <= (&e.at, self.seq));
        it.steps.insert(pos, Step { at: e.at.clone(), who: e.from.clone(), what, event: e.id.clone(), seq: self.seq });
        if it.steps.len() > STEPS_MAX {
            let cut = it.steps.len() - STEPS_MAX;
            it.steps.drain(..cut);
        }
    }

    /// Apply one event through every deriver, positioned after everything applied before it.
    pub fn apply(&mut self, e: &Event, derivers: &[Box<dyn Deriver>]) {
        self.apply_at(e, None, derivers);
    }

    /// Apply one event at stable position `pos` (the room's source rank and order within the
    /// source). The first position an id gets is kept for good.
    pub fn apply_at(&mut self, e: &Event, pos: Option<u64>, derivers: &[Box<dyn Deriver>]) {
        let first_time = !self.order.contains_key(&e.id);
        if first_time {
            self.next += 1;
            self.order.insert(e.id.clone(), pos.unwrap_or(self.next));
        }
        self.seq = self.order[&e.id];
        if !matches!(e.body, Body::Verdict { .. }) && let Some(old) = self.review_events.remove(&e.id) {
            self.drop_review(&old);
        }
        if !matches!(e.body, Body::Message { .. }) && let Some(old) = self.ask_targets.remove(&e.id) {
            for id in old {
                if let Some(it) = self.items.get_mut(&id) {
                    it.remove_ask(&e.id);
                    self.dirty.push(id);
                }
            }
        }
        for d in derivers {
            d.apply(e, first_time, self);
        }
        for id in self.dirty.drain(..) {
            if let Some(it) = self.items.get_mut(&id) && it.kind() == "item" {
                let lane = crate::work::reading(it).lane;
                if it.work_lane != Some(lane) {
                    it.work_lane = Some(lane);
                    it.moved_by.clone_from(&e.from);
                }
            }
        }
    }

    pub fn position(&self, id: &str) -> Option<u64> {
        self.order.get(id).copied()
    }

    pub fn has_event(&self, id: &str) -> bool {
        self.position(id).is_some()
    }
}

pub trait Deriver: Send + Sync {
    fn apply(&self, e: &Event, first_time: bool, g: &mut Graph);
}

/// Phrases that ask for a review even without an @mention.
const REVIEW_ASKS: &[&str] = &["please review", "please re-review", "ready for review", "up for review", "for your review", "fixed: re-review", "design for review"];

/// `p` occurs in `s` as whole words: "review" matches "re-review" but not "preview" or "reviewers".
fn has_phrase(s: &str, p: &str) -> bool {
    s.match_indices(p).any(|(i, _)| {
        let before = s[..i].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        let after = s[i + p.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric());
        before && after
    })
}

/// A message that reports a verdict (a line starting `verdict ` outside code fences, the same
/// lines the posts parser reads) answers a review, never asks one.
fn reports_verdict(text: &str) -> bool {
    let mut fence = None;
    text.lines().any(|l| {
        let was = fence.is_some();
        fence = crate::source::fence_step(l, fence);
        !was && fence.is_none() && l.trim_start().trim_start_matches(['-', '*', ' ']).to_ascii_lowercase().starts_with("verdict ")
    })
}

/// Messages create items for the ids they mention and record a step on each.
pub struct ItemDeriver;
impl Deriver for ItemDeriver {
    fn apply(&self, e: &Event, _first: bool, g: &mut Graph) {
        let Body::Message { items, mentions, detail, .. } = &e.body else { return };
        let summary = e.summary();
        // "@codex please review P0a" asks for a review of every item it names.
        let mut of: Vec<String> = mentions.iter().filter(|m| **m != e.from).cloned().collect();
        // A message that reports a verdict is an answer, not a request. Posts files often ask
        // without an @mention ("A1-A4 ready for review"): that request goes to the other agent.
        let lower = summary.to_ascii_lowercase();
        let phrased = REVIEW_ASKS.iter().any(|p| has_phrase(&lower, p));
        let answer = reports_verdict(&summary) || detail.as_deref().is_some_and(reports_verdict);
        let asks_review = !answer && ((!of.is_empty() && has_phrase(&lower, "review")) || phrased);
        if asks_review && of.is_empty() {
            of.push("both".into());
        }
        let seq = g.seq;
        if let Some(old) = g.ask_targets.remove(&e.id) {
            let kept: HashSet<_> = if asks_review { items.iter().collect() } else { HashSet::new() };
            for id in old {
                if !kept.contains(&id) && let Some(it) = g.items.get_mut(&id) {
                    it.remove_ask(&e.id);
                    g.dirty.push(id);
                }
            }
        }
        if asks_review {
            g.ask_targets.insert(e.id.clone(), items.clone());
        }
        for id in items {
            let it = g.item(id, &e.at, &e.from);
            // A definition wins over plain mentions; until one arrives, the latest message naming it.
            let def = definition(&summary, id).map(|d| (2, d)).or_else(|| detail.as_deref().and_then(|d| definition(d, id)).map(|d| (1, d)));
            let key = (&e.at, seq);
            let old = (&it.title_key.0, it.title_key.1);
            let in_summary = summary.contains(id.as_str());
            let better_guess = match (in_summary, it.title_from_summary) {
                (true, false) => true,
                (false, true) => false,
                (true, true) => key >= old,
                (false, false) => key <= old,
            };
            match def {
                Some((rank, d)) if rank > it.defined || (rank == it.defined && key <= old) => {
                    it.title = d;
                    it.defined = rank;
                    it.title_key = (e.at.clone(), seq);
                }
                _ if it.defined == 0 && (it.title.is_empty() || better_guess) => {
                    it.title = clip(&summary, 90);
                    it.title_key = (e.at.clone(), seq);
                    it.title_from_summary = in_summary;
                }
                _ => {}
            }
            if asks_review {
                it.add_ask(&e.id, ReviewAsk { at: e.at.clone(), seq, by: e.from.clone(), of: of.clone() });
            }
            g.step(id, e, summary.clone());
            g.edge(&e.from, id, "touched");
        }
    }
}

/// `claim X` sets the owner lane.
pub struct ClaimDeriver;
impl Deriver for ClaimDeriver {
    fn apply(&self, e: &Event, _first: bool, g: &mut Graph) {
        let Body::Claim { item } = &e.body else { return };
        let seq = g.seq;
        let it = g.item(item, &e.at, &e.from);
        if it.claim_key().is_none_or(|key| key <= (e.at.as_str(), seq)) {
            it.owner = e.from.clone();
            it.claimed_by = Some(e.from.clone());
            it.claim_key = Some((e.at.clone(), seq));
        }
        g.step(item, e, format!("{} claimed it", e.from));
        g.edge(&e.from, item, "claimed");
    }
}

/// `verdict X: pass|changes` moves an item's state and adds a review edge.
pub struct ReviewDeriver;
impl Deriver for ReviewDeriver {
    fn apply(&self, e: &Event, _first: bool, g: &mut Graph) {
        let Body::Verdict { item, outcome } = &e.body else { return };
        let state = match outcome.as_str() {
            "pass" | "passed" | "approve" => "pass",
            "changes" | "revise" => "changes",
            "fail" | "blocked" => "blocked",
            _ => "review",
        };
        if !g.review(e, item, state) {
            return;
        }
        let seq = g.seq;
        let line = e.verdict_line(item).to_string();
        let it = g.item(item, &e.at, &e.from);
        keep_verdict(&mut it.verdicts, Verdict { at: e.at.clone(), seq, by: e.from.clone(), outcome: state.into(), on: item.clone(), line });
        g.step(item, e, format!("{} review: {state}", e.from));
        g.branch_changed(item);
    }
}

/// Run events draw who-woke-whom edges and mark an agent as running.
pub struct WakeDeriver;
impl Deriver for WakeDeriver {
    fn apply(&self, e: &Event, _first: bool, g: &mut Graph) {
        match &e.body {
            Body::RunStarted { agent, .. } => {
                g.edge(&e.from, agent, "woke");
                g.first_run.entry(agent.clone()).or_insert_with(|| e.at.clone());
                let s = g.now.entry(agent.clone()).or_insert(Status { text: String::new(), at: e.at.clone(), running: true });
                s.running = true;
            }
            Body::RunFinished { agent, .. } => {
                if let Some(s) = g.now.get_mut(agent) {
                    s.running = false;
                }
            }
            _ => {}
        }
    }
}

/// Terminal `Status:` lines become each participant's "now".
pub struct StatusDeriver;
impl Deriver for StatusDeriver {
    fn apply(&self, e: &Event, _first: bool, g: &mut Graph) {
        let Body::Status { text } = &e.body else { return };
        let lines = g.status_lines.entry(e.from.clone()).or_default();
        lines.replace(&e.id, (e.at.clone(), g.seq), text);
        if let Some(((at, _), text)) = lines.latest.as_ref().and_then(|id| lines.lines.get(id)) {
            let s = g.now.entry(e.from.clone()).or_insert(Status { text: String::new(), at: String::new(), running: false });
            s.text.clone_from(text);
            s.at.clone_from(at);
        } else if let Some(at) = g.first_run.get(&e.from) {
            let s = g.now.get_mut(&e.from).expect("a run creates a status");
            s.text.clear();
            s.at.clone_from(at);
        } else {
            g.now.remove(&e.from);
        }
    }
}

/// Branch tips: a branch node per `claude/*` or `codex/*` branch, linked to the items its
/// latest subject mentions.
pub struct BranchDeriver;
impl Deriver for BranchDeriver {
    fn apply(&self, e: &Event, first: bool, g: &mut Graph) {
        let Body::Commit { branch, sha, subject, touches, merged, gone } = &e.body else { return };
        if !first && crate::gitwatch::is_transition(e) {
            return;
        }
        if let Some(serial) = crate::gitwatch::serial(e) {
            g.git_serials.entry(branch.clone()).and_modify(|n| *n = (*n).max(serial)).or_insert(serial);
        }
        if crate::gitwatch::is_refresh(e) {
            if g.items.get(branch).is_some_and(|b| b.tip_sha == *sha && b.merged_at.is_none()) {
                g.touches.insert(branch.clone(), touches.clone());
            }
            return;
        }
        if *gone {
            g.drop_item(branch);
            return;
        }
        if *merged {
            g.merged.insert(branch.clone());
            g.touches.remove(branch);
        } else {
            g.merged.remove(branch);
            g.touches.insert(branch.clone(), touches.clone());
        }
        let owner = Some(crate::model::branch_prefix(branch)).filter(|p| ["claude", "codex"].contains(p)).map_or_else(|| e.from.clone(), str::to_string);
        let seq = g.seq;
        let it = g.item(branch, &e.at, &owner);
        it.tip_sha.clone_from(sha);
        it.title = clip(subject, 90);
        let mut changed = false;
        if !*merged {
            changed = it.merged_at.take().is_some();
        } else if it.merged_at.is_none() || it.merge_sha != *sha {
            it.merged_at = Some((e.at.clone(), seq));
            it.merge_sha.clone_from(sha);
            changed = true;
        }
        g.step(branch, e, format!("{} {}", &sha[..sha.len().min(8)], clip(subject, 80)));
        for id in crate::model::item_ids(subject) {
            g.item(&id, &e.at, &owner);
            changed |= g.link(branch, &id);
        }
        if changed {
            g.branch_changed(branch);
        }
    }
}

pub fn derivers() -> Vec<Box<dyn Deriver>> {
    vec![Box::new(ItemDeriver), Box::new(ClaimDeriver), Box::new(ReviewDeriver), Box::new(WakeDeriver), Box::new(StatusDeriver), Box::new(BranchDeriver)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str, at: &str, from: &str, body: Body) -> Event {
        Event { id: id.into(), at: at.into(), from: from.into(), body }
    }

    #[test]
    fn review_flow_moves_state_and_dedups() {
        let d = derivers();
        let mut g = Graph::default();
        let msg = ev("m1", "2026-11-23T10:00", "claude", Body::Message { summary: "P0a ready for review".into(), detail: None, mentions: vec![], items: vec!["P0a".into()] });
        g.apply(&msg, &d);
        g.apply(&msg, &d); // re-emitted post: same id, no duplicate step
        g.apply(&ev("c1", "2026-11-23T10:01", "claude", Body::Claim { item: "P0a".into() }), &d);
        g.apply(&ev("v1", "2026-11-23T11:00", "codex", Body::Verdict { item: "P0a".into(), outcome: "changes".into() }), &d);
        let it = &g.items["P0a"];
        assert_eq!(crate::work::all(&g, "sanjith")["P0a"].state, "changes");
        assert_eq!(it.owner, "claude");
        assert_eq!(it.steps.len(), 3);
        assert!(g.edges.iter().any(|e| e.from == "codex" && e.kind == "review:changes"));
        g.apply(&ev("s1", "2026-11-23T11:01", "codex", Body::Status { text: "done".into() }), &d);
        assert_eq!(g.now["codex"].text, "done");
    }

    #[test]
    fn branch_verdict_colours_branch_and_items() {
        let d = derivers();
        let mut g = Graph::default();
        g.apply(&ev("v1", "2026-11-23T10:00", "codex", Body::Verdict { item: "claude/hooks-p0a".into(), outcome: "pass".into() }), &d);
        g.apply(&ev("git:claude/hooks-p0a", "2026-11-23T09:00", "claude", Body::Commit { branch: "claude/hooks-p0a".into(), sha: "553f50d1".into(), subject: "P0a fixes".into(), touches: vec![], merged: false, gone: false }), &d);
        assert_eq!(crate::work::latest_verdict(&g.items["claude/hooks-p0a"]).unwrap().outcome, "pass");
        assert_eq!(crate::work::all(&g, "sanjith")["P0a"].state, "pass");
    }

    #[test]
    fn a_deleted_branch_leaves_the_map() {
        let d = derivers();
        let mut g = Graph::default();
        let touch = Touch { part: "src/hook".into(), lines: 9, files: 1 };
        let tip = |gone: bool| Body::Commit { branch: "claude/x".into(), sha: if gone { "".into() } else { "abc".into() }, subject: "XF1 work".into(), touches: vec![touch.clone()], merged: false, gone };
        g.apply(&ev("git:claude/x", "2026-11-23T09:00", "claude", tip(false)), &d);
        assert!(g.items.contains_key("claude/x") && g.touches.contains_key("claude/x"));
        assert!(g.edges.iter().any(|e| e.from == "claude/x"));
        g.apply(&ev("git:claude/x", "2026-11-23T09:30", "claude", tip(true)), &d);
        assert!(!g.items.contains_key("claude/x") && !g.touches.contains_key("claude/x"));
        assert!(!g.edges.iter().any(|e| e.from == "claude/x" || e.to == "claude/x"));
        assert!(g.items.contains_key("XF1"), "the work item itself stays");
    }

}
