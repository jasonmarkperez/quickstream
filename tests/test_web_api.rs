//! Request handling of the REST API through the real server on 127.0.0.1: `/api/action` refuses
//! bodies it can't act on (issue #109), routes match whole paths and answer 405 for a method they
//! don't take (issue #26), and the shared response helpers keep the bytes on the wire (issue #110).

mod common;

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use quickstream::web::{
    NowPlayingState, ServerLimits, StreamServerState, WebBind, WebCommand, WebServer, WebServerHandle,
};

fn start() -> (WebServerHandle, mpsc::Receiver<WebCommand>) {
    start_with_limits(ServerLimits::default())
}

fn start_with_limits(limits: ServerLimits) -> (WebServerHandle, mpsc::Receiver<WebCommand>) {
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, rx) = mpsc::channel();
    let handle = WebServer::start_with_options(
        WebBind::Loopback,
        0,
        state,
        tx,
        Arc::new(StreamServerState::new()),
        limits,
    )
    .expect("start web server");
    (handle, rx)
}

/// Status code, header block and body of a raw response.
fn parse(raw: &[u8]) -> (u16, String, Vec<u8>) {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(raw.len());
    let headers = String::from_utf8_lossy(&raw[..end]).to_string();
    let body = raw.get(end + 4..).map(<[u8]>::to_vec).unwrap_or_default();
    let status = headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, headers, body)
}

fn connect(port: u16) -> TcpStream {
    let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    stream
}

fn read_all(mut stream: TcpStream) -> Vec<u8> {
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    raw
}

/// Send `request` and read the whole response, unparsed.
fn raw_exchange(port: u16, request: &[u8]) -> Vec<u8> {
    let mut stream = connect(port);
    stream.write_all(request).unwrap();
    read_all(stream)
}

/// A request with a valid Host; methods other than GET / HEAD carry a JSON Content-Type so the
/// access checks let them through to routing.
fn request(port: u16, method: &str, path: &str, body: &str) -> (u16, String, Vec<u8>) {
    let content_type = if method == "GET" || method == "HEAD" {
        ""
    } else {
        "Content-Type: application/json\r\n"
    };
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{content_type}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    parse(&raw_exchange(port, req.as_bytes()))
}

fn post_action(port: u16, body: &str) -> (u16, String, serde_json::Value) {
    let (status, headers, body) = request(port, "POST", "/api/action", body);
    let json = serde_json::from_slice(&body).unwrap_or_else(|e| panic!("JSON body ({e}): {headers}"));
    (status, headers, json)
}

fn header_value(headers: &str, name: &str) -> Option<String> {
    headers.lines().skip(1).find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
    })
}

/// A 400 answer to `/api/action`: JSON `{"ok":false,"error":…}` with the API's headers.
fn assert_refused(status: u16, headers: &str, json: &serde_json::Value, error: &str, case: &str) {
    assert_eq!(status, 400, "{case}: {headers}");
    assert_eq!(json["ok"], serde_json::json!(false), "{case}: {json}");
    assert_eq!(json["error"], serde_json::json!(error), "{case}: {json}");
    assert_eq!(
        header_value(headers, "content-type").as_deref(),
        Some("application/json"),
        "{case}"
    );
    assert_eq!(
        header_value(headers, "x-content-type-options").as_deref(),
        Some("nosniff"),
        "{case}"
    );
    assert_eq!(
        header_value(headers, "cache-control").as_deref(),
        Some("no-store"),
        "{case}"
    );
}

const PLAYLIST_DELETE: &str = r#"{"action":"playlist_delete","name":"Mix"}"#;

#[test]
fn test_action_body_cut_short_is_refused_with_400() {
    common::isolate();
    let (handle, rx) = start();

    // The client declares the whole body, sends 10 bytes of it and closes its side.
    let mut stream = connect(handle.port);
    let head = format!(
        "POST /api/action HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        handle.port,
        PLAYLIST_DELETE.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(&PLAYLIST_DELETE.as_bytes()[..10]).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let (status, headers, body) = parse(&read_all(stream));
    let json: serde_json::Value = serde_json::from_slice(&body).expect("JSON body");
    assert_refused(status, &headers, &json, "Incomplete request body", "closed early");

    assert!(rx.try_recv().is_err(), "no command for a body that was cut short");
    handle.stop();
}

#[test]
fn test_action_body_that_times_out_is_refused_with_400() {
    common::isolate();
    let limits = ServerLimits {
        request_timeout: Duration::from_secs(1),
        ..ServerLimits::default()
    };
    let (handle, rx) = start_with_limits(limits);

    // Part of the body, then nothing: the request deadline runs out with the socket still open.
    let mut stream = connect(handle.port);
    let head = format!(
        "POST /api/action HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        handle.port,
        PLAYLIST_DELETE.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(&PLAYLIST_DELETE.as_bytes()[..10]).unwrap();
    let (status, headers, body) = parse(&read_all(stream));
    let json: serde_json::Value = serde_json::from_slice(&body).expect("JSON body");
    assert_refused(status, &headers, &json, "Incomplete request body", "timed out");

    assert!(rx.try_recv().is_err(), "no command for a body that timed out");
    handle.stop();
}

#[test]
fn test_action_body_that_is_not_an_action_is_refused_with_400() {
    common::isolate();
    let (handle, rx) = start();

    for body in [
        "not json",
        "",
        "[]",
        r#"{"name":"Mix"}"#,
        r#"{"action":42}"#,
        r#"{"action":"playlist_remove","name":"Mix","index":"0","revision":1}"#,
        r#"{"action":"volume","value":"loud"}"#,
        r#"{"action":"next""#,
    ] {
        let (status, headers, json) = post_action(handle.port, body);
        assert_refused(status, &headers, &json, "Invalid request", body);
    }

    assert!(rx.try_recv().is_err(), "no command for a body that isn't an action");
    handle.stop();
}

#[test]
fn test_unknown_action_is_refused_with_400() {
    common::isolate();
    let (handle, rx) = start();

    for body in [r#"{"action":"nope"}"#, r#"{"action":""}"#, r#"{"action":"NEXT"}"#] {
        let (status, headers, json) = post_action(handle.port, body);
        assert_refused(status, &headers, &json, "Unknown action", body);
    }

    assert!(rx.try_recv().is_err(), "no command for an unknown action");
    handle.stop();
}

#[test]
fn test_known_actions_keep_their_answers() {
    common::isolate();
    let (handle, rx) = start();

    // Fire-and-forget playback actions: 200 {"ok":true} and their command.
    let (status, headers, json) = post_action(handle.port, r#"{"action":"next"}"#);
    assert_eq!(status, 200, "{headers}");
    assert_eq!(json, serde_json::json!({ "ok": true }));
    assert_eq!(header_value(&headers, "cache-control").as_deref(), Some("no-store"));
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), WebCommand::Next);

    // Unknown fields are ignored, as before.
    let (status, _, json) = post_action(handle.port, r#"{"action":"prev","extra":[1,2]}"#);
    assert_eq!(status, 200);
    assert_eq!(json, serde_json::json!({ "ok": true }));
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), WebCommand::Prev);

    // A known action missing its value still answers 200 and sends nothing.
    let (status, _, json) = post_action(handle.port, r#"{"action":"volume"}"#);
    assert_eq!(status, 200);
    assert_eq!(json, serde_json::json!({ "ok": true }));
    assert!(rx.try_recv().is_err(), "volume without a value sends nothing");

    // Playlist actions keep their own refusals.
    let (status, headers, json) = post_action(handle.port, r#"{"action":"playlist_delete"}"#);
    assert_refused(status, &headers, &json, "Missing playlist name", "playlist_delete");

    handle.stop();
}

#[test]
fn test_routes_match_whole_paths_only() {
    common::isolate();
    let (handle, rx) = start();

    for path in [
        "/api/statusXYZ",
        "/api/status/",
        "/api/status/extra",
        "/api/playlistsX",
        "/api/coverX",
        "/api/cover/art.jpg",
        "/api/browserX",
        "/api/browser/x",
        "/api/",
        "/api",
        "/index.htmlX",
        "/streamXYZ",
        "/stream",
        "/stream/",
        "/stream/a/b.mp3",
        "/nope",
    ] {
        for method in ["GET", "HEAD"] {
            let (status, headers, _) = request(handle.port, method, path, "");
            assert_eq!(status, 404, "{method} {path}: {headers}");
        }
    }
    for path in ["/api/actionXYZ", "/api/action/", "/api/action/next"] {
        let (status, headers, _) = request(handle.port, "POST", path, r#"{"action":"next"}"#);
        assert_eq!(status, 404, "POST {path}: {headers}");
    }

    assert!(rx.try_recv().is_err(), "no command for a path that isn't a route");
    handle.stop();
}

#[test]
fn test_routes_keep_working_with_query_strings() {
    common::isolate();
    let (handle, rx) = start();

    for path in [
        "/",
        "/?t=1",
        "/index.html",
        "/api/status",
        "/api/status?t=1",
        "/api/playlists?t=1",
        "/api/cover?t=1",
    ] {
        let (status, headers, _) = request(handle.port, "GET", path, "");
        assert_eq!(status, 200, "GET {path}: {headers}");
    }
    // `/api/playlists` has the playlists' files checked.
    assert_eq!(rx.try_recv().ok(), Some(WebCommand::CheckPlaylists));

    let (status, _, body) = request(handle.port, "POST", "/api/action?x=1", r#"{"action":"next"}"#);
    assert_eq!(status, 200);
    assert_eq!(body, br#"{"ok":true}"#.to_vec());
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), WebCommand::Next);

    handle.stop();
}

#[test]
fn test_wrong_method_on_a_route_is_405_with_allow() {
    common::isolate();
    let (handle, rx) = start();
    let continuous = handle.stream_state().arm_continuous_session();

    let cases: Vec<(&str, String, &str)> = vec![
        ("GET", "/api/action".to_string(), "POST"),
        ("HEAD", "/api/action".to_string(), "POST"),
        ("PUT", "/api/action".to_string(), "POST"),
        ("DELETE", "/api/action".to_string(), "POST"),
        ("POST", "/".to_string(), "GET, HEAD"),
        ("POST", "/index.html".to_string(), "GET, HEAD"),
        ("POST", "/api/status".to_string(), "GET, HEAD"),
        ("PUT", "/api/status?t=1".to_string(), "GET, HEAD"),
        ("POST", "/api/playlists".to_string(), "GET, HEAD"),
        ("DELETE", "/api/playlists".to_string(), "GET, HEAD"),
        ("POST", "/api/cover".to_string(), "GET, HEAD"),
        ("POST", "/api/browser".to_string(), "GET, HEAD"),
        ("OPTIONS", "/api/status".to_string(), "GET, HEAD"),
        ("POST", "/stream/song.mp3".to_string(), "GET, HEAD"),
        ("POST", continuous.clone(), "GET, HEAD"),
    ];
    for (method, path, allow) in cases {
        let (status, headers, _) = request(handle.port, method, &path, r#"{"action":"next"}"#);
        assert_eq!(status, 405, "{method} {path}: {headers}");
        assert_eq!(
            header_value(&headers, "allow").as_deref(),
            Some(allow),
            "{method} {path}"
        );
        assert_eq!(
            header_value(&headers, "connection").as_deref(),
            Some("close"),
            "{method} {path}"
        );
    }

    // A refused method neither runs the route nor starts the continuous stream.
    assert!(rx.try_recv().is_err(), "no command for a refused method");
    assert!(!handle
        .stream_state()
        .continuous
        .is_active
        .load(std::sync::atomic::Ordering::SeqCst));
    handle.stop();
}

#[test]
fn test_not_found_and_json_responses_keep_their_bytes() {
    common::isolate();
    let (handle, _rx) = start();
    let port = handle.port;

    let get = |path: &str| {
        raw_exchange(
            port,
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
    };
    assert_eq!(
        get("/nope"),
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found".to_vec()
    );
    assert_eq!(
        get("/stream/unknown.mp3"),
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found".to_vec()
    );

    for path in ["/api/status", "/api/playlists"] {
        let (status, headers, body) = parse(&get(path));
        assert_eq!(status, 200, "{path}");
        let lines: Vec<&str> = headers.lines().collect();
        assert_eq!(
            lines,
            vec![
                "HTTP/1.1 200 OK",
                "Content-Type: application/json",
                "X-Content-Type-Options: nosniff",
                "Cache-Control: no-cache",
                &format!("Content-Length: {}", body.len()),
                "Connection: close",
            ],
            "{path}"
        );
    }

    // HEAD gets the same head and no body.
    let raw = raw_exchange(
        port,
        format!("HEAD /api/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").as_bytes(),
    );
    let (status, headers, body) = parse(&raw);
    assert_eq!(status, 200);
    assert!(body.is_empty(), "HEAD has no body");
    assert!(
        header_value(&headers, "content-length").is_some_and(|n| n != "0"),
        "{headers}"
    );

    let (_, headers, body) = request(port, "POST", "/api/action", r#"{"action":"next"}"#);
    let lines: Vec<&str> = headers.lines().collect();
    assert_eq!(
        lines,
        vec![
            "HTTP/1.1 200 OK",
            "Content-Type: application/json",
            "X-Content-Type-Options: nosniff",
            "Cache-Control: no-store",
            &format!("Content-Length: {}", body.len()),
            "Connection: close",
        ]
    );
    assert_eq!(body, br#"{"ok":true}"#.to_vec());

    handle.stop();
}
