//! Branch tips for the map, read on a worker thread so git never runs under the room lock.
//!
//! The room asks for a poll at most every 30 s and collects the result on a later tick without
//! waiting, so a slow `git` call can delay only the map, never chat or routing. A branch becomes
//! an event only when its tip or the base tip moves, and a branch that disappears (deleted ref)
//! becomes a `gone` event that removes it from the graph.
//!
//! Tip changes, merges and deletions are durable, so restart keeps the history of each ref.
//! A merge is stamped when seen, or with commit time if it was already merged on first sight.

use crate::model::{component_of, Body, Event, Touch};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const EVERY: Duration = Duration::from_secs(30);

/// One finished poll.
#[derive(Debug, Default)]
pub struct Poll {
    /// Base-only refreshes change the diff without adding activity.
    pub tips: Vec<Event>,
    /// Transitions, in the order the watcher observed them.
    pub durable: Vec<Event>,
}

#[derive(Default)]
pub struct Seed {
    pub serials: HashMap<String, u64>,
    pub tips: Vec<(String, String, String, bool)>,
}

pub struct GitWatch {
    req: mpsc::Sender<Option<Seed>>,
    res: mpsc::Receiver<Option<Poll>>,
    busy: bool,
    last: Option<Instant>,
}

impl GitWatch {
    /// Start the worker. It ends when the watch (and so the request channel) is dropped.
    pub fn start(repo: PathBuf, base: Option<String>) -> Self {
        let (req, req_rx) = mpsc::channel::<Option<Seed>>();
        let (res_tx, res) = mpsc::channel();
        let _ = std::thread::Builder::new().name("trio-git".into()).spawn(move || {
            let mut tips = Tips::new(repo, base);
            while let Ok(seed) = req_rx.recv() {
                if let Some(seed) = seed {
                    tips.restore(seed);
                }
                if res_tx.send(tips.poll()).is_err() {
                    break;
                }
            }
        });
        Self { req, res, busy: false, last: None }
    }

    /// Finished polls (a failed listing yields none); starts the next poll when it is due.
    /// Never blocks.
    pub fn take(&mut self, seed: Option<Seed>) -> Vec<Poll> {
        let mut out = Vec::new();
        while let Ok(p) = self.res.try_recv() {
            self.busy = false;
            out.extend(p);
        }
        if !self.busy && self.last.is_none_or(|t| t.elapsed() >= EVERY) && self.req.send(seed).is_ok() {
            self.busy = true;
            self.last = Some(Instant::now());
        }
        out
    }

    /// Wait up to `limit` for a poll to finish (tests only).
    #[cfg(test)]
    pub fn wait(&mut self, limit: Duration) -> Option<Poll> {
        if !self.busy {
            self.last = None;
            self.take(None);
        }
        let p = self.res.recv_timeout(limit).ok().flatten();
        self.busy = false;
        p
    }
}

/// The worker's state: what each branch looked like when it was last emitted.
pub struct Tips {
    repo: PathBuf,
    configured_base: Option<String>,
    base: Option<String>,
    /// What each branch looked like when it was last emitted.
    seen: HashMap<String, Tip>,
    serials: HashMap<String, u64>,
}

#[derive(Clone)]
struct Tip {
    /// `<tip sha>:<base sha>`
    key: String,
    sha: String,
    subject: String,
    merged: bool,
}

impl Tips {
    pub fn new(repo: PathBuf, base: Option<String>) -> Self {
        Self { repo, configured_base: base, base: None, seen: HashMap::new(), serials: HashMap::new() }
    }

    fn restore(&mut self, seed: Seed) {
        self.serials = seed.serials;
        self.seen.extend(seed.tips.into_iter().map(|(branch, sha, subject, merged)| {
            (branch, Tip { key: String::new(), sha, subject, merged })
        }));
    }

    /// Events for branches whose tip or base moved, plus `gone` events for branches that no
    /// longer exist. A failed listing changes nothing (None).
    pub fn poll(&mut self) -> Option<Poll> {
        let repo = self.repo.clone();
        let list = git(
            &repo,
            &["for-each-ref", "--format=%(refname:short)%09%(objectname)%09%(committerdate:format-local:%Y-%m-%dT%H:%M)%09%(subject)", "refs/heads/claude", "refs/heads/codex"],
        )?;
        if self.base.is_none() {
            self.base = base_branch(&repo, self.configured_base.as_deref());
        }
        // A merge moves the base, not the branch: key on both tips.
        let base_tip = self.base.as_deref().and_then(|b| git(&repo, &["rev-parse", b])).unwrap_or_default();
        let mut out = Poll::default();
        let observed = crate::room::now();
        let mut listed = std::collections::HashSet::new();
        for l in list.lines() {
            let mut p = l.splitn(4, '\t');
            let (Some(branch), Some(sha), Some(at)) = (p.next(), p.next(), p.next()) else { continue };
            let subject = p.next().unwrap_or("");
            listed.insert(branch);
            let key = format!("{sha}:{base_tip}");
            let mut was = self.seen.get(branch).cloned();
            if was.as_ref().is_some_and(|w| w.key == key) {
                continue;
            }
            let (touches, merged) = self.base.as_deref().map(|b| branch_touches(&repo, b, sha)).unwrap_or_default();
            if was.as_ref().is_some_and(|w| w.sha != sha && !is_ancestor(&repo, &w.sha, sha)) {
                self.retire(&mut out, branch, &was.take().expect("previous tip"), &observed);
            }
            self.seen.insert(branch.to_string(), Tip { key, sha: sha.into(), subject: subject.into(), merged });
            let body = Body::Commit { branch: branch.into(), sha: sha.into(), subject: subject.into(), touches, merged, gone: false };
            let moved = was.as_ref().is_none_or(|w| w.sha != sha);
            if moved && (!merged || was.is_some()) {
                let mut tip = body.clone();
                if let Body::Commit { merged, .. } = &mut tip {
                    *merged = false;
                }
                out.durable.push(self.transition(branch, "tip", sha, at, &observed, tip));
            }
            if merged && (moved || was.as_ref().is_some_and(|w| !w.merged)) {
                let at = if was.is_some() { observed.as_str() } else { at };
                out.durable.push(self.transition(branch, "merged", sha, at, &observed, body));
            } else if !moved && was.is_some_and(|w| w.merged != merged) {
                out.durable.push(self.transition(branch, "reset", sha, &observed, &observed, body));
            } else if !moved && !merged {
                out.tips.push(Event { id: format!("git:{branch}:refresh"), at: at.to_string(), from: owner(branch), body });
            }
        }
        let mut gone: Vec<String> = self.seen.keys().filter(|b| !listed.contains(b.as_str())).cloned().collect();
        gone.sort_unstable();
        for branch in gone {
            let Some(t) = self.seen.remove(&branch) else { continue };
            self.retire(&mut out, &branch, &t, &observed);
        }
        Some(out)
    }

    fn retire(&mut self, out: &mut Poll, branch: &str, tip: &Tip, observed: &str) {
        // A merge and deletion can both happen between polls.
        let in_base = !tip.merged && self.base.as_deref().is_some_and(|b| is_ancestor(&self.repo, &tip.sha, b));
        if in_base {
            let body = Body::Commit { branch: branch.into(), sha: tip.sha.clone(), subject: tip.subject.clone(), touches: Vec::new(), merged: true, gone: false };
            out.durable.push(self.transition(branch, "merged", &tip.sha, observed, observed, body));
        }
        let gone = Body::Commit { branch: branch.into(), sha: tip.sha.clone(), subject: String::new(), touches: Vec::new(), merged: false, gone: true };
        out.durable.push(self.transition(branch, "gone", &tip.sha, observed, observed, gone));
    }

    fn transition(&mut self, branch: &str, action: &str, sha: &str, at: &str, observed: &str, body: Body) -> Event {
        let serial = self.serials.entry(branch.into()).or_default();
        *serial += 1;
        Event { id: format!("git:{branch}:{action}:{sha}:{serial}@{observed}"), at: at.into(), from: owner(branch), body }
    }
}

fn owner(branch: &str) -> String {
    branch.split('/').next().unwrap_or("git").to_string()
}

fn action(e: &Event) -> Option<&str> {
    let Body::Commit { branch, .. } = &e.body else { return None };
    e.id.strip_prefix("git:")?.strip_prefix(branch)?.strip_prefix(':')?.split(':').next()
}

pub fn serial(e: &Event) -> Option<u64> {
    action(e)?;
    e.id.rsplit_once('@')?.0.rsplit_once(':')?.1.parse().ok()
}

/// Ref transitions replay in observation order even when a new tip has an old commit date.
pub fn replay_at(e: &Event) -> &str {
    if serial(e).is_some() && let Some((_, observed)) = e.id.rsplit_once('@') {
        observed
    } else {
        &e.at
    }
}

pub fn is_refresh(e: &Event) -> bool {
    action(e) == Some("refresh")
}

pub fn is_transition(e: &Event) -> bool {
    matches!(action(e), Some("tip" | "merged" | "gone" | "reset"))
}

pub fn is_merge(e: &Event) -> bool {
    matches!(e.body, Body::Commit { merged: true, .. }) && action(e) != Some("tip")
}

pub fn is_tip(e: &Event) -> bool {
    matches!(e.body, Body::Commit { gone: false, .. }) && !is_merge(e) && !matches!(action(e), Some("reset" | "refresh"))
}

fn is_ancestor(repo: &Path, sha: &str, base: &str) -> bool {
    std::process::Command::new("git").arg("-C").arg(repo).args(["merge-base", "--is-ancestor", sha, base]).output().is_ok_and(|o| o.status.success())
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

    fn summary(p: Option<Poll>) -> Vec<(String, bool, bool)> {
        let p = p.expect("listing");
        let mut v: Vec<_> = p
            .tips
            .iter()
            .chain(p.durable.iter())
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
        assert_eq!(summary(tips.poll()), vec![("claude/a".into(), true, false), ("codex/b".into(), false, false)]);
        assert!(summary(tips.poll()).is_empty(), "nothing moved");
        r.run(&["merge", "-q", "--ff-only", "codex/b"]); // the base moves: both are re-read
        let before = crate::room::now();
        let p = tips.poll().unwrap();
        let after = crate::room::now();
        assert_eq!(p.durable.len(), 1, "codex/b was seen merging");
        assert!(p.durable[0].id.starts_with("git:codex/b:merged:") && (before..=after).contains(&p.durable[0].at), "stamped when seen");
        assert_eq!(summary(Some(p)), vec![("codex/b".into(), true, false)]);
        r.run(&["branch", "-D", "codex/b"]);
        assert_eq!(summary(tips.poll()), vec![("codex/b".into(), false, true)]);
        assert!(summary(tips.poll()).is_empty());
    }

    #[test]
    fn a_branch_merged_and_deleted_between_polls_still_records_the_merge() {
        let r = Repo::new();
        r.run(&["checkout", "-qb", "codex/c"]);
        std::fs::write(r.0.join("src/main.rs"), "c\n").unwrap();
        r.run(&["add", "."]);
        r.run(&["commit", "-qm", "XF7 work"]);
        r.run(&["checkout", "-q", "main"]);
        let mut tips = Tips::new(r.0.clone(), None);
        assert_eq!(summary(tips.poll()), vec![("codex/c".into(), false, false)]);
        r.run(&["merge", "-q", "--ff-only", "codex/c"]);
        r.run(&["branch", "-D", "codex/c"]);
        let p = tips.poll().unwrap();
        let got: Vec<(String, bool, bool)> = p.durable.iter().map(|e| match &e.body {
            Body::Commit { branch, merged, gone, .. } => (branch.clone(), *merged, *gone),
            _ => unreachable!(),
        }).collect();
        assert_eq!(got, vec![("codex/c".into(), true, false), ("codex/c".into(), false, true)], "merge first, then the deletion");
    }

    #[test]
    fn the_watch_polls_off_thread() {
        let r = Repo::new();
        r.run(&["branch", "claude/a"]);
        let mut w = GitWatch::start(r.0.clone(), None);
        assert_eq!(summary(w.wait(Duration::from_secs(20))), vec![("claude/a".into(), true, false)]);
        assert!(w.take(None).is_empty(), "the next poll is not due for 30 s");
    }
}
