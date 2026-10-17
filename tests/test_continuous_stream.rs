//! The continuous (gapless) WAV stream is only served during a UPnP continuous session, at a
//! per-session unguessable path, to the renderer that first connected.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use quickstream::discovery::DLNADevice;
use quickstream::player::upnp::UPnPPlayer;
use quickstream::player::AudioPlayer;
use quickstream::web::{
    ContinuousAdmission, ContinuousStreamController, NowPlayingState, StreamServerState, WebCommand, WebQueueItem,
    WebServer, WebServerHandle,
};

fn isolate_config() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("qs_continuous_cfg_{}", std::process::id()));
        std::env::set_var("XDG_CONFIG_HOME", dir);
    });
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qs_continuous_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

/// A short silent 16-bit 44.1 kHz stereo WAV.
fn write_wav(path: &Path, millis: u32) {
    let data_len = 44100 * 4 * millis / 1000;
    let mut hdr = Vec::with_capacity(44 + data_len as usize);
    hdr.extend_from_slice(b"RIFF");
    hdr.extend_from_slice(&(36 + data_len).to_le_bytes());
    hdr.extend_from_slice(b"WAVEfmt ");
    hdr.extend_from_slice(&16u32.to_le_bytes());
    hdr.extend_from_slice(&1u16.to_le_bytes());
    hdr.extend_from_slice(&2u16.to_le_bytes());
    hdr.extend_from_slice(&44100u32.to_le_bytes());
    hdr.extend_from_slice(&(44100u32 * 4).to_le_bytes());
    hdr.extend_from_slice(&4u16.to_le_bytes());
    hdr.extend_from_slice(&16u16.to_le_bytes());
    hdr.extend_from_slice(b"data");
    hdr.extend_from_slice(&data_len.to_le_bytes());
    hdr.resize(44 + data_len as usize, 0);
    std::fs::write(path, hdr).unwrap();
}

fn queue_item(index: usize, path: &Path) -> WebQueueItem {
    WebQueueItem {
        index,
        title: format!("Track {}", index),
        artist: "Artist".to_string(),
        album: "Album".to_string(),
        duration_seconds: 0.1,
        filepath: path.to_string_lossy().to_string(),
        is_current: index == 0,
    }
}

fn start(state: Arc<RwLock<NowPlayingState>>) -> (WebServerHandle, mpsc::Receiver<WebCommand>) {
    isolate_config();
    let (tx, rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    (handle, rx)
}

/// Send a GET and return the status with the reader positioned after the response head.
fn open(port: u16, path: &str) -> (u16, BufReader<TcpStream>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream
        .write_all(format!("GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n", path, port).as_bytes())
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    let _ = reader.read_line(&mut status_line);
    let status = status_line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
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

fn is_session_path(path: &str) -> bool {
    path.strip_prefix("/stream/continuous-")
        .and_then(|rest| rest.strip_suffix(".wav"))
        .is_some_and(|nonce| nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

#[test]
fn test_continuous_stream_is_not_served_without_an_armed_session() {
    let dir = temp_dir("unarmed");
    let song = dir.join("a.wav");
    write_wav(&song, 100);
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    {
        let mut s = state.write().unwrap();
        s.queue = vec![queue_item(0, &song), queue_item(1, &song)];
    }
    let (handle, rx) = start(state.clone());
    let port = handle.port;

    for path in [
        "/stream/continuous.wav",
        "/stream/autopilot.wav",
        "/stream/continuous-0123456789abcdef0123456789abcdef.wav",
        "/stream/continuous.wav?x=1",
    ] {
        let (status, _) = open(port, path);
        assert_eq!(status, 404, "{}", path);
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(!handle.stream_state().continuous.is_active.load(Ordering::SeqCst));
    assert_eq!(state.read().unwrap().title, "No Track Loaded", "an unarmed request must not change playback state");
    assert!(rx.try_recv().is_err(), "an unarmed request must not send commands");

    handle.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_armed_continuous_session_serves_only_its_own_path() {
    let (handle, _rx) = start(Arc::new(RwLock::new(NowPlayingState::default())));
    let port = handle.port;
    let ss = handle.stream_state();

    let path = ss.arm_continuous_session();
    assert!(is_session_path(&path), "{}", path);
    assert_eq!(ss.continuous_session_path().as_deref(), Some(path.as_str()));

    // Other continuous paths stay closed.
    for other in ["/stream/continuous.wav", "/stream/autopilot.wav", "/stream/continuous-ffffffffffffffffffffffffffffffff.wav"] {
        let (status, _) = open(port, other);
        assert_eq!(status, 404, "{}", other);
    }

    // The armed path streams (empty queue: silence after the WAV header).
    let (status, mut reader) = open(port, &path);
    assert_eq!(status, 200);
    let mut wav_hdr = [0u8; 44];
    reader.read_exact(&mut wav_hdr).unwrap();
    assert_eq!(&wav_hdr[0..4], b"RIFF");
    assert!(ss.continuous.is_active.load(Ordering::SeqCst));
    drop(reader);

    // Closing the streams ends the session.
    ss.close_active_streams();
    assert_eq!(ss.continuous_session_path(), None);
    let (status, _) = open(port, &path);
    assert_eq!(status, 404);

    // A new session gets a new path.
    let next = ss.arm_continuous_session();
    assert!(is_session_path(&next));
    assert_ne!(next, path);
    let (status, _) = open(port, &path);
    assert_eq!(status, 404);

    handle.stop();
}

#[test]
fn test_continuous_session_admits_only_the_first_peer() {
    let c = ContinuousStreamController::new();
    let renderer = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20));
    let other = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 66));

    assert_eq!(c.admit("/stream/continuous.wav", renderer), ContinuousAdmission::NotFound);
    let path = c.arm_session();
    assert!(matches!(c.admit(&path, renderer), ContinuousAdmission::Admitted(_)));
    // The renderer may reconnect (e.g. HEAD then GET, or after a buffer underrun).
    assert!(matches!(c.admit(&path, renderer), ContinuousAdmission::Admitted(_)));
    // Another host knowing the path cannot take the stream over.
    assert_eq!(c.admit(&path, other), ContinuousAdmission::Conflict);
    // Wrong or legacy paths are unknown.
    assert_eq!(c.admit("/stream/continuous-00000000000000000000000000000000.wav", renderer), ContinuousAdmission::NotFound);
    assert_eq!(c.admit("/stream/autopilot.wav", renderer), ContinuousAdmission::NotFound);

    // A new session binds afresh.
    let next = c.arm_session();
    assert_eq!(c.admit(&path, renderer), ContinuousAdmission::NotFound);
    assert!(matches!(c.admit(&next, other), ContinuousAdmission::Admitted(_)));
    assert_eq!(c.admit(&next, renderer), ContinuousAdmission::Conflict);

    c.disarm_session();
    assert_eq!(c.admit(&next, other), ContinuousAdmission::NotFound);
}

#[test]
fn test_armed_continuous_stream_plays_queue_gaplessly_and_shuffles_on_wrap() {
    if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("ffmpeg not installed; skipping");
        return;
    }
    let dir = temp_dir("wrap");
    let a = dir.join("a.wav");
    let b = dir.join("b.wav");
    write_wav(&a, 100);
    write_wav(&b, 100);
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    state.write().unwrap().queue = vec![queue_item(0, &a), queue_item(1, &b)];
    let (handle, rx) = start(state.clone());
    let port = handle.port;
    let path = handle.stream_state().arm_continuous_session();

    let (status, mut reader) = open(port, &path);
    assert_eq!(status, 200);
    let reading = Arc::new(AtomicBool::new(true));
    let reading_thread = reading.clone();
    let drain = std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        while reading_thread.load(Ordering::SeqCst) {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });

    // Both tracks are decoded into the one connection, then the wrap asks for a reshuffle.
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(WebCommand::ShuffleQueue) => {}
        other => panic!("expected ShuffleQueue after the queue wrapped, got {:?}", other),
    }
    assert!(state.read().unwrap().title.starts_with("Track "));

    reading.store(false, Ordering::SeqCst);
    handle.stream_state().close_active_streams();
    let _ = drain.join();
    // Once the session is closed, no further shuffles arrive.
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if let Ok(cmd) = rx.recv_timeout(Duration::from_millis(50)) {
            // A wrap that completed just before the close may still be in flight.
            assert_eq!(cmd, WebCommand::ShuffleQueue);
        }
    }
    assert!(rx.recv_timeout(Duration::from_millis(500)).is_err(), "closed session kept sending commands");
    let (status, _) = open(port, &path);
    assert_eq!(status, 404);

    handle.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A renderer that accepts every SOAP call and records the URI given to SetAVTransportURI.
fn spawn_mock_renderer() -> (u16, Arc<AtomicBool>, Arc<Mutex<String>>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let uri = Arc::new(Mutex::new(String::new()));
    let (stop_t, uri_t) = (stop.clone(), uri.clone());
    let handle = std::thread::spawn(move || {
        while !stop_t.load(Ordering::SeqCst) {
            let mut stream = match listener.accept() {
                Ok((s, _)) => s,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(_) => break,
            };
            let _ = stream.set_nonblocking(false);
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            let _ = reader.read_exact(&mut body);
            let body = String::from_utf8_lossy(&body);
            if let Some(start) = body.find("<CurrentURI>") {
                if let Some(len) = body[start..].find("</CurrentURI>") {
                    *uri_t.lock().unwrap() = body[start + 12..start + len].to_string();
                }
            }
            let xml = "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:SuccessResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\" /></s:Body></s:Envelope>";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                xml.len(),
                xml
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    (port, stop, uri, handle)
}

#[test]
fn test_upnp_continuous_play_uses_the_armed_session_path() {
    let (port, stop, uri, mock) = spawn_mock_renderer();
    let device = DLNADevice {
        udn: "uuid:continuous".to_string(),
        friendly_name: "Mock Renderer".to_string(),
        model_name: "Test".to_string(),
        manufacturer: "Test".to_string(),
        host: "127.0.0.1".to_string(),
        port,
        location: format!("http://127.0.0.1:{}/desc.xml", port),
        av_transport_control_url: format!("http://127.0.0.1:{}/ctrl/AVTransport", port),
        rendering_control_url: format!("http://127.0.0.1:{}/ctrl/RenderingControl", port),
    };
    let ss = Arc::new(StreamServerState::new());
    let mut player = UPnPPlayer::new(device, 8080).with_stream_state(Some(ss.clone()));

    player.play(Path::new("__continuous__.wav")).expect("start continuous session");
    let session_path = ss.continuous_session_path().expect("session armed");
    assert!(is_session_path(&session_path), "{}", session_path);
    let sent = uri.lock().unwrap().clone();
    assert!(sent.starts_with("http://"), "{}", sent);
    assert!(sent.ends_with(&format!(":8080{}", session_path)), "{} vs {}", sent, session_path);
    assert!(ss.lookup_file("__continuous__.wav").is_none(), "the placeholder name is not a servable file");

    // Stopping ends the session.
    let _ = player.stop();
    assert_eq!(ss.continuous_session_path(), None);

    stop.store(true, Ordering::SeqCst);
    let _ = mock.join();
}
