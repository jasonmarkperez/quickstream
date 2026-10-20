//! The continuous (gapless) WAV stream is only served during a UPnP continuous session, at a
//! per-session unguessable path, to the renderer that first connected.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use quickstream::discovery::DLNADevice;
use quickstream::metadata::AudioMetadata;
use quickstream::player::upnp::UPnPPlayer;
use quickstream::player::AudioPlayer;
use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;
use quickstream::web::{
    ContinuousAdmission, ContinuousStreamController, NowPlayingState, StreamServerState, WebCommand, WebQueueItem,
    WebServer, WebServerHandle,
};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qs_continuous_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

/// A silent 16-bit 44.1 kHz stereo WAV.
fn write_wav(path: &Path, millis: u32) {
    let data_len = u32::try_from(44100 * 4 * u64::from(millis) / 1000).expect("WAV under 4 GB");
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

fn is_session_path(path: &str) -> bool {
    path.strip_prefix("/stream/continuous-")
        .and_then(|rest| rest.strip_suffix(".wav"))
        .is_some_and(|nonce| {
            nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

#[test]
fn test_continuous_stream_is_not_served_without_an_armed_session() {
    common::isolate();
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
    assert_eq!(
        state.read().unwrap().title,
        "No Track Loaded",
        "an unarmed request must not change playback state"
    );
    assert!(rx.try_recv().is_err(), "an unarmed request must not send commands");

    handle.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_armed_continuous_session_serves_only_its_own_path() {
    common::isolate();
    let (handle, _rx) = start(Arc::new(RwLock::new(NowPlayingState::default())));
    let port = handle.port;
    let ss = handle.stream_state();

    let path = ss.arm_continuous_session();
    assert!(is_session_path(&path), "{}", path);
    assert_eq!(ss.continuous_session_path().as_deref(), Some(path.as_str()));

    // Other continuous paths stay closed.
    for other in [
        "/stream/continuous.wav",
        "/stream/autopilot.wav",
        "/stream/continuous-ffffffffffffffffffffffffffffffff.wav",
    ] {
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
    common::isolate();
    let c = ContinuousStreamController::new();
    let renderer = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20));
    let other = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 66));

    assert_eq!(
        c.admit("/stream/continuous.wav", renderer),
        ContinuousAdmission::NotFound
    );
    let path = c.arm_session();
    assert!(matches!(c.admit(&path, renderer), ContinuousAdmission::Admitted(_)));
    // The renderer may reconnect (e.g. HEAD then GET, or after a buffer underrun).
    assert!(matches!(c.admit(&path, renderer), ContinuousAdmission::Admitted(_)));
    // Another host knowing the path cannot take the stream over.
    assert_eq!(c.admit(&path, other), ContinuousAdmission::Conflict);
    // Wrong or legacy paths are unknown.
    assert_eq!(
        c.admit("/stream/continuous-00000000000000000000000000000000.wav", renderer),
        ContinuousAdmission::NotFound
    );
    assert_eq!(
        c.admit("/stream/autopilot.wav", renderer),
        ContinuousAdmission::NotFound
    );

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
    common::isolate();
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
    assert!(
        rx.recv_timeout(Duration::from_millis(500)).is_err(),
        "closed session kept sending commands"
    );
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
    common::isolate();
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

    player
        .play(Path::new("__continuous__.wav"))
        .expect("start continuous session");
    let session_path = ss.continuous_session_path().expect("session armed");
    assert!(is_session_path(&session_path), "{}", session_path);
    let sent = uri.lock().unwrap().clone();
    assert!(sent.starts_with("http://"), "{}", sent);
    assert!(
        sent.ends_with(&format!(":8080{}", session_path)),
        "{} vs {}",
        sent,
        session_path
    );
    assert!(
        ss.lookup_file("__continuous__.wav").is_none(),
        "the placeholder name is not a servable file"
    );

    // Stopping ends the session.
    let _ = player.stop();
    assert_eq!(ss.continuous_session_path(), None);

    stop.store(true, Ordering::SeqCst);
    let _ = mock.join();
}

fn track_meta(path: &Path, title: &str, seconds: f64) -> AudioMetadata {
    AudioMetadata {
        filepath: path.to_path_buf(),
        filename: path.file_name().unwrap().to_string_lossy().to_string(),
        title: title.to_string(),
        artist: "Artist".to_string(),
        album: "Album".to_string(),
        genre: String::new(),
        format_name: "wav".to_string(),
        duration_seconds: seconds,
        file_size: 0,
        bitrate_kbps: 0,
        sample_rate_hz: 0,
        channels: 0,
    }
}

/// A track whose length wasn't known when the continuous stream started it (placeholder
/// metadata, tags not read yet) gets its length as soon as the UI publishes the queue with it
/// (`AppUI::update_web_state`), without waiting for the next track, and whatever the stream's
/// socket is doing: here the renderer has stopped reading, so the stream's writer is blocked.
#[test]
fn test_continuous_stream_learns_an_unknown_length_mid_track() {
    common::isolate();
    if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("ffmpeg not installed; skipping");
        return;
    }
    let dir = temp_dir("learn_length");
    let a = dir.join("a.wav");
    let b = dir.join("b.wav");
    let c = dir.join("c.wav");
    // Track A is longer than the socket buffers can hold (Linux lets the server write several MB
    // ahead of the reader), so the server is still on A, blocked writing it, while the test runs.
    write_wav(&a, 120_000);
    write_wav(&b, 100);
    write_wav(&c, 100);
    let settings = SettingsManager::with_path(dir.join("config.json"));
    let mut app = AppUI::new(Some(dir.clone()), settings);
    app.queue
        .add_tracks(vec![track_meta(&a, "Track 0", 0.0), track_meta(&b, "Track 1", 0.0)]);
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, app.web_state.clone(), tx).expect("start web server");
    let port = handle.port;
    let controller = handle.stream_state().continuous.clone();
    let path = handle.stream_state().arm_continuous_session();
    app.web_server = Some(handle);
    app.update_web_state();
    let state = app.web_state.clone();
    // The UI has published A as the current track already (its title and path too), but not as
    // playing: the local player is stopped. Only the stream marks it playing, once it has started A.
    assert!(!state.read().unwrap().is_playing);

    // The renderer reads the response head, then nothing more. The stream is active before it
    // writes that head (#79), and starts A after it.
    let (status, reader) = open(port, &path);
    assert_eq!(status, 200);
    assert!(
        controller.is_active.load(Ordering::SeqCst),
        "active before the head was sent"
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !state.read().unwrap().is_playing && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    {
        let s = state.read().unwrap();
        let index = *controller.current_track_index.lock().unwrap();
        assert!(
            s.is_playing && index == 0 && s.filepath.as_deref() == Some(a.as_path()),
            "track A never started: is_playing {}, index {}, filepath {:?}",
            s.is_playing,
            index,
            s.filepath
        );
    }
    assert_eq!(*controller.current_duration.lock().unwrap(), 0.0);

    // Wait until the writer is stuck: while writes go through it moves the position every
    // 250 ms, so a position that holds for a second means the socket is full. (It can hold at 0:
    // a socket that fills within the first 250 ms never publishes a position.)
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let before = *controller.current_position.lock().unwrap();
        std::thread::sleep(Duration::from_secs(1));
        if *controller.current_position.lock().unwrap() == before {
            break;
        }
        assert!(Instant::now() < deadline, "the stream never filled the socket");
    }
    assert_eq!(*controller.current_track_index.lock().unwrap(), 0, "still on track A");

    // Another track with a known length at A's place in the queue: not the track being
    // streamed, so its length is not taken.
    app.queue.insert_tracks(0, vec![track_meta(&c, "Track C", 50.0)]);
    app.update_web_state();
    assert_eq!(
        *controller.current_duration.lock().unwrap(),
        0.0,
        "C's length is not A's"
    );
    app.queue.remove_track(0);

    // A's tags arrive: publishing the queue gives the stream its length at once.
    app.queue.update_metadata(&track_meta(&a, "Track 0", 120.0));
    app.update_web_state();
    assert_eq!(*controller.current_duration.lock().unwrap(), 120.0, "length learned");
    assert_eq!(state.read().unwrap().duration_seconds, 120.0, "and published");
    assert_eq!(*controller.current_track_index.lock().unwrap(), 0, "still on track A");

    if let Some(handle) = app.web_server.take() {
        handle.stream_state().close_active_streams();
        drop(reader);
        handle.stop();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Seeking in the continuous stream is clamped to the track's length once it is known, and
/// never goes below 0 while it isn't.
#[test]
fn test_upnp_continuous_seek_is_clamped_to_the_known_length() {
    common::isolate();
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let device = DLNADevice {
        udn: "uuid:clamp".to_string(),
        friendly_name: "Clamp Renderer".to_string(),
        model_name: "Test".to_string(),
        manufacturer: "Test".to_string(),
        host: "127.0.0.1".to_string(),
        port,
        location: format!("http://127.0.0.1:{}/desc.xml", port),
        av_transport_control_url: format!("http://127.0.0.1:{}/ctrl/AVTransport", port),
        rendering_control_url: format!("http://127.0.0.1:{}/ctrl/RenderingControl", port),
    };
    let ss = Arc::new(StreamServerState::new());
    ss.continuous.is_active.store(true, Ordering::SeqCst);
    let mut player = UPnPPlayer::new(device, 8080).with_stream_state(Some(ss.clone()));
    let target = || *ss.continuous.target_seek.lock().unwrap();

    // Unknown length: any target from 0 up.
    player.seek_to(-5.0).unwrap();
    assert_eq!(target(), Some(0.0));
    player.seek_to(500.0).unwrap();
    assert_eq!(target(), Some(500.0));

    // Known length: clamped to it.
    *ss.continuous.current_duration.lock().unwrap() = 120.0;
    player.seek_to(500.0).unwrap();
    assert_eq!(target(), Some(120.0));
    player.seek_to(f64::NAN).unwrap();
    assert_eq!(target(), Some(0.0));
    assert_eq!(player.duration_seconds(), 120.0);
    ss.continuous.is_active.store(false, Ordering::SeqCst);
}
