//! Sources of events. `Source` is the interface; `FileSource` inherits it through a blanket
//! impl, so every file-backed source gets incremental tailing for free:
//!   poll = stat (O(1)) -> read only the new bytes (O(delta)) -> parse complete lines.
//! A file that shrank was rewritten; its state resets and it is re-read once.

use crate::model::{item_ids, Body, Event};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub trait Source {
    fn name(&self) -> &str;
    /// New or replaced events since the last poll.
    fn poll(&mut self) -> io::Result<Vec<Event>>;
    /// For an append-only source: the byte offset of each event the last poll returned, in order.
    /// Offsets say exactly what arrived after a given point, whatever ids the rows reuse.
    fn offsets(&mut self) -> Vec<u64> {
        Vec::new()
    }
    /// True once after the source found its file edited in place rather than appended to. Its
    /// earlier events may no longer exist, so the room replays everything again.
    fn rewritten(&mut self) -> bool {
        false
    }
}

#[derive(Default)]
pub struct Tail {
    offset: u64,
    /// Bytes after the last newline, held until the line completes.
    partial: Vec<u8>,
    /// Modification time at the last read, and a fingerprint of every byte before `offset`,
    /// for sources that are edited in place.
    mtime: Option<std::time::SystemTime>,
    fnv: u64,
    edited: bool,
}

const FNV0: u64 = 0xcbf2_9ce4_8422_2325;

fn fnv(h: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(h, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
}

pub trait FileSource {
    fn path(&self) -> &Path;
    fn tail(&mut self) -> &mut Tail;
    /// `base` is the file offset of the first byte of `lines`.
    fn parse(&mut self, lines: &str, base: u64) -> Vec<Event>;
    /// Called when the file was truncated or replaced, before a full re-read.
    fn reset(&mut self) {}
    /// Sources that hold a pending record (markdown posts) flush it here.
    fn flush(&mut self) -> Vec<Event> {
        Vec::new()
    }
    /// True for files people edit in place (posts), not only append to. When such a file
    /// changes, the bytes already read are checked: an append is tailed as usual, an edit
    /// anywhere before the end (even one that keeps the length) is reported by `rewritten`.
    fn rewrites(&self) -> bool {
        false
    }
    /// See `Source::offsets`.
    fn take_offsets(&mut self) -> Vec<u64> {
        Vec::new()
    }
}

/// Edited files are checked up to this size; past it they are only tailed.
const REREAD_MAX: u64 = 4 << 20;

/// Whether the first `n` bytes of the file still hash to `h`.
fn same_prefix(path: &Path, n: u64, h: u64) -> bool {
    let mut buf = Vec::with_capacity(n as usize);
    File::open(path).and_then(|f| f.take(n).read_to_end(&mut buf)).is_ok_and(|got| got as u64 == n) && fnv(FNV0, &buf) == h
}

impl<T: FileSource> Source for T {
    fn name(&self) -> &str {
        self.path().file_name().and_then(|n| n.to_str()).unwrap_or("?")
    }

    fn offsets(&mut self) -> Vec<u64> {
        self.take_offsets()
    }

    fn rewritten(&mut self) -> bool {
        std::mem::take(&mut self.tail().edited)
    }

    fn poll(&mut self) -> io::Result<Vec<Event>> {
        let path = self.path().to_path_buf();
        let (len, mtime) = match std::fs::metadata(&path) {
            Ok(m) => (m.len(), m.modified().ok()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let checked = self.rewrites() && self.tail().mtime.is_some() && mtime != self.tail().mtime && len <= REREAD_MAX;
        if checked && (len < self.tail().offset || !same_prefix(&path, self.tail().offset, self.tail().fnv)) {
            // Edited in place: what was read before may be gone. Report it; read nothing more.
            self.tail().edited = true;
            self.tail().mtime = mtime;
            return Ok(Vec::new());
        }
        if len < self.tail().offset {
            *self.tail() = Tail::default();
            self.reset();
        }
        self.tail().mtime = mtime;
        if len == self.tail().offset {
            return Ok(Vec::new());
        }
        let mut f = File::open(&path)?;
        let start = self.tail().offset;
        f.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::with_capacity((len - start) as usize);
        f.take(len - start).read_to_end(&mut buf)?;
        let tail = self.tail();
        tail.fnv = fnv(if start == 0 { FNV0 } else { tail.fnv }, &buf);
        let base = start - tail.partial.len() as u64;
        let mut bytes = std::mem::take(&mut tail.partial);
        bytes.extend_from_slice(&buf);
        tail.offset = start + buf.len() as u64;
        let cut = bytes.iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
        tail.partial = bytes[cut..].to_vec();
        let text = String::from_utf8_lossy(&bytes[..cut]).into_owned();
        let mut out = self.parse(&text, base);
        out.extend(self.flush());
        Ok(out)
    }
}

// ---- room.jsonl: the ledger Trio owns ----------------------------------------------------

pub struct RoomLedger {
    path: PathBuf,
    tail: Tail,
    offsets: Vec<u64>,
}

impl RoomLedger {
    pub fn new(path: PathBuf) -> Self {
        Self { path, tail: Tail::default(), offsets: Vec::new() }
    }

    /// Start reading at byte `offset`. A line cut by a mid-line offset fails to parse and is
    /// skipped, so any offset is safe.
    pub fn at(path: PathBuf, offset: u64) -> Self {
        Self { path, tail: Tail { offset, ..Tail::default() }, offsets: Vec::new() }
    }

    /// Offset just past the last complete line read: where the next reader should start.
    pub fn offset(&self) -> u64 {
        self.tail.offset - self.tail.partial.len() as u64
    }
}

impl FileSource for RoomLedger {
    fn path(&self) -> &Path {
        &self.path
    }
    fn take_offsets(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.offsets)
    }
    fn tail(&mut self) -> &mut Tail {
        &mut self.tail
    }
    fn parse(&mut self, lines: &str, base: u64) -> Vec<Event> {
        let mut out = Vec::new();
        let mut off = base;
        for line in lines.split_inclusive('\n') {
            let t = line.trim();
            if !t.is_empty() {
                if let Ok(mut e) = serde_json::from_str::<Event>(t) {
                    if e.id.is_empty() {
                        e.id = format!("l{off}");
                    }
                    out.push(e);
                    self.offsets.push(off);
                }
            }
            off += line.len() as u64;
        }
        out
    }
}

/// Append one event as one JSON line under an exclusive lock. Readers never see half a line
/// because they only parse up to the last newline.
pub fn append(path: &Path, event: &Event) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // read(true) as well: on Windows, LockFileEx needs read or write access, and append-only
    // handles carry neither.
    let mut f = OpenOptions::new().create(true).read(true).append(true).open(path)?;
    f.lock()?;
    let mut line = serde_json::to_string(event).map_err(io::Error::other)?;
    line.push('\n');
    let r = f.write_all(line.as_bytes());
    let _ = f.unlock();
    r
}

// ---- <agent>-posts.md: the existing thread protocol, read-only ----------------------------

/// Posts start at `## YYYY-MM-DD[ HH:MM] title` outside code fences. A post's terminal
/// `Status:` line becomes a Status event; `verdict X: pass|changes` and `claim X` lines become
/// Verdict and Claim events. The last post is re-emitted (same ids) as it grows.
pub struct PostsFile {
    path: PathBuf,
    from: String,
    /// The room's human handle, so `@sanjith` in a post is a mention like any other.
    human: String,
    tail: Tail,
    fence: Option<(u8, usize)>,
    /// The post being read: (offset of its header, header text, body lines).
    open: Option<(u64, String, Vec<String>)>,
    published_status: Option<Event>,
}

impl PostsFile {
    pub fn new(path: PathBuf, from: &str, human: &str) -> Self {
        Self { path, from: from.to_string(), human: human.to_string(), tail: Tail::default(), fence: None, open: None, published_status: None }
    }

    fn close(&self, off: u64, head: &str, body: &[String]) -> Vec<Event> {
        let file = self.path.file_name().and_then(|n| n.to_str()).unwrap_or("posts");
        let id = format!("{file}@{off}");
        let (at, title) = split_header(head);
        let detail = body.join("\n").trim().to_string();
        let mut items = item_ids(title);
        let mut seen: std::collections::HashSet<_> = items.iter().cloned().collect();
        for i in item_ids(&detail) {
            if seen.insert(i.clone()) {
                items.push(i);
            }
        }
        // The title names the reviewers when it addresses anyone, including @both.
        let mut named = mentions_in(title, &self.human);
        if named.is_empty() {
            named = mentions_in(&detail, &self.human);
        }
        let mut out = vec![Event {
            id: id.clone(),
            at: at.clone(),
            from: self.from.clone(),
            body: Body::Message { summary: title.to_string(), detail: Some(detail.clone()), mentions: named, items },
        }];
        let mut fence: Option<(u8, usize)> = None;
        let mut last_text = "";
        for (n, line) in body.iter().enumerate() {
            fence = fence_step(line, fence);
            if fence.is_some() || line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
                continue;
            }
            let l = line.trim();
            if !l.is_empty() {
                last_text = l;
            }
            let eid = format!("{id}#{n}");
            for (item, body) in line_facts(l) {
                out.push(Event { id: format!("{eid}:{item}"), at: at.clone(), from: self.from.clone(), body });
            }
        }
        if let Some(s) = last_text.strip_prefix("Status:") {
            out.push(Event { id: format!("{id}#status"), at, from: self.from.clone(), body: Body::Status { text: s.trim().to_string() } });
        }
        out
    }

    fn publish(&mut self, mut rows: Vec<Event>) -> Vec<Event> {
        let status = rows.last().filter(|e| matches!(&e.body, Body::Status { .. })).cloned();
        // The previous Status line can stop being terminal as this post grows.
        if status.is_none() && let Some(mut old) = self.published_status.take() {
            old.body = Body::Status { text: String::new() };
            rows.push(old);
        }
        self.published_status = status;
        rows
    }
}

/// The facts one line states, by item: `verdict X: pass|changes ...` (X and any ids after the
/// colon) and `claim X Y`. Posts files, chat and `trio say` all read lines the same way.
pub fn line_facts(l: &str) -> Vec<(String, Body)> {
    let l = l.trim();
    let mut out = Vec::new();
    if let Some(rest) = l.strip_prefix("verdict ") {
        if let Some((target, tail)) = rest.split_once(':') {
            let outcome = tail.split_whitespace().next().unwrap_or("").trim_matches(|c: char| !c.is_alphanumeric());
            for item in std::iter::once(target.trim().to_string()).chain(item_ids(tail)) {
                out.push((item.clone(), Body::Verdict { item, outcome: outcome.to_string() }));
            }
        }
    } else if let Some(rest) = l.strip_prefix("claim ") {
        for item in item_ids(rest) {
            out.push((item.clone(), Body::Claim { item }));
        }
    }
    out
}

impl FileSource for PostsFile {
    fn path(&self) -> &Path {
        &self.path
    }
    fn rewrites(&self) -> bool {
        true
    }
    fn tail(&mut self) -> &mut Tail {
        &mut self.tail
    }
    fn reset(&mut self) {
        self.fence = None;
        self.open = None;
        self.published_status = None;
    }
    fn parse(&mut self, lines: &str, base: u64) -> Vec<Event> {
        let mut out = Vec::new();
        let mut off = base;
        for raw in lines.split_inclusive('\n') {
            let line = raw.trim_end_matches(['\n', '\r']);
            let was_fenced = self.fence.is_some();
            self.fence = fence_step(line, self.fence);
            if !was_fenced && self.fence.is_none() && is_post_head(line) {
                if let Some((o, h, b)) = self.open.take() {
                    let rows = self.close(o, &h, &b);
                    out.extend(self.publish(rows));
                    self.published_status = None;
                }
                self.open = Some((off, line[3..].to_string(), Vec::new()));
            } else if let Some((_, _, body)) = self.open.as_mut() {
                body.push(line.to_string());
            }
            off += raw.len() as u64;
        }
        out
    }
    fn flush(&mut self) -> Vec<Event> {
        // Re-emit the open post so a growing last post shows; ids are stable, so it replaces.
        let Some((o, h, b)) = &self.open else { return Vec::new() };
        let rows = self.close(*o, h, b);
        self.publish(rows)
    }
}

fn is_post_head(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("## ") else { return false };
    let b = rest.as_bytes();
    b.len() >= 10
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
}

/// `2026-11-24 02:55 Title` -> ("2026-11-24T02:55", "Title"); date-only headers keep the date.
fn split_header(head: &str) -> (String, &str) {
    let date = &head[..10];
    let rest = head[10..].trim_start();
    let b = rest.as_bytes();
    if b.len() >= 5 && b[..2].iter().all(u8::is_ascii_digit) && b[2] == b':' && b[3..5].iter().all(u8::is_ascii_digit) {
        (format!("{date}T{}", &rest[..5]), rest[5..].trim())
    } else {
        (format!("{date}T00:00"), rest)
    }
}

/// CommonMark-style fence tracking: open on ``` or ~~~ (3+), close on the same char, >= length.
pub fn fence_step(line: &str, open: Option<(u8, usize)>) -> Option<(u8, usize)> {
    let t = line.trim_start_matches(' ');
    if line.len() - t.len() > 3 {
        return open;
    }
    let b = t.as_bytes();
    let Some(&c) = b.first() else { return open };
    if c != b'`' && c != b'~' {
        return open;
    }
    let n = b.iter().take_while(|&&x| x == c).count();
    if n < 3 {
        return open;
    }
    match open {
        None => Some((c, n)),
        Some((oc, on)) if oc == c && n >= on && t[n..].trim().is_empty() => None,
        other => other,
    }
}

/// `@claude`, `@codex`, `@both` and `@<human>` (the room's `me`; empty for none) in `text`, in
/// the order they first appear, so the first name is the one addressed.
pub fn mentions_in(text: &str, human: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let human_l = human.to_ascii_lowercase();
    let mut found: Vec<(usize, String)> = [("claude", "claude"), ("codex", "codex"), ("both", "both"), (human_l.as_str(), human)]
        .iter()
        .filter(|(w, _)| !w.is_empty())
        .filter_map(|(w, name)| {
            let tag = format!("@{w}");
            // A whole handle: `@codex` but not `@codexbot`.
            lower.match_indices(&tag).find(|(i, _)| !lower[i + tag.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')).map(|(i, _)| (i, name.to_string()))
        })
        .collect();
    found.sort();
    found.dedup_by(|a, b| a.1 == b.1);
    found.into_iter().map(|(_, w)| w).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("trio-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&d);
        d
    }

    #[test]
    fn ledger_tails_only_new_complete_lines() {
        let p = tmp("ledger.jsonl");
        let mut src = RoomLedger::new(p.clone());
        let e = Event { id: String::new(), at: "t".into(), from: "sanjith".into(), body: Body::Status { text: "hi".into() } };
        append(&p, &e).unwrap();
        assert_eq!(src.poll().unwrap().len(), 1);
        assert_eq!(src.poll().unwrap().len(), 0, "nothing new");
        // A half-written line is held until its newline arrives.
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(br#"{"at":"t","from":"codex","kind":"status","text":"x"}"#).unwrap();
        assert_eq!(src.poll().unwrap().len(), 0);
        f.write_all(b"\n").unwrap();
        let got = src.poll().unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].id.starts_with('l'));
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn posts_parse_heads_fences_marks_and_status() {
        let p = tmp("codex-posts.md");
        std::fs::write(&p, "# Codex\n\n## 2026-11-23 18:30 P0a review\n\nverdict claude/hooks-p0a: changes at d44201ba\n\n- XF29 one\n```\n## 2026-11-23 19:00 not a post\n```\nclaim XF31\n\nStatus: done\n").unwrap();
        let mut src = PostsFile::new(p.clone(), "codex", "sanjith");
        let ev = src.poll().unwrap();
        let msgs: Vec<_> = ev.iter().filter(|e| matches!(e.body, Body::Message { .. })).collect();
        assert_eq!(msgs.len(), 1, "fenced header is not a post");
        assert_eq!(msgs[0].at, "2026-11-23T18:30");
        let Body::Message { summary, items, .. } = &msgs[0].body else { unreachable!() };
        assert_eq!(summary, "P0a review");
        assert!(items.contains(&"XF29".to_string()));
        assert!(ev.iter().any(|e| matches!(&e.body, Body::Verdict { item, outcome } if item == "claude/hooks-p0a" && outcome == "changes")));
        assert!(ev.iter().any(|e| matches!(&e.body, Body::Claim { item } if item == "XF31")));
        assert!(ev.iter().any(|e| matches!(&e.body, Body::Status { text } if text == "done")));
        // Appending a new post closes the old one and re-emits it with the same id.
        let first_id = msgs[0].id.clone();
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"\n## 2026-11-23 19:10 Next\n\nStatus: working\n").unwrap();
        let ev2 = src.poll().unwrap();
        assert!(ev2.iter().any(|e| e.id == first_id));
        assert!(ev2.iter().any(|e| matches!(&e.body, Body::Message { summary, .. } if summary == "Next")));
        std::fs::remove_file(&p).unwrap();
    }
}
