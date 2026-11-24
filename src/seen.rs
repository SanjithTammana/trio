//! "Since you left": one seen mark per room (when the human was last here, and each item's Work
//! lane at that moment) and the catch-up it yields on return. Work done outside Trio shows up
//! because it reaches the ledger anyway: posts files, branch commits and attached-session hooks
//! are all sources. Agents get the same idea as a short digest in their wake prompt.

use crate::graph::Graph;
use crate::model::{clip, Body, Event};
use crate::work::{Work, LANES};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Moves listed by name in the catch-up; the rest are counted.
const MOVES_SHOWN: usize = 8;
/// Lines in an agent's "since your last run" digest.
const DIGEST_LINES: usize = 5;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Seen {
    /// Wall time for display. The process-local position decides what's new.
    pub at: String,
    pub position: u64,
    /// Work lane of every item at the mark.
    pub lanes: BTreeMap<String, String>,
    /// How many events each source had given at the mark (`ledger`, `import:<file>`). Replay
    /// numbers a source's events the same way every time, so after a restart the events past
    /// these counts are the ones that came in while Trio was closed.
    #[serde(default)]
    pub sources: BTreeMap<String, u64>,
}

/// The saved mark, if any. A missing or unreadable file is no mark.
pub fn load(root: &Path) -> Option<Seen> {
    serde_json::from_slice(&std::fs::read(path(root)).ok()?).ok()
}

pub fn path(root: &Path) -> PathBuf {
    root.join(".trio").join("seen.json")
}

/// Write through a temporary file, so a crash never leaves half a mark.
pub fn save(root: &Path, s: &Seen) -> std::io::Result<()> {
    let p = path(root);
    let tmp = p.with_extension("json.tmp");
    std::fs::create_dir_all(root.join(".trio"))?;
    std::fs::write(&tmp, serde_json::to_vec(s).map_err(std::io::Error::other)?)?;
    std::fs::rename(tmp, p)
}

pub fn mark(at: &str, position: u64, work: &HashMap<String, Work>, sources: BTreeMap<String, u64>) -> Seen {
    Seen { at: at.to_string(), position, lanes: work.iter().map(|(id, w)| (id.clone(), w.lane.to_string())).collect(), sources }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Moved {
    pub id: String,
    pub title: String,
    /// None: the item is new since the mark.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub to: String,
    pub yours: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Catchup {
    pub since: String,
    pub moved: Vec<Moved>,
    /// Moves beyond the ones listed.
    pub more: usize,
    pub waiting_on_you: usize,
    pub commits: usize,
    pub branches: usize,
    pub verdicts: usize,
    pub messages: usize,
    pub partial: bool,
}

/// What changed since `seen`, by others. None when nothing did. O(items + events).
pub fn catchup<'a>(seen: &Seen, g: &Graph, work: &HashMap<String, Work>, events: impl Iterator<Item = &'a (u64, Event)>, me: &str, partial: bool) -> Option<Catchup> {
    let rank = |l: &str| LANES.iter().position(|x| *x == l).unwrap_or(0);
    // Waiting on you first, then by board order, then id: a stable list between snapshots.
    let mut top = Vec::new();
    let mut moves = 0usize;
    for (id, w) in work {
        let before = seen.lanes.get(id);
        if g.items[id].moved_by == me || before.is_some_and(|b| b == w.lane) {
            continue;
        }
        moves += 1;
        let key = (w.waiting_on.as_deref() != Some(me), rank(w.lane), id);
        let i = top.partition_point(|x| *x < key);
        if i < MOVES_SHOWN {
            top.insert(i, key);
            top.truncate(MOVES_SHOWN);
        }
    }
    let moved = top.into_iter().map(|(not_yours, _, id)| Moved {
        id: id.clone(), title: g.items[id].title.clone(), from: seen.lanes.get(id).cloned(),
        to: work[id].lane.into(), yours: !not_yours,
    }).collect();
    let mut branches = HashSet::new();
    let mut verdicts = HashSet::new();
    let mut c = Catchup {
        since: seen.at.clone(),
        more: moves.saturating_sub(MOVES_SHOWN),
        waiting_on_you: work.values().filter(|w| w.waiting_on.as_deref() == Some(me)).count(),
        commits: 0, branches: 0, verdicts: 0, messages: 0, moved, partial,
    };
    for (_, e) in events.filter(|(pos, e)| *pos > seen.position && e.from != me) {
        match &e.body {
            Body::Commit { branch, .. } if crate::gitwatch::is_tip(e) => {
                c.commits += 1;
                branches.insert(branch);
            }
            Body::Verdict { item, .. } => { verdicts.insert(e.verdict_line(item)); }
            Body::Message { .. } => c.messages += 1,
            _ => {}
        }
    }
    c.branches = branches.len();
    c.verdicts = verdicts.len();
    (c.partial || moves > 0 || c.commits + c.verdicts + c.messages > 0).then_some(c)
}

/// For a woken agent: what others did since its last run, newest last, at most five lines.
/// The latest messages already reach it, so this covers commits, verdicts and claims.
pub fn digest<'a>(events: impl Iterator<Item = &'a (u64, Event)> + Clone, agent: &str) -> Vec<String> {
    let since = events.clone().filter_map(|(pos, e)| match &e.body {
        Body::RunStarted { agent: a, .. } if a == agent => Some(*pos),
        _ => None,
    }).max();
    let Some(since) = since else { return Vec::new() };
    let mut lines: Vec<_> = events
        .filter(|(pos, e)| *pos > since && e.from != agent)
        .filter_map(|(pos, e)| match &e.body {
            Body::Commit { branch, gone: true, .. } => Some(format!("{branch} was deleted")),
            Body::Commit { branch, .. } if crate::gitwatch::is_merge(e) => Some(format!("{branch} was merged")),
            Body::Commit { branch, subject, .. } if crate::gitwatch::is_tip(e) => Some(format!("{} committed on {branch}: {}", e.from, clip(subject, 80))),
            Body::Verdict { item, outcome } => Some(format!("{} gave {item} a verdict: {outcome}", e.from)),
            Body::Claim { item } => Some(format!("{} claimed {item}", e.from)),
            _ => None,
        }.map(|line| ((e.at.as_str(), *pos), line)))
        .collect();
    lines.sort_unstable_by_key(|(key, _)| *key);
    let skip = lines.len().saturating_sub(DIGEST_LINES);
    let mut out: Vec<String> = Vec::new();
    if skip > 0 {
        out.push(format!("({skip} earlier changes not shown)"));
    }
    out.extend(lines.into_iter().skip(skip).map(|(_, line)| line));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::derivers;

    fn ev(id: &str, at: &str, from: &str, body: Body) -> Event {
        Event { id: id.into(), at: at.into(), from: from.into(), body }
    }
    fn msg(summary: &str, items: &[&str]) -> Body {
        Body::Message { summary: summary.into(), detail: None, mentions: vec![], items: items.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn the_catchup_lists_what_moved_since_the_mark() {
        let d = derivers();
        let mut g = Graph::default();
        let mut events = vec![(1, ev("m1", "2026-11-23T09:00", "claude", msg("XF1 needs doing", &["XF1"])))];
        for (_, e) in &events {
            g.apply(e, &d);
        }
        let seen = mark("2026-11-23T09:30", 1, &crate::work::all(&g, "sanjith"), Default::default());
        assert_eq!(catchup(&seen, &g, &crate::work::all(&g, "sanjith"), events.iter(), "sanjith", false), None, "nothing changed");
        // While away: Codex verdicts XF1 and a new item appears.
        let later = [
            ev("v1", "2026-11-23T10:00", "codex", Body::Verdict { item: "XF1".into(), outcome: "pass".into() }),
            ev("m2", "2026-11-23T10:05", "codex", msg("XF2 found a bug", &["XF2"])),
        ];
        for e in later {
            g.apply(&e, &d);
            events.push((events.len() as u64 + 1, e));
        }
        let c = catchup(&seen, &g, &crate::work::all(&g, "sanjith"), events.iter(), "sanjith", false).expect("changes");
        assert_eq!(c.since, "2026-11-23T09:30");
        let moves: Vec<(&str, Option<&str>, &str)> = c.moved.iter().map(|m| (m.id.as_str(), m.from.as_deref(), m.to.as_str())).collect();
        assert_eq!(moves, vec![("XF2", None, "asked"), ("XF1", Some("asked"), "done")]);
        assert_eq!((c.verdicts, c.messages), (1, 1));
        // Your own actions are not news to you.
        let mine = [(4, ev("m3", "2026-11-23T10:10", "sanjith", msg("noted", &[])))];
        events.extend(mine);
        assert_eq!(catchup(&seen, &g, &crate::work::all(&g, "sanjith"), events.iter(), "sanjith", false).expect("still").messages, 1);
    }

    #[test]
    fn an_agent_digest_covers_what_others_did_since_its_last_run() {
        let events = vec![
            ev("r1", "2026-11-23T09:00", "sanjith", Body::RunStarted { agent: "codex".into(), hop: 1, trigger: "m0".into() }),
            ev("c1", "2026-11-23T09:30", "claude", Body::Commit { branch: "claude/x".into(), sha: "abc12345".into(), subject: "XF1 fix".into(), touches: vec![], merged: false, gone: false }),
            ev("c2", "2026-11-23T09:40", "codex", Body::Claim { item: "XF9".into() }),
        ].into_iter().zip(1..).map(|(e, pos)| (pos, e)).collect::<Vec<_>>();
        assert_eq!(digest(events.iter(), "codex"), vec!["claude committed on claude/x: XF1 fix".to_string()]);
        assert!(digest(events.iter(), "claude").is_empty(), "never ran: the recent messages are enough");
    }

    #[test]
    fn catchup_counts_tip_updates_and_verdict_lines_without_human_moves() {
        let d = derivers();
        let mut g = Graph::default();
        let at = "2026-11-23T09:00";
        g.apply(&ev("start", at, "claude", msg("XF1 needs work", &["XF1"])), &d);
        let before = mark(at, 1, &crate::work::all(&g, "sanjith"), Default::default());
        let git = |id: &str, branch: &str, merged, gone| ev(id, at, "claude", Body::Commit {
            branch: branch.into(), sha: id.into(), subject: "work".into(), touches: vec![], merged, gone,
        });
        let verdict = |id: &str, item: &str, from: &str| ev(id, at, from, Body::Verdict { item: item.into(), outcome: "pass".into() });
        let rows = [
            git("git:claude/x:tip:1", "claude/x", false, false),
            git("git:claude/x:tip:2", "claude/x", false, false),
            git("git:claude/y:merged:3", "claude/y", true, false),
            git("git:claude/z:gone:4", "claude/z", false, true),
            verdict("post@0#1:XF2", "XF2", "codex"),
            verdict("post@0#1:XF3", "XF3", "codex"),
            verdict("human-pass", "XF1", "sanjith"),
            ev("note", at, "codex", msg("XF1 looks good", &["XF1"])),
            ev("first-claim", at, "claude", Body::Claim { item: "XF4".into() }),
            ev("same-lane", at, "sanjith", Body::Claim { item: "XF4".into() }),
        ].into_iter().zip(2..).map(|(e, pos)| (pos, e)).collect::<Vec<_>>();
        for (_, e) in &rows {
            g.apply(e, &d);
        }
        let c = catchup(&before, &g, &crate::work::all(&g, "sanjith"), rows.iter(), "sanjith", false).unwrap();
        assert_eq!((c.commits, c.branches, c.verdicts, c.messages), (2, 1, 1, 1));
        // Branches no item names are cards of their own, so they move too (claude/y merged: done).
        assert_eq!(c.moved.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["XF4", "claude/x", "XF2", "XF3", "claude/y"]);
    }

    #[test]
    fn digest_uses_time_order_and_includes_same_minute_and_deleted_activity() {
        let at = "2026-11-23T09:00";
        let claim = |item| Body::Claim { item: String::from(item) };
        let rows = [
            (1, ev("run", at, "sanjith", Body::RunStarted { agent: "codex".into(), hop: 1, trigger: "m".into() })),
            (7, ev("latest", "2026-11-23T12:00", "claude", claim("XF7"))),
            (2, ev("same", at, "claude", claim("XF2"))),
            (3, ev("first", "2026-11-23T09:01", "claude", claim("XF3"))),
            (4, ev("merge", "2026-11-23T09:30", "claude", Body::Commit {
                branch: "claude/x".into(), sha: "abc".into(), subject: "work".into(), touches: vec![], merged: true, gone: false,
            })),
            (6, ev("gone", "2026-11-23T10:30", "claude", Body::Commit {
                branch: "claude/x".into(), sha: "abc".into(), subject: "".into(), touches: vec![], merged: false, gone: true,
            })),
            (5, ev("middle", "2026-11-23T10:00", "claude", claim("XF5"))),
        ];
        assert_eq!(digest(rows[..3].iter(), "codex"), ["claude claimed XF2", "claude claimed XF7"]);
        assert_eq!(digest(rows.iter(), "codex"), ["(1 earlier changes not shown)", "claude claimed XF3", "claude/x was merged", "claude claimed XF5", "claude/x was deleted", "claude claimed XF7"]);
    }

    #[test]
    fn the_mark_round_trips_on_disk() {
        let dir = std::env::temp_dir().join(format!("trio-seen-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".trio")).unwrap();
        let s = Seen { at: "2026-11-23T09:30".into(), position: 42, lanes: [("XF1".to_string(), "done".to_string())].into(), sources: [("ledger".to_string(), 7)].into() };
        save(&dir, &s).unwrap();
        assert_eq!(serde_json::from_slice::<Seen>(&std::fs::read(path(&dir)).unwrap()).unwrap(), s);
        std::fs::remove_dir_all(dir).ok();
    }
}
