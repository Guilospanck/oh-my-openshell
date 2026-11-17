//! HTTP/1.1 for the MCP server, written by hand on `std::net`: one `POST /mcp` route carrying
//! a JSON-RPC body to `crate::mcp::handle`. Everything a stranger can send is bounded (header
//! and body size, total time to deliver a request, simultaneous connections) and nothing in the
//! parser panics on input.

use crate::mcp;
use crate::mcpserver::token;
use crate::ops::Ctx;
use std::cell::Cell;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// What a stranger may make the server spend.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Request line plus headers, in bytes.
    pub max_header: usize,
    pub max_body: usize,
    /// Time to deliver one whole request (not per read: a trickle must not hold a thread).
    pub read_timeout: Duration,
    /// Connections served at once; more are answered `503`.
    pub max_conns: usize,
    /// Connections one address may hold at once, so a single peer cannot take every slot.
    pub max_per_ip: usize,
    /// Time to send one whole response to a client that reads slowly.
    pub write_timeout: Duration,
    /// An address (besides localhost) whose `Origin` is accepted: the one the server listens on.
    pub origin_host: Option<IpAddr>,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_header: 16 * 1024,
            max_body: 4 * 1024 * 1024,
            read_timeout: Duration::from_secs(10),
            max_conns: 64,
            max_per_ip: 16,
            write_timeout: Duration::from_secs(60),
            origin_host: None,
        }
    }
}

pub struct Request {
    pub method: String,
    pub path: String,
    /// Names lowercased, values trimmed.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum HttpError {
    /// The peer went away (or sent nothing): no answer.
    Closed,
    Timeout,
    Bad(&'static str),
    HeaderTooLarge,
    BodyTooLarge,
    LengthRequired,
    NotImplemented,
}

impl HttpError {
    fn status(&self) -> u16 {
        match self {
            HttpError::Closed | HttpError::Bad(_) => 400,
            HttpError::Timeout => 408,
            HttpError::LengthRequired => 411,
            HttpError::BodyTooLarge => 413,
            HttpError::HeaderTooLarge => 431,
            HttpError::NotImplemented => 501,
        }
    }

    fn message(&self) -> &'static str {
        match self {
            HttpError::Closed => "closed",
            HttpError::Timeout => "the request took too long to arrive",
            HttpError::Bad(m) => m,
            HttpError::HeaderTooLarge => "request headers too large",
            HttpError::BodyTooLarge => "request body too large",
            HttpError::LengthRequired => "Content-Length is required",
            HttpError::NotImplemented => "chunked bodies are not supported",
        }
    }
}

fn io_err(e: io::Error) -> HttpError {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => HttpError::Timeout,
        _ => HttpError::Closed,
    }
}

/// One header or request line, without its line ending; `used` counts bytes toward `max`.
fn read_line(r: &mut impl BufRead, max: usize, used: &mut usize) -> Result<String, HttpError> {
    let mut buf = Vec::new();
    let room = max.saturating_sub(*used) as u64 + 1;
    let n = (&mut *r)
        .take(room)
        .read_until(b'\n', &mut buf)
        .map_err(io_err)?;
    if n == 0 {
        return Err(HttpError::Closed);
    }
    *used += n;
    if *used > max {
        return Err(HttpError::HeaderTooLarge);
    }
    if buf.last() != Some(&b'\n') {
        return Err(HttpError::Bad("truncated request"));
    }
    buf.pop();
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    String::from_utf8(buf).map_err(|_| HttpError::Bad("request is not text"))
}

/// The request line and headers; the body is left unread (its length is returned) so a request
/// can be refused before its body is taken.
pub fn read_head(r: &mut impl BufRead, l: &Limits) -> Result<(Request, usize), HttpError> {
    let mut used = 0usize;
    let line = read_line(r, l.max_header, &mut used)?;
    let mut parts = line.split(' ');
    let (Some(method), Some(path), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(HttpError::Bad("bad request line"));
    };
    if !version.starts_with("HTTP/1.")
        || method.is_empty()
        || !method.bytes().all(|b| b.is_ascii_uppercase())
        || !path.starts_with('/')
    {
        return Err(HttpError::Bad("bad request line"));
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let line = read_line(r, l.max_header, &mut used).map_err(|e| match e {
            HttpError::Closed => HttpError::Bad("truncated headers"),
            other => other,
        })?;
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HttpError::Bad("bad header"));
        };
        // No spaces or control characters in a name: this also refuses obsolete line folding.
        if name.is_empty() || name.bytes().any(|b| b <= b' ' || b >= 0x7f) {
            return Err(HttpError::Bad("bad header name"));
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
        if headers.len() > 100 {
            return Err(HttpError::HeaderTooLarge);
        }
    }
    let req = Request {
        method: method.to_string(),
        path: path.to_string(),
        headers,
        body: Vec::new(),
    };
    if req.header("transfer-encoding").is_some() {
        return Err(HttpError::NotImplemented);
    }
    let mut lengths = req
        .headers
        .iter()
        .filter(|(k, _)| k == "content-length")
        .map(|(_, v)| v.as_str());
    let len = match lengths.next() {
        None => {
            if req.method == "POST" {
                return Err(HttpError::LengthRequired);
            }
            0
        }
        Some(first) => {
            if lengths.any(|other| other != first) {
                return Err(HttpError::Bad("conflicting Content-Length"));
            }
            if first.is_empty() || first.len() > 18 || !first.bytes().all(|b| b.is_ascii_digit()) {
                return Err(HttpError::Bad("bad Content-Length"));
            }
            first
                .parse::<usize>()
                .map_err(|_| HttpError::Bad("bad Content-Length"))?
        }
    };
    if len > l.max_body {
        return Err(HttpError::BodyTooLarge);
    }
    Ok((req, len))
}

/// Read the `len` body bytes `read_head` announced.
pub fn read_body(r: &mut impl BufRead, req: &mut Request, len: usize) -> Result<(), HttpError> {
    let mut body = Vec::with_capacity(len.min(64 * 1024));
    (&mut *r)
        .take(len as u64)
        .read_to_end(&mut body)
        .map_err(io_err)?;
    if body.len() < len {
        return Err(HttpError::Bad("the body ended early"));
    }
    req.body = body;
    Ok(())
}

pub fn read_request(r: &mut impl BufRead, l: &Limits) -> Result<Request, HttpError> {
    let (mut req, len) = read_head(r, l)?;
    read_body(r, &mut req, len)?;
    Ok(req)
}

pub fn write_response(
    w: &mut impl Write,
    status: u16,
    headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        // A value with a line break would let a header smuggle another one.
        if !k.contains(['\r', '\n']) && !v.contains(['\r', '\n']) {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    w.write_all(body)?;
    w.flush()
}

/// Told about every authenticated request, to keep the list of connected clients.
pub trait Observer: Send + Sync {
    /// `client` is the name and version from `initialize`, when this request was one.
    fn seen(&self, session: &str, peer: &str, client: Option<(String, String)>);
}

/// A reader that gives up when the request as a whole has taken too long, however slowly the
/// bytes come.
struct Timed {
    stream: TcpStream,
    until: Rc<Cell<Instant>>,
    /// Set when any byte of the current request has arrived.
    got: Rc<Cell<bool>>,
}

impl Read for Timed {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.until.get().saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.stream.set_read_timeout(Some(left))?;
        let n = self.stream.read(buf)?;
        if n > 0 {
            self.got.set(true);
        }
        Ok(n)
    }
}

fn new_session_id() -> String {
    let mut b = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_ok();
    if !ok {
        // Only if the system has no /dev/urandom: unique enough for a label.
        static N: AtomicUsize = AtomicUsize::new(0);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        b = (t ^ ((N.fetch_add(1, Ordering::Relaxed) as u128) << 64)).to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A session id from a client, if it is short printable ASCII.
fn clean_session(v: &str) -> Option<String> {
    let v = v.trim();
    (!v.is_empty() && v.len() <= 64 && v.bytes().all(|b| (0x21..0x7f).contains(&b)))
        .then(|| v.to_string())
}

type Reply = (u16, Vec<(String, String)>, Vec<u8>);

fn plain(status: u16, headers: &[(&str, &str)], body: &str) -> Reply {
    (
        status,
        headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body.as_bytes().to_vec(),
    )
}

/// An `Origin` a browser could only send from a page on this machine or on the address the
/// server listens on. A page on any other site (including one that points its own name at this
/// machine, which is how DNS rebinding works) is refused. Clients that are not browsers send none.
fn origin_allowed(origin: &str, extra: Option<IpAddr>) -> bool {
    let Some((_, rest)) = origin.split_once("://") else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or("");
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
        || extra.is_some_and(|ip| ip.to_string() == host)
}

/// Everything that can be decided from the request line and headers: the route, the origin and
/// the token. `Some` is the refusal, sent before the body is read.
fn gate(req: &Request, dir: &Path, l: &Limits) -> Option<Reply> {
    if req.path != "/mcp" {
        return Some(plain(404, &[("Content-Type", "text/plain")], "not found"));
    }
    if req.method != "POST" {
        return Some(plain(
            405,
            &[("Allow", "POST"), ("Content-Type", "text/plain")],
            "use POST",
        ));
    }
    if req
        .header("origin")
        .is_some_and(|o| !origin_allowed(o, l.origin_host))
    {
        return Some(plain(
            403,
            &[("Content-Type", "text/plain")],
            "this origin is not allowed",
        ));
    }
    let authorised = req
        .header("authorization")
        .and_then(|a| a.split_once(' '))
        .is_some_and(|(scheme, t)| {
            scheme.eq_ignore_ascii_case("bearer") && token::matches(dir, t.trim())
        });
    if !authorised {
        return Some(plain(
            401,
            &[
                ("WWW-Authenticate", "Bearer"),
                ("Content-Type", "text/plain"),
            ],
            "a bearer token is required",
        ));
    }
    None
}

fn answer(
    req: &Request,
    peer: SocketAddr,
    ctx: &(dyn Fn() -> Ctx + Send + Sync),
    obs: &dyn Observer,
) -> Reply {
    let json = [("Content-Type", "application/json")];
    let Ok(text) = std::str::from_utf8(&req.body) else {
        return plain(
            200,
            &json,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#,
        );
    };
    let parsed: Option<serde_json::Value> = serde_json::from_str(text).ok();
    let is_init = parsed
        .as_ref()
        .and_then(|v| v["method"].as_str())
        .is_some_and(|m| m == "initialize");
    let client = is_init
        .then(|| {
            let info = &parsed.as_ref()?["params"]["clientInfo"];
            Some((
                info["name"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect(),
                info["version"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(40)
                    .collect(),
            ))
        })
        .flatten();
    let ip = peer.ip().to_string();
    let (session, handed_out) = if is_init {
        (new_session_id(), true)
    } else {
        let own = req.header("mcp-session-id").and_then(clean_session);
        (own.unwrap_or_else(|| format!("ip:{ip}")), false)
    };
    obs.seen(&session, &ip, client);
    let mut headers: Vec<(String, String)> = Vec::new();
    if handed_out {
        headers.push(("Mcp-Session-Id".into(), session));
    }
    match mcp::handle(text.trim(), &ctx()) {
        Some(line) => {
            headers.push(("Content-Type".into(), "application/json".into()));
            (200, headers, line.into_bytes())
        }
        None => (202, headers, Vec::new()),
    }
}

/// A writer that gives up when the whole response has taken too long, however slowly the
/// client reads it.
struct TimedWriter<'a> {
    stream: &'a mut TcpStream,
    until: Instant,
}

impl Write for TimedWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.stream.set_write_timeout(Some(left))?;
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// Close after an answer the client may still be sending its request into: say we are done,
/// then take (a bounded amount of) what is still arriving, so the close does not become a
/// connection reset that wipes the answer before the client reads it.
fn close_politely(stream: &mut TcpStream) {
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut sink = [0u8; 8192];
    let mut taken = 0usize;
    while taken < 256 * 1024 && Instant::now() < deadline {
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => taken += n,
        }
    }
}

fn send(
    out: &mut TcpStream,
    l: &Limits,
    status: u16,
    headers: &[(&str, &str)],
    body: &[u8],
) -> bool {
    let mut w = TimedWriter {
        stream: out,
        until: Instant::now() + l.write_timeout,
    };
    write_response(&mut w, status, headers, body).is_ok()
}

fn connection(
    stream: TcpStream,
    peer: SocketAddr,
    dir: &Path,
    ctx: &(dyn Fn() -> Ctx + Send + Sync),
    obs: &dyn Observer,
    l: &Limits,
) {
    let until = Rc::new(Cell::new(Instant::now()));
    let got = Rc::new(Cell::new(false));
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(Timed {
        stream: read_half,
        until: until.clone(),
        got: got.clone(),
    });
    let mut out = stream;
    let fail = |out: &mut TcpStream, e: HttpError| {
        let _ = send(
            out,
            l,
            e.status(),
            &[("Content-Type", "text/plain"), ("Connection", "close")],
            e.message().as_bytes(),
        );
        close_politely(out);
    };
    loop {
        until.set(Instant::now() + l.read_timeout);
        got.set(false);
        let (mut req, len) = match read_head(&mut reader, l) {
            Ok(r) => r,
            Err(HttpError::Closed) => return,
            // A keep-alive connection that stayed quiet is closed, not answered.
            Err(HttpError::Timeout) if !got.get() => return,
            Err(e) => return fail(&mut out, e),
        };
        if let Some((status, mut headers, body)) = gate(&req, dir, l) {
            headers.push(("Connection".into(), "close".into()));
            let borrowed: Vec<(&str, &str)> = headers
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let _ = send(&mut out, l, status, &borrowed, &body);
            close_politely(&mut out);
            return;
        }
        if let Err(e) = read_body(&mut reader, &mut req, len) {
            if e == HttpError::Closed {
                return;
            }
            return fail(&mut out, e);
        }
        let keep = !req
            .header("connection")
            .is_some_and(|c| c.eq_ignore_ascii_case("close"));
        let (status, mut headers, body) = answer(&req, peer, ctx, obs);
        if !keep {
            headers.push(("Connection".into(), "close".into()));
        }
        let borrowed: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        if !send(&mut out, l, status, &borrowed, &body) || !keep {
            return;
        }
    }
}

/// Counts a connection while a thread serves it, however the thread ends.
struct Live(Arc<AtomicUsize>);

impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Answer `503` and hang up, without ever waiting on the peer: the accept loop must not stall.
/// What the peer has already sent is taken first, because closing a socket with unread data
/// resets the connection and the peer may then never see the answer.
fn refuse(mut s: TcpStream) {
    let _ = s.set_nonblocking(true);
    let _ = s.read(&mut [0u8; 8192]);
    let _ = s.set_nonblocking(false);
    let _ = s.set_write_timeout(Some(Duration::from_secs(1)));
    let _ = write_response(
        &mut s,
        503,
        &[("Content-Type", "text/plain"), ("Connection", "close")],
        b"too many connections",
    );
    let _ = s.shutdown(Shutdown::Write);
}

/// One connection counted against its peer's address while a thread serves it.
struct IpSlot {
    map: Arc<Mutex<HashMap<IpAddr, usize>>>,
    ip: IpAddr,
    /// How many this address holds, this one included.
    held: usize,
}

impl IpSlot {
    fn take(map: &Arc<Mutex<HashMap<IpAddr, usize>>>, ip: IpAddr) -> IpSlot {
        let mut m = map.lock().unwrap_or_else(|e| e.into_inner());
        let n = m.entry(ip).or_insert(0);
        *n += 1;
        IpSlot {
            map: map.clone(),
            ip,
            held: *n,
        }
    }
}

impl Drop for IpSlot {
    fn drop(&mut self) {
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = m.get_mut(&self.ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&self.ip);
            }
        }
    }
}

/// Accept connections until `stop` is set. Each one is served on its own thread, up to
/// `limits.max_conns`; `ctx` is called for every request so changed settings apply at once.
pub fn serve(
    listener: TcpListener,
    dir: PathBuf,
    ctx: Arc<dyn Fn() -> Ctx + Send + Sync>,
    obs: Arc<dyn Observer>,
    limits: Limits,
    stop: Arc<AtomicBool>,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let live = Arc::new(AtomicUsize::new(0));
    let per_ip: Arc<Mutex<HashMap<IpAddr, usize>>> = Arc::default();
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                // Some systems hand the accepted socket the listener's non-blocking mode.
                let _ = stream.set_nonblocking(false);
                let guard = Live(live.clone());
                let slot = IpSlot::take(&per_ip, peer.ip());
                if live.fetch_add(1, Ordering::SeqCst) >= limits.max_conns
                    || slot.held > limits.max_per_ip
                {
                    refuse(stream);
                    drop(guard);
                    drop(slot);
                    continue;
                }
                let (dir, ctx, obs) = (dir.clone(), ctx.clone(), obs.clone());
                let spawned = thread::Builder::new()
                    .name("osh-mcp-conn".into())
                    .spawn(move || {
                        let _guard = guard;
                        let _slot = slot;
                        connection(stream, peer, &dir, &*ctx, &*obs, &limits);
                    });
                // A thread that cannot start drops its closure, and with it the count.
                let _ = spawned;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25))
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // Out of descriptors or memory, an aborted connection and the like are the machine's
            // trouble of the moment: wait and go on, so requests already being served finish.
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    type Seen = Arc<Mutex<Vec<(String, String, Option<(String, String)>)>>>;
    struct Rec(Seen);
    impl Observer for Rec {
        fn seen(&self, session: &str, peer: &str, client: Option<(String, String)>) {
            self.0
                .lock()
                .unwrap()
                .push((session.into(), peer.into(), client));
        }
    }

    struct Server {
        addr: SocketAddr,
        dir: Tmp,
        seen: Seen,
        stop: Arc<AtomicBool>,
        join: Option<thread::JoinHandle<()>>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
        }
    }

    fn quick() -> Limits {
        Limits {
            read_timeout: Duration::from_secs(3),
            ..Limits::default()
        }
    }

    fn start(tag: &str, limits: Limits) -> Server {
        let dir = std::env::temp_dir().join(format!("osh-http-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(token::path(&dir), "tok-123\n").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Seen::default();
        let stop = Arc::new(AtomicBool::new(false));
        let ctx_dir = dir.clone();
        let ctx: Arc<dyn Fn() -> Ctx + Send + Sync> = Arc::new(move || Ctx {
            dir: ctx_dir.clone(),
            gateway: None,
            allow_share: false,
            refuse_risky_share: true,
            repo_vars: Some(Vec::new()),
            share_roots: Some(Vec::new()),
            version: "t-1".into(),
            job_max_secs: 0,
        });
        let (d, s, st) = (dir.clone(), seen.clone(), stop.clone());
        let join = thread::spawn(move || {
            serve(listener, d, ctx, Arc::new(Rec(s)), limits, st).unwrap();
        });
        Server {
            addr,
            dir: Tmp(dir),
            seen,
            stop,
            join: Some(join),
        }
    }

    struct Resp {
        status: u16,
        head: String,
        body: String,
    }

    fn parse(raw: &str) -> Resp {
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Resp {
            status,
            head: head.to_string(),
            body: body.to_string(),
        }
    }

    /// Send bytes and read until the server closes (or 5 s pass).
    fn exchange(addr: SocketAddr, bytes: &[u8]) -> Resp {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let _ = s.write_all(bytes);
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        parse(&String::from_utf8_lossy(&out))
    }

    fn post(s: &Server, bearer: Option<&str>, body: &str) -> Resp {
        let auth = bearer.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: x\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        exchange(s.addr, req.as_bytes())
    }

    const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"factory","version":"0.3"}}}"#;

    #[test]
    fn a_valid_initialize_returns_json_and_a_session_id() {
        let s = start("init", quick());
        let r = post(&s, Some("tok-123"), INIT);
        assert_eq!(r.status, 200, "{}", r.head);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["result"]["serverInfo"]["name"], "osh");
        assert_eq!(v["result"]["serverInfo"]["version"], "t-1");
        let sid = r
            .head
            .lines()
            .find_map(|l| l.strip_prefix("Mcp-Session-Id: "))
            .expect("a session id header")
            .to_string();
        assert_eq!(sid.len(), 32);
        let seen = s.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, sid);
        assert_eq!(seen[0].1, "127.0.0.1");
        assert_eq!(seen[0].2, Some(("factory".into(), "0.3".into())));
    }

    #[test]
    fn a_repeat_with_the_session_id_is_reported_under_it() {
        let s = start("repeat", quick());
        let body = r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#;
        let req = format!(
            "POST /mcp HTTP/1.1\r\nAuthorization: Bearer tok-123\r\nMcp-Session-Id: abc-def\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let r = exchange(s.addr, req.as_bytes());
        assert_eq!(r.status, 200);
        assert_eq!(s.seen.lock().unwrap()[0].0, "abc-def");
        assert!(
            !r.head.contains("Mcp-Session-Id"),
            "only initialize hands one out"
        );
    }

    #[test]
    fn a_client_without_a_session_is_listed_by_its_address() {
        let s = start("nosession", quick());
        post(
            &s,
            Some("tok-123"),
            r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
        );
        assert_eq!(s.seen.lock().unwrap()[0].0, "ip:127.0.0.1");
    }

    #[test]
    fn no_bearer_is_401_and_wrong_bearer_is_401_and_nothing_runs() {
        let s = start("auth", quick());
        for bearer in [None, Some("nope"), Some("tok-12"), Some("tok-1234")] {
            let r = post(&s, bearer, INIT);
            assert_eq!(r.status, 401, "{bearer:?}: {}", r.head);
            assert!(r.head.contains("WWW-Authenticate: Bearer"));
            assert!(!r.body.contains("serverInfo"));
        }
        assert!(s.seen.lock().unwrap().is_empty(), "never reported");
    }

    #[test]
    fn a_replaced_token_works_on_the_next_request() {
        let s = start("rotate", quick());
        assert_eq!(post(&s, Some("tok-123"), INIT).status, 200);
        std::fs::write(token::path(&s.dir.0), "fresh-token\n").unwrap();
        assert_eq!(post(&s, Some("tok-123"), INIT).status, 401);
        assert_eq!(post(&s, Some("fresh-token"), INIT).status, 200);
    }

    #[test]
    fn get_is_405_and_an_unknown_path_is_404() {
        let s = start("routes", quick());
        let r = exchange(
            s.addr,
            b"GET /mcp HTTP/1.1\r\nAuthorization: Bearer tok-123\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(r.status, 405);
        assert!(r.head.contains("Allow: POST"));
        let r = exchange(
            s.addr,
            b"POST /other HTTP/1.1\r\nAuthorization: Bearer tok-123\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn an_oversized_body_is_413_without_reading_it() {
        let s = start(
            "bigbody",
            Limits {
                max_body: 100,
                ..quick()
            },
        );
        let started = Instant::now();
        let r = exchange(
            s.addr,
            b"POST /mcp HTTP/1.1\r\nAuthorization: Bearer tok-123\r\nContent-Length: 99999999\r\n\r\nabc",
        );
        assert_eq!(r.status, 413);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "did not wait for the body"
        );
    }

    #[test]
    fn an_oversized_header_is_431() {
        let s = start(
            "bighead",
            Limits {
                max_header: 256,
                ..quick()
            },
        );
        let req = format!(
            "POST /mcp HTTP/1.1\r\nX-Filler: {}\r\nContent-Length: 0\r\n\r\n",
            "a".repeat(1000)
        );
        assert_eq!(exchange(s.addr, req.as_bytes()).status, 431);
    }

    #[test]
    fn a_slow_client_is_cut_off_at_the_read_timeout() {
        let s = start(
            "slow",
            Limits {
                read_timeout: Duration::from_millis(300),
                ..Limits::default()
            },
        );
        let mut c = TcpStream::connect(s.addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let started = Instant::now();
        // One byte every 60 ms for 3 s: each read is quick, the request as a whole never ends.
        let mut w = c.try_clone().unwrap();
        let trickle = thread::spawn(move || {
            for b in b"POST /mcp HTTP/1.1\r\nX-A: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .iter()
                .cycle()
                .take(50)
            {
                if w.write_all(&[*b]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(60));
            }
        });
        let mut out = Vec::new();
        let _ = c.read_to_end(&mut out);
        let waited = started.elapsed();
        let r = parse(&String::from_utf8_lossy(&out));
        assert_eq!(r.status, 408, "{}", r.head);
        assert!(
            waited < Duration::from_millis(1500),
            "cut off by the deadline for the whole request, not by a quiet read: {waited:?}"
        );
        let _ = trickle.join();
    }

    #[test]
    fn garbage_gets_400_and_the_server_keeps_serving() {
        let s = start("garbage", quick());
        assert_eq!(
            exchange(s.addr, b"\x00\x01\x02 garbage\r\n\r\n").status,
            400
        );
        assert_eq!(exchange(s.addr, b"GET\r\n\r\n").status, 400);
        assert_eq!(
            exchange(s.addr, b"POST /mcp HTTP/1.1\r\n bad folded: x\r\n\r\n").status,
            400
        );
        assert_eq!(post(&s, Some("tok-123"), INIT).status, 200);
    }

    #[test]
    fn connections_over_the_cap_get_503() {
        let s = start(
            "cap",
            Limits {
                max_conns: 1,
                ..quick()
            },
        );
        let _held = TcpStream::connect(s.addr).unwrap();
        thread::sleep(Duration::from_millis(150));
        let r = exchange(s.addr, b"GET / HTTP/1.1\r\n\r\n");
        assert_eq!(r.status, 503, "{}", r.head);
    }

    #[test]
    fn a_notification_gets_202_and_no_body() {
        let s = start("notify", quick());
        let r = post(
            &s,
            Some("tok-123"),
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        );
        assert_eq!(r.status, 202);
        assert!(r.body.is_empty());
    }

    #[test]
    fn chunked_is_501_and_a_post_without_a_length_is_411() {
        let s = start("framing", quick());
        let r = exchange(
            s.addr,
            b"POST /mcp HTTP/1.1\r\nAuthorization: Bearer tok-123\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        );
        assert_eq!(r.status, 501);
        let r = exchange(
            s.addr,
            b"POST /mcp HTTP/1.1\r\nAuthorization: Bearer tok-123\r\n\r\n",
        );
        assert_eq!(r.status, 411);
    }

    #[test]
    fn a_body_that_is_not_utf8_is_a_json_rpc_parse_error() {
        let s = start("utf8", quick());
        let mut req = b"POST /mcp HTTP/1.1\r\nAuthorization: Bearer tok-123\r\nContent-Length: 3\r\nConnection: close\r\n\r\n".to_vec();
        req.extend_from_slice(&[0xff, 0xfe, 0xfd]);
        let r = exchange(s.addr, &req);
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["error"]["code"], -32700);
    }

    #[test]
    fn keep_alive_serves_two_requests_on_one_connection() {
        let s = start("alive", quick());
        let mut c = TcpStream::connect(s.addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let body = r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#;
        let req = format!(
            "POST /mcp HTTP/1.1\r\nAuthorization: Bearer tok-123\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        for _ in 0..2 {
            c.write_all(req.as_bytes()).unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 512];
            while !String::from_utf8_lossy(&got).contains("\"result\"") {
                let n = c.read(&mut buf).unwrap();
                assert!(n > 0, "connection closed early");
                got.extend_from_slice(&buf[..n]);
            }
        }
    }

    #[test]
    fn a_refused_request_is_answered_before_its_body_is_read() {
        let s = start("early", quick());
        let started = Instant::now();
        // Headers only, a 1000 byte body promised and never sent: a server that reads the body
        // first would wait out the whole read timeout before it said 401.
        let r = exchange(
            s.addr,
            b"POST /mcp HTTP/1.1\r\nContent-Length: 1000\r\n\r\n",
        );
        assert_eq!(r.status, 401, "{}", r.head);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_browser_request_from_another_origin_is_refused() {
        let s = start("origin", quick());
        let with = |origin: &str| {
            let body = r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#;
            let req = format!(
                "POST /mcp HTTP/1.1\r\nAuthorization: Bearer tok-123\r\nOrigin: {origin}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            exchange(s.addr, req.as_bytes()).status
        };
        assert_eq!(with("http://evil.example"), 403);
        assert_eq!(with("null"), 403);
        assert_eq!(with("http://localhost:3000"), 200);
        assert_eq!(with("http://127.0.0.1:17800"), 200);
        assert_eq!(with("http://[::1]:5173"), 200);
        assert_eq!(
            post(&s, Some("tok-123"), INIT).status,
            200,
            "no Origin: not a browser"
        );
    }

    #[test]
    fn the_configured_address_is_an_allowed_origin() {
        let s = start(
            "originhost",
            Limits {
                origin_host: Some("10.1.2.3".parse().unwrap()),
                ..quick()
            },
        );
        let body = r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#;
        let req = format!(
            "POST /mcp HTTP/1.1\r\nAuthorization: Bearer tok-123\r\nOrigin: http://10.1.2.3:17800\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        assert_eq!(exchange(s.addr, req.as_bytes()).status, 200);
    }

    #[test]
    fn one_peer_cannot_take_every_slot() {
        let s = start(
            "perip",
            Limits {
                max_per_ip: 2,
                ..quick()
            },
        );
        let _a = TcpStream::connect(s.addr).unwrap();
        let _b = TcpStream::connect(s.addr).unwrap();
        thread::sleep(Duration::from_millis(150));
        let r = exchange(s.addr, b"GET / HTTP/1.1\r\n\r\n");
        assert_eq!(r.status, 503, "{}", r.head);
        assert!(r.body.contains("too many"), "{}", r.body);
    }

    #[test]
    fn an_idle_connection_is_closed_without_an_answer() {
        let s = start(
            "idle",
            Limits {
                read_timeout: Duration::from_millis(300),
                ..Limits::default()
            },
        );
        let mut c = TcpStream::connect(s.addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut out = Vec::new();
        let _ = c.read_to_end(&mut out);
        assert!(
            out.is_empty(),
            "nothing was asked, nothing is answered: {out:?}"
        );
    }

    #[test]
    fn the_refusal_reaches_a_client_that_keeps_sending() {
        let s = start(
            "drain",
            Limits {
                max_body: 100,
                ..quick()
            },
        );
        let mut c = TcpStream::connect(s.addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c.write_all(
            b"POST /mcp HTTP/1.1\r\nAuthorization: Bearer tok-123\r\nContent-Length: 60000\r\n\r\n",
        )
        .unwrap();
        // The body follows although the server will not take it.
        let _ = c.write_all(&vec![b'x'; 60000]);
        let mut out = Vec::new();
        let _ = c.read_to_end(&mut out);
        assert_eq!(parse(&String::from_utf8_lossy(&out)).status, 413);
    }

    #[test]
    fn read_request_enforces_framing_rules() {
        let l = Limits::default();
        let ok = b"POST /mcp HTTP/1.1\r\nCONTENT-length: 4\r\nX: y\r\n\r\nabcdEXTRA";
        let r = read_request(&mut &ok[..], &l).unwrap();
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/mcp"));
        assert_eq!(r.body, b"abcd");
        assert_eq!(r.header("content-length"), Some("4"));
        for bad in [
            &b"POST /mcp HTTP/1.1\r\nContent-Length: 4\r\nContent-Length: 5\r\n\r\nabcde"[..],
            b"POST /mcp HTTP/1.1\r\nContent-Length: -1\r\n\r\n",
            b"POST /mcp HTTP/1.1\r\nContent-Length: 4\r\n\r\nab",
            b"POST /mcp HTTP/2\r\nContent-Length: 0\r\n\r\n",
            b"post /mcp HTTP/1.1\r\nContent-Length: 0\r\n\r\n",
        ] {
            assert!(
                read_request(&mut &bad[..], &l).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        assert!(matches!(
            read_request(&mut &b""[..], &l),
            Err(HttpError::Closed)
        ));
    }
}
