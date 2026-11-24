//! Project graph, derived deterministically from events (no model calls).
//!
//! `Node` inherits `Timed + Summarized`. Each `Deriver` owns one kind of fact and updates the
//! graph incrementally, so applying an event costs O(1) amortized (hash lookups) and the whole
//! graph is O(V + E) memory with per-item step lists capped at `STEPS_MAX`.

use crate::model::{clip, Body, Entity, Event, Summarized, Timed, Touch};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

const STEPS_MAX: usize = 60;

pub trait Node: Timed + Summarized {
    fn lane(&self) -> &str;
    fn state(&self) -> &str;
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub at: String,
    pub who: String,
    pub what: String,
    /// Event id, so the UI can open the message behind a step.
    pub event: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkItem {
    pub id: String,
    pub first: String,
    pub last: String,
    pub owner: String,
    pub state: String,
    pub title: String,
    pub touches: u32,
    pub steps: Vec<Step>,
    /// The latest message that asked for a review of this item: when, by whom, and of whom.
    #[serde(skip)]
    pub review_asked: Option<ReviewAsk>,
    /// Time of the latest verdict, so a later review request reads as "in review" again.
    #[serde(skip)]
    pub verdict_at: String,
}

#[derive(Debug, Clone)]
pub struct ReviewAsk {
    pub at: String,
    pub by: String,
    pub of: Vec<String>,
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
    fn state(&self) -> &str {
        &self.state
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
    /// Latest status line per participant.
    pub now: HashMap<String, Status>,
    /// What each unmerged branch changes, replaced whenever its tip moves.
    pub touches: BTreeMap<String, Vec<Touch>>,
    /// Branches whose tip is already in the base branch.
    pub merged: BTreeSet<String>,
    /// Events already applied, so a re-emitted post does not double-count steps.
    #[serde(skip)]
    seen: HashSet<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub text: String,
    pub at: String,
    pub running: bool,
}

impl Graph {
    fn item(&mut self, id: &str, at: &str, who: &str) -> &mut WorkItem {
        self.items.entry(id.to_string()).or_insert_with(|| WorkItem {
            id: id.to_string(),
            first: at.to_string(),
            last: at.to_string(),
            owner: who.to_string(),
            state: "open".into(),
            title: String::new(),
            touches: 0,
            steps: Vec::new(),
            review_asked: None,
            verdict_at: String::new(),
        })
    }

    /// Forget an item and its edges (a deleted branch).
    fn drop_item(&mut self, id: &str) {
        self.items.remove(id);
        self.touches.remove(id);
        self.merged.remove(id);
        self.edges.retain(|e| e.from != id && e.to != id);
        self.edge_set.retain(|e| e.from != id && e.to != id);
    }

    fn edge(&mut self, from: &str, to: &str, kind: &str) {
        let e = Edge { from: from.into(), to: to.into(), kind: kind.into() };
        if self.edge_set.insert(e.clone()) {
            self.edges.push(e);
        }
    }

    fn step(&mut self, item: &str, e: &Event, what: String) {
        let it = self.items.get_mut(item).expect("item exists");
        if e.at.as_str() > it.last.as_str() {
            it.last = e.at.clone();
        }
        it.touches += 1;
        if let Some(s) = it.steps.iter_mut().find(|s| s.event == e.id) {
            s.what = what; // a re-emitted post updates its step in place
            return;
        }
        // Insert in time order: O(log n) search instead of re-sorting the list on every step.
        let pos = it.steps.partition_point(|s| s.at <= e.at);
        it.steps.insert(pos, Step { at: e.at.clone(), who: e.from.clone(), what, event: e.id.clone() });
        if it.steps.len() > STEPS_MAX {
            let cut = it.steps.len() - STEPS_MAX;
            it.steps.drain(..cut);
        }
    }

    /// Apply one event through every deriver.
    pub fn apply(&mut self, e: &Event, derivers: &[Box<dyn Deriver>]) {
        let first_time = self.seen.insert(e.id.clone());
        for d in derivers {
            d.apply(e, first_time, self);
        }
    }
}

pub trait Deriver: Send + Sync {
    fn apply(&self, e: &Event, first_time: bool, g: &mut Graph);
}

/// Phrases that ask for a review even without an @mention.
const REVIEW_ASKS: &[&str] = &["please review", "please re-review", "ready for review", "up for review", "for your review", "fixed: re-review", "design for review"];

/// Messages create items for the ids they mention and record a step on each.
pub struct ItemDeriver;
impl Deriver for ItemDeriver {
    fn apply(&self, e: &Event, _first: bool, g: &mut Graph) {
        let Body::Message { items, mentions, .. } = &e.body else { return };
        let summary = e.summary();
        // "@codex please review P0a" asks for a review of every item it names.
        let mut of: Vec<String> = mentions.iter().filter(|m| **m != e.from).cloned().collect();
        // A message that reports a verdict is an answer, not a request. Posts files often ask
        // without an @mention ("A1-A4 ready for review"): that request goes to the other agent.
        let lower = summary.to_ascii_lowercase();
        let phrased = REVIEW_ASKS.iter().any(|p| lower.contains(p));
        let asks_review = !lower.contains("verdict") && ((!of.is_empty() && lower.contains("review")) || phrased);
        if asks_review && of.is_empty() {
            of.push("both".into());
        }
        for id in items {
            let it = g.item(id, &e.at, &e.from);
            if it.title.is_empty() || summary.contains(id.as_str()) {
                it.title = clip(&summary, 90);
            }
            if asks_review && it.review_asked.as_ref().is_none_or(|r| r.at <= e.at) {
                it.review_asked = Some(ReviewAsk { at: e.at.clone(), by: e.from.clone(), of: of.clone() });
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
        let it = g.item(item, &e.at, &e.from);
        it.owner = e.from.clone();
        if it.state == "open" {
            it.state = "claimed".into();
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
        let it = g.item(item, &e.at, &e.from);
        if e.at.as_str() >= it.last.as_str() || matches!(it.state.as_str(), "open" | "claimed" | "branch") {
            it.state = state.into();
        }
        if e.at > it.verdict_at {
            it.verdict_at = e.at.clone();
        }
        g.step(item, e, format!("{} review: {state}", e.from));
        g.edge(&e.from, item, &format!("review:{state}"));
        // A verdict on a branch carries to the items that branch implements (rare event, O(E)).
        if state == "pass" {
            let targets: Vec<String> = g.edges.iter().filter(|x| x.kind == "implements" && &x.from == item).map(|x| x.to.clone()).collect();
            for t in targets {
                inherit_pass(g, &t);
            }
        }
    }
}

/// Run events draw who-woke-whom edges and mark an agent as running.
pub struct WakeDeriver;
impl Deriver for WakeDeriver {
    fn apply(&self, e: &Event, _first: bool, g: &mut Graph) {
        match &e.body {
            Body::RunStarted { agent, .. } => {
                g.edge(&e.from, agent, "woke");
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
        let s = g.now.entry(e.from.clone()).or_insert(Status { text: String::new(), at: String::new(), running: false });
        if e.at.as_str() >= s.at.as_str() {
            s.text = clip(text, 120);
            s.at = e.at.clone();
        }
    }
}

/// Branch tips: a branch node per `claude/*` or `codex/*` branch, linked to the items its
/// latest subject mentions.
pub struct BranchDeriver;
impl Deriver for BranchDeriver {
    fn apply(&self, e: &Event, _first: bool, g: &mut Graph) {
        let Body::Commit { branch, sha, subject, touches, merged, gone } = &e.body else { return };
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
        let owner = branch.split('/').next().unwrap_or("").to_string();
        let it = g.item(branch, &e.at, &owner);
        it.title = clip(subject, 90);
        if it.state == "open" {
            it.state = "branch".into(); // a reviewed branch keeps its verdict colour
        }
        let passed = it.state == "pass";
        g.step(branch, e, format!("{} {}", &sha[..sha.len().min(8)], clip(subject, 80)));
        for id in crate::model::item_ids(subject) {
            g.item(&id, &e.at, &owner);
            g.edge(branch, &id, "implements");
            if passed {
                inherit_pass(g, &id);
            }
        }
    }
}

/// An item implemented on a passed branch reads as passed unless a later verdict says otherwise.
fn inherit_pass(g: &mut Graph, id: &str) {
    if let Some(t) = g.items.get_mut(id) {
        if matches!(t.state.as_str(), "open" | "claimed" | "review") {
            t.state = "pass".into();
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
        assert_eq!(it.state, "changes");
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
        assert_eq!(g.items["claude/hooks-p0a"].state, "pass");
        assert_eq!(g.items["P0a"].state, "pass");
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
