//! The continuous (gapless) WAV stream is only served during a UPnP continuous session, at a
//! per-session unguessable path, to the renderer that first connected.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    let target = || ss.continuous.pending_request().map(|r| r.position);

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

/// A fake ffmpeg for the continuous stream (issues #106, #107): logs its arguments, one line per
/// run, and its pid (so the test can kill any it leaves). An input whose name contains `short`
/// gets 100 ms of PCM and ends 0.5 s later; any other input gets nothing, as while the real one
/// opens a file on a slow share, until it is killed.
#[cfg(unix)]
struct FakeDecoder {
    binary: PathBuf,
    log: PathBuf,
    pids: PathBuf,
}

#[cfg(unix)]
impl FakeDecoder {
    fn new(dir: &Path) -> Self {
        let binary = dir.join("fake-ffmpeg");
        let log = dir.join("ffmpeg.log");
        let pids = dir.join("ffmpeg.pids");
        let text = format!(
            "#!/bin/sh\necho $$ >> '{pids}'\nin=''\nprev=''\nfor a in \"$@\"; do [ \"$prev\" = '-i' ] && in=\"$a\"; prev=\"$a\"; done\nprintf '%s\\n' \"$*\" >> '{log}'\ncase \"$in\" in\n  *short*) head -c 17640 /dev/zero; exec sleep 0.5 ;;\nesac\nexec sleep 30\n",
            pids = pids.display(),
            log = log.display()
        );
        common::fake_exec::write_script(&binary, &text);
        Self { binary, log, pids }
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The runs so far, once there are `n` of them.
    fn wait_for_calls(&self, n: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let calls = self.calls();
            if calls.len() >= n {
                return calls;
            }
            assert!(
                Instant::now() < deadline,
                "expected {} decoder runs, got {:?}",
                n,
                calls
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(unix)]
impl Drop for FakeDecoder {
    fn drop(&mut self) {
        let pids = std::fs::read_to_string(&self.pids).unwrap_or_default();
        let pids: Vec<&str> = pids.split_whitespace().collect();
        if !pids.is_empty() {
            let _ = std::process::Command::new("kill")
                .args(&pids)
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

/// A continuous stream being read by a renderer that keeps up, counting the bytes after the
/// WAV header.
struct Renderer {
    received: Arc<AtomicU64>,
    reading: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Renderer {
    fn connect(port: u16, path: &str) -> Self {
        let (status, mut reader) = open(port, path);
        assert_eq!(status, 200);
        let mut wav_hdr = [0u8; 44];
        reader.read_exact(&mut wav_hdr).unwrap();
        let received = Arc::new(AtomicU64::new(0));
        let reading = Arc::new(AtomicBool::new(true));
        let (received_t, reading_t) = (received.clone(), reading.clone());
        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while reading_t.load(Ordering::SeqCst) {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        received_t.fetch_add(n as u64, Ordering::SeqCst);
                    }
                }
            }
        });
        Self {
            received,
            reading,
            thread: Some(thread),
        }
    }

    /// Bytes per second received over `window`.
    fn rate_over(&self, window: Duration) -> f64 {
        let (start, before) = (Instant::now(), self.received.load(Ordering::SeqCst));
        std::thread::sleep(window);
        let after = self.received.load(Ordering::SeqCst);
        (after - before) as f64 / start.elapsed().as_secs_f64()
    }

    /// Stop reading and close the connection.
    fn hang_up(mut self) {
        self.reading.store(false, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 44.1 kHz, 16-bit stereo: what a renderer playing at real time reads per second.
const REAL_TIME_BYTES_PER_SEC: f64 = 176_400.0;

fn assert_about_real_time(rate: f64, what: &str) {
    assert!(
        (0.85 * REAL_TIME_BYTES_PER_SEC..=1.3 * REAL_TIME_BYTES_PER_SEC).contains(&rate),
        "{}: {:.0} B/s, real time is {:.0} B/s",
        what,
        rate,
        REAL_TIME_BYTES_PER_SEC
    );
}

/// A web server streaming `names` (files in a fresh folder, starting at `first`) through a
/// [`FakeDecoder`] to a renderer that keeps reading.
#[cfg(unix)]
struct DecoderFixture {
    dir: PathBuf,
    tracks: Vec<PathBuf>,
    state: Arc<RwLock<NowPlayingState>>,
    rx: mpsc::Receiver<WebCommand>,
    controller: Arc<ContinuousStreamController>,
    renderer: Option<Renderer>,
    handle: Option<WebServerHandle>,
    decoder: FakeDecoder,
}

#[cfg(unix)]
impl DecoderFixture {
    fn start(name: &str, names: &[&str], first: usize) -> Self {
        let dir = temp_dir(name);
        let decoder = FakeDecoder::new(&dir);
        let tracks: Vec<PathBuf> = names.iter().map(|n| dir.join(n)).collect();
        for track in &tracks {
            std::fs::write(track, b"never opened: the fake decoder makes up its output").unwrap();
        }
        let state = Arc::new(RwLock::new(NowPlayingState::default()));
        state.write().unwrap().queue = tracks.iter().enumerate().map(|(i, p)| queue_item(i, p)).collect();
        let (handle, rx) = start(state.clone());
        let controller = handle.stream_state().continuous.clone();
        controller.set_decoder(&decoder.binary);
        *controller.current_track_index.lock().unwrap() = first;
        let path = handle.stream_state().arm_continuous_session();
        let renderer = Renderer::connect(handle.port, &path);
        Self {
            dir,
            tracks,
            state,
            rx,
            controller,
            renderer: Some(renderer),
            handle: Some(handle),
            decoder,
        }
    }

    /// Wait for decoder run `n` (from 1) and return its input and `-ss` offset (`None`: from
    /// the start).
    fn run(&self, n: usize) -> (PathBuf, Option<String>) {
        let calls = self.decoder.wait_for_calls(n);
        let call = &calls[n - 1];
        let input = call
            .split(" -i ")
            .nth(1)
            .and_then(|rest| rest.split(" -f ").next())
            .unwrap_or_else(|| panic!("no -i in {:?}", call));
        let seek = call
            .strip_prefix("-ss ")
            .and_then(|rest| rest.split(' ').next())
            .map(str::to_string);
        (PathBuf::from(input), seek)
    }

    /// Wait for decoder run `n`, then long enough for the stream to be blocked reading its
    /// (empty) output.
    fn wait_for_startup(&self, n: usize) -> (PathBuf, Option<String>) {
        let run = self.run(n);
        std::thread::sleep(Duration::from_millis(300));
        run
    }
}

#[cfg(unix)]
impl Drop for DecoderFixture {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.stream_state().close_active_streams();
            if let Some(renderer) = self.renderer.take() {
                renderer.hang_up();
            }
            handle.stop();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A seek while the decoder is still starting (no output yet) restarts the same track at the
/// offset, twice in a row too: it is not taken for the end of the track (#106).
#[cfg(unix)]
#[test]
fn test_continuous_seek_while_the_decoder_starts_restarts_the_same_track() {
    common::isolate();
    let f = DecoderFixture::start("seek_startup", &["t0.wav", "t1.wav", "t2.wav", "t3.wav"], 0);
    assert_eq!(f.wait_for_startup(1), (f.tracks[0].clone(), None));

    f.controller.seek_to(42.0);
    assert_eq!(f.wait_for_startup(2), (f.tracks[0].clone(), Some("42.000".to_string())));
    f.controller.seek_to(10.0);
    assert_eq!(f.wait_for_startup(3), (f.tracks[0].clone(), Some("10.000".to_string())));

    assert_eq!(*f.controller.current_track_index.lock().unwrap(), 0);
    assert_eq!(f.state.read().unwrap().queue_index, 0);
    assert_eq!(f.state.read().unwrap().position_seconds, 10.0);
    assert!(f.rx.try_recv().is_err(), "nothing wrapped");
}

/// A skip while the decoder is still starting plays the track asked for, from its start, and
/// leaves nothing behind for a later seek to pick up (#106).
#[cfg(unix)]
#[test]
fn test_continuous_skip_while_the_decoder_starts_plays_the_requested_track() {
    common::isolate();
    let f = DecoderFixture::start("skip_startup", &["t0.wav", "t1.wav", "t2.wav", "t3.wav"], 0);
    f.wait_for_startup(1);

    f.controller.skip_to_index(2);
    assert_eq!(f.wait_for_startup(2), (f.tracks[2].clone(), None));
    assert_eq!(*f.controller.current_track_index.lock().unwrap(), 2);
    assert_eq!(f.state.read().unwrap().queue_index, 2);

    // A later seek is for track 2, not a stale target.
    f.controller.seek_to(7.0);
    assert_eq!(f.wait_for_startup(3), (f.tracks[2].clone(), Some("7.000".to_string())));
    assert_eq!(*f.controller.current_track_index.lock().unwrap(), 2);
    assert!(f.rx.try_recv().is_err(), "nothing wrapped");
}

/// A seek on the last track restarts it: nothing wrapped, so the queue is not reshuffled (#106).
#[cfg(unix)]
#[test]
fn test_continuous_seek_on_the_last_track_sends_no_shuffle() {
    common::isolate();
    let f = DecoderFixture::start("seek_last", &["t0.wav", "t1.wav"], 1);
    assert_eq!(f.wait_for_startup(1), (f.tracks[1].clone(), None));

    f.controller.seek_to(5.0);
    assert_eq!(f.wait_for_startup(2), (f.tracks[1].clone(), Some("5.000".to_string())));
    if let Ok(cmd) = f.rx.recv_timeout(Duration::from_millis(300)) {
        panic!("a seek on the last track sent {:?}", cmd);
    }
    assert_eq!(*f.controller.current_track_index.lock().unwrap(), 1);
}

/// A skip that arrives as a track ends, while the stream waits to pick the next one, is not
/// lost: the stream plays the track asked for, not the one after the track that ended (#106).
#[cfg(unix)]
#[test]
fn test_continuous_skip_at_a_track_boundary_is_not_lost() {
    common::isolate();
    let f = DecoderFixture::start("skip_boundary", &["t0_short.wav", "t1.wav", "t2.wav", "t3.wav"], 0);
    assert_eq!(f.run(1), (f.tracks[0].clone(), None));

    {
        // Hold the web state while track 0 ends, so the stream waits at the top of its loop.
        let _held = f.state.write().unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(f.decoder.calls().len(), 1, "the stream waits for the web state");
        f.controller.skip_to_index(3);
    }
    assert_eq!(f.wait_for_startup(2), (f.tracks[3].clone(), None));
    assert_eq!(*f.controller.current_track_index.lock().unwrap(), 3);
    assert_eq!(f.controller.pending_request(), None);
}

/// A track that ends with no request pending still moves the stream on to the next one.
#[cfg(unix)]
#[test]
fn test_continuous_track_end_without_a_request_moves_on() {
    common::isolate();
    let f = DecoderFixture::start("natural_end", &["t0_short.wav", "t1.wav"], 0);
    assert_eq!(f.run(1), (f.tracks[0].clone(), None));
    assert_eq!(f.wait_for_startup(2), (f.tracks[1].clone(), None));
    assert_eq!(*f.controller.current_track_index.lock().unwrap(), 1);
    assert!(f.rx.try_recv().is_err(), "not the last track: no reshuffle");
}

/// With an empty queue the stream sends silence at real time, so a renderer doesn't run dry
/// (#107).
#[test]
fn test_continuous_stream_sends_silence_at_real_time_while_the_queue_is_empty() {
    common::isolate();
    let (handle, _rx) = start(Arc::new(RwLock::new(NowPlayingState::default())));
    let path = handle.stream_state().arm_continuous_session();
    let renderer = Renderer::connect(handle.port, &path);
    std::thread::sleep(Duration::from_millis(300));
    assert_about_real_time(renderer.rate_over(Duration::from_millis(1500)), "empty queue");

    handle.stream_state().close_active_streams();
    renderer.hang_up();
    handle.stop();
}

/// Lines of the log naming `needle`.
fn log_lines_naming(needle: &str) -> usize {
    assert!(quickstream::logger::flush(), "the log was flushed");
    let log = std::fs::read_to_string(quickstream::logger::get_log_file_path()).unwrap_or_default();
    log.lines().filter(|line| line.contains(needle)).count()
}

/// A queue whose files are all missing (the share was unmounted) doesn't make the stream spin:
/// it logs them once, sends silence at real time, notices the renderer leave, and plays a file
/// once it is there again (#107).
#[cfg(unix)]
#[test]
fn test_continuous_stream_with_every_file_missing_sends_silence_without_spinning() {
    common::isolate();
    quickstream::logger::init();
    let dir = temp_dir("all_missing");
    let decoder = FakeDecoder::new(&dir);
    let missing = [dir.join("gone/a.wav"), dir.join("gone/b.wav")];
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    state.write().unwrap().queue = missing.iter().enumerate().map(|(i, p)| queue_item(i, p)).collect();
    let (handle, _rx) = start(state.clone());
    let controller = handle.stream_state().continuous.clone();
    controller.set_decoder(&decoder.binary);
    let path = handle.stream_state().arm_continuous_session();

    let renderer = Renderer::connect(handle.port, &path);
    std::thread::sleep(Duration::from_millis(300));
    assert_about_real_time(renderer.rate_over(Duration::from_millis(1500)), "every file missing");
    let lines = log_lines_naming(&dir.join("gone").display().to_string());
    assert!(
        (1..=6).contains(&lines),
        "the missing files are logged once, not on every pass: {} lines",
        lines
    );
    assert!(decoder.calls().is_empty(), "nothing to decode");

    // The renderer leaves: the stream notices through its silence writes.
    renderer.hang_up();
    let deadline = Instant::now() + Duration::from_secs(5);
    while controller.is_active.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the stream outlived its renderer");
        std::thread::sleep(Duration::from_millis(20));
    }

    // It comes back, and so does a file: the stream plays it.
    let renderer = Renderer::connect(handle.port, &path);
    std::thread::sleep(Duration::from_millis(200));
    std::fs::create_dir_all(dir.join("gone")).unwrap();
    std::fs::write(&missing[1], b"back").unwrap();
    let calls = decoder.wait_for_calls(1);
    assert!(calls[0].contains(&missing[1].display().to_string()), "{:?}", calls);
    assert_eq!(*controller.current_track_index.lock().unwrap(), 1);

    handle.stream_state().close_active_streams();
    renderer.hang_up();
    handle.stop();
    drop(decoder);
    let _ = std::fs::remove_dir_all(&dir);
}
