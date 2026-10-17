//! Last.fm enrichment client (#27): no key in source, opt-in only, User-Agent, throttling and
//! an in-memory cache. Every request here goes to a mock server on 127.0.0.1.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use quickstream::analysis::online::{OnlineConfig, OnlineMetadataClient, API_KEY_ENV};
use quickstream::settings::Settings;

const TAGS_JSON: &str = r#"{"toptags":{"tag":[{"name":"Shoegaze","count":100},{"name":"Dream Pop","count":80},{"name":"uk","count":10}],"@attr":{"artist":"Slowdive"}}}"#;
const SIMILAR_JSON: &str = r#"{"similarartists":{"artist":[{"name":"Ride","match":"0.92"},{"name":"Lush","match":"0.81"},{"name":"Broken","match":"x"}]}}"#;
const RATE_LIMITED_JSON: &str = r#"{"error":29,"message":"Rate Limit Exceeded"}"#;

#[derive(Clone, Debug)]
struct Seen {
    at: Instant,
    request_line: String,
    headers: Vec<String>,
}

/// A tiny HTTP server that answers each request with the next body from `bodies` (repeating
/// the last one) and records what it was sent.
struct MockServer {
    base_url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(bodies: Vec<&'static str>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://127.0.0.1:{}/2.0/", listener.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen2, stop2) = (seen.clone(), stop.clone());
        let handle = thread::spawn(move || {
            let mut served = 0usize;
            while !stop2.load(Ordering::SeqCst) {
                let (stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(_) => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                };
                let at = Instant::now();
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                let _ = reader.read_line(&mut request_line);
                let mut headers = Vec::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    headers.push(line.trim_end().to_string());
                }
                seen2.lock().unwrap().push(Seen { at, request_line: request_line.trim_end().to_string(), headers });
                let body = bodies[served.min(bodies.len() - 1)];
                served += 1;
                let mut stream = reader.into_inner();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.flush();
            }
        });
        MockServer { base_url, seen, stop, handle: Some(handle) }
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn config(server: &MockServer, enabled: bool, key: Option<&str>) -> OnlineConfig {
    OnlineConfig {
        enabled,
        api_key: key.map(str::to_string),
        base_url: server.base_url.clone(),
        min_interval: Duration::from_millis(0),
        timeout: Duration::from_secs(2),
    }
}

// ---------------------------------------------------------------------------------------------
// No key in source
// ---------------------------------------------------------------------------------------------

#[test]
fn test_source_has_no_hardcoded_api_key() {
    common::isolate();
    let src = include_str!("../src/analysis/online.rs");
    let bytes = src.as_bytes();
    let has_hex_run = bytes
        .windows(32)
        .any(|w| w.iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b)));
    assert!(!has_hex_run, "online.rs contains a 32-character hex string (an API key?)");
    assert!(!src.contains("api_key=b"), "online.rs embeds an api_key value");
}

// ---------------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------------

#[test]
fn test_config_from_settings_is_off_by_default_and_reads_key_from_env() {
    common::isolate();
    std::env::remove_var(API_KEY_ENV);
    let defaults = Settings::default();
    let cfg = OnlineConfig::from_settings(&defaults);
    assert!(!cfg.enabled, "online enrichment must be opt-in");
    assert_eq!(cfg.api_key, None);
    assert!(!OnlineMetadataClient::new(cfg).is_enabled());

    let opted_in = Settings { online_enrichment: true, ..Settings::default() };
    let cfg = OnlineConfig::from_settings(&opted_in);
    assert!(cfg.enabled);
    assert_eq!(cfg.api_key, None);
    assert!(!OnlineMetadataClient::new(cfg).is_enabled(), "no key means no client");

    std::env::set_var(API_KEY_ENV, "  ");
    assert_eq!(OnlineConfig::from_settings(&opted_in).api_key, None, "blank key counts as absent");

    std::env::set_var(API_KEY_ENV, " envkey123 ");
    let cfg = OnlineConfig::from_settings(&opted_in);
    assert_eq!(cfg.api_key.as_deref(), Some("envkey123"));
    assert!(cfg.min_interval >= Duration::from_millis(250));
    assert!(cfg.base_url.starts_with("https://"));
    assert!(OnlineMetadataClient::new(cfg).is_enabled());

    // A key alone does not opt in.
    let cfg = OnlineConfig::from_settings(&defaults);
    assert!(!OnlineMetadataClient::new(cfg).is_enabled());
    std::env::remove_var(API_KEY_ENV);
}

#[test]
fn test_disabled_setting_makes_no_requests() {
    common::isolate();
    let server = MockServer::start(vec![TAGS_JSON]);
    let client = OnlineMetadataClient::new(config(&server, false, Some("testkey")));
    assert!(client.fetch_artist_tags("Slowdive").is_empty());
    assert!(client.fetch_similar_artists("Slowdive").is_empty());
    thread::sleep(Duration::from_millis(100));
    assert!(server.requests().is_empty(), "{:?}", server.requests());
}

#[test]
fn test_missing_or_blank_api_key_makes_no_requests() {
    common::isolate();
    let server = MockServer::start(vec![TAGS_JSON]);
    for key in [None, Some(""), Some("   ")] {
        let client = OnlineMetadataClient::new(config(&server, true, key));
        assert!(!client.is_enabled());
        assert!(client.fetch_artist_tags("Slowdive").is_empty());
    }
    thread::sleep(Duration::from_millis(100));
    assert!(server.requests().is_empty(), "{:?}", server.requests());
}

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

#[test]
fn test_fetch_tags_sends_key_and_user_agent_and_parses_response() {
    common::isolate();
    let server = MockServer::start(vec![TAGS_JSON]);
    let client = OnlineMetadataClient::new(config(&server, true, Some("testkey")));
    let tags = client.fetch_artist_tags("Sigur Rós & Co");
    assert_eq!(tags, vec!["shoegaze".to_string(), "dream pop".to_string()]);

    let reqs = server.requests();
    assert_eq!(reqs.len(), 1);
    let line = &reqs[0].request_line;
    assert!(line.starts_with("GET /2.0/?"), "{}", line);
    assert!(line.contains("method=artist.gettoptags"), "{}", line);
    assert!(line.contains("artist=Sigur%20R%C3%B3s%20%26%20Co"), "{}", line);
    assert!(line.contains("api_key=testkey"), "{}", line);
    assert!(line.contains("format=json"), "{}", line);
    let ua = reqs[0]
        .headers
        .iter()
        .find_map(|h| h.strip_prefix("User-Agent: ").or_else(|| h.strip_prefix("user-agent: ")))
        .expect("User-Agent header");
    assert!(ua.starts_with("quickstream/"), "{}", ua);
    assert!(ua.contains(env!("CARGO_PKG_VERSION")), "{}", ua);
}

#[test]
fn test_fetch_similar_artists_parses_response() {
    common::isolate();
    let server = MockServer::start(vec![SIMILAR_JSON]);
    let client = OnlineMetadataClient::new(config(&server, true, Some("testkey")));
    let similar = client.fetch_similar_artists("Slowdive");
    assert_eq!(similar, vec![("Ride".to_string(), 0.92), ("Lush".to_string(), 0.81)]);
    let reqs = server.requests();
    assert_eq!(reqs.len(), 1);
    assert!(reqs[0].request_line.contains("method=artist.getsimilar"), "{}", reqs[0].request_line);
}

#[test]
fn test_results_are_cached_per_artist() {
    common::isolate();
    let server = MockServer::start(vec![TAGS_JSON]);
    let client = OnlineMetadataClient::new(config(&server, true, Some("testkey")));
    let first = client.fetch_artist_tags("Slowdive");
    let second = client.fetch_artist_tags("  slowdive ");
    assert_eq!(first, second);
    assert_eq!(server.requests().len(), 1, "second lookup must come from the cache");

    // A different artist is a new request; tags and similar artists are cached separately.
    client.fetch_artist_tags("Ride");
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn test_error_responses_are_not_cached() {
    common::isolate();
    let server = MockServer::start(vec![RATE_LIMITED_JSON, TAGS_JSON]);
    let client = OnlineMetadataClient::new(config(&server, true, Some("testkey")));
    assert!(client.fetch_artist_tags("Slowdive").is_empty());
    assert_eq!(client.fetch_artist_tags("Slowdive").len(), 2);
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn test_requests_are_spaced_by_the_minimum_interval() {
    common::isolate();
    let server = MockServer::start(vec![TAGS_JSON]);
    let mut cfg = config(&server, true, Some("testkey"));
    cfg.min_interval = Duration::from_millis(250);
    let client = OnlineMetadataClient::new(cfg);
    for artist in ["A1", "A2", "A3"] {
        client.fetch_artist_tags(artist);
    }
    let reqs = server.requests();
    assert_eq!(reqs.len(), 3);
    for pair in reqs.windows(2) {
        let gap = pair[1].at.duration_since(pair[0].at);
        assert!(gap >= Duration::from_millis(240), "requests only {:?} apart", gap);
    }
}

#[test]
fn test_unreachable_server_returns_empty() {
    common::isolate();
    // Bind then drop to get a port nothing listens on.
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let client = OnlineMetadataClient::new(OnlineConfig {
        enabled: true,
        api_key: Some("testkey".to_string()),
        base_url: format!("http://127.0.0.1:{}/2.0/", port),
        min_interval: Duration::from_millis(0),
        timeout: Duration::from_millis(500),
    });
    assert!(client.fetch_artist_tags("Slowdive").is_empty());
    assert!(client.fetch_similar_artists("Slowdive").is_empty());
}

#[test]
fn test_blank_artist_makes_no_request() {
    common::isolate();
    let server = MockServer::start(vec![TAGS_JSON]);
    let client = OnlineMetadataClient::new(config(&server, true, Some("testkey")));
    assert!(client.fetch_artist_tags("   ").is_empty());
    thread::sleep(Duration::from_millis(100));
    assert!(server.requests().is_empty());
}
