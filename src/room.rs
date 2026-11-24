//! A room: its config, its sources, a bounded event window, the derived graph, and the agents.
//!
//! Cost model:
//! - tick: one `stat` per source, O(sources); bytes read only when a file grew, O(delta).
//! - apply: O(1) amortized per event (hash index into the window; the window is trimmed in
//!   batches, so the O(n) re-index runs once per `WINDOW / 4` events).
//! - memory: at most `WINDOW * 1.25` events plus the graph, O(V + E) with capped step lists.

use crate::agent::{wake_prompt, Agent, AgentConfig, CliAgent, Guard, WakeDecision};
use crate::graph::{derivers, Deriver, Graph};
use crate::model::{clip, split_reply, Body, Event, Summarized, SUMMARY_MAX};
use crate::source::{append, mentions_in, PostsFile, RoomLedger, Source};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    /// Git repo whose `claude/*` and `codex/*` branch tips appear in the graph.
    #[serde(default)]
    pub repo: Option<PathBuf>,
    /// Branch that work merges into (default: `main`, else `master`).
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub agents: HashMap<String, AgentConfig>,
}

fn default_budget() -> u32 {
    3
}

#[derive(Debug, Deserialize, Clone)]
pub struct Import {
    pub file: String,
    pub from: String,
}

pub struct Room {
    pub root: PathBuf,
    pub ledger: PathBuf,
    pub cfg: RoomCfg,
    sources: Vec<Box<dyn Source + Send>>,
    events: Vec<Event>,
    index: HashMap<String, usize>,
    pub graph: Graph,
    derivers: Vec<Box<dyn Deriver>>,
    pub rev: u64,
    /// Which sessions hold the agents, as last seen; a change is published like an event.
    presence: String,
    pub agents: HashMap<String, CliAgent>,
    started: HashMap<String, Instant>,
    hop: u32,
    live: bool,
    /// Branch tips for the map, read off the room lock (None without `repo`).
    git: Option<crate::gitwatch::GitWatch>,
    trio_exe: String,
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
        let root = root.canonicalize()?;
        let dir = root.join(".trio");
        let cfg: RoomCfg = match std::fs::read_to_string(dir.join("room.toml")) {
            Ok(t) => toml::from_str(&t).map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => RoomCfg { hop_budget: 3, ..Default::default() },
            Err(e) => return Err(e),
        };
        let ledger = dir.join("room.jsonl");
        let mut sources: Vec<Box<dyn Source + Send>> = Vec::new();
        for imp in &cfg.import {
            sources.push(Box::new(PostsFile::new(root.join(&imp.file), &imp.from)));
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
        let trio_exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "trio".into());
        let git = cfg.repo.clone().map(|r| crate::gitwatch::GitWatch::start(r, cfg.base.clone()));
        Ok(Self {
            root,
            ledger,
            cfg,
            sources,
            events: Vec::new(),
            index: HashMap::new(),
            graph: Graph::default(),
            derivers: derivers(),
            rev: 0,
            presence: String::new(),
            agents,
            started: HashMap::new(),
            hop: 0,
            live: false,
            git,
            trio_exe,
        })
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
        // A reaped reply was just appended to the ledger. Read and route it now: the loop may
        // sleep after this tick (no page, no live run), and a mention in it must still wake the
        // other agent. Bounded: each pass needs a run to finish, and there are two agents.
        while self.reap() {
            changed = true;
            self.ingest();
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

    /// Poll every source, apply new events and route new ledger messages.
    fn ingest(&mut self) -> bool {
        let mut fresh: Vec<(Event, bool)> = Vec::new();
        for i in 0..self.sources.len() {
            let from_ledger = i == self.sources.len() - 1;
            if let Ok(evs) = self.sources[i].poll() {
                fresh.extend(evs.into_iter().map(|e| (e, from_ledger)));
            }
        }
        if let Some(g) = &mut self.git {
            fresh.extend(g.take().into_iter().map(|e| (e, false)));
        }
        let changed = !fresh.is_empty();
        fresh.sort_by(|a, b| a.0.at.cmp(&b.0.at));
        for (e, from_ledger) in fresh {
            let route = self.live && from_ledger && matches!(e.body, Body::Message { .. }) && !self.index.contains_key(&e.id);
            self.apply(e.clone());
            if route {
                self.route(&e);
            }
        }
        changed
    }

    fn apply(&mut self, e: Event) {
        self.graph.apply(&e, &self.derivers);
        if let Some(&i) = self.index.get(&e.id) {
            self.events[i] = e;
            return;
        }
        self.index.insert(e.id.clone(), self.events.len());
        self.events.push(e);
        if self.events.len() > WINDOW + WINDOW / 4 {
            let cut = self.events.len() - WINDOW;
            self.events.drain(..cut);
            self.index = self.events.iter().enumerate().map(|(i, e)| (e.id.clone(), i)).collect();
        }
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
        let e = Event {
            id: String::new(),
            at: now(),
            from: from.to_string(),
            body: Body::Message { summary: summary.to_string(), detail, mentions: mentions_in(&text, &self.me()), items: crate::model::item_ids(&text) },
        };
        append(&self.ledger, &e).map_err(|e| e.to_string())
    }

    fn record(&self, from: &str, body: Body) {
        let _ = append(&self.ledger, &Event { id: String::new(), at: now(), from: from.to_string(), body });
    }

    fn route(&mut self, e: &Event) {
        let Body::Message { mentions, .. } = &e.body else { return };
        let me = self.me();
        if e.from == me {
            self.hop = 0;
        } else {
            self.hop += 1;
        }
        let mut targets: Vec<String> = mentions.iter().filter(|m| m.as_str() != "both" && m.as_str() != me).cloned().collect();
        if mentions.iter().any(|m| m == "both") {
            targets.extend(["claude".to_string(), "codex".to_string()]);
        }
        targets.retain(|t| t != &e.from);
        targets.dedup();
        let recent: Vec<String> = self
            .events
            .iter()
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
        let room = self.root.display().to_string();
        for t in targets {
            // Attached agents are pull-on-turn: the mention reaches them with their next message.
            if let Some(a) = crate::hook::attached(&self.root, &t) {
                self.record("trio", Body::Status { text: format!("{t} is attached: waiting on you in {}", a.surface.label()) });
                continue;
            }
            let guard = Guard { paused: self.cfg.paused, hop: self.hop, budget: self.cfg.hop_budget, from_human: e.from == me };
            let prompt = wake_prompt(&t, &me, &room, &recent, detail);
            let decision = match self.agents.get_mut(&t) {
                Some(a) => a.wake_if_allowed(&prompt, &guard),
                None => WakeDecision::Disabled,
            };
            match decision {
                WakeDecision::Started => {
                    self.started.insert(t.clone(), Instant::now());
                    self.record(&e.from, Body::RunStarted { agent: t, hop: self.hop, trigger: e.id.clone() });
                }
                other => self.record("trio", Body::Status { text: clip(&format!("{t} not woken: {other:?}"), 120) }),
            }
        }
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
                self.record(&n, Body::RunFinished { agent: n.clone(), exit: r.exit, secs });
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
        self.index.get(id).and_then(|&i| self.events[i].detail())
    }

    /// The chat view: the newest `limit` messages and run events, oldest first.
    pub fn messages(&self, limit: usize) -> Vec<MessageView<'_>> {
        let mut out: Vec<&Event> = self.events.iter().filter(|e| !matches!(e.body, Body::Claim { .. } | Body::Verdict { .. } | Body::Commit { .. })).collect();
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
                    Body::RunFinished { agent, exit, secs } => format!("{agent} finished in {secs}s (exit {exit})"),
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

    pub fn agent_states(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        for (n, a) in &self.agents {
            m.insert(n.clone(), serde_json::json!({ "enabled": a.cfg.enabled, "running": a.is_running(), "hue": crate::agent::Participant::hue(a) }));
        }
        // Attached sessions show up whether or not a headless agent is configured.
        for n in ["claude", "codex"] {
            let attached = crate::hook::attached(&self.root, n).map(|a| a.surface);
            let entry = m.entry(n.to_string()).or_insert_with(|| serde_json::json!({ "enabled": false, "running": false, "hue": "dusk" }));
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
pub fn human(cfg: &RoomCfg) -> String {
    let clean = |s: &str| -> Option<String> {
        let s: String = s.trim().to_ascii_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        (!s.is_empty() && !["claude", "codex", "both", "trio"].contains(&s.as_str())).then_some(s)
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
