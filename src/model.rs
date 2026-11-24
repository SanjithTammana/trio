//! Base interfaces and the event vocabulary.
//!
//! Trait hierarchy:
//!   Entity  <- Timed, Summarized  <- graph::Node
//!   Entity  <- agent::Participant <- agent::Agent
//! Closed data is an enum (`Body`); open behaviour is a trait.

use serde::{Deserialize, Serialize};

/// Hard cap on a bubble summary, in chars. The chat shows summaries; detail opens on tap.
pub const SUMMARY_MAX: usize = 140;

pub trait Entity {
    fn id(&self) -> &str;
    fn kind(&self) -> &'static str;
}

pub trait Timed: Entity {
    fn at(&self) -> &str;
}

/// Visual-first contract: everything shown in the UI has a short summary and optional detail.
pub trait Summarized: Entity {
    fn raw_summary(&self) -> Option<&str> {
        None
    }
    fn detail(&self) -> Option<&str> {
        None
    }
    /// Inherited default: the explicit summary, else the first sentence of the detail, clipped.
    fn summary(&self) -> String {
        match self.raw_summary().filter(|s| !s.trim().is_empty()) {
            Some(s) => clip(s, SUMMARY_MAX),
            None => self.detail().map(first_sentence).unwrap_or_default(),
        }
    }
}

/// One line of `room.jsonl`, or one record derived from an imported file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Stable identity. Ledger lines get `l<offset>`; imported posts get `<file>@<offset>`,
    /// so a re-read of the same post replaces it instead of duplicating it.
    #[serde(default)]
    pub id: String,
    pub at: String,
    pub from: String,
    #[serde(flatten)]
    pub body: Body,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Body {
    Message {
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mentions: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        items: Vec<String>,
    },
    RunStarted { agent: String, hop: u32, trigger: String },
    RunFinished {
        agent: String,
        exit: i32,
        secs: u64,
        /// Dollars the run cost (Claude reports it; Codex is priced from its tokens).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost: Option<f64>,
    },
    Verdict { item: String, outcome: String },
    Claim { item: String },
    Status { text: String },
    Commit {
        branch: String,
        sha: String,
        subject: String,
        /// Parts of the codebase the branch changes since its merge-base, largest first.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        touches: Vec<Touch>,
        /// The tip is already in the base branch.
        #[serde(default)]
        merged: bool,
        /// The branch no longer exists; the graph drops it.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        gone: bool,
    },
}

/// One component a branch changes: a path prefix (`src/hook`, `web/src`, `(root)`) and the
/// lines added plus deleted under it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Touch {
    pub part: String,
    pub lines: u32,
    pub files: u32,
}

/// Directory levels kept for a component. Two lumped a monorepo's `projects/dev/*` into one
/// always-hot node; three separates projects while `src/hook`-style layouts stay as they are.
const COMPONENT_DEPTH: usize = 3;

/// The component a changed path belongs to: its directory, cut to `COMPONENT_DEPTH` levels, or
/// `(root)` for top-level files. Rename notation from `git diff --numstat` (`a/{x => y}/z`,
/// `x => y`) resolves to the new path.
pub fn component_of(path: &str) -> String {
    let path = match (path.find('{'), path.find('}')) {
        (Some(o), Some(c)) if o < c => {
            let new = path[o + 1..c].rsplit(" => ").next().unwrap_or("");
            format!("{}{}{}", &path[..o], new, &path[c + 1..]).replace("//", "/")
        }
        _ => path.rsplit(" => ").next().unwrap_or(path).to_string(),
    };
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let dirs = &segs[..segs.len().saturating_sub(1)];
    if dirs.is_empty() {
        return "(root)".into();
    }
    dirs[..dirs.len().min(COMPONENT_DEPTH)].join("/")
}

impl Entity for Event {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &'static str {
        match self.body {
            Body::Message { .. } => "message",
            Body::RunStarted { .. } => "run_started",
            Body::RunFinished { .. } => "run_finished",
            Body::Verdict { .. } => "verdict",
            Body::Claim { .. } => "claim",
            Body::Status { .. } => "status",
            Body::Commit { .. } => "commit",
        }
    }
}

impl Timed for Event {
    fn at(&self) -> &str {
        &self.at
    }
}

impl Summarized for Event {
    fn raw_summary(&self) -> Option<&str> {
        match &self.body {
            Body::Message { summary, .. } => Some(summary),
            Body::Status { text } => Some(text),
            Body::Commit { subject, .. } => Some(subject),
            _ => None,
        }
    }
    fn detail(&self) -> Option<&str> {
        match &self.body {
            Body::Message { detail, .. } => detail.as_deref(),
            _ => None,
        }
    }
}

/// Clip to `max` chars on a char boundary, adding an ellipsis when cut.
pub fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// First sentence of markdown-ish text: skips blank lines, headings markers and list bullets.
pub fn first_sentence(text: &str) -> String {
    let line = text
        .lines()
        .map(|l| l.trim().trim_start_matches(['#', '-', '*', '>', ' ']))
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let end = line
        .char_indices()
        .find(|&(i, c)| matches!(c, '.' | '!' | '?') && line[i + c.len_utf8()..].starts_with(' '))
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(line.len());
    clip(&line[..end], SUMMARY_MAX)
}

/// An agent's final answer -> (bubble summary, detail). Line 1 is the summary when it fits
/// (markdown emphasis stripped), else its first sentence clipped; the full text is the detail
/// whenever it says more than the summary. None for an empty answer.
pub fn split_reply(text: &str) -> Option<(String, Option<String>)> {
    let t = text.trim();
    let line = t.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line.trim_start_matches(['#', '>', ' ']).trim().trim_matches('*').trim();
    let summary = if line.chars().count() <= SUMMARY_MAX { line.to_string() } else { first_sentence(line) };
    if summary.is_empty() {
        return None;
    }
    let rest = t.splitn(2, '\n').nth(1).map(str::trim).unwrap_or("");
    let detail = (!rest.is_empty() || summary != line).then(|| t.to_string());
    Some((summary, detail))
}

/// What a message says an item *is*, for a readable title instead of a bare id. Recognizes
/// `XF32 — Limit the clean claim`, `P0a: hook trigger matrix`, `**XF22.** Each CLI run loses…`
/// and `P0b (a separately approved staged redeploy)`. None when the text only mentions the id.
/// O(n) per id: one scan for its occurrences.
pub fn definition(text: &str, id: &str) -> Option<String> {
    if id.is_empty() {
        return None;
    }
    let mut from = 0;
    while let Some(k) = text[from..].find(id) {
        let (s, e) = (from + k, from + k + id.len());
        from = e;
        let before = text[..s].chars().next_back();
        let rest = &text[e..];
        let mut after = rest.chars();
        let next = after.next();
        // A whole id: not inside a word, and not a prefix of a longer id (P1 in P1.2, XF2 in XF29).
        let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
        // Only free-standing ids define: not inside a URL or path (`?ticket=P0a:x`), not after
        // other text glued to it.
        if before.is_some_and(|c| !(c.is_whitespace() || "*([>\"'".contains(c))) || next.is_some_and(word) || (next == Some('.') && after.next().is_some_and(|c| c.is_ascii_digit())) {
            continue;
        }
        // Not inside an inline code span: an odd number of backticks earlier on the line.
        let line_start = text[..s].rfind('\n').map_or(0, |i| i + 1);
        if text[line_start..s].matches('`').count() % 2 == 1 {
            continue;
        }
        let bold = text[..s].ends_with("**");
        if let Some(d) = defined_by(rest, bold) {
            return Some(d);
        }
    }
    None
}

fn defined_by(rest: &str, bold: bool) -> Option<String> {
    let r = rest.trim_start_matches('*').trim_start();
    for sep in ["\u{2014}", "\u{2013}", ":", "- "] {
        if let Some(x) = r.strip_prefix(sep) {
            // `P0a:manual` is a token, not a definition: a colon must be followed by a space.
            if sep == ":" && !x.starts_with(char::is_whitespace) {
                return None;
            }
            return title_text(x);
        }
    }
    if let Some(x) = r.strip_prefix('(') {
        return x.split(')').next().and_then(title_text);
    }
    if bold {
        // `**XF22.** Each CLI run …`: the sentence after the bold id, past an optional `(file)`.
        let x = rest.strip_prefix('.').unwrap_or(rest).strip_prefix("**")?.trim_start();
        let x = match x.strip_prefix('(') {
            Some(p) => p.split_once(')').map_or("", |(_, tail)| tail),
            None => x,
        };
        return title_text(x.trim_start().trim_start_matches(':'));
    }
    None
}

/// The leading phrase of `x`, cleaned of markdown, if it reads as words (not code or hashes).
fn title_text(x: &str) -> Option<String> {
    // A leading bullet or dash ("XF28: - All file I/O …") is punctuation, not title.
    let x = x.trim_start().trim_start_matches(['-', '*', '\u{2013}', '\u{2014}', ' ']);
    let end = ["**", "\n", ". ", "; ", " \u{2014} "].iter().filter_map(|m| x.find(m)).min().unwrap_or(x.len());
    let t: String = x[..end].chars().filter(|c| *c != '`' && *c != '*').collect();
    let t = t.trim().trim_end_matches(['.', ':', ',']).trim();
    let words = t.split_whitespace().filter(|w| w.chars().all(|c| c.is_alphabetic() || "'-,".contains(c)) && w.chars().any(char::is_alphabetic)).count();
    // A list of ids (`item 16, item 17, item 18`) is not a description.
    if words < 3 || t.len() < 10 || item_ids(t).len() >= 2 {
        return None;
    }
    let mut out: String = t.chars().take(80).collect();
    if t.chars().count() > 80 {
        if let Some(cut) = out.rfind(' ') {
            out.truncate(cut);
        }
        out.push('…');
    }
    // Sentence case, unless the first word is a name from code (artifact_dir.mjs, hasOwn).
    let first = out.split_whitespace().next().unwrap_or("");
    if !first.chars().all(char::is_alphabetic) || first.chars().skip(1).any(char::is_uppercase) {
        return Some(out);
    }
    let mut c = out.chars();
    Some(c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default())
}

/// Work-item ids in free text: `P0a`, `P1`, `XF29`, `H3`, `A3`, `L5`, `item 16`, `#tag`.
/// One pass over the bytes, O(n).
pub fn item_ids(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: String| {
        if !out.contains(&s) {
            out.push(s)
        }
    };
    let words: Vec<&str> = text
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '#' || c == '-' || c == '_'))
        .filter(|w| !w.is_empty())
        .collect();
    for (i, w) in words.iter().enumerate() {
        if let Some(tag) = w.strip_prefix('#') {
            if tag.len() >= 2 && tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                push(format!("#{}", tag.to_ascii_lowercase()));
            }
            continue;
        }
        if w.eq_ignore_ascii_case("item") {
            if let Some(n) = words.get(i + 1).filter(|n| n.chars().all(|c| c.is_ascii_digit())) {
                push(format!("item {n}"));
            }
            continue;
        }
        if is_code_id(w) {
            push(w.to_string());
        }
    }
    out
}

/// `XF29`, `P0a`, `P1`, `H3`: 1-2 uppercase letters, digits, optional one lowercase suffix.
fn is_code_id(w: &str) -> bool {
    let b = w.as_bytes();
    let letters = b.iter().take_while(|c| c.is_ascii_uppercase()).count();
    if !(1..=2).contains(&letters) {
        return false;
    }
    let digits = b[letters..].iter().take_while(|c| c.is_ascii_digit()).count();
    if !(1..=3).contains(&digits) {
        return false;
    }
    let rest = &b[letters + digits..];
    // Reject things like "A1" style noise when two letters look like a word prefix ("UI2" is fine).
    let prefix = &w[..letters];
    let allowed = ["XF", "P", "H", "A", "L", "S", "B", "M"];
    allowed.contains(&prefix) && (rest.is_empty() || (rest.len() == 1 && rest[0].is_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_give_items_readable_titles() {
        let d = |t: &str, id: &str| definition(t, id);
        assert_eq!(d("- **XF32 \u{2014} Limit the clean claim to the sources checked** (`hook-drift.mjs:4`)", "XF32").as_deref(), Some("Limit the clean claim to the sources checked"));
        assert_eq!(d("P0 splits into P0a (an agreed per-surface trigger matrix plus a read-only drift receipt) and P0b (a separately approved staged redeploy with rollback).", "P0b").as_deref(), Some("A separately approved staged redeploy with rollback"));
        assert_eq!(d("**XF22.** Each CLI run loses its failure log when the hook exits early. More text.", "XF22").as_deref(), Some("Each CLI run loses its failure log when the hook exits early"));
        // Mentions, hashes, code and longer ids are not definitions.
        assert_eq!(d("P0a (d44201ba) is merged", "P0a"), None);
        assert_eq!(d("**A3.** `_meta/tools/artifact_dir.mjs <project|os> <task>`:", "A3"), None);
        assert_eq!(d("**P1.2** capability routing for metered models", "P1"), None);
        assert_eq!(d("XF29 is still waiting on Codex", "XF29"), None);
        assert_eq!(d("XF28: - All file I/O goes through io, so a test can stub it", "XF28").as_deref(), Some("All file I/O goes through io, so a test can stub it"));
        assert_eq!(d("XF2 vs XF29: two separate things here", "XF2").as_deref(), None);
        // Quoted examples, URL fragments, id lists and an empty id are not definitions.
        assert_eq!(d("Use `P0a: this is an example` in the docs", "P0a"), None);
        assert_eq!(d("See https://example.test/?ticket=P0a:manual for the full report", "P0a"), None);
        assert_eq!(d("P0a: item 16, item 17, item 18", "P0a"), None);
        assert_eq!(d("anything at all", ""), None);
    }

    #[test]
    fn reply_splits_into_bubble_and_detail() {
        assert_eq!(split_reply("  \n"), None);
        assert_eq!(split_reply("ready"), Some(("ready".into(), None)));
        let (s, d) = split_reply("**P0a passes.**\n\n- XF29 fixed\n- XF30 fixed").unwrap();
        assert_eq!(s, "P0a passes.");
        assert!(d.unwrap().contains("XF30"));
        let long = format!("{} Then more.", "word ".repeat(40));
        let (s, d) = split_reply(&long).unwrap();
        assert!(s.chars().count() <= SUMMARY_MAX && d.is_some());
    }

    #[test]
    fn paths_group_into_components() {
        assert_eq!(component_of("Cargo.toml"), "(root)");
        assert_eq!(component_of("src/main.rs"), "src");
        assert_eq!(component_of("src/hook/mod.rs"), "src/hook");
        assert_eq!(component_of("web/src/App.tsx"), "web/src");
        assert_eq!(component_of("projects/dev/trio/V2.md"), "projects/dev/trio");
        assert_eq!(component_of("projects/dev/trio/web/x.ts"), "projects/dev/trio");
        assert_eq!(component_of("src/{old => hook}/claude.rs"), "src/hook");
        assert_eq!(component_of("src/{ => hook}/claude.rs"), "src/hook");
        assert_eq!(component_of("old.rs => src/new.rs"), "src");
    }

    #[test]
    fn ids_found_once_in_order() {
        let t = "verdict claude/hooks-p0a: changes; XF29, XF30 and P0a. See item 16 and #desk. XF29 again. UTC9 no.";
        assert_eq!(item_ids(t), vec!["XF29", "XF30", "P0a", "item 16", "#desk"]);
    }

    #[test]
    fn summary_falls_back_to_first_sentence() {
        let e = Event {
            id: "x".into(),
            at: "t".into(),
            from: "codex".into(),
            body: Body::Message { summary: "".into(), detail: Some("## Head\n\nFirst one. Second.".into()), mentions: vec![], items: vec![] },
        };
        assert_eq!(e.summary(), "Head");
        assert_eq!(first_sentence("- First one. Second."), "First one.");
        assert_eq!(clip(&"a".repeat(200), 10).chars().count(), 10);
    }
}
