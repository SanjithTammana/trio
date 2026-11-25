//! Talks to the room's local server over loopback HTTP/1.1, the same API the web UI uses.
//!
//! One background thread holds the change stream (/api/events) while the window is visible and
//! refetches the snapshot, debounced, when the revision moves; a burst of changes costs one
//! fetch. A minimized window drops the stream, so the server can go back to sleep.
//!
//! The port comes from the room's `server.json`, which can be stale: a port that now belongs to
//! another room's server is recognised by its room id and never shown or written to. Every read
//! is bounded (response size, header and line length, chunk framing, a deadline per request),
//! because whatever listens on a loopback port is not necessarily Trio.

use crate::model::{split_for_send, Snapshot};
use std::fmt;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How long the change stream blocks between checks while nothing is pending.
const IDLE_WAIT: Duration = Duration::from_secs(1);
const REQUEST_DEADLINE: Duration = Duration::from_secs(15);
const MAX_BODY: usize = 32 << 20;
const MAX_HEAD: usize = 16 << 10;
const MAX_LINE: usize = 64 << 10;
/// Failed snapshot refreshes in a row before the stream is given up and the server rediscovered.
const REFRESH_TRIES: u32 = 3;

#[derive(Clone)]
pub struct Client {
    room: PathBuf,
    id: String,
    shared: Arc<Mutex<Shared>>,
    repaint: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(serde::Deserialize)]
struct ServerInfo {
    port: u16,
}

/// A generation changes when the peer is invalidated, even if its port stays the same.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Target {
    port: u16,
    generation: u64,
}

pub struct Shared {
    target: Target,
    pub snap: Option<Arc<Snapshot>>,
    pub error: Option<String>,
}

#[derive(Debug)]
enum RequestError {
    Other(String),
    WrongRoom(u16),
}

impl From<String> for RequestError {
    fn from(error: String) -> Self {
        Self::Other(error)
    }
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Other(error) => f.write_str(error),
            Self::WrongRoom(port) => write!(f, "port {port} now serves another room"),
        }
    }
}

impl Client {
    fn new(room: &Path, port: u16) -> Self {
        Self {
            room: room.to_path_buf(),
            id: crate::roomid::room_id(room),
            shared: Arc::new(Mutex::new(Shared {
                target: Target { port, generation: 0 },
                snap: None,
                error: None,
            })),
            repaint: None,
        }
    }

    pub fn shared(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().expect("shared")
    }

    fn target(&self) -> Target {
        self.shared().target
    }

    fn open_verified(&self, target: Target) -> Result<(Connection, Snapshot), RequestError> {
        let mut conn = Connection::open(target.port)?;
        let (code, body) = conn.request("GET", "/api/state", None)?;
        if code != 200 {
            return Err(format!("server answered {code}").into());
        }
        match serde_json::from_slice::<Snapshot>(&body) {
            Ok(snap) if snap.room_id == self.id => {
                crate::model::set_roster(&snap.agents);
                Ok((conn, snap))
            }
            Ok(_) => Err(RequestError::WrongRoom(target.port)),
            Err(error) => {
                // A partial state reply can still explicitly identify a different room.
                #[derive(serde::Deserialize)]
                struct Identity {
                    room_id: String,
                }
                if serde_json::from_slice::<Identity>(&body).is_ok_and(|proof| proof.room_id != self.id) {
                    return Err(RequestError::WrongRoom(target.port));
                }
                Err(error.to_string().into())
            }
        }
    }

    fn verified_for_action(&self, target: Target) -> Result<(Connection, Snapshot), String> {
        match self.open_verified(target) {
            Ok(found) => Ok(found),
            Err(error) => {
                if matches!(error, RequestError::WrongRoom(_)) {
                    self.record_failure(target, &error, false);
                }
                Err(error.to_string())
            }
        }
    }

    pub fn detail(&self, id: &str) -> Result<String, String> {
        let (mut conn, _) = self.verified_for_action(self.target())?;
        let (code, body) = conn.request("GET", &format!("/api/detail?id={}", encode(id)), None)?;
        if code != 200 {
            return Ok(String::new());
        }
        let v: serde_json::Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
        Ok(v["detail"].as_str().unwrap_or_default().to_string())
    }

    pub fn say(&self, token: &str, text: &str) -> Result<(), String> {
        let (text, detail) = split_for_send(text);
        let body = serde_json::json!({ "token": token, "text": text, "detail": detail });
        self.post("/api/say", token, body)
    }

    /// Dismiss the catch-up: the room's seen mark moves to now.
    /// Every room you have, read by the server on demand.
    pub fn rooms(&self) -> Result<Vec<crate::model::RoomSummary>, String> {
        let (mut conn, _) = self.verified_for_action(self.target())?;
        let (code, body) = conn.request("GET", "/api/rooms", None)?;
        if code != 200 {
            return Err(format!("error {code}"));
        }
        serde_json::from_slice(&body).map_err(|e| e.to_string())
    }

    /// Open another of your rooms in its own window.
    pub fn open_room(&self, token: &str, path: &str) -> Result<(), String> {
        self.post("/api/rooms/open", token, serde_json::json!({ "token": token, "path": path, "web": false }))
    }

    /// Mark the catch-up built at `upto` seen. The server refuses (and keeps its mark) when rows
    /// arrived since: the next snapshot shows the newer strip.
    pub fn seen(&self, token: &str, upto: u64) -> Result<(), String> {
        self.post("/api/seen", token, serde_json::json!({ "token": token, "upto": upto }))
    }

    pub fn pause(&self, token: &str, paused: bool) -> Result<(), String> {
        let body = serde_json::json!({ "token": token, "paused": paused });
        self.post("/api/pause", token, body)
    }

    fn post(&self, path: &str, token: &str, body: serde_json::Value) -> Result<(), String> {
        let write_target = || {
            let shared = self.shared();
            shared.snap.as_ref().filter(|s| s.token == token).map(|_| shared.target)
        };
        let unavailable = "waiting for a verified room snapshot";
        let target = write_target().ok_or(unavailable)?;
        let (mut conn, proof) = self.verified_for_action(target)?;
        if write_target() != Some(target) {
            return Err(unavailable.into());
        }
        // The peer on this connection must hold the token being sent. A server that restarted
        // (or anything else now on the port) has another one: adopt its snapshot and stop.
        if proof.token != token {
            self.publish(target, proof);
            return Err("the room's server restarted; send again".into());
        }
        let (code, reply) = conn.request("POST", path, Some(body.to_string().as_bytes()))?;
        if (200..300).contains(&code) {
            Ok(())
        } else {
            let msg = String::from_utf8_lossy(&reply).trim().to_string();
            Err(if msg.is_empty() { format!("error {code}") } else { msg })
        }
    }

    fn repaint(&self) {
        if let Some(repaint) = &self.repaint {
            repaint();
        }
    }

    fn publish(&self, target: Target, snap: Snapshot) {
        let mut shared = self.shared();
        if shared.target != target {
            return;
        }
        // A restarted server starts its revisions over with a new token.
        if shared.snap.as_ref().is_none_or(|old| old.rev != snap.rev || old.token != snap.token) {
            shared.snap = Some(Arc::new(snap));
        }
        shared.error = None;
        drop(shared);
        self.repaint();
    }

    /// Re-read the marker on a mismatch, or when the current stream cannot be opened or kept.
    /// Return true when the watcher can retry immediately instead of backing off.
    /// A stream that cannot be opened or kept means the room's identity can no longer be
    /// established: the snapshot is cleared (the window shows "connecting…" and writes stop)
    /// until a verified one arrives.
    fn record_failure(&self, target: Target, error: &RequestError, stream_failed: bool) -> bool {
        let invalidate = stream_failed || matches!(error, RequestError::WrongRoom(_));
        // Invalidate first, so the controls go inactive at once; then read server.json outside
        // `Shared` (the UI locks it every frame, and the room may sit on a slow share).
        let next = Target { port: target.port, generation: target.generation + 1 };
        {
            let mut shared = self.shared();
            if shared.target != target {
                return true;
            }
            if invalidate {
                shared.snap = None;
                shared.target = next;
            }
            shared.error = Some(error.to_string());
        }
        self.repaint();
        if !invalidate {
            return false;
        }
        let port = read_port(&self.room).unwrap_or(target.port);
        if port != target.port {
            let mut shared = self.shared();
            if shared.target == next {
                shared.target = Target { port, generation: next.generation + 1 };
            }
        }
        port != target.port
    }

    fn watch(&self, target: Target, visible: &AtomicBool) -> Result<(), RequestError> {
        let (mut conn, snap) = self.open_verified(target)?;
        self.publish(target, snap);
        let mut due = None;
        // A refresh that fails is retried with backoff (1 s, 2 s); a third failure gives up the
        // stream, so the snapshot is cleared and the server rediscovered instead of left stale.
        let failures = std::cell::Cell::new(0u32);
        conn.follow(
            || visible.load(Ordering::Relaxed) && self.target() == target && failures.get() < REFRESH_TRIES,
            |rev| {
                if rev && due.is_none() {
                    due = Some(Instant::now() + Duration::from_millis(120));
                }
                if due.is_some_and(|d| Instant::now() >= d) {
                    due = None;
                    match self.open_verified(target) {
                        Ok((_, snap)) => {
                            failures.set(0);
                            self.publish(target, snap);
                        }
                        Err(error) => {
                            failures.set(failures.get() + 1);
                            self.record_failure(target, &error, false);
                            due = Some(Instant::now() + Duration::from_secs(1 << (failures.get() - 1).min(4)));
                        }
                    }
                }
                due.map_or(IDLE_WAIT, |d| d.saturating_duration_since(Instant::now()).max(Duration::from_millis(1)))
            },
        )?;
        if failures.get() >= REFRESH_TRIES {
            return Err(RequestError::Other("lost the room's snapshot; reconnecting".into()));
        }
        Ok(())
    }

    /// Keep the snapshot current while visible, waking the UI after each change.
    pub fn sync(&mut self, visible: Arc<AtomicBool>, repaint: impl Fn() + Send + Sync + 'static) {
        self.repaint = Some(Arc::new(repaint));
        let client = self.clone();
        std::thread::Builder::new()
            .name("trio-sync".into())
            .spawn(move || loop {
                if !visible.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(250));
                    continue;
                }
                let target = client.target();
                if let Err(error) = client.watch(target, &visible)
                    && !client.record_failure(target, &error, true)
                {
                    std::thread::sleep(IDLE_WAIT);
                }
            })
            .expect("spawn sync thread");
    }
}

/// A socket is pinned to its original port for both the proof and the following request.
/// Requests are sequential. A close-delimited response or Connection: close ends reuse.
struct Connection {
    socket: TcpStream,
    host: String,
    first_deadline: Option<Instant>,
    reusable: bool,
}

impl Connection {
    fn open(port: u16) -> Result<Self, String> {
        let first_deadline = Some(Instant::now() + REQUEST_DEADLINE);
        let socket = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(2))
            .map_err(|e| e.to_string())?;
        socket.set_nodelay(true).map_err(|e| e.to_string())?;
        Ok(Self { socket, host: format!("127.0.0.1:{port}"), first_deadline, reusable: true })
    }

    fn start(&mut self, method: &str, path: &str, body: Option<&[u8]>) -> Result<Instant, String> {
        if !self.reusable {
            return Err("the server closed the HTTP connection to this room".into());
        }
        self.reusable = false;
        let deadline = self.first_deadline.take().unwrap_or_else(|| Instant::now() + REQUEST_DEADLINE);
        let host = &self.host;
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\n");
        if path == "/api/events" {
            head.push_str("Accept: text/event-stream\r\n");
        }
        if let Some(body) = body {
            head.push_str(&format!("Origin: http://{host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");
        self.write(head.as_bytes(), deadline)?;
        if let Some(body) = body {
            self.write(body, deadline)?;
        }
        Ok(deadline)
    }

    fn write(&mut self, mut bytes: &[u8], deadline: Instant) -> Result<(), String> {
        while !bytes.is_empty() {
            self.socket.set_write_timeout(Some(time_left(deadline)?)).map_err(|e| e.to_string())?;
            match self.socket.write(bytes) {
                Ok(0) => return Err("the connection closed while sending a request".into()),
                Ok(n) => bytes = &bytes[n..],
                Err(e) if retryable(&e) => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
    }

    fn read(&mut self, response: &mut Response, out: &mut Vec<u8>, timeout: Duration) -> Result<bool, String> {
        self.socket.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
        let mut bytes = [0; 16 << 10];
        match self.socket.read(&mut bytes) {
            Ok(0) => {
                response.eof()?;
                Ok(false)
            }
            Ok(n) => {
                response.feed(&bytes[..n], out)?;
                Ok(true)
            }
            Err(e) if retryable(&e) => Ok(false),
            Err(e) => Err(e.to_string()),
        }
    }

    fn request(&mut self, method: &str, path: &str, body: Option<&[u8]>) -> Result<(u16, Vec<u8>), String> {
        let deadline = self.start(method, path, body)?;
        let mut response = Response::default();
        let mut out = Vec::new();
        while !response.done() {
            self.read(&mut response, &mut out, time_left(deadline)?)?;
        }
        let head = response.head.expect("complete response");
        self.reusable = head.persistent;
        Ok((head.code, out))
    }

    /// Once the SSE response opens, 15 s of silence is a dead stream; the server pings every 5 s.
    /// The one-second idle read also lets the loop notice a minimize or an invalidated target.
    fn follow(&mut self, active: impl Fn() -> bool, mut on_tick: impl FnMut(bool) -> Duration) -> Result<(), String> {
        if !active() {
            return Ok(());
        }
        let deadline = self.start("GET", "/api/events", None)?;
        let mut response = Response::default();
        let mut events = EventStream::default();
        let mut out = Vec::new();
        let (mut wait, mut heard) = (IDLE_WAIT, Instant::now());
        while active() {
            let left = if response.head.is_some() {
                time_left(heard + REQUEST_DEADLINE).map_err(|_| "stream went quiet")?
            } else {
                time_left(deadline)?
            };
            if self.read(&mut response, &mut out, wait.min(left))? {
                heard = Instant::now();
            }
            if !active() {
                break;
            }
            if let Some(head) = &response.head
                && head.code != 200
            {
                return Err(format!("change stream answered {}", head.code));
            }
            wait = on_tick(events.feed(&out)?);
            out.clear();
            if response.done() {
                return Err("stream closed".into());
            }
        }
        Ok(())
    }
}

fn retryable(error: &std::io::Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted)
}

fn time_left(deadline: Instant) -> Result<Duration, String> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err("the server took too long to answer".into());
    }
    Ok(left)
}

/// The same framing decoder serves finite responses and the event stream.
#[derive(Default)]
struct Response {
    pending_head: Vec<u8>,
    head: Option<Head>,
    received: usize,
}

struct Head {
    code: u16,
    body: Body,
    persistent: bool,
}

enum Body {
    Length(usize),
    Chunked(Chunked),
    Close,
}

impl Response {
    fn feed(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
        let pending;
        let (head, bytes) = if let Some(head) = &mut self.head {
            (head, bytes)
        } else {
            self.pending_head.extend_from_slice(bytes);
            let Some((head, body_at)) = parse_head(&self.pending_head)? else { return Ok(()) };
            pending = std::mem::take(&mut self.pending_head);
            (self.head.insert(head), &pending[body_at..])
        };
        let before = out.len();
        match &mut head.body {
            Body::Length(left) => {
                *left = left.checked_sub(bytes.len()).ok_or("data after content-length")?;
                out.extend_from_slice(bytes);
            }
            Body::Chunked(decoder) => decoder.feed(bytes, out)?,
            Body::Close => out.extend_from_slice(bytes),
        }
        self.received += out.len() - before;
        if self.received > MAX_BODY {
            return Err("response too large".into());
        }
        Ok(())
    }

    fn done(&self) -> bool {
        self.head.as_ref().is_some_and(|head| match &head.body {
            Body::Length(left) => *left == 0,
            Body::Chunked(decoder) => decoder.done(),
            Body::Close => false,
        })
    }

    fn eof(&mut self) -> Result<(), String> {
        if self.done() {
            return Ok(());
        }
        let head = self.head.as_mut().ok_or("response ended before its head")?;
        match head.body {
            Body::Close => {
                head.body = Body::Length(0);
                Ok(())
            }
            Body::Length(_) => Err("response ended early".into()),
            Body::Chunked(_) => Err("response ended mid-chunk".into()),
        }
    }
}

/// RFC 9112 section 6.3. An incomplete head can still receive more bytes.
fn parse_head(raw: &[u8]) -> Result<Option<(Head, usize)>, String> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4);
    if end.unwrap_or(raw.len()) > MAX_HEAD {
        return Err("response head too long".into());
    }
    let Some(end) = end else { return Ok(None) };
    let head = std::str::from_utf8(&raw[..end - 4]).map_err(|_| "response head is not text")?;
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or_default();
    if status.bytes().any(|b| b.is_ascii_control() && b != b'\t') {
        return Err("malformed status line".into());
    }
    let mut status = status.split(' ');
    let version = status.next().unwrap_or_default();
    if !matches!(version, "HTTP/1.1" | "HTTP/1.0") {
        return Err("not an HTTP/1.x response".into());
    }
    let code = status.next().filter(|s| s.len() == 3 && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse::<u16>().ok()).filter(|c| (200..600).contains(c))
        .ok_or("unsupported or malformed HTTP status")?;
    let (mut chunked, mut length) = (false, None);
    let mut persistent = version == "HTTP/1.1";
    for line in lines {
        let (name, value) = line.split_once(':').ok_or("malformed header field")?;
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^`_|~".contains(&b))
            || value.bytes().any(|b| b.is_ascii_control() && b != b'\t')
        {
            return Err("malformed header field".into());
        }
        let value = value.trim_matches([' ', '\t']);
        if name.eq_ignore_ascii_case("transfer-encoding") {
            if chunked || !value.eq_ignore_ascii_case("chunked") || version != "HTTP/1.1" {
                return Err("unsupported transfer-encoding".into());
            }
            chunked = true;
        } else if name.eq_ignore_ascii_case("content-length") {
            for part in value.split(',') {
                let part = part.trim_matches([' ', '\t']);
                let n = part.parse::<usize>().ok().filter(|_| part.bytes().all(|b| b.is_ascii_digit()))
                    .ok_or("bad content-length")?;
                if length.is_some_and(|old| old != n) {
                    return Err("conflicting content-length values".into());
                }
                length = Some(n);
            }
        } else if name.eq_ignore_ascii_case("connection")
            && value.split(',').any(|part| part.trim_matches([' ', '\t']).eq_ignore_ascii_case("close"))
        {
            persistent = false;
        }
    }
    if chunked && length.is_some() {
        return Err("transfer-encoding with content-length".into());
    }
    let body = match (code, length, chunked) {
        (204, _, true) | (204, Some(_), false) => return Err("framing on a 204 response".into()),
        (204 | 304, _, _) => Body::Length(0),
        (_, Some(n), _) if n > MAX_BODY => return Err("response too large".into()),
        (_, Some(n), _) => Body::Length(n),
        (_, None, true) => Body::Chunked(Chunked::default()),
        (_, None, false) => {
            persistent = false;
            Body::Close
        }
    };
    Ok(Some((Head { code, body, persistent }, end)))
}

/// Incremental `Transfer-Encoding: chunked` decoder (RFC 9112 §7.1). Bounded: a size line longer
/// than `MAX_HEAD`, a size that is not plain hex or overflows, or a chunk not followed by CRLF
/// is an error; the caller bounds the decoded total.
#[derive(Default)]
struct Chunked {
    state: Stage,
    line: Vec<u8>,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum Stage {
    #[default]
    Size,
    Data(usize),
    DataEnd(u8),
    Trailer,
    Done,
}

impl Chunked {
    fn done(&self) -> bool {
        self.state == Stage::Done
    }

    fn feed(&mut self, mut b: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
        while !b.is_empty() {
            match self.state {
                Stage::Size | Stage::Trailer => {
                    let nl = b.iter().position(|&c| c == b'\n');
                    let take = nl.map_or(b.len(), |i| i + 1);
                    self.line.extend_from_slice(&b[..take]);
                    b = &b[take..];
                    if self.line.len() > MAX_HEAD {
                        return Err("chunk header too long".into());
                    }
                    if nl.is_none() {
                        continue;
                    }
                    let line = std::mem::take(&mut self.line);
                    let text = line.strip_suffix(b"\r\n").ok_or("chunk line not ended by CRLF")?;
                    if self.state == Stage::Trailer {
                        if text.is_empty() {
                            self.state = Stage::Done;
                        }
                        continue;
                    }
                    let hex = text.split(|&c| c == b';').next().unwrap_or_default();
                    if hex.is_empty() || !hex.iter().all(u8::is_ascii_hexdigit) {
                        return Err("bad chunk size".into());
                    }
                    let n = usize::from_str_radix(std::str::from_utf8(hex).expect("hex digits"), 16).map_err(|_| "chunk size overflows")?;
                    self.state = if n == 0 { Stage::Trailer } else { Stage::Data(n) };
                }
                Stage::Data(n) => {
                    let k = n.min(b.len());
                    out.extend_from_slice(&b[..k]);
                    b = &b[k..];
                    self.state = if k == n { Stage::DataEnd(0) } else { Stage::Data(n - k) };
                }
                Stage::DataEnd(seen) => {
                    if b[0] != b"\r\n"[usize::from(seen)] {
                        return Err("chunk not followed by CRLF".into());
                    }
                    b = &b[1..];
                    self.state = if seen == 1 { Stage::Size } else { Stage::DataEnd(1) };
                }
                Stage::Done => return Err("data after the last chunk".into()),
            }
        }
        Ok(())
    }
}

/// `feed` says whether a `data:` line (a new revision) arrived in the decoded body.
#[derive(Default)]
struct EventStream {
    line: Vec<u8>,
}

impl EventStream {
    fn feed(&mut self, bytes: &[u8]) -> Result<bool, String> {
        let mut rev = false;
        for &b in bytes {
            if b == b'\n' {
                rev |= self.line.starts_with(b"data:");
                self.line.clear();
            } else if self.line.len() >= MAX_LINE {
                return Err("event line too long".into());
            } else {
                self.line.push(b);
            }
        }
        Ok(rev)
    }
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Fetch on a worker thread and hand the result back through `done`.
pub fn background<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static, done: impl FnOnce(T) + Send + 'static) {
    std::thread::spawn(move || done(job()));
}

fn read_port(room: &Path) -> Option<u16> {
    let text = std::fs::read_to_string(room.join(".trio").join("server.json")).ok()?;
    serde_json::from_str::<ServerInfo>(&text).ok().map(|si| si.port)
}

/// The server for `room`, started if none answers. A server counts only when it reports this
/// room's id; while one is starting (here or from another window), keep re-reading server.json.
pub fn attach(room: &Path) -> Result<Client, String> {
    let room = room.canonicalize().map_err(|e| format!("{}: {e}", crate::roomid::display(room)))?;
    let read = || -> Option<Client> {
        let c = Client::new(&room, read_port(&room)?);
        c.open_verified(c.target()).ok().map(|_| c)
    };
    if let Some(c) = read() {
        return Ok(c);
    }
    start_server(&room)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(150));
        if let Some(c) = read() {
            return Ok(c);
        }
    }
    Err(format!("the Trio server for {} did not start", crate::roomid::display(&room)))
}

/// `trio open <room> --no-browser --port 0`: next to this exe, else on PATH. No console window.
/// If another window's server already holds the room, this one exits and `attach` finds that one.
fn start_server(room: &Path) -> Result<(), String> {
    let name = format!("trio{}", std::env::consts::EXE_SUFFIX);
    let beside = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(&name))).filter(|p| p.exists());
    let exe = beside.unwrap_or_else(|| PathBuf::from(&name));
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("open").arg(room).args(["--no-browser", "--port", "0"]);
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn().map(|_| ()).map_err(|e| format!("could not start trio: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::thread::{self, JoinHandle};

    fn decode(raw: &[u8]) -> Result<(u16, Vec<u8>), String> {
        let mut response = Response::default();
        let mut out = Vec::new();
        response.feed(raw, &mut out)?;
        response.eof()?;
        Ok((response.head.unwrap().code, out))
    }

    #[test]
    fn responses_parse_plain_and_chunked() {
        let plain = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nhi";
        assert_eq!(decode(plain).unwrap(), (200, b"hi".to_vec()));
        let repeated = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\ncontent-length: 2, 2\r\n\r\nhi";
        assert_eq!(decode(repeated).unwrap(), (200, b"hi".to_vec()));
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: ChUnKeD\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\nx-trailer: 1\r\n\r\n";
        assert_eq!(decode(chunked).unwrap(), (200, b"abcde".to_vec()));
        assert_eq!(decode(b"HTTP/1.0 200 OK\r\n\r\nclosed").unwrap(), (200, b"closed".to_vec()));
        assert_eq!(decode(b"HTTP/1.1 204 No Content\r\n\r\n").unwrap(), (204, Vec::new()));
        assert_eq!(encode("P0a b/c"), "P0a%20b%2Fc");
    }

    #[test]
    fn bad_framing_is_refused() {
        let cases: &[&[u8]] = &[
            b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhi",
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n",
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\nffffffffffffffffffff\r\n",
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n+3\r\nabc\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabcX\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\ncontent-length: 99999999999\r\n\r\n",
            b"SSH-2.0-OpenSSH\r\n\r\n",
        ];
        for raw in cases {
            assert!(decode(raw).is_err(), "{}", String::from_utf8_lossy(raw));
        }
        let long_head = [b"HTTP/1.1 200 OK\r\nx: ".as_slice(), &vec![b'a'; MAX_HEAD + 1]].concat();
        assert!(parse_head(&long_head).is_err());
    }

    #[test]
    fn conflicting_content_lengths_are_refused() {
        let separate = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\ncontent-length: 2\r\n\r\nhi";
        let combined = b"HTTP/1.1 200 OK\r\nContent-Length: 3, 2\r\n\r\nhi";
        for raw in [separate.as_slice(), combined] {
            assert_eq!(decode(raw).unwrap_err(), "conflicting content-length values");
        }
    }

    #[test]
    fn only_chunked_is_supported_and_cannot_coexist_with_content_length() {
        for headers in [
            "Transfer-Encoding: gzip, chunked",
            "Transfer-Encoding: gzip",
            "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked",
            "Transfer-Encoding: chunked\r\nContent-Length: 3",
        ] {
            let raw = format!("HTTP/1.1 200 OK\r\n{headers}\r\n\r\n3\r\nhey\r\n0\r\n\r\n");
            assert!(decode(raw.as_bytes()).is_err(), "{headers}");
        }
    }

    #[test]
    fn the_event_stream_decodes_chunks_split_anywhere() {
        // An event line split across two chunks, with the head and chunk boundaries split too.
        let raw = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n3\r\ndat\r\n5\r\na: 7\n\r\n2\r\n:\n\r\n";
        for cut in 1..raw.len() {
            let mut response = Response::default();
            let mut events = EventStream::default();
            let mut out = Vec::new();
            let mut rev = false;
            for bytes in [&raw[..cut], &raw[cut..]] {
                response.feed(bytes, &mut out).unwrap();
                rev |= events.feed(&out).unwrap();
                out.clear();
            }
            assert!(rev, "cut at {cut}");
        }
        let mut events = EventStream::default();
        assert!(!events.feed(b":\n").unwrap());
        assert!(events.feed(&vec![b'x'; MAX_LINE + 1]).is_err());
    }

    fn listen() -> TcpListener {
        TcpListener::bind("127.0.0.1:0").unwrap()
    }

    fn serve(listener: TcpListener, job: impl FnOnce(TcpListener) + Send + 'static) -> (u16, JoinHandle<()>) {
        (listener.local_addr().unwrap().port(), thread::spawn(move || job(listener)))
    }

    fn accept(listener: &TcpListener) -> BufReader<TcpStream> {
        let (socket, _) = listener.accept().unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        BufReader::new(socket)
    }

    fn read_request(socket: &mut BufReader<TcpStream>, expected: &str) -> (String, Vec<u8>) {
        let mut line = String::new();
        socket.read_line(&mut line).unwrap();
        assert_eq!(line, format!("{expected} HTTP/1.1\r\n"));
        let mut length = 0;
        let mut headers = String::new();
        loop {
            line.clear();
            assert_ne!(socket.read_line(&mut line).unwrap(), 0, "incomplete request");
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                length = value.trim().parse().unwrap();
            }
            headers.push_str(&line);
        }
        assert!(!headers.contains("Connection: close"));
        let mut body = vec![0; length];
        socket.read_exact(&mut body).unwrap();
        (headers, body)
    }

    fn snapshot_json(room_id: &str, token: &str) -> String {
        serde_json::json!({
            "room": "room", "room_id": room_id, "rev": 1, "paused": false,
            "hop": 0, "budget": 3, "me": "me", "token": token, "graph": {},
        }).to_string()
    }

    fn state_reply(socket: &mut BufReader<TcpStream>, body: &str) {
        let (_, request_body) = read_request(socket, "GET /api/state");
        assert!(request_body.is_empty());
        write!(socket.get_mut(), "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    }

    fn expect_eof(socket: &mut BufReader<TcpStream>) {
        let mut bytes = Vec::new();
        socket.read_to_end(&mut bytes).unwrap();
        assert!(bytes.is_empty(), "unexpected bytes after response");
    }

    #[test]
    fn content_length_bounds_the_event_stream() {
        let (port, server) = serve(listen(), |listener| {
            for (length, body) in [(8, "data: 1\n"), (2, ":\ndata: 2\n"), (80, ":\n")] {
                let mut socket = accept(&listener);
                read_request(&mut socket, "GET /api/events");
                write!(socket.get_mut(), "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\n\r\n{body}").unwrap();
                if length == 8 {
                    // The stream must finish even when the peer leaves the TCP connection open.
                    expect_eof(&mut socket);
                }
            }
        });
        for (expected_revisions, errors) in [
            (1, ["stream closed", "stream closed"]),
            (0, ["data after content-length", "stream closed"]),
            (0, ["response ended early", "response ended early"]),
        ] {
            let mut conn = Connection::open(port).unwrap();
            let mut revisions = 0;
            let err = conn.follow(|| true, |rev| {
                revisions += usize::from(rev);
                IDLE_WAIT
            }).unwrap_err();
            assert!(errors.contains(&err.as_str()), "{err}");
            assert_eq!(revisions, expected_revisions);
        }
        server.join().unwrap();
    }

    #[test]
    fn writes_prove_the_room_on_the_same_connection() {
        let room = std::env::temp_dir();
        let id = crate::roomid::room_id(&room);
        let body = snapshot_json(&id, "token");
        let snap = serde_json::from_str(&body).unwrap();
        let (port, server) = serve(listen(), move |listener| {
            for path in ["/api/say", "/api/pause"] {
                let mut socket = accept(&listener);
                state_reply(&mut socket, &body);
                let (headers, bytes) = read_request(&mut socket, &format!("POST {path}"));
                let host = listener.local_addr().unwrap();
                assert!(headers.contains(&format!("Origin: http://{host}")));
                let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(json["token"], "token");
                if path == "/api/say" {
                    assert_eq!(json["text"], "draft");
                } else {
                    assert_eq!(json["paused"], true);
                }
                socket.get_mut().write_all(b"HTTP/1.1 204 No Content\r\n\r\n").unwrap();
                expect_eof(&mut socket);
            }
        });
        let client = Client::new(&room, port);
        client.publish(client.target(), snap);
        client.say("token", "draft").unwrap();
        client.pause("token", true).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn another_room_receives_neither_draft_nor_token() {
        let room = std::env::temp_dir();
        let body = snapshot_json("another-room", "other-token");
        let (port, server) = serve(listen(), move |listener| {
            for body in [body.as_str(), r#"{"room_id":"another-room"}"#] {
                let mut socket = accept(&listener);
                // The request consists only of the state GET and its ordinary headers.
                state_reply(&mut socket, body);
                expect_eof(&mut socket);
            }
        });
        let client = Client::new(&room, port);
        let snap: Snapshot = serde_json::from_str(&snapshot_json(&client.id, "secret-token")).unwrap();
        client.publish(client.target(), snap.clone());
        assert!(client.say("secret-token", "private draft").unwrap_err().contains("another room"));
        assert!(client.shared().snap.is_none());
        client.publish(client.target(), snap);
        assert!(client.pause("secret-token", true).unwrap_err().contains("another room"));
        server.join().unwrap();
    }

    #[test]
    fn a_peer_with_another_token_never_receives_the_draft() {
        // Same room id (a restarted server, or a listener claiming the id), different token.
        let room = std::env::temp_dir();
        let id = crate::roomid::room_id(&room);
        let body = snapshot_json(&id, "new-token");
        let (port, server) = serve(listen(), move |listener| {
            let mut socket = accept(&listener);
            state_reply(&mut socket, &body);
            expect_eof(&mut socket);
        });
        let client = Client::new(&room, port);
        let snap: Snapshot = serde_json::from_str(&snapshot_json(&client.id, "old-token")).unwrap();
        client.publish(client.target(), snap);
        assert!(client.say("old-token", "private draft").unwrap_err().contains("restarted"));
        assert_eq!(client.shared().snap.as_ref().unwrap().token, "new-token", "the proof's snapshot was not adopted");
        server.join().unwrap();
    }

    #[test]
    fn a_stream_failure_clears_the_snapshot() {
        let room = std::env::temp_dir();
        let client = Client::new(&room, 1);
        let snap: Snapshot = serde_json::from_str(&snapshot_json(&client.id, "t")).unwrap();
        client.publish(client.target(), snap);
        client.record_failure(client.target(), &RequestError::Other("stream closed".into()), true);
        let s = client.shared();
        assert!(s.snap.is_none(), "a stale snapshot stayed active after the stream was lost");
        assert!(s.error.is_some());
    }

    #[test]
    fn a_rejected_refresh_invalidates_state_and_rediscovers_with_the_stream_still_open() {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let room = std::env::temp_dir().join(format!("trio-app-net-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(room.join(".trio")).unwrap();
        let marker = room.join(".trio").join("server.json");
        let id = crate::roomid::room_id(&room);
        let initial = snapshot_json(&id, "old");
        let recovered = snapshot_json(&id, "new");
        let visible = Arc::new(AtomicBool::new(true));
        let hide = visible.clone();
        let (new_port, fresh) = serve(listen(), move |listener| {
            let mut stream = accept(&listener);
            state_reply(&mut stream, &recovered);
            read_request(&mut stream, "GET /api/events");
            hide.store(false, Ordering::Relaxed);
            expect_eof(&mut stream);
        });
        let update = marker.clone();
        let old_snap: Snapshot = serde_json::from_str(&initial).unwrap();
        let (old_port, stale) = serve(listen(), move |listener| {
            state_reply(&mut accept(&listener), &initial); // attach probe
            let mut stream = accept(&listener);
            state_reply(&mut stream, &initial);
            read_request(&mut stream, "GET /api/events");
            std::fs::write(update, format!(r#"{{"port":{new_port}}}"#)).unwrap();
            stream.get_mut().write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n8\r\ndata: 2\n\r\n").unwrap();
            state_reply(&mut accept(&listener), &snapshot_json("another-room", "ignored"));
            // Rediscovery must close this stream without waiting for the server to close it.
            expect_eof(&mut stream);
        });
        std::fs::write(&marker, format!(r#"{{"port":{old_port}}}"#)).unwrap();
        let client = attach(&room).unwrap();
        assert_eq!(client.room, room.canonicalize().unwrap());
        let old_target = client.target();
        client.watch(old_target, &visible).unwrap();
        stale.join().unwrap();
        assert!(client.shared().snap.is_none());
        assert!(client.shared().error.as_deref().unwrap().contains("another room"));
        assert_eq!(client.target().port, new_port);
        // An already-started snapshot request must not resurrect the invalidated state.
        client.publish(old_target, old_snap);
        assert!(client.shared().snap.is_none());
        assert!(client.say("old", "draft").is_err());
        client.watch(client.target(), &visible).unwrap();
        fresh.join().unwrap();
        assert_eq!(client.shared().snap.as_ref().unwrap().token, "new");
        assert!(client.shared().error.is_none());
        std::fs::remove_dir_all(room).unwrap();
    }
}
