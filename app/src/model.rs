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
    /// What moved since the human was last here (server `seen.rs`); absent when nothing did.
    #[serde(default)]
    pub catchup: Option<Catchup>,
    /// Each agent's availability (server `usage.rs`).
    #[serde(default)]
    pub usage: BTreeMap<String, Usage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsageWindow {
    pub label: String,
    pub used: f64,
    #[serde(default)]
    pub resets: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    /// ok | low | out
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub windows: Vec<UsageWindow>,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub observed: Option<String>,
    #[serde(default)]
    pub spent_today: f64,
    #[serde(default)]
    pub budget: Option<f64>,
    #[serde(default)]
    pub held: usize,
}

/// `2026-11-24T15:00` -> `15:00`.
pub fn hhmm(at: &str) -> &str {
    at.split_once('T').map_or(at, |(_, t)| t)
}

impl Usage {
    pub fn out(&self) -> bool {
        self.state == "out"
    }
    /// "out until 15:00 (5h window used up)", as the server's status lines say it.
    pub fn why(&self) -> String {
        let until = self.until.as_deref().map(|u| format!(" until {}", hhmm(u))).unwrap_or_default();
        let reason = self.reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default();
        format!("{}{until}{reason}", self.state)
    }
    /// The fullest window, 0 to 100 (100 when out).
    pub fn fullest(&self) -> f32 {
        if self.out() {
            return 100.0;
        }
        self.windows.iter().map(|w| w.used as f32).fold(0.0, f32::max)
    }
    /// One line for the open island: state, windows with resets, spend, held wakes.
    pub fn line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.state != "ok" {
            parts.push(self.why());
        }
        for w in &self.windows {
            parts.push(format!("{} {:.0}%{}", w.label, w.used, w.resets.as_deref().map(|r| format!(" resets {}", hhmm(r))).unwrap_or_default()));
        }
        if self.windows.is_empty() {
            parts.push("no limit reading yet".into());
        }
        if self.spent_today > 0.0 || self.budget.is_some() {
            parts.push(format!("${:.2} today{}", self.spent_today, self.budget.map(|b| format!(" of ${b:.2}")).unwrap_or_default()));
        }
        if self.held > 0 {
            parts.push(format!("{} wake{} held", self.held, if self.held == 1 { "" } else { "s" }));
        }
        parts.join(" \u{b7} ")
    }
    /// Where the reading came from and how old it is, for hover text.
    pub fn source_line(&self, today: &str) -> String {
        if self.source.is_empty() {
            return "No window reading yet.".into();
        }
        format!("From the {}{}.", self.source, self.observed.as_deref().map(|o| format!(", read {}", time_label(o, today))).unwrap_or_default())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Catchup {
    pub since: String,
    #[serde(default)]
    pub moved: Vec<Moved>,
    #[serde(default)]
    pub more: usize,
    #[serde(default)]
    pub waiting_on_you: usize,
    #[serde(default)]
    pub commits: usize,
    #[serde(default)]
    pub branches: usize,
    #[serde(default)]
    pub verdicts: usize,
    #[serde(default)]
    pub messages: usize,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Moved {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub from: Option<String>,
    pub to: String,
    #[serde(default)]
    pub yours: bool,
}

impl Catchup {
    /// "14 items moved · 4 commits on 4 branches · …", the same facts as the web strip.
    pub fn facts(&self) -> String {
        let n = |k: usize, one: &str, many: &str| format!("{k} {}", if k == 1 { one } else { many });
        let moved = self.moved.len() + self.more;
        let mut f: Vec<String> = Vec::new();
        if moved > 0 {
            f.push(n(moved, "item moved", "items moved"));
        }
        if self.waiting_on_you > 0 {
            f.push(format!("{} wait{} on you", self.waiting_on_you, if self.waiting_on_you == 1 { "s" } else { "" }));
        }
        if self.commits > 0 {
            f.push(format!("{} on {}", n(self.commits, "commit", "commits"), n(self.branches, "branch", "branches")));
        }
        if self.verdicts > 0 {
            f.push(n(self.verdicts, "verdict", "verdicts"));
        }
        if self.messages > 0 {
            f.push(n(self.messages, "message", "messages"));
        }
        f.join(" \u{b7} ")
    }
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
    /// Items only: the Work lens stage, who it waits on, and its one-paragraph story.
    #[serde(default)]
    pub work: Option<Work>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Work {
    pub lane: String,
    #[serde(default)]
    pub waiting_on: Option<String>,
    #[serde(default)]
    pub story: String,
    /// When the item or a branch implementing it last moved (older servers: empty).
    #[serde(default)]
    pub last: String,
}

/// The Work lens stages in order, with the caption explain mode shows under each (web: WORK_LANES).
pub const WORK_LANES: [(&str, &str, &str); 5] = [
    ("asked", "Asked", "Named in chat, nobody has picked it up yet."),
    ("progress", "In progress", "Someone claimed it, or a branch implements it."),
    ("review", "In review", "A review was asked for and no verdict has come back since."),
    ("changes", "Changes", "The last verdict asked for changes: the work is back with its author."),
    ("done", "Done", "Passed review, or every branch that implements it is merged."),
];

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
