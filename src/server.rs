//! Local HTTP server: embedded UI, JSON views, SSE change feed.
//!
//! `View` is the interface each screen implements; the server only routes to views.
//! Security (same rules as desk.mjs): bind 127.0.0.1, Host must match (else 421), writes need
//! the per-run token and a same-origin request, every response is `no-store`.

use crate::room::Room;
use axum::{
    body::{Body, Bytes},
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue, Request, StatusCode},
    middleware::{self, Next},
    response::{sse::{Event as SseEvent, KeepAlive, Sse}, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rust_embed::RustEmbed;
use serde::Deserialize;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

/// The web UI, built by `npm run build` in web/. A build without it still compiles and serves
/// the API (the native window needs nothing else); the page then says how to build it.
#[derive(RustEmbed)]
#[folder = "web/dist/"]
#[allow_missing = true]
struct Assets;

pub struct App {
    pub room: Mutex<Room>,
    pub token: String,
    pub hosts: Vec<String>,
    pub tx: broadcast::Sender<u64>,
    /// Wakes the idle loop: a page connected, or a message/pause arrived.
    pub wake: tokio::sync::Notify,
    /// `hook::room_id` of this server's room; a poke must carry it.
    pub room_id: String,
    /// The last serialized /api/state and the `rev` it was built at.
    pub snapshot: Mutex<Option<(u64, Bytes)>>,
}

impl App {
    /// Watched = a visible page holds the event stream (hidden tabs disconnect themselves).
    pub fn active(&self) -> bool {
        self.tx.receiver_count() > 0 || self.room.lock().expect("room lock").any_running()
    }
}

pub trait View: Send + Sync {
    fn snapshot(&self, room: &Room) -> serde_json::Value;
}

pub struct ChatView;
impl View for ChatView {
    fn snapshot(&self, room: &Room) -> serde_json::Value {
        serde_json::json!({ "messages": room.messages(400) })
    }
}

/// Edge kinds a client draws.
const DRAWN_EDGES: &[&str] = &["implements"];

fn graph_snapshot(room: &Room, work: &HashMap<String, crate::work::Work>) -> serde_json::Value {
    use crate::graph::Node;
    use crate::model::{Entity, Summarized, Timed};
    let mut items: Vec<_> = room.graph.items.values().collect();
    items.sort_by(|a, b| a.at().cmp(b.at()).then(a.id().cmp(b.id())));
    let nodes: Vec<_> = items.iter().map(|n| {
        let w = work.get(n.id());
        let state = match w {
            Some(w) => &w.state,
            None => crate::work::latest_verdict(n).map_or(n.base_state(), |v| &v.outcome),
        };
        let mut v = serde_json::json!({
            "id": n.id(), "kind": n.kind(), "lane": n.lane(), "state": state,
            "first": n.at(), "last": n.last, "summary": n.summary(),
            "touches": n.touches, "steps": n.steps,
        });
        if let Some(w) = w {
            v["work"] = serde_json::json!(w);
        }
        v
    }).collect();
    // Clients draw only these; the other derived edges stay server-side.
    let drawn: Vec<_> = room.graph.edges.iter().filter(|e| DRAWN_EDGES.contains(&e.kind.as_str())).collect();
    serde_json::json!({
        "items": nodes, "edges": drawn, "now": room.graph.now,
        "touches": room.graph.touches, "merged": room.graph.merged,
    })
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/api/state", get(state))
        .route("/api/detail", get(detail))
        .route("/api/events", get(events))
        .route("/api/say", post(say))
        .route("/api/pause", post(pause))
        .route("/api/poke", post(poke))
        .route("/api/seen", post(seen))
        .route("/api/rooms", get(rooms))
        .route("/api/rooms/open", post(open_room))
        .fallback(get(asset))
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

async fn guard(State(app): State<Arc<App>>, req: Request<Body>, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let mut res = if app.hosts.iter().any(|h| h == host) {
        next.run(req).await
    } else {
        (StatusCode::MISDIRECTED_REQUEST, "wrong host").into_response()
    };
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

async fn state(State(app): State<Arc<App>>) -> Response {
    let mut room = app.room.lock().expect("room lock");
    // Catch up on demand (the loop may have been asleep). Whatever this tick consumes is
    // broadcast too, so other open pages never miss a change the requester saw first.
    if room.tick() {
        let _ = app.tx.send(room.rev);
    }
    // Every change bumps `rev`, so one serialization serves every client until the next one.
    let mut cache = app.snapshot.lock().expect("snapshot lock");
    let bytes = match &*cache {
        Some((rev, bytes)) if *rev == room.rev => bytes.clone(),
        _ => {
            let bytes = Bytes::from(serde_json::to_vec(&snapshot(&room, &app.token, &app.room_id)).unwrap_or_default());
            *cache = Some((room.rev, bytes.clone()));
            bytes
        }
    };
    ([(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))], bytes).into_response()
}

fn snapshot(room: &Room, token: &str, room_id: &str) -> serde_json::Value {
    let mut v = serde_json::json!({
        "room": room.name(),
        // The native window checks this before showing state or writing, so a stale port that
        // now belongs to another room's server is never taken for this room.
        "room_id": room_id,
        "rev": room.rev,
        "paused": room.cfg.paused,
        "hop": room.hop(),
        "budget": room.cfg.hop_budget,
        "me": room.me(),
        "token": token,
        "agents": room.agent_states(),
    });
    let obj = v.as_object_mut().expect("object");
    if let serde_json::Value::Object(chat) = ChatView.snapshot(room) {
        obj.extend(chat);
    }
    let work = crate::work::all(&room.graph, &room.me());
    obj.insert("graph".into(), graph_snapshot(room, &work));
    // Each agent's availability for the island bars and for "held until" on cards.
    let usage: serde_json::Map<String, serde_json::Value> = ["claude", "codex"]
        .iter()
        .filter(|a| room.agents.contains_key(**a))
        .map(|a| {
            let mut u = serde_json::json!(room.usage(a));
            u["held"] = serde_json::json!(room.held_for(a));
            (a.to_string(), u)
        })
        .collect();
    obj.insert("usage".into(), serde_json::Value::Object(usage));
    if let Some(c) = room.catchup(&work) {
        obj.insert("catchup".into(), serde_json::json!(c));
    }
    v
}

async fn detail(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> Response {
    let room = app.room.lock().expect("room lock");
    match q.get("id").and_then(|id| room.detail(id)) {
        Some(d) => Json(serde_json::json!({ "detail": d })).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn events(State(app): State<Arc<App>>) -> Sse<impl tokio_stream::Stream<Item = Result<SseEvent, Infallible>>> {
    let rev = app.room.lock().expect("room lock").rev;
    let first = tokio_stream::once(Ok(SseEvent::default().data(rev.to_string())));
    let rest = BroadcastStream::new(app.tx.subscribe()).filter_map(|r| r.ok().map(|rev| Ok(SseEvent::default().data(rev.to_string()))));
    app.wake.notify_one();
    // The keep-alive write is also how a closed page is noticed, which lets the loop sleep again.
    Sse::new(first.chain(rest)).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(5)))
}

fn same_origin(app: &App, headers: &HeaderMap) -> bool {
    let origin = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()).unwrap_or("");
    app.hosts.iter().any(|h| origin == format!("http://{h}"))
}

#[derive(Deserialize)]
struct SayReq {
    token: String,
    text: String,
    #[serde(default)]
    detail: Option<String>,
}

async fn say(State(app): State<Arc<App>>, headers: HeaderMap, Json(r): Json<SayReq>) -> Response {
    if r.token != app.token || !same_origin(&app, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let detail = r.detail.filter(|d| !d.trim().is_empty());
    let result = {
        let room = app.room.lock().expect("room lock");
        room.say(&room.me(), &r.text, detail)
    };
    app.wake.notify_one(); // route the mention now, even if the page closes right after
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

#[derive(Deserialize)]
struct PauseReq {
    token: String,
    paused: bool,
}

async fn pause(State(app): State<Arc<App>>, headers: HeaderMap, Json(r): Json<PauseReq>) -> Response {
    if r.token != app.token || !same_origin(&app, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let rev = {
        let mut room = app.room.lock().expect("room lock");
        room.set_paused(r.paused);
        room.rev
    };
    let _ = app.tx.send(rev);
    app.wake.notify_one();
    StatusCode::NO_CONTENT.into_response()
}

/// Every room you have, for the project switcher. Replays each room's files off the runtime.
async fn rooms(State(app): State<Arc<App>>) -> Response {
    let root = app.room.lock().expect("room lock").root.clone();
    match tokio::task::spawn_blocking(move || crate::projects::all(&root)).await {
        Ok(list) => Json(list).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
struct OpenReq {
    token: String,
    path: String,
    /// The browser asks for a URL to go to; the native window lets the room open its own.
    #[serde(default)]
    web: bool,
}

/// Open another of your rooms: only one registered in `~/.trio/rooms.txt`.
async fn open_room(State(app): State<Arc<App>>, headers: HeaderMap, Json(r): Json<OpenReq>) -> Response {
    if r.token != app.token || !same_origin(&app, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(root) = crate::projects::known(&r.path) else {
        return (StatusCode::NOT_FOUND, "not one of your rooms").into_response();
    };
    match tokio::task::spawn_blocking(move || crate::projects::open(&root, r.web)).await {
        Ok(Ok(url)) => Json(serde_json::json!({ "url": url })).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_GATEWAY, e).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
struct SeenReq {
    token: String,
}

/// "Dismiss" on the catch-up: move the seen mark to now.
async fn seen(State(app): State<Arc<App>>, headers: HeaderMap, Json(r): Json<SeenReq>) -> Response {
    if r.token != app.token || !same_origin(&app, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let rev = {
        let mut room = app.room.lock().expect("room lock");
        if let Err(e) = room.mark_seen() {
            eprintln!("trio: could not save the seen mark: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "could not save the seen mark").into_response();
        }
        room.rev
    };
    let _ = app.tx.send(rev);
    StatusCode::NO_CONTENT.into_response()
}

/// A hook appended to the ledger: tick now so the line is routed and shown even while idle.
/// It changes no state beyond what the loop would read anyway. The custom header forces a CORS
/// preflight, which this server never grants, so a web page cannot send it.
async fn poke(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let status = poke_status(headers.get("x-trio-poke").map(|v| v.as_bytes()), &app.room_id);
    if status == StatusCode::NO_CONTENT {
        app.wake.notify_one();
    }
    status.into_response()
}

/// No header: 403 (not a hook). Another room's id: 409, and nothing wakes.
fn poke_status(header: Option<&[u8]>, room_id: &str) -> StatusCode {
    match header {
        None => StatusCode::FORBIDDEN,
        Some(id) if id == room_id.as_bytes() => StatusCode::NO_CONTENT,
        Some(_) => StatusCode::CONFLICT,
    }
}


async fn asset(req: Request<Body>) -> Response {
    let path = req.uri().path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match Assets::get(path).or_else(|| Assets::get("index.html")) {
        Some(file) => {
            let mime = mime_guess::from_path(if Assets::get(path).is_some() { path } else { "index.html" }).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.as_ref().to_string())], file.data.into_owned()).into_response()
        }
        None if path == "index.html" => (StatusCode::NOT_FOUND, "This trio was built without its web UI: run `npm ci && npm run build` in web/, then rebuild. The native window (trio-app) does not need it.").into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poke_must_name_this_room() {
        let id = crate::hook::room_id(std::path::Path::new("Z:/one/refactor"));
        assert_ne!(id, crate::hook::room_id(std::path::Path::new("Z:/two/refactor")));
        assert_eq!(poke_status(None, &id), StatusCode::FORBIDDEN);
        assert_eq!(poke_status(Some(b"1"), &id), StatusCode::CONFLICT);
        assert_eq!(poke_status(Some(id.as_bytes()), &id), StatusCode::NO_CONTENT);
    }

    #[test]
    fn the_exported_state_and_work_use_the_branch_verdict_after_a_late_link() {
        use crate::model::{Body, Event};
        let mut g = crate::graph::Graph::default();
        let derivers = crate::graph::derivers();
        let apply = |g: &mut crate::graph::Graph, id: &str, at: &str, body| {
            g.apply(&Event { id: id.into(), at: at.into(), from: "codex".into(), body }, &derivers);
        };
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("trio-state-link-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut room = Room::open(&root).unwrap();
        apply(&mut g, "changes", "2026-11-23T09:00", Body::Verdict { item: "XF1".into(), outcome: "changes".into() });
        apply(&mut g, "pass", "2026-11-23T10:00", Body::Verdict { item: "claude/x".into(), outcome: "pass".into() });
        let tip = |branch: &str, subject: &str| Body::Commit {
            branch: branch.into(), sha: "a".into(), subject: subject.into(), touches: vec![], merged: false, gone: false,
        };
        apply(&mut g, "link", "2026-11-23T10:00", tip("claude/x", "XF1 work"));
        apply(&mut g, "tie", "2026-11-23T10:00", tip("claude/a", "other work"));
        room.graph = g;
        let snap = snapshot(&room, "t", "r");
        let nodes = snap["graph"]["items"].as_array().unwrap();
        assert_eq!(nodes.iter().map(|n| n["id"].as_str().unwrap()).collect::<Vec<_>>(), ["XF1", "claude/a", "claude/x"]);
        assert_eq!((&nodes[0]["state"], &nodes[0]["work"]["lane"]), (&serde_json::json!("pass"), &serde_json::json!("done")));
        assert_eq!(nodes[1]["state"], "branch");
        assert_eq!(nodes[2]["state"], "pass");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn seen_post_reports_a_failed_save_without_advancing_the_mark() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("trio-seen-http-{}-{nanos}", std::process::id()));
        let blocked = crate::seen::path(&root);
        std::fs::create_dir_all(&blocked).unwrap();
        let mut room = Room::open(&root).unwrap();
        room.tick();
        let before = room.seen.clone();
        let rev = room.rev;
        let (tx, mut rx) = broadcast::channel(16);
        let app = Arc::new(App { room: Mutex::new(room), token: "t".into(), hosts: vec!["localhost:8964".into()], tx, wake: tokio::sync::Notify::new(), room_id: "r".into(), snapshot: Mutex::new(None) });
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, HeaderValue::from_static("http://localhost:8964"));
        let response = seen(State(app.clone()), headers.clone(), Json(SeenReq { token: "t".into() })).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        {
            let room = app.room.lock().unwrap();
            assert_eq!((room.seen.clone(), room.rev), (before, rev));
        }
        assert!(rx.try_recv().is_err());
        std::fs::remove_dir(blocked).unwrap();
        let response = seen(State(app), headers, Json(SeenReq { token: "t".into() })).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(rx.try_recv().unwrap(), rev + 1);
        let _ = std::fs::remove_dir_all(root);
    }

    async fn body(app: &Arc<App>) -> Bytes {
        axum::body::to_bytes(state(State(app.clone())).await.into_body(), usize::MAX).await.unwrap()
    }

    /// One serialization per revision: unchanged rooms get the cached bytes, a change rebuilds.
    #[tokio::test]
    async fn the_snapshot_is_built_once_per_revision() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("trio-snap-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(root.join(".trio")).unwrap();
        std::fs::write(root.join(".trio").join("room.toml"), "hop_budget = 3\n").unwrap();
        let (tx, _) = broadcast::channel(16);
        let app = Arc::new(App { room: Mutex::new(Room::open(&root).unwrap()), token: "t".into(), hosts: vec![], tx, wake: tokio::sync::Notify::new(), room_id: "r".into(), snapshot: Mutex::new(None) });
        let a = body(&app).await;
        let b = body(&app).await;
        assert_eq!(a.as_ptr(), b.as_ptr(), "an unchanged room was serialized again");
        app.room.lock().unwrap().set_paused(true);
        let c = body(&app).await;
        let v: serde_json::Value = serde_json::from_slice(&c).unwrap();
        assert_eq!(v["paused"], true, "a change was served from the stale cache");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A change consumed by a /api/state request reaches every other open page too.
    #[tokio::test]
    async fn a_state_request_broadcasts_what_it_consumed() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("trio-state-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(root.join(".trio").join("attached")).unwrap();
        std::fs::write(root.join(".trio").join("room.toml"), "hop_budget = 3\n").unwrap();
        let mut room = Room::open(&root).unwrap();
        room.tick();
        let (tx, mut rx) = broadcast::channel(16);
        let app = Arc::new(App { room: Mutex::new(room), token: "t".into(), hosts: vec![], tx, wake: tokio::sync::Notify::new(), room_id: "r".into(), snapshot: Mutex::new(None) });
        // A session joins while no ticker runs; the next state request sees it first.
        std::fs::write(root.join(".trio").join("attached").join("claude.json"), r#"{"surface":"claude","session":"s","since":"t"}"#).unwrap();
        let _ = state(State(app.clone())).await.into_response();
        let rev = app.room.lock().unwrap().rev;
        assert_eq!(rx.try_recv().ok(), Some(rev), "the join was not broadcast to other pages");
        let _ = std::fs::remove_dir_all(root);
    }
}
