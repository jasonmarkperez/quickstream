//! Hardening tests for SSDP discovery and the shared HTTP client against hostile or
//! misbehaving responders. Every server here binds a loopback address; nothing is
//! sent to the LAN.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use quickstream::discovery::{fetch_xml, parse_http_response, HttpError};

type Responder = Arc<dyn Fn(&mut TcpStream) + Send + Sync>;

/// Minimal HTTP server on `bind_ip` that answers every request with `respond`,
/// one thread per connection. Stops accepting when dropped.
struct LoopbackServer {
    url_host: String,
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl LoopbackServer {
    fn url(&self) -> String {
        format!("http://{}:{}/desc.xml", self.url_host, self.port)
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn spawn_server_on(bind_ip: &str, respond: Responder) -> Option<LoopbackServer> {
    let listener = TcpListener::bind((bind_ip, 0)).ok()?;
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let handle = std::thread::spawn(move || {
        while !stop_t.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut s, _)) => {
                    let respond = respond.clone();
                    std::thread::spawn(move || {
                        let _ = s.set_nonblocking(false);
                        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                        let mut reader = BufReader::new(s.try_clone().unwrap());
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                                break;
                            }
                        }
                        respond(&mut s);
                        let _ = s.flush();
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    let url_host = if bind_ip.contains(':') {
        format!("[{}]", bind_ip)
    } else {
        bind_ip.to_string()
    };
    Some(LoopbackServer {
        url_host,
        port,
        stop,
        handle: Some(handle),
    })
}

fn spawn_server(respond: Responder) -> LoopbackServer {
    spawn_server_on("127.0.0.1", respond).expect("bind 127.0.0.1")
}

// ---------------------------------------------------------------------------
// Interim (1xx) responses and header-section size
// ---------------------------------------------------------------------------

fn interim_flood(count: usize) -> Vec<u8> {
    let mut raw = Vec::new();
    for _ in 0..count {
        raw.extend_from_slice(b"HTTP/1.1 100\n\n");
    }
    raw.extend_from_slice(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    raw
}

/// A long run of interim responses is rejected with an error, not processed by
/// unbounded recursion.
#[test]
fn test_parse_http_response_rejects_interim_flood() {
    let raw = interim_flood(50_000);
    assert!(matches!(parse_http_response(&raw, false), Err(HttpError::Malformed(_))));
    assert!(matches!(parse_http_response(&raw, true), Err(HttpError::Malformed(_))));
}

/// A few interim responses before the final one are still skipped.
#[test]
fn test_parse_http_response_skips_a_few_interim_responses() {
    let raw = interim_flood(3);
    let resp = parse_http_response(&raw, false).unwrap().unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"ok");

    let mixed = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 102 Processing\n\nHTTP/1.1 204 No Content\r\n\r\n";
    assert_eq!(parse_http_response(mixed, false).unwrap().unwrap().status, 204);
}

/// More than the allowed number of interim responses is an error.
#[test]
fn test_parse_http_response_limits_interim_count() {
    assert!(parse_http_response(&interim_flood(8), false).unwrap().is_some());
    assert!(parse_http_response(&interim_flood(9), false).is_err());
}

/// The header section is bounded even while the peer keeps the connection open.
#[test]
fn test_parse_http_response_limits_header_section() {
    let mut raw = b"HTTP/1.1 200 OK\r\n".to_vec();
    while raw.len() < 70 * 1024 {
        raw.extend_from_slice(b"X-Filler: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n");
    }
    // No end of headers yet, but already past the cap: give up instead of waiting.
    assert!(matches!(parse_http_response(&raw, false), Err(HttpError::Malformed(_))));
    // A complete but oversized header section is rejected too.
    raw.extend_from_slice(b"Content-Length: 0\r\n\r\n");
    assert!(matches!(parse_http_response(&raw, false), Err(HttpError::Malformed(_))));

    // A normal header section well under the cap still parses, LF-only included.
    let ok = b"HTTP/1.1 200 OK\nContent-Length: 2\n\nok";
    assert_eq!(parse_http_response(ok, false).unwrap().unwrap().body, b"ok");
}

/// End to end: a description server that floods interim responses makes the fetch
/// fail promptly instead of taking the process down.
#[test]
fn test_fetch_xml_survives_interim_flood() {
    let server = spawn_server(Arc::new(|s: &mut TcpStream| {
        let _ = s.write_all(&interim_flood(5_000));
    }));
    let start = Instant::now();
    assert!(fetch_xml(&server.url()).is_none());
    assert!(start.elapsed() < Duration::from_secs(3), "took {:?}", start.elapsed());
}
