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
    ContinuousAdmission, ContinuousStreamController, NowPlayingState, StreamRequest, StreamServerState, WebCommand,
    WebQueueItem, WebServer, WebServerHandle,
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
/// gets 100 ms of PCM and ends 0.5 s later; one with `endless` gets PCM as fast as it is read
/// until it is killed (for a renderer that stops reading); one with `live` gets PCM at about real
/// time until it is killed (for one that keeps reading, so it doesn't load the machine); one with
/// `bad` gets nothing and an exit at once (a file ffmpeg can't decode); `ones` gets 0.5 s of
/// `0x01` bytes at about real time and `burst` 1 s of them at once, then an exit (a track that
/// ends); `twos` gets 0.1 s of `0x02` bytes, then nothing until killed; any other input gets
/// nothing, as while the real one opens a file on a slow share, until it is killed.
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
            "#!/bin/sh\necho $$ >> '{pids}'\nin=''\nprev=''\nfor a in \"$@\"; do [ \"$prev\" = '-i' ] && in=\"$a\"; prev=\"$a\"; done\nprintf '%s\\n' \"$*\" >> '{log}'\ncase \"$in\" in\n  *short*) head -c 17640 /dev/zero; exec sleep 0.5 ;;\n  *endless*) exec cat /dev/zero ;;\n  *live*) while :; do head -c 17640 /dev/zero; sleep 0.1; done ;;\n  *bad*) exit 1 ;;\n  *ones*) for i in 1 2 3 4 5; do head -c 17640 /dev/zero | tr '\\000' '\\001'; sleep 0.1; done; exit 0 ;;\n  *burst*) head -c 176400 /dev/zero | tr '\\000' '\\001'; exit 0 ;;\n  *twos*) head -c 17640 /dev/zero | tr '\\000' '\\002'; exec sleep 30 ;;\nesac\nexec sleep 30\n",
            pids = pids.display(),
            log = log.display()
        );
        common::fake_exec::write_script(&binary, &text);
        // The first run of a just-written script can take seconds to start on a loaded machine;
        // paid here, before any stream waits on it, then forgotten.
        let warm_up = std::process::Command::new(&binary)
            .args(["-i", "warm_up_bad"])
            .status()
            .expect("run the fake decoder");
        assert!(!warm_up.success(), "the warm-up run exits at once");
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(&pids);
        Self { binary, log, pids }
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The pid of decoder run `n` (from 1).
    fn pid(&self, n: usize) -> String {
        let pids = std::fs::read_to_string(&self.pids).unwrap_or_default();
        pids.split_whitespace()
            .nth(n - 1)
            .unwrap_or_else(|| panic!("no pid for decoder run {}: {:?}", n, pids))
            .to_string()
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

/// Whether process `pid` is still running (or not yet reaped).
#[cfg(unix)]
fn pid_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Wait up to 10 s for `done`.
fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "{}", what);
        std::thread::sleep(Duration::from_millis(10));
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
        // As long as the stream's own write timeout: the stream sends nothing while a decoder
        // starts, which can take seconds on a loaded machine (or a slow share).
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
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

    /// Whether the stream closes the connection within `window`.
    fn closed_within(&self, window: Duration) -> bool {
        let deadline = Instant::now() + window;
        while !self.thread.as_ref().is_some_and(|t| t.is_finished()) {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
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
    /// The web server's port and the session's path, for a second connection.
    port: u16,
    path: String,
    renderer: Option<Renderer>,
    handle: Option<WebServerHandle>,
    decoder: FakeDecoder,
}

#[cfg(unix)]
impl DecoderFixture {
    fn start(name: &str, names: &[&str], first: usize) -> Self {
        Self::start_with(name, names, first, |_, _| {})
    }

    /// As [`Self::start`], with `setup` run on the controller before the renderer connects.
    fn start_with(
        name: &str,
        names: &[&str],
        first: usize,
        setup: impl FnOnce(&ContinuousStreamController, &[PathBuf]),
    ) -> Self {
        let mut f = Self::armed(name, names, first, setup);
        f.renderer = Some(Renderer::connect(f.port, &f.path));
        f
    }

    /// As [`Self::start_with`], with no renderer connected yet.
    fn armed(
        name: &str,
        names: &[&str],
        first: usize,
        setup: impl FnOnce(&ContinuousStreamController, &[PathBuf]),
    ) -> Self {
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
        setup(&controller, &tracks);
        *controller.current_track_index.lock().unwrap() = first;
        let path = handle.stream_state().arm_continuous_session();
        Self {
            dir,
            tracks,
            state,
            rx,
            controller,
            port: handle.port,
            path,
            renderer: None,
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

/// A skip and seek still pending when the session ends (the stream was blocked writing to a
/// renderer that stopped reading) are dropped with it: the next session starts where the UI
/// rewound it, not on the stale track or offset.
#[cfg(unix)]
#[test]
fn test_continuous_request_pending_at_session_end_is_not_carried_over() {
    common::isolate();
    let dir = temp_dir("stale_request");
    let decoder = FakeDecoder::new(&dir);
    let tracks: Vec<PathBuf> = ["t0.wav", "t1_endless.wav", "t2.wav"]
        .iter()
        .map(|n| dir.join(n))
        .collect();
    for track in &tracks {
        std::fs::write(track, b"never opened: the fake decoder makes up its output").unwrap();
    }
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    state.write().unwrap().queue = tracks.iter().enumerate().map(|(i, p)| queue_item(i, p)).collect();
    let (handle, _rx) = start(state);
    let ss = handle.stream_state();
    let controller = ss.continuous.clone();
    controller.set_decoder(&decoder.binary);
    *controller.current_track_index.lock().unwrap() = 1;
    let path = ss.arm_continuous_session();

    // Reads nothing after the response head, so the stream ends up blocked writing to it.
    let (status, stalled) = open(handle.port, &path);
    assert_eq!(status, 200);
    assert!(decoder.wait_for_calls(1)[0].contains("t1_endless"));
    std::thread::sleep(Duration::from_millis(500));
    controller.skip_to_index(2);
    controller.seek_to(30.0);
    let stale = StreamRequest {
        index: 2,
        position: 30.0,
    };
    assert_eq!(
        controller.pending_request(),
        Some(stale),
        "the blocked stream hasn't taken it"
    );

    // The session ends as `AppUI::end_continuous_stream` ends it.
    ss.close_active_streams();
    *controller.current_track_index.lock().unwrap() = 0;
    drop(stalled);
    assert_eq!(controller.pending_request(), None);

    let path = ss.arm_continuous_session();
    let renderer = Renderer::connect(handle.port, &path);
    let calls = decoder.wait_for_calls(2);
    assert!(
        calls[1].contains(&tracks[0].display().to_string()) && !calls[1].contains("-ss"),
        "the new session starts on track 0 from its start: {:?}",
        calls[1]
    );

    ss.close_active_streams();
    renderer.hang_up();
    handle.stop();
    drop(decoder);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A queue whose files all exist but give no audio (ffmpeg can't decode them) doesn't make the
/// stream spin either: after one try of each it sends silence at real time, tries again once a
/// second, asks for no reshuffle, and plays a file that joins the queue, shown as playing once
/// it gives audio (#107).
#[cfg(unix)]
#[test]
fn test_continuous_stream_with_no_decodable_file_sends_silence_without_spinning() {
    common::isolate();
    let f = DecoderFixture::start("none_decodable", &["a_bad.wav", "b_bad.wav", "c_bad.wav"], 0);
    // Measured once each file has been tried: the first run of a script just written can take
    // seconds to start on macOS while other tests start theirs.
    f.decoder.wait_for_calls(3);
    std::thread::sleep(Duration::from_millis(300));
    let renderer = f.renderer.as_ref().unwrap();
    assert_about_real_time(renderer.rate_over(Duration::from_millis(1500)), "no decodable file");
    let runs = f.decoder.calls().len();
    assert!(
        (3..=8).contains(&runs),
        "one try of each file, then one a second: {} decoder runs",
        runs
    );
    assert!(f.rx.try_recv().is_err(), "nothing played, so nothing to reshuffle");

    let good = f.dir.join("d_live.wav");
    std::fs::write(&good, b"never opened: the fake decoder makes up its output").unwrap();
    {
        let mut s = f.state.write().unwrap();
        let index = s.queue.len();
        s.queue.push(queue_item(index, &good));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f
        .decoder
        .calls()
        .iter()
        .any(|c| c.contains(&good.display().to_string()))
    {
        assert!(
            Instant::now() < deadline,
            "the new file never played: {:?}",
            f.decoder.calls()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    wait_until("the new file was never shown as playing", || {
        *f.controller.current_track_index.lock().unwrap() == 3
    });
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
/// once it is there again, shown as playing once it gives audio (#107).
#[cfg(unix)]
#[test]
fn test_continuous_stream_with_every_file_missing_sends_silence_without_spinning() {
    common::isolate();
    quickstream::logger::init();
    let dir = temp_dir("all_missing");
    let decoder = FakeDecoder::new(&dir);
    let missing = [dir.join("gone/a.wav"), dir.join("gone/b_live.wav")];
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
    wait_until("the file that came back was never shown as playing", || {
        *controller.current_track_index.lock().unwrap() == 1
    });

    handle.stream_state().close_active_streams();
    renderer.hang_up();
    handle.stop();
    drop(decoder);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A track that gave audio starts the count of silent tracks afresh, however it was stopped: a
/// skip out of it doesn't leave the earlier failures counted, so the bad files after it don't
/// make the stream idle with a playable file in the queue (#107).
#[cfg(unix)]
#[test]
fn test_continuous_audio_resets_the_silent_count_when_skipped_out_of() {
    common::isolate();
    let f = DecoderFixture::start("silent_count", &["a_live.wav", "b_bad.wav", "c_bad.wav"], 1);
    // b and c give nothing, then a plays.
    assert_eq!(f.run(3).0, f.tracks[0]);
    wait_until("a never played", || {
        *f.controller.current_track_index.lock().unwrap() == 0
    });

    f.controller.skip_to_index(1);
    let asked = Instant::now();
    assert_eq!(f.run(4).0, f.tracks[1]);
    assert_eq!(f.run(5).0, f.tracks[2]);
    assert_eq!(f.run(6).0, f.tracks[0]);
    assert!(
        asked.elapsed() < Duration::from_secs(1),
        "b and c failing again is not the whole queue failing: a came back after {:?}",
        asked.elapsed()
    );
}

/// Next twice in a row while the stream is blocked writing (the renderer reads slowly) lands two
/// tracks on: `skip_to_index` moves `current_track_index` at once, so the event loop's sync
/// doesn't put the queue back on the old track before the stream acts on the skip (#106).
#[cfg(unix)]
#[test]
fn test_continuous_two_skips_while_the_stream_is_blocked_land_two_tracks_on() {
    common::isolate();
    let dir = temp_dir("double_skip");
    let decoder = FakeDecoder::new(&dir);
    let tracks: Vec<PathBuf> = ["t0_endless.wav", "t1.wav", "t2.wav", "t3.wav"]
        .iter()
        .map(|n| dir.join(n))
        .collect();
    for track in &tracks {
        std::fs::write(track, b"never opened: the fake decoder makes up its output").unwrap();
    }
    let settings = SettingsManager::with_path(dir.join("config.json"));
    let mut app = AppUI::new(Some(dir.clone()), settings);
    app.queue.add_tracks(
        tracks
            .iter()
            .enumerate()
            .map(|(i, p)| track_meta(p, &format!("Track {}", i), 60.0))
            .collect(),
    );
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, app.web_state.clone(), tx).expect("start web server");
    let port = handle.port;
    let controller = handle.stream_state().continuous.clone();
    controller.set_decoder(&decoder.binary);
    let path = handle.stream_state().arm_continuous_session();
    app.web_server = Some(handle);
    app.update_web_state();

    // Reads nothing after the response head, so the stream ends up blocked writing t0.
    let (status, stalled) = open(port, &path);
    assert_eq!(status, 200);
    assert!(decoder.wait_for_calls(1)[0].contains("t0_endless"));
    std::thread::sleep(Duration::from_millis(500));

    app.play_next_track();
    assert_eq!(
        *controller.current_track_index.lock().unwrap(),
        1,
        "the stream's index follows the skip at once"
    );
    app.sync_continuous_stream_index();
    assert_eq!(app.queue.current_index(), 1, "the event loop's sync keeps the skip");
    app.play_next_track();
    app.sync_continuous_stream_index();
    assert_eq!(app.queue.current_index(), 2);
    assert_eq!(*controller.current_track_index.lock().unwrap(), 2);
    assert_eq!(controller.pending_request().map(|r| r.position), Some(0.0));
    assert_eq!(decoder.calls().len(), 1, "the stream is still blocked writing t0");

    if let Some(handle) = app.web_server.take() {
        handle.stream_state().close_active_streams();
        drop(stalled);
        handle.stop();
    }
    drop(decoder);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A seek that arrives after a track has ended, before the stream starts the next one, is for
/// the track it was asked on: that track restarts at the offset, and the next one still starts
/// from its beginning (#106).
#[cfg(unix)]
#[test]
fn test_continuous_seek_just_after_a_track_ends_is_not_applied_to_the_next_track() {
    common::isolate();
    let f = DecoderFixture::start("seek_after_end", &["t0_short.wav", "t1.wav", "t2.wav"], 0);
    assert_eq!(f.run(1), (f.tracks[0].clone(), None));

    {
        // Hold the web state while track 0 ends, so the stream waits to pick the next track.
        let _held = f.state.write().unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(f.decoder.calls().len(), 1, "the stream waits for the web state");
        f.controller.seek_to(40.0);
    }
    assert_eq!(f.run(2), (f.tracks[0].clone(), Some("40.000".to_string())));
    assert_eq!(f.run(3), (f.tracks[1].clone(), None), "the next track from its start");
}

/// When the last track ends, the stream's index is on the first track before it asks for the
/// reshuffle, so the UI (which follows that index, and whose shuffle keeps its current track in
/// front) never puts the track that just ended first and plays it again (#106).
#[cfg(unix)]
#[test]
fn test_continuous_wrap_reshuffle_keeps_the_next_track_in_front() {
    common::isolate();
    let f = DecoderFixture::start("wrap_front", &["t0.wav", "t1_short.wav"], 1);
    assert_eq!(f.run(1), (f.tracks[1].clone(), None));
    // Past t1's audio (and the position update it may bring), well before t1 ends.
    std::thread::sleep(Duration::from_millis(200));

    {
        // Hold the web state while t1 ends, so the stream waits to pick the next track; then do
        // what one UI tick does with the reshuffle: follow the stream's index, shuffle with that
        // track in front, publish.
        let mut held = f.state.write().unwrap();
        match f.rx.recv_timeout(Duration::from_secs(10)) {
            Ok(WebCommand::ShuffleQueue) => {}
            other => panic!("expected ShuffleQueue after the queue wrapped, got {:?}", other),
        }
        let front = *f.controller.current_track_index.lock().unwrap();
        let kept = held.queue.remove(front);
        held.queue.insert(0, kept);
    }
    assert_eq!(
        f.run(2),
        (f.tracks[0].clone(), None),
        "the track that ended doesn't play again"
    );
}

/// The UI follows the continuous stream's index before it reshuffles the queue for a wrap, so
/// the track the stream plays next stays in front even when the reshuffle arrives in the same
/// tick as the index moved (#106).
#[test]
fn test_continuous_reshuffle_follows_the_stream_index_first() {
    common::isolate();
    let dir = temp_dir("reshuffle_sync");
    let tracks: Vec<PathBuf> = (0..4).map(|i| dir.join(format!("t{}.wav", i))).collect();
    let settings = SettingsManager::with_path(dir.join("config.json"));
    let mut app = AppUI::new(Some(dir.clone()), settings);
    app.queue.add_tracks(
        tracks
            .iter()
            .enumerate()
            .map(|(i, p)| track_meta(p, &format!("Track {}", i), 60.0))
            .collect(),
    );
    app.queue.set_current_index(3);
    let (tx, _web_rx) = mpsc::channel();
    let handle = WebServer::start(0, app.web_state.clone(), tx).expect("start web server");
    let controller = handle.stream_state().continuous.clone();
    controller.is_active.store(true, Ordering::SeqCst);
    *controller.current_track_index.lock().unwrap() = 0;
    app.web_server = Some(handle);
    let (cmd_tx, cmd_rx) = mpsc::channel();
    app.web_cmd_rx = Some(cmd_rx);

    cmd_tx.send(WebCommand::ShuffleQueue).unwrap();
    app.handle_web_commands();
    assert_eq!(app.queue.current_index(), 0);
    assert_eq!(
        app.queue.current_track().map(|t| t.filepath.clone()),
        Some(tracks[0].clone()),
        "the stream's next track is kept in front, not the one that ended"
    );

    if let Some(handle) = app.web_server.take() {
        handle.stream_state().close_active_streams();
        handle.stop();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A skip or seek asked while the queue is empty is dropped: the next queue starts at its top,
/// from its start, not at a stale index or offset (#106).
#[cfg(unix)]
#[test]
fn test_continuous_request_made_while_the_queue_is_empty_is_dropped() {
    common::isolate();
    let f = DecoderFixture::start("empty_queue_request", &[], 0);
    std::thread::sleep(Duration::from_millis(200));
    f.controller.skip_to_index(5);
    f.controller.seek_to(40.0);
    wait_until("the request stayed pending", || {
        f.controller.pending_request().is_none()
    });

    let tracks: Vec<PathBuf> = (0..6).map(|i| f.dir.join(format!("t{}.wav", i))).collect();
    for track in &tracks {
        std::fs::write(track, b"never opened: the fake decoder makes up its output").unwrap();
    }
    f.state.write().unwrap().queue = tracks.iter().enumerate().map(|(i, p)| queue_item(i, p)).collect();
    assert_eq!(f.run(1), (tracks[0].clone(), None));
    assert_eq!(*f.controller.current_track_index.lock().unwrap(), 0);
}

/// A renderer that reconnects to its session supersedes the old connection: the new connection's
/// decoder takes the old one's place without leaving it running, the old connection closes, and
/// a skip then stops the new connection's decoder.
#[cfg(unix)]
#[test]
fn test_continuous_reconnect_stops_the_old_connections_decoder() {
    common::isolate();
    let f = DecoderFixture::start("reconnect", &["t0.wav", "t1.wav"], 0);
    f.wait_for_startup(1);
    let old_decoder = f.decoder.pid(1);

    let second = Renderer::connect(f.port, &f.path);
    assert_eq!(f.wait_for_startup(2), (f.tracks[0].clone(), None));
    wait_until("the old connection's decoder was left running", || {
        !pid_alive(&old_decoder)
    });
    assert!(
        f.renderer.as_ref().unwrap().closed_within(Duration::from_secs(3)),
        "the old connection was not closed"
    );

    let new_decoder = f.decoder.pid(2);
    f.controller.skip_to_index(1);
    assert_eq!(f.wait_for_startup(3), (f.tracks[1].clone(), None));
    wait_until("the skip didn't stop the new connection's decoder", || {
        !pid_alive(&new_decoder)
    });

    f.controller.kill_active_decoder();
    if let Some(handle) = f.handle.as_ref() {
        handle.stream_state().close_active_streams();
    }
    second.hang_up();
}

/// While the stream idles on a queue that gives no audio, the files it tries again once a
/// second are not shown as playing: the stream's track index and the web state stay on the
/// track that was playing or asked for (#107).
#[cfg(unix)]
#[test]
fn test_continuous_idle_rechecks_do_not_step_now_playing_through_the_queue() {
    common::isolate();
    let f = DecoderFixture::start("idle_now_playing", &["a_bad.wav", "b_bad.wav", "c_bad.wav"], 0);
    f.run(3);
    std::thread::sleep(Duration::from_millis(300));
    let shown = || {
        let s = f.state.read().unwrap();
        let index = *f.controller.current_track_index.lock().unwrap();
        (index, s.queue_index, s.title.clone())
    };
    let before = shown();
    assert_eq!(before, (0, 0, "Track 0".to_string()));
    let runs = f.decoder.calls().len();
    std::thread::sleep(Duration::from_millis(2500));
    assert!(f.decoder.calls().len() >= runs + 2, "the stream kept trying");
    assert_eq!(shown(), before, "a file tried again is not playing");
}

/// Without a decoder to run (ffmpeg not installed) the stream sends silence, logs the failure
/// once rather than on every try, and doesn't step now playing through the queue (#107).
#[cfg(unix)]
#[test]
fn test_continuous_stream_without_a_decoder_logs_the_failure_once() {
    common::isolate();
    quickstream::logger::init();
    let dir = temp_dir("no_decoder");
    let tracks = [dir.join("a.wav"), dir.join("b.wav")];
    for track in &tracks {
        std::fs::write(track, b"never opened").unwrap();
    }
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    state.write().unwrap().queue = tracks.iter().enumerate().map(|(i, p)| queue_item(i, p)).collect();
    let (handle, _rx) = start(state.clone());
    let controller = handle.stream_state().continuous.clone();
    let missing = dir.join("no-such-ffmpeg");
    controller.set_decoder(&missing);
    let path = handle.stream_state().arm_continuous_session();

    let renderer = Renderer::connect(handle.port, &path);
    std::thread::sleep(Duration::from_millis(300));
    assert_about_real_time(renderer.rate_over(Duration::from_millis(2500)), "no decoder");
    assert_eq!(
        log_lines_naming(&missing.display().to_string()),
        1,
        "the spawn failure is logged once"
    );
    assert_eq!(*controller.current_track_index.lock().unwrap(), 0);
    assert_eq!(state.read().unwrap().queue_index, 0);

    handle.stream_state().close_active_streams();
    renderer.hang_up();
    handle.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A queue whose files are all missing is logged once, and so is a queue that replaces it, also
/// one of the same length (#107).
#[cfg(unix)]
#[test]
fn test_continuous_stream_logs_each_missing_queue_once() {
    common::isolate();
    quickstream::logger::init();
    let dir = temp_dir("missing_queues");
    let decoder = FakeDecoder::new(&dir);
    let (first, second) = (dir.join("gone1"), dir.join("gone2"));
    let queue = |folder: &Path| -> Vec<WebQueueItem> {
        ["a.wav", "b.wav"]
            .iter()
            .enumerate()
            .map(|(i, n)| queue_item(i, &folder.join(n)))
            .collect()
    };
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    state.write().unwrap().queue = queue(&first);
    let (handle, _rx) = start(state.clone());
    handle.stream_state().continuous.set_decoder(&decoder.binary);
    let path = handle.stream_state().arm_continuous_session();
    let renderer = Renderer::connect(handle.port, &path);

    let first_name = first.display().to_string();
    wait_until("the first queue was never logged", || log_lines_naming(&first_name) > 0);
    state.write().unwrap().queue = queue(&second);
    let second_name = second.display().to_string();
    wait_until("the queue that replaced it was never logged", || {
        log_lines_naming(&second_name) > 0
    });
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(log_lines_naming(&first_name), 2, "each missing file once");
    assert_eq!(log_lines_naming(&second_name), 2, "each missing file once");

    // A replacement followed at once by a skip (as `play_current_track` sends one) is logged too.
    let third = dir.join("gone3");
    state.write().unwrap().queue = queue(&third);
    handle.stream_state().continuous.skip_to_index(0);
    let third_name = third.display().to_string();
    wait_until("the queue replaced before a skip was never logged", || {
        log_lines_naming(&third_name) > 0
    });
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(log_lines_naming(&third_name), 2, "each missing file once");
    assert!(decoder.calls().is_empty(), "nothing to decode");

    handle.stream_state().close_active_streams();
    renderer.hang_up();
    handle.stop();
    drop(decoder);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file check standing in for a network share that hangs rather than unmounting: checks of the
/// paths it holds block until it is released, the others answer from the disk. Every check is
/// counted per path.
#[derive(Clone, Default)]
struct HeldPathCheck {
    released: Arc<(Mutex<bool>, std::sync::Condvar)>,
    calls: Arc<Mutex<std::collections::HashMap<PathBuf, usize>>>,
}

/// Releases a [`HeldPathCheck`] when dropped, so no check (nor, without the fix, the stream
/// blocked in one) outlives the test, even one that fails.
struct ReleaseOnDrop(HeldPathCheck);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

impl HeldPathCheck {
    /// Check files on `controller` through this, holding `held`.
    fn install(&self, controller: &ContinuousStreamController, held: &[PathBuf]) {
        let (released, calls, held) = (self.released.clone(), self.calls.clone(), held.to_vec());
        controller.set_path_check(move |path: &Path| {
            *calls.lock().unwrap().entry(path.to_path_buf()).or_default() += 1;
            if held.iter().any(|h| h == path) {
                let (lock, cvar) = &*released;
                let mut done = lock.lock().unwrap();
                while !*done {
                    done = cvar.wait(done).unwrap();
                }
            }
            path.exists()
        });
    }

    fn release(&self) {
        let (lock, cvar) = &*self.released;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }

    fn release_on_drop(&self) -> ReleaseOnDrop {
        ReleaseOnDrop(self.clone())
    }

    fn calls(&self, path: &Path) -> usize {
        self.calls.lock().unwrap().get(path).copied().unwrap_or(0)
    }
}

/// While the check of the next file hangs (a network share that hangs), the renderer still gets
/// silence at real time, no more than the lead ahead; once the check answers, the file plays
/// (#117).
#[cfg(unix)]
#[test]
fn test_continuous_stream_sends_silence_while_a_file_check_hangs() {
    common::isolate();
    let hang = HeldPathCheck::default();
    let f = DecoderFixture::start_with("hung_check", &["t0_live.wav", "t1_live.wav"], 0, |c, tracks| {
        hang.install(c, &tracks[..1])
    });
    let _release = hang.release_on_drop();
    let connected = Instant::now();
    std::thread::sleep(Duration::from_millis(300));
    let renderer = f.renderer.as_ref().unwrap();
    assert_about_real_time(
        renderer.rate_over(Duration::from_millis(1200)),
        "while the file check hangs",
    );
    let received = renderer.received.load(Ordering::SeqCst) as f64;
    let lead = 0.1 * REAL_TIME_BYTES_PER_SEC;
    assert!(
        received <= (connected.elapsed().as_secs_f64() + 0.1) * REAL_TIME_BYTES_PER_SEC + lead,
        "silence runs at most the lead ahead of real time: {:.0} bytes after {:?}",
        received,
        connected.elapsed()
    );
    assert!(f.decoder.calls().is_empty(), "nothing decoded while the check hangs");
    assert_eq!(hang.calls(&f.tracks[0]), 1, "one check of the file, not one per pass");

    hang.release();
    assert_eq!(f.run(1), (f.tracks[0].clone(), None));
    wait_until("the file was never shown as playing", || {
        f.state.read().unwrap().title == "Track 0"
    });
    assert_eq!(*f.controller.current_track_index.lock().unwrap(), 0);
}

/// A seek asked while the stream waits on a file check stays pending, and is applied to that
/// file once its check answers; a skip asked then plays the track skipped to (#117).
#[cfg(unix)]
#[test]
fn test_continuous_requests_made_while_a_file_check_hangs_are_kept() {
    common::isolate();
    let hang = HeldPathCheck::default();
    // Four tracks, so the check of the file after the one skipped to (made as it starts) is not
    // of the first.
    let names = ["t0_live.wav", "t1.wav", "t2.wav", "t3.wav"];
    let f = DecoderFixture::start_with("hung_request", &names, 0, |c, tracks| hang.install(c, &tracks[..1]));
    let _release = hang.release_on_drop();
    wait_until("the file was never checked", || hang.calls(&f.tracks[0]) == 1);
    f.controller.seek_to(12.0);
    std::thread::sleep(Duration::from_millis(300));
    let seek = StreamRequest {
        index: 0,
        position: 12.0,
    };
    assert_eq!(f.controller.pending_request(), Some(seek), "kept while the check hangs");
    hang.release();
    assert_eq!(f.run(1), (f.tracks[0].clone(), Some("12.000".to_string())));
    wait_until("the seek was never taken", || f.controller.pending_request().is_none());

    f.controller.skip_to_index(2);
    assert_eq!(f.run(2), (f.tracks[2].clone(), None));
    assert_eq!(hang.calls(&f.tracks[0]), 1, "a file checked since is not checked again");
}

/// A file whose check doesn't answer in time is passed over for the next one, which plays
/// (#117).
#[cfg(unix)]
#[test]
fn test_continuous_stream_plays_the_next_file_while_a_check_hangs() {
    common::isolate();
    let hang = HeldPathCheck::default();
    let f = DecoderFixture::start_with("hung_first", &["t0_live.wav", "t1_live.wav"], 0, |c, tracks| {
        hang.install(c, &tracks[..1])
    });
    let _release = hang.release_on_drop();
    assert_eq!(f.run(1), (f.tracks[1].clone(), None));
    wait_until("the next file was never shown as playing", || {
        *f.controller.current_track_index.lock().unwrap() == 1
    });
    assert_eq!(hang.calls(&f.tracks[0]), 1);
}

/// Checks that never answer (the share stays hung) don't pile up: one is in flight per path,
/// however often the stream looks through the queue, also after the renderer reconnects; the
/// stream sends silence at real time all along (#117).
#[cfg(unix)]
#[test]
fn test_continuous_hung_file_checks_run_once_per_path() {
    common::isolate();
    let hang = HeldPathCheck::default();
    let f = DecoderFixture::start_with("hung_all", &["t0_live.wav", "t1_live.wav"], 0, |c, tracks| {
        hang.install(c, tracks)
    });
    let _release = hang.release_on_drop();
    std::thread::sleep(Duration::from_millis(300));
    assert_about_real_time(
        f.renderer.as_ref().unwrap().rate_over(Duration::from_millis(1200)),
        "every check hangs",
    );
    // Past both checks' deadlines and a few rechecks of the queue.
    std::thread::sleep(Duration::from_millis(5500));
    for track in &f.tracks {
        assert_eq!(hang.calls(track), 1, "one check in flight for {}", track.display());
    }

    let again = Renderer::connect(f.port, &f.path);
    std::thread::sleep(Duration::from_millis(300));
    assert_about_real_time(again.rate_over(Duration::from_millis(1500)), "after a reconnect");
    for track in &f.tracks {
        assert_eq!(
            hang.calls(track),
            1,
            "no new check for {} after a reconnect",
            track.display()
        );
    }
    assert!(f.decoder.calls().is_empty(), "nothing decoded");
    again.hang_up();
}

/// While the stream idles on a queue whose files are all missing (the share was unmounted), its
/// rechecks reuse the helper threads that answered before rather than starting one per file per
/// recheck, and the silence stays at real time (#117).
#[cfg(unix)]
#[test]
fn test_continuous_missing_file_rechecks_reuse_their_threads() {
    common::isolate();
    let dir = temp_dir("recheck_threads");
    let decoder = FakeDecoder::new(&dir);
    let missing: Vec<PathBuf> = (0..20).map(|i| dir.join(format!("gone/{i:02}.wav"))).collect();
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    state.write().unwrap().queue = missing.iter().enumerate().map(|(i, p)| queue_item(i, p)).collect();
    let (handle, _rx) = start(state.clone());
    let controller = handle.stream_state().continuous.clone();
    controller.set_decoder(&decoder.binary);
    let threads = Arc::new(Mutex::new(Vec::new()));
    let threads_c = threads.clone();
    controller.set_path_check(move |path: &Path| {
        threads_c.lock().unwrap().push(std::thread::current().id());
        path.exists()
    });
    let path = handle.stream_state().arm_continuous_session();

    let renderer = Renderer::connect(handle.port, &path);
    std::thread::sleep(Duration::from_millis(300));
    assert_about_real_time(renderer.rate_over(Duration::from_millis(3500)), "every file missing");
    let checks = threads.lock().unwrap().clone();
    let mut distinct = checks.clone();
    distinct.sort_by_key(|id| format!("{id:?}"));
    distinct.dedup();
    assert!(
        checks.len() >= 3 * missing.len(),
        "a few rechecks: {} checks",
        checks.len()
    );
    assert!(
        distinct.len() <= 4,
        "{} checks ran on {} threads, not on the few that answered before",
        checks.len(),
        distinct.len()
    );
    assert!(decoder.calls().is_empty(), "nothing to decode");

    handle.stream_state().close_active_streams();
    renderer.hang_up();
    handle.stop();
    drop(decoder);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file check that takes `delay` for the paths it slows (a slow but working share), and
/// answers from the disk at once for the others. Every check is counted per path, as it starts
/// and as it answers.
#[derive(Clone, Default)]
struct SlowPathCheck {
    started: Arc<Mutex<std::collections::HashMap<PathBuf, usize>>>,
    answered: Arc<Mutex<std::collections::HashMap<PathBuf, usize>>>,
}

impl SlowPathCheck {
    fn install(&self, controller: &ContinuousStreamController, slow: &[PathBuf], delay: Duration) {
        let (started, answered, slow) = (self.started.clone(), self.answered.clone(), slow.to_vec());
        controller.set_path_check(move |path: &Path| {
            *started.lock().unwrap().entry(path.to_path_buf()).or_default() += 1;
            if slow.iter().any(|s| s == path) {
                std::thread::sleep(delay);
            }
            let exists = path.exists();
            *answered.lock().unwrap().entry(path.to_path_buf()).or_default() += 1;
            exists
        });
    }

    fn started(&self, path: &Path) -> usize {
        self.started.lock().unwrap().get(path).copied().unwrap_or(0)
    }

    fn answered(&self, path: &Path) -> usize {
        self.answered.lock().unwrap().get(path).copied().unwrap_or(0)
    }
}

/// A renderer that keeps every byte of the stream after the WAV header.
struct RecordingRenderer {
    bytes: Arc<Mutex<Vec<u8>>>,
    reading: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RecordingRenderer {
    fn connect(port: u16, path: &str) -> Self {
        let (status, mut reader) = open(port, path);
        assert_eq!(status, 200);
        let mut wav_hdr = [0u8; 44];
        reader.read_exact(&mut wav_hdr).unwrap();
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let reading = Arc::new(AtomicBool::new(true));
        let (bytes_t, reading_t) = (bytes.clone(), reading.clone());
        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while reading_t.load(Ordering::SeqCst) {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => bytes_t.lock().unwrap().extend_from_slice(&buf[..n]),
                }
            }
        });
        Self {
            bytes,
            reading,
            thread: Some(thread),
        }
    }

    fn hang_up(mut self) {
        self.reading.store(false, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Stream `first` (`ones` or `burst`: `0x01` bytes, then its end) and then `second` (`twos`:
/// `0x02` bytes), the second's check taking 150 ms; return the run of bytes the renderer got
/// right before the first `0x02` byte, which must all be the first track's.
#[cfg(unix)]
fn bytes_before_the_second_track(name: &str, first: &str, second: &str) -> (usize, usize) {
    let slow = SlowPathCheck::default();
    let f = DecoderFixture::armed(name, &[first, second], 0, |c, tracks| {
        slow.install(c, &tracks[1..], Duration::from_millis(150))
    });
    let renderer = RecordingRenderer::connect(f.port, &f.path);
    wait_until("the second track never played", || {
        renderer.bytes.lock().unwrap().contains(&2)
    });
    let bytes = renderer.bytes.lock().unwrap().clone();
    // The second track sends nothing more, so the stream is closed for the reader to finish.
    f.handle.as_ref().unwrap().stream_state().close_active_streams();
    renderer.hang_up();
    let calls = f.decoder.calls();
    assert!(
        calls.iter().any(|c| c.contains(second)),
        "the second track was decoded: {:?}",
        calls
    );
    let at = bytes.iter().position(|&b| b == 2).unwrap();
    let ones = bytes[..at].iter().rev().take_while(|&&b| b == 1).count();
    (ones, at)
}

/// The next file is checked as soon as the track before it starts, so a slow but working share
/// (a check that takes 150 ms) has answered by the time the track ends, and the next track's PCM
/// follows the last byte of the one before with no silence between them (#117).
#[cfg(unix)]
#[test]
fn test_continuous_next_file_is_checked_while_the_track_before_plays() {
    common::isolate();
    let (ones, at) = bytes_before_the_second_track("check_ahead", "a_ones.wav", "b_twos.wav");
    assert_eq!(
        ones,
        5 * 17640,
        "the first track's bytes, then the second's at once (nothing between them, {} bytes before)",
        at
    );
}

/// A file check still answering as a track ends (150 ms on a slow share) writes no silence while
/// the renderer is still well ahead of real time on the track before: the next track's PCM
/// follows its last byte (#117).
#[cfg(unix)]
#[test]
fn test_continuous_slow_check_at_a_track_end_writes_no_silence_while_the_renderer_is_ahead() {
    common::isolate();
    let (ones, at) = bytes_before_the_second_track("check_lead", "a_burst.wav", "b_twos.wav");
    assert_eq!(
        ones, 176_400,
        "the first track's bytes, then the second's at once (nothing between them, {} bytes before)",
        at
    );
}

/// A check that answers after its deadline (2.5 s on a slow but working share) still counts: the
/// file is passed over while it is unanswered, and played when it is asked for again, rather than
/// checked again only to miss the deadline once more (#117).
#[cfg(unix)]
#[test]
fn test_continuous_late_file_check_answer_counts_for_the_next_look() {
    common::isolate();
    let slow = SlowPathCheck::default();
    let f = DecoderFixture::start_with(
        "late_answer",
        &["t0_live.wav", "t1_live.wav", "t2_live.wav"],
        0,
        |c, t| slow.install(c, &t[2..], Duration::from_millis(2500)),
    );
    assert_eq!(f.run(1), (f.tracks[0].clone(), None));
    f.controller.skip_to_index(2);
    assert_eq!(f.run(2), (f.tracks[0].clone(), None), "passed over while unanswered");
    wait_until("the slow check never answered", || slow.answered(&f.tracks[2]) == 1);
    f.controller.skip_to_index(2);
    assert_eq!(f.run(3), (f.tracks[2].clone(), None), "played on its late answer");
    assert_eq!(slow.started(&f.tracks[2]), 1, "not checked again");
}

/// Files whose checks hang are checked at the same time, not one deadline after another, so a
/// file that answers later in the queue plays within about one deadline (#117).
#[cfg(unix)]
#[test]
fn test_continuous_files_after_hung_checks_are_checked_ahead() {
    common::isolate();
    let hang = HeldPathCheck::default();
    let mut names: Vec<String> = (0..12).map(|i| format!("h{i:02}_live.wav")).collect();
    names.push("z_live.wav".to_string());
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let f = DecoderFixture::start_with("hung_many", &names, 0, |c, tracks| hang.install(c, &tracks[..12]));
    let _release = hang.release_on_drop();
    assert_eq!(f.run(1), (f.tracks[12].clone(), None), "the file that answers plays");
}

/// Checks that never answer hold at most a few helper threads, however long the queue: a file
/// past them waits for a free helper (and is passed over meanwhile), and is checked once one
/// frees (#117).
#[cfg(unix)]
#[test]
fn test_continuous_hung_file_checks_hold_a_bounded_number_of_threads() {
    common::isolate();
    let hang = HeldPathCheck::default();
    let names: Vec<String> = (0..40).map(|i| format!("h{i:02}_live.wav")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let f = DecoderFixture::start_with("hung_cap", &names, 0, |c, tracks| hang.install(c, tracks));
    let _release = hang.release_on_drop();
    let total = || f.tracks.iter().map(|t| hang.calls(t)).sum::<usize>();
    std::thread::sleep(Duration::from_millis(300));
    assert_about_real_time(
        f.renderer.as_ref().unwrap().rate_over(Duration::from_millis(1200)),
        "every check hangs",
    );
    // Long past the deadlines of a whole queue's worth of checks ahead.
    std::thread::sleep(Duration::from_millis(6000));
    let held = total();
    assert!(
        (1..=16).contains(&held),
        "{} checks hung at once, one helper thread each",
        held
    );
    assert_about_real_time(
        f.renderer.as_ref().unwrap().rate_over(Duration::from_millis(1000)),
        "still every check hangs",
    );
    hang.release();
    wait_until("the files past the hung checks were never checked", || {
        hang.calls(&f.tracks[39]) >= 1
    });
}

/// A seek to a position that isn't a number starts the track from 0, and the stream doesn't spin
/// on it (a NaN never equals itself) (#117).
#[cfg(unix)]
#[test]
fn test_continuous_nan_seek_starts_from_zero() {
    common::isolate();
    let hang = HeldPathCheck::default();
    let f = DecoderFixture::start_with("nan_seek", &["t0_live.wav", "t1_live.wav"], 0, |c, tracks| {
        hang.install(c, &tracks[..1])
    });
    let _release = hang.release_on_drop();
    wait_until("the file was never checked", || hang.calls(&f.tracks[0]) == 1);
    f.controller.seek_to(f64::NAN);
    assert_eq!(
        f.controller.pending_request(),
        Some(StreamRequest {
            index: 0,
            position: 0.0
        })
    );
    hang.release();
    assert_eq!(f.run(1), (f.tracks[0].clone(), None));
    wait_until("the seek was never taken", || f.controller.pending_request().is_none());
    f.controller.seek_to(-3.0);
    assert_eq!(f.run(2), (f.tracks[0].clone(), None), "a negative seek starts from 0");
}

/// A request made while a look through the queue waits on a hung check starts a fresh look: a
/// file found missing for the request before (the share remounted since) is checked again and
/// plays (#117).
#[cfg(unix)]
#[test]
fn test_continuous_new_request_during_a_look_checks_files_afresh() {
    common::isolate();
    let hang = HeldPathCheck::default();
    let f = DecoderFixture::start_with(
        "fresh_look",
        &["t0_live.wav", "t1_live.wav", "t2_live.wav"],
        0,
        |c, t| {
            std::fs::remove_file(&t[1]).unwrap();
            hang.install(c, &t[2..])
        },
    );
    let _release = hang.release_on_drop();
    assert_eq!(f.run(1), (f.tracks[0].clone(), None));
    f.controller.skip_to_index(1);
    wait_until("the look never reached the hung file", || hang.calls(&f.tracks[2]) == 1);
    std::fs::write(&f.tracks[1], b"back").unwrap();
    f.controller.skip_to_index(1);
    wait_until("the file back on the share never played", || {
        f.decoder.calls().iter().any(|c| c.contains("t1_live"))
    });
}
