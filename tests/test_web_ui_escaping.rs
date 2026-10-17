//! Web dashboard output-encoding tests.
//!
//! Track titles, file names, paths and device names come from untrusted
//! sources (tags, shared network volumes, SSDP). These tests make sure the
//! embedded dashboard never turns that data into markup or code.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use quickstream::web::{
    NowPlayingState, WebBrowserResponse, WebDeviceItem, WebQueueItem, WebServer,
};

/// Names that would break naive HTML / JS-string interpolation.
const HOSTILE: &[&str] = &[
    "it's a track",
    "say \"hello\"",
    "x&#39;);globalThis.__pwned=1;//",
    "</script><script>globalThis.__pwned=1</script>",
    "ends with backslash\\",
    "<img src=x onerror=\"globalThis.__pwned=1\">",
];

fn http_get(port: u16, path: &str) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        path, port
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(raw.len());
    let headers = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let body = raw.get(header_end + 4..).map(|b| b.to_vec()).unwrap_or_default();
    let status = headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, headers, body)
}

fn fetch_dashboard() -> String {
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    let (status, _, body) = http_get(handle.port, "/");
    handle.stop();
    assert_eq!(status, 200);
    String::from_utf8(body).expect("dashboard is utf-8")
}

/// Returns every inline event-handler attribute (`onclick="..."` etc.) in `html`.
fn inline_handler_attrs(html: &str) -> Vec<String> {
    let bytes = html.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i + 3 < bytes.len() {
        let boundary = bytes[i].is_ascii_whitespace() || bytes[i] == b'<' || bytes[i] == b'"' || bytes[i] == b'\'';
        if boundary && bytes[i + 1] == b'o' && bytes[i + 2] == b'n' {
            let mut j = i + 3;
            while j < bytes.len() && bytes[j].is_ascii_lowercase() {
                j += 1;
            }
            let mut k = j;
            while k < bytes.len() && bytes[k] == b' ' {
                k += 1;
            }
            if j > i + 3 && k < bytes.len() && bytes[k] == b'=' {
                let mut v = k + 1;
                while v < bytes.len() && bytes[v] == b' ' {
                    v += 1;
                }
                if v < bytes.len() && (bytes[v] == b'"' || bytes[v] == b'\'') {
                    found.push(html[i + 1..(v + 1).min(html.len())].to_string());
                }
            }
        }
        i += 1;
    }
    found
}

#[test]
fn test_dashboard_has_no_inline_event_handlers() {
    let html = fetch_dashboard();
    let handlers = inline_handler_attrs(&html);
    assert!(
        handlers.is_empty(),
        "dashboard contains inline event handler attributes: {:?}",
        handlers
    );
    assert!(
        !html.contains("escapeAttr"),
        "dashboard still builds JS-string attributes with escapeAttr"
    );
}

#[test]
fn test_dashboard_never_builds_html_from_data() {
    let html = fetch_dashboard();
    for sink in ["insertAdjacentHTML", "outerHTML", "document.write"] {
        assert!(!html.contains(sink), "dashboard uses HTML sink {}", sink);
    }
    // Every innerHTML assignment must be a constant: no template
    // interpolation and no string concatenation.
    let mut rest = html.as_str();
    let mut count = 0;
    while let Some(pos) = rest.find("innerHTML") {
        let stmt_start = &rest[pos..];
        let end = stmt_start.find(';').unwrap_or(stmt_start.len());
        let stmt = &stmt_start[..end];
        assert!(
            !stmt.contains("${") && !stmt.contains(" + ") && !stmt.contains(".map("),
            "innerHTML built from data: {}",
            stmt
        );
        count += 1;
        rest = &stmt_start[end..];
    }
    assert!(count < 20, "unexpected number of innerHTML uses: {}", count);
}

fn hostile_test_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qs_ui_escaping_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// File names cannot contain '/', so swap it for '_' (the rest of each
/// payload is kept as-is).
fn as_filename(name: &str) -> String {
    name.replace('/', "_")
}

#[test]
fn test_browser_api_round_trips_hostile_names() {
    let dir = hostile_test_dir("browser");
    let mut expected_dirs = Vec::new();
    let mut expected_files = Vec::new();
    for name in HOSTILE.iter().map(|n| as_filename(n)) {
        let d = format!("dir {}", name);
        fs::create_dir_all(dir.join(&d)).expect("create hostile dir");
        expected_dirs.push(d);
        let f = format!("{}.mp3", name);
        fs::write(dir.join(&f), b"fake").expect("write hostile file");
        expected_files.push(f);
    }
    assert_eq!(expected_files.len(), HOSTILE.len());

    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    // The browser API only lists folders under the configured library roots.
    state.write().unwrap().allowed_roots = vec![dir.canonicalize().expect("canonical temp dir")];
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    let encoded = quickstream::player::upnp::url_encode(&dir.to_string_lossy());
    let (status, headers, body) = http_get(handle.port, &format!("/api/browser?path={}", encoded));
    handle.stop();

    assert_eq!(status, 200);
    assert!(headers.contains("application/json"));
    let resp: WebBrowserResponse = serde_json::from_slice(&body).expect("valid JSON");

    let mut got_dirs: Vec<String> = resp.entries.iter().filter(|e| e.is_dir).map(|e| e.name.clone()).collect();
    let mut got_files: Vec<String> = resp.entries.iter().filter(|e| !e.is_dir).map(|e| e.name.clone()).collect();
    got_dirs.sort();
    got_files.sort();
    expected_dirs.sort();
    expected_files.sort();
    assert_eq!(got_dirs, expected_dirs);
    assert_eq!(got_files, expected_files);
    for entry in &resp.entries {
        // The browser API reports canonical paths (e.g. /private/var/... on macOS).
        assert_eq!(Path::new(&entry.path), dir.canonicalize().unwrap().join(&entry.name));
    }

    let _ = fs::remove_dir_all(&dir);
}

fn hostile_state() -> NowPlayingState {
    let mut s = NowPlayingState {
        title: HOSTILE[3].to_string(),
        artist: HOSTILE[5].to_string(),
        album: HOSTILE[2].to_string(),
        filepath: Some(PathBuf::from(format!("/music/{}", HOSTILE[4]))),
        output_device: HOSTILE[1].to_string(),
        next_track_title: Some(HOSTILE[0].to_string()),
        // /api/status only reports paths inside the allowed folders.
        allowed_roots: vec![PathBuf::from("/music")],
        ..NowPlayingState::default()
    };
    s.queue = HOSTILE
        .iter()
        .enumerate()
        .map(|(i, v)| WebQueueItem {
            index: i,
            title: v.to_string(),
            artist: v.to_string(),
            album: v.to_string(),
            duration_seconds: 60.0,
            filepath: format!("/music/{}", v),
            is_current: i == 0,
        })
        .collect();
    s.devices = HOSTILE
        .iter()
        .enumerate()
        .map(|(i, v)| WebDeviceItem {
            id: i,
            name: v.to_string(),
            host_port: v.to_string(),
            is_active: i == 0,
        })
        .collect();
    s
}

#[test]
fn test_status_api_round_trips_hostile_names() {
    let state = Arc::new(RwLock::new(hostile_state()));
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state.clone(), tx).expect("start web server");
    let (status, headers, body) = http_get(handle.port, "/api/status");
    handle.stop();

    assert_eq!(status, 200);
    assert!(headers.contains("application/json"));
    let got: NowPlayingState = serde_json::from_slice(&body).expect("valid JSON");
    let want = state.read().unwrap().clone();
    assert_eq!(got.title, want.title);
    assert_eq!(got.artist, want.artist);
    assert_eq!(got.album, want.album);
    assert_eq!(got.filepath, want.filepath);
    assert_eq!(got.output_device, want.output_device);
    assert_eq!(got.next_track_title, want.next_track_title);
    assert_eq!(got.queue, want.queue);
    assert_eq!(got.devices, want.devices);
}

/// Extracts the dashboard's inline `<script>` body.
fn dashboard_script(html: &str) -> String {
    let start = html.rfind("<script>").expect("dashboard has an inline script") + "<script>".len();
    let end = html[start..].find("</script>").expect("script is closed") + start;
    html[start..end].to_string()
}

#[test]
fn test_dashboard_renders_hostile_names_as_text_in_js() {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("SKIPPED: node is not installed; dashboard DOM behavior test not run");
        return;
    }
    let html = fetch_dashboard();
    let dir = hostile_test_dir("js");
    let script_path = dir.join("dashboard.js");
    fs::write(&script_path, dashboard_script(&html)).expect("write dashboard script");

    let harness = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/dashboard_dom_harness.js");
    let output = Command::new("node")
        .arg(&harness)
        .arg(&script_path)
        .output()
        .expect("run node");
    let _ = fs::remove_dir_all(&dir);

    assert!(
        output.status.success(),
        "dashboard DOM harness failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
