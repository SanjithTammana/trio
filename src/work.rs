//! The Work lens: which stage each work item is in, who it waits on, and a one-paragraph story,
//! all derived from the graph with templates (no model calls, O(V + E) per snapshot).

use crate::graph::{Graph, WorkItem};
use crate::model::Entity;
use serde::Serialize;
use std::collections::HashMap;

/// The five Work lanes, in board order.
pub const LANES: [&str; 5] = ["asked", "progress", "review", "changes", "done"];

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Work {
    pub lane: &'static str,
    /// The participant the item waits on next, when one can be named.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_on: Option<String>,
    pub story: String,
}

/// Branches that implement each item, from the `implements` edges.
pub fn implementers(g: &Graph) -> HashMap<&str, Vec<&str>> {
    let mut by: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in g.edges.iter().filter(|e| e.kind == "implements") {
        by.entry(e.to.as_str()).or_default().push(e.from.as_str());
    }
    by
}

/// The Work reading of one item (not a branch).
pub fn work(it: &WorkItem, branches: &[&str], g: &Graph, me: &str) -> Work {
    // Reviews usually name the branch, not the item: the latest verdict on the item or on any
    // branch that implements it decides its state.
    let mut verdict_at = it.verdict_at.as_str();
    let mut state = it.state.as_str();
    for b in branches.iter().filter_map(|b| g.items.get(*b)) {
        if b.verdict_at.as_str() > verdict_at {
            verdict_at = &b.verdict_at;
            state = &b.state;
        }
    }
    let ask = it.review_asked.as_ref().filter(|r| r.at.as_str() > verdict_at);
    let author = branches
        .iter()
        .filter_map(|b| b.split('/').next())
        .find(|o| *o == "claude" || *o == "codex")
        .map(str::to_string)
        .unwrap_or_else(|| it.owner.clone());
    let all_merged = !branches.is_empty() && branches.iter().all(|b| g.merged.contains(*b));
    let (lane, waiting_on) = match state {
        "pass" => ("done", None),
        _ if all_merged => ("done", None),
        // A request made after the last verdict hands the item back to a reviewer.
        _ if ask.is_some() => ("review", ask.and_then(|r| reviewer(&r.of, &r.by))),
        "changes" | "blocked" => ("changes", Some(author.clone())),
        "review" => ("review", None),
        "claimed" => ("progress", Some(it.owner.clone())),
        _ if !branches.is_empty() => ("progress", Some(author.clone())),
        _ => ("asked", None),
    };
    Work { lane, story: story(it, branches, g, lane, waiting_on.as_deref(), me), waiting_on }
}

/// Who a review request is addressed to: the first named participant that is not the asker.
fn reviewer(of: &[String], by: &str) -> Option<String> {
    let named: Vec<&str> = of
        .iter()
        .flat_map(|m| if m == "both" { vec!["claude", "codex"] } else { vec![m.as_str()] })
        .filter(|m| *m != by)
        .collect();
    named.first().map(|m| m.to_string())
}

fn who(name: &str, me: &str) -> String {
    if name == me || name.is_empty() {
        return "you".into();
    }
    cap(name)
}

/// Sentence case for a name that starts a sentence ("you" -> "You").
fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// `2026-11-23T18:30:00` → `11-23 18:30`.
fn when(at: &str) -> String {
    let (d, t) = at.split_once('T').unwrap_or((at, ""));
    format!("{} {}", d.get(5..).unwrap_or(d), t.get(..5).unwrap_or(t)).trim().to_string()
}

/// One paragraph from the item's own steps: who raised it, claims, branches, verdicts, and now.
fn story(it: &WorkItem, branches: &[&str], g: &Graph, lane: &str, waiting_on: Option<&str>, me: &str) -> String {
    let mut s: Vec<String> = Vec::new();
    let raiser = it.steps.first().map(|x| x.who.as_str()).unwrap_or(it.owner.as_str());
    s.push(format!("{} raised it at {}.", cap(&who(raiser, me)), when(&it.first)));
    if let Some(c) = it.steps.iter().rev().find(|x| x.what.ends_with(" claimed it")) {
        s.push(format!("{} claimed it.", cap(&who(&c.who, me))));
    }
    if !branches.is_empty() {
        let commits: usize = branches.iter().filter_map(|b| g.items.get(*b)).map(|b| b.steps.len()).sum();
        let names = branches.join(", ");
        s.push(format!("{commits} commit{} on {names}.", if commits == 1 { "" } else { "s" }));
    }
    // Verdict steps are exactly "<who> review: <state>" (ReviewDeriver), on the item or on a
    // branch that implements it; a message titled "Design for review: ..." is not one.
    let mut verdicts: Vec<(&str, &str, &str)> = std::iter::once(it)
        .chain(branches.iter().filter_map(|b| g.items.get(*b)))
        .flat_map(|n| n.steps.iter())
        .filter_map(|x| {
            let (by, v) = x.what.split_once(" review: ")?;
            (by == x.who && matches!(v, "pass" | "changes" | "blocked" | "review")).then_some((x.at.as_str(), by, v))
        })
        .collect();
    verdicts.sort();
    let verdicts: Vec<(&str, &str)> = verdicts.into_iter().map(|(_, by, v)| (by, v)).collect();
    if let Some((by, _)) = verdicts.last() {
        let list: Vec<&str> = verdicts.iter().map(|(_, v)| *v).collect();
        let n = list.len();
        s.push(format!("{} reviewed it {}: {}.", cap(&who(by, me)), if n == 1 { "once".to_string() } else { format!("{n} times") }, list.join(", ")));
    }
    let now = match (lane, waiting_on) {
        ("done", _) => "Done.".to_string(),
        ("asked", _) => "Nobody has picked it up yet.".to_string(),
        (_, Some(w)) if w == me => "Waiting on you.".to_string(),
        (_, Some(w)) => format!("Waiting on {}.", who(w, me)),
        ("review", None) => "Waiting on a review.".to_string(),
        _ => "In progress.".to_string(),
    };
    s.push(now);
    s.join(" ")
}

/// The Work reading of every item in the graph, keyed by id. Branches are left to the Code lens.
pub fn all(g: &Graph, me: &str) -> HashMap<String, Work> {
    let by = implementers(g);
    g.items
        .values()
        .filter(|it| it.kind() == "item")
        .map(|it| {
            let branches = by.get(it.id.as_str()).map(Vec::as_slice).unwrap_or(&[]);
            (it.id.clone(), work(it, branches, g, me))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::derivers;
    use crate::model::{Body, Event};

    fn ev(id: &str, at: &str, from: &str, body: Body) -> Event {
        Event { id: id.into(), at: at.into(), from: from.into(), body }
    }
    fn msg(summary: &str, mentions: &[&str], items: &[&str]) -> Body {
        Body::Message { summary: summary.into(), detail: None, mentions: mentions.iter().map(|s| s.to_string()).collect(), items: items.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn an_item_moves_through_the_lanes() {
        let d = derivers();
        let mut g = Graph::default();
        g.apply(&ev("m1", "2026-11-23T18:30", "sanjith", msg("P0a: fix the hook drift", &[], &["P0a"])), &d);
        assert_eq!(all(&g, "sanjith")["P0a"].lane, "asked");

        g.apply(&ev("c1", "2026-11-23T18:35", "claude", Body::Claim { item: "P0a".into() }), &d);
        let w = &all(&g, "sanjith")["P0a"];
        assert_eq!((w.lane, w.waiting_on.as_deref()), ("progress", Some("claude")));

        g.apply(&ev("m2", "2026-11-23T18:40", "claude", msg("@codex please review P0a", &["codex"], &["P0a"])), &d);
        let w = &all(&g, "sanjith")["P0a"];
        assert_eq!((w.lane, w.waiting_on.as_deref()), ("review", Some("codex")));

        g.apply(&ev("v1", "2026-11-23T18:50", "codex", Body::Verdict { item: "P0a".into(), outcome: "changes".into() }), &d);
        let w = &all(&g, "sanjith")["P0a"];
        assert_eq!((w.lane, w.waiting_on.as_deref()), ("changes", Some("claude")));

        // A re-review request after the verdict puts it back in review.
        g.apply(&ev("m3", "2026-11-23T19:00", "claude", msg("@codex please re-review P0a", &["codex"], &["P0a"])), &d);
        let w = &all(&g, "sanjith")["P0a"];
        assert_eq!((w.lane, w.waiting_on.as_deref()), ("review", Some("codex")));

        g.apply(&ev("v2", "2026-11-23T19:10", "codex", Body::Verdict { item: "P0a".into(), outcome: "pass".into() }), &d);
        let w = &all(&g, "sanjith")["P0a"];
        assert_eq!((w.lane, w.waiting_on.as_deref()), ("done", None));
        assert_eq!(
            w.story,
            "You raised it at 11-23 18:30. Claude claimed it. Codex reviewed it 2 times: changes, pass. Done."
        );
    }

    #[test]
    fn a_review_request_to_both_waits_on_the_other_agent_and_names_you() {
        let d = derivers();
        let mut g = Graph::default();
        g.apply(&ev("m1", "2026-11-23T10:00", "claude", msg("B3 design ready for review @both", &["both"], &["B3"])), &d);
        let w = &all(&g, "sanjith")["B3"];
        assert_eq!((w.lane, w.waiting_on.as_deref()), ("review", Some("codex")));
        g.apply(&ev("m2", "2026-11-23T10:05", "codex", msg("@sanjith B3 needs your review", &["sanjith"], &["B3"])), &d);
        let w = &all(&g, "sanjith")["B3"];
        assert_eq!(w.waiting_on.as_deref(), Some("sanjith"));
        assert!(w.story.ends_with("Waiting on you."), "{}", w.story);
    }

    #[test]
    fn a_branch_makes_an_item_in_progress_and_merging_it_finishes_it() {
        let d = derivers();
        let mut g = Graph::default();
        let commit = |merged| Body::Commit { branch: "codex/x".into(), sha: "abc12345".into(), subject: "XF1 work".into(), touches: vec![], merged, gone: false };
        g.apply(&ev("g1", "2026-11-23T09:00", "codex", commit(false)), &d);
        let w = &all(&g, "sanjith")["XF1"];
        assert_eq!((w.lane, w.waiting_on.as_deref()), ("progress", Some("codex")));
        assert!(w.story.contains("1 commit on codex/x."), "{}", w.story);
        assert!(!all(&g, "sanjith").contains_key("codex/x"), "branches are not Work cards");
        g.apply(&ev("g2", "2026-11-23T10:00", "codex", commit(true)), &d);
        assert_eq!(all(&g, "sanjith")["XF1"].lane, "done");
    }

    #[test]
    fn a_verdict_on_the_branch_moves_the_items_it_implements() {
        let d = derivers();
        let mut g = Graph::default();
        let commit = Body::Commit { branch: "claude/hooks-p0a".into(), sha: "553f50d1".into(), subject: "P0a: fix drift".into(), touches: vec![], merged: false, gone: false };
        g.apply(&ev("g1", "2026-11-23T18:40", "claude", commit), &d);
        g.apply(&ev("m1", "2026-11-23T18:41", "claude", msg("@codex please review P0a on claude/hooks-p0a", &["codex"], &["P0a"])), &d);
        assert_eq!(all(&g, "sanjith")["P0a"].lane, "review");
        g.apply(&ev("v1", "2026-11-23T20:23", "codex", Body::Verdict { item: "claude/hooks-p0a".into(), outcome: "changes".into() }), &d);
        // The verdict message itself mentions Claude but is not a new request.
        g.apply(&ev("m2", "2026-11-23T20:23", "codex", msg("verdict claude/hooks-p0a: changes; P0a review @claude", &["claude"], &["P0a"])), &d);
        let w = &all(&g, "sanjith")["P0a"];
        assert_eq!((w.lane, w.waiting_on.as_deref()), ("changes", Some("claude")));
        assert!(w.story.contains("Codex reviewed it once: changes."), "{}", w.story);
    }

    #[test]
    fn a_title_with_review_in_it_is_not_a_verdict() {
        let d = derivers();
        let mut g = Graph::default();
        g.apply(&ev("m1", "2026-11-23T02:38", "claude", msg("B3 Design for review: project-owned artifacts", &[], &["B3"])), &d);
        let w = &all(&g, "sanjith")["B3"];
        assert!(!w.story.contains("reviewed"), "{}", w.story);
        // Asked without an @mention: the request goes to the other agent.
        assert_eq!((w.lane, w.waiting_on.as_deref()), ("review", Some("codex")));
        g.apply(&ev("m2", "2026-11-23T03:00", "codex", msg("XF29 is still waiting on Codex re-review", &[], &["XF29"])), &d);
        assert_eq!(all(&g, "sanjith")["XF29"].lane, "asked", "a status line is not a request");
    }
}
