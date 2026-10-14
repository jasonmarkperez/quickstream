use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use quickstream::web::{NowPlayingState, WebCommand, WebServer};

fn parse_http_response(raw: &[u8]) -> (u16, String, Vec<u8>) {
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(raw.len());
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
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
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
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
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
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    let port = handle.port;

    // 1. Toggle pause
    let (status, _, _) = http_post(port, "/api/action", r#"{"action":"toggle_pause"}"#);
    assert_eq!(status, 200);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), WebCommand::TogglePause);

    // 2. Volume
    let (status, _, _) = http_post(port, "/api/action", r#"{"action":"volume","value":75.0}"#);
    assert_eq!(status, 200);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), WebCommand::SetVolume(75));

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
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), WebCommand::SeekTo(45.0));

    // 6. Seek Relative
    let (status, _, _) = http_post(port, "/api/action", r#"{"action":"seek_relative","value":-10.0}"#);
    assert_eq!(status, 200);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), WebCommand::SeekRelative(-10.0));

    handle.stop();
}

#[test]
fn test_web_server_audio_streaming_byte_ranges() {
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
    let temp_dir = std::env::temp_dir().join("test_web_close_streams");
    let _ = fs::create_dir_all(&temp_dir);
    let test_file = temp_dir.join("large_stream.mp3");

    // 2 MB dummy data
    let test_data: Vec<u8> = vec![0xAA; 2 * 1024 * 1024];
    fs::write(&test_file, &test_data).unwrap();

    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    let port = handle.port;

    handle.register_stream_file(&test_file);

    // Connect client and start reading partial stream
    let mut client = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
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
