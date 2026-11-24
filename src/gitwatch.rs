//! Branch tips for the map, read on a worker thread so git never runs under the room lock.
//!
//! The room asks for a poll at most every 30 s and collects the result on a later tick without
//! waiting, so a slow `git` call can delay only the map, never chat or routing. A branch becomes
//! an event only when its tip or the base tip moves, and a branch that disappears (deleted ref)
//! becomes a `gone` event that removes it from the graph.

use crate::model::{component_of, Body, Event, Touch};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const EVERY: Duration = Duration::from_secs(30);

pub struct GitWatch {
    req: mpsc::Sender<()>,
    res: mpsc::Receiver<Vec<Event>>,
    busy: bool,
    last: Option<Instant>,
}

impl GitWatch {
    /// Start the worker. It ends when the watch (and so the request channel) is dropped.
    pub fn start(repo: PathBuf, base: Option<String>) -> Self {
        let (req, req_rx) = mpsc::channel::<()>();
        let (res_tx, res) = mpsc::channel();
        let _ = std::thread::Builder::new().name("trio-git".into()).spawn(move || {
            let mut tips = Tips::new(repo, base);
            while req_rx.recv().is_ok() {
                if res_tx.send(tips.poll()).is_err() {
                    break;
                }
            }
        });
        Self { req, res, busy: false, last: None }
    }

    /// Events from a finished poll, if one finished; starts the next poll when it is due.
    /// Never blocks.
    pub fn take(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(evs) = self.res.try_recv() {
            self.busy = false;
            out.extend(evs);
        }
        if !self.busy && self.last.is_none_or(|t| t.elapsed() >= EVERY) && self.req.send(()).is_ok() {
            self.busy = true;
            self.last = Some(Instant::now());
        }
        out
    }

    /// Wait up to `limit` for a poll to finish (tests only).
    #[cfg(test)]
    pub fn wait(&mut self, limit: Duration) -> Vec<Event> {
        self.take();
        match self.res.recv_timeout(limit) {
            Ok(evs) => {
                self.busy = false;
                evs
            }
            Err(_) => Vec::new(),
        }
    }
}

/// The worker's state: what each branch looked like when it was last emitted.
pub struct Tips {
    repo: PathBuf,
    configured_base: Option<String>,
    base: Option<String>,
    /// branch -> `<tip sha>:<base sha>` it was last emitted at.
    seen: HashMap<String, String>,
}

impl Tips {
    pub fn new(repo: PathBuf, base: Option<String>) -> Self {
        Self { repo, configured_base: base, base: None, seen: HashMap::new() }
    }

    /// Events for branches whose tip or base moved, plus `gone` events for branches that no
    /// longer exist. A failed listing changes nothing.
    pub fn poll(&mut self) -> Vec<Event> {
        let repo = self.repo.clone();
        let Some(list) = git(
            &repo,
            &["for-each-ref", "--format=%(refname:short)%09%(objectname)%09%(committerdate:format-local:%Y-%m-%dT%H:%M)%09%(subject)", "refs/heads/claude", "refs/heads/codex"],
        ) else {
            return Vec::new();
        };
        if self.base.is_none() {
            self.base = base_branch(&repo, self.configured_base.as_deref());
        }
        // A merge moves the base, not the branch: key on both tips.
        let base_tip = self.base.as_deref().and_then(|b| git(&repo, &["rev-parse", b])).unwrap_or_default();
        let mut evs = Vec::new();
        let mut listed = Vec::new();
        for l in list.lines() {
            let mut p = l.splitn(4, '\t');
            let (Some(branch), Some(sha), Some(at)) = (p.next(), p.next(), p.next()) else { continue };
            let subject = p.next().unwrap_or("");
            listed.push(branch.to_string());
            let key = format!("{sha}:{base_tip}");
            if self.seen.get(branch) == Some(&key) {
                continue;
            }
            let (touches, merged) = self.base.as_deref().map(|b| branch_touches(&repo, b, sha)).unwrap_or_default();
            self.seen.insert(branch.to_string(), key);
            evs.push(Event {
                id: format!("git:{branch}"),
                at: at.to_string(),
                from: branch.split('/').next().unwrap_or("git").to_string(),
                body: Body::Commit { branch: branch.into(), sha: sha.into(), subject: subject.into(), touches, merged, gone: false },
            });
        }
        let gone: Vec<String> = self.seen.keys().filter(|b| !listed.contains(b)).cloned().collect();
        for branch in gone {
            self.seen.remove(&branch);
            evs.push(Event {
                id: format!("git:{branch}"),
                at: crate::room::now(),
                from: branch.split('/').next().unwrap_or("git").to_string(),
                body: Body::Commit { branch, sha: String::new(), subject: String::new(), touches: Vec::new(), merged: false, gone: true },
            });
        }
        evs
    }
}

pub fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").arg("-C").arg(repo).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The configured base branch, else `main`, else `master`, if it exists.
fn base_branch(repo: &Path, configured: Option<&str>) -> Option<String> {
    let candidates: Vec<&str> = configured.map(|c| vec![c]).unwrap_or_else(|| vec!["main", "master"]);
    candidates.into_iter().find(|b| git(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{b}")]).is_some()).map(str::to_string)
}

/// Components a branch changes since its merge-base with `base`, largest first (top 12), and
/// whether the tip is already merged. Two git calls, made only when a tip moves.
fn branch_touches(repo: &Path, base: &str, sha: &str) -> (Vec<Touch>, bool) {
    let Some(mb) = git(repo, &["merge-base", base, sha]) else { return (Vec::new(), false) };
    if mb == sha {
        return (Vec::new(), true);
    }
    let Some(stat) = git(repo, &["diff", "--numstat", &mb, sha]) else { return (Vec::new(), false) };
    let mut parts: HashMap<String, (u32, u32)> = HashMap::new();
    for line in stat.lines() {
        let mut f = line.splitn(3, '\t');
        let (Some(a), Some(d), Some(path)) = (f.next(), f.next(), f.next()) else { continue };
        let n = a.parse::<u32>().unwrap_or(0) + d.parse::<u32>().unwrap_or(0); // "-" = binary
        let e = parts.entry(component_of(path)).or_default();
        e.0 += n.max(1);
        e.1 += 1;
    }
    let mut out: Vec<Touch> = parts.into_iter().map(|(part, (lines, files))| Touch { part, lines, files }).collect();
    out.sort_by(|a, b| b.lines.cmp(&a.lines).then(a.part.cmp(&b.part)));
    out.truncate(12);
    (out, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Repo(PathBuf);

    impl Repo {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let r = Self(std::env::temp_dir().join(format!("trio-git-{}-{nanos}", std::process::id())));
            std::fs::create_dir_all(r.0.join("src/hook")).unwrap();
            r.run(&["init", "-q", "-b", "main"]);
            r.run(&["config", "user.email", "t@example.com"]);
            r.run(&["config", "user.name", "t"]);
            r.run(&["config", "commit.gpgsign", "false"]);
            std::fs::write(r.0.join("README.md"), "a\n").unwrap();
            r.run(&["add", "."]);
            r.run(&["commit", "-qm", "base"]);
            r
        }
        fn run(&self, args: &[&str]) {
            let ok = std::process::Command::new("git").arg("-C").arg(&self.0).args(args).output().unwrap().status.success();
            assert!(ok, "git {args:?}");
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn branch_touches_group_the_diff_since_the_merge_base() {
        let r = Repo::new();
        r.run(&["checkout", "-qb", "claude/x"]);
        std::fs::write(r.0.join("src/hook/mod.rs"), "1\n2\n3\n").unwrap();
        std::fs::write(r.0.join("src/main.rs"), "1\n").unwrap();
        r.run(&["add", "."]);
        r.run(&["commit", "-qm", "work"]);
        let sha = git(&r.0, &["rev-parse", "HEAD"]).unwrap();

        assert_eq!(base_branch(&r.0, None).as_deref(), Some("main"));
        assert_eq!(base_branch(&r.0, Some("trunk")), None);
        let (t, merged) = branch_touches(&r.0, "main", &sha);
        assert!(!merged);
        let got: Vec<_> = t.iter().map(|t| (t.part.as_str(), t.lines, t.files)).collect();
        assert_eq!(got, vec![("src/hook", 3, 1), ("src", 1, 1)]);

        r.run(&["checkout", "-q", "main"]);
        r.run(&["merge", "-q", "--ff-only", "claude/x"]);
        assert_eq!(branch_touches(&r.0, "main", &sha), (vec![], true));
    }

    fn summary(evs: &[Event]) -> Vec<(String, bool, bool)> {
        let mut v: Vec<_> = evs
            .iter()
            .filter_map(|e| match &e.body {
                Body::Commit { branch, merged, gone, .. } => Some((branch.clone(), *merged, *gone)),
                _ => None,
            })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn tips_emit_changes_merges_and_deletions_once() {
        let r = Repo::new();
        r.run(&["branch", "claude/a"]);
        r.run(&["checkout", "-qb", "codex/b"]);
        std::fs::write(r.0.join("src/main.rs"), "1\n").unwrap();
        r.run(&["add", "."]);
        r.run(&["commit", "-qm", "b"]);
        r.run(&["checkout", "-q", "main"]);

        let mut tips = Tips::new(r.0.clone(), None);
        assert_eq!(summary(&tips.poll()), vec![("claude/a".into(), true, false), ("codex/b".into(), false, false)]);
        assert!(tips.poll().is_empty(), "nothing moved");
        r.run(&["merge", "-q", "--ff-only", "codex/b"]); // the base moves: both are re-read
        assert_eq!(summary(&tips.poll()), vec![("claude/a".into(), true, false), ("codex/b".into(), true, false)]);
        r.run(&["branch", "-D", "codex/b"]);
        assert_eq!(summary(&tips.poll()), vec![("codex/b".into(), false, true)]);
        assert!(tips.poll().is_empty());
    }

    #[test]
    fn the_watch_polls_off_thread() {
        let r = Repo::new();
        r.run(&["branch", "claude/a"]);
        let mut w = GitWatch::start(r.0.clone(), None);
        let evs = w.wait(Duration::from_secs(20));
        assert_eq!(summary(&evs), vec![("claude/a".into(), true, false)]);
        assert!(w.take().is_empty(), "the next poll is not due for 30 s");
    }
}
