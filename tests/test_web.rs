mod common;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use std::path::PathBuf;

use quickstream::web::{
    parse_byte_range, ByteRange, NowPlayingState, ServerLimits, StreamServerState, WebBind, WebBrowserResponse,
    WebCommand, WebDeviceItem, WebQueueItem, WebServer, WebServerHandle,
};

fn parse_http_response(raw: &[u8]) -> (u16, String, Vec<u8>) {
    let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(raw.len());
    let header_str = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let body = if header_end + 4 <= raw.len() {
        raw[header_end + 4..].to_vec()
    } else {
        Vec::new()
    };
    let status_code = header_str
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    (status_code, header_str, body)
}

fn http_get(port: u16, path: &str) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        path, port
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut resp = Vec::new();
    let _ = stream.read_to_end(&mut resp);
    parse_http_response(&resp)
}

fn http_post(port: u16, path: &str, body: &str) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let req = format!(
        "POST {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        path, port, body.len(), body
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut resp = Vec::new();
    let _ = stream.read_to_end(&mut resp);
    parse_http_response(&resp)
}

#[test]
fn test_web_server_dashboard_endpoint() {
    common::isolate();
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    let port = handle.port;

    let (status, headers, body) = http_get(port, "/");
    assert_eq!(status, 200);
    assert!(headers.contains("text/html"));
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("QUICKSTREAM"));
    assert!(html.contains("statusLabel"));
    assert!(html.contains("volumeSlider"));

    handle.stop();
}

#[test]
fn test_web_server_status_api() {
    common::isolate();
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state.clone(), tx).expect("start web server");
    let port = handle.port;

    // Update state
    {
        let mut s = state.write().unwrap();
        s.title = "Starlight".to_string();
        s.artist = "Muse".to_string();
        s.album = "Black Holes and Revelations".to_string();
        s.is_playing = true;
        s.position_seconds = 42.5;
        s.duration_seconds = 240.0;
        s.volume = 85;
    }

    let (status, headers, body) = http_get(port, "/api/status");
    assert_eq!(status, 200);
    assert!(headers.contains("application/json"));
    let json = String::from_utf8_lossy(&body);
    assert!(json.contains("\"title\":\"Starlight\""));
    assert!(json.contains("\"artist\":\"Muse\""));
    assert!(json.contains("\"is_playing\":true"));
    assert!(json.contains("\"volume\":85"));

    handle.stop();
}

#[test]
fn test_web_server_cover_placeholder() {
    common::isolate();
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    let port = handle.port;

    let (status, headers, body) = http_get(port, "/api/cover");
    assert_eq!(status, 200);
    assert!(headers.contains("image/svg+xml"));
    let svg = String::from_utf8_lossy(&body);
    assert!(svg.contains("<svg"));

    handle.stop();
}

#[test]
fn test_web_server_cover_from_folder() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_web_cover_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).expect("create temp dir");

    let cover_file = temp_dir.join("cover.jpg");
    let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46];
    fs::write(&cover_file, &fake_jpeg).expect("write cover.jpg");

    let audio_file = temp_dir.join("track01.flac");
    fs::write(&audio_file, b"fake flac content").expect("write track01.flac");

    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    {
        let mut s = state.write().unwrap();
        s.filepath = Some(audio_file);
    }

    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    let port = handle.port;

    let (status, headers, body) = http_get(port, "/api/cover");
    assert_eq!(status, 200);
    assert!(headers.contains("image/jpeg"));
    assert_eq!(body, fake_jpeg);

    handle.stop();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_web_server_action_commands() {
    common::isolate();
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    let port = handle.port;

    // 1. Toggle pause
    let (status, _, _) = http_post(port, "/api/action", r#"{"action":"toggle_pause"}"#);
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::TogglePause
    );

    // 2. Volume
    let (status, _, _) = http_post(port, "/api/action", r#"{"action":"volume","value":75.0}"#);
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::SetVolume(75)
    );

    // 3. Next
    let (status, _, _) = http_post(port, "/api/action", r#"{"action":"next"}"#);
    assert_eq!(status, 200);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), WebCommand::Next);

    // 4. Prev
    let (status, _, _) = http_post(port, "/api/action", r#"{"action":"prev"}"#);
    assert_eq!(status, 200);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), WebCommand::Prev);

    // 5. Seek (Absolute)
    let (status, _, _) = http_post(port, "/api/action", r#"{"action":"seek","value":45.0}"#);
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::SeekTo(45.0)
    );

    // 6. Seek Relative
    let (status, _, _) = http_post(port, "/api/action", r#"{"action":"seek_relative","value":-10.0}"#);
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::SeekRelative(-10.0)
    );

    handle.stop();
}

#[test]
fn test_web_server_audio_streaming_byte_ranges() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_stream_test_{}", std::process::id()));
    let _ = fs::create_dir_all(&temp_dir);
    let test_file = temp_dir.join("test_song.mp3");

    // Create 100 bytes of dummy audio data
    let test_data: Vec<u8> = (0..100).collect();
    fs::write(&test_file, &test_data).unwrap();

    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    {
        let mut s = state.write().unwrap();
        s.filepath = Some(test_file.clone());
    }
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state.clone(), tx).expect("start web server");
    let port = handle.port;

    // 1. Full GET request
    let (status, headers, body) = http_get(port, "/stream/test_song.mp3");
    assert_eq!(status, 200);
    assert!(headers.contains("transferMode.dlna.org: Streaming"));
    assert!(headers.contains("Content-Length: 100"));
    assert_eq!(body, test_data);

    // 2. Partial GET with Range: bytes=10-24 (15 bytes)
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
    let req = format!(
        "GET /stream/test_song.mp3 HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nRange: bytes=10-24\r\nConnection: close\r\n\r\n",
        port
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut resp = Vec::new();
    let _ = stream.read_to_end(&mut resp);
    let (status, headers, body) = parse_http_response(&resp);
    assert_eq!(status, 206);
    assert!(headers.contains("Content-Range: bytes 10-24/100"));
    assert!(headers.contains("Content-Length: 15"));
    assert_eq!(body, &test_data[10..25]);

    // 3. HEAD request
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
    let req = format!(
        "HEAD /stream/test_song.mp3 HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        port
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut resp = Vec::new();
    let _ = stream.read_to_end(&mut resp);
    let (status, headers, body) = parse_http_response(&resp);
    assert_eq!(status, 200);
    assert!(headers.contains("Content-Length: 100"));
    assert!(body.is_empty());

    // 4. Non-existent stream when state.filepath is cleared
    {
        let mut s = state.write().unwrap();
        s.filepath = None;
    }
    let (status, _, _) = http_get(port, "/stream/nonexistent_xyz.mp3");
    assert_eq!(status, 404);

    handle.stop();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_web_server_audio_streaming_seek_pos_query() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join("test_web_seek_stream");
    let _ = fs::create_dir_all(&temp_dir);
    let test_file = temp_dir.join("test_seek.mp3");

    // Create 1000 bytes of dummy audio data
    let test_data: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
    fs::write(&test_file, &test_data).unwrap();

    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    {
        let mut s = state.write().unwrap();
        s.filepath = Some(test_file.clone());
        s.duration_seconds = 100.0;
    }
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state.clone(), tx).expect("start web server");
    let port = handle.port;

    // Request stream with ?pos=50.0 (middle of the file)
    let (status, headers, body) = http_get(port, "/stream/test_seek.mp3?pos=50.0");
    assert_eq!(status, 200);
    assert!(headers.contains("transferMode.dlna.org: Streaming"));
    // Since pos=50.0 out of 100.0s, ratio is 0.50, so body length is ~500 bytes
    assert!(
        body.len() <= 550 && body.len() >= 450,
        "Body length with pos=50.0 should be roughly half (got {} bytes)",
        body.len()
    );

    handle.stop();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_web_server_stream_file_registry() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join("test_web_registry");
    let _ = fs::create_dir_all(&temp_dir);
    let test_file = temp_dir.join("registered_song.mp3");

    let test_data: Vec<u8> = vec![0x42; 256];
    fs::write(&test_file, &test_data).unwrap();

    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    // state.filepath is explicitly None to prove registry resolves the file independently
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    let port = handle.port;

    // Register file in the stream registry
    handle.register_stream_file(&test_file);

    // 1. Exact filename lookup
    let (status, headers, body) = http_get(port, "/stream/registered_song.mp3");
    assert_eq!(status, 200);
    assert!(headers.contains("Content-Length: 256"));
    assert_eq!(body, test_data);

    // 2. Case-insensitive lookup
    let (status_ci, headers_ci, body_ci) = http_get(port, "/stream/REGISTERED_SONG.MP3");
    assert_eq!(status_ci, 200);
    assert!(headers_ci.contains("Content-Length: 256"));
    assert_eq!(body_ci, test_data);

    handle.stop();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_web_server_close_active_streams_terminates_clients() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join("test_web_close_streams");
    let _ = fs::create_dir_all(&temp_dir);
    let test_file = temp_dir.join("large_stream.mp3");

    // 32 MB dummy data: larger than loopback socket buffers (several MB on Linux), so the
    // server cannot have handed the whole file to the kernel before the streams are closed.
    let test_data: Vec<u8> = vec![0xAA; 32 * 1024 * 1024];
    fs::write(&test_file, &test_data).unwrap();

    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    let port = handle.port;

    handle.register_stream_file(&test_file);

    // Connect client and start reading partial stream
    let mut client = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
    client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let req = format!(
        "GET /stream/large_stream.mp3 HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        port
    );
    client.write_all(req.as_bytes()).unwrap();

    // Read headers and some bytes
    let mut initial_buf = [0u8; 1024];
    let n = client.read(&mut initial_buf).unwrap();
    assert!(n > 0);

    // Now abruptly close all active streaming sockets from the server side
    handle.close_active_streams();

    // Attempting to read more should immediately return EOF or error
    let mut read_after_close = Vec::new();
    let _ = client.read_to_end(&mut read_after_close);
    // Stream should have terminated well short of the full 2MB
    assert!(
        read_after_close.len() < test_data.len(),
        "Active stream should be aborted, got {} bytes",
        read_after_close.len()
    );

    handle.stop();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_web_server_browser_api() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_test_{}", std::process::id()));
    let _ = fs::create_dir_all(&temp_dir);
    // The browser lists canonical paths (macOS: /var -> /private/var).
    let temp_dir = fs::canonicalize(&temp_dir).unwrap();

    // Create subfolders
    fs::create_dir_all(temp_dir.join("Rock")).unwrap();
    fs::create_dir_all(temp_dir.join("Ambient")).unwrap();
    // Create audio files
    fs::write(temp_dir.join("track1.mp3"), b"fake_mp3_data").unwrap();
    fs::write(temp_dir.join("track2.flac"), b"fake_flac_data").unwrap();
    // Non-audio file should be excluded
    fs::write(temp_dir.join("notes.txt"), b"some text").unwrap();

    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    // The web browser only reaches folders inside the allowed roots.
    state.write().unwrap().allowed_roots = vec![temp_dir.parent().unwrap().to_path_buf()];
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state.clone(), tx).expect("start web server");
    let port = handle.port;

    let encoded_path = quickstream::player::upnp::url_encode(&temp_dir.to_string_lossy());
    let req_url = format!("/api/browser?path={}", encoded_path);

    let (status, headers, body) = http_get(port, &req_url);
    assert_eq!(status, 200);
    assert!(headers.contains("application/json"));

    let resp: WebBrowserResponse = serde_json::from_slice(&body).expect("parse browser response");
    assert_eq!(resp.current_path, temp_dir.to_string_lossy());
    assert!(resp.parent_path.is_some());
    assert_eq!(resp.home_dir, None);

    let entry_names: Vec<String> = resp.entries.iter().map(|e| e.name.clone()).collect();
    // Folders first (alphabetical)
    assert_eq!(entry_names[0], "Ambient");
    assert!(resp.entries[0].is_dir);
    assert_eq!(entry_names[1], "Rock");
    assert!(resp.entries[1].is_dir);

    // Audio files next (alphabetical)
    assert!(entry_names.contains(&"track1.mp3".to_string()));
    assert!(entry_names.contains(&"track2.flac".to_string()));
    // Non-audio file filtered out
    assert!(!entry_names.contains(&"notes.txt".to_string()));

    // Verify that setting state.home_dir makes /api/browser open home_dir by default
    {
        let mut s = state.write().unwrap();
        s.home_dir = Some(temp_dir.join("Rock").to_string_lossy().to_string());
    }
    let (status_home, _, body_home) = http_get(port, "/api/browser");
    assert_eq!(status_home, 200);
    let resp_home: WebBrowserResponse = serde_json::from_slice(&body_home).expect("parse browser home response");
    assert_eq!(resp_home.current_path, temp_dir.join("Rock").to_string_lossy());
    assert_eq!(
        resp_home.home_dir,
        Some(temp_dir.join("Rock").to_string_lossy().to_string())
    );

    // Verify that setting state.current_browser_dir makes /api/browser open the terminal browsed dir by default
    {
        let mut s = state.write().unwrap();
        s.current_browser_dir = temp_dir.join("Ambient").to_string_lossy().to_string();
    }
    let (status_term, _, body_term) = http_get(port, "/api/browser");
    assert_eq!(status_term, 200);
    let resp_term: WebBrowserResponse = serde_json::from_slice(&body_term).expect("parse browser terminal response");
    assert_eq!(resp_term.current_path, temp_dir.join("Ambient").to_string_lossy());
    assert_eq!(
        resp_term.terminal_dir,
        Some(temp_dir.join("Ambient").to_string_lossy().to_string())
    );

    handle.stop();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_web_server_extended_action_commands() {
    common::isolate();
    // Path commands only reach files inside the allowed roots and carry the resolved path.
    let music = std::env::temp_dir().join(format!("qs_web_actions_{}", std::process::id()));
    fs::create_dir_all(music.join("album")).unwrap();
    fs::write(music.join("song.flac"), b"fake_flac").unwrap();
    let music = fs::canonicalize(&music).unwrap();
    let json_path = |p: &std::path::Path| serde_json::to_string(&p.to_string_lossy()).unwrap();
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    state.write().unwrap().allowed_roots = vec![music.clone()];
    let (tx, rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    let port = handle.port;

    // 1. Play index
    let (status, _, _) = http_post(port, "/api/action", r#"{"action":"play_index","index":4}"#);
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::PlayIndex(4)
    );

    // 2. Remove index
    let (status, _, _) = http_post(port, "/api/action", r#"{"action":"remove_index","index":2}"#);
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::RemoveIndex(2)
    );

    // 3. Clear queue
    let (status, _, _) = http_post(port, "/api/action", r#"{"action":"clear_queue"}"#);
    assert_eq!(status, 200);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), WebCommand::ClearQueue);

    // 4. Add file
    let body = format!(
        r#"{{"action":"add_file","path":{}}}"#,
        json_path(&music.join("song.flac"))
    );
    let (status, _, _) = http_post(port, "/api/action", &body);
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::AddFile(music.join("song.flac"))
    );

    // 5. Add folder
    let body = format!(
        r#"{{"action":"add_folder","path":{}}}"#,
        json_path(&music.join("album"))
    );
    let (status, _, _) = http_post(port, "/api/action", &body);
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::AddFolder(music.join("album"))
    );

    // 6. Select device
    let (status, _, _) = http_post(port, "/api/action", r#"{"action":"select_device","id":1}"#);
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::SelectDevice(1)
    );

    // 7. Rescan devices
    let (status, _, _) = http_post(port, "/api/action", r#"{"action":"rescan_devices"}"#);
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::RescanDevices
    );

    // 8. Set queue mode
    let (status, _, _) = http_post(
        port,
        "/api/action",
        r#"{"action":"set_queue_mode","mode":"repeat_one"}"#,
    );
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::SetQueueMode("repeat_one".to_string())
    );

    // 9. Set home with directory path
    let body = format!(r#"{{"action":"set_home","path":{}}}"#, json_path(&music.join("album")));
    let (status, _, _) = http_post(port, "/api/action", &body);
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::SetHome(Some(music.join("album").to_string_lossy().to_string()))
    );

    // 10. Clear home (path: null)
    let (status, _, _) = http_post(port, "/api/action", r#"{"action":"set_home"}"#);
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::SetHome(None)
    );

    handle.stop();
    let _ = fs::remove_dir_all(&music);
}

#[test]
fn test_web_server_status_api_with_queue_and_devices() {
    common::isolate();
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state.clone(), tx).expect("start web server");
    let port = handle.port;

    {
        let mut s = state.write().unwrap();
        s.queue_mode = "repeat_one".to_string();
        s.is_scanning = true;
        s.queue = vec![
            WebQueueItem {
                index: 0,
                title: "Track 1".to_string(),
                artist: "Artist 1".to_string(),
                album: "Album 1".to_string(),
                duration_seconds: 180.0,
                filepath: "/music/t1.mp3".to_string(),
                is_current: true,
            },
            WebQueueItem {
                index: 1,
                title: "Track 2".to_string(),
                artist: "Artist 2".to_string(),
                album: "Album 2".to_string(),
                duration_seconds: 240.0,
                filepath: "/music/t2.mp3".to_string(),
                is_current: false,
            },
        ];
        s.devices = vec![
            WebDeviceItem {
                id: 0,
                name: "Local Audio".to_string(),
                host_port: "Local Device".to_string(),
                is_active: false,
            },
            WebDeviceItem {
                id: 1,
                name: "Living Room (Sonos)".to_string(),
                host_port: "192.168.1.100:1400".to_string(),
                is_active: true,
            },
        ];
    }

    let (status, headers, body) = http_get(port, "/api/status");
    assert_eq!(status, 200);
    assert!(headers.contains("application/json"));

    let returned_state: NowPlayingState = serde_json::from_slice(&body).expect("parse status json");
    assert_eq!(returned_state.queue_mode, "repeat_one");
    assert!(returned_state.is_scanning);
    assert_eq!(returned_state.queue.len(), 2);
    assert_eq!(returned_state.queue[0].title, "Track 1");
    assert!(returned_state.queue[0].is_current);
    assert_eq!(returned_state.devices.len(), 2);
    assert_eq!(returned_state.devices[1].name, "Living Room (Sonos)");
    assert!(returned_state.devices[1].is_active);

    handle.stop();
}

#[test]
fn test_web_server_defaults_to_loopback_bind() {
    common::isolate();
    use quickstream::web::WebBind;
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    assert_eq!(handle.bind, WebBind::Loopback);

    // Not reachable on the machine's network address.
    let lan_ip = quickstream::discovery::get_local_ip_for_target("8.8.8.8", 80);
    if lan_ip != "127.0.0.1" {
        let addr: std::net::SocketAddr = format!("{}:{}", lan_ip, handle.port).parse().unwrap();
        assert!(TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_err());
    }
    handle.stop();
}

#[test]
#[ignore = "binds all network interfaces (macOS firewall prompt); run with --include-ignored"]
fn test_web_server_stops_when_another_listener_shadows_its_loopback_port() {
    common::isolate();
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start_with_bind(quickstream::web::WebBind::Lan, 0, state, tx).expect("start web server");
    // Another server in the process (or another app) holds 127.0.0.1 on the same port, so a
    // wake-up connection to 127.0.0.1:port reaches it instead of this server.
    // macOS allows this bind next to the server's 0.0.0.0 bind; Linux refuses it, so the
    // situation the test guards against cannot happen there.
    let shadow = match std::net::TcpListener::bind(("127.0.0.1", handle.port)) {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("SKIPPED: this OS does not allow a 127.0.0.1 bind beside 0.0.0.0");
            handle.stop();
            return;
        }
        Err(e) => panic!("shadow listener: {}", e),
    };

    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        handle.stop();
        let _ = done_tx.send(());
    });
    assert!(
        done_rx.recv_timeout(Duration::from_secs(2)).is_ok(),
        "WebServerHandle::stop must not depend on a wake-up connection reaching the listener"
    );
    drop(shadow);
}

#[test]
fn test_web_server_stream_rejects_unregistered_absolute_path() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_stream_secret_{}", std::process::id()));
    let _ = fs::create_dir_all(&temp_dir);
    let secret = temp_dir.join("id_rsa");
    fs::write(&secret, b"PRIVATE KEY").unwrap();

    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    let port = handle.port;

    let encoded = secret.to_string_lossy().replace('/', "%2F");
    let (status, _, body) = http_get(port, &format!("/stream/{}", encoded));
    assert_eq!(status, 404);
    assert!(!String::from_utf8_lossy(&body).contains("PRIVATE KEY"));

    handle.stop();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_web_server_stream_rejects_cwd_relative_path() {
    common::isolate();
    // cargo runs integration tests with the crate root as cwd, so Cargo.toml exists relative to it.
    assert!(PathBuf::from("Cargo.toml").exists());

    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    let port = handle.port;

    let (status, _, _) = http_get(port, "/stream/Cargo.toml");
    assert_eq!(status, 404);
    let (status, _, _) = http_get(port, "/stream/src%2Fmain.rs");
    assert_eq!(status, 404);

    handle.stop();
}

#[test]
fn test_web_server_stream_unknown_name_does_not_fall_back_to_current_track() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_stream_nofallback_{}", std::process::id()));
    let _ = fs::create_dir_all(&temp_dir);
    let current = temp_dir.join("now_playing.mp3");
    fs::write(&current, vec![0x11; 64]).unwrap();

    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    state.write().unwrap().filepath = Some(current.clone());
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    let port = handle.port;

    let (status, _, _) = http_get(port, "/stream/some_other_name.mp3");
    assert_eq!(status, 404);
    let (status, _, _) = http_get(port, "/stream/now_playing.mp3");
    assert_eq!(status, 200);

    handle.stop();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_web_server_stream_prefers_now_playing_over_same_name_registration() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_stream_samename_{}", std::process::id()));
    let album_a = temp_dir.join("album_a");
    let album_b = temp_dir.join("album_b");
    fs::create_dir_all(&album_a).unwrap();
    fs::create_dir_all(&album_b).unwrap();
    let playing = album_a.join("01 Intro.mp3");
    let other = album_b.join("01 Intro.mp3");
    fs::write(&playing, vec![0xAA; 32]).unwrap();
    fs::write(&other, vec![0xBB; 64]).unwrap();

    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    state.write().unwrap().filepath = Some(playing.clone());
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    handle.register_stream_file(&playing);
    // A later registration (e.g. autopilot queueing another album) reuses the same file name.
    handle.register_stream_file(&other);

    let (status, _, body) = http_get(handle.port, "/stream/01%20Intro.mp3");
    assert_eq!(status, 200);
    assert_eq!(
        body,
        vec![0xAA; 32],
        "must serve the now-playing file, not the other album's"
    );

    handle.stop();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_web_server_stream_serves_registered_file_with_encoded_name() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_stream_encoded_{}", std::process::id()));
    fs::create_dir_all(&temp_dir).unwrap();
    let file = temp_dir.join("Café + Friends (Live).flac");
    fs::write(&file, vec![0x42; 16]).unwrap();

    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    handle.register_stream_file(&file);

    let encoded = quickstream::player::upnp::url_encode("Café + Friends (Live).flac");
    let (status, _, body) = http_get(handle.port, &format!("/stream/{}", encoded));
    assert_eq!(status, 200);
    assert_eq!(body, vec![0x42; 16]);

    handle.stop();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_web_server_stream_rejects_separator_and_dot_names() {
    common::isolate();
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    for path in [
        "/stream/",
        "/stream/..",
        "/stream/%2E%2E",
        "/stream/..%5CCargo.toml",
        "/stream/%5Cetc%5Cpasswd",
    ] {
        let (status, _, _) = http_get(handle.port, path);
        assert_eq!(status, 404, "{} must be rejected", path);
    }
    handle.stop();
}

/// Linux allows file names that are not valid UTF-8; DLNA URLs carry them lossily encoded.
#[cfg(target_os = "linux")]
#[test]
fn test_web_server_stream_serves_non_utf8_file_name() {
    common::isolate();
    use std::os::unix::ffi::OsStrExt;
    let temp_dir = std::env::temp_dir().join(format!("qs_stream_nonutf8_{}", std::process::id()));
    fs::create_dir_all(&temp_dir).unwrap();
    let name = std::ffi::OsStr::from_bytes(b"caf\xe9.mp3");
    let file = temp_dir.join(name);
    fs::write(&file, vec![0x33; 8]).unwrap();

    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    handle.register_stream_file(&file);

    let encoded = quickstream::player::upnp::url_encode(&name.to_string_lossy());
    let (status, _, body) = http_get(handle.port, &format!("/stream/{}", encoded));
    assert_eq!(status, 200);
    assert_eq!(body, vec![0x33; 8]);

    handle.stop();
    let _ = fs::remove_dir_all(&temp_dir);
}

// ---------------------------------------------------------------------------
// Request limits, connection caps and HTTP Range handling (issue #22)
// ---------------------------------------------------------------------------

/// Send raw bytes, then read the whole response.
fn http_raw(port: u16, request: &[u8]) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let _ = stream.write_all(request);
    let mut resp = Vec::new();
    let _ = stream.read_to_end(&mut resp);
    parse_http_response(&resp)
}

fn header_value(headers: &str, name: &str) -> Option<String> {
    headers.lines().skip(1).find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
    })
}

fn start_limited(
    limits: ServerLimits,
) -> (
    WebServerHandle,
    Arc<RwLock<NowPlayingState>>,
    mpsc::Receiver<WebCommand>,
) {
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, rx) = mpsc::channel();
    let handle = WebServer::start_with_options(
        WebBind::Loopback,
        0,
        state.clone(),
        tx,
        Arc::new(StreamServerState::new()),
        limits,
    )
    .expect("start web server");
    (handle, state, rx)
}

/// Registers a 100-byte file (bytes 0..100) and returns the server and its contents.
fn start_range_fixture(tag: &str) -> (WebServerHandle, PathBuf, Vec<u8>) {
    let dir = std::env::temp_dir().join(format!("qs_range_{}_{}", tag, std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("range_song.mp3");
    let data: Vec<u8> = (0..100).collect();
    fs::write(&file, &data).unwrap();
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    handle.register_stream_file(&file);
    (handle, dir, data)
}

fn range_get(port: u16, range: &str) -> (u16, String, Vec<u8>) {
    let req = format!(
        "GET /stream/range_song.mp3 HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nRange: {}\r\n\r\n",
        port, range
    );
    http_raw(port, req.as_bytes())
}

#[test]
fn test_parse_byte_range_cases() {
    common::isolate();
    use ByteRange::{Full, Partial, Unsatisfiable};
    let p = |start, end| Partial { start, end };

    assert_eq!(parse_byte_range("bytes=10-24", 100), p(10, 24));
    assert_eq!(parse_byte_range("bytes=90-", 100), p(90, 99));
    assert_eq!(parse_byte_range("bytes=50-1000", 100), p(50, 99));
    assert_eq!(parse_byte_range("Bytes=0-0", 100), p(0, 0));
    assert_eq!(parse_byte_range("bytes=0-99999999999999999999999", 100), p(0, 99));

    // Suffix ranges: the last N bytes.
    assert_eq!(parse_byte_range("bytes=-10", 100), p(90, 99));
    assert_eq!(parse_byte_range("bytes=-500", 100), p(0, 99));
    assert_eq!(parse_byte_range("bytes=-0", 100), Unsatisfiable);

    // Unsatisfiable ranges.
    assert_eq!(parse_byte_range("bytes=100-", 100), Unsatisfiable);
    assert_eq!(parse_byte_range("bytes=99999999999999999999999-", 100), Unsatisfiable);
    assert_eq!(parse_byte_range("bytes=5-2", 100), Unsatisfiable);
    assert_eq!(parse_byte_range("bytes=0-", 0), Unsatisfiable);
    assert_eq!(parse_byte_range("bytes=-5", 0), Unsatisfiable);

    // Multiple ranges: the first satisfiable one is served.
    assert_eq!(parse_byte_range("bytes=10-19, 50-59", 100), p(10, 19));
    assert_eq!(parse_byte_range("bytes=200-300,5-9", 100), p(5, 9));
    assert_eq!(parse_byte_range("bytes=200-300,400-", 100), Unsatisfiable);

    // Invalid syntax or unknown units: the Range header is ignored.
    for bad in [
        "bytes=abc-",
        "bytes=",
        "bytes=-",
        "bytes=1-2-3",
        "bytes=+1-2",
        "bytes=0-5,x",
        "items=0-5",
        "bytes 0-5",
    ] {
        assert_eq!(parse_byte_range(bad, 100), Full, "{:?} should be ignored", bad);
    }
}

#[test]
fn test_web_server_stream_suffix_range() {
    common::isolate();
    let (handle, dir, data) = start_range_fixture("suffix");

    let (status, headers, body) = range_get(handle.port, "bytes=-10");
    assert_eq!(status, 206, "headers: {}", headers);
    assert_eq!(
        header_value(&headers, "Content-Range").as_deref(),
        Some("bytes 90-99/100")
    );
    assert_eq!(header_value(&headers, "Content-Length").as_deref(), Some("10"));
    assert_eq!(body, &data[90..]);

    // A suffix longer than the file selects the whole file.
    let (status, headers, body) = range_get(handle.port, "bytes=-500");
    assert_eq!(status, 206, "headers: {}", headers);
    assert_eq!(
        header_value(&headers, "Content-Range").as_deref(),
        Some("bytes 0-99/100")
    );
    assert_eq!(body, data);

    handle.stop();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_web_server_stream_multi_range_serves_first_range() {
    common::isolate();
    let (handle, dir, data) = start_range_fixture("multi");

    let (status, headers, body) = range_get(handle.port, "bytes=10-19,50-59");
    assert_eq!(status, 206, "headers: {}", headers);
    assert_eq!(
        header_value(&headers, "Content-Range").as_deref(),
        Some("bytes 10-19/100")
    );
    assert_eq!(header_value(&headers, "Content-Length").as_deref(), Some("10"));
    assert_eq!(body, &data[10..20]);

    handle.stop();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_web_server_stream_invalid_range_serves_full_body() {
    common::isolate();
    let (handle, dir, data) = start_range_fixture("invalid");

    for bad in ["bytes=abc-", "bytes=1-2-3", "items=0-5"] {
        let (status, headers, body) = range_get(handle.port, bad);
        assert_eq!(status, 200, "{}: headers: {}", bad, headers);
        assert_eq!(header_value(&headers, "Content-Range"), None);
        assert_eq!(header_value(&headers, "Content-Length").as_deref(), Some("100"));
        assert_eq!(body, data);
    }

    handle.stop();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_web_server_stream_416_headers() {
    common::isolate();
    let (handle, dir, _data) = start_range_fixture("416");

    for unsatisfiable in ["bytes=100-", "bytes=5-2", "bytes=-0"] {
        let (status, headers, body) = range_get(handle.port, unsatisfiable);
        assert_eq!(status, 416, "{}: headers: {}", unsatisfiable, headers);
        assert_eq!(header_value(&headers, "Content-Range").as_deref(), Some("bytes */100"));
        assert_eq!(header_value(&headers, "Content-Length").as_deref(), Some("0"));
        assert_eq!(header_value(&headers, "Connection").as_deref(), Some("close"));
        assert!(body.is_empty());
    }

    handle.stop();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_web_server_stream_responses_advertise_connection_close() {
    common::isolate();
    let (handle, dir, _data) = start_range_fixture("close");

    let (status, headers, _) = http_get(handle.port, "/stream/range_song.mp3");
    assert_eq!(status, 200);
    assert_eq!(header_value(&headers, "Connection").as_deref(), Some("close"));

    let (status, headers, _) = range_get(handle.port, "bytes=0-9");
    assert_eq!(status, 206);
    assert_eq!(header_value(&headers, "Connection").as_deref(), Some("close"));

    handle.stop();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_web_server_rejects_oversized_header_line() {
    common::isolate();
    let (handle, _state, _rx) = start_limited(ServerLimits::default());

    let req = format!(
        "GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Big: {}\r\n\r\n",
        "a".repeat(9000)
    );
    let (status, headers, _) = http_raw(handle.port, req.as_bytes());
    assert_eq!(status, 431, "headers: {}", headers);
    assert_eq!(header_value(&headers, "Connection").as_deref(), Some("close"));

    // The server keeps working afterwards.
    let (status, _, _) = http_get(handle.port, "/api/status");
    assert_eq!(status, 200);

    handle.stop();
}

#[test]
fn test_web_server_rejects_oversized_request_line() {
    common::isolate();
    let (handle, _state, _rx) = start_limited(ServerLimits::default());

    let req = format!(
        "GET /api/status?pad={} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        "a".repeat(9000)
    );
    let (status, headers, _) = http_raw(handle.port, req.as_bytes());
    assert_eq!(status, 414, "headers: {}", headers);

    handle.stop();
}

#[test]
fn test_web_server_rejects_too_many_headers() {
    common::isolate();
    let (handle, _state, _rx) = start_limited(ServerLimits::default());

    let port = handle.port;
    let build = |count: usize| {
        let mut req = format!("GET /api/status HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n", port);
        for i in 1..count {
            req.push_str(&format!("X-Header-{}: v\r\n", i));
        }
        req.push_str("\r\n");
        req
    };

    // 100 header fields (Host + 99) are accepted.
    let (status, _, _) = http_raw(handle.port, build(100).as_bytes());
    assert_eq!(status, 200);

    // 101 are rejected.
    let (status, headers, _) = http_raw(handle.port, build(101).as_bytes());
    assert_eq!(status, 431, "headers: {}", headers);

    handle.stop();
}

#[test]
fn test_web_server_rejects_oversized_content_length_before_reading_body() {
    common::isolate();
    let (handle, _state, rx) = start_limited(ServerLimits::default());

    for declared in ["18446744073709551615", "99999999999999999999999", "65537"] {
        // Headers only: a server that tried to read (or allocate) the declared body would hang or
        // fail instead of answering 413 straight away.
        let req = format!(
            "POST /api/action HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            declared
        );
        let started = Instant::now();
        let (status, headers, _) = http_raw(handle.port, req.as_bytes());
        assert_eq!(status, 413, "Content-Length {}: headers: {}", declared, headers);
        assert!(started.elapsed() < Duration::from_secs(3));
    }
    assert!(rx.try_recv().is_err(), "no command should be dispatched");

    // A body at the limit is still accepted.
    let body = format!("{{\"action\":\"next\",\"pad\":\"{}\"}}", "a".repeat(64 * 1024 - 40));
    assert!(body.len() <= 64 * 1024);
    let (status, _, _) = http_post(handle.port, "/api/action", &body);
    assert_eq!(status, 200);
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), WebCommand::Next);

    // The server is still alive.
    let (status, _, _) = http_get(handle.port, "/api/status");
    assert_eq!(status, 200);

    handle.stop();
}

#[test]
fn test_web_server_request_head_deadline_is_total_not_per_read() {
    common::isolate();
    let limits = ServerLimits {
        request_timeout: Duration::from_secs(1),
        ..ServerLimits::default()
    };
    let (handle, _state, _rx) = start_limited(limits);

    let mut stream = TcpStream::connect(("127.0.0.1", handle.port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let started = Instant::now();
    // Drip one header line every 250 ms for 5 s: each read succeeds well within any per-read
    // timeout, but the head is never finished.
    let dripper = std::thread::spawn(move || {
        if writer.write_all(b"GET /api/status HTTP/1.1\r\n").is_err() {
            return;
        }
        for _ in 0..20 {
            if writer.write_all(b"X-Drip: 1\r\n").is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });

    let mut resp = Vec::new();
    let _ = stream.read_to_end(&mut resp);
    let elapsed = started.elapsed();
    let (status, headers, _) = parse_http_response(&resp);
    assert_eq!(status, 408, "headers: {}", headers);
    assert!(elapsed < Duration::from_secs(4), "took {:?}", elapsed);

    let _ = dripper.join();
    handle.stop();
}

#[test]
fn test_web_server_caps_concurrent_connections() {
    common::isolate();
    let limits = ServerLimits {
        max_connections: 2,
        ..ServerLimits::default()
    };
    let (handle, _state, _rx) = start_limited(limits);
    let port = handle.port;

    // Two idle connections take both slots (accepted in order, before the third).
    let idle1 = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let idle2 = TcpStream::connect(("127.0.0.1", port)).unwrap();

    let mut third = TcpStream::connect(("127.0.0.1", port)).unwrap();
    third.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut resp = Vec::new();
    let _ = third.read_to_end(&mut resp);
    let (status, headers, _) = parse_http_response(&resp);
    assert_eq!(status, 503, "headers: {}", headers);
    assert_eq!(header_value(&headers, "Connection").as_deref(), Some("close"));

    // Closing the idle connections frees their slots.
    drop(idle1);
    drop(idle2);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (status, _, _) = http_get(port, "/api/status");
        if status == 200 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "slots were not released (last status {})",
            status
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    handle.stop();
}

/// Open the armed continuous stream path and return the status code with the reader positioned
/// after the response head.
fn open_continuous(port: u16, path: &str) -> (u16, BufReader<TcpStream>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream
        .write_all(format!("GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n", path, port).as_bytes())
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    let _ = reader.read_line(&mut status_line);
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) if line.trim().is_empty() => break,
            Ok(_) => {}
        }
    }
    (status, reader)
}

#[test]
fn test_web_server_streams_use_separate_pool_and_release_on_disconnect() {
    common::isolate();
    let limits = ServerLimits {
        max_connections: 1,
        max_streams: 1,
        ..ServerLimits::default()
    };
    // Empty queue: the continuous stream sends silence until the client goes away.
    let (handle, _state, _rx) = start_limited(limits);
    let port = handle.port;
    let session = handle.stream_state().arm_continuous_session();

    let (status, mut first) = open_continuous(port, &session);
    assert_eq!(status, 200);
    let mut wav_hdr = [0u8; 44];
    first.read_exact(&mut wav_hdr).unwrap();
    assert_eq!(&wav_hdr[0..4], b"RIFF");

    // The open stream does not occupy the request pool.
    let (status, _, _) = http_get(port, "/api/status");
    assert_eq!(status, 200);

    // The stream pool is full.
    let (status, _) = open_continuous(port, &session);
    assert_eq!(status, 503);

    // Once the client disconnects, the silence loop must notice the dead socket and exit,
    // freeing the stream slot.
    drop(first);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (status, conn) = open_continuous(port, &session);
        drop(conn);
        if status == 200 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stream slot was not released (last status {})",
            status
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    handle.stop();
}

// ---------------------------------------------------------------------------------------------
// Up Next (issue #104): /api/status names what plays next as tab [1]'s UP NEXT does
// ---------------------------------------------------------------------------------------------

/// An app with its web server on (loopback, any port) and one tagged track `T<i>` queued per
/// length (0 for an unknown one).
fn up_next_app_with(lengths: &[f64]) -> quickstream::ui::AppUI {
    let mut settings_mgr = quickstream::settings::SettingsManager::new();
    settings_mgr.settings.web_server_enabled = true;
    settings_mgr.settings.web_server_lan = false;
    settings_mgr.settings.web_server_port = 0;
    let mut app = quickstream::ui::AppUI::new(Some(std::env::temp_dir()), settings_mgr);
    assert!(app.web_server.is_some(), "web server did not start");
    for (i, &length) in lengths.iter().enumerate() {
        app.queue.add_track(quickstream::metadata::AudioMetadata {
            filepath: PathBuf::from(format!("/music/t{i}.flac")),
            title: format!("T{i}"),
            artist: "Mira Lenz".to_string(),
            duration_seconds: length,
            ..Default::default()
        });
    }
    app
}

/// Four tracks of 200, 201, 202 and 203 seconds.
fn up_next_app() -> quickstream::ui::AppUI {
    up_next_app_with(&[200.0, 201.0, 202.0, 203.0])
}

/// Publish the app's state and read `/api/status` back.
fn up_next_status(app: &mut quickstream::ui::AppUI) -> serde_json::Value {
    app.update_web_state();
    let (status, _, body) = http_get(app.web_port().expect("web port"), "/api/status");
    assert_eq!(status, 200);
    serde_json::from_slice(&body).expect("status JSON")
}

/// What `/api/status` should say plays next, written out per case.
struct WantUpNext<'a> {
    /// The queue indices `up_next` lists (each track `T<i>`, its artist and length).
    listed: &'a [usize],
    remaining: usize,
    end: &'a str,
    /// `next_track_title`: the first listed track, else the current one when it plays again.
    next: Option<&'a str>,
    /// The position in `up_next` where the queue starts over at its first track.
    wraps_before: Option<usize>,
    /// Every remaining track is listed, so the end note follows them.
    shows_end: bool,
    /// The remaining tracks' total length, when every one is known.
    total: Option<f64>,
}

fn assert_up_next(app: &mut quickstream::ui::AppUI, what: &str, want: WantUpNext) {
    let status = up_next_status(app);
    let lengths: Vec<f64> = app.queue.tracks().iter().map(|t| t.duration_seconds).collect();
    let items: Vec<serde_json::Value> = want
        .listed
        .iter()
        .map(|&i| {
            serde_json::json!({
                "index": i,
                "title": format!("T{i}"),
                "artist": "Mira Lenz",
                "duration_seconds": lengths[i],
            })
        })
        .collect();
    assert_eq!(status["up_next"], serde_json::Value::Array(items), "{what}: {status}");
    assert_eq!(status["up_next_remaining"], want.remaining, "{what}: {status}");
    assert_eq!(status["up_next_end"], want.end, "{what}: {status}");
    assert_eq!(status["next_track_title"].as_str(), want.next, "{what}: {status}");
    assert_eq!(
        status["up_next_wraps_before"].as_u64(),
        want.wraps_before.map(|p| p as u64),
        "{what}: {status}"
    );
    assert_eq!(status["up_next_shows_end"], want.shows_end, "{what}: {status}");
    assert_eq!(status["up_next_total_seconds"].as_f64(), want.total, "{what}: {status}");
}

fn stop_up_next_app(mut app: quickstream::ui::AppUI) {
    if let Some(handle) = app.web_server.take() {
        handle.stop();
    }
}

#[test]
fn test_status_up_next_in_stop_mode_ends_where_playback_stops() {
    common::isolate();
    let mut app = up_next_app();
    app.settings_mgr.settings.queue_mode = "stop".to_string();
    app.queue.set_current_index(1);
    assert_up_next(
        &mut app,
        "stop, track 1",
        WantUpNext {
            listed: &[2, 3],
            remaining: 2,
            end: "stops",
            next: Some("T2"),
            wraps_before: None,
            shows_end: true,
            total: Some(405.0),
        },
    );
    app.queue.set_current_index(3);
    assert_up_next(
        &mut app,
        "stop, last track",
        WantUpNext {
            listed: &[],
            remaining: 0,
            end: "stops",
            next: None,
            wraps_before: None,
            shows_end: true,
            total: None,
        },
    );
    stop_up_next_app(app);
}

/// More tracks remain than `up_next` lists: the end isn't shown yet, and the total counts every
/// remaining track, not only the listed ones; one unknown length leaves the total out.
#[test]
fn test_status_up_next_lists_the_first_tracks_and_totals_them_all() {
    common::isolate();
    let mut app = up_next_app_with(&[100.0, 10.0, 20.0, 30.0, 40.0, 50.0]);
    app.settings_mgr.settings.queue_mode = "stop".to_string();
    app.queue.set_current_index(0);
    assert_up_next(
        &mut app,
        "stop, track 0 of 6",
        WantUpNext {
            listed: &[1, 2, 3],
            remaining: 5,
            end: "stops",
            next: Some("T1"),
            wraps_before: None,
            shows_end: false,
            total: Some(150.0),
        },
    );
    stop_up_next_app(app);

    let mut app = up_next_app_with(&[100.0, 10.0, 20.0, 30.0, 40.0, 0.0]);
    app.settings_mgr.settings.queue_mode = "stop".to_string();
    app.queue.set_current_index(0);
    assert_up_next(
        &mut app,
        "stop, an unknown length after the listed tracks",
        WantUpNext {
            listed: &[1, 2, 3],
            remaining: 5,
            end: "stops",
            next: Some("T1"),
            wraps_before: None,
            shows_end: false,
            total: None,
        },
    );
    stop_up_next_app(app);
}

#[test]
fn test_status_up_next_in_loop_mode_wraps_to_the_first_track() {
    common::isolate();
    let mut app = up_next_app();
    app.settings_mgr.settings.queue_mode = "loop".to_string();
    // Three tracks follow track 2: track 3, then past the end tracks 0 and 1.
    app.queue.set_current_index(2);
    assert_up_next(
        &mut app,
        "loop, track 2",
        WantUpNext {
            listed: &[3, 0, 1],
            remaining: 3,
            end: "loops",
            next: Some("T3"),
            wraps_before: Some(1),
            shows_end: true,
            total: Some(604.0),
        },
    );
    // On the last track the queue starts over at once.
    app.queue.set_current_index(3);
    assert_up_next(
        &mut app,
        "loop, last track",
        WantUpNext {
            listed: &[0, 1, 2],
            remaining: 3,
            end: "loops",
            next: Some("T0"),
            wraps_before: Some(0),
            shows_end: true,
            total: Some(603.0),
        },
    );
    // A single looping track plays again.
    while app.queue.len() > 1 {
        app.queue.remove(1);
    }
    app.queue.set_current_index(0);
    assert_up_next(
        &mut app,
        "loop, single track",
        WantUpNext {
            listed: &[],
            remaining: 0,
            end: "loops",
            next: Some("T0"),
            wraps_before: None,
            shows_end: true,
            total: None,
        },
    );
    stop_up_next_app(app);
}

#[test]
fn test_status_up_next_in_repeat_one_names_the_current_track() {
    common::isolate();
    let mut app = up_next_app();
    app.settings_mgr.settings.queue_mode = "repeat_one".to_string();
    for (current, title) in [(1, "T1"), (3, "T3")] {
        app.queue.set_current_index(current);
        assert_up_next(
            &mut app,
            &format!("repeat one, track {current}"),
            WantUpNext {
                listed: &[],
                remaining: 0,
                end: "repeat_one",
                next: Some(title),
                wraps_before: None,
                shows_end: true,
                total: None,
            },
        );
    }
    stop_up_next_app(app);
}

/// Mark the app's continuous stream as playing track `index`.
fn stream_plays(app: &quickstream::ui::AppUI, index: usize) {
    let continuous = &app.web_server.as_ref().unwrap().stream_state().continuous;
    continuous.is_active.store(true, std::sync::atomic::Ordering::SeqCst);
    *continuous.current_track_index.lock().unwrap() = index;
}

/// While the UPnP continuous stream plays, the queue plays on to its end whatever the mode, then
/// reshuffles: the last track names no next track, never the first.
#[test]
fn test_status_up_next_follows_the_continuous_stream() {
    common::isolate();
    let mut app = up_next_app();
    for mode in ["stop", "loop", "repeat_one"] {
        app.settings_mgr.settings.queue_mode = mode.to_string();
        app.queue.set_current_index(1);
        stream_plays(&app, 1);
        assert_up_next(
            &mut app,
            &format!("{mode}, stream on track 1"),
            WantUpNext {
                listed: &[2, 3],
                remaining: 2,
                end: "reshuffles",
                next: Some("T2"),
                wraps_before: None,
                shows_end: true,
                total: Some(405.0),
            },
        );
        app.queue.set_current_index(3);
        stream_plays(&app, 3);
        assert_up_next(
            &mut app,
            &format!("{mode}, stream on the last track"),
            WantUpNext {
                listed: &[],
                remaining: 0,
                end: "reshuffles",
                next: None,
                wraps_before: None,
                shows_end: true,
                total: None,
            },
        );
    }
    stop_up_next_app(app);
}

/// A single track under the continuous stream: the stream plays it again (a one-track queue is
/// not reshuffled), so the end is `loops` and the next track is that one, whatever the mode (#134).
#[test]
fn test_status_up_next_single_track_under_the_continuous_stream_repeats() {
    common::isolate();
    let mut app = up_next_app_with(&[200.0]);
    app.queue.set_current_index(0);
    stream_plays(&app, 0);
    for mode in ["stop", "loop", "repeat_one"] {
        app.settings_mgr.settings.queue_mode = mode.to_string();
        assert_up_next(
            &mut app,
            &format!("{mode}, stream on the only track"),
            WantUpNext {
                listed: &[],
                remaining: 0,
                end: "loops",
                next: Some("T0"),
                wraps_before: None,
                shows_end: true,
                total: None,
            },
        );
    }
    stop_up_next_app(app);
}

/// The stream has started a later track than the UI's queue names (it follows at its next tick):
/// the dashboard shows the stream's track and what follows it, all from one index (#104).
#[test]
fn test_status_up_next_takes_the_stream_index_ahead_of_the_ui() {
    common::isolate();
    let mut app = up_next_app();
    app.settings_mgr.settings.queue_mode = "stop".to_string();
    app.queue.set_current_index(0);
    stream_plays(&app, 2);
    assert_up_next(
        &mut app,
        "stream on track 2, UI on track 0",
        WantUpNext {
            listed: &[3],
            remaining: 1,
            end: "reshuffles",
            next: Some("T3"),
            wraps_before: None,
            shows_end: true,
            total: Some(203.0),
        },
    );
    let status = up_next_status(&mut app);
    assert_eq!(status["queue_index"], 2, "{status}");
    assert_eq!(status["title"], "T2", "{status}");
    let current: Vec<bool> = status["queue"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["is_current"].as_bool().unwrap())
        .collect();
    assert_eq!(current, [false, false, true, false], "{status}");
    stop_up_next_app(app);
}

#[test]
fn test_status_up_next_is_empty_with_nothing_queued() {
    common::isolate();
    let mut app = up_next_app();
    app.queue.clear();
    let status = up_next_status(&mut app);
    assert_eq!(status["up_next"], serde_json::json!([]));
    assert_eq!(status["up_next_end"], serde_json::Value::Null);
    assert_eq!(status["next_track_title"], serde_json::Value::Null);
    assert_eq!(status["up_next_shows_end"], false);
    stop_up_next_app(app);
}

fn web_queue(n: usize) -> Vec<WebQueueItem> {
    (0..n)
        .map(|i| WebQueueItem {
            index: i,
            title: format!("Q{i}"),
            artist: String::new(),
            album: String::new(),
            duration_seconds: 60.0,
            filepath: format!("/music/q{i}.flac"),
            is_current: i == 0,
        })
        .collect()
}

/// Up Next comes from the queue the state publishes, whatever queue was published before it:
/// indices, count, titles and end from one snapshot, never a length taken from another (#104).
#[test]
fn test_publish_up_next_comes_from_the_published_queue() {
    common::isolate();
    let mut state = NowPlayingState {
        queue: web_queue(6),
        queue_index: 1,
        queue_mode: "stop".to_string(),
        ..Default::default()
    };
    state.publish_up_next(false);
    let listed = |s: &NowPlayingState| s.up_next.iter().map(|t| (t.index, t.title.clone())).collect::<Vec<_>>();
    assert_eq!(
        listed(&state),
        [(2, "Q2".to_string()), (3, "Q3".to_string()), (4, "Q4".to_string())]
    );
    assert_eq!(state.up_next_remaining, 4);
    assert!(!state.up_next_shows_end);
    assert_eq!(state.up_next_total_seconds, Some(240.0));

    // The queue shrinks under the same index: everything follows the new one.
    state.queue = web_queue(3);
    state.publish_up_next(false);
    assert_eq!(listed(&state), [(2, "Q2".to_string())]);
    assert_eq!(state.up_next_remaining, 1);
    assert!(state.up_next_shows_end);
    assert_eq!(state.up_next_total_seconds, Some(60.0));

    // An index past the end of the published queue lists nothing and names no next track.
    state.queue_index = 7;
    state.publish_up_next(true);
    assert!(state.up_next.is_empty());
    assert_eq!(state.up_next_remaining, 0);
    assert_eq!(state.up_next_end, None);
    assert_eq!(state.next_track_title, None);
}

/// A row the web dashboard removes moves the queue cursor with its track: one before it leaves
/// the cursor on the same track, the cursor's own row leaves it on the track that took its
/// place, and the last row moves it up to the new last one (PR #137 review).
#[test]
fn test_web_remove_keeps_the_queue_cursor_on_its_track() {
    common::isolate();
    let mut app = up_next_app_with(&[200.0, 201.0, 202.0, 203.0, 204.0]);
    let (cmd_tx, cmd_rx) = mpsc::channel();
    app.web_cmd_rx = Some(cmd_rx);
    app.queue_cursor = 3;
    let cursor_title = |app: &quickstream::ui::AppUI| {
        app.queue
            .tracks()
            .get(app.queue_cursor)
            .map(|t| t.title.clone())
            .unwrap_or_default()
    };
    for (row, want) in [(1, "T3"), (2, "T4"), (2, "T2")] {
        cmd_tx.send(WebCommand::RemoveIndex(row)).unwrap();
        app.handle_web_commands();
        assert_eq!(cursor_title(&app), want, "after removing row {row}");
    }
    assert_eq!(app.queue_cursor, 1);
    stop_up_next_app(app);
}
