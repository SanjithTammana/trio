//! "Since you left": one seen mark per room (when the human was last here, and each item's Work
//! lane at that moment) and the catch-up it yields on return. Work done outside Trio shows up
//! because it reaches the ledger anyway: posts files, branch commits and attached-session hooks
//! are all sources. Agents get the same idea as a short digest in their wake prompt.

use crate::graph::Graph;
use crate::model::{clip, Body, Event};
use crate::work::{Work, LANES};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// A visit shorter than this does not move the mark (a glance is not catching up).
pub const VISIT_SECS: u64 = 60;
/// Moves listed by name in the catch-up; the rest are counted.
const MOVES_SHOWN: usize = 8;
/// Lines in an agent's "since your last run" digest.
const DIGEST_LINES: usize = 5;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Seen {
    /// `YYYY-MM-DDTHH:MM`, the ledger's clock.
    pub at: String,
    /// Work lane of every item at the mark.
    pub lanes: BTreeMap<String, String>,
}

pub fn path(root: &Path) -> PathBuf {
    root.join(".trio").join("seen.json")
}

pub fn load(root: &Path) -> Option<Seen> {
    serde_json::from_slice(&std::fs::read(path(root)).ok()?).ok()
}

/// Write through a temporary file, so a crash never leaves half a mark.
pub fn save(root: &Path, s: &Seen) -> std::io::Result<()> {
    let p = path(root);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(s).map_err(std::io::Error::other)?)?;
    std::fs::rename(tmp, p)
}

pub fn mark(at: &str, work: &HashMap<String, Work>) -> Seen {
    Seen { at: at.to_string(), lanes: work.iter().map(|(id, w)| (id.clone(), w.lane.to_string())).collect() }
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
}

/// What changed since `seen`, by others. None when nothing did. O(items + events).
pub fn catchup(seen: &Seen, g: &Graph, work: &HashMap<String, Work>, events: &[Event], me: &str) -> Option<Catchup> {
    let rank = |l: &str| LANES.iter().position(|x| *x == l).unwrap_or(0);
    let mut moved: Vec<Moved> = work
        .iter()
        .filter_map(|(id, w)| {
            let it = g.items.get(id)?;
            let before = seen.lanes.get(id);
            let changed = match before {
                Some(b) => b != w.lane,
                None => it.first.as_str() > seen.at.as_str(),
            };
            changed.then(|| Moved { id: id.clone(), title: it.title.clone(), from: before.cloned(), to: w.lane.to_string(), yours: w.waiting_on.as_deref() == Some(me) })
        })
        .collect();
    // Waiting on you first, then by board order, then id: a stable list between snapshots.
    moved.sort_by(|a, b| b.yours.cmp(&a.yours).then(rank(&a.to).cmp(&rank(&b.to))).then(a.id.cmp(&b.id)));
    let after: Vec<&Event> = events.iter().filter(|e| e.at.as_str() > seen.at.as_str() && e.from != me).collect();
    let branches: BTreeSet<&str> = after.iter().filter_map(|e| if let Body::Commit { branch, .. } = &e.body { Some(branch.as_str()) } else { None }).collect();
    let count = |f: fn(&Body) -> bool| after.iter().filter(|e| f(&e.body)).count();
    let c = Catchup {
        since: seen.at.clone(),
        more: moved.len().saturating_sub(MOVES_SHOWN),
        waiting_on_you: work.values().filter(|w| w.waiting_on.as_deref() == Some(me)).count(),
        commits: count(|b| matches!(b, Body::Commit { gone: false, .. })),
        branches: branches.len(),
        verdicts: count(|b| matches!(b, Body::Verdict { .. })),
        messages: count(|b| matches!(b, Body::Message { .. })),
        moved: moved.into_iter().take(MOVES_SHOWN).collect(),
    };
    (!c.moved.is_empty() || c.commits + c.verdicts + c.messages > 0).then_some(c)
}

/// For a woken agent: what others did since its last run, newest last, at most five lines.
/// The latest messages already reach it, so this covers commits, verdicts and claims.
pub fn digest(events: &[Event], agent: &str) -> Vec<String> {
    let since = events.iter().rev().find_map(|e| match &e.body {
        Body::RunStarted { agent: a, .. } if a == agent => Some(e.at.as_str()),
        _ => None,
    });
    let Some(since) = since else { return Vec::new() };
    let lines: Vec<String> = events
        .iter()
        .filter(|e| e.at.as_str() > since && e.from != agent)
        .filter_map(|e| match &e.body {
            Body::Commit { branch, gone: false, merged: true, .. } if e.id.contains(":merged:") => Some(format!("{branch} was merged")),
            Body::Commit { branch, subject, gone: false, .. } => Some(format!("{} committed on {branch}: {}", e.from, clip(subject, 80))),
            Body::Verdict { item, outcome } => Some(format!("{} gave {item} a verdict: {outcome}", e.from)),
            Body::Claim { item } => Some(format!("{} claimed {item}", e.from)),
            _ => None,
        })
        .collect();
    let skip = lines.len().saturating_sub(DIGEST_LINES);
    let mut out: Vec<String> = Vec::new();
    if skip > 0 {
        out.push(format!("({skip} earlier changes not shown)"));
    }
    out.extend(lines.into_iter().skip(skip));
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
        let mut events = vec![ev("m1", "2026-11-23T09:00", "claude", msg("XF1 needs doing", &["XF1"]))];
        for e in &events {
            g.apply(e, &d);
        }
        let seen = mark("2026-11-23T09:30", &crate::work::all(&g, "sanjith"));
        assert_eq!(catchup(&seen, &g, &crate::work::all(&g, "sanjith"), &events, "sanjith"), None, "nothing changed");
        // While away: Codex verdicts XF1 and a new item appears.
        let later = [
            ev("v1", "2026-11-23T10:00", "codex", Body::Verdict { item: "XF1".into(), outcome: "pass".into() }),
            ev("m2", "2026-11-23T10:05", "codex", msg("XF2 found a bug", &["XF2"])),
        ];
        for e in later {
            g.apply(&e, &d);
            events.push(e);
        }
        let c = catchup(&seen, &g, &crate::work::all(&g, "sanjith"), &events, "sanjith").expect("changes");
        assert_eq!(c.since, "2026-11-23T09:30");
        let moves: Vec<(&str, Option<&str>, &str)> = c.moved.iter().map(|m| (m.id.as_str(), m.from.as_deref(), m.to.as_str())).collect();
        assert_eq!(moves, vec![("XF2", None, "asked"), ("XF1", Some("asked"), "done")]);
        assert_eq!((c.verdicts, c.messages), (1, 1));
        // Your own actions are not news to you.
        let mine = [ev("m3", "2026-11-23T10:10", "sanjith", msg("noted", &[]))];
        events.extend(mine);
        assert_eq!(catchup(&seen, &g, &crate::work::all(&g, "sanjith"), &events, "sanjith").expect("still").messages, 1);
    }

    #[test]
    fn an_agent_digest_covers_what_others_did_since_its_last_run() {
        let events = vec![
            ev("r1", "2026-11-23T09:00", "sanjith", Body::RunStarted { agent: "codex".into(), hop: 1, trigger: "m0".into() }),
            ev("c1", "2026-11-23T09:30", "claude", Body::Commit { branch: "claude/x".into(), sha: "abc12345".into(), subject: "XF1 fix".into(), touches: vec![], merged: false, gone: false }),
            ev("c2", "2026-11-23T09:40", "codex", Body::Claim { item: "XF9".into() }),
        ];
        assert_eq!(digest(&events, "codex"), vec!["claude committed on claude/x: XF1 fix".to_string()]);
        assert!(digest(&events, "claude").is_empty(), "never ran: the recent messages are enough");
    }

    #[test]
    fn the_mark_round_trips_on_disk() {
        let dir = std::env::temp_dir().join(format!("trio-seen-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".trio")).unwrap();
        let s = Seen { at: "2026-11-23T09:30".into(), lanes: [("XF1".to_string(), "done".to_string())].into() };
        save(&dir, &s).unwrap();
        assert_eq!(load(&dir), Some(s));
        std::fs::remove_dir_all(dir).ok();
    }
}
