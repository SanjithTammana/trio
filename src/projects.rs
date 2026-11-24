//! Every room you have, at a glance: what waits on you, what is in flight, what moved since you
//! last looked. Read on demand (the project switcher), never per snapshot.
//!
//! Each room is replayed read-only from its files: no git or GitHub watchers start and nothing
//! is written, so listing rooms never runs `git`, never calls GitHub, and never moves a mark.

use crate::room::Room;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Summary {
    pub name: String,
    pub path: String,
    /// The room this server serves.
    pub current: bool,
    /// A server is up for it now.
    pub running: bool,
    pub waiting_on_you: usize,
    pub in_flight: usize,
    /// Cards that moved, plus messages, since its seen mark.
    pub new: usize,
    /// Its newest activity (`YYYY-MM-DDTHH:MM`), for sorting and "2h ago".
    pub last: String,
}

/// One room's summary from its files. None when it cannot be read.
pub fn summary(root: &Path, current: &Path) -> Option<Summary> {
    let mut room = Room::open_read(root).ok()?;
    room.tick();
    let me = room.me();
    let work = crate::work::all(&room.graph, &me);
    let waiting_on_you = work.values().filter(|w| w.waiting_on.as_deref() == Some(me.as_str())).count();
    let in_flight = work.values().filter(|w| matches!(w.lane, "progress" | "review" | "changes")).count();
    let new = room.catchup(&work).map_or(0, |c| c.moved.len() + c.more + c.messages);
    let last = work.values().map(|w| w.last.as_str()).max().unwrap_or("").to_string();
    Some(Summary {
        name: room.name(),
        path: root.display().to_string(),
        current: same(root, current),
        running: crate::hook::poke(root).is_some(),
        waiting_on_you,
        in_flight,
        new,
        last,
    })
}

/// Every registered room (`~/.trio/rooms.txt`), this one first, then the ones waiting on you,
/// then by newest activity.
pub fn all(current: &Path) -> Vec<Summary> {
    let mut rooms: Vec<PathBuf> = crate::hook::Store::from_env().map(|s| s.known_rooms()).unwrap_or_default();
    if !rooms.iter().any(|r| same(r, current)) {
        rooms.insert(0, current.to_path_buf());
    }
    let mut out: Vec<Summary> = rooms.iter().filter_map(|r| summary(r, current)).collect();
    out.sort_by(|a, b| b.current.cmp(&a.current).then((b.waiting_on_you > 0).cmp(&(a.waiting_on_you > 0))).then(b.last.cmp(&a.last)).then(a.name.cmp(&b.name)));
    out
}

/// A registered room by path; anything else is refused, so a page can only open your own rooms.
pub fn known(path: &str) -> Option<PathBuf> {
    let p = PathBuf::from(path);
    crate::hook::Store::from_env().ok()?.known_rooms().into_iter().find(|r| same(r, &p))
}

/// Start (or find) the room's server. With `web`, wait for it and return its URL; otherwise it
/// opens its own window. Blocking: call off the async runtime.
pub fn open(root: &Path, web: bool) -> Result<Option<String>, String> {
    if let Some(port) = crate::hook::poke(root) {
        return Ok(web.then(|| format!("http://127.0.0.1:{port}/")));
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("open").arg(root);
    if web {
        cmd.arg("--no-browser");
    }
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.spawn().map_err(|e| e.to_string())?;
    if !web {
        return Ok(None);
    }
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Some(port) = crate::hook::poke(root) {
            return Ok(Some(format!("http://127.0.0.1:{port}/")));
        }
    }
    Err("the room's server did not start".into())
}

fn same(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_counts_what_waits_on_you_without_writing() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("trio-projects-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(root.join(".trio")).unwrap();
        std::fs::write(root.join(".trio/room.toml"), "me = 'sanjith'\nname = 'api'\nrepo = ''\n").unwrap();
        let ledger = root.join(".trio/room.jsonl");
        let row = |id: &str, at: &str, from: &str, summary: &str, items: &[&str], mentions: &[&str]| crate::model::Event {
            id: id.into(), at: at.into(), from: from.into(),
            body: crate::model::Body::Message { summary: summary.into(), detail: None, mentions: mentions.iter().map(|s| s.to_string()).collect(), items: items.iter().map(|s| s.to_string()).collect() },
        };
        crate::source::append(&ledger, &row("a", "2026-11-24T09:00", "codex", "XF1 ready for review @sanjith", &["XF1"], &["sanjith"])).unwrap();
        crate::source::append(&ledger, &row("b", "2026-11-24T09:05", "claude", "claim XF2", &["XF2"], &[])).unwrap();
        let before = std::fs::read_dir(root.join(".trio")).unwrap().count();
        let s = summary(&root, &root).unwrap();
        assert_eq!((s.name.as_str(), s.current, s.waiting_on_you), ("api", true, 1));
        assert_eq!(s.last, "2026-11-24T09:05");
        assert_eq!(std::fs::read_dir(root.join(".trio")).unwrap().count(), before, "listing writes nothing");
        let _ = std::fs::remove_dir_all(root);
    }
}
