//! Hardening tests for SSDP discovery and the shared HTTP client against hostile or
//! misbehaving responders. Every server here binds a loopback address; nothing is
//! sent to the LAN.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use quickstream::discovery::{
    extract_xml_tag, fetch_xml, http_exchange, parse_device_description, parse_http_response, DLNADevice,
    DescriptionFetcher, FetchJob, FetchScheduler, HttpError, DESCRIPTION_FETCH_TIMEOUT,
    MAX_PARALLEL_DESCRIPTION_FETCHES,
};
use quickstream::player::upnp::{SoapError, UPnPClient, AV_TRANSPORT_SERVICE};

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
    common::isolate();
    let raw = interim_flood(50_000);
    assert!(matches!(parse_http_response(&raw, false), Err(HttpError::Malformed(_))));
    assert!(matches!(parse_http_response(&raw, true), Err(HttpError::Malformed(_))));
}

/// A few interim responses before the final one are still skipped.
#[test]
fn test_parse_http_response_skips_a_few_interim_responses() {
    common::isolate();
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
    common::isolate();
    assert!(parse_http_response(&interim_flood(8), false).unwrap().is_some());
    assert!(parse_http_response(&interim_flood(9), false).is_err());
}

/// The header section is bounded even while the peer keeps the connection open.
#[test]
fn test_parse_http_response_limits_header_section() {
    common::isolate();
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
    common::isolate();
    let server = spawn_server(Arc::new(|s: &mut TcpStream| {
        let _ = s.write_all(&interim_flood(5_000));
    }));
    let start = Instant::now();
    assert!(fetch_xml(&server.url()).is_none());
    assert!(start.elapsed() < Duration::from_secs(3), "took {:?}", start.elapsed());
}

// ---------------------------------------------------------------------------
// Description fetch fairness: one host cannot starve the others
// ---------------------------------------------------------------------------

fn renderer_xml(name: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><root xmlns="urn:schemas-upnp-org:device-1-0"><device><friendlyName>{}</friendlyName><UDN>uuid:{}</UDN><serviceList><service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType><controlURL>/AVTransport/control</controlURL></service></serviceList></device></root>"#,
        name, name
    )
}

fn write_xml(s: &mut TcpStream, xml: &str) {
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        xml.len(),
        xml
    );
    let _ = s.write_all(resp.as_bytes());
}

/// Accepts connections and never answers.
fn hanging_server() -> LoopbackServer {
    spawn_server(Arc::new(|_s: &mut TcpStream| {
        std::thread::sleep(Duration::from_secs(6));
    }))
}

/// A server that accepts but never sends a byte is given up on well before the
/// overall description deadline.
#[test]
fn test_fetch_xml_gives_up_early_without_first_byte() {
    common::isolate();
    let server = hanging_server();
    let start = Instant::now();
    assert!(fetch_xml(&server.url()).is_none());
    let elapsed = start.elapsed();
    assert!(elapsed < Duration::from_millis(1800), "took {:?}", elapsed);
}

// ---------------------------------------------------------------------------
// XML tag extraction cost and behaviour
// ---------------------------------------------------------------------------

/// Run `f` on a helper thread and fail if it does not finish within `limit`.
fn finishes_within<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit)
        .unwrap_or_else(|_| panic!("did not finish within {:?}", limit))
}

fn repeat_to(parts: &[&str], target_len: usize) -> String {
    let mut s = String::with_capacity(target_len + 64);
    while s.len() < target_len {
        for p in parts {
            s.push_str(p);
        }
    }
    s
}

/// Hostile bodies (many namespaced opening tags followed by many unrelated closing
/// tags) must be scanned in roughly linear time.
#[test]
fn test_extract_xml_tag_is_linear_on_hostile_input() {
    common::isolate();
    let half = 128 * 1024;
    let mut xml = repeat_to(&["<u:friendlyName>"], half);
    xml.push_str(&repeat_to(&["</a>"], half));
    let start = Instant::now();
    let res = finishes_within(Duration::from_secs(2), move || extract_xml_tag(&xml, "friendlyName"));
    assert_eq!(res, None);
    assert!(start.elapsed() < Duration::from_millis(500), "took {:?}", start.elapsed());

    // Unterminated closing tags after a matching opening tag.
    let mut xml = String::from("<u:CurrentVolume>");
    xml.push_str(&repeat_to(&["</"], 256 * 1024));
    let res = finishes_within(Duration::from_secs(2), move || extract_xml_tag(&xml, "CurrentVolume"));
    assert_eq!(res, None);

    // A full device description padded to the size cap parses quickly too.
    let mut xml = repeat_to(&["<u:modelName>", "</x>", "<y:z a=\"1\">"], 250 * 1024);
    xml.push_str(&renderer_xml("Padded"));
    let dev = finishes_within(Duration::from_secs(2), move || {
        parse_device_description(&xml, "http://10.0.0.5:49152/desc.xml")
    });
    assert_eq!(dev.map(|d| d.friendly_name), Some("Padded".to_string()));
}

#[test]
fn test_extract_xml_tag_behaviour() {
    common::isolate();
    let x = |xml: &str, tag: &str| extract_xml_tag(xml, tag);
    assert_eq!(x("<a><b> hi </b></a>", "b").as_deref(), Some("hi"));
    assert_eq!(x("<u:b xmlns:u=\"urn:x\">v</u:b>", "b").as_deref(), Some("v"));
    assert_eq!(x("<b attr=\"1\">v</b>", "b").as_deref(), Some("v"));
    assert_eq!(x("<u:b>v</b>", "b").as_deref(), Some("v"));
    assert_eq!(x("<b>v</u:b >", "b").as_deref(), Some("v"));
    // The plain form wins over an earlier namespaced one.
    assert_eq!(x("<u:b>first</u:b><b>second</b>", "b").as_deref(), Some("second"));
    // Content up to the first matching close, other tags included.
    assert_eq!(x("<u:b><i>x</i>y</u:b>", "b").as_deref(), Some("<i>x</i>y"));
    // A stray '<' in the content does not hide the closing tag.
    assert_eq!(x("<u:b>1 < 2</u:b>", "b").as_deref(), Some("1 < 2"));
    // Similar names, self-closing tags, closing tags without an opening one.
    assert_eq!(x("<bb>v</bb>", "b"), None);
    assert_eq!(x("<u:bb>v</u:bb>", "b"), None);
    assert_eq!(x("<b/>", "b"), None);
    assert_eq!(x("v</b>", "b"), None);
    assert_eq!(x("<u:b>unterminated", "b"), None);
    assert_eq!(x("", "b"), None);
    // Multi-byte text around the tags.
    assert_eq!(x("<é><u:b>ünï</u:b>", "b").as_deref(), Some("ünï"));
}

/// SOAP responses are capped at a realistic size.
#[test]
fn test_soap_response_size_is_capped() {
    common::isolate();
    let server = spawn_server(Arc::new(|s: &mut TcpStream| {
        let filler = "x".repeat(200 * 1024);
        let body = format!(
            "<s:Envelope><s:Body><u:GetTransportInfoResponse><CurrentTransportState>PLAYING</CurrentTransportState><Pad>{}</Pad></u:GetTransportInfoResponse></s:Body></s:Envelope>",
            filler
        );
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(resp.as_bytes());
    }));
    let device = DLNADevice {
        friendly_name: "Big".to_string(),
        location: server.url(),
        host: "127.0.0.1".to_string(),
        port: server.port,
        manufacturer: String::new(),
        model_name: String::new(),
        av_transport_control_url: format!("http://127.0.0.1:{}/ctrl", server.port),
        rendering_control_url: String::new(),
        udn: String::new(),
    };
    let client = UPnPClient::new(device);
    let res = client.call_action(
        &client.device.av_transport_control_url,
        AV_TRANSPORT_SERVICE,
        "GetTransportInfo",
        &[("InstanceID", "0")],
        Duration::from_secs(3),
    );
    assert!(
        matches!(res, Err(SoapError::Transport(HttpError::TooLarge(_)))),
        "got {:?}",
        res.map(|b| b.len())
    );
}

// ---------------------------------------------------------------------------
// Control URLs must name an IP address
// ---------------------------------------------------------------------------

/// SOAP actions to a control URL whose host is a name are refused up front instead
/// of waiting on a DNS lookup outside the request deadline.
#[test]
fn test_soap_control_url_must_be_ip_literal() {
    common::isolate();
    let device = DLNADevice {
        friendly_name: "Named".to_string(),
        location: "http://10.0.0.9:49152/desc.xml".to_string(),
        host: "10.0.0.9".to_string(),
        port: 49152,
        manufacturer: String::new(),
        model_name: String::new(),
        av_transport_control_url: "http://renderer.invalid:49152/ctrl".to_string(),
        rendering_control_url: String::new(),
        udn: String::new(),
    };
    let client = UPnPClient::new(device);
    let res = client.call_action(
        &client.device.av_transport_control_url,
        AV_TRANSPORT_SERVICE,
        "GetTransportInfo",
        &[("InstanceID", "0")],
        Duration::from_secs(3),
    );
    assert!(matches!(res, Err(SoapError::InvalidUrl(_))), "got {:?}", res);
}

/// The shared HTTP client never resolves host names.
#[test]
fn test_http_exchange_rejects_host_names() {
    common::isolate();
    let deadline = Instant::now() + Duration::from_secs(1);
    let res = http_exchange("renderer.invalid", 80, b"GET / HTTP/1.1\r\n\r\n", deadline, 1024);
    assert!(matches!(res, Err(HttpError::Resolve(_))), "got {:?}", res);
    // IP literals, including IPv6, still work.
    let server = spawn_server(Arc::new(|s: &mut TcpStream| {
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    }));
    let res = http_exchange("127.0.0.1", server.port, b"GET / HTTP/1.1\r\n\r\n", deadline, 1024);
    assert_eq!(res.unwrap().body, b"ok");
    if let Some(v6) = spawn_server_on(
        "::1",
        Arc::new(|s: &mut TcpStream| {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nv6");
        }),
    ) {
        let res = http_exchange("::1", v6.port, b"GET / HTTP/1.1\r\n\r\n", deadline, 1024);
        assert_eq!(res.unwrap().body, b"v6");
    }
}

fn drain(s: &mut FetchScheduler) -> Vec<FetchJob> {
    std::iter::from_fn(|| s.next_job()).collect()
}

/// Per-host budgets: concurrency, LOCATIONs per scan, and round-robin across hosts.
#[test]
fn test_fetch_scheduler_is_fair_across_hosts() {
    common::isolate();
    let mut s = FetchScheduler::new(8, 2, 4, 64);
    let mut accepted = 0;
    for i in 0..50 {
        if s.submit(format!("http://10.0.0.66:{}/d{}.xml", 40000 + i, i)) {
            accepted += 1;
        }
    }
    assert_eq!(accepted, 4, "one host may contribute at most 4 LOCATIONs");
    assert!(s.submit("http://10.0.0.7:49152/desc.xml".to_string()));
    assert!(s.submit("http://[fe80::7]:49152/desc.xml".to_string()));
    // Duplicates and non-http URLs are ignored.
    assert!(!s.submit("http://10.0.0.7:49152/desc.xml".to_string()));
    assert!(!s.submit("https://10.0.0.8/desc.xml".to_string()));

    let started = drain(&mut s);
    let hosts: Vec<&str> = started.iter().map(|j| j.host.as_str()).collect();
    assert_eq!(hosts, ["10.0.0.66", "10.0.0.7", "fe80::7", "10.0.0.66"]);
    assert_eq!(s.in_flight(), 4);

    // The busy host gets its next slot only when one of its fetches finishes.
    s.complete("10.0.0.66");
    let next = drain(&mut s);
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].host, "10.0.0.66");
    assert_eq!(next[0].index, 2);
}

/// The overall concurrency cap holds across many hosts, and the per-scan budget
/// still applies.
#[test]
fn test_fetch_scheduler_global_limits() {
    common::isolate();
    let mut s = FetchScheduler::new(3, 2, 4, 5);
    for i in 0..10 {
        s.submit(format!("http://10.0.1.{}/desc.xml", i));
    }
    assert_eq!(drain(&mut s).len(), 3);
    for host in ["10.0.1.0", "10.0.1.1", "10.0.1.2"] {
        s.complete(host);
    }
    assert_eq!(drain(&mut s).len(), 2, "only 5 LOCATIONs are accepted per scan");
    s.complete("10.0.1.3");
    s.complete("10.0.1.4");
    assert!(s.is_idle());
    // IPv4-mapped IPv6 is the same host as its IPv4 form.
    let mut s = FetchScheduler::new(8, 1, 4, 64);
    s.submit("http://10.0.2.1:1/a.xml".to_string());
    s.submit("http://[::ffff:10.0.2.1]:1/b.xml".to_string());
    assert_eq!(drain(&mut s).len(), 1);
}

/// Many hanging LOCATIONs from one host must not stop a real renderer on another
/// host from being found within the normal post-listen window.
#[test]
fn test_one_host_cannot_starve_description_fetches() {
    common::isolate();
    let Some(real) = spawn_server_on(
        "::1",
        Arc::new(|s: &mut TcpStream| write_xml(s, &renderer_xml("Real Renderer"))),
    ) else {
        eprintln!("IPv6 loopback unavailable; covered by the scheduler unit tests");
        return;
    };
    let hostile = hanging_server();

    let mut fetcher = DescriptionFetcher::new(MAX_PARALLEL_DESCRIPTION_FETCHES, DESCRIPTION_FETCH_TIMEOUT);
    for i in 0..80 {
        fetcher.submit(format!("http://127.0.0.1:{}/hang{}.xml", hostile.port, i));
    }
    fetcher.submit(real.url());

    let start = Instant::now();
    let deadline = start + DESCRIPTION_FETCH_TIMEOUT;
    let devices = fetcher.finish(deadline);
    let elapsed = start.elapsed();

    let names: Vec<&str> = devices.iter().map(|d| d.friendly_name.as_str()).collect();
    assert_eq!(names, ["Real Renderer"]);
    assert!(
        elapsed < DESCRIPTION_FETCH_TIMEOUT + Duration::from_millis(500),
        "finish overran its deadline: {:?}",
        elapsed
    );
}
