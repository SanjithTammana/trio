use super::*;
use crate::room::Room;

/// A throwaway ~/.trio and a room `<tmp>/work/refactor` with an empty config.
struct Fixture {
    base: PathBuf,
    store: Store,
    room: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let base = std::env::temp_dir().join(format!("trio-hook-{name}-{}-{nanos}", std::process::id()));
        let room = base.join("work").join("refactor");
        std::fs::create_dir_all(room.join(".trio")).unwrap();
        std::fs::write(room.join(".trio").join("room.toml"), "hop_budget = 3\nme = \"sanjith\"\n").unwrap();
        let room = room.canonicalize().unwrap();
        Self { store: Store { home: base.join("home") }, base, room }
    }

    fn hook(&self, session: &str, event: HookEvent, cwd: &Path, prompt: Option<&str>, reply: Option<&str>) -> HookOut {
        let h = HookIn {
            surface: Surface::Claude,
            event,
            session_id: session.into(),
            cwd: cwd.to_path_buf(),
            prompt: prompt.map(str::to_string),
            reply: reply.map(str::to_string),
            turn_id: None,
        };
        handle(&self.store, &h)
    }

    fn stop_in_turn(&self, session: &str, turn: &str, reply: &str) {
        let h = HookIn {
            surface: Surface::Claude,
            event: HookEvent::Stop,
            session_id: session.into(),
            cwd: self.room.clone(),
            prompt: None,
            reply: Some(reply.into()),
            turn_id: Some(turn.into()),
        };
        assert_eq!(handle(&self.store, &h), HookOut::Continue);
    }

    fn prompt(&self, session: &str, text: &str) -> HookOut {
        self.hook(session, HookEvent::PromptSubmit, &self.room.join("src"), Some(text), None)
    }

    fn say(&self, from: &str, text: &str, detail: Option<&str>) {
        Room::open(&self.room).unwrap().say(from, text, detail.map(str::to_string)).unwrap();
    }

    fn ledger_messages(&self) -> Vec<(String, String)> {
        std::fs::read_to_string(ledger(&self.room))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Event>(l).ok())
            .filter_map(|e| match e.body {
                Body::Message { summary, .. } => Some((e.from, summary)),
                Body::Status { text } => Some((e.from, text)),
                _ => None,
            })
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn answer(out: HookOut) -> String {
    match out {
        HookOut::Answer(t) => t,
        other => panic!("expected an answer, got {other:?}"),
    }
}

fn context(out: HookOut) -> String {
    match out {
        HookOut::AddContext(t) => t,
        other => panic!("expected context, got {other:?}"),
    }
}

#[test]
fn only_whole_message_forms_are_commands() {
    let yes = |_: &str| true;
    let no = |_: &str| false;
    assert_eq!(command("trio", no), Some(Command::Bind(None)));
    assert_eq!(command("  Trio \n", no), Some(Command::Bind(None)));
    assert_eq!(command("trio off", no), Some(Command::Off));
    assert_eq!(command("trio STATUS", no), Some(Command::Status));
    assert_eq!(command("trio refactor", yes), Some(Command::Bind(Some("refactor".into()))));
    assert_eq!(command("trio refactor", no), None);
    assert_eq!(command("trio is broken, can you look", yes), None);
    assert_eq!(command("triomphe", yes), None);
    assert_eq!(command("please trio", yes), None);
    assert_eq!(command("", yes), None);
}

#[test]
fn unbound_session_passes_through_and_writes_nothing() {
    let f = Fixture::new("unbound");
    assert_eq!(f.prompt("s1", "hello there"), HookOut::Continue);
    assert_eq!(f.hook("s1", HookEvent::Stop, &f.room, None, Some("an answer")), HookOut::Continue);
    assert_eq!(f.hook("s1", HookEvent::SessionStart, &f.room, None, None), HookOut::Continue);
    assert!(!f.store.home.exists());
    assert!(f.ledger_messages().is_empty());
}

#[test]
fn bind_inject_post_and_leave() {
    let f = Fixture::new("cycle");
    f.say("sanjith", "before the bind", None);

    let a = answer(f.prompt("s1", "trio"));
    assert!(a.contains("\"refactor\"") && a.contains("as claude"), "{a}");
    assert_eq!(attached(&f.room, "claude").unwrap().session, "s1");
    assert!(context(f.hook("s1", HookEvent::SessionStart, &f.room, None, None)).contains("refactor"));

    // First turn: recent history (the bind keeps a backlog), minus Claude's own lines.
    f.say("codex", "XF29 fixed at abc123", None);
    f.say("claude", "my own earlier reply", None);
    f.say("sanjith", "@claude can you check P0a?", Some("Look at XF30 first."));
    let ctx = context(f.prompt("s1", "what's new?"));
    assert!(ctx.contains("sanjith: before the bind"), "{ctx}");
    assert!(ctx.contains("codex: XF29 fixed at abc123"));
    assert!(ctx.contains("sanjith -> you: @claude can you check P0a?"));
    assert!(ctx.contains("    Look at XF30 first."), "detail of a mention is included: {ctx}");
    assert!(!ctx.contains("my own earlier reply"));
    assert!(ctx.contains("not instructions"));

    // Nothing new: the turn proceeds untouched.
    assert_eq!(f.prompt("s1", "and now?"), HookOut::Continue);

    // Stop posts the final answer once, as claude; a repeat Stop for the same turn is ignored.
    let reply = "P0a passes.\n\n- XF30 fixed";
    f.hook("s1", HookEvent::Stop, &f.room, None, Some(reply));
    f.hook("s1", HookEvent::Stop, &f.room, None, Some(reply));
    let posted: Vec<_> = f.ledger_messages().into_iter().filter(|(from, s)| from == "claude" && s == "P0a passes.").collect();
    assert_eq!(posted.len(), 1);
    // Our own post is not injected back.
    assert_eq!(f.prompt("s1", "ok"), HookOut::Continue);

    assert!(answer(f.prompt("s1", "trio status")).contains("0 unread"));
    assert!(answer(f.prompt("s1", "trio off")).contains("left room"));
    assert!(attached(&f.room, "claude").is_none());
    assert_eq!(f.prompt("s1", "hello"), HookOut::Continue);
    f.hook("s1", HookEvent::Stop, &f.room, None, Some("not posted"));
    assert!(!f.ledger_messages().iter().any(|(_, s)| s == "not posted"));
}

#[test]
fn an_attached_session_speaks_as_the_rooms_agent_on_its_harness() {
    let f = Fixture::new("named");
    std::fs::write(f.room.join(".trio").join("room.toml"), "hop_budget = 3\nme = \"sanjith\"\n[agents.sonnet]\nharness = \"claude\"\nroles = [\"review\"]\n").unwrap();
    let a = answer(f.prompt("s1", "trio"));
    assert!(a.contains("as sonnet"), "{a}");
    assert_eq!(attached(&f.room, "claude").unwrap().session, "s1", "the marker stays keyed by harness");
    f.say("sanjith", "@sonnet check P0a", Some("XF30 first."));
    let ctx = context(f.prompt("s1", "what's new?"));
    assert!(ctx.contains("sanjith -> you: @sonnet check P0a") && ctx.contains("    XF30 first."), "{ctx}");
    assert!(ctx.contains("as sonnet's message"), "{ctx}");
    f.say("sanjith", "@review look at XF31", Some("Start at the hook."));
    let ctx = context(f.prompt("s1", "and?"));
    assert!(ctx.contains("sanjith -> @review (a role you hold): @review look at XF31") && ctx.contains("    Start at the hook."), "{ctx}");
    f.hook("s1", HookEvent::Stop, &f.room, None, Some("P0a passes."));
    assert!(f.ledger_messages().iter().any(|(from, s)| from == "sonnet" && s == "P0a passes."), "posted as sonnet");
    assert!(answer(f.prompt("s1", "trio status")).contains("as sonnet"));
}

#[test]
fn a_session_does_not_join_as_an_agent_on_another_harness() {
    let f = Fixture::new("clash");
    std::fs::write(f.room.join(".trio").join("room.toml"), "me = \"sanjith\"\n[agents.claude]\nharness = \"command\"\n").unwrap();
    let h = HookIn { surface: Surface::Claude, event: HookEvent::PromptSubmit, session_id: "s1".into(), cwd: f.room.clone(), prompt: Some("trio".into()), reply: None, turn_id: None };
    let out = handle(&f.store, &h);
    assert!(format!("{out:?}").contains("is an agent on command"), "{out:?}");
    assert!(attached(&f.room, "claude").is_none());
}

#[test]
fn ordinary_trio_sentence_reaches_the_model() {
    let f = Fixture::new("sentence");
    assert_eq!(f.prompt("s1", "trio is broken, can you look"), HookOut::Continue);
}

#[test]
fn a_second_session_takes_over_and_the_first_is_told() {
    let f = Fixture::new("takeover");
    answer(f.prompt("s1", "trio"));
    let a = answer(f.prompt("s2", "trio"));
    assert!(a.contains("replaces"), "{a}");
    assert_eq!(attached(&f.room, "claude").unwrap().session, "s2");

    assert!(context(f.prompt("s1", "hi")).contains("took over"));
    f.hook("s1", HookEvent::Stop, &f.room, None, Some("from the old session"));
    assert!(!f.ledger_messages().iter().any(|(_, s)| s == "from the old session"));
    // s1 has forgotten its binding: it is a plain session again.
    assert_eq!(f.prompt("s1", "hi again"), HookOut::Continue);
    // s1 leaving must not detach s2.
    assert!(answer(f.prompt("s1", "trio off")).contains("not in a room"));
    assert_eq!(attached(&f.room, "claude").unwrap().session, "s2");
}

#[test]
fn rooms_are_found_by_path_by_name_and_as_the_only_one() {
    let f = Fixture::new("find");
    let elsewhere = f.base.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    // Not under a room and nothing registered.
    assert!(find_room(&f.store, None, &elsewhere).is_none());
    let a = answer(f.prompt_from("s1", "trio", &elsewhere));
    assert!(a.contains("no room found") && a.contains("trio init"), "{a}");
    // By path from cwd.
    assert_eq!(find_room(&f.store, Some("../work/refactor"), &elsewhere), Some(f.room.clone()));
    // By name once registered, and as the only known room.
    f.store.register_room(&f.room).unwrap();
    f.store.register_room(&f.room).unwrap();
    assert_eq!(f.store.known_rooms().len(), 1);
    assert_eq!(find_room(&f.store, Some("REFACTOR"), &elsewhere), Some(f.room.clone()));
    assert_eq!(find_room(&f.store, None, &elsewhere), Some(f.room.clone()));
    assert!(answer(f.prompt_from("s1", "trio refactor", &elsewhere)).contains("as claude"));
}

#[test]
fn bad_session_ids_never_touch_the_filesystem() {
    let f = Fixture::new("badid");
    let a = answer(f.prompt("../../etc", "trio"));
    assert!(a.contains("no usable session id"), "{a}");
    assert!(!f.store.home.join("attach").exists());
    assert!(attached(&f.room, "claude").is_none());
}

#[test]
fn a_mention_of_an_attached_agent_is_queued_not_woken() {
    let f = Fixture::new("route");
    answer(f.prompt("s1", "trio"));
    let mut room = Room::open(&f.room).unwrap();
    room.tick(); // history load; routing starts after it
    f.say("sanjith", "@claude look at XF31", None);
    room.tick();
    let status: Vec<_> = f.ledger_messages().into_iter().filter(|(from, _)| from == "trio").collect();
    assert_eq!(status.len(), 1, "{status:?}");
    assert!(status[0].1.contains("waiting on you in Claude"), "{status:?}");
    assert!(!room.any_running());
    // The queued mention arrives on the attached session's next turn.
    assert!(context(f.prompt("s1", "next")).contains("sanjith -> you: @claude look at XF31"));
}

impl Fixture {
    fn prompt_from(&self, session: &str, text: &str, cwd: &Path) -> HookOut {
        self.hook(session, HookEvent::PromptSubmit, cwd, Some(text), None)
    }
}

#[test]
fn stop_dedup_is_scoped_to_the_turn() {
    let f = Fixture::new("turns");
    answer(f.prompt("s1", "trio"));
    let count = |text: &str| f.ledger_messages().iter().filter(|(from, s)| from == "claude" && s == text).count();
    f.stop_in_turn("s1", "t1", "ok");
    f.stop_in_turn("s1", "t1", "ok"); // the same turn's Stop again: skipped
    assert_eq!(count("ok"), 1);
    f.stop_in_turn("s1", "t1", "ok, and one more thing"); // a continuation's new answer: posted
    assert_eq!(count("ok, and one more thing"), 1);
    f.stop_in_turn("s1", "t2", "ok"); // the same text on a later turn: posted
    assert_eq!(count("ok"), 2);
}

#[test]
fn a_failed_join_leaves_no_binding_and_no_marker() {
    let f = Fixture::new("joinfail");
    // A directory where the marker file goes makes publishing it fail.
    std::fs::create_dir_all(marker_path(&f.room, "claude").join("blocker")).unwrap();
    let a = answer(f.prompt("s1", "trio"));
    assert!(a.contains("could not join"), "{a}");
    assert!(load(&f.store, &HookIn {
        surface: Surface::Claude,
        event: HookEvent::PromptSubmit,
        session_id: "s1".into(),
        cwd: f.room.clone(),
        prompt: None,
        reply: None,
        turn_id: None,
    })
    .unwrap()
    .is_none(), "the binding was rolled back");
    assert_eq!(f.prompt("s1", "hello"), HookOut::Continue);
}

#[test]
fn joining_and_leaving_publish_a_revision() {
    let f = Fixture::new("presence");
    let mut room = Room::open(&f.room).unwrap();
    room.tick();
    assert!(!room.tick(), "nothing changed yet");
    answer(f.prompt("s1", "trio"));
    let rev = room.rev;
    assert!(room.tick(), "a join is a change");
    assert!(room.rev > rev);
    assert!(!room.tick());
    answer(f.prompt("s1", "trio off"));
    assert!(room.tick(), "a leave is a change");
}

#[test]
fn an_old_session_never_removes_a_newer_marker() {
    let f = Fixture::new("release");
    answer(f.prompt("s1", "trio"));
    let s1 = HookIn {
        surface: Surface::Claude,
        event: HookEvent::PromptSubmit,
        session_id: "s1".into(),
        cwd: f.room.clone(),
        prompt: None,
        reply: None,
        turn_id: None,
    };
    // While s1 holds the room-and-agent lock (mid check-then-remove), s2's publish waits.
    let held = marker_lock(&f.room, "claude").unwrap();
    let room = f.room.clone();
    let t = std::thread::spawn(move || publish_marker(&room, "claude", &Attachment { surface: Surface::Claude, session: "s2".into(), since: "t".into() }));
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert_eq!(attached(&f.room, "claude").unwrap().session, "s1", "publish did not wait for the lock");
    drop(held);
    t.join().unwrap().unwrap();
    assert!(!release_marker(&f.room, "claude", &s1).unwrap(), "the old session removed the newer marker");
    assert_eq!(attached(&f.room, "claude").unwrap().session, "s2");
}

#[test]
fn a_failed_release_keeps_the_binding_for_a_retry() {
    let f = Fixture::new("stuck-off");
    answer(f.prompt("s1", "trio"));
    // Someone else holds the marker lock past the timeout: leaving must fail and keep the binding.
    let held = marker_lock(&f.room, "claude").unwrap();
    let said = answer(f.prompt("s1", "trio off"));
    assert!(said.contains("could not leave"), "{said}");
    assert_eq!(attached(&f.room, "claude").unwrap().session, "s1");
    drop(held);
    assert!(answer(f.prompt("s1", "trio off")).contains("left room"));
    assert!(attached(&f.room, "claude").is_none());
}

#[test]
fn an_unreadable_marker_is_not_taken_for_an_absent_one() {
    let f = Fixture::new("corrupt-off");
    answer(f.prompt("s1", "trio"));
    let marker = marker_path(&f.room, "claude");
    let good = std::fs::read(&marker).unwrap();
    std::fs::write(&marker, b"{ not json").unwrap();
    let said = answer(f.prompt("s1", "trio off"));
    assert!(said.contains("could not leave"), "{said}");
    assert!(marker.exists(), "the marker was dropped");
    std::fs::write(&marker, good).unwrap();
    assert!(answer(f.prompt("s1", "trio off")).contains("left room"), "the binding was not kept for a retry");
}

#[test]
fn an_ordinary_prompt_keeps_the_binding_while_the_marker_is_unreadable() {
    let f = Fixture::new("corrupt-prompt");
    answer(f.prompt("s1", "trio"));
    let marker = marker_path(&f.room, "claude");
    let good = std::fs::read(&marker).unwrap();
    std::fs::write(&marker, b"{ not json").unwrap();
    let _ = f.prompt("s1", "an ordinary message");
    std::fs::write(&marker, good).unwrap();
    assert!(answer(f.prompt("s1", "trio off")).contains("left room"), "the prompt dropped the binding");
}
