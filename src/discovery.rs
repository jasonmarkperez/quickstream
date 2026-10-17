//! SSDP UPnP discovery and XML device description parser for DLNA MediaRenderers.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DLNADevice {
    pub friendly_name: String,
    pub location: String,
    pub host: String,
    pub port: u16,
    pub manufacturer: String,
    pub model_name: String,
    pub av_transport_control_url: String,
    pub rendering_control_url: String,
    pub udn: String,
}

impl DLNADevice {
    pub fn display_label(&self) -> String {
        let mut details = Vec::new();
        if !self.manufacturer.is_empty() {
            details.push(self.manufacturer.as_str());
        }
        if !self.model_name.is_empty() && self.model_name != self.friendly_name {
            details.push(self.model_name.as_str());
        }
        let detail_str = if !details.is_empty() {
            format!(" [{}]", details.join(", "))
        } else {
            String::new()
        };
        format!("{}{}", self.friendly_name, detail_str)
    }
}

/// Determine the local IPv4 address routable to the target host.
pub fn get_local_ip_for_target(target_host: &str, target_port: u16) -> String {
    let port = if target_port > 0 { target_port } else { 80 };
    let addr = format!("{}:{}", target_host, port);
    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") {
        if socket.connect(&addr).is_ok() {
            if let Ok(local_addr) = socket.local_addr() {
                let ip = local_addr.ip().to_string();
                if !ip.starts_with("127.") {
                    return ip;
                }
            }
        }
    }
    // Fallback probes to common gateway destinations
    for probe in ["192.168.1.1:80", "192.168.0.1:80", "10.0.0.1:80", "8.8.8.8:80", "1.1.1.1:80"] {
        if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") {
            if socket.connect(probe).is_ok() {
                if let Ok(local_addr) = socket.local_addr() {
                    let ip = local_addr.ip().to_string();
                    if !ip.starts_with("127.") {
                        return ip;
                    }
                }
            }
        }
    }
    "127.0.0.1".to_string()
}

/// Extract content between XML tag delimiters `<tag>...</tag>`, ignoring namespaces.
pub fn extract_xml_tag(xml: &str, tag_name: &str) -> Option<String> {
    let open_pat_simple = format!("<{}>", tag_name);
    let close_pat_simple = format!("</{}>", tag_name);

    // Try simple tag match
    if let Some(start) = xml.find(&open_pat_simple) {
        let content_start = start + open_pat_simple.len();
        if let Some(end) = xml[content_start..].find(&close_pat_simple) {
            return Some(xml[content_start..content_start + end].trim().to_string());
        }
    }

    // Try tag with namespace (e.g. <u:tag> or <ns:tag>) or attributes (<tag attr="x">).
    // One forward pass over the `<...>` regions: find the first matching opening tag,
    // then the first matching closing tag after it. Every byte is looked at a bounded
    // number of times, so hostile input cannot make this quadratic.
    fn local_name(name: &str) -> &str {
        match name.find(':') {
            Some(colon) => &name[colon + 1..],
            None => name,
        }
    }
    let mut content_start: Option<usize> = None;
    let mut cursor = 0;
    while let Some(rel) = xml[cursor..].find('<') {
        let mut open = cursor + rel;
        let close = match xml[open + 1..].find('>') {
            Some(i) => open + 1 + i,
            None => break,
        };
        match content_start {
            None => {
                let header = &xml[open + 1..close];
                let name = header.split_whitespace().next().unwrap_or("");
                if local_name(name) == tag_name {
                    content_start = Some(close + 1);
                }
            }
            Some(start) => {
                // A stray '<' in the content must not hide the closing tag: the tag
                // ending at `close` starts at the last '<' before it.
                if let Some(inner) = xml[open + 1..close].rfind('<') {
                    open = open + 1 + inner;
                }
                if let Some(end_header) = xml[open + 1..close].strip_prefix('/') {
                    if local_name(end_header).trim() == tag_name {
                        return Some(xml[start..open].trim().to_string());
                    }
                }
            }
        }
        cursor = close + 1;
    }

    None
}

/// Parsed `http://` URL.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpUrl {
    /// Host without brackets (an IPv6 literal is returned bare).
    pub host: String,
    pub port: u16,
    /// Authority as written in the URL, used for the `Host:` header.
    pub host_port: String,
    pub path: String,
}

/// Parse an `http://host[:port][/path]` URL. Returns `None` for other schemes or bad ports.
pub fn parse_http_url(url: &str) -> Option<HttpUrl> {
    let trimmed = url.trim();
    let scheme_end = trimmed.find("://")?;
    if !trimmed[..scheme_end].eq_ignore_ascii_case("http") {
        return None;
    }
    let without_proto = &trimmed[scheme_end + 3..];
    let (host_port, path) = match without_proto.find('/') {
        Some(slash) => (&without_proto[..slash], &without_proto[slash..]),
        None => (without_proto, "/"),
    };
    if host_port.is_empty() || host_port.contains('@') {
        return None;
    }
    let (host, port_str) = if let Some(rest) = host_port.strip_prefix('[') {
        let close = rest.find(']')?;
        let after = &rest[close + 1..];
        let port = match after.strip_prefix(':') {
            Some(p) => Some(p),
            None if after.is_empty() => None,
            None => return None,
        };
        (&rest[..close], port)
    } else {
        match host_port.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (host_port, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port_str {
        Some(p) => p.parse::<u16>().ok()?,
        None => 80,
    };
    Some(HttpUrl {
        host: host.to_string(),
        port,
        host_port: host_port.to_string(),
        path: path.to_string(),
    })
}

/// HTTP response returned by [`http_exchange`], with any chunked framing removed.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// First header value with the given (case-insensitive) name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

/// Transport-level failure of an HTTP exchange.
#[derive(Debug, Clone, PartialEq)]
pub enum HttpError {
    Resolve(String),
    Connect(String),
    Io { kind: std::io::ErrorKind, message: String },
    /// The overall deadline passed before a complete response arrived.
    DeadlineExceeded,
    /// The response grew past the byte limit.
    TooLarge(usize),
    Malformed(String),
}

impl HttpError {
    /// Errors worth retrying: timeouts and dropped connections, not refusals or bad replies.
    pub fn is_transient(&self) -> bool {
        use std::io::ErrorKind;
        match self {
            HttpError::DeadlineExceeded => true,
            HttpError::Io { kind, .. } => matches!(
                kind,
                ErrorKind::TimedOut
                    | ErrorKind::WouldBlock
                    | ErrorKind::Interrupted
                    | ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
                    | ErrorKind::UnexpectedEof
            ),
            _ => false,
        }
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::Resolve(m) => write!(f, "address resolution failed: {}", m),
            HttpError::Connect(m) => write!(f, "connection failed: {}", m),
            HttpError::Io { message, .. } => write!(f, "I/O error: {}", message),
            HttpError::DeadlineExceeded => write!(f, "request timeout (deadline exceeded)"),
            HttpError::TooLarge(limit) => write!(f, "response larger than {} bytes", limit),
            HttpError::Malformed(m) => write!(f, "malformed HTTP response: {}", m),
        }
    }
}

/// Parse the numeric status code from an HTTP status line such as `HTTP/1.1 200 OK`.
pub fn parse_status_code(status_line: &str) -> Option<u16> {
    let mut parts = status_line.split_whitespace();
    let version = parts.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    let code = parts.next()?;
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    code.parse().ok()
}

/// Decode an HTTP/1.1 chunked body.
///
/// Returns `Ok(Some(body))` once the terminating zero-size chunk has been read,
/// `Ok(None)` if more data is needed, and `Err` if the framing is invalid.
pub fn decode_chunked(data: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        let line_end = match data[pos..].iter().position(|&b| b == b'\n') {
            Some(i) => pos + i,
            None => return Ok(None),
        };
        let line = String::from_utf8_lossy(&data[pos..line_end]);
        let size_str = line.trim_end_matches('\r').split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| format!("invalid chunk size '{}'", size_str))?;
        pos = line_end + 1;
        if size == 0 {
            // Trailers (if any) carry nothing we need.
            return Ok(Some(out));
        }
        let chunk_end = pos.checked_add(size).ok_or("chunk size overflow")?;
        if data.len() < chunk_end {
            return Ok(None);
        }
        out.extend_from_slice(&data[pos..chunk_end]);
        pos = chunk_end;
        // Each chunk's data is followed by CRLF (tolerate a bare LF).
        if data[pos..].starts_with(b"\r\n") {
            pos += 2;
        } else if data[pos..].starts_with(b"\n") {
            pos += 1;
        } else if data.len() - pos < 2 {
            return Ok(None);
        } else {
            return Err("missing CRLF after chunk data".to_string());
        }
    }
}

/// Largest header section accepted, counting any interim (1xx) responses before the
/// final one. Real renderers send well under 4 KiB.
pub const MAX_HTTP_HEADER_BYTES: usize = 64 * 1024;
/// Interim (1xx) responses skipped before the final response; more is an error.
pub const MAX_INTERIM_RESPONSES: usize = 8;

/// Find the end of the header block starting at `start`: returns the offset of the
/// blank-line delimiter and its length. Only the first [`MAX_HTTP_HEADER_BYTES`] of
/// `buf` are searched, so a delimiter beyond the cap is never found.
fn find_header_end(buf: &[u8], start: usize) -> Option<(usize, usize)> {
    let limit = buf.len().min(MAX_HTTP_HEADER_BYTES);
    if start >= limit {
        return None;
    }
    let window = &buf[start..limit];
    let crlf = window.windows(4).position(|w| w == b"\r\n\r\n").map(|p| (p, 4));
    let lf = window.windows(2).position(|w| w == b"\n\n").map(|p| (p, 2));
    let (pos, len) = match (crlf, lf) {
        (Some(a), Some(b)) => if a.0 <= b.0 { a } else { b },
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    Some((start + pos, len))
}

/// Try to parse a complete HTTP response from `buf`.
///
/// Returns `Ok(None)` while more data is needed. With `eof = true` the peer has closed
/// the connection, so whatever has arrived is final. A body shorter than its
/// Content-Length is accepted at EOF because some renderers send a wrong length.
///
/// Up to [`MAX_INTERIM_RESPONSES`] interim 1xx responses are skipped, and the header
/// section (interim responses included) may not exceed [`MAX_HTTP_HEADER_BYTES`].
pub fn parse_http_response(buf: &[u8], eof: bool) -> Result<Option<HttpResponse>, HttpError> {
    let mut start = 0;
    let mut interim = 0;
    let (status, headers, body_start) = loop {
        let (header_end, delim_len) = match find_header_end(buf, start) {
            Some(found) => found,
            None => {
                if buf.len() >= MAX_HTTP_HEADER_BYTES {
                    return Err(HttpError::Malformed(format!(
                        "header section larger than {} bytes",
                        MAX_HTTP_HEADER_BYTES
                    )));
                }
                return if eof {
                    Err(HttpError::Malformed(if buf.len() == start {
                        "empty response".to_string()
                    } else {
                        "no end of headers".to_string()
                    }))
                } else {
                    Ok(None)
                };
            }
        };
        let body_start = header_end + delim_len;
        if body_start > MAX_HTTP_HEADER_BYTES {
            return Err(HttpError::Malformed(format!(
                "header section larger than {} bytes",
                MAX_HTTP_HEADER_BYTES
            )));
        }

        let head = String::from_utf8_lossy(&buf[start..header_end]);
        let mut lines = head.lines();
        let status_line = lines.next().unwrap_or("");
        let status = parse_status_code(status_line)
            .ok_or_else(|| HttpError::Malformed(format!("bad status line '{}'", status_line)))?;

        // Skip interim 1xx responses (e.g. "100 Continue").
        if (100..200).contains(&status) {
            interim += 1;
            if interim > MAX_INTERIM_RESPONSES {
                return Err(HttpError::Malformed(format!(
                    "more than {} interim responses",
                    MAX_INTERIM_RESPONSES
                )));
            }
            start = body_start;
            continue;
        }

        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        break (status, headers, body_start);
    };
    let body = &buf[body_start..];

    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let chunked = header("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);

    let body = if status == 204 || status == 304 {
        Vec::new()
    } else if chunked {
        match decode_chunked(body) {
            Ok(Some(decoded)) => decoded,
            Ok(None) if eof => return Err(HttpError::Malformed("truncated chunked body".to_string())),
            Ok(None) => return Ok(None),
            Err(e) => return Err(HttpError::Malformed(e)),
        }
    } else if let Some(cl) = header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        if body.len() >= cl {
            body[..cl].to_vec()
        } else if eof {
            body.to_vec()
        } else {
            return Ok(None);
        }
    } else if eof {
        body.to_vec()
    } else {
        return Ok(None);
    };

    Ok(Some(HttpResponse { status, headers, body }))
}

fn remaining_until(deadline: Instant) -> Result<Duration, HttpError> {
    let now = Instant::now();
    if now >= deadline {
        return Err(HttpError::DeadlineExceeded);
    }
    Ok((deadline - now).max(Duration::from_millis(1)))
}

/// Send a raw HTTP/1.x request to `host:port` and read the response.
///
/// The whole exchange (resolve, connect, write, read) must finish by `deadline`, and the
/// raw response may not exceed `max_bytes`; a peer that trickles bytes or never closes
/// the connection cannot hold the caller past the deadline or grow memory without bound.
pub fn http_exchange(
    host: &str,
    port: u16,
    request: &[u8],
    deadline: Instant,
    max_bytes: usize,
) -> Result<HttpResponse, HttpError> {
    http_exchange_with_first_byte_deadline(host, port, request, deadline, deadline, max_bytes)
}

/// Like [`http_exchange`], but connecting and receiving the first response byte must
/// also happen by `first_byte_deadline` (clamped to `deadline`), so a peer that accepts
/// the connection and then stays silent is dropped early.
pub fn http_exchange_with_first_byte_deadline(
    host: &str,
    port: u16,
    request: &[u8],
    first_byte_deadline: Instant,
    deadline: Instant,
    max_bytes: usize,
) -> Result<HttpResponse, HttpError> {
    let first_byte_deadline = first_byte_deadline.min(deadline);
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| HttpError::Resolve(format!("{}:{}: {}", host, port, e)))?;

    let mut stream = None;
    let mut last_err = String::from("no addresses");
    for addr in addrs {
        let timeout = remaining_until(first_byte_deadline)?;
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    let mut stream = stream.ok_or_else(|| HttpError::Connect(format!("{}:{}: {}", host, port, last_err)))?;

    let io_err = |e: std::io::Error| {
        if Instant::now() >= deadline {
            HttpError::DeadlineExceeded
        } else {
            HttpError::Io {
                kind: e.kind(),
                message: e.to_string(),
            }
        }
    };

    let _ = stream.set_write_timeout(Some(remaining_until(first_byte_deadline)?));
    stream.write_all(request).map_err(io_err)?;
    let _ = stream.flush();

    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        if let Some(resp) = parse_http_response(&buf, false)? {
            return Ok(resp);
        }
        let read_deadline = if buf.is_empty() { first_byte_deadline } else { deadline };
        let _ = stream.set_read_timeout(Some(remaining_until(read_deadline)?));
        match stream.read(&mut chunk) {
            Ok(0) => {
                return parse_http_response(&buf, true)?
                    .ok_or_else(|| HttpError::Malformed("incomplete response".to_string()));
            }
            Ok(n) => {
                if buf.len() + n > max_bytes {
                    return Err(HttpError::TooLarge(max_bytes));
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e)
                if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) =>
            {
                if Instant::now() >= read_deadline {
                    return Err(HttpError::DeadlineExceeded);
                }
            }
            Err(e) => return Err(io_err(e)),
        }
    }
}

/// Largest device description accepted from a renderer.
pub const MAX_DESCRIPTION_BYTES: usize = 256 * 1024;
/// Overall deadline for fetching one device description.
pub const DESCRIPTION_FETCH_TIMEOUT: Duration = Duration::from_secs(3);
/// Device descriptions fetched concurrently during a scan.
pub const MAX_PARALLEL_DESCRIPTION_FETCHES: usize = 8;
/// Distinct LOCATION URLs followed per scan; further responders are ignored.
pub const MAX_LOCATIONS_PER_SCAN: usize = 64;
/// Description fetches running at once against a single host.
pub const MAX_PARALLEL_FETCHES_PER_HOST: usize = 2;
/// Distinct LOCATION URLs followed per host per scan. Real devices advertise one or
/// two root descriptions; the cap keeps one host from using up the per-scan budget.
pub const MAX_LOCATIONS_PER_HOST: usize = 4;
/// Time allowed to connect to a device and receive the first byte of its description.
pub const DESCRIPTION_FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(1);

/// Download a device description with the default deadline and size cap.
pub fn fetch_xml(url: &str) -> Option<String> {
    fetch_xml_with_limits(url, DESCRIPTION_FETCH_TIMEOUT, MAX_DESCRIPTION_BYTES)
}

/// Download a device description. The whole request must finish within `timeout`
/// (connecting and the first response byte within [`DESCRIPTION_FIRST_BYTE_TIMEOUT`])
/// and the body may not exceed `max_body_bytes`; otherwise `None` is returned.
pub fn fetch_xml_with_limits(url: &str, timeout: Duration, max_body_bytes: usize) -> Option<String> {
    let now = Instant::now();
    fetch_xml_by(url, now + DESCRIPTION_FIRST_BYTE_TIMEOUT, now + timeout, max_body_bytes)
}

fn fetch_xml_by(url: &str, first_byte_deadline: Instant, deadline: Instant, max_body_bytes: usize) -> Option<String> {
    let u = parse_http_url(url)?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: Quickstream/0.2.0 UPnP/1.1\r\nConnection: close\r\n\r\n",
        u.path, u.host_port
    );
    // Leave room for the status line and headers on top of the body cap.
    let resp = match http_exchange_with_first_byte_deadline(
        &u.host,
        u.port,
        request.as_bytes(),
        first_byte_deadline,
        deadline,
        max_body_bytes + 16 * 1024,
    ) {
        Ok(r) => r,
        Err(e) => {
            crate::log_info!("discovery", "Device description fetch from {} failed: {}", url, e);
            return None;
        }
    };
    if !(200..300).contains(&resp.status) || resp.body.is_empty() || resp.body.len() > max_body_bytes {
        crate::log_info!(
            "discovery",
            "Ignoring device description from {} (HTTP {}, {} bytes)",
            url,
            resp.status,
            resp.body.len()
        );
        return None;
    }
    Some(resp.body_text())
}

/// Extract the LOCATION URL from an SSDP response, but only if its host is the IP
/// address the response came from.
///
/// Without this check any host on the network (or a spoofed packet) could point
/// discovery at an arbitrary URL. LOCATION hosts that are names rather than IP
/// literals are rejected too, since they cannot be checked without a DNS lookup.
pub fn ssdp_location_for_sender(response: &str, sender: std::net::IpAddr) -> Option<String> {
    let location = response.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("location") {
            Some(value.trim().to_string())
        } else {
            None
        }
    })?;
    let url = parse_http_url(&location)?;
    let host_ip: std::net::IpAddr = url.host.parse().ok()?;
    if host_ip.to_canonical() == sender.to_canonical() {
        Some(location)
    } else {
        None
    }
}

/// Scheduling key for a LOCATION URL: its host, with IP literals in canonical form.
fn location_host_key(url: &str) -> Option<String> {
    let u = parse_http_url(url)?;
    Some(match u.host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.to_canonical().to_string(),
        Err(_) => u.host.to_ascii_lowercase(),
    })
}

#[derive(Debug)]
struct HostQueue {
    key: String,
    pending: std::collections::VecDeque<(usize, String)>,
    in_flight: usize,
    accepted: usize,
}

/// A LOCATION chosen by [`FetchScheduler::next_job`].
#[derive(Debug, Clone, PartialEq)]
pub struct FetchJob {
    /// Submission order, used to keep results in a stable order.
    pub index: usize,
    pub url: String,
    /// Scheduling key of the URL's host; pass it back to [`FetchScheduler::complete`].
    pub host: String,
}

/// Decides which device descriptions to fetch and when. Pure state, no I/O.
///
/// LOCATIONs are grouped by host and started round-robin across hosts, with at most
/// `max_per_host` running per host and `max_parallel` overall. Each host may contribute
/// at most `max_locations_per_host` LOCATIONs and the scan at most `max_locations`, so
/// one host answering with many LOCATIONs cannot crowd out the others.
#[derive(Debug)]
pub struct FetchScheduler {
    max_parallel: usize,
    max_per_host: usize,
    max_locations_per_host: usize,
    max_locations: usize,
    seen: HashSet<String>,
    hosts: Vec<HostQueue>,
    next_host: usize,
    in_flight: usize,
}

impl FetchScheduler {
    pub fn new(max_parallel: usize, max_per_host: usize, max_locations_per_host: usize, max_locations: usize) -> Self {
        Self {
            max_parallel: max_parallel.max(1),
            max_per_host: max_per_host.max(1),
            max_locations_per_host,
            max_locations,
            seen: HashSet::new(),
            hosts: Vec::new(),
            next_host: 0,
            in_flight: 0,
        }
    }

    /// Scheduler with the limits used for SSDP scans.
    pub fn for_scan(max_parallel: usize) -> Self {
        Self::new(
            max_parallel,
            MAX_PARALLEL_FETCHES_PER_HOST,
            MAX_LOCATIONS_PER_HOST,
            MAX_LOCATIONS_PER_SCAN,
        )
    }

    /// Queue a LOCATION URL. Returns `false` if it was ignored: a duplicate, not an
    /// `http://` URL, or over the per-host or per-scan budget.
    pub fn submit(&mut self, url: String) -> bool {
        let Some(key) = location_host_key(&url) else {
            return false;
        };
        if self.seen.len() >= self.max_locations || self.seen.contains(&url) {
            return false;
        }
        let slot = match self.hosts.iter().position(|h| h.key == key) {
            Some(i) => i,
            None => {
                self.hosts.push(HostQueue {
                    key,
                    pending: std::collections::VecDeque::new(),
                    in_flight: 0,
                    accepted: 0,
                });
                self.hosts.len() - 1
            }
        };
        let host = &mut self.hosts[slot];
        if host.accepted >= self.max_locations_per_host {
            return false;
        }
        host.accepted += 1;
        let index = self.seen.len();
        self.seen.insert(url.clone());
        host.pending.push_back((index, url));
        true
    }

    /// Next LOCATION to start, if the overall and per-host limits allow one.
    pub fn next_job(&mut self) -> Option<FetchJob> {
        if self.in_flight >= self.max_parallel || self.hosts.is_empty() {
            return None;
        }
        let n = self.hosts.len();
        for step in 0..n {
            let slot = (self.next_host + step) % n;
            let host = &mut self.hosts[slot];
            if host.in_flight >= self.max_per_host {
                continue;
            }
            if let Some((index, url)) = host.pending.pop_front() {
                host.in_flight += 1;
                self.in_flight += 1;
                self.next_host = (slot + 1) % n;
                return Some(FetchJob {
                    index,
                    url,
                    host: host.key.clone(),
                });
            }
        }
        None
    }

    /// Mark a job started by [`next_job`](Self::next_job) as finished.
    pub fn complete(&mut self, host: &str) {
        if let Some(h) = self.hosts.iter_mut().find(|h| h.key == host) {
            if h.in_flight > 0 {
                h.in_flight -= 1;
                self.in_flight = self.in_flight.saturating_sub(1);
            }
        }
    }

    /// Fetches currently running.
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// Nothing queued and nothing running.
    pub fn is_idle(&self) -> bool {
        self.in_flight == 0 && self.hosts.iter().all(|h| h.pending.is_empty())
    }
}

/// Fetches device descriptions on background threads, at most `max_parallel` at once
/// and at most [`MAX_PARALLEL_FETCHES_PER_HOST`] per host (see [`FetchScheduler`]).
///
/// Each fetch is bounded by `fetch_timeout`, must connect and start answering within
/// [`DESCRIPTION_FIRST_BYTE_TIMEOUT`], and never runs past the scan deadline, so a slow
/// or silent device delays only itself.
pub struct DescriptionFetcher {
    scheduler: FetchScheduler,
    fetch_timeout: Duration,
    first_byte_timeout: Duration,
    deadline: Option<Instant>,
    tx: std::sync::mpsc::Sender<(usize, String, Option<DLNADevice>)>,
    rx: std::sync::mpsc::Receiver<(usize, String, Option<DLNADevice>)>,
    results: Vec<(usize, DLNADevice)>,
}

impl DescriptionFetcher {
    pub fn new(max_parallel: usize, fetch_timeout: Duration) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            scheduler: FetchScheduler::for_scan(max_parallel),
            fetch_timeout,
            first_byte_timeout: DESCRIPTION_FIRST_BYTE_TIMEOUT.min(fetch_timeout),
            deadline: None,
            tx,
            rx,
            results: Vec::new(),
        }
    }

    /// Overall scan deadline: no fetch is started after it, and running fetches are
    /// cut off at it. An earlier deadline replaces a later one, never the reverse.
    pub fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = Some(match self.deadline {
            Some(d) => d.min(deadline),
            None => deadline,
        });
    }

    /// Queue a LOCATION URL. Duplicates and URLs over the per-host or per-scan budget
    /// ([`MAX_LOCATIONS_PER_HOST`], [`MAX_LOCATIONS_PER_SCAN`]) are ignored.
    pub fn submit(&mut self, url: String) {
        if self.scheduler.submit(url) {
            self.pump();
        }
    }

    /// Collect finished fetches and start queued ones. Never blocks.
    pub fn pump(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.record(msg);
        }
        loop {
            let now = Instant::now();
            if self.deadline.is_some_and(|d| now >= d) {
                break;
            }
            let Some(job) = self.scheduler.next_job() else {
                break;
            };
            let mut deadline = now + self.fetch_timeout;
            if let Some(scan_deadline) = self.deadline {
                deadline = deadline.min(scan_deadline);
            }
            let first_byte_deadline = (now + self.first_byte_timeout).min(deadline);
            let tx = self.tx.clone();
            thread::spawn(move || {
                let device = fetch_xml_by(&job.url, first_byte_deadline, deadline, MAX_DESCRIPTION_BYTES)
                    .and_then(|xml| parse_device_description(&xml, &job.url));
                let _ = tx.send((job.index, job.host, device));
            });
        }
    }

    fn record(&mut self, (index, host, device): (usize, String, Option<DLNADevice>)) {
        self.scheduler.complete(&host);
        if let Some(dev) = device {
            self.results.push((index, dev));
        }
    }

    /// Keep starting queued fetches and collecting results until everything is done or
    /// `deadline` passes, then return the devices found, in the order their LOCATIONs
    /// were submitted. Fetches still running at the deadline are cut off by it.
    pub fn finish(mut self, deadline: Instant) -> Vec<DLNADevice> {
        self.set_deadline(deadline);
        let deadline = self.deadline.unwrap_or(deadline);
        loop {
            self.pump();
            if self.scheduler.is_idle() {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match self.rx.recv_timeout(deadline - now) {
                Ok(msg) => self.record(msg),
                Err(_) => break,
            }
        }
        self.results.sort_by_key(|(i, _)| *i);
        self.results.into_iter().map(|(_, d)| d).collect()
    }
}

/// Fetch and parse several device descriptions concurrently (at most `max_parallel`
/// at a time and [`MAX_PARALLEL_FETCHES_PER_HOST`] per host, each bounded by
/// `fetch_timeout`).
pub fn fetch_device_descriptions(urls: &[String], max_parallel: usize, fetch_timeout: Duration) -> Vec<DLNADevice> {
    let mut fetcher = DescriptionFetcher::new(max_parallel, fetch_timeout);
    for url in urls {
        fetcher.submit(url.clone());
    }
    // Worst case every URL is on the same host, which runs the fewest at once.
    let per_round = max_parallel.clamp(1, MAX_PARALLEL_FETCHES_PER_HOST);
    let rounds = urls.len().div_ceil(per_round).max(1) as u32;
    fetcher.finish(Instant::now() + fetch_timeout * rounds + Duration::from_millis(250))
}

/// Parse UPnP Device Description XML and extract device information and service URLs.
pub fn parse_device_description(xml_data: &str, location_url: &str) -> Option<DLNADevice> {
    // Parse host and port from location URL
    let url_trimmed = location_url.trim();
    let without_proto = url_trimmed.strip_prefix("http://")?;
    let host_port = if let Some(slash) = without_proto.find('/') {
        &without_proto[..slash]
    } else {
        without_proto
    };

    let (host, port) = if let Some(colon) = host_port.find(':') {
        let h = colon;
        (&host_port[..h], host_port[h + 1..].parse::<u16>().unwrap_or(80))
    } else {
        (host_port, 80)
    };

    let friendly_name = extract_xml_tag(xml_data, "friendlyName")
        .unwrap_or_else(|| format!("MediaRenderer ({})", host));
    let manufacturer = extract_xml_tag(xml_data, "manufacturer").unwrap_or_default();
    let model_name = extract_xml_tag(xml_data, "modelName").unwrap_or_default();
    let udn = extract_xml_tag(xml_data, "UDN").unwrap_or_default();

    // Look for service blocks: AVTransport & RenderingControl
    let mut av_transport_control = String::new();
    let mut rendering_control = String::new();

    let mut cursor = 0;
    while let Some(service_start) = xml_data[cursor..].find("<service>") {
        let abs_start = cursor + service_start;
        let abs_end = match xml_data[abs_start..].find("</service>") {
            Some(idx) => abs_start + idx + 10,
            None => break,
        };
        let service_block = &xml_data[abs_start..abs_end];

        let service_type = extract_xml_tag(service_block, "serviceType").unwrap_or_default();
        let control_url_rel = extract_xml_tag(service_block, "controlURL").unwrap_or_default();

        let full_control_url = if control_url_rel.starts_with("http://") {
            control_url_rel
        } else if control_url_rel.starts_with('/') {
            format!("http://{}{}", host_port, control_url_rel)
        } else {
            format!("http://{}/{}", host_port, control_url_rel)
        };

        if service_type.contains(":AVTransport:") {
            av_transport_control = full_control_url;
        } else if service_type.contains(":RenderingControl:") {
            rendering_control = full_control_url;
        }

        cursor = abs_end;
    }

    // A valid DLNA MediaRenderer must provide an AVTransport service
    if av_transport_control.is_empty() {
        return None;
    }

    Some(DLNADevice {
        friendly_name,
        location: location_url.to_string(),
        host: host.to_string(),
        port,
        manufacturer,
        model_name,
        av_transport_control_url: av_transport_control,
        rendering_control_url: rendering_control,
        udn,
    })
}

/// Discover DLNA MediaRenderers on the local network via SSDP UDP multicast (239.255.255.250:1900).
pub fn discover_devices(timeout: Duration) -> Vec<DLNADevice> {
    let multicast_addr = "239.255.255.250:1900";
    let search_targets = [
        "urn:schemas-upnp-org:device:MediaRenderer:1",
        "urn:schemas-upnp-org:service:AVTransport:1",
        "ssdp:all",
    ];

    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    // Short read timeout so finished description fetches are collected promptly.
    let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
    let _ = socket.set_multicast_ttl_v4(2);

    for target in &search_targets {
        let msg = format!(
            "M-SEARCH * HTTP/1.1\r\n\
             HOST: {}\r\n\
             MAN: \"ssdp:discover\"\r\n\
             MX: 2\r\n\
             ST: {}\r\n\
             \r\n",
            multicast_addr, target
        );
        let _ = socket.send_to(msg.as_bytes(), multicast_addr);
    }

    let start = Instant::now();
    // Listening plus one description timeout: the whole scan ends by then, however
    // many LOCATIONs arrive or how they behave.
    let scan_deadline = start + timeout + DESCRIPTION_FETCH_TIMEOUT;
    let mut fetcher = DescriptionFetcher::new(MAX_PARALLEL_DESCRIPTION_FETCHES, DESCRIPTION_FETCH_TIMEOUT);
    fetcher.set_deadline(scan_deadline);
    let mut rejected = HashSet::new();
    let mut buf = [0u8; 4096];

    while start.elapsed() < timeout {
        if let Ok((len, src)) = socket.recv_from(&mut buf) {
            let resp = String::from_utf8_lossy(&buf[..len]);
            match ssdp_location_for_sender(&resp, src.ip()) {
                Some(url) => fetcher.submit(url),
                None => {
                    if rejected.insert(src.ip()) {
                        crate::log_info!(
                            "discovery",
                            "Ignoring SSDP response from {}: no LOCATION on the sender's own IP",
                            src.ip()
                        );
                    }
                }
            }
        }
        fetcher.pump();
    }

    // Queued and running descriptions get until the overall scan deadline.
    fetcher.finish(scan_deadline)
}

/// Asynchronous background device discovery manager.
pub struct DeviceManager {
    pub devices: Arc<Mutex<Vec<DLNADevice>>>,
    pub is_scanning: Arc<Mutex<bool>>,
    pub active_device: Arc<Mutex<Option<DLNADevice>>>,
}

impl Default for DeviceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceManager {
    pub fn new() -> Self {
        Self {
            devices: Arc::new(Mutex::new(Vec::new())),
            is_scanning: Arc::new(Mutex::new(false)),
            active_device: Arc::new(Mutex::new(None)),
        }
    }

    pub fn start_background_scan(&self) {
        let devices_clone = Arc::clone(&self.devices);
        let scanning_clone = Arc::clone(&self.is_scanning);

        thread::spawn(move || {
            {
                let mut scanning = scanning_clone.lock().unwrap();
                if *scanning {
                    return;
                }
                *scanning = true;
            }

            let scan_start = std::time::Instant::now();
            let found = discover_devices(Duration::from_millis(2500));
            let found_count = found.len();

            {
                let mut dev_list = devices_clone.lock().unwrap();
                for dev in found {
                    if !dev_list.iter().any(|d| d.location == dev.location) {
                        dev_list.push(dev);
                    }
                }
            }

            {
                let mut scanning = scanning_clone.lock().unwrap();
                *scanning = false;
            }

            crate::logger::log_raw(
                "PERF",
                "discovery",
                &format!(
                    "DLNA device discovery complete ({} devices found) -> elapsed: {:.2?}",
                    found_count,
                    scan_start.elapsed()
                ),
            );
        });
    }

    pub fn get_devices(&self) -> Vec<DLNADevice> {
        self.devices.lock().unwrap().clone()
    }

    pub fn is_scanning(&self) -> bool {
        *self.is_scanning.lock().unwrap()
    }

    pub fn set_active_device(&self, dev: Option<DLNADevice>) {
        *self.active_device.lock().unwrap() = dev;
    }

    pub fn get_active_device(&self) -> Option<DLNADevice> {
        self.active_device.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_device_description() {
        let sample_xml = r#"<?xml version="1.0"?>
        <root xmlns="urn:schemas-upnp-org:device-1-0">
          <device>
            <friendlyName>Living Room Stereo</friendlyName>
            <manufacturer>Primare</manufacturer>
            <modelName>I15 Prisma</modelName>
            <UDN>uuid:12345-6789</UDN>
            <serviceList>
              <service>
                <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
                <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
                <controlURL>/AVTransport/control</controlURL>
              </service>
              <service>
                <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
                <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>
                <controlURL>/RenderingControl/control</controlURL>
              </service>
            </serviceList>
          </device>
        </root>"#;

        let dev = parse_device_description(sample_xml, "http://192.168.1.50:8000/desc.xml").unwrap();
        assert_eq!(dev.friendly_name, "Living Room Stereo");
        assert_eq!(dev.manufacturer, "Primare");
        assert_eq!(dev.model_name, "I15 Prisma");
        assert_eq!(dev.av_transport_control_url, "http://192.168.1.50:8000/AVTransport/control");
        assert_eq!(dev.rendering_control_url, "http://192.168.1.50:8000/RenderingControl/control");
        assert_eq!(dev.display_label(), "Living Room Stereo [Primare, I15 Prisma]");
    }
}
