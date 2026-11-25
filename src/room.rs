//! A room: its config, its sources, a bounded event window, the derived graph, and the agents.
//!
//! Cost model:
//! - tick: one `stat` per source, O(sources); bytes read only when a file grew, O(delta).
//! - apply: O(1) amortized per event (hash index into the window; the window is trimmed in
//!   batches, so the O(n) re-index runs once per `WINDOW / 4` events).
//! - memory: two independent windows of at most `WINDOW * 1.25` events, plus the graph.

use crate::agent::{wake_prompt, Agent, AgentConfig, CliAgent, Guard, WakeDecision};
use crate::graph::{derivers, Deriver, Graph};
use crate::model::{clip, split_reply, Body, Event, Summarized, SUMMARY_MAX};
use crate::source::{append, mentions_in, PostsFile, RoomLedger, Source};
use crate::work::cap;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const WINDOW: usize = 1500;

#[derive(Debug, Deserialize, Default)]
pub struct RoomCfg {
    #[serde(default)]
    pub name: String,
    /// The human's handle in this room (`trio say --as <me>`, `@<me>`). Default: the OS user name.
    #[serde(default)]
    pub me: String,
    #[serde(default = "default_budget")]
    pub hop_budget: u32,
    #[serde(default)]
    pub paused: bool,
    /// Existing thread files to show read-only: `{ file = "codex-posts.md", from = "codex" }`.
    #[serde(default)]
    pub import: Vec<Import>,
    /// Git repo whose branch tips appear in the graph. Unset: the work tree the room sits in.
    /// `repo = ""` turns git off.
    #[serde(default)]
    pub repo: Option<PathBuf>,
    /// Branch that work merges into (default: `main`, else `master`).
    #[serde(default)]
    pub base: Option<String>,
    /// More repos the project spans: `[[repos]] path = "../api"` (name defaults to the folder).
    /// With more than one repo, branches show as `<name>:<branch>`.
    #[serde(default)]
    pub repos: Vec<RepoCfg>,
    /// GitHub repos whose pull requests appear as review requests and verdicts:
    /// `[[github]] repo = "owner/name"`. Read with your `gh` login every two minutes.
    #[serde(default)]
    pub github: Vec<GitHubCfg>,
    /// Your own item id prefixes, on top of the built-in ones (P, XF, H, ...):
    /// `item_prefixes = ["WEB", "ENG"]` makes `WEB-12` and `ENG42` work items.
    #[serde(default)]
    pub item_prefixes: Vec<String>,
    #[serde(default)]
    pub agents: HashMap<String, AgentConfig>,
    /// Daily spend caps in dollars: `[budget] claude_usd_day = 5.0`, `codex_usd_day = 3.0`.
    #[serde(default)]
    pub budget: Budget,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Budget {
    #[serde(default)]
    pub claude_usd_day: Option<f64>,
    #[serde(default)]
    pub codex_usd_day: Option<f64>,
}

impl Budget {
    pub fn of(&self, harness: &str) -> Option<f64> {
        match harness {
            "claude" => self.claude_usd_day,
            "codex" => self.codex_usd_day,
            _ => None,
        }
    }
}

/// A wake waiting for its agent to be available again.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Held {
    agent: String,
    event: Event,
    /// The handoff note of a rerouted message ("this was for Claude, who is out..."), so the
    /// wake that finally goes out still says why the agent is taking it.
    #[serde(default)]
    handoff: Vec<String>,
    /// Never rerouted: the other agent wrote the message or the work it asks about. Decided when
    /// held, so it holds after a ledger cut re-keys the event.
    #[serde(default)]
    stay: bool,
    /// A reroute of this entry already failed to start: not tried again until a restart or a
    /// replay. The entry still goes out when its own agent is back.
    #[serde(skip)]
    tried: bool,
}

fn held_path(root: &Path) -> PathBuf {
    root.join(".trio").join("held.json")
}

/// Held wakes kept at most (the oldest go first).
const HELD_MAX: usize = 20;
/// How often usage readings are re-read from disk.
const READ_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

fn default_budget() -> u32 {
    3
}

/// The other agent, for rerouting.
/// The agents' handles in the order room.toml lists them (toml tables carry no order once
/// parsed into a map, so it is read off the document itself). Handles it cannot place go last.
fn agent_order(text: &str, cfg: &RoomCfg) -> Vec<String> {
    let mut order: Vec<String> = text
        .parse::<toml_edit::DocumentMut>()
        .ok()
        .and_then(|d| d.get("agents").and_then(|a| a.as_table_like()).map(|t| t.iter().map(|(k, _)| k.to_string()).collect()))
        .unwrap_or_default();
    order.retain(|n| cfg.agents.contains_key(n));
    let mut rest: Vec<String> = cfg.agents.keys().filter(|k| !order.contains(k)).cloned().collect();
    rest.sort();
    order.extend(rest);
    order
}

#[derive(Debug, Deserialize, Clone)]
pub struct Import {
    pub file: String,
    pub from: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RepoCfg {
    pub path: PathBuf,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub base: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GitHubCfg {
    /// `owner/name`
    pub repo: String,
    /// Label for its branches when it has no local clone here (default: the repo's name).
    #[serde(default)]
    pub name: String,
}

/// How each `[[github]]` entry maps into the room: a linked local clone (same `origin`) shares
/// that clone's branch names; one without a clone stands in for its branches itself.
pub fn github_links(cfg: &RoomCfg, root: &Path) -> Vec<crate::github::Link> {
    let local: Vec<(Option<String>, Option<String>)> = repos_of(cfg, root).into_iter().map(|(label, path, _)| (label, crate::github::repo_of_clone(&path))).collect();
    let many = local.len() + cfg.github.len() > 1;
    // A repo without a clone gets a label no local repo (or earlier link) uses, so a remote
    // branch never shares an id with a local one.
    let mut taken: Vec<String> = repos_named(cfg, root).into_iter().map(|(n, _, _)| n.to_ascii_lowercase()).collect();
    let mut unique = move |own: String| {
        let mut l = own.clone();
        let mut n = 1;
        while taken.contains(&l.to_ascii_lowercase()) {
            n += 1;
            l = format!("{own}-{n}");
        }
        taken.push(l.to_ascii_lowercase());
        l
    };
    cfg.github
        .iter()
        .filter(|g| {
            let ok = crate::github::valid_repo(&g.repo);
            if !ok {
                eprintln!("trio: [[github]] repo {:?} is not owner/name; skipped", g.repo);
            }
            ok
        })
        .map(|g| {
            let clone = local.iter().find(|(_, r)| r.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(&g.repo)));
            let own = if g.name.is_empty() { g.repo.rsplit('/').next().unwrap_or(&g.repo).to_string() } else { g.name.clone() };
            let own = own.replace(':', "-");
            crate::github::Link {
                repo: g.repo.clone(),
                label: match clone {
                    Some((l, _)) => l.clone(),
                    None => many.then(|| unique(own)),
                },
                remote_only: clone.is_none(),
                me: human(cfg),
                my_login: None,
                agents: cfg.agents.keys().cloned().collect(),
            }
        })
        .collect()
}

/// One watched repo: its label (None when the room has only one) and its watcher.
struct GitSource {
    label: Option<String>,
    watch: crate::gitwatch::GitWatch,
}

/// Branch names change shape when a room gains or loses repos: one repo's branches are bare
/// (`claude/x`), several repos' carry their repo's label (`web:claude/x`). Rows recorded in the
/// other shape are read in this one, so history joins the live watcher instead of leaving a twin.
struct Relabel {
    /// The label of the room's first repo, which bare names belong to.
    main: String,
    many: bool,
}

impl Relabel {
    fn name(&self, b: &str) -> Option<String> {
        if !b.contains('/') {
            return None;
        }
        if self.many {
            (!b.contains(':')).then(|| format!("{}:{b}", self.main))
        } else {
            b.strip_prefix(&format!("{}:", self.main)).filter(|r| !r.contains(':')).map(str::to_string)
        }
    }

    fn fix(&self, b: &mut String) {
        if let Some(n) = self.name(b) {
            *b = n;
        }
    }

    fn apply(&self, e: &mut Event) {
        match &mut e.body {
            Body::Commit { branch, .. } => {
                if let Some(n) = self.name(branch) {
                    if let Some(rest) = e.id.strip_prefix(&format!("git:{branch}:")) {
                        e.id = format!("git:{n}:{rest}");
                    }
                    *branch = n;
                }
            }
            Body::Message { items, .. } => items.iter_mut().for_each(|i| self.fix(i)),
            Body::Verdict { item, .. } | Body::Claim { item } => self.fix(item),
            _ => {}
        }
    }
}

/// Every repo a room watches, with its name: `repo` (or the work tree the room sits in), then
/// `[[repos]]`. A repo listed twice is watched once; a name used twice gets a number.
fn repos_named(cfg: &RoomCfg, root: &Path) -> Vec<(String, PathBuf, Option<String>)> {
    let first = match &cfg.repo {
        Some(r) if r.as_os_str().is_empty() => None,
        Some(r) => Some(root.join(r)),
        None => git_root(root),
    };
    let mut all: Vec<(String, PathBuf, Option<String>)> = Vec::new();
    for (name, path, base) in first.map(|p| (String::new(), p, cfg.base.clone())).into_iter().chain(cfg.repos.iter().map(|r| (r.name.clone(), root.join(&r.path), r.base.clone()))) {
        let key = path.canonicalize().unwrap_or_else(|_| path.clone());
        if all.iter().any(|(_, p, _)| p.canonicalize().unwrap_or_else(|_| p.clone()) == key) {
            continue;
        }
        let name = if name.is_empty() { path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into()) } else { name };
        let name = name.replace(':', "-");
        let mut unique = name.clone();
        let mut n = 1;
        while all.iter().any(|(u, _, _)| u.eq_ignore_ascii_case(&unique)) {
            n += 1;
            unique = format!("{name}-{n}");
        }
        all.push((unique, path, base));
    }
    all
}

/// Every repo a room watches, labelled when there is more than one.
pub fn repos_of(cfg: &RoomCfg, root: &Path) -> Vec<(Option<String>, PathBuf, Option<String>)> {
    let all = repos_named(cfg, root);
    let many = all.len() > 1;
    all.into_iter().map(|(name, path, base)| (many.then_some(name), path, base)).collect()
}

#[derive(Default)]
struct Window {
    rows: Vec<(u64, Event)>,
    index: HashMap<String, usize>,
    dropped: u64,
}

impl Window {
    fn get(&self, id: &str) -> Option<&Event> {
        self.index.get(id).map(|&i| &self.rows[i].1)
    }

    fn push(&mut self, e: Event, arrival: u64) {
        if let Some(&i) = self.index.get(&e.id) {
            self.rows[i] = (arrival, e);
            return;
        }
        self.index.insert(e.id.clone(), self.rows.len());
        self.rows.push((arrival, e));
        if self.rows.len() > WINDOW + WINDOW / 4 {
            let cut = self.rows.len() - WINDOW;
            for (arrival, _) in self.rows.drain(..cut) {
                self.dropped = self.dropped.max(arrival);
            }
            self.index = self.rows.iter().enumerate().map(|(i, (_, e))| (e.id.clone(), i)).collect();
        }
    }

    fn events(&self) -> impl DoubleEndedIterator<Item = &Event> {
        self.rows.iter().map(|(_, e)| e)
    }
}

/// An event read this tick: whether it came from the ledger, its stable position, and its byte
/// offset when it came from the ledger.
type Fresh = (Event, bool, u64, Option<u64>);

pub struct Room {
    pub root: PathBuf,
    pub ledger: PathBuf,
    pub cfg: RoomCfg,
    sources: Vec<Box<dyn Source + Send>>,
    chat: Window,
    git_events: Window,
    arrival: u64,
    pub graph: Graph,
    derivers: Vec<Box<dyn Deriver>>,
    pub rev: u64,
    /// Which sessions hold the agents, as last seen; a change is published like an event.
    presence: String,
    pub agents: HashMap<String, CliAgent>,
    /// Agent handles in room.toml order: ties in routing go to the one listed first.
    order: Vec<String>,
    started: HashMap<String, Instant>,
    hop: u32,
    live: bool,
    /// Branch tips for the map, read off the room lock (None without `repo`).
    git: Vec<GitSource>,
    git_seeded: bool,
    /// Failed writes stay in order for the next tick.
    pending_git: VecDeque<Event>,
    /// The mark saved by an earlier run, until the first ingest turns it into this run's mark.
    restored: Option<crate::seen::Seen>,
    github: Vec<crate::github::GitHubWatch>,
    /// The last recorded row for each `gh:` id, so an unchanged pull request is not re-appended.
    gh_rows: HashMap<String, Event>,
    /// Per source: how many ids it has yielded, for stable positions.
    counts: Vec<u64>,
    /// Byte offset just past the last ledger row read, and a fingerprint of every imported
    /// event: what a seen mark records (`seen.rs`).
    ledger_read: u64,
    prints: HashMap<String, u64>,
    relabel: Option<Relabel>,
    /// Each agent's usage state as last published, so a reset that no event marks still updates
    /// the meters.
    usage_shown: String,
    /// An imported file was edited in place, or a file was cut short: replay the room on this tick.
    replay: bool,
    ledger_replaced: bool,
    /// Ledger rows before this offset were routed already (set by a replay).
    route_from: u64,
    trio_exe: String,
    /// When the human was last here, for the catch-up (`seen.rs`).
    pub seen: Option<crate::seen::Seen>,
    /// Latest window reading per agent, and when it was read (`usage.rs`).
    readings: HashMap<String, crate::usage::Reading>,
    read_at: Option<Instant>,
    /// Agents stopped by a usage limit, until the reset their run named.
    stops: HashMap<String, crate::usage::Stop>,
    /// Wakes waiting for their agent to come back.
    held: Vec<Held>,
    /// The message each running agent was woken for, so a limit hit can hand it on.
    triggers: HashMap<String, Event>,
    /// Per message: the agents that have it (woken for it, holding it, or attached when it was
    /// routed). A message reaches each agent once, whatever retries and reroutes follow.
    delivered: HashMap<String, std::collections::BTreeSet<String>>,
    delivered_order: VecDeque<String>,
    /// Cost of every finished run, by event id, so a day's spend survives the chat window.
    spend: HashMap<String, (String, String, f64)>,
}

#[derive(Serialize)]
pub struct MessageView<'a> {
    id: &'a str,
    at: &'a str,
    from: &'a str,
    kind: &'static str,
    summary: String,
    has_detail: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    items: Vec<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    mentions: Vec<&'a str>,
}

impl Room {
    pub fn open(root: &Path) -> io::Result<Self> {
        Self::open_with(root, true)
    }

    /// The room replayed from its files only: no git or GitHub watchers (for the project list).
    pub fn open_read(root: &Path) -> io::Result<Self> {
        Self::open_with(root, false)
    }

    fn open_with(root: &Path, watch: bool) -> io::Result<Self> {
        let root = root.canonicalize()?;
        let dir = root.join(".trio");
        let (cfg, order): (RoomCfg, Vec<String>) = match std::fs::read_to_string(dir.join("room.toml")) {
            Ok(t) => {
                let cfg: RoomCfg = toml::from_str(&t).map_err(io::Error::other)?;
                let order = agent_order(&t, &cfg);
                (cfg, order)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (RoomCfg { hop_budget: 3, ..Default::default() }, Vec::new()),
            Err(e) => return Err(e),
        };
        if watch {
            // `@<handle>` and `@<role>` are mentions in the room this process serves.
            let mut words = order.clone();
            words.extend(order.iter().flat_map(|n| cfg.agents[n].roles()));
            crate::source::add_handles(&words);
        }
        // Prefixes are process-wide: only the room this process serves adds its own. A read-only
        // replay (the project list) must not change which ids the served room recognises.
        if watch {
            crate::model::add_item_prefixes(&cfg.item_prefixes);
        }
        let ledger = dir.join("room.jsonl");
        let mut sources: Vec<Box<dyn Source + Send>> = Vec::new();
        for imp in &cfg.import {
            sources.push(Box::new(PostsFile::new(root.join(&imp.file), &imp.from, &human(&cfg))));
        }
        sources.push(Box::new(RoomLedger::new(ledger.clone())));
        let hues = [("claude", "clay"), ("codex", "moss")];
        let agents = cfg
            .agents
            .iter()
            .map(|(name, c)| {
                let hue = hues.iter().find(|(n, _)| n == name).map(|(_, h)| *h).unwrap_or("dusk");
                (name.clone(), CliAgent::new(name, hue, c.clone()))
            })
            .collect();
        let roster: Vec<(String, Vec<String>)> = order.iter().map(|n| (n.clone(), cfg.agents[n].roles())).collect();
        let trio_exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "trio".into());
        let named = repos_named(&cfg, &root);
        let relabel = named.first().map(|(main, _, _)| Relabel { main: main.clone(), many: named.len() > 1 });
        let git = repos_of(&cfg, &root)
            .into_iter()
            .filter(|_| watch)
            .map(|(label, path, base)| GitSource { watch: crate::gitwatch::GitWatch::start(path, base, label.clone()), label })
            .collect();
        let restored = crate::seen::load(&root);
        // A read-only replay (the project list) never wakes anyone: no agents, nothing held.
        let held: Vec<Held> = if watch { std::fs::read(held_path(&root)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default() } else { Vec::new() };
        let mut delivered: HashMap<String, std::collections::BTreeSet<String>> = HashMap::new();
        for h in &held {
            delivered.entry(h.event.id.clone()).or_default().insert(h.agent.clone());
        }
        let delivered_order: VecDeque<String> = delivered.keys().cloned().collect();
        let agents = if watch { agents } else { HashMap::new() };
        let github = if watch { github_links(&cfg, &root).into_iter().map(crate::github::GitHubWatch::start).collect() } else { Vec::new() };
        Ok(Self {
            root,
            ledger,
            cfg,
            sources,
            chat: Window::default(),
            git_events: Window::default(),
            arrival: 0,
            graph: Graph::with_agents(roster),
            derivers: derivers(),
            rev: 0,
            presence: String::new(),
            agents,
            order,
            started: HashMap::new(),
            hop: 0,
            live: false,
            git,
            git_seeded: false,
            pending_git: VecDeque::new(),
            restored,
            github,
            gh_rows: HashMap::new(),
            counts: Vec::new(),
            ledger_read: 0,
            prints: HashMap::new(),
            relabel,
            usage_shown: String::new(),
            replay: false,
            ledger_replaced: false,
            route_from: 0,
            trio_exe,
            seen: None,
            readings: HashMap::new(),
            read_at: None,
            stops: HashMap::new(),
            held,
            spend: HashMap::new(),
            triggers: HashMap::new(),
            delivered,
            delivered_order,
        })
    }

    fn activity(&self) -> impl Iterator<Item = &(u64, Event)> + Clone {
        self.chat.rows.iter().chain(&self.git_events.rows)
    }

    pub fn catchup(&self, work: &HashMap<String, crate::work::Work>) -> Option<crate::seen::Catchup> {
        let seen = self.seen.as_ref()?;
        let partial = self.chat.dropped.max(self.git_events.dropped) > seen.position;
        let mut c = crate::seen::catchup(seen, &self.graph, work, self.activity(), &self.me(), partial)?;
        c.upto = self.arrival;
        Some(c)
    }

    /// Move the seen mark to now, with every item's current lane, and publish the change.
    pub fn mark_seen(&mut self) -> io::Result<()> {
        self.mark_seen_upto(None).map(|_| ())
    }

    /// Move the seen mark to now, unless rows arrived after `upto` (the catch-up the human
    /// dismissed): then nothing moves and the answer is false, so they see the newer strip.
    pub fn mark_seen_upto(&mut self, upto: Option<u64>) -> io::Result<bool> {
        if upto.is_some_and(|u| u < self.arrival) {
            return Ok(false);
        }
        let s = self.seen_now();
        crate::seen::save(&self.root, &s)?;
        self.seen = Some(s);
        self.rev += 1;
        Ok(true)
    }

    fn seen_now(&self) -> crate::seen::Seen {
        let prints = self.prints.iter().map(|(k, v)| (k.clone(), *v)).collect();
        let mut s = crate::seen::mark(&now(), self.arrival, &crate::work::all(&self.graph, &self.me()), self.ledger_read, prints);
        s.ledger_head = crate::source::head_print(&self.ledger, self.ledger_read);
        s
    }

    /// Whether the ledger still starts as it did at `mark` (it was not replaced or cut).
    fn same_ledger(&self, mark: &crate::seen::Seen) -> bool {
        match (mark.ledger, mark.ledger_head) {
            (Some(n), Some(h)) => crate::source::head_print(&self.ledger, n) == Some(h),
            _ => true,
        }
    }

    fn print_key(&self, i: usize, e: &Event) -> Option<String> {
        self.cfg.import.get(i).map(|imp| format!("{}|{}", imp.file, e.id))
    }

    /// Whether an event read at the first ingest was already there at a saved mark: a ledger row
    /// by its byte offset, an imported post by its content. A mark without offsets (an older
    /// Trio) falls back to its time.
    /// When in doubt an event counts as new: showing a seen row again is a nuisance, hiding a
    /// new one is the failure the catch-up exists to prevent.
    fn before_mark(&self, mark: &crate::seen::Seen, e: &Event, pos: u64, offset: Option<u64>, same_ledger: bool) -> bool {
        let Some(read) = mark.ledger else { return e.at < mark.at };
        if let Some(o) = offset {
            return same_ledger && o < read;
        }
        let i = ((pos >> 40) as usize).saturating_sub(1);
        match self.print_key(i, e) {
            Some(k) => mark.prints.get(&k) == Some(&crate::seen::print(e)),
            None => e.at < mark.at,
        }
    }

    /// The human's handle: `me` from room.toml when usable, else the OS user name.
    pub fn me(&self) -> String {
        human(&self.cfg)
    }

    pub fn name(&self) -> String {
        if self.cfg.name.is_empty() {
            self.root.file_name().and_then(|n| n.to_str()).unwrap_or("room").to_string()
        } else {
            self.cfg.name.clone()
        }
    }

    /// Poll every source, apply new events, route mentions, reap finished runs.
    /// Returns true when anything changed. The first call loads history without waking anyone.
    pub fn tick(&mut self) -> bool {
        let mut changed = self.ingest();
        if std::mem::take(&mut self.replay) {
            changed |= self.replay();
        }
        // A reaped reply was just appended to the ledger. Read and route it now: the loop may
        // sleep after this tick (no page, no live run), and a mention in it must still wake the
        // other agent. Bounded: each pass needs a run to finish, and there are two agents.
        while self.reap() {
            changed = true;
            self.ingest();
        }
        self.refresh_usage();
        if self.release_held() {
            changed = true;
        }
        let usage = ["claude", "codex"].map(|a| serde_json::to_string(&self.usage(a)).unwrap_or_default()).join("|");
        if usage != self.usage_shown {
            self.usage_shown = usage;
            changed = true;
        }
        // Joining or leaving writes no ledger line, so compare the attachment markers too.
        let presence = self.presence();
        if presence != self.presence {
            self.presence = presence;
            changed = true;
        }
        self.live = true;
        if changed {
            self.rev += 1;
        }
        changed
    }

    /// Replay the whole room from its files, as a restart would, keeping what only this process
    /// knows: running agents and their triggers, usage, the hop count, presence. Ledger rows
    /// this process already read were routed then and are not routed again.
    fn replay(&mut self) -> bool {
        let mut fresh = match Room::open_with(&self.root, true) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("trio: could not replay the room: {e}");
                return false;
            }
        };
        std::mem::swap(&mut fresh.agents, &mut self.agents);
        std::mem::swap(&mut fresh.started, &mut self.started);
        std::mem::swap(&mut fresh.triggers, &mut self.triggers);
        let cut = std::mem::take(&mut self.ledger_replaced);
        // Ledger ids are offsets, so a cut ledger reuses them for new rows: its delivery record
        // starts again from the held wakes (which `open_with` read back).
        if !cut {
            std::mem::swap(&mut fresh.delivered, &mut self.delivered);
            std::mem::swap(&mut fresh.delivered_order, &mut self.delivered_order);
        }
        // The operator's pause holds until they lift it, whatever room.toml says.
        fresh.cfg.paused = self.cfg.paused;
        // Arrival numbers keep rising, so a strip drawn before the replay is older than the room.
        fresh.arrival = self.arrival;
        std::mem::swap(&mut fresh.stops, &mut self.stops);
        std::mem::swap(&mut fresh.readings, &mut self.readings);
        fresh.read_at = self.read_at;
        fresh.hop = self.hop;
        fresh.presence = std::mem::take(&mut self.presence);
        fresh.restored = self.seen.take().or_else(|| self.restored.take());
        // Rows read before were routed then. A ledger that was cut or replaced is a new file
        // whose rows this process cannot place: none of them is routed now, and rows written
        // after the replay route as usual.
        fresh.route_from = if cut {
            self.record("trio", Body::Status { text: "room.jsonl was cut or replaced: replayed it without waking anyone".into() });
            u64::MAX
        } else {
            self.ledger_read
        };
        fresh.live = self.live;
        fresh.ingest();
        if cut {
            fresh.route_from = fresh.ledger_read;
            for h in &mut fresh.held {
                h.event.id = format!("{}@cut{}", h.event.id, self.rev);
            }
            for e in fresh.triggers.values_mut() {
                e.id = format!("{}@cut{}", e.id, self.rev);
            }
            fresh.save_held();
            fresh.delivered.clear();
            fresh.delivered_order.clear();
            for h in fresh.held.clone() {
                fresh.deliver(&h.event.id, &h.agent);
            }
            for (agent, e) in fresh.triggers.clone() {
                fresh.deliver(&e.id, &agent);
            }
        }
        fresh.rev = self.rev + 1;
        *self = fresh;
        true
    }

    /// Stable position for an event from source `i`: sources in
    /// config order, then each source's own order. Same-minute events from different sources
    /// therefore order the same way live and on replay; within a file, file order decides.
    fn position(&mut self, i: usize, id: &str) -> u64 {
        if let Some(pos) = self.graph.position(id) {
            return pos;
        }
        if self.counts.len() <= i {
            self.counts.resize(i + 1, 0);
        }
        self.counts[i] += 1;
        ((i as u64 + 1) << 40) | self.counts[i]
    }

    /// Poll every source, apply new events and route new ledger messages.
    fn ingest(&mut self) -> bool {
        let mut fresh: Vec<Fresh> = Vec::new();
        let n = self.sources.len();
        for i in 0..n {
            self.poll_source(i, &mut fresh);
        }
        let mut changed;
        if self.seen.is_none() {
            // Arrival positions start over in each process. With a saved mark, history up to it
            // arrives first and the rest after the new mark, so what came in while Trio was
            // closed is the catch-up. Without one, all history is the baseline.
            let restored = self.restored.take();
            let same = restored.as_ref().is_none_or(|m| self.same_ledger(m));
            let (old, new): (Vec<_>, Vec<_>) = fresh.into_iter().partition(|(e, _, pos, off)| restored.as_ref().is_none_or(|m| self.before_mark(m, e, *pos, *off, same)));
            changed = self.apply_batch(old);
            // The mark this process starts from is the saved one, not what was just read: rows
            // read now but past the saved mark stay unread (a replay, too, starts from it).
            let mut mark = self.seen_now();
            if let Some(m) = restored {
                (mark.at, mark.lanes, mark.prints) = (m.at, m.lanes, m.prints);
                (mark.ledger, mark.ledger_head) = if same { (m.ledger, m.ledger_head) } else { (Some(0), crate::source::head_print(&self.ledger, 0)) };
            }
            self.seen = Some(mark);
            changed |= self.apply_batch(new);
        } else {
            changed = self.apply_batch(fresh);
        }
        // Each watcher is seeded once with the branches replay already knows under its label.
        let seeded = self.git_seeded;
        self.git_seeded = true;
        let mut polls = Vec::new();
        for s in &mut self.git {
            let mine = |id: &str| match (&s.label, id.rsplit_once(':')) {
                (Some(l), Some((p, _))) => p == l,
                (None, None) => true,
                _ => false,
            };
            let seed = (!seeded).then(|| crate::gitwatch::Seed {
                serials: self.graph.git_serials.iter().filter(|(b, _)| mine(b)).map(|(b, n)| (b.clone(), *n)).collect(),
                tips: self.graph.items.values().filter(|b| !b.tip_sha.is_empty() && mine(&b.id))
                    .map(|b| (b.id.clone(), b.tip_sha.clone(), b.title.clone(), self.graph.merged.contains(&b.id))).collect(),
            });
            polls.extend(s.watch.take(seed));
        }
        for p in polls {
            changed |= self.apply_git_poll(p);
        }
        let mut fresh = Vec::new();
        if self.flush_github() {
            self.poll_source(n - 1, &mut fresh);
        }
        self.flush_git(n - 1, &mut fresh);
        changed | self.apply_batch(fresh)
    }

    fn apply_git_poll(&mut self, mut poll: crate::gitwatch::Poll) -> bool {
        // A branch without an agent prefix is the human's.
        let me = self.me();
        for e in poll.durable.iter_mut().chain(poll.tips.iter_mut()).filter(|e| e.from.is_empty()) {
            e.from = me.clone();
        }
        self.pending_git.extend(poll.durable);
        let mut changed = false;
        for e in poll.tips {
            let pos = self.position(self.sources.len(), &e.id);
            changed |= self.apply(e, pos);
        }
        changed
    }

    fn flush_git(&mut self, ledger_index: usize, fresh: &mut Vec<Fresh>) {
        let mut appended = std::collections::HashSet::new();
        while let Some(e) = self.pending_git.front() {
            if !self.graph.has_event(&e.id) && !appended.contains(&e.id) {
                if let Err(err) = append(&self.ledger, e) {
                    eprintln!("trio: could not record {}: {err}", e.id);
                    break;
                }
                appended.insert(e.id.clone());
            }
            self.pending_git.pop_front();
        }
        if !appended.is_empty() {
            self.poll_source(ledger_index, fresh);
        }
    }

    fn apply_batch(&mut self, mut fresh: Vec<Fresh>) -> bool {
        let mut changed = false;
        // A row that reuses an id replaces the earlier one, so within one batch only the last
        // version in source order counts, at the first version's position: the same result as
        // reading the rows one tick at a time, whatever their times say.
        let mut last: HashMap<&str, usize> = HashMap::new();
        for (k, f) in fresh.iter().enumerate() {
            last.insert(f.0.id.as_str(), k);
        }
        if last.len() < fresh.len() {
            let mut first: HashMap<String, u64> = HashMap::new();
            for f in &fresh {
                first.entry(f.0.id.clone()).or_insert(f.2);
            }
            let keep: std::collections::HashSet<usize> = last.into_values().collect();
            fresh = fresh.into_iter().enumerate().filter(|(k, _)| keep.contains(k)).map(|(_, mut f)| {
                f.2 = first[&f.0.id];
                f
            }).collect();
        } else {
            drop(last);
        }
        fresh.sort_by(|a, b| crate::gitwatch::replay_at(&a.0).cmp(crate::gitwatch::replay_at(&b.0)).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)));
        for (e, from_ledger, pos, offset) in fresh {
            let route = self.live && from_ledger && matches!(e.body, Body::Message { .. }) && !self.graph.has_event(&e.id) && offset.is_none_or(|o| o >= self.route_from);
            if route {
                changed |= self.apply(e.clone(), pos);
                self.route(&e);
            } else {
                changed |= self.apply(e, pos);
            }
        }
        changed
    }

    fn poll_source(&mut self, i: usize, fresh: &mut Vec<Fresh>) {
        let from_ledger = i == self.sources.len() - 1;
        let polled = self.sources[i].poll();
        if self.sources[i].rewritten() {
            self.replay = true;
            self.ledger_replaced |= i + 1 == self.sources.len();
        }
        if let Some(end) = self.sources[i].read_to() {
            self.ledger_read = end;
        }
        if let Ok(evs) = polled {
            let offsets = self.sources[i].offsets();
            for (k, mut e) in evs.into_iter().enumerate() {
                if let Some(r) = &self.relabel {
                    r.apply(&mut e);
                }
                let offset = offsets.get(k).copied();
                if let Some(key) = self.print_key(i, &e) {
                    self.prints.insert(key, crate::seen::print(&e));
                }
                let pos = self.position(i, &e.id);
                fresh.push((e, from_ledger, pos, offset));
            }
        }
    }

    /// Record pull-request rows that changed since they were last recorded. True if any were.
    fn flush_github(&mut self) -> bool {
        let mut appended = false;
        for i in 0..self.github.len() {
            for e in self.github[i].take() {
                if self.gh_rows.get(&e.id) == Some(&e) {
                    continue;
                }
                if let Err(err) = append(&self.ledger, &e) {
                    eprintln!("trio: could not record {}: {err}", e.id);
                    return appended;
                }
                self.gh_rows.insert(e.id.clone(), e);
                appended = true;
            }
        }
        appended
    }

    fn apply(&mut self, e: Event, pos: u64) -> bool {
        if e.id.starts_with("gh:") {
            self.gh_rows.insert(e.id.clone(), e.clone());
        }
        if let Body::RunFinished { agent, cost: Some(c), .. } = &e.body {
            self.spend.insert(e.id.clone(), (e.at.get(..10).unwrap_or("").to_string(), agent.clone(), *c));
            if self.spend.len() > 4096 {
                let today = now();
                self.spend.retain(|_, (day, _, _)| today.starts_with(day.as_str()));
            }
        }
        if let Body::Commit { branch, sha, touches, .. } = &e.body
            && crate::gitwatch::is_refresh(&e)
        {
            if self.graph.items.get(branch).is_none_or(|b| b.tip_sha != *sha || b.merged_at.is_some())
                || self.graph.touches.get(branch) == Some(touches)
            {
                return false;
            }
            self.graph.apply_at(&e, Some(pos), &self.derivers);
            return true;
        }
        let window = if matches!(e.body, Body::Commit { .. }) {
            if crate::gitwatch::is_transition(&e) && self.graph.has_event(&e.id) {
                return false;
            }
            &mut self.git_events
        } else {
            &mut self.chat
        };
        if window.get(&e.id) == Some(&e) {
            return false;
        }
        self.graph.apply_at(&e, Some(pos), &self.derivers);
        self.arrival += 1;
        window.push(e, self.arrival);
        true
    }

    /// Append to the ledger (the file is the source of truth; the next tick reads it back).
    pub fn say(&self, from: &str, summary: &str, detail: Option<String>) -> Result<(), String> {
        let summary = summary.trim();
        if summary.is_empty() {
            return Err("empty message".into());
        }
        if summary.chars().count() > SUMMARY_MAX {
            return Err(format!("summary is over {SUMMARY_MAX} chars; put the rest in detail"));
        }
        let text = format!("{summary}\n{}", detail.as_deref().unwrap_or(""));
        let at = now();
        // `claim X` and `verdict X: pass` lines say the same here as in a posts file, with ids in
        // the same shape (`<line>:<item>`), so one line naming several items is one verdict.
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let base = format!("say-{}-{nanos}", std::process::id());
        let facts: Vec<(String, Body)> = text
            .lines()
            .enumerate()
            .flat_map(|(n, l)| crate::source::line_facts(l).into_iter().map(move |(item, b)| (format!("#{n}:{item}"), b)))
            .map(|(tail, b)| (format!("{base}{tail}"), b))
            .collect();
        let e = Event {
            id: String::new(),
            at: at.clone(),
            from: from.to_string(),
            body: Body::Message { summary: summary.to_string(), detail, mentions: mentions_in(&text, &self.me()), items: crate::model::item_ids(&text) },
        };
        append(&self.ledger, &e).map_err(|e| e.to_string())?;
        for (id, body) in facts {
            append(&self.ledger, &Event { id, at: at.clone(), from: from.to_string(), body }).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn record(&self, from: &str, body: Body) {
        let _ = append(&self.ledger, &Event { id: String::new(), at: now(), from: from.to_string(), body });
    }

    fn route(&mut self, e: &Event) {
        let Body::Message { mentions, .. } = &e.body else { return };
        let me = self.me();
        // GitHub rows are read back from a poll: neither a human turn nor an agent hop.
        if !e.id.starts_with("gh:") {
            if e.from == me {
                self.hop = 0;
            } else {
                self.hop += 1;
            }
        }
        let mut targets: Vec<String> = Vec::new();
        for m in mentions {
            if m == &me {
                continue;
            }
            if m == "both" || m == "all" {
                // Everyone but whoever wrote the work: an agent never reviews its own.
                for a in self.order.clone() {
                    if a == e.from || targets.contains(&a) || !self.enabled(&a) {
                        continue;
                    }
                    if self.reviews_own_work(e, &a) {
                        self.record("trio", Body::Status { text: format!("{} wrote it, so @{m} leaves it out of the review", cap(&a)) });
                        continue;
                    }
                    targets.push(a);
                }
            } else if self.agents.contains_key(m) || m == "claude" || m == "codex" {
                targets.push(m.clone());
            } else if self.is_role(m)
                && let Some(a) = self.pick(m, e)
            {
                self.record("trio", Body::Status { text: format!("@{m}: {} takes it", cap(&a)) });
                targets.push(a);
            }
        }
        targets.retain(|t| t != &e.from);
        let mut seen: Vec<String> = Vec::new();
        targets.retain(|t| {
            let first = !seen.contains(t);
            seen.push(t.clone());
            first
        });
        let recent: Vec<String> = self
            .chat.events()
            .rev()
            .filter(|x| matches!(x.body, Body::Message { .. }))
            .take(8)
            .map(|x| format!("{}: {}", x.from, x.summary()))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let detail = match &e.body {
            Body::Message { detail, .. } => detail.as_deref(),
            _ => None,
        };
        for t in &targets {
            self.dispatch(t, e, &recent, detail);
        }
    }

    fn enabled(&self, a: &str) -> bool {
        self.agents.get(a).is_some_and(|x| x.cfg.enabled)
    }

    fn roles_of(&self, a: &str) -> Vec<String> {
        self.agents.get(a).map(|x| x.cfg.roles()).unwrap_or_default()
    }

    fn is_role(&self, w: &str) -> bool {
        self.agents.values().any(|a| a.cfg.roles().iter().any(|r| r == w))
    }

    /// The usage pool `a` draws on: agents on one account are out together.
    fn account_of(&self, a: &str) -> String {
        self.agents.get(a).map(|x| x.cfg.account(a)).unwrap_or_else(|| a.to_string())
    }

    /// The attached session `a` is, if any. A harness's one attached session (its plugin hooks
    /// write `attached/<harness>.json`) stands for the first agent listed on that harness.
    fn attached_to(&self, a: &str) -> Option<crate::hook::Attachment> {
        let h = self.agents.get(a).map(|x| x.cfg.harness(a)).unwrap_or_else(|| crate::harness::of(a, None));
        if !h.attach {
            return None;
        }
        let first = self.order.iter().find(|n| self.agents[n.as_str()].cfg.harness(n).name == h.name).map(String::as_str).unwrap_or(a);
        if first != a {
            return None;
        }
        crate::hook::attached(&self.root, h.name)
    }

    /// Lower is better: available before out, idle before busy, ok before low, then list order.
    fn rank(&self, a: &str) -> (u8, u8, u8, usize) {
        let u = self.usage(a);
        let idle = self.agents.get(a).is_some_and(|x| x.ready());
        (u8::from(u.out()), u8::from(!idle), u8::from(u.state == "low"), self.order.iter().position(|n| n == a).unwrap_or(usize::MAX))
    }

    /// The agent `@<role>` reaches: the best-ranked enabled holder of it that did not write the
    /// message or the work it asks about.
    fn pick(&mut self, role: &str, e: &Event) -> Option<String> {
        let holders: Vec<String> = self.order.iter().filter(|a| self.enabled(a) && self.roles_of(a).iter().any(|r| r == role) && **a != e.from).cloned().collect();
        let mut fit: Vec<String> = holders.iter().filter(|a| !self.reviews_own_work(e, a)).cloned().collect();
        fit.sort_by_key(|a| self.rank(a));
        if fit.is_empty() {
            let why = if holders.is_empty() { "nobody holds it" } else { "its only holders wrote the work" };
            self.record("trio", Body::Status { text: format!("@{role} reaches no one: {why}") });
        }
        fit.into_iter().next()
    }

    /// Who could take `e` in `t`'s place, best first: enabled, available, not attached, not its
    /// author or the author of the work it asks about, and sharing a role with `t` when any
    /// such agent does.
    fn stand_ins(&self, t: &str, e: &Event) -> Vec<String> {
        let mut c: Vec<String> = self
            .order
            .iter()
            .filter(|o| *o != t && **o != e.from && self.enabled(o) && !self.usage(o).out() && self.attached_to(o).is_none() && !self.reviews_own_work(e, o))
            .cloned()
            .collect();
        let mine = self.roles_of(t);
        if c.iter().any(|o| self.roles_of(o).iter().any(|r| mine.contains(r))) {
            c.retain(|o| self.roles_of(o).iter().any(|r| mine.contains(r)));
        }
        c.sort_by_key(|o| self.rank(o));
        c
    }

    /// True when no agent but `t` could ever take `e`: every other one wrote it or its work.
    fn only(&self, t: &str, e: &Event) -> bool {
        !self.order.iter().any(|o| o != t && *o != e.from && !self.reviews_own_work(e, o))
    }

    /// Wake `t` for `e`, by its usage. Out (limit, used-up window or spent budget): the other
    /// agent takes it, with the item history, unless that would have the author review its own
    /// work; then the wake is held until `t` is back. Low: woken anyway, with a warning line.
    fn has(&self, id: &str, agent: &str) -> bool {
        self.delivered.get(id).is_some_and(|s| s.contains(agent))
    }

    fn deliver(&mut self, id: &str, agent: &str) {
        if !self.delivered.contains_key(id) {
            self.delivered_order.push_back(id.to_string());
            if self.delivered_order.len() > 512
                && let Some(k) = self.delivered_order.iter().position(|id| !self.held.iter().any(|h| &h.event.id == id))
                && let Some(old) = self.delivered_order.remove(k)
            {
                self.delivered.remove(&old);
            }
        }
        self.delivered.entry(id.to_string()).or_default().insert(agent.to_string());
    }

    fn undeliver(&mut self, id: &str, agent: &str) {
        if let Some(s) = self.delivered.get_mut(id) {
            s.remove(agent);
        }
    }

    fn dispatch(&mut self, t: &str, e: &Event, recent: &[String], detail: Option<&str>) {
        if self.has(&e.id, t) {
            return; // it reached `t` already (a reroute earlier in this route, or held)
        }
        // Attached agents are pull-on-turn: the mention reaches them with their next message.
        if let Some(a) = self.attached_to(t) {
            self.record("trio", Body::Status { text: format!("{t} is attached: waiting on you in {}", a.surface.label()) });
            self.deliver(&e.id, t);
            return;
        }
        let u = self.usage(t);
        if !u.out() {
            if u.state == "low" {
                self.record("trio", Body::Status { text: clip(&format!("{t} is low ({}); woken anyway", u.reason.as_deref().unwrap_or("")), 120) });
            }
            let d = self.wake(t, e, recent, detail, &[]);
            self.queue_if_busy(t, e, &d, &[]);
            return;
        }
        if e.from != self.me() && self.hop > self.cfg.hop_budget {
            self.record("trio", Body::Status { text: clip(&format!("{t} is {}; not held: over the hop budget ({}/{})", u.why(), self.hop, self.cfg.hop_budget), 120) });
            return;
        }
        let all = self.stand_ins(t, e);
        let other = all.iter().find(|o| !self.has(&e.id, o)).cloned();
        let author = self.order.iter().find(|o| *o != t && **o != e.from && self.enabled(o) && !self.usage(o).out() && self.reviews_own_work(e, o)).cloned();
        match (other.as_deref(), author.as_deref()) {
            (None, _) if !all.is_empty() => {
                self.record("trio", Body::Status { text: clip(&format!("{t} is {}; {} already has this", u.why(), cap(&all[0])), 120) });
            }
            (Some(o), _) => {
                self.record("trio", Body::Status { text: clip(&format!("routed to {}: {} is {}", cap(o), cap(t), u.why()), 120) });
                let note = self.handoff(t, &u, e);
                let d = self.wake(o, e, recent, detail, &note);
                if !self.queue_if_busy(o, e, &d, &note) && matches!(d, WakeDecision::Failed(_)) {
                    self.hold(t, e, &[]);
                    if let Some(h) = self.held.iter_mut().find(|h| h.agent == t && h.event.id == e.id) {
                        h.tried = true;
                    }
                    self.record("trio", Body::Status { text: clip(&format!("held for {}: {} could not take it", cap(t), cap(o)), 120) });
                }
            }
            (None, Some(o)) => {
                self.hold(t, e, &[]);
                self.record("trio", Body::Status { text: clip(&format!("held for {}: {} wrote it, so its review waits until {} is back ({})", cap(t), cap(o), cap(t), u.why()), 120) });
            }
            (None, None) => {
                self.hold(t, e, &[]);
                self.record("trio", Body::Status { text: clip(&format!("held for {}: {} is {}", cap(t), cap(t), u.why()), 120) });
            }
        }
    }

    /// Wake `t` for `e`. Only a run that started counts as delivered.
    fn wake(&mut self, t: &str, e: &Event, recent: &[String], detail: Option<&str>, handoff: &[String]) -> WakeDecision {
        let me = self.me();
        let room = self.root.display().to_string();
        let guard = Guard { paused: self.cfg.paused, hop: self.hop, budget: self.cfg.hop_budget, from_human: e.from == me };
        let mut since = handoff.to_vec();
        since.extend(crate::seen::digest(self.activity(), t));
        let others: Vec<String> = self.order.iter().filter(|o| *o != t).cloned().collect();
        let prompt = wake_prompt(t, &me, &room, &others, recent, &since, detail);
        let decision = match self.agents.get_mut(t) {
            Some(a) => a.wake_if_allowed(&prompt, &guard),
            None => WakeDecision::Disabled,
        };
        match &decision {
            WakeDecision::Started => {
                self.started.insert(t.to_string(), Instant::now());
                self.triggers.insert(t.to_string(), e.clone());
                self.deliver(&e.id, t);
                self.record(&e.from, Body::RunStarted { agent: t.to_string(), hop: self.hop, trigger: e.id.clone() });
            }
            other => self.record("trio", Body::Status { text: clip(&format!("{t} not woken: {other:?}"), 120) }),
        }
        decision
    }

    /// A wake that found `t` mid-run (or its own session busy) waits for that to end. True when
    /// it was queued.
    fn queue_if_busy(&mut self, t: &str, e: &Event, d: &WakeDecision, handoff: &[String]) -> bool {
        if !matches!(d, WakeDecision::Running | WakeDecision::Busy) {
            return false;
        }
        self.hold(t, e, handoff);
        self.record("trio", Body::Status { text: format!("held for {} until its current run ends", cap(t)) });
        true
    }

    fn hold(&mut self, t: &str, e: &Event, handoff: &[String]) {
        self.deliver(&e.id, t);
        if !self.held.iter().any(|h| h.agent == t && h.event.id == e.id) {
            let stay = self.only(t, e);
            self.held.push(Held { agent: t.to_string(), event: e.clone(), handoff: handoff.to_vec(), stay, tried: false });
            if self.held.len() > HELD_MAX {
                let old = self.held.remove(0);
                self.undeliver(&old.event.id, &old.agent);
                self.record("trio", Body::Status { text: format!("dropped the oldest held message for {}: the queue is full", cap(&old.agent)) });
            }
            self.save_held();
        }
    }

    /// A held wake whose agent is out goes to the other agent once that one can take it, by the
    /// same rules as a live reroute. True when it went out.
    fn reroute_held(&mut self, i: usize) -> bool {
        let h = self.held[i].clone();
        if h.stay || h.tried {
            return false;
        }
        let over = h.event.from != self.me() && self.hop > self.cfg.hop_budget;
        if over {
            return false;
        }
        let Some(o) = self.stand_ins(&h.agent, &h.event).into_iter().find(|o| self.agents.get(o.as_str()).is_some_and(|a| a.ready()) && !self.has(&h.event.id, o)) else {
            return false;
        };
        let o = o.as_str();
        let u = self.usage(&h.agent);
        self.record("trio", Body::Status { text: clip(&format!("routed to {}: {} is {}", cap(o), cap(&h.agent), u.why()), 120) });
        // An entry that was itself a reroute goes back to the agent it was for: no note needed.
        let note = if h.handoff.is_empty() { self.handoff(&h.agent, &u, &h.event) } else { Vec::new() };
        let recent = self.recent();
        let detail = h.event.detail().map(str::to_string);
        if !matches!(self.wake(o, &h.event, &recent, detail.as_deref(), &note), WakeDecision::Started) {
            self.held[i].tried = true;
            return false;
        }
        self.undeliver(&h.event.id, &h.agent);
        self.held.retain(|x| !(x.agent == h.agent && x.event.id == h.event.id));
        self.save_held();
        true
    }

    /// Held wakes outlive a restart: write them through on every change.
    fn save_held(&self) {
        let path = held_path(&self.root);
        if self.held.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let tmp = path.with_extension("json.tmp");
        let ok = serde_json::to_vec(&self.held).ok().is_some_and(|b| std::fs::write(&tmp, b).is_ok()) && std::fs::rename(&tmp, &path).is_ok();
        if !ok {
            eprintln!("trio: could not save held wakes");
        }
    }

    /// Wakes waiting for an agent to come back (the server keeps ticking while there are any).
    pub fn has_held(&self) -> bool {
        !self.held.is_empty()
    }

    /// Wake held agents that are back, oldest first and one at a time per agent: the next waits
    /// until that run ends. A held wake leaves the queue only when its run starts, or when it can
    /// never start (disabled, over the hop budget, failed to spawn); while the room is paused,
    /// the agent is busy, or there is no runtime to run it on, it stays. True when any changed.
    fn release_held(&mut self) -> bool {
        if self.cfg.paused || tokio::runtime::Handle::try_current().is_err() {
            return false;
        }
        let mut changed = false;
        let mut waiting: Vec<String> = Vec::new();
        for h in &self.held {
            if !waiting.contains(&h.agent) {
                waiting.push(h.agent.clone());
            }
        }
        for agent in waiting {
            let agent = agent.as_str();
            let Some(i) = self.held.iter().position(|h| h.agent == agent) else { continue };
            if let Some(a) = self.attached_to(agent) {
                // An attached session pulls unread lines on its next turn: nothing to spawn.
                let n = self.held.iter().filter(|h| h.agent == agent).count();
                self.held.retain(|h| h.agent != agent);
                self.save_held();
                self.record("trio", Body::Status { text: format!("{} is attached: {n} held message(s) wait for you in {}", cap(agent), a.surface.label()) });
                changed = true;
                continue;
            }
            if !self.agents.get(agent).is_some_and(|a| a.cfg.enabled) {
                let gone: Vec<String> = self.held.iter().filter(|h| h.agent == agent).map(|h| h.event.id.clone()).collect();
                for id in &gone {
                    self.undeliver(id, agent);
                }
                self.held.retain(|h| h.agent != agent);
                self.save_held();
                self.record("trio", Body::Status { text: format!("dropped {} held message(s) for {}: it is turned off", gone.len(), cap(agent)) });
                changed = true;
                continue;
            }
            if self.usage(agent).out() {
                let mine: Vec<usize> = (i..self.held.len()).filter(|&k| self.held[k].agent == agent).collect();
                for k in mine {
                    if self.reroute_held(k) {
                        changed = true;
                        break;
                    }
                }
                continue;
            }
            if !self.agents.get(agent).is_some_and(|a| a.ready()) {
                continue;
            }
            let h = self.held[i].clone();
            let recent: Vec<String> = self.recent();
            self.record("trio", Body::Status { text: format!("{} is back: waking it for the held message", cap(&h.agent)) });
            let detail = h.event.detail().map(str::to_string);
            let decision = self.wake(&h.agent, &h.event, &recent, detail.as_deref(), &h.handoff);
            if matches!(decision, WakeDecision::Running | WakeDecision::Busy | WakeDecision::Paused) {
                continue;
            }
            if !matches!(decision, WakeDecision::Started) {
                self.undeliver(&h.event.id, &h.agent);
            }
            self.held.retain(|x| !(x.agent == h.agent && x.event.id == h.event.id));
            self.save_held();
            changed = true;
        }
        changed
    }

    /// The newest eight chat lines, oldest first, for a wake prompt.
    fn recent(&self) -> Vec<String> {
        let mut v: Vec<String> = self.chat.events().rev().filter(|x| matches!(x.body, Body::Message { .. })).take(8).map(|x| format!("{}: {}", x.from, x.summary())).collect();
        v.reverse();
        v
    }

    /// True when `e` asks for a review of an item that `author` wrote.
    fn reviews_own_work(&self, e: &Event, author: &str) -> bool {
        let Body::Message { items, .. } = &e.body else { return false };
        items.iter().any(|id| {
            self.graph.items.get(id).is_some_and(|it| it.review_request(&e.id))
                && crate::work::author_of(&self.graph, id).as_deref() == Some(author)
        })
    }

    /// For a rerouted wake: who it was for, why they are out, and each item's story so far.
    fn handoff(&self, t: &str, u: &crate::usage::Usage, e: &Event) -> Vec<String> {
        let mut out = vec![format!("This message was for {}, who is {}. You are taking it instead.", cap(t), u.why())];
        if let Body::Message { items, .. } = &e.body {
            let work = crate::work::all(&self.graph, &self.me());
            for id in items.iter().take(3) {
                if let (Some(it), Some(w)) = (self.graph.items.get(id), work.get(id)) {
                    out.push(clip(&format!("{id} ({}): {}", it.title, w.story), 400));
                }
            }
        }
        out
    }

    /// Re-read usage sources at most every READ_EVERY (two small reads, off the hot path).
    fn refresh_usage(&mut self) {
        if cfg!(test) || self.read_at.is_some_and(|t| t.elapsed() < READ_EVERY) {
            return;
        }
        self.read_at = Some(Instant::now());
        // Each harness reads its own account's windows; an agent on a named account of its own
        // (another subscription or key) has no reading, only the limits its runs report.
        let mut read: Vec<&'static str> = Vec::new();
        let harnesses: Vec<&'static crate::harness::Preset> = self.order.iter().map(|n| self.agents[n.as_str()].cfg.harness(n)).collect();
        for h in harnesses {
            if read.contains(&h.name) {
                continue;
            }
            read.push(h.name);
            if let Some(r) = h.usage.and_then(|f| f()) {
                self.readings.insert(h.name.to_string(), r);
            }
        }
    }

    /// Dollars this room's runs of `agent` cost today (estimated for Codex).
    fn spent_today(&self, agent: &str) -> f64 {
        let today = &now()[..10];
        self.spend.values().filter(|(day, a, _)| day == today && a == agent).map(|(_, _, c)| c).sum()
    }

    /// `agent`'s availability now: ok, low or out, with why and until when.
    pub fn usage(&self, agent: &str) -> crate::usage::Usage {
        let account = self.account_of(agent);
        let cfg = self.agents.get(agent).map(|a| &a.cfg);
        let budget = cfg.and_then(|c| c.usd_day).or_else(|| self.cfg.budget.of(cfg.map(|c| c.harness(agent).name).unwrap_or(agent)));
        crate::usage::assess(self.readings.get(&account), self.stops.get(&account), self.spent_today(agent), budget, crate::usage::epoch_now())
    }

    /// The room's agents in room.toml order.
    pub fn agent_names(&self) -> &[String] {
        &self.order
    }

    /// Wakes held, per agent, for the snapshot.
    pub fn held_for(&self, agent: &str) -> usize {
        self.held.iter().filter(|h| h.agent == agent).count()
    }

    fn reap(&mut self) -> bool {
        let mut changed = false;
        let names: Vec<String> = self.agents.keys().cloned().collect();
        for n in names {
            let a = self.agents.get_mut(&n).expect("agent");
            let secs = self.started.get(&n).map(|t| t.elapsed().as_secs()).unwrap_or(0);
            if a.is_running() && secs > 20 * 60 {
                a.kill();
            }
            if let Some(r) = a.reap() {
                self.started.remove(&n);
                let trigger = self.triggers.remove(&n);
                let stopped = r.stop.is_some();
                if let (true, Some(e)) = (stopped, &trigger) {
                    self.undeliver(&e.id, &n); // it never answered: the message is not delivered
                }
                self.record(&n, Body::RunFinished { agent: n.clone(), exit: r.exit, secs, cost: r.cost });
                // A usage limit: the agent is out until its reset, and the message it was woken
                // for goes on by the same rules as a fresh one (rerouted or held).
                if let Some(stop) = r.stop.clone() {
                    let account = self.account_of(&n);
                    self.stops.insert(account, stop);
                    let u = self.usage(&n);
                    self.record("trio", Body::Status { text: clip(&format!("{n} hit a usage limit: {}", u.why()), 120) });
                    if let Some(e) = trigger {
                        let recent = self.recent();
                        let detail = e.detail().map(str::to_string);
                        self.dispatch(&n, &e, &recent, detail.as_deref());
                    }
                    changed = true;
                    continue;
                }
                // Only a successful, complete final answer becomes the agent's chat message (and
                // can wake the next agent). Anything else is a status line, never a message.
                match r.reply.as_deref().and_then(split_reply) {
                    Some((summary, detail)) => {
                        if let Err(err) = self.say(&n, &summary, detail) {
                            self.record("trio", Body::Status { text: clip(&format!("{n} reply dropped: {err}"), 120) });
                        }
                    }
                    None => {
                        let why = r.problem.unwrap_or_else(|| "finished without a reply".into());
                        self.record("trio", Body::Status { text: clip(&format!("{n} {why}"), 120) });
                    }
                }
                changed = true;
            }
        }
        changed
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.cfg.paused = paused;
        self.rev += 1;
    }

    pub fn detail(&self, id: &str) -> Option<&str> {
        self.chat.get(id).and_then(Event::detail)
    }

    /// The chat view: the newest `limit` messages and run events, oldest first.
    pub fn messages(&self, limit: usize) -> Vec<MessageView<'_>> {
        let mut out: Vec<&Event> = self.chat.events().filter(|e| match &e.body {
            Body::Claim { .. } | Body::Verdict { .. } => false,
            Body::Status { text } if text.is_empty() && e.id.ends_with("#status") => false,
            _ => true,
        }).collect();
        out.sort_by(|a, b| a.at.cmp(&b.at));
        let skip = out.len().saturating_sub(limit);
        out[skip..]
            .iter()
            .map(|e| MessageView {
                id: &e.id,
                at: &e.at,
                from: &e.from,
                kind: crate::model::Entity::kind(*e),
                summary: match &e.body {
                    Body::RunStarted { agent, hop, .. } => format!("woke {agent} (hop {hop})"),
                    Body::RunFinished { agent, exit, secs, cost } => match cost {
                        Some(c) => format!("{agent} finished in {secs}s (exit {exit}, ${c:.2})"),
                        None => format!("{agent} finished in {secs}s (exit {exit})"),
                    },
                    _ => e.summary(),
                },
                has_detail: e.detail().is_some_and(|d| !d.trim().is_empty()),
                items: match &e.body {
                    Body::Message { items, .. } => items.iter().map(String::as_str).collect(),
                    _ => Vec::new(),
                },
                mentions: match &e.body {
                    Body::Message { mentions, .. } => mentions.iter().map(String::as_str).collect(),
                    _ => Vec::new(),
                },
            })
            .collect()
    }

    fn presence(&self) -> String {
        ["claude", "codex"]
            .iter()
            .map(|n| crate::hook::attached(&self.root, n).and_then(|a| serde_json::to_string(&a).ok()).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("|")
    }

    /// Each agent for the clients: state, how to draw it (`glyph`: Claude a circle, Codex a
    /// triangle, the rest by list position), what it runs on, its roles, and its list position.
    pub fn agent_states(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        let mut spare = crate::harness::GLYPHS.iter();
        for (i, n) in self.order.iter().enumerate() {
            let a = &self.agents[n.as_str()];
            let glyph = match n.as_str() {
                "claude" => "claude",
                "codex" => "codex",
                _ => spare.next().copied().unwrap_or("dot"),
            };
            let h = a.cfg.harness(n);
            m.insert(n.clone(), serde_json::json!({
                "enabled": a.cfg.enabled, "running": a.is_running(), "hue": crate::agent::Participant::hue(a),
                "glyph": glyph, "harness": h.label, "model": a.cfg.model, "roles": a.cfg.roles(), "order": i,
            }));
        }
        // Attached sessions show up whether or not a headless agent is configured.
        for n in ["claude", "codex"] {
            let attached = crate::hook::attached(&self.root, n).map(|a| a.surface);
            let entry = m.entry(n.to_string()).or_insert_with(|| serde_json::json!({ "enabled": false, "running": false, "hue": "dusk", "glyph": n, "roles": crate::agent::DEFAULT_ROLES, "order": 1000 }));
            entry["attached"] = serde_json::json!(attached);
        }
        serde_json::Value::Object(m)
    }

    pub fn any_running(&self) -> bool {
        self.agents.values().any(|a| a.is_running())
    }

    pub fn hop(&self) -> u32 {
        self.hop
    }
}

/// The room's human handle. Agent names and `trio` are refused so routing stays unambiguous.
/// The top of the git work tree `dir` is in, if any.
pub fn git_root(dir: &Path) -> Option<PathBuf> {
    crate::gitwatch::git(dir, &["rev-parse", "--show-toplevel"]).filter(|s| !s.is_empty()).map(PathBuf::from)
}

pub fn human(cfg: &RoomCfg) -> String {
    let clean = |s: &str| -> Option<String> {
        let s: String = s.trim().to_ascii_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        let taken = cfg.agents.contains_key(&s) || cfg.agents.values().any(|a| a.roles().contains(&s));
        (!s.is_empty() && !taken && !["claude", "codex", "both", "all", "trio", "github"].contains(&s.as_str())).then_some(s)
    };
    clean(&cfg.me)
        .or_else(|| std::env::var("USERNAME").ok().or_else(|| std::env::var("USER").ok()).and_then(|u| clean(&u)))
        .unwrap_or_else(|| "you".into())
}

/// Local wall-clock time as `YYYY-MM-DDTHH:MM`, matching the thread headers.
pub fn now() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_room(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("trio-room-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(root.join(".trio")).unwrap();
        root
    }

    fn message(id: &str, at: &str, from: &str, summary: &str) -> Event {
        Event { id: id.into(), at: at.into(), from: from.into(), body: Body::Message {
            summary: summary.into(), detail: None, mentions: vec![], items: crate::model::item_ids(summary),
        } }
    }

    /// A chat line as someone other than the operator would write it, mentions parsed.
    fn said(at: &str, from: &str, text: &str) -> Event {
        let mut e = message("", at, from, text);
        if let Body::Message { mentions, .. } = &mut e.body {
            *mentions = mentions_in(text, "sanjith");
        }
        e
    }

    fn commit(id: &str, at: &str, branch: &str, sha: &str, subject: &str, merged: bool, gone: bool) -> Event {
        Event { id: id.into(), at: at.into(), from: "claude".into(), body: Body::Commit {
            branch: branch.into(), sha: sha.into(), subject: subject.into(), touches: vec![], merged, gone,
        } }
    }

    #[test]
    fn a_growing_import_retracts_its_ask_live_and_on_cold_replay() {
        use std::io::Write;
        let root = temp_room("grow");
        std::fs::write(root.join(".trio/room.toml"), "me = 'sanjith'\n[[import]]\nfile = 'claude-posts.md'\nfrom = 'claude'\n").unwrap();
        let file = root.join("claude-posts.md");
        std::fs::write(&file, "## 2026-11-23 09:00 @codex please review XF1\nStatus: awaiting review\n").unwrap();
        let mut live = Room::open(&root).unwrap();
        live.tick();
        assert_eq!(crate::work::all(&live.graph, "sanjith")["XF1"].lane, "review");
        assert_eq!(live.graph.now["claude"].text, "awaiting review");
        writeln!(std::fs::OpenOptions::new().append(true).open(file).unwrap(), "verdict XF2: pass").unwrap();
        live.tick();
        let mut cold = Room::open(&root).unwrap();
        cold.tick();
        for room in [&live, &cold] {
            let work = crate::work::all(&room.graph, "sanjith");
            assert_eq!((work["XF1"].lane, work["XF2"].lane), ("asked", "done"));
            assert!(!room.graph.now.contains_key("claude"));
        }
        assert_eq!(serde_json::to_value(&live.graph).unwrap(), serde_json::to_value(&cold.graph).unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_seen_mark_keeps_same_minute_and_backdated_arrivals() {
        let root = temp_room("arrival");
        let mut room = Room::open(&root).unwrap();
        room.tick();
        room.mark_seen().unwrap();
        let at = room.seen.as_ref().unwrap().at.clone();
        append(&room.ledger, &message("same", &at, "claude", "XF1 just arrived")).unwrap();
        append(&room.ledger, &message("late", "2026-01-01T09:00", "claude", "XF2 was imported late")).unwrap();
        room.tick();
        let work = crate::work::all(&room.graph, &room.me());
        let c = room.catchup(&work).unwrap();
        assert_eq!((c.messages, c.moved.len(), c.partial), (2, 2, false));
        assert_eq!(room.seen.as_ref().unwrap().position, 0, "ingest cannot advance the mark");
        room.mark_seen().unwrap();
        assert!(room.catchup(&work).is_none());
        let mut cold = Room::open(&root).unwrap();
        cold.tick();
        assert_eq!(cold.seen.as_ref().unwrap().position, 2);
        assert!(cold.catchup(&work).is_none(), "a restart keeps the saved mark");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn work_done_while_trio_was_closed_is_the_catch_up() {
        let root = temp_room("away");
        let mut room = Room::open(&root).unwrap();
        append(&room.ledger, &message("before", "2026-11-23T09:00", "claude", "XF1 started")).unwrap();
        room.tick();
        room.mark_seen().unwrap();
        drop(room);
        // Trio is closed; an agent keeps working, one row backdated before the mark's time.
        let ledger = root.join(".trio").join("room.jsonl");
        append(&ledger, &message("away1", "2026-11-23T08:00", "codex", "XF2 opened while you were out")).unwrap();
        append(&ledger, &message("away2", "2099-01-01T09:00", "claude", "XF1 ready for review")).unwrap();
        let mut back = Room::open(&root).unwrap();
        back.tick();
        let work = crate::work::all(&back.graph, &back.me());
        let c = back.catchup(&work).expect("what happened while Trio was closed");
        assert_eq!(c.messages, 2, "both rows written while closed, whatever their times");
        assert!(c.moved.iter().any(|m| m.id == "XF2"), "{c:?}");
        back.mark_seen().unwrap();
        drop(back);
        let mut again = Room::open(&root).unwrap();
        again.tick();
        assert!(again.catchup(&crate::work::all(&again.graph, &again.me())).is_none(), "nothing new since the last mark");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn git_churn_keeps_chat_and_runs_and_marks_catchup_partial() {
        let root = temp_room("windows");
        let mut room = Room::open(&root).unwrap();
        room.tick();
        let at = "2026-11-23T09:00";
        room.apply(message("chat", at, "claude", "still here"), 1);
        room.apply(Event { id: "run".into(), at: at.into(), from: "claude".into(),
            body: Body::RunStarted { agent: "codex".into(), hop: 1, trigger: "chat".into() },
        }, 2);
        room.mark_seen().unwrap();
        for i in 1..=WINDOW + WINDOW / 4 + 1 {
            room.apply(commit(&format!("git:claude/x:tip:{i}"), at, "claude/x", &i.to_string(), "work", false, false), i as u64 + 2);
        }
        assert_eq!(room.messages(10).iter().map(|m| m.id).collect::<Vec<_>>(), ["chat", "run"]);
        let c = room.catchup(&crate::work::all(&room.graph, &room.me())).unwrap();
        assert_eq!((c.commits, c.branches, c.messages, c.partial), (WINDOW, 1, 0, true));
        assert_eq!(crate::seen::digest(room.activity(), "codex").len(), 6, "the run survived to anchor the digest");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn two_durable_merge_rows_use_the_second_sha_and_clock_on_replay() {
        let root = temp_room("second-merge");
        let mut room = Room::open(&root).unwrap();
        let ledger = room.ledger.clone();
        append(&ledger, &commit("git:claude/x:merged:old", "2026-11-23T09:00", "claude/x", "old", "XF1 work", true, false)).unwrap();
        append(&ledger, &Event { id: "changes".into(), at: "2026-11-23T10:00".into(), from: "codex".into(),
            body: Body::Verdict { item: "XF1".into(), outcome: "changes".into() },
        }).unwrap();
        append(&ledger, &commit("git:claude/x:merged:new", "2026-11-23T12:00", "claude/x", "new", "XF1 more", true, false)).unwrap();
        room.tick();
        assert_eq!(room.graph.items["claude/x"].merged_at.as_ref().unwrap().0, "2026-11-23T12:00");
        assert_eq!(crate::work::all(&room.graph, &room.me())["XF1"].lane, "done");
        let _ = std::fs::remove_dir_all(root);
    }

    fn git(root: &Path, args: &[&str]) -> String {
        crate::gitwatch::git(root, args).unwrap_or_else(|| panic!("git {args:?}"))
    }

    fn git_room(name: &str) -> PathBuf {
        let root = temp_room(name);
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        git(&root, &["commit", "--allow-empty", "-qm", "base"]);
        let repo = toml::Value::String(root.display().to_string());
        std::fs::write(root.join(".trio/room.toml"), format!("repo = {repo}\nme = 'sanjith'\n")).unwrap();
        root
    }

    fn poll_git(room: &mut Room) -> Vec<Event> {
        let poll = room.git[0].watch.wait(Duration::from_secs(20)).unwrap();
        let durable = poll.durable.clone();
        room.apply_git_poll(poll);
        room.tick();
        durable
    }

    #[test]
    fn a_room_over_two_repos_labels_their_branches() {
        let api = git_room("repo-api");
        let web = git_room("repo-web");
        for (r, subject) in [(&api, "XF1 endpoint"), (&web, "Login form")] {
            git(r, &["checkout", "-qb", "feature/login"]);
            git(r, &["commit", "--allow-empty", "-qm", subject]);
        }
        let room = temp_room("two-repos");
        let path = |p: &Path| toml::Value::String(p.display().to_string());
        std::fs::write(room.join(".trio/room.toml"), format!(
            "me = 'sanjith'\nrepo = ''\n[[repos]]\npath = {}\nname = 'api'\n[[repos]]\npath = {}\nname = 'web'\n[[repos]]\npath = {}\n",
            path(&api), path(&web), path(&web),
        )).unwrap();
        let mut r = Room::open(&room).unwrap();
        assert_eq!(r.git.iter().map(|s| s.label.clone().unwrap()).collect::<Vec<_>>(), ["api", "web"], "the same repo twice is watched once");
        r.tick();
        for i in 0..2 {
            let poll = r.git[i].watch.wait(Duration::from_secs(20)).unwrap();
            r.apply_git_poll(poll);
        }
        r.tick();
        let work = crate::work::all(&r.graph, &r.me());
        assert_eq!(r.graph.items["web:feature/login"].title, "Login form");
        assert!(work.contains_key("web:feature/login"), "an unlinked branch is its own card");
        assert!(!work.contains_key("api:feature/login"), "it implements XF1");
        assert_eq!(work["XF1"].lane, "progress");
        assert_eq!(r.graph.items["api:feature/login"].owner, "sanjith", "a branch without an agent prefix is the person's");
        // A single repo keeps plain names.
        assert_eq!(repos_of(&toml::from_str(&format!("repo = {}", path(&api))).unwrap(), &room)[0].0, None);
        for d in [api, web, room] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn claims_and_verdicts_said_in_chat_count() {
        let root = temp_room("said");
        std::fs::write(root.join(".trio/room.toml"), "me = 'sanjith'\nrepo = ''\n").unwrap();
        let mut room = Room::open(&root).unwrap();
        room.say("claude", "claim XF7", None).unwrap();
        room.tick();
        assert_eq!(crate::work::all(&room.graph, "sanjith")["XF7"].lane, "progress");
        room.say("codex", "XF7 looked at", Some("verdict XF7: pass, all good".into())).unwrap();
        room.tick();
        assert_eq!(crate::work::all(&room.graph, "sanjith")["XF7"].lane, "done");
        room.mark_seen().unwrap();
        room.say("codex", "XF8 and XF9 checked", Some("verdict XF8: pass, XF9 too".into())).unwrap();
        room.tick();
        let work = crate::work::all(&room.graph, "sanjith");
        assert_eq!((work["XF8"].lane, work["XF9"].lane), ("done", "done"));
        assert_eq!(room.catchup(&work).unwrap().verdicts, 1, "one verdict line naming two items is one verdict");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn listing_a_room_never_changes_the_served_rooms_ids() {
        let root = temp_room("prefix-read");
        std::fs::write(root.join(".trio/room.toml"), "me = 'sanjith'\nrepo = ''\nitem_prefixes = ['QZW']\n").unwrap();
        let _ = Room::open_read(&root).unwrap();
        assert!(crate::model::item_ids("QZW-4 done").is_empty(), "a read-only replay adds no prefixes");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_pull_request_moves_its_branch_through_review() {
        use crate::github::{Link, Login, Pr, Requested, Review};
        let root = temp_room("pull");
        std::fs::write(root.join(".trio/room.toml"), "me = 'sanjith'\nrepo = ''\n").unwrap();
        let link = Link { repo: "o/r".into(), label: None, remote_only: true, me: "sanjith".into(), my_login: Some("ana".into()), agents: Vec::new() };
        let mut pr = Pr {
            number: 7, title: "Rate limit the login".into(), head_ref_name: "feature/limit".into(), head_ref_oid: "abc".into(),
            state: "OPEN".into(), is_draft: false, created_at: "2026-11-24T15:00:00Z".into(), updated_at: "2026-11-24T15:00:00Z".into(),
            merged_at: None, url: String::new(), author: Some(Login { login: "bo".into() }),
            review_requests: vec![Requested { login: Some("ana".into()), name: None }], latest_reviews: vec![],
        };
        let mut room = Room::open(&root).unwrap();
        let push = |room: &mut Room, pr: &Pr| {
            for e in link.events(pr) {
                if room.gh_rows.get(&e.id) != Some(&e) {
                    append(&room.ledger, &e).unwrap();
                }
            }
            room.tick();
            crate::work::all(&room.graph, &room.me())["feature/limit"].clone()
        };
        let w = push(&mut room, &pr);
        assert_eq!((w.lane, w.waiting_on.as_deref()), ("review", Some("sanjith")), "my login was asked: waiting on you");
        assert_eq!(room.graph.items["feature/limit"].title, "Rate limit the login");
        pr.latest_reviews.push(Review { author: Some(Login { login: "ana".into() }), state: "APPROVED".into(), submitted_at: "2026-11-24T16:00:00Z".into() });
        assert_eq!(push(&mut room, &pr).lane, "done");
        pr.state = "MERGED".into();
        pr.merged_at = Some("2026-11-24T17:00:00Z".into());
        assert_eq!(push(&mut room, &pr).lane, "done");
        let rows = ledger_rows(&room).len();
        push(&mut room, &pr);
        assert_eq!(ledger_rows(&room).len(), rows, "an unchanged pull request is not recorded again");
        drop(room);
        let mut cold = Room::open(&root).unwrap();
        cold.tick();
        assert_eq!(crate::work::all(&cold.graph, &cold.me())["feature/limit"].lane, "done", "replay agrees");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_reopened_remote_pull_request_replays_as_the_live_room_shows_it() {
        use crate::github::{Link, Login, Pr};
        let root = temp_room("reopen");
        std::fs::write(root.join(".trio/room.toml"), "me = 'sanjith'\nrepo = ''\n").unwrap();
        let link = Link { repo: "o/r".into(), label: None, remote_only: true, me: "sanjith".into(), my_login: None, agents: Vec::new() };
        let mut pr = Pr {
            number: 12, title: "Add the login form".into(), head_ref_name: "feature/login".into(), head_ref_oid: "abc".into(),
            state: "OPEN".into(), is_draft: false, created_at: "2026-11-24T15:00:00Z".into(), updated_at: "2026-11-24T15:00:00Z".into(),
            merged_at: None, url: String::new(), author: Some(Login { login: "bo".into() }), review_requests: vec![], latest_reviews: vec![],
        };
        let mut room = Room::open(&root).unwrap();
        let push = |room: &mut Room, pr: &Pr| {
            for e in link.events(pr) {
                if room.gh_rows.get(&e.id) != Some(&e) {
                    append(&room.ledger, &e).unwrap();
                }
            }
            room.tick();
            room.graph.items.contains_key("feature/login")
        };
        assert!(push(&mut room, &pr));
        pr.state = "CLOSED".into();
        pr.updated_at = "2026-11-24T16:00:00Z".into();
        assert!(!push(&mut room, &pr), "closing retires the branch");
        pr.state = "OPEN".into();
        pr.updated_at = "2026-11-24T17:00:00Z".into();
        assert!(push(&mut room, &pr), "reopening brings it back");
        let live = crate::work::all(&room.graph, &room.me())["feature/login"].lane.to_string();
        drop(room);
        let mut cold = Room::open(&root).unwrap();
        cold.tick();
        let w = crate::work::all(&cold.graph, &cold.me());
        assert_eq!(w.get("feature/login").map(|w| w.lane.to_string()), Some(live), "the stale close row does not delete it on replay");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_import_continued_while_closed_is_the_catch_up() {
        let root = temp_room("grow-away");
        std::fs::write(root.join(".trio/room.toml"), "me = 'sanjith'\nrepo = ''\n[[import]]\nfile = 'claude-posts.md'\nfrom = 'claude'\n").unwrap();
        let file = root.join("claude-posts.md");
        std::fs::write(&file, "## 2026-11-23 09:00 XF1 started\nStatus: working on XF1\n").unwrap();
        let mut room = Room::open(&root).unwrap();
        room.tick();
        room.mark_seen().unwrap();
        drop(room);
        // Trio is closed: the last post goes on past its status, and a second post follows.
        std::fs::write(&file, "## 2026-11-23 09:00 XF1 started\nStatus: working on XF1\nverdict XF2: pass\n## 2026-11-23 09:30 XF1 ready for review\n").unwrap();
        let mut back = Room::open(&root).unwrap();
        back.tick();
        let c = back.catchup(&crate::work::all(&back.graph, "sanjith")).expect("the continued post and the new one");
        assert_eq!((c.messages, c.verdicts), (2, 1), "{c:?}");
        assert!(!back.graph.now.contains_key("claude"), "the status is no longer the last line");
        back.mark_seen().unwrap();
        drop(back);
        let mut again = Room::open(&root).unwrap();
        again.tick();
        assert!(again.catchup(&crate::work::all(&again.graph, "sanjith")).is_none(), "nothing new since the last mark");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rows_that_reuse_an_id_never_resurface_after_a_restart() {
        let root = temp_room("reuse");
        let ledger = root.join(".trio/room.jsonl");
        let mut room = Room::open(&root).unwrap();
        append(&ledger, &message("gh:o/r#1", "2026-11-23T09:00", "bo", "PR #1 is a draft")).unwrap();
        room.tick();
        append(&ledger, &message("gh:o/r#1", "2026-11-23T09:00", "bo", "PR #1 is ready for review")).unwrap();
        append(&ledger, &message("", "2026-11-23T09:05", "codex", "XF3 noted")).unwrap();
        room.tick();
        room.mark_seen().unwrap();
        drop(room);
        let mut back = Room::open(&root).unwrap();
        back.tick();
        assert!(back.catchup(&crate::work::all(&back.graph, "sanjith")).is_none(), "nothing arrived while closed");
        assert_eq!(back.chat.get("gh:o/r#1").map(|e| e.summary()), Some("PR #1 is ready for review".into()), "the newer row wins on replay");
        drop(back);
        append(&ledger, &message("", "2026-11-23T08:00", "codex", "XF4 written while you were out")).unwrap();
        let mut again = Room::open(&root).unwrap();
        again.tick();
        let c = again.catchup(&crate::work::all(&again.graph, "sanjith")).expect("one row arrived");
        assert_eq!(c.messages, 1, "{c:?}");
        assert!(!again.mark_seen_upto(Some(c.upto - 1)).unwrap(), "a strip older than the room does not move the mark");
        assert!(again.mark_seen_upto(Some(c.upto)).unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_import_edited_in_place_replays_to_the_cold_result() {
        let root = temp_room("edit");
        std::fs::write(root.join(".trio/room.toml"), "me = 'sanjith'\nrepo = ''\n[[import]]\nfile = 'claude-posts.md'\nfrom = 'claude'\n").unwrap();
        let file = root.join("claude-posts.md");
        std::fs::write(&file, "## 2026-11-23 09:00 XF1 please review\nStatus: waiting\n## 2026-11-23 09:10 XF2 started\n").unwrap();
        let mut live = Room::open(&root).unwrap();
        live.tick();
        assert_eq!(crate::work::all(&live.graph, "sanjith")["XF1"].lane, "review");
        // The same length, in the middle of the file.
        std::fs::write(&file, "## 2026-11-23 09:00 XF1 all done here\nStatus: waiting\n## 2026-11-23 09:10 XF2 started\n").unwrap();
        let later = std::time::SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(later).unwrap();
        live.tick();
        let mut cold = Room::open(&root).unwrap();
        cold.tick();
        for room in [&live, &cold] {
            assert_eq!(crate::work::all(&room.graph, "sanjith")["XF1"].lane, "asked");
        }
        // The edited post is applied after the mark, so edges may be listed in another order.
        let sorted = |r: &Room| {
            let mut v = serde_json::to_value(&r.graph).unwrap();
            v["edges"].as_array_mut().unwrap().sort_by_key(|e| e.to_string());
            v
        };
        assert_eq!(sorted(&live), sorted(&cold));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn adding_a_repo_relabels_the_branches_already_recorded() {
        let web = git_room("relabel-web");
        let api = git_room("relabel-api");
        let path = |p: &Path| toml::Value::String(p.display().to_string());
        let ledger = web.join(".trio/room.jsonl");
        append(&ledger, &commit("c1", "2026-11-23T09:00", "claude/topic", "abc12345", "Topic work", false, false)).unwrap();
        append(&ledger, &Event { id: "v1".into(), at: "2026-11-23T09:10".into(), from: "codex".into(), body: Body::Verdict { item: "claude/topic".into(), outcome: "pass".into() } }).unwrap();
        let mut one = Room::open_read(&web).unwrap();
        one.tick();
        assert!(one.graph.items.contains_key("claude/topic"));
        std::fs::write(web.join(".trio/room.toml"), format!("me = 'sanjith'\nrepo = {}\nname = 'w'\n[[repos]]\npath = {}\nname = 'api'\n", path(&web), path(&api))).unwrap();
        let main = repos_of(&toml::from_str(&std::fs::read_to_string(web.join(".trio/room.toml")).unwrap()).unwrap(), &web)[0].0.clone().unwrap();
        let mut two = Room::open_read(&web).unwrap();
        two.tick();
        let ids: Vec<&String> = two.graph.items.keys().filter(|k| k.contains("topic")).collect();
        assert_eq!(ids, [&format!("{main}:claude/topic")], "one node, under the main repo's label");
        assert_eq!(crate::work::all(&two.graph, "sanjith")[&format!("{main}:claude/topic")].lane, "done", "its verdict came along");
        // Two repos with the same name get distinct labels.
        let same: RoomCfg = toml::from_str(&format!("repo = ''\n[[repos]]\npath = {}\nname = 'x'\n[[repos]]\npath = {}\nname = 'x'\n", path(&web), path(&api))).unwrap();
        assert_eq!(repos_of(&same, &web).into_iter().map(|r| r.0.unwrap()).collect::<Vec<_>>(), ["x", "x-2"]);
        for d in [web, api] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn a_pull_request_asks_for_its_items_and_a_closed_one_never_recreates_its_branch() {
        use crate::github::{Link, Login, Pr};
        let root = temp_room("pr-linked");
        std::fs::write(root.join(".trio/room.toml"), "me = 'sanjith'\nrepo = ''\n").unwrap();
        let link = Link { repo: "o/r".into(), label: None, remote_only: false, me: "sanjith".into(), my_login: None, agents: Vec::new() };
        let mut pr = Pr {
            number: 3, title: "Ready for review: the login".into(), head_ref_name: "claude/x".into(), head_ref_oid: "abc".into(),
            state: "OPEN".into(), is_draft: false, created_at: "2026-11-23T15:00:00Z".into(), updated_at: "2026-11-23T15:00:00Z".into(),
            merged_at: None, url: String::new(), author: Some(Login { login: "bo".into() }), review_requests: vec![], latest_reviews: vec![],
        };
        let mut room = Room::open(&root).unwrap();
        append(&room.ledger, &commit("c1", "2026-11-23T08:00", "claude/x", "abc", "XF1 login", false, false)).unwrap();
        let push = |room: &mut Room, pr: &Pr| {
            for e in link.events(pr) {
                if room.gh_rows.get(&e.id) != Some(&e) {
                    append(&room.ledger, &e).unwrap();
                }
            }
            room.tick();
        };
        push(&mut room, &pr);
        assert_eq!(crate::work::all(&room.graph, "sanjith")["XF1"].lane, "review", "the pull request asks for review of what its branch implements");
        pr.is_draft = true;
        push(&mut room, &pr);
        assert_ne!(crate::work::all(&room.graph, "sanjith")["XF1"].lane, "review", "a draft asks nothing, whatever its title says");
        append(&room.ledger, &commit("c2", "2099-01-01T09:00", "claude/x", "abc", "", false, true)).unwrap();
        room.tick();
        pr.state = "CLOSED".into();
        pr.updated_at = "2099-01-02T10:00:00Z".into();
        push(&mut room, &pr);
        let mut cold = Room::open(&root).unwrap();
        cold.tick();
        for r in [&room, &cold] {
            assert!(!r.graph.items.contains_key("claude/x"), "a closed PR does not bring back a deleted branch");
            assert_ne!(crate::work::all(&r.graph, "sanjith")["XF1"].lane, "review");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_replay_keeps_the_pause_and_what_is_unread() {
        let root = temp_room("replay-unread");
        std::fs::write(root.join(".trio/room.toml"), "me = 'sanjith'\nrepo = ''\n[[import]]\nfile = 'claude-posts.md'\nfrom = 'claude'\n").unwrap();
        let file = root.join("claude-posts.md");
        std::fs::write(&file, "## 2026-11-23 09:00 XF1 started\nworking\n").unwrap();
        let mut room = Room::open(&root).unwrap();
        room.tick();
        room.mark_seen().unwrap();
        drop(room);
        append(&root.join(".trio/room.jsonl"), &message("", "2026-11-23T09:30", "codex", "XF2 opened while you were out")).unwrap();
        let mut room = Room::open(&root).unwrap();
        room.tick();
        let upto = room.catchup(&crate::work::all(&room.graph, "sanjith")).expect("one unread").upto;
        room.set_paused(true);
        std::fs::write(&file, "## 2026-11-23 09:00 XF1 started\nworking \n").unwrap();
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(std::time::SystemTime::now() + Duration::from_secs(5)).unwrap();
        room.tick();
        assert!(room.cfg.paused, "a replay keeps the pause");
        let c = room.catchup(&crate::work::all(&room.graph, "sanjith")).expect("still unread after the replay");
        assert!(c.messages >= 1, "{c:?}");
        assert!(!room.mark_seen_upto(Some(upto)).unwrap(), "a strip from before the replay is stale");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_replaced_ledger_counts_as_new() {
        let root = temp_room("ledger-cut");
        let ledger = root.join(".trio/room.jsonl");
        let mut room = Room::open(&root).unwrap();
        append(&ledger, &message("", "2026-11-23T09:00", "codex", "XF1 a long first message that sets the offset well past the next one")).unwrap();
        room.tick();
        room.mark_seen().unwrap();
        drop(room);
        std::fs::remove_file(&ledger).unwrap();
        append(&ledger, &message("", "2026-11-23T08:00", "codex", "XF2 short")).unwrap();
        let mut back = Room::open(&root).unwrap();
        back.tick();
        let c = back.catchup(&crate::work::all(&back.graph, "sanjith")).expect("the new ledger's row is new");
        assert_eq!(c.messages, 1, "{c:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_ledger_cut_while_open_replays_without_waking_anyone() {
        let (root, mut room) = room_with("cut-live", &[("codex", "On it.", 0)]);
        room.tick();
        room.say(&room.me(), "@codex XF1", None).unwrap();
        room.tick();
        settle(&mut room).await;
        assert!(!room.delivered.is_empty());
        let ledger = root.join(".trio/room.jsonl");
        std::fs::write(&ledger, "").unwrap();
        append(&ledger, &said("2026-11-23T09:00", "sanjith", "@codex XF2")).unwrap();
        room.tick();
        room.tick();
        assert!(room.graph.items.contains_key("XF2"));
        assert!(!room.any_running(), "rows in a replaced ledger are not routed");
        let text = std::fs::read_to_string(&ledger).unwrap();
        assert!(text.contains("cut or replaced"), "{text}");
        assert!(room.delivered.is_empty(), "the old file's delivery record does not carry over: its ids are reused");
        // A message written after the replay routes as usual.
        room.say(&room.me(), "@codex XF3", None).unwrap();
        room.tick();
        assert!(room.any_running(), "rows after the cut are routed");
        settle(&mut room).await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_reroute_to_a_running_agent_waits_for_its_run() {
        #[cfg(windows)]
        let slow = "On it.& ping -n 3 127.0.0.1 >nul";
        #[cfg(unix)]
        let slow = "On it.'; sleep 2; echo '";
        let (root, mut room) = room_with("reroute-busy", &[("claude", "Done.", 0), ("codex", slow, 0)]);
        room.tick();
        room.say(&room.me(), "@codex look at XF1", None).unwrap();
        room.tick();
        assert!(room.any_running());
        room.stops.insert("claude".into(), stop());
        room.say(&room.me(), "@claude look at XF2", None).unwrap();
        room.tick();
        assert_eq!(room.held_for("codex"), 1, "Codex is mid-run: the rerouted message waits for it");
        assert!(room.held[0].handoff.iter().any(|l| l.contains("You are taking it instead")), "the note waits with it");
        settle(&mut room).await;
        room.tick();
        settle(&mut room).await;
        assert_eq!(room.held_for("codex"), 0);
        let ledger = std::fs::read_to_string(root.join(".trio").join("room.jsonl")).unwrap();
        assert_eq!(ledger.matches("\"agent\":\"codex\",\"hop\"").count(), 2, "{ledger}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_reroute_over_the_hop_budget_is_dropped_not_held() {
        let (root, mut room) = room_with("reroute-budget", &[("claude", "Done.", 0), ("codex", "On it.", 0)]);
        room.tick();
        room.stops.insert("claude".into(), stop());
        room.hop = 9;
        append(&room.ledger, &said("2026-11-24T09:00", "bo", "@claude look at XF1")).unwrap();
        room.tick();
        assert_eq!((room.held_for("claude"), room.held_for("codex")), (0, 0), "the hop budget stops it, as it would with nobody out");
        let ledger = std::fs::read_to_string(&room.ledger).unwrap();
        assert!(ledger.contains("over the hop budget"), "{ledger}");
        assert!(!room.any_running());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_message_queued_behind_a_run_goes_to_the_other_agent_if_its_agent_runs_out() {
        #[cfg(windows)]
        let slow = "Done.& ping -n 3 127.0.0.1 >nul";
        #[cfg(unix)]
        let slow = "Done.'; sleep 2; echo '";
        let (root, mut room) = room_with("queued-out", &[("claude", slow, 0), ("codex", "On it.", 0)]);
        room.tick();
        room.say(&room.me(), "@claude look at XF1", None).unwrap();
        room.tick();
        assert!(room.any_running());
        room.say(&room.me(), "@claude and XF2", None).unwrap();
        room.tick();
        assert_eq!(room.held_for("claude"), 1, "queued behind Claude's run");
        room.stops.insert("claude".into(), stop());
        settle(&mut room).await;
        assert_eq!(room.held_for("claude"), 0);
        let ledger = std::fs::read_to_string(root.join(".trio").join("room.jsonl")).unwrap();
        assert_eq!(ledger.matches("\"agent\":\"codex\",\"hop\"").count(), 1, "{ledger}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn an_agents_message_to_an_out_agent_waits_for_it_within_the_hop_budget() {
        let (root, mut room) = room_with("agent-to-out", &[("claude", "Done.", 0), ("codex", "On it.", 0)]);
        room.tick();
        room.stops.insert("claude".into(), stop());
        append(&room.ledger, &said("2026-11-24T09:00", "codex", "@claude look at XF1")).unwrap();
        room.tick();
        assert_eq!((room.held_for("claude"), room.held_for("codex")), (1, 0), "never rerouted back to its author");
        assert!(room.held[0].stay && !room.any_running());
        room.hop = 9;
        append(&room.ledger, &said("2026-11-24T09:01", "codex", "@claude and XF2")).unwrap();
        room.tick();
        assert_eq!(room.held_for("claude"), 1, "past the hop budget nothing is queued");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_cut_re_keys_held_wakes_so_a_reused_offset_is_a_new_message() {
        let (root, mut room) = room_with("cut-held", &[("claude", "Done.", 0)]);
        room.tick();
        room.stops.insert("claude".into(), stop());
        let ledger = root.join(".trio/room.jsonl");
        std::fs::write(&ledger, "").unwrap();
        room.tick();
        room.say(&room.me(), "@claude look at XF1", None).unwrap();
        room.tick();
        assert_eq!(room.held_for("claude"), 1);
        let old = room.held[0].event.id.clone();
        std::fs::write(&ledger, "").unwrap();
        room.tick();
        room.tick();
        assert!(room.held[0].event.id.starts_with(&format!("{old}@cut")), "{}", room.held[0].event.id);
        assert!(std::fs::read_to_string(held_path(&root)).unwrap().contains("@cut"));
        assert!(!room.has(&old, "claude"));
        room.say(&room.me(), "@claude look at XF2", None).unwrap();
        room.tick();
        assert_eq!(room.held_for("claude"), 2, "the new message is held too, though it may reuse the old id");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_reroute_that_cannot_start_is_tried_once() {
        let (root, room) = room_with("reroute-fail", &[("claude", "Done.", 0)]);
        drop(room);
        let path = root.join(".trio").join("room.toml");
        let mut cfg = std::fs::read_to_string(&path).unwrap();
        let cwd = toml::Value::String(root.join("cw-codex").display().to_string()).to_string();
        cfg.push_str(&format!("[agents.codex]\nenabled = true\nsession = \"s\"\ncwd = {cwd}\nwake = [\"trio-no-such-binary-xyz\"]\n"));
        std::fs::write(&path, cfg).unwrap();
        let mut room = Room::open(&root).unwrap();
        room.tick();
        room.stops.insert("claude".into(), stop());
        room.say(&room.me(), "@claude look at XF1", None).unwrap();
        for _ in 0..5 {
            room.tick();
        }
        assert_eq!(room.held_for("claude"), 1, "it waits for Claude");
        let ledger = std::fs::read_to_string(&room.ledger).unwrap();
        assert_eq!(ledger.matches("routed to Codex").count(), 1, "{ledger}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn held_wakes_for_a_turned_off_agent_are_dropped() {
        let (root, mut room) = room_with("held-off", &[("claude", "Done.", 0)]);
        room.tick();
        room.stops.insert("claude".into(), stop());
        room.say(&room.me(), "@claude look at XF1", None).unwrap();
        room.tick();
        assert_eq!(room.held_for("claude"), 1);
        room.agents.get_mut("claude").unwrap().cfg.enabled = false;
        room.stops.clear();
        room.tick();
        assert_eq!(room.held_for("claude"), 0);
        assert!(!held_path(&root).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn pull_request_lifecycles_agree_live_and_cold() {
        use crate::github::{Link, Login, Pr};
        let root = temp_room("pr-life");
        std::fs::write(root.join(".trio/room.toml"), "me = 'sanjith'\nrepo = ''\n").unwrap();
        let link = Link { repo: "o/r".into(), label: None, remote_only: false, me: "sanjith".into(), my_login: None, agents: Vec::new() };
        let mut pr = Pr {
            number: 4, title: "Login".into(), head_ref_name: "claude/y".into(), head_ref_oid: "def".into(),
            state: "OPEN".into(), is_draft: false, created_at: "2026-11-23T15:00:00Z".into(), updated_at: "2026-11-23T15:00:00Z".into(),
            merged_at: None, url: String::new(), author: Some(Login { login: "bo".into() }), review_requests: vec![], latest_reviews: vec![],
        };
        let mut room = Room::open(&root).unwrap();
        append(&room.ledger, &commit("c1", "2026-11-23T08:00", "claude/y", "def", "XF3 login", false, false)).unwrap();
        let push = |room: &mut Room, pr: &Pr| {
            for e in link.events(pr) {
                if room.gh_rows.get(&e.id) != Some(&e) {
                    append(&room.ledger, &e).unwrap();
                }
            }
            room.tick();
            crate::work::all(&room.graph, "sanjith")["XF3"].lane
        };
        assert_eq!(push(&mut room, &pr), "review");
        pr.state = "CLOSED".into();
        pr.updated_at = "2026-11-23T18:00:00Z".into();
        assert_ne!(push(&mut room, &pr), "review");
        pr.state = "OPEN".into();
        pr.updated_at = "2026-11-23T19:00:00Z".into();
        assert_eq!(push(&mut room, &pr), "review", "reopened");
        let mut cold = Room::open(&root).unwrap();
        cold.tick();
        assert_eq!(crate::work::all(&cold.graph, "sanjith")["XF3"].lane, "review", "the reopen is the last row, cold too");
        // Merging its only branch answers the request.
        append(&room.ledger, &commit("c2", "2026-11-23T20:00", "claude/y", "def", "XF3 login", true, false)).unwrap();
        room.tick();
        assert_ne!(crate::work::all(&room.graph, "sanjith")["XF3"].lane, "review");
        pr.is_draft = true;
        let Body::Message { items, .. } = &link.events(&pr)[0].body else { panic!() };
        assert!(items.is_empty(), "a draft names no branch");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_remote_repo_never_shares_a_local_repos_label() {
        let root = temp_room("labels");
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let cfg: RoomCfg = toml::from_str(&format!("repo = {}\n[[github]]\nrepo = 'o/work'\n", toml::Value::String(work.display().to_string()))).unwrap();
        let links = github_links(&cfg, &root);
        assert_eq!((links[0].remote_only, links[0].label.as_deref()), (true, Some("work-2")));
        let _ = std::fs::remove_dir_all(root);
    }

    fn ledger_rows(room: &Room) -> Vec<Event> {
        RoomLedger::new(room.ledger.clone()).poll().unwrap()
    }

    #[test]
    fn a_first_sight_merge_is_durable_through_deletion_and_restart() {
        let root = git_room("first-merge");
        git(&root, &["checkout", "-qb", "claude/x"]);
        let output = std::process::Command::new("git").arg("-C").arg(&root)
            .env("GIT_COMMITTER_DATE", "2026-01-02T12:00:00+00:00")
            .args(["commit", "--allow-empty", "-qm", "XF1 implemented"]).output().unwrap();
        assert!(output.status.success());
        git(&root, &["checkout", "-q", "main"]);
        git(&root, &["merge", "-q", "--ff-only", "claude/x"]);
        let mut live = Room::open(&root).unwrap();
        live.tick();
        let rows = poll_git(&mut live);
        assert_eq!(rows.len(), 1);
        assert!(crate::gitwatch::is_merge(&rows[0]));
        let stamp = git(&root, &["for-each-ref", "--format=%(committerdate:format-local:%Y-%m-%dT%H:%M)", "refs/heads/claude/x"]);
        assert_eq!(rows[0].at, stamp);
        git(&root, &["branch", "-D", "claude/x"]);
        assert_eq!(poll_git(&mut live).len(), 1);
        assert_eq!(ledger_rows(&live).len(), 2);
        drop(live);
        let mut cold = Room::open(&root).unwrap();
        cold.tick();
        assert!(poll_git(&mut cold).is_empty());
        assert!(!cold.graph.items.contains_key("claude/x"));
        let work = crate::work::all(&cold.graph, &cold.me());
        assert_eq!((work["XF1"].lane, work["XF1"].last.as_str()), ("done", stamp.as_str()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_second_observed_merge_after_changes_survives_cold_replay() {
        let root = git_room("merge-again");
        git(&root, &["checkout", "-qb", "claude/x"]);
        git(&root, &["commit", "--allow-empty", "-qm", "XF1 work"]);
        git(&root, &["checkout", "-q", "main"]);
        let mut live = Room::open(&root).unwrap();
        live.tick();
        poll_git(&mut live);
        git(&root, &["merge", "-q", "--ff-only", "claude/x"]);
        let first = poll_git(&mut live).pop().unwrap();
        assert!(crate::gitwatch::is_merge(&first));
        append(&live.ledger, &Event { id: "changes".into(), at: first.at.clone(), from: "codex".into(),
            body: Body::Verdict { item: "XF1".into(), outcome: "changes".into() },
        }).unwrap();
        live.tick();
        assert_eq!(crate::work::all(&live.graph, &live.me())["XF1"].lane, "changes");
        git(&root, &["checkout", "-q", "claude/x"]);
        git(&root, &["commit", "--allow-empty", "-qm", "XF1 fixes"]);
        git(&root, &["checkout", "-q", "main"]);
        poll_git(&mut live);
        git(&root, &["merge", "-q", "--ff-only", "claude/x"]);
        let second = poll_git(&mut live).pop().unwrap();
        assert!(crate::gitwatch::is_merge(&second));
        assert_ne!(first.id, second.id);
        assert_eq!(crate::work::all(&live.graph, &live.me())["XF1"].lane, "done");
        let mut cold = Room::open(&root).unwrap();
        cold.tick();
        assert_eq!(crate::work::all(&cold.graph, &cold.me())["XF1"].lane, "done");
        assert_eq!(cold.graph.items["claude/x"].merged_at, live.graph.items["claude/x"].merged_at);
        let _ = poll_git(&mut cold);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_recreated_ref_retires_its_old_generation_and_keeps_ledger_ids_unique() {
        let root = git_room("recreate");
        let base = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["checkout", "-qb", "claude/x"]);
        git(&root, &["commit", "--allow-empty", "-qm", "XF1 implemented"]);
        git(&root, &["checkout", "-q", "main"]);
        git(&root, &["merge", "-q", "--ff-only", "claude/x"]);
        let old_sha = git(&root, &["rev-parse", "claude/x"]);
        let mut first = Room::open(&root).unwrap();
        first.tick();
        poll_git(&mut first);
        append(&first.ledger, &Event { id: "branch-pass".into(), at: now(), from: "codex".into(),
            body: Body::Verdict { item: "claude/x".into(), outcome: "pass".into() },
        }).unwrap();
        first.tick();
        drop(first);

        git(&root, &["branch", "-D", "claude/x"]);
        git(&root, &["checkout", "-qb", "claude/x", &base]);
        git(&root, &["commit", "--allow-empty", "-qm", "XF2 implemented"]);
        git(&root, &["checkout", "-q", "main"]);
        let mut second = Room::open(&root).unwrap();
        second.tick();
        let rows = poll_git(&mut second);
        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[0].body, Body::Commit { gone: true, .. }));
        assert!(crate::gitwatch::is_tip(&rows[1]));
        let work = crate::work::all(&second.graph, &second.me());
        assert_eq!((work["XF1"].lane, work["XF2"].lane), ("done", "progress"));
        assert_eq!(work["XF1"].state, "pass");
        assert!(second.graph.items["claude/x"].verdicts.is_empty());
        let targets: Vec<_> = second.graph.edges.iter().filter(|e| e.kind == "implements").map(|e| e.to.as_str()).collect();
        assert_eq!(targets, ["XF2"]);
        second.apply_git_poll(crate::gitwatch::Poll { durable: rows, ..Default::default() });
        second.tick();
        let count = ledger_rows(&second).len();
        assert_eq!(count, 4, "a repeated poll must not append known rows");
        drop(second);

        let mut cold = Room::open(&root).unwrap();
        cold.tick();
        assert!(poll_git(&mut cold).is_empty());
        assert_eq!(ledger_rows(&cold).len(), count, "an unchanged restart must not append rows");
        let work = crate::work::all(&cold.graph, &cold.me());
        assert_eq!((work["XF1"].lane, work["XF2"].lane), ("done", "progress"));
        git(&root, &["branch", "-D", "claude/x"]);
        poll_git(&mut cold);
        for _ in 0..2 {
            git(&root, &["branch", "claude/x", &old_sha]);
            poll_git(&mut cold);
            git(&root, &["branch", "-D", "claude/x"]);
            poll_git(&mut cold);
        }
        let git_rows = ledger_rows(&cold);
        let ids: std::collections::HashSet<_> = git_rows.iter().map(|e| &e.id).collect();
        assert_eq!(ids.len(), git_rows.len());
        assert_eq!(git_rows.iter().filter(|e| matches!(&e.body, Body::Commit { sha, gone: true, .. } if sha == &old_sha)).count(), 3);
        let _ = std::fs::remove_dir_all(root);
    }

    /// A room whose claude agent is a shell command printing `reply` and exiting with `code`.
    fn room_with_claude(name: &str, reply: &str, code: i32) -> (PathBuf, Room) {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("trio-room-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(root.join(".trio")).unwrap();
        #[cfg(windows)]
        let wake = ["cmd".to_string(), "/C".into(), format!("echo {reply}& exit {code}")];
        #[cfg(unix)]
        let wake = ["sh".to_string(), "-c".into(), format!("echo '{reply}'; exit {code}")];
        let q = |s: &str| toml::Value::String(s.into()).to_string();
        let cfg = format!(
            "hop_budget = 3\n[agents.claude]\nenabled = true\nsession = \"s\"\ncwd = {}\nwake = [{}]\n",
            q(&root.join("cw").display().to_string()),
            wake.iter().map(|a| q(a)).collect::<Vec<_>>().join(", ")
        );
        std::fs::write(root.join(".trio").join("room.toml"), cfg).unwrap();
        let room = Room::open(&root).unwrap();
        (root, room)
    }

    /// Tick until the claude run started by a mention has been reaped; returns the ledger text
    /// as it stood right after that tick (no further ticks).
    async fn wake_and_reap(root: &Path, room: &mut Room) -> String {
        room.tick();
        room.say(&room.me(), "@claude go", None).unwrap();
        room.tick();
        assert!(room.any_running(), "the mention should have started a run");
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            room.tick();
            if !room.any_running() {
                return std::fs::read_to_string(root.join(".trio").join("room.jsonl")).unwrap();
            }
        }
        panic!("run never finished");
    }

    #[test]
    fn the_human_handle_is_configured_cleaned_and_never_an_agent() {
        let cfg = |me: &str| RoomCfg { me: me.into(), ..Default::default() };
        assert_eq!(human(&cfg("Sanjith")), "sanjith");
        assert_eq!(human(&cfg(" Ana Lee ")), "analee");
        for bad in ["claude", "Codex", "both", "trio", "  ", "@!"] {
            assert!(!["claude", "codex", "both", "trio", ""].contains(&human(&cfg(bad)).as_str()), "{bad}");
        }
    }

    #[tokio::test]
    async fn a_reaped_reply_is_routed_in_the_same_tick() {
        let (root, mut room) = room_with_claude("route", "@codex ping", 0);
        let ledger = wake_and_reap(&root, &mut room).await;
        assert!(ledger.contains("\"from\":\"claude\",\"kind\":\"message\",\"summary\":\"@codex ping\""), "{ledger}");
        // codex is not configured, so routing records why it was not woken: proof it was routed.
        assert!(ledger.contains("codex not woken: Disabled"), "{ledger}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A room with both agents as shell commands: (name, printed reply, exit code).
    fn room_with(name: &str, agents: &[(&str, &str, i32)]) -> (PathBuf, Room) {
        let with: Vec<(&str, &str, i32, &str)> = agents.iter().map(|(a, r, c)| (*a, *r, *c, "")).collect();
        room_with_cfg(name, &with)
    }

    /// As `room_with`, with extra room.toml lines per agent (`roles = [..]`, `account = ..`).
    fn room_with_cfg(name: &str, agents: &[(&str, &str, i32, &str)]) -> (PathBuf, Room) {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("trio-room-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(root.join(".trio")).unwrap();
        let q = |s: &str| toml::Value::String(s.into()).to_string();
        let mut cfg = "hop_budget = 3\n".to_string();
        for (agent, reply, code, extra) in agents {
            #[cfg(windows)]
            let wake = ["cmd".to_string(), "/C".into(), format!("echo {reply}& exit {code}")];
            #[cfg(unix)]
            let wake = ["sh".to_string(), "-c".into(), format!("echo '{reply}'; exit {code}")];
            cfg.push_str(&format!(
                "[agents.{agent}]\nenabled = true\nsession = \"s\"\ncwd = {}\nwake = [{}]\n{extra}\n",
                q(&root.join(format!("cw-{agent}")).display().to_string()),
                wake.iter().map(|a| q(a)).collect::<Vec<_>>().join(", ")
            ));
        }
        std::fs::write(root.join(".trio").join("room.toml"), cfg).unwrap();
        let room = Room::open(&root).unwrap();
        (root, room)
    }

    async fn settle(room: &mut Room) {
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            room.tick();
            if !room.any_running() {
                return;
            }
        }
        panic!("runs never finished");
    }

    #[tokio::test]
    async fn a_usage_limit_hands_the_message_to_the_other_agent() {
        let (root, mut room) = room_with("limit", &[("claude", "You've hit your session limit - resets 3:45pm", 1), ("codex", "Taking it.", 0)]);
        room.tick();
        room.say(&room.me(), "@claude look at XF1", None).unwrap();
        room.tick();
        settle(&mut room).await;
        let ledger = std::fs::read_to_string(root.join(".trio").join("room.jsonl")).unwrap();
        assert!(ledger.contains("claude hit a usage limit: out until 15:45"), "{ledger}");
        assert!(ledger.contains("routed to Codex: Claude is out until 15:45"), "{ledger}");
        assert!(ledger.contains("\"agent\":\"codex\""), "codex was woken: {ledger}");
        assert!(ledger.contains("\"summary\":\"Taking it.\""), "{ledger}");
        assert!(room.usage("claude").out() && !room.usage("codex").out());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_review_of_the_other_agents_work_waits_for_the_reviewer() {
        let (root, mut room) = room_with("hold", &[("claude", "Reviewed.", 0), ("codex", "Not me.", 0)]);
        room.tick();
        // Codex wrote XF1 on its branch; Claude is out.
        let branch = Event { id: "git:codex/x".into(), at: now(), from: "codex".into(), body: Body::Commit { branch: "codex/x".into(), sha: "abc12345".into(), subject: "XF1 work".into(), touches: vec![], merged: false, gone: false } };
        append(&room.ledger, &branch).unwrap();
        room.stops.insert("claude".into(), crate::usage::Stop { until: crate::usage::epoch_now() + 3600, reason: "session limit".into() });
        room.say(&room.me(), "@claude please review XF1", None).unwrap();
        room.tick();
        let ledger = std::fs::read_to_string(root.join(".trio").join("room.jsonl")).unwrap();
        assert!(ledger.contains("held for Claude: Codex wrote it"), "{ledger}");
        assert!(!ledger.contains("\"agent\":\"codex\""), "never rerouted to the author: {ledger}");
        assert_eq!(room.held_for("claude"), 1);
        // Claude's limit resets: the held review goes to Claude.
        room.stops.clear();
        room.tick();
        settle(&mut room).await;
        let ledger = std::fs::read_to_string(root.join(".trio").join("room.jsonl")).unwrap();
        assert!(ledger.contains("Claude is back"), "{ledger}");
        assert!(ledger.contains("\"summary\":\"Reviewed.\""), "{ledger}");
        assert_eq!(room.held_for("claude"), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn held_wakes_go_out_one_at_a_time_and_survive_a_restart() {
        let (root, mut room) = room_with("held-two", &[("claude", "Done.", 0)]);
        room.tick();
        room.stops.insert("claude".into(), crate::usage::Stop { until: crate::usage::epoch_now() + 3600, reason: "session limit".into() });
        room.say(&room.me(), "@claude look at XF1", None).unwrap();
        room.say(&room.me(), "@claude and XF2", None).unwrap();
        room.tick();
        assert_eq!(room.held_for("claude"), 2);
        drop(room);
        let mut room = Room::open(&root).unwrap();
        assert_eq!(room.held_for("claude"), 2, "held wakes survive a restart");
        room.tick();
        assert_eq!(room.held_for("claude"), 1, "one run at a time");
        settle(&mut room).await;
        settle(&mut room).await;
        assert_eq!(room.held_for("claude"), 0);
        let ledger = std::fs::read_to_string(root.join(".trio").join("room.jsonl")).unwrap();
        assert_eq!(ledger.matches("\"kind\":\"run_started\"").count(), 2, "both held messages woke Claude: {ledger}");
        assert!(!ledger.contains("not woken"), "{ledger}");
        assert!(!root.join(".trio/held.json").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    fn stop() -> crate::usage::Stop {
        crate::usage::Stop { until: crate::usage::epoch_now() + 3600, reason: "session limit".into() }
    }

    #[tokio::test]
    async fn a_held_both_message_reaches_each_agent_once() {
        let (root, mut room) = room_with("held-both", &[("claude", "You've hit your session limit - resets 3:45pm", 1), ("codex", "On it.", 0)]);
        room.tick();
        room.stops.insert("claude".into(), stop());
        room.stops.insert("codex".into(), stop());
        room.say(&room.me(), "@both look at XF1", None).unwrap();
        room.tick();
        assert_eq!((room.held_for("claude"), room.held_for("codex")), (1, 1));
        room.stops.remove("codex");
        room.tick();
        settle(&mut room).await;
        room.stops.remove("claude");
        room.tick();
        settle(&mut room).await;
        let ledger = std::fs::read_to_string(root.join(".trio").join("room.jsonl")).unwrap();
        assert_eq!(ledger.matches("\"agent\":\"codex\",\"hop\"").count(), 1, "Codex had it already: {ledger}");
        assert!(ledger.contains("Codex already has this"), "{ledger}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_held_wake_waits_for_a_runtime_and_an_unpaused_room() {
        let (root, mut room) = room_with("held-rt", &[("claude", "Done.", 0)]);
        room.tick();
        room.stops.insert("claude".into(), stop());
        room.say(&room.me(), "@claude look at XF1", None).unwrap();
        room.tick();
        room.stops.clear();
        room.tick(); // no runtime: a run could not start, so the wake stays queued
        assert_eq!(room.held_for("claude"), 1);
        assert!(held_path(&root).exists());
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        room.set_paused(true);
        {
            let _inside = rt.enter();
            room.tick();
        }
        assert_eq!(room.held_for("claude"), 1, "a paused room keeps it");
        room.set_paused(false);
        {
            let _inside = rt.enter();
            room.tick();
        }
        assert_eq!(room.held_for("claude"), 0);
        assert!(room.any_running());
        rt.block_on(settle(&mut room));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_limit_retry_never_wakes_an_agent_twice_for_one_message() {
        let (root, mut room) = room_with("retry", &[("claude", "You've hit your session limit - resets 3:45pm", 1), ("codex", "On it.", 0)]);
        room.tick();
        room.say(&room.me(), "@both look at XF1", None).unwrap();
        room.tick();
        settle(&mut room).await;
        let ledger = std::fs::read_to_string(root.join(".trio").join("room.jsonl")).unwrap();
        assert_eq!(ledger.matches("\"agent\":\"codex\",\"hop\"").count(), 1, "{ledger}");
        assert!(ledger.contains("Codex already has this"), "{ledger}");
        let _ = std::fs::remove_dir_all(root);
    }

    fn woken(root: &Path) -> Vec<String> {
        let ledger = std::fs::read_to_string(root.join(".trio").join("room.jsonl")).unwrap();
        ledger
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["kind"] == "run_started")
            .filter_map(|v| v["agent"].as_str().map(str::to_string))
            .collect()
    }

    fn three() -> [(&'static str, &'static str, i32, &'static str); 3] {
        [
            ("claude", "Done.", 0, "roles = ['build']"),
            ("gemini", "Looked.", 0, "harness = 'command'\nroles = ['review']"),
            ("codex", "On it.", 0, "roles = ['build', 'review']"),
        ]
    }

    #[tokio::test]
    async fn a_role_mention_wakes_one_holder_the_first_listed_when_all_are_free() {
        let (root, mut room) = room_with_cfg("role-one", &three());
        room.tick();
        assert_eq!(room.agent_names(), ["claude", "gemini", "codex"], "room.toml order");
        room.say(&room.me(), "@review look at XF1", None).unwrap();
        room.tick();
        settle(&mut room).await;
        assert_eq!(woken(&root), ["gemini"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_role_mention_skips_a_holder_that_is_out() {
        let (root, mut room) = room_with_cfg("role-out", &three());
        room.tick();
        room.stops.insert("gemini".into(), stop());
        room.say(&room.me(), "@review look at XF1", None).unwrap();
        room.tick();
        settle(&mut room).await;
        assert_eq!(woken(&root), ["codex"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn an_out_agent_hands_off_to_one_that_shares_its_role() {
        let (root, mut room) = room_with_cfg("share-role", &three());
        room.tick();
        room.stops.insert("claude".into(), stop());
        room.say(&room.me(), "@claude fix XF2", None).unwrap();
        room.tick();
        settle(&mut room).await;
        assert_eq!(woken(&root), ["codex"], "Gemini only reviews; Codex also builds");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn all_wakes_every_agent_but_the_author() {
        let (root, mut room) = room_with_cfg("all", &three());
        room.tick();
        append(&room.ledger, &said("2026-11-24T09:00", "codex", "@all have a look at XF3")).unwrap();
        room.tick();
        settle(&mut room).await;
        let mut w = woken(&root);
        w.sort();
        assert_eq!(w, ["claude", "gemini"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn agents_on_one_account_run_out_together() {
        let (root, mut room) = room_with_cfg("account", &[("claude", "Done.", 0, ""), ("sonnet", "Done.", 0, "harness = 'claude'"), ("codex", "On it.", 0, "")]);
        room.tick();
        room.stops.insert("claude".into(), stop());
        assert!(room.usage("sonnet").out(), "Sonnet draws on the same Claude account");
        assert!(!room.usage("codex").out());
        room.say(&room.me(), "@sonnet fix XF4", None).unwrap();
        room.tick();
        settle(&mut room).await;
        assert_eq!(woken(&root), ["codex"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn two_command_agents_do_not_share_limits() {
        let (root, mut room) = room_with_cfg("cmd-accounts", &[("bot1", "a", 0, ""), ("bot2", "b", 0, "")]);
        room.tick();
        room.stops.insert("bot1".into(), stop());
        assert!(room.usage("bot1").out() && !room.usage("bot2").out());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn both_never_asks_the_author_to_review_its_own_work() {
        let (root, mut room) = room_with("both-author", &[("claude", "x", 0), ("codex", "y", 0)]);
        room.tick();
        let branch = Event { id: "git:codex/x".into(), at: now(), from: "codex".into(), body: Body::Commit { branch: "codex/x".into(), sha: "abc12345".into(), subject: "XF1 work".into(), touches: vec![], merged: false, gone: false } };
        append(&room.ledger, &branch).unwrap();
        room.tick();
        room.say(&room.me(), "@both please review XF1", None).unwrap();
        room.tick();
        let ledger = std::fs::read_to_string(root.join(".trio").join("room.jsonl")).unwrap();
        assert!(ledger.contains("Codex wrote it, so @both leaves it out of the review"), "{ledger}");
        assert!(!ledger.contains("\"agent\":\"codex\",\"hop\""), "{ledger}");
        assert!(ledger.contains("\"agent\":\"claude\",\"hop\""), "{ledger}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_daily_budget_puts_an_agent_out_until_midnight() {
        let (root, mut room) = room_with("budget", &[("claude", "x", 0)]);
        room.cfg.budget.claude_usd_day = Some(1.0);
        room.tick();
        append(&room.ledger, &Event { id: String::new(), at: now(), from: "claude".into(), body: Body::RunFinished { agent: "claude".into(), exit: 0, secs: 9, cost: Some(1.25) } }).unwrap();
        room.tick();
        let u = room.usage("claude");
        assert_eq!((u.state, u.spent_today), ("out", 1.25));
        assert!(u.why().contains("today's $1.00 budget is spent"), "{}", u.why());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_failed_run_never_posts_as_the_agent() {
        let (root, mut room) = room_with_claude("fail", "@codex half an answer", 3);
        let ledger = wake_and_reap(&root, &mut room).await;
        assert!(!ledger.contains("half an answer"), "{ledger}");
        assert!(ledger.contains("claude run failed (exit 3); output not posted"), "{ledger}");
        assert!(!ledger.contains("codex not woken"), "a failed run must not wake anyone: {ledger}");
        let _ = std::fs::remove_dir_all(root);
    }
}
