//! The listener, connection limits and request parsing.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use super::api::handle_client;
use super::state::{NowPlayingState, WebCommand};
use super::stream::StreamServerState;

/// Network interfaces the web server listens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebBind {
    /// 127.0.0.1 only: reachable from this computer.
    Loopback,
    /// All interfaces: reachable from other devices on the network. Needed while a DLNA
    /// renderer streams from this server.
    Lan,
}

/// Handle to the background web server process.
pub struct WebServerHandle {
    pub port: u16,
    /// Interfaces the server actually bound to.
    pub bind: WebBind,
    stop_flag: Arc<AtomicBool>,
    stream_state: Arc<StreamServerState>,
    join_handle: Option<std::thread::JoinHandle<()>>,
}

impl WebServerHandle {
    pub fn stream_state(&self) -> Arc<StreamServerState> {
        self.stream_state.clone()
    }

    pub fn close_active_streams(&self) {
        self.stream_state.close_active_streams();
    }

    pub fn register_stream_file(&self, path: &Path) {
        self.stream_state.register_file(path);
    }

    pub fn stop(mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        self.stream_state.close_active_streams();
        if let Some(h) = self.join_handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for WebServerHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        self.stream_state.close_active_streams();
        if let Some(h) = self.join_handle.take() {
            let _ = h.join();
        }
    }
}

/// Resource limits for the embedded HTTP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerLimits {
    /// Connections allowed at once while reading a request or serving a non-stream route.
    pub max_connections: usize,
    /// `/stream/...` responses allowed at once. Streams are long-lived, so they move out of the
    /// `max_connections` pool once the route is known.
    pub max_streams: usize,
    /// Total time allowed to receive the request line, headers and body (not per read).
    pub request_timeout: Duration,
    /// Longest request line or header line, in bytes.
    pub max_line_bytes: usize,
    /// Most header fields in one request.
    pub max_headers: usize,
    /// Most bytes in the request line plus headers.
    pub max_head_bytes: usize,
    /// Largest request body (`Content-Length`), in bytes.
    pub max_body_bytes: usize,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            max_connections: 64,
            max_streams: 16,
            request_timeout: Duration::from_secs(10),
            max_line_bytes: 8 * 1024,
            max_headers: 100,
            max_head_bytes: 64 * 1024,
            max_body_bytes: 64 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermitKind {
    Request,
    Stream,
}

/// Counts live connection handlers so a flood of connections cannot spawn unbounded threads.
struct ConnectionLimiter {
    limits: ServerLimits,
    requests: AtomicUsize,
    streams: AtomicUsize,
}

impl ConnectionLimiter {
    fn new(limits: ServerLimits) -> Self {
        Self {
            limits,
            requests: AtomicUsize::new(0),
            streams: AtomicUsize::new(0),
        }
    }

    fn counter(&self, kind: PermitKind) -> &AtomicUsize {
        match kind {
            PermitKind::Request => &self.requests,
            PermitKind::Stream => &self.streams,
        }
    }

    fn try_acquire(self: &Arc<Self>, kind: PermitKind) -> Option<ConnectionPermit> {
        let max = match kind {
            PermitKind::Request => self.limits.max_connections,
            PermitKind::Stream => self.limits.max_streams,
        };
        self.counter(kind)
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < max).then_some(n + 1))
            .ok()?;
        Some(ConnectionPermit {
            limiter: self.clone(),
            kind,
        })
    }
}

/// A slot in one of the limiter's pools, released on drop.
pub(super) struct ConnectionPermit {
    limiter: Arc<ConnectionLimiter>,
    kind: PermitKind,
}

impl ConnectionPermit {
    /// Move a request-pool slot to the stream pool; returns `self` unchanged if the stream pool is full.
    pub(super) fn into_stream(self) -> Result<ConnectionPermit, ConnectionPermit> {
        match self.limiter.try_acquire(PermitKind::Stream) {
            Some(stream_permit) => Ok(stream_permit),
            None => Err(self),
        }
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.limiter.counter(self.kind).fetch_sub(1, Ordering::AcqRel);
    }
}

/// Error statuses the server sends before routing a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Rejection {
    BadRequest,
    RequestTimeout,
    PayloadTooLarge,
    UriTooLong,
    HeadersTooLarge,
    Busy,
}

impl Rejection {
    fn status_line(self) -> &'static str {
        match self {
            Rejection::BadRequest => "400 Bad Request",
            Rejection::RequestTimeout => "408 Request Timeout",
            Rejection::PayloadTooLarge => "413 Content Too Large",
            Rejection::UriTooLong => "414 URI Too Long",
            Rejection::HeadersTooLarge => "431 Request Header Fields Too Large",
            Rejection::Busy => "503 Service Unavailable",
        }
    }

    fn response(self) -> String {
        format!(
            "HTTP/1.1 {}\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n",
            self.status_line(),
            if self == Rejection::Busy {
                "Retry-After: 1\r\n"
            } else {
                ""
            }
        )
    }
}

/// Answer a connection the accept loop has no slot for. Runs on the accept thread, so it never
/// waits on the client: the response fits in a fresh socket's send buffer.
fn refuse_busy_connection(mut stream: TcpStream) {
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let _ = stream.write_all(Rejection::Busy.response().as_bytes());
    let _ = stream.flush();
}

/// Send an error response, then drain unread request bytes for a moment before closing: closing
/// with unread data makes the kernel send a reset, which can discard the response at the client.
pub(super) fn reject_request(stream: &mut TcpStream, reader: &mut BufReader<TcpStream>, rejection: Rejection) {
    let _ = stream.write_all(rejection.response().as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Write);

    const DRAIN_TIME: Duration = Duration::from_millis(500);
    const DRAIN_BYTES: usize = 256 * 1024;
    let deadline = Instant::now() + DRAIN_TIME;
    let mut drained = 0usize;
    let mut buf = [0u8; 8192];
    while drained < DRAIN_BYTES {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || reader.get_ref().set_read_timeout(Some(remaining)).is_err() {
            break;
        }
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained += n,
        }
    }
}

/// Write a JSON response with `Cache-Control: <cache>`; a HEAD request gets the head only.
pub(super) fn write_json(
    out: &mut impl Write,
    status: &str,
    cache: &str,
    body: &str,
    is_head: bool,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nX-Content-Type-Options: nosniff\r\nCache-Control: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        cache,
        body.len()
    );
    out.write_all(head.as_bytes())?;
    if !is_head {
        out.write_all(body.as_bytes())?;
    }
    out.flush()
}

/// Write a short plain-text response: the status line, `extra` header lines, then `text` as the
/// body; a HEAD request gets the head only (with the same `Content-Length`).
pub(super) fn write_plain(
    out: &mut impl Write,
    status: &str,
    extra: &str,
    text: &str,
    is_head: bool,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        extra,
        text.len(),
        if is_head { "" } else { text }
    );
    out.write_all(response.as_bytes())?;
    out.flush()
}

/// Write a `404 Not Found`.
pub(super) fn write_not_found(out: &mut impl Write, is_head: bool) -> std::io::Result<()> {
    write_plain(out, "404 Not Found", "", "Not Found", is_head)
}

/// Write a `405 Method Not Allowed` naming the methods the route takes (`allow`).
pub(super) fn write_method_not_allowed(out: &mut impl Write, allow: &str, is_head: bool) -> std::io::Result<()> {
    write_plain(
        out,
        "405 Method Not Allowed",
        &format!("Allow: {}\r\n", allow),
        "Method Not Allowed",
        is_head,
    )
}

/// Request line and headers of an incoming request.
pub(super) struct RequestHead {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) headers: Vec<(String, String)>,
    pub(super) content_length: usize,
}

impl RequestHead {
    /// Value of the first header named `name` (case-insensitive).
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

enum LineError {
    /// The peer closed the connection or the socket failed.
    Closed,
    TooLong,
    Timeout,
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

/// Set the socket read timeout to the time left before `deadline`.
fn arm_read_deadline(stream: &TcpStream, deadline: Instant) -> Result<(), LineError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(LineError::Timeout);
    }
    stream.set_read_timeout(Some(remaining)).map_err(|_| LineError::Closed)
}

/// Read one line (including its `\n`) of at most `max` bytes, finishing before `deadline`.
fn read_line_bounded(reader: &mut BufReader<TcpStream>, deadline: Instant, max: usize) -> Result<Vec<u8>, LineError> {
    let mut line = Vec::new();
    loop {
        arm_read_deadline(reader.get_ref(), deadline)?;
        let available = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if is_timeout(&e) => return Err(LineError::Timeout),
            Err(_) => return Err(LineError::Closed),
        };
        if available.is_empty() {
            return Err(LineError::Closed);
        }
        let (take, done) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len(), false),
        };
        if line.len() + take > max {
            return Err(LineError::TooLong);
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if done {
            return Ok(line);
        }
    }
}

/// Read the request line and headers within the configured limits. `Ok(None)` means the client
/// closed the connection before sending a request line.
pub(super) fn read_request_head(
    reader: &mut BufReader<TcpStream>,
    deadline: Instant,
    limits: &ServerLimits,
) -> Result<Option<RequestHead>, Rejection> {
    let request_line = match read_line_bounded(reader, deadline, limits.max_line_bytes) {
        Ok(line) => line,
        Err(LineError::Closed) => return Ok(None),
        Err(LineError::TooLong) => return Err(Rejection::UriTooLong),
        Err(LineError::Timeout) => return Err(Rejection::RequestTimeout),
    };
    let mut head_bytes = request_line.len();
    let request_line = std::str::from_utf8(&request_line).map_err(|_| Rejection::BadRequest)?;
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Err(Rejection::BadRequest);
    };
    let mut head = RequestHead {
        method: method.to_string(),
        path: path.to_string(),
        headers: Vec::new(),
        content_length: 0,
    };

    loop {
        let line = match read_line_bounded(reader, deadline, limits.max_line_bytes) {
            Ok(line) => line,
            // A client that half-closes after its last header still gets an answer.
            Err(LineError::Closed) => break,
            Err(LineError::TooLong) => return Err(Rejection::HeadersTooLarge),
            Err(LineError::Timeout) => return Err(Rejection::RequestTimeout),
        };
        head_bytes += line.len();
        if head_bytes > limits.max_head_bytes {
            return Err(Rejection::HeadersTooLarge);
        }
        let text = String::from_utf8_lossy(&line);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            break;
        }
        if head.headers.len() >= limits.max_headers {
            return Err(Rejection::HeadersTooLarge);
        }
        let Some((name, value)) = trimmed.split_once(':') else {
            return Err(Rejection::BadRequest);
        };
        let (name, value) = (name.trim(), value.trim());
        if name.eq_ignore_ascii_case("content-length") {
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Rejection::BadRequest);
            }
            // Digits only, so a parse failure means the value overflowed usize.
            let declared = value.parse::<usize>().map_err(|_| Rejection::PayloadTooLarge)?;
            if declared > limits.max_body_bytes {
                return Err(Rejection::PayloadTooLarge);
            }
            head.content_length = declared;
        }
        head.headers.push((name.to_string(), value.to_string()));
    }
    Ok(Some(head))
}

/// Read a request body of `len` bytes (already checked against the body limit) before `deadline`.
pub(super) fn read_request_body(reader: &mut BufReader<TcpStream>, len: usize, deadline: Instant) -> Option<Vec<u8>> {
    let mut body = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        arm_read_deadline(reader.get_ref(), deadline).ok()?;
        match reader.read(&mut body[filled..]) {
            Ok(0) => return None,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    Some(body)
}

pub struct WebServer;

impl WebServer {
    /// Start the local web server on loopback on the requested port (or fallback to an available port).
    pub fn start(
        port: u16,
        state: Arc<RwLock<NowPlayingState>>,
        cmd_tx: mpsc::Sender<WebCommand>,
    ) -> anyhow::Result<WebServerHandle> {
        Self::start_with_bind(WebBind::Loopback, port, state, cmd_tx)
    }

    /// Start the local web server on the given interfaces.
    pub fn start_with_bind(
        bind: WebBind,
        port: u16,
        state: Arc<RwLock<NowPlayingState>>,
        cmd_tx: mpsc::Sender<WebCommand>,
    ) -> anyhow::Result<WebServerHandle> {
        Self::start_with_stream_state(bind, port, state, cmd_tx, Arc::new(StreamServerState::new()))
    }

    /// Start the local web server sharing an existing StreamServerState.
    pub fn start_with_stream_state(
        bind: WebBind,
        port: u16,
        state: Arc<RwLock<NowPlayingState>>,
        cmd_tx: mpsc::Sender<WebCommand>,
        stream_state: Arc<StreamServerState>,
    ) -> anyhow::Result<WebServerHandle> {
        Self::start_with_options(bind, port, state, cmd_tx, stream_state, ServerLimits::default())
    }

    /// Start the local web server with a shared StreamServerState and explicit resource limits.
    pub fn start_with_options(
        bind: WebBind,
        port: u16,
        state: Arc<RwLock<NowPlayingState>>,
        cmd_tx: mpsc::Sender<WebCommand>,
        stream_state: Arc<StreamServerState>,
        limits: ServerLimits,
    ) -> anyhow::Result<WebServerHandle> {
        let candidate_ports = [port, 8080, 8090, 8091, 0];
        let mut listener_opt = None;
        // Never fall back to the other interface: if another app holds *:port, binding
        // 127.0.0.1:port still succeeds on macOS, and renderers' LAN requests would reach
        // that other app. Move to the next candidate port instead.
        let host = match bind {
            WebBind::Lan => "0.0.0.0",
            WebBind::Loopback => "127.0.0.1",
        };
        for &p in &candidate_ports {
            if let Ok(l) = TcpListener::bind(format!("{}:{}", host, p)) {
                listener_opt = Some(l);
                break;
            }
        }
        let listener = listener_opt.ok_or_else(|| anyhow::anyhow!("Failed to bind web server to any port"))?;
        let actual_bind = bind;
        let actual_port = listener.local_addr()?.port();
        crate::log_info!("web", "Web server listening on {}", listener.local_addr()?);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_thread = stop_flag.clone();
        let stream_state_thread = stream_state.clone();
        let limiter = Arc::new(ConnectionLimiter::new(limits));

        // Non-blocking accept so the loop sees the stop flag within one poll interval. A wake-up
        // connection to 127.0.0.1:port is not reliable: another listener bound to 127.0.0.1 on
        // the same port (allowed next to a 0.0.0.0 bind) would receive it instead.
        listener.set_nonblocking(true)?;
        let join_handle = std::thread::Builder::new()
            .name("QuickstreamWebServer".to_string())
            .spawn(move || {
                while !stop_flag_thread.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, peer)) => {
                            if stop_flag_thread.load(Ordering::SeqCst) {
                                break;
                            }
                            // Accepted sockets inherit O_NONBLOCK on macOS/BSD.
                            let _ = stream.set_nonblocking(false);
                            let Some(permit) = limiter.try_acquire(PermitKind::Request) else {
                                crate::log_warn!(
                                    "web",
                                    "Connection limit ({}) reached; refusing {}",
                                    limits.max_connections,
                                    peer
                                );
                                refuse_busy_connection(stream);
                                continue;
                            };
                            let state_clone = state.clone();
                            let cmd_tx_clone = cmd_tx.clone();
                            let stream_state_clone = stream_state_thread.clone();
                            let spawned = std::thread::Builder::new()
                                .name("QuickstreamWebConn".to_string())
                                .spawn(move || {
                                    let _ = handle_client(
                                        stream,
                                        permit,
                                        &limits,
                                        &state_clone,
                                        &cmd_tx_clone,
                                        &stream_state_clone,
                                    );
                                });
                            if let Err(e) = spawned {
                                // The closure (socket and permit) is dropped with the error.
                                crate::log_warn!("web", "Failed to spawn connection handler for {}: {}", peer, e);
                            }
                        }
                        Err(_) => {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                    }
                }
            })?;

        Ok(WebServerHandle {
            port: actual_port,
            bind: actual_bind,
            stop_flag,
            stream_state,
            join_handle: Some(join_handle),
        })
    }
}
