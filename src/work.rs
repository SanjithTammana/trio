//! The Work lens: which stage each work item is in, who it waits on, and a one-paragraph story,
//! all derived from the graph with templates (no model calls, O(V + E) per snapshot).

use crate::graph::{Graph, ReviewAsk, Verdict, WorkItem};
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
    /// When the item or any branch implementing it last moved: what a card's age shows.
    pub last: String,
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
///
/// The newest review event decides: a request after the last verdict puts it in review; else the
/// last verdict (pass is done; changes go back to the author unless a merge came after them);
/// else a full merge is done; else a claim or a branch is progress; else it is only asked.
/// Events compare by (time, arrival), so same-minute posts keep their file order.
pub fn work(it: &WorkItem, branches: &[&str], g: &Graph, me: &str) -> Work {
    let live: Vec<&WorkItem> = branches.iter().filter_map(|b| g.items.get(*b)).collect();
    let verdict = std::iter::once(it).chain(live.iter().copied()).flat_map(|n| n.verdicts.iter()).max_by(|a, b| a.key().cmp(&b.key()));
    let ask = it.review_asked.as_ref().filter(|r| verdict.is_none_or(|v| (r.at.as_str(), r.seq) > v.key()));
    let author = author(it, &live);
    let has_branch = !live.is_empty() || !it.merged_gone.is_empty();
    let all_merged = has_branch && live.iter().all(|b| g.merged.contains(&b.id));
    // The latest merge, by the same (minute, position) key as verdicts and requests.
    let merged = live.iter().filter_map(|b| b.merged_at.as_ref()).chain(it.merged_gone.iter().map(|m| &m.merged)).max();
    let (lane, waiting_on) = if let Some(r) = ask {
        ("review", reviewer(r, author.as_deref()))
    } else if let Some(v) = verdict {
        match v.outcome.as_str() {
            "pass" => ("done", None),
            _ if all_merged && merged.is_some_and(|(at, s)| (at.as_str(), *s) > v.key()) => ("done", None),
            "changes" | "blocked" => ("changes", agent_of(&v.on).or(author.clone())),
            _ => ("review", None),
        }
    } else if all_merged {
        ("done", None)
    } else if let Some(c) = &it.claimed_by {
        ("progress", Some(c.clone()))
    } else if has_branch {
        ("progress", author.clone())
    } else {
        ("asked", None)
    };
    let last = live.iter().map(|b| b.last.as_str()).chain(std::iter::once(it.last.as_str())).max().unwrap_or("").to_string();
    Work { lane, story: story(it, &live, all_merged, ask, lane, waiting_on.as_deref(), me), waiting_on, last }
}

/// The agent a branch belongs to, from its prefix (`claude/hooks-p0a` -> claude).
fn agent_of(node: &str) -> Option<String> {
    node.split_once('/').map(|(o, _)| o).filter(|o| AGENTS.contains(o)).map(str::to_string)
}

/// Who wrote the work: the one agent whose branches implement it, else whoever claimed it.
fn author(it: &WorkItem, live: &[&WorkItem]) -> Option<String> {
    let mut owners: Vec<String> = live.iter().map(|b| b.id.as_str()).chain(it.merged_gone.iter().map(|m| m.branch.as_str())).filter_map(agent_of).collect();
    owners.sort();
    owners.dedup();
    match owners.as_slice() {
        [one] => Some(one.clone()),
        _ => it.claimed_by.clone().filter(|c| owners.is_empty() || owners.contains(c)),
    }
}

/// Who a review request waits on. A named participant other than the asker is taken as asked;
/// "both" or no name means the agent that neither wrote the work nor asked, when exactly one is.
fn reviewer(r: &ReviewAsk, author: Option<&str>) -> Option<String> {
    if let Some(n) = r.of.iter().find(|m| *m != "both" && **m != r.by) {
        return Some(n.clone());
    }
    let c: Vec<&str> = AGENTS.iter().copied().filter(|a| *a != r.by && Some(*a) != author).collect();
    (c.len() == 1).then(|| c[0].to_string())
}

const AGENTS: [&str; 2] = ["claude", "codex"];

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

/// One paragraph from recorded facts only: who raised and claimed it, its branches, each
/// verdict with its own reviewer, an open review request, and who it waits on now.
fn story(it: &WorkItem, live: &[&WorkItem], all_merged: bool, ask: Option<&ReviewAsk>, lane: &str, waiting_on: Option<&str>, me: &str) -> String {
    let mut s: Vec<String> = Vec::new();
    s.push(format!("{} raised it at {}.", cap(&who(&it.raised_by, me)), when(&it.first)));
    if let Some(c) = &it.claimed_by {
        s.push(format!("{} claimed it.", cap(&who(c, me))));
    }
    let names: Vec<&str> = live.iter().map(|b| b.id.as_str()).chain(it.merged_gone.iter().map(|m| m.branch.as_str())).collect();
    if !names.is_empty() {
        let shown = names.iter().take(3).copied().collect::<Vec<_>>().join(", ");
        let more = if names.len() > 3 { format!(" and {} more", names.len() - 3) } else { String::new() };
        let merged = if all_merged { if names.len() == 1 { ", merged" } else { ", all merged" } } else { "" };
        s.push(format!("On {} {shown}{more}{merged}.", if names.len() == 1 { "branch" } else { "branches" }));
    }
    // A verdict naming a branch and its item arrives twice (one event per node): count it once.
    let mut verdicts: Vec<&Verdict> = std::iter::once(it).chain(live.iter().copied()).flat_map(|n| n.verdicts.iter()).collect();
    verdicts.sort_by(|a, b| a.key().cmp(&b.key()));
    let mut lines = std::collections::HashSet::new();
    verdicts.retain(|v| lines.insert(v.line.as_str()));
    if let Some(first) = verdicts.first() {
        let n = verdicts.len();
        let recent = &verdicts[n.saturating_sub(6)..];
        let earlier = if n > recent.len() { "…, " } else { "" };
        if verdicts.iter().all(|v| v.by == first.by) {
            let list: Vec<&str> = recent.iter().map(|v| v.outcome.as_str()).collect();
            let times = if n == 1 { "once".to_string() } else { format!("{n} times") };
            s.push(format!("{} reviewed it {times}: {earlier}{}.", cap(&who(&first.by, me)), list.join(", ")));
        } else {
            let list: Vec<String> = recent.iter().map(|v| format!("{} {}", who(&v.by, me), v.outcome)).collect();
            s.push(format!("Reviews: {earlier}{}.", list.join(", ")));
        }
    }
    if let Some(r) = ask {
        s.push(format!("{} asked for a review at {}.", cap(&who(&r.by, me)), when(&r.at)));
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
        assert!(w.story.contains("On branch codex/x."), "{}", w.story);
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

    fn verdict(item: &str, outcome: &str) -> Body {
        Body::Verdict { item: item.into(), outcome: outcome.into() }
    }
    fn commit(branch: &str, subject: &str, merged: bool, gone: bool) -> Body {
        Body::Commit { branch: branch.into(), sha: "abc12345".into(), subject: subject.into(), touches: vec![], merged, gone }
    }

    #[test]
    fn the_newest_review_event_decides_whatever_node_it_names() {
        let d = derivers();
        let mut g = Graph::default();
        g.apply(&ev("c1", "2026-11-23T09:00", "claude", commit("claude/x", "XF1 work", false, false)), &d);
        g.apply(&ev("v1", "2026-11-23T10:00", "codex", verdict("XF1", "changes")), &d);
        // An older branch pass arriving late does not override the newer item verdict.
        g.apply(&ev("v0", "2026-11-23T09:30", "codex", verdict("claude/x", "pass")), &d);
        let w = &all(&g, "sanjith")["XF1"];
        assert_eq!((w.lane, w.waiting_on.as_deref()), ("changes", Some("claude")), "{}", w.story);
        assert!(w.story.contains("Codex reviewed it 2 times: pass, changes."), "{}", w.story);
        // A request in the same minute as the verdict but after it in the file is newer.
        g.apply(&ev("m1", "2026-11-23T10:00", "claude", msg("XF1 fixed: re-review please @codex", &["codex"], &["XF1"])), &d);
        let w = &all(&g, "sanjith")["XF1"];
        assert_eq!((w.lane, w.waiting_on.as_deref()), ("review", Some("codex")), "{}", w.story);
        assert!(w.story.contains("Claude asked for a review at 11-23 10:00."), "{}", w.story);
        // A request after a pass reopens review.
        g.apply(&ev("v2", "2026-11-23T11:00", "codex", verdict("XF1", "pass")), &d);
        assert_eq!(all(&g, "sanjith")["XF1"].lane, "done");
        g.apply(&ev("m2", "2026-11-23T12:00", "sanjith", msg("@both please review XF1 again", &["both"], &["XF1"])), &d);
        let w = &all(&g, "sanjith")["XF1"];
        // From a third participant, "both" means the agent that did not write it.
        assert_eq!((w.lane, w.waiting_on.as_deref()), ("review", Some("codex")), "{}", w.story);
    }

    #[test]
    fn changes_after_a_merge_go_back_to_the_branch_author() {
        let d = derivers();
        let mut g = Graph::default();
        g.apply(&ev("c1", "2026-11-23T09:00", "claude", commit("claude/x", "XF1 work", true, false)), &d);
        assert_eq!(all(&g, "sanjith")["XF1"].lane, "done");
        g.apply(&ev("v1", "2026-11-23T10:00", "codex", verdict("claude/x", "changes")), &d);
        let w = &all(&g, "sanjith")["XF1"];
        assert_eq!((w.lane, w.waiting_on.as_deref()), ("changes", Some("claude")), "{}", w.story);
    }

    #[test]
    fn an_unaddressed_request_from_the_reviewer_names_nobody() {
        let d = derivers();
        let mut g = Graph::default();
        g.apply(&ev("c1", "2026-11-23T09:00", "claude", commit("claude/x", "XF1 work", false, false)), &d);
        g.apply(&ev("m1", "2026-11-23T10:00", "codex", msg("XF1 ready for review", &[], &["XF1"])), &d);
        let w = &all(&g, "sanjith")["XF1"];
        assert_eq!((w.lane, w.waiting_on.as_deref()), ("review", None), "never Claude on its own work: {}", w.story);
    }

    #[test]
    fn review_words_are_whole_words_and_verdict_lines_are_answers() {
        let d = derivers();
        let mut g = Graph::default();
        g.apply(&ev("m1", "2026-11-23T10:00", "claude", msg("@codex P0a preview is live", &["codex"], &["P0a"])), &d);
        g.apply(&ev("m2", "2026-11-23T10:01", "claude", msg("fixture: XF2 already for reviewers", &[], &["XF2"])), &d);
        g.apply(&ev("m3", "2026-11-23T10:02", "claude", msg("@codex please review the verdict serializer XF3", &["codex"], &["XF3"])), &d);
        let all = all(&g, "sanjith");
        assert_eq!(all["P0a"].lane, "asked");
        assert_eq!(all["XF2"].lane, "asked");
        assert_eq!(all["XF3"].lane, "review");
    }

    #[test]
    fn done_survives_deleting_the_merged_branch() {
        let d = derivers();
        let mut g = Graph::default();
        g.apply(&ev("c1", "2026-11-23T09:00", "claude", commit("claude/x", "XF1 work", false, false)), &d);
        g.apply(&ev("c2", "2026-11-23T10:00", "claude", commit("claude/x", "XF1 work", true, false)), &d);
        // The watcher's deletion event says merged: false; the graph remembers the merge.
        g.apply(&ev("c3", "2026-11-23T11:00", "claude", commit("claude/x", "", false, true)), &d);
        let w = &all(&g, "sanjith")["XF1"];
        assert_eq!(w.lane, "done", "{}", w.story);
        assert!(w.story.contains("On branch claude/x, merged."), "{}", w.story);
    }

    fn post(file: &str, from: &str, text: &str) -> Vec<Event> {
        use crate::source::Source;
        let p = std::env::temp_dir().join(format!("trio-work-{}-{file}", std::process::id()));
        std::fs::write(&p, text).unwrap();
        let evs = crate::source::PostsFile::new(p.clone(), from, "sanjith").poll().unwrap();
        std::fs::remove_file(p).ok();
        evs
    }

    #[test]
    fn a_growing_ask_post_keeps_its_place_after_a_same_minute_pass() {
        let d = derivers();
        let mut g = Graph::default();
        let ask = ev("claude-posts.md@0", "2026-11-23T10:00", "claude", msg("@codex please review XF1", &["codex"], &["XF1"]));
        g.apply(&ask, &d);
        g.apply(&ev("codex-posts.md@0#1:XF1", "2026-11-23T10:00", "codex", verdict("XF1", "pass")), &d);
        assert_eq!(all(&g, "sanjith")["XF1"].lane, "done");
        // The ask post grows (same id): still the same request, still answered.
        let grown = ev("claude-posts.md@0", "2026-11-23T10:00", "claude", msg("@codex please review XF1 (now with notes)", &["codex"], &["XF1"]));
        g.apply(&grown, &d);
        assert_eq!(all(&g, "sanjith")["XF1"].lane, "done");
    }

    #[test]
    fn live_and_cold_replay_agree_across_two_imports() {
        // Source 1 (claude-posts) asks, source 2 (codex-posts) passes, in the same minute.
        let ask = ev("claude-posts.md@9", "2026-11-23T10:00", "claude", msg("@codex please re-review XF1", &["codex"], &["XF1"]));
        let pass = ev("codex-posts.md@9#2:XF1", "2026-11-23T10:00", "codex", verdict("XF1", "pass"));
        let (p_ask, p_pass) = ((1u64 << 40) | 1, (2u64 << 40) | 1);
        let lane = |first: (&Event, u64), second: (&Event, u64)| {
            let d = derivers();
            let mut g = Graph::default();
            g.apply_at(first.0, Some(first.1), &d);
            g.apply_at(second.0, Some(second.1), &d);
            all(&g, "sanjith")["XF1"].lane
        };
        let live = lane((&pass, p_pass), (&ask, p_ask));
        let cold = lane((&ask, p_ask), (&pass, p_pass));
        assert_eq!(live, cold, "arrival order must not decide");
    }

    #[test]
    fn a_post_title_names_the_reviewer_and_a_fenced_example_is_not_an_answer() {
        let d = derivers();
        let mut g = Graph::default();
        let text = "# Claude\n\n## 2026-11-23 10:00 @codex XF1 needs review\n\nThanks @claude for implementing it. Reply like:\n\n```\nverdict XF1: pass\n```\n";
        for e in post("claude-posts.md", "claude", text) {
            g.apply(&e, &d);
        }
        let w = &all(&g, "sanjith")["XF1"];
        assert_eq!((w.lane, w.waiting_on.as_deref()), ("review", Some("codex")), "{}", w.story);
        // The human's handle counts as a mention in imported posts too.
        for e in post("codex-posts.md", "codex", "## 2026-11-23 11:00 @sanjith XF2 needs your review\n") {
            g.apply(&e, &d);
        }
        assert_eq!(all(&g, "sanjith")["XF2"].waiting_on.as_deref(), Some("sanjith"));
    }

    #[test]
    fn a_late_branch_link_never_overrides_a_newer_item_verdict() {
        let d = derivers();
        let mut g = Graph::default();
        g.apply(&ev("v0", "2026-11-23T09:30", "codex", verdict("claude/x", "pass")), &d);
        g.apply(&ev("v1", "2026-11-23T10:00", "codex", verdict("XF1", "review")), &d);
        // Git links the branch to XF1 afterwards, then refreshes it.
        g.apply(&ev("git:claude/x", "2026-11-23T10:30", "claude", commit("claude/x", "XF1 work", false, false)), &d);
        g.apply(&ev("git:claude/x", "2026-11-23T10:40", "claude", commit("claude/x", "XF1 more", false, false)), &d);
        assert_eq!(g.items["XF1"].state, "review", "exported state");
        assert_eq!(all(&g, "sanjith")["XF1"].lane, "review");
    }

    #[test]
    fn same_minute_verdicts_by_one_reviewer_all_count() {
        let d = derivers();
        let mut g = Graph::default();
        for (i, o) in ["pass", "changes", "pass"].iter().enumerate() {
            g.apply(&ev(&format!("codex-posts.md@0#{i}:XF1"), "2026-11-23T10:00", "codex", verdict("XF1", o)), &d);
        }
        let w = &all(&g, "sanjith")["XF1"];
        assert_eq!(w.lane, "done", "{}", w.story);
        assert!(w.story.contains("3 times: pass, changes, pass"), "{}", w.story);
    }

    #[test]
    fn the_cap_drops_the_oldest_verdicts_not_the_newest() {
        let d = derivers();
        let mut g = Graph::default();
        g.apply(&ev("vnew", "2026-11-23T12:00", "codex", verdict("XF1", "pass")), &d);
        for i in 0..40 {
            g.apply(&ev(&format!("vold{i}"), "2026-11-22T09:00", "codex", verdict("XF1", "changes")), &d);
        }
        assert_eq!(g.items["XF1"].verdicts.len(), 32);
        assert_eq!(all(&g, "sanjith")["XF1"].lane, "done");
    }

    #[test]
    fn a_merge_seen_after_changes_finishes_the_item_even_in_the_same_minute() {
        let d = derivers();
        let mut g = Graph::default();
        g.apply(&ev("git:claude/x", "2026-11-23T09:00", "claude", commit("claude/x", "XF1 work", false, false)), &d);
        g.apply(&ev("v1", "2026-11-23T10:00", "codex", verdict("claude/x", "changes")), &d);
        assert_eq!(all(&g, "sanjith")["XF1"].lane, "changes");
        // The watcher stamps the merge when it sees it, in the verdict's minute, after it.
        g.apply(&ev("git:claude/x:merged:abc", "2026-11-23T10:00", "claude", commit("claude/x", "XF1 work", true, false)), &d);
        assert_eq!(all(&g, "sanjith")["XF1"].lane, "done");
    }

    #[test]
    fn a_restart_replays_a_merge_whose_branch_is_gone() {
        // What the ledger holds after the watcher saw claude/x merge and then disappear.
        let d = derivers();
        let mut g = Graph::default();
        g.apply(&ev("m1", "2026-11-23T08:00", "sanjith", msg("XF1: fix the hook drift on startup", &[], &["XF1"])), &d);
        g.apply(&ev("git:claude/x:merged:abc", "2026-11-23T11:00", "claude", commit("claude/x", "XF1 work", true, false)), &d);
        g.apply(&ev("git:claude/x:gone:abc", "2026-11-23T12:00", "claude", commit("claude/x", "", false, true)), &d);
        let w = &all(&g, "sanjith")["XF1"];
        assert_eq!(w.lane, "done", "{}", w.story);
        assert!(!g.items.contains_key("claude/x"));
    }
}
