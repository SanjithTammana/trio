//! The server's /api/state snapshot and the pure helpers every view shares (names, lanes, time
//! labels, components). Mirrors web/src/api.ts so both clients read the room the same way.

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Deserialize)]
pub struct Snapshot {
    pub room: String,
    /// `roomid::room_id` of the server's room; checked before the snapshot is shown or used.
    #[serde(default)]
    pub room_id: String,
    pub rev: u64,
    pub paused: bool,
    pub hop: u32,
    pub budget: u32,
    pub me: String,
    pub token: String,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentState>,
    #[serde(default)]
    pub messages: Vec<Message>,
    pub graph: Graph,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentState {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub attached: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub id: String,
    pub at: String,
    pub from: String,
    pub kind: String,
    pub summary: String,
    #[serde(default)]
    pub has_detail: bool,
    #[serde(default)]
    pub items: Vec<String>,
    #[serde(default)]
    pub mentions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Step {
    pub at: String,
    pub who: String,
    pub what: String,
    pub event: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Node {
    pub id: String,
    pub kind: String,
    pub lane: String,
    pub state: String,
    #[serde(default)]
    pub last: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub touches: u32,
    #[serde(default)]
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Now {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub at: String,
    #[serde(default)]
    pub running: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Touch {
    pub part: String,
    pub lines: u32,
    pub files: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Graph {
    #[serde(default)]
    pub items: Vec<Node>,
    #[serde(default)]
    pub edges: Vec<Edge>,
    #[serde(default)]
    pub now: HashMap<String, Now>,
    #[serde(default)]
    pub touches: BTreeMap<String, Vec<Touch>>,
    #[serde(default)]
    pub merged: Vec<String>,
}

/// Display name; the room's human is always "You".
pub fn name_of(who: &str, me: &str) -> String {
    if who == me {
        return "You".into();
    }
    match who {
        "claude" => "Claude".into(),
        "codex" => "Codex".into(),
        "trio" => "Trio".into(),
        "both" => "Both".into(),
        _ => who.into(),
    }
}

/// Lane order for the chat gutter, the island and the time strip: you, Claude, Codex.
pub fn lanes(me: &str) -> [String; 3] {
    [me.to_string(), "claude".into(), "codex".into()]
}

pub fn state_label(state: &str) -> &str {
    match state {
        "open" => "open",
        "claimed" => "claimed",
        "review" => "in review",
        "changes" => "changes requested",
        "pass" => "passed",
        "blocked" => "blocked",
        "branch" => "branch",
        "merged" => "merged",
        "component" => "component",
        other => other,
    }
}

/// Local `YYYY-MM-DD` of today, on the same clock the ledger writes.
pub fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// `HH:MM` today, `MM-DD HH:MM` otherwise.
pub fn time_label(at: &str, today: &str) -> String {
    let (d, t) = at.split_once('T').unwrap_or((at, ""));
    let hm: String = t.chars().take(5).collect();
    if d == today { hm } else { format!("{} {hm}", d.get(5..).unwrap_or(d)) }
}

/// Milliseconds for ordering and placing timestamps (the ledger's local wall clock, no zone).
pub fn ms(at: &str) -> i64 {
    let s = at.trim_end_matches('Z');
    ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"]
        .iter()
        .find_map(|f| chrono::NaiveDateTime::parse_from_str(s, f).ok())
        .map(|t| t.and_utc().timestamp_millis())
        .unwrap_or(0)
}

/// The inverse of `ms`, as a ledger-style timestamp for `time_label`.
pub fn iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.naive_utc().format("%Y-%m-%dT%H:%M").to_string()).unwrap_or_default()
}

#[derive(Debug, Clone)]
pub struct BranchShare {
    pub branch: String,
    pub lines: u32,
    pub files: u32,
}

#[derive(Debug, Clone)]
pub struct Component {
    pub part: String,
    pub lines: u32,
    pub files: u32,
    pub branches: Vec<BranchShare>,
    pub hot: bool,
}

/// Components across the given unmerged branches, in first-seen order, with who touches each.
/// A component two or more branches change is hot (whichever merges second may conflict).
pub fn components(touches: &BTreeMap<String, Vec<Touch>>, branches: &[&str]) -> Vec<Component> {
    let mut out: Vec<Component> = Vec::new();
    let mut at: HashMap<&str, usize> = HashMap::new();
    for b in branches {
        for t in touches.get(*b).map(Vec::as_slice).unwrap_or(&[]) {
            let i = *at.entry(t.part.as_str()).or_insert_with(|| {
                out.push(Component { part: t.part.clone(), lines: 0, files: 0, branches: Vec::new(), hot: false });
                out.len() - 1
            });
            let c = &mut out[i];
            c.lines += t.lines;
            c.files += t.files;
            c.branches.push(BranchShare { branch: (*b).to_string(), lines: t.lines, files: t.files });
            c.hot = c.branches.len() >= 2;
        }
    }
    out
}

/// Long input becomes summary + detail: the first sentence (20..=138 chars, on the first line)
/// up front, else the first 137 chars and an ellipsis; the whole text is the detail.
pub fn split_for_send(text: &str) -> (String, Option<String>) {
    let t = text.trim();
    let chars: Vec<char> = t.chars().collect();
    if chars.len() <= 140 {
        return (t.to_string(), None);
    }
    let first_line_end = chars.iter().position(|&c| c == '\n' || c == '\r').unwrap_or(chars.len());
    let head = (20..=138.min(first_line_end.saturating_sub(1)))
        .find(|&i| matches!(chars[i], '.' | '!' | '?') && chars.get(i + 1).is_some_and(|c| c.is_whitespace()))
        .map(|i| chars[..=i].iter().collect::<String>())
        .unwrap_or_else(|| format!("{}…", chars[..137].iter().collect::<String>().trim_end()));
    (head, Some(t.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_splits_at_the_first_sentence() {
        let long = format!("This is the opening sentence of a note. {}", "more words ".repeat(20));
        let (head, detail) = split_for_send(&long);
        assert_eq!(head, "This is the opening sentence of a note.");
        assert_eq!(detail.as_deref(), Some(long.trim()));
        let (short, none) = split_for_send("  hi  ");
        assert_eq!((short.as_str(), none), ("hi", None));
        let run_on = "x".repeat(200);
        assert_eq!(split_for_send(&run_on).0.chars().count(), 138);
    }

    #[test]
    fn components_mark_hot_spots_in_first_seen_order() {
        let mut t = BTreeMap::new();
        t.insert("a".to_string(), vec![Touch { part: "src".into(), lines: 5, files: 1 }, Touch { part: "web".into(), lines: 2, files: 1 }]);
        t.insert("b".to_string(), vec![Touch { part: "src".into(), lines: 3, files: 2 }]);
        let c = components(&t, &["a", "b"]);
        assert_eq!(c.iter().map(|c| c.part.as_str()).collect::<Vec<_>>(), ["src", "web"]);
        assert!(c[0].hot && !c[1].hot);
        assert_eq!((c[0].lines, c[0].files), (8, 3));
    }

    #[test]
    fn labels_drop_the_date_today() {
        assert_eq!(time_label("2026-11-23T22:12:05", "2026-11-23"), "22:12");
        assert_eq!(time_label("2026-11-22T09:01", "2026-11-23"), "11-22 09:01");
        assert_eq!(iso(ms("2026-11-23T22:12")), "2026-11-23T22:12");
    }
}
