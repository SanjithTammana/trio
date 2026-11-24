//! GitHub pull requests as room events, read with the `gh` CLI on a worker thread.
//!
//! Each pull request becomes a few events keyed by stable ids, so a re-read replaces what changed:
//! - `gh:<repo>#<n>`: one message on the PR's branch. While the PR is open and not a draft it asks
//!   for review; merged, closed or draft, it no longer does (the ask is withdrawn on replay too).
//! - `gh:<repo>#<n>:review:<login>:<time>`: an approval or a change request, as a verdict.
//! - `gh:<repo>#<n>:tip` / `:gone`: for a repo with no local clone in the room, the branch itself
//!   (title, head commit, merged), and its removal when the PR closes unmerged.
//!
//! A message mentions the people asked to review (so the card waits on them, or on you), but
//! never an agent: a pull request does not wake Claude or Codex by itself.
//! The room appends these to its ledger like git transitions, so restart replays them.

use crate::model::{Body, Event};
use serde::Deserialize;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const EVERY: Duration = Duration::from_secs(120);
const LIMIT: &str = "40";

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Pr {
    pub number: u64,
    pub title: String,
    pub head_ref_name: String,
    #[serde(default)]
    pub head_ref_oid: String,
    /// OPEN, CLOSED or MERGED.
    pub state: String,
    #[serde(default)]
    pub is_draft: bool,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub merged_at: Option<String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub author: Option<Login>,
    #[serde(default)]
    pub review_requests: Vec<Requested>,
    #[serde(default)]
    pub latest_reviews: Vec<Review>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Login {
    #[serde(default)]
    pub login: String,
}

/// A requested reviewer: a user (`login`) or a team (`name`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Requested {
    #[serde(default)]
    pub login: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    #[serde(default)]
    pub author: Option<Login>,
    pub state: String,
    #[serde(default)]
    pub submitted_at: String,
}

/// How one GitHub repo maps into a room.
#[derive(Debug, Clone, Default)]
pub struct Link {
    /// `owner/name`
    pub repo: String,
    /// Branch label, matching the local repo's when one is linked (see `room::repos_of`).
    pub label: Option<String>,
    /// No local clone in the room: the PR stands in for the branch.
    pub remote_only: bool,
    /// The room's human, and their GitHub login when `gh` knows it.
    pub me: String,
    pub my_login: Option<String>,
    /// The room's agent handles (branch prefixes that name an agent). Empty: claude, codex.
    pub agents: Vec<String>,
}

impl Link {
    fn branch(&self, head: &str) -> String {
        self.label.as_deref().map_or_else(|| head.to_string(), |l| format!("{l}:{head}"))
    }

    /// The room participant for a GitHub login: the room's human for their own login, an agent
    /// for its branch prefix (PRs Claude or Codex opened), else the login itself.
    fn who(&self, login: &str, head: Option<&str>) -> String {
        if let Some(agent) = head.map(crate::model::branch_prefix).filter(|p| self.is_agent(p)) {
            return agent.to_string();
        }
        if login.is_empty() {
            return "github".into(); // a deleted account or an app: never taken for you
        }
        if self.my_login.as_deref().is_some_and(|m| m.eq_ignore_ascii_case(login)) {
            return self.me.clone();
        }
        login.to_ascii_lowercase()
    }

    fn is_agent(&self, name: &str) -> bool {
        if self.agents.is_empty() { ["claude", "codex"].contains(&name) } else { self.agents.iter().any(|a| a == name) }
    }

    /// The events one pull request stands for. Pure: the same PR always gives the same events.
    pub fn events(&self, pr: &Pr) -> Vec<Event> {
        let branch = self.branch(&pr.head_ref_name);
        let id = format!("gh:{}#{}", self.repo, pr.number);
        let author = self.who(pr.author.as_ref().map_or("", |a| a.login.as_str()), Some(&pr.head_ref_name));
        let opened = local(&pr.created_at).unwrap_or_else(crate::room::now);
        let title = format!("PR #{} \u{201c}{}\u{201d}", pr.number, pr.title);
        let reviewers: Vec<String> = pr.review_requests.iter().filter_map(|r| r.login.clone().map(|l| self.who(&l, None)).or_else(|| r.name.clone())).collect();
        let open = pr.state == "OPEN";
        let asks = open && !pr.is_draft;
        // Only an open, non-draft PR asks for review. Any other state keeps the title out of the
        // summary (a title saying "ready for review" must not ask) and names no branch: a closed
        // PR's row must not recreate a branch deleted since.
        let summary = match (pr.state.as_str(), pr.is_draft) {
            ("MERGED", _) => format!("PR #{} was merged", pr.number),
            ("CLOSED", _) => format!("PR #{} was closed without merging", pr.number),
            (_, true) => format!("PR #{} is a draft", pr.number),
            _ if reviewers.is_empty() => format!("{title} is ready for review"),
            _ => format!("{title} is ready for review by {}", reviewers.join(", ")),
        };
        let detail = if asks { pr.url.clone() } else { format!("\u{201c}{}\u{201d}\n{}", pr.title, pr.url) };
        // Rows replay in time order, so a row that closes the PR is dated when it closed.
        let at = match pr.state.as_str() {
            "MERGED" => pr.merged_at.as_deref().and_then(local).or_else(|| local(&pr.updated_at)),
            "CLOSED" => local(&pr.updated_at),
            _ => None,
        }
        .unwrap_or_else(|| opened.clone());
        let mut out = vec![Event {
            id: id.clone(),
            at,
            from: author.clone(),
            body: Body::Message {
                summary,
                detail: (!detail.trim().is_empty()).then(|| detail.trim().to_string()),
                mentions: if asks { pr.review_requests.iter().filter_map(|r| r.login.as_deref()).map(|l| self.who(l, None)).filter(|w| !self.is_agent(w)).collect() } else { Vec::new() },
                items: if asks { vec![branch.clone()] } else { Vec::new() },
            },
        }];
        // Reviews name the branch, so after a PR closes they could bring back a branch deleted
        // here. Those seen while it was open are recorded already; a clone's own refs say the rest.
        let reviews: &[Review] = if open || self.remote_only { &pr.latest_reviews } else { &[] };
        for r in reviews {
            let outcome = match r.state.as_str() {
                "APPROVED" => "pass",
                "CHANGES_REQUESTED" => "changes",
                _ => continue,
            };
            let login = r.author.as_ref().map_or("", |a| a.login.as_str());
            out.push(Event {
                id: format!("{id}:review:{login}:{}", r.submitted_at),
                at: local(&r.submitted_at).unwrap_or_else(|| opened.clone()),
                from: self.who(login, None),
                body: Body::Verdict { item: branch.clone(), outcome: outcome.into() },
            });
        }
        if self.remote_only {
            let merged = pr.state == "MERGED";
            let at = pr.merged_at.as_deref().and_then(local).or_else(|| local(&pr.updated_at)).unwrap_or_else(|| opened.clone());
            out.push(Event {
                id: format!("{id}:tip"),
                at: if merged { at } else { opened.clone() },
                from: author.clone(),
                body: Body::Commit { branch: branch.clone(), sha: pr.head_ref_oid.clone(), subject: pr.title.clone(), touches: Vec::new(), merged, gone: false },
            });
            if pr.state == "CLOSED" {
                let at = local(&pr.updated_at).unwrap_or(opened);
                out.push(Event { id: format!("{id}:gone"), at, from: author, body: Body::Commit { branch, sha: pr.head_ref_oid.clone(), subject: String::new(), touches: Vec::new(), merged: false, gone: true } });
            }
        }
        out
    }
}

/// `owner/name` and nothing else: no host (`gh` would accept `host/owner/name`), no flag-like
/// leading dash, only the characters GitHub allows.
pub fn valid_repo(repo: &str) -> bool {
    let chars = |p: &str| !p.is_empty() && !p.starts_with('-') && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    // Owners never start with a dot; names may (`org/.github`), but are never `.` or `..`.
    matches!(repo.split_once('/'), Some((o, n)) if chars(o) && !o.starts_with('.') && chars(n) && n != "." && n != "..")
}

/// `2026-11-24T18:03:11Z` -> local `2026-11-24T12:03`.
pub fn local(iso: &str) -> Option<String> {
    let t = chrono::DateTime::parse_from_rfc3339(iso).ok()?;
    Some(t.with_timezone(&chrono::Local).format("%Y-%m-%dT%H:%M").to_string())
}

/// `owner/name` from a GitHub remote URL (https or ssh), else None.
pub fn repo_of_url(url: &str) -> Option<String> {
    let rest = url.trim().strip_prefix("https://github.com/").or_else(|| url.trim().strip_prefix("git@github.com:")).or_else(|| url.trim().strip_prefix("ssh://git@github.com/"))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = rest.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    let ok = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    (ok(owner) && ok(name) && parts.next().is_none()).then(|| format!("{owner}/{name}"))
}

/// The GitHub repo a local clone pushes to (`origin`), if it is on GitHub.
pub fn repo_of_clone(dir: &std::path::Path) -> Option<String> {
    repo_of_url(&crate::gitwatch::git(dir, &["remote", "get-url", "origin"])?)
}

fn gh(args: &[&str]) -> Option<String> {
    let mut cmd = std::process::Command::new("gh");
    cmd.args(args).env("GH_PROMPT_DISABLED", "1").env("NO_COLOR", "1");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flash from the server
    }
    let out = cmd.output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// One read of the repo's recent pull requests. None when `gh` is missing, signed out or offline.
pub fn read(repo: &str) -> Option<Vec<Pr>> {
    let fields = "number,title,headRefName,headRefOid,state,isDraft,createdAt,updatedAt,mergedAt,url,author,reviewRequests,latestReviews";
    serde_json::from_str(&gh(&["pr", "list", "-R", repo, "--state", "all", "--limit", LIMIT, "--json", fields])?).ok()
}

/// Pull requests for one linked repo, read every two minutes on a worker thread.
pub struct GitHubWatch {
    req: mpsc::Sender<()>,
    res: mpsc::Receiver<Vec<Event>>,
    busy: bool,
    last: Option<Instant>,
}

impl GitHubWatch {
    pub fn start(mut link: Link) -> Self {
        let (req, req_rx) = mpsc::channel::<()>();
        let (res_tx, res) = mpsc::channel();
        let _ = std::thread::Builder::new().name("trio-github".into()).spawn(move || {
            while req_rx.recv().is_ok() {
                if link.my_login.is_none() {
                    link.my_login = gh(&["api", "user", "--jq", ".login"]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
                }
                let events = read(&link.repo).map(|prs| prs.iter().flat_map(|p| link.events(p)).collect()).unwrap_or_default();
                if res_tx.send(events).is_err() {
                    break;
                }
            }
        });
        Self { req, res, busy: false, last: None }
    }

    /// Finished reads; starts the next one when it is due. Never blocks.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(state: &str, draft: bool, reviews: Vec<Review>) -> Pr {
        Pr {
            number: 12,
            title: "Add the login form".into(),
            head_ref_name: "feature/login".into(),
            head_ref_oid: "abc123".into(),
            state: state.into(),
            is_draft: draft,
            created_at: "2026-11-24T15:00:00Z".into(),
            updated_at: "2026-11-24T16:00:00Z".into(),
            merged_at: (state == "MERGED").then(|| "2026-11-24T17:00:00Z".into()),
            url: "https://github.com/o/r/pull/12".into(),
            author: Some(Login { login: "Sanjith".into() }),
            review_requests: vec![Requested { login: Some("ana".into()), name: None }],
            latest_reviews: reviews,
        }
    }

    fn link(remote_only: bool) -> Link {
        Link { repo: "o/r".into(), label: Some("web".into()), remote_only, me: "sanjith".into(), my_login: Some("sanjith".into()), agents: Vec::new() }
    }

    #[test]
    fn only_owner_and_name_reach_gh() {
        assert!(valid_repo("SanjithTammana/trio") && valid_repo("o/r.js") && valid_repo("a-b/c_d") && valid_repo("org/.github"));
        for bad in ["github.com/o/r", "o", "o/", "/r", "-R/x", "o/-x", "o/r/x", "o r/x", "../x", "o/..", "o/.", ".o/x"] {
            assert!(!valid_repo(bad), "{bad}");
        }
    }

    #[test]
    fn an_open_pr_asks_for_review_and_reviews_are_verdicts() {
        let review = |who: &str, state: &str| Review { author: Some(Login { login: who.into() }), state: state.into(), submitted_at: "2026-11-24T16:30:00Z".into() };
        let evs = link(false).events(&pr("OPEN", false, vec![review("ana", "CHANGES_REQUESTED"), review("bo", "COMMENTED")]));
        assert_eq!(evs.len(), 2, "a comment is not a verdict; a linked clone owns the branch itself");
        let Body::Message { summary, items, mentions, .. } = &evs[0].body else { panic!() };
        assert_eq!((summary.as_str(), items.as_slice(), evs[0].from.as_str()), ("PR #12 \u{201c}Add the login form\u{201d} is ready for review by ana", ["web:feature/login".to_string()].as_slice(), "sanjith"));
        assert_eq!(mentions, &["ana".to_string()], "the card waits on the requested reviewer; never an agent");
        assert_eq!(evs[1].body, Body::Verdict { item: "web:feature/login".into(), outcome: "changes".into() });
        assert_eq!(evs[1].from, "ana");
    }

    #[test]
    fn a_remote_only_pr_is_the_branch_and_closing_it_retires_it() {
        let merged = link(true).events(&pr("MERGED", false, vec![]));
        let Body::Message { summary, .. } = &merged[0].body else { panic!() };
        assert!(summary.ends_with("was merged"));
        assert!(matches!(&merged[1].body, Body::Commit { merged: true, subject, .. } if subject == "Add the login form"));
        assert_eq!(merged[1].at, local("2026-11-24T17:00:00Z").unwrap(), "merged when GitHub merged it");
        let closed = link(true).events(&pr("CLOSED", false, vec![]));
        assert!(matches!(closed.last().unwrap().body, Body::Commit { gone: true, .. }));
        let draft = link(true).events(&pr("OPEN", true, vec![]));
        let Body::Message { summary, .. } = &draft[0].body else { panic!() };
        assert!(summary.ends_with("is a draft"), "{summary}");
    }

    #[test]
    fn agent_branches_are_the_agents_and_urls_name_repos() {
        let mut p = pr("OPEN", false, vec![]);
        p.head_ref_name = "codex/parser".into();
        assert_eq!(link(false).events(&p)[0].from, "codex");
        assert_eq!(repo_of_url("https://github.com/SanjithTammana/trio.git").as_deref(), Some("SanjithTammana/trio"));
        assert_eq!(repo_of_url("git@github.com:o/r").as_deref(), Some("o/r"));
        assert_eq!(repo_of_url("https://gitlab.com/o/r"), None);
        assert_eq!(repo_of_url("https://github.com/o/r/pull/3"), None);
    }
}
