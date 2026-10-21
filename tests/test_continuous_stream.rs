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
use quickstream::queue::RowMap;
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
    open_on(TcpStream::connect(("127.0.0.1", port)).unwrap(), port, path)
}

/// As [`open`], on a connection already made to the web server at `port`.
fn open_on(mut stream: TcpStream, port: u16, path: &str) -> (u16, BufReader<TcpStream>) {
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
        Ok(WebCommand::StreamReshuffle) => {}
        other => panic!("expected StreamReshuffle after the queue wrapped, got {:?}", other),
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
            assert_eq!(cmd, WebCommand::StreamReshuffle);
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
/// gets 100 ms of PCM and ends 0.5 s later, one with `long` 3 s later; one with `endless` gets PCM as fast as it is read
/// until it is killed (for a renderer that stops reading); one with `live` gets PCM at about real
/// time until it is killed (for one that keeps reading, so it doesn't load the machine); one with
/// `bad` gets nothing and an exit at once (a file ffmpeg can't decode); `ones` gets 0.5 s of
/// `0x01` bytes at about real time (`longones` 3 s) and `burst` 1 s of them at once, then an exit
/// (a track that ends); `twos` gets 0.1 s of `0x02` bytes, then nothing until killed; `waking` gets
/// nothing for 2 s (a share that hangs as ffmpeg opens the file), then `0x01` bytes at about real
/// time until it is killed; `stall` gets 0.5 s of `0x01` bytes at about real time, then nothing
/// for 2.5 s (a share that hangs as ffmpeg reads the file), then `0x01` bytes at about real time
/// until it is killed; `flood` gets 5 s of `0x01` bytes at once, then an exit; `surge` gets 6 s of
/// them at once, nothing for 1.2 s, then 5 s of them at about real time, then an exit; `sleepy` gets
/// nothing for 1 s, then `0x02` bytes at about real time until it is killed; `lone` gets a lone
/// 32 KiB chunk of `0x01` bytes (0.19 s) every 1.2 s until it is killed; any other input gets
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
            "#!/bin/sh\necho $$ >> '{pids}'\nin=''\nprev=''\nfor a in \"$@\"; do [ \"$prev\" = '-i' ] && in=\"$a\"; prev=\"$a\"; done\nprintf '%s\\n' \"$*\" >> '{log}'\ncase \"${{in##*/}}\" in\n  *lone*) while :; do head -c 32768 /dev/zero | tr '\\000' '\\001'; sleep 1.2; done ;;\n  *short*) head -c 17640 /dev/zero; exec sleep 0.5 ;;\n  *longones*) for i in $(seq 30); do head -c 17640 /dev/zero | tr '\\000' '\\001'; sleep 0.1; done; exit 0 ;;\n  *long*) head -c 17640 /dev/zero; exec sleep 3 ;;\n  *endless*) exec cat /dev/zero ;;\n  *live*) while :; do head -c 17640 /dev/zero; sleep 0.1; done ;;\n  *late*) sleep 1; while :; do head -c 17640 /dev/zero; sleep 0.1; done ;;\n  *bad*) exit 1 ;;\n  *ones*) for i in 1 2 3 4 5; do head -c 17640 /dev/zero | tr '\\000' '\\001'; sleep 0.1; done; exit 0 ;;\n  *burst*) head -c 176400 /dev/zero | tr '\\000' '\\001'; exit 0 ;;\n  *twos*) head -c 17640 /dev/zero | tr '\\000' '\\002'; exec sleep 30 ;;\n  *waking*) sleep 2; while :; do head -c 17640 /dev/zero | tr '\\000' '\\001'; sleep 0.1; done ;;\n  *stall*) for i in 1 2 3 4 5; do head -c 17640 /dev/zero | tr '\\000' '\\001'; sleep 0.1; done; sleep 2.5; while :; do head -c 17640 /dev/zero | tr '\\000' '\\001'; sleep 0.1; done ;;\n  *surge*) head -c 1058400 /dev/zero | tr '\\000' '\\001'; sleep 1.2; for i in $(seq 50); do head -c 17640 /dev/zero | tr '\\000' '\\001'; sleep 0.1; done; exit 0 ;;\n  *flood*) head -c 882000 /dev/zero | tr '\\000' '\\001'; exit 0 ;;\n  *sleepy*) sleep 1; while :; do head -c 17640 /dev/zero | tr '\\000' '\\002'; sleep 0.1; done ;;\nesac\nexec sleep 30\n",
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

    /// Wait for run `n` (from 1) and return its input and `-ss` offset (`None`: from the start).
    fn run(&self, n: usize) -> (PathBuf, Option<String>) {
        let calls = self.wait_for_calls(n);
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
fn wait_until(what: &str, done: impl FnMut() -> bool) {
    wait_within(Duration::from_secs(10), what, done)
}

/// Wait up to `timeout` for `done`.
fn wait_within(timeout: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
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
        Self::connected(Self::armed_editing(name, names, first, |_| {}, setup))
    }

    /// As [`Self::start`], with `edit` applied to the published queue before the stream starts.
    fn start_editing(name: &str, names: &[&str], first: usize, edit: impl FnOnce(&mut Vec<WebQueueItem>)) -> Self {
        Self::connected(Self::armed_editing(name, names, first, edit, |_, _| {}))
    }

    fn connected(mut f: Self) -> Self {
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
        Self::armed_editing(name, names, first, |_| {}, setup)
    }

    /// As [`Self::armed`], with `edit` applied to the published queue first.
    fn armed_editing(
        name: &str,
        names: &[&str],
        first: usize,
        edit: impl FnOnce(&mut Vec<WebQueueItem>),
        setup: impl FnOnce(&ContinuousStreamController, &[PathBuf]),
    ) -> Self {
        let dir = temp_dir(name);
        let decoder = FakeDecoder::new(&dir);
        let tracks: Vec<PathBuf> = names.iter().map(|n| dir.join(n)).collect();
        for track in &tracks {
            std::fs::write(track, b"never opened: the fake decoder makes up its output").unwrap();
        }
        let state = Arc::new(RwLock::new(NowPlayingState::default()));
        {
            let mut s = state.write().unwrap();
            s.queue = tracks.iter().enumerate().map(|(i, p)| queue_item(i, p)).collect();
            edit(&mut s.queue);
        }
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
        self.decoder.run(n)
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

/// `up_next` indices, `up_next_remaining`, `up_next_end` and `next_track_title`.
#[cfg(unix)]
type UpNext = (Vec<usize>, usize, Option<quickstream::queue::QueueEnd>, Option<String>);

#[cfg(unix)]
fn published_up_next(state: &NowPlayingState) -> UpNext {
    (
        state.up_next.iter().map(|item| item.index).collect(),
        state.up_next_remaining,
        state.up_next_end,
        state.next_track_title.clone(),
    )
}

/// The stream publishes Up Next with each track it starts, in the same write as the track and
/// through the UI's own `publish_up_next` (wrapping, as the continuous stream plays on), so
/// `/api/status` never lists the track now playing as the next one between the stream's write
/// and the UI's next tick, and the UI's tick republishes the same values. A queue the UI has
/// shortened since the stream took its copy is listed as published (#104).
#[cfg(unix)]
#[test]
fn test_continuous_stream_publishes_up_next_with_its_track() {
    common::isolate();
    use quickstream::queue::QueueEnd;
    let names = ["t0_live.wav", "t1_live.wav", "t2_live.wav", "t3_live.wav"];
    let f = DecoderFixture::start("up_next_stream", &names, 0);
    wait_until("the stream never published track 0", || {
        f.state.read().unwrap().title == "Track 0"
    });
    // The UI's tick for track 0.
    f.state.write().unwrap().publish_up_next(true);
    assert_eq!(
        published_up_next(&f.state.read().unwrap()),
        (
            vec![1, 2, 3],
            3,
            Some(QueueEnd::Reshuffles),
            Some("Track 1".to_string())
        )
    );

    f.controller.skip_to_index(2);
    wait_until("the stream never published track 2", || {
        f.state.read().unwrap().title == "Track 2"
    });
    {
        let s = f.state.read().unwrap();
        assert_eq!(
            published_up_next(&s),
            (vec![3], 1, Some(QueueEnd::Reshuffles), Some("Track 3".to_string()))
        );
        assert_ne!(s.next_track_title.as_deref(), Some(s.title.as_str()));
        let mut ui = s.clone();
        ui.publish_up_next(true);
        assert_eq!(published_up_next(&ui), published_up_next(&s), "the UI's tick agrees");
    }

    // The UI drops the last two tracks from the queue it publishes; the stream starts track 1.
    f.state.write().unwrap().queue.truncate(2);
    f.controller.skip_to_index(1);
    wait_until("the stream never published track 1", || {
        f.state.read().unwrap().title == "Track 1"
    });
    let s = f.state.read().unwrap();
    assert_eq!(published_up_next(&s), (vec![], 0, Some(QueueEnd::Reshuffles), None));
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

/// A queue of one track: at its end the stream plays that track again from its start and asks
/// for no reshuffle (there is nothing to reorder), and Up Next says it repeats, as tab [1] and
/// the dashboard do (#134).
#[cfg(unix)]
#[test]
fn test_continuous_single_track_repeats_without_a_reshuffle() {
    common::isolate();
    use quickstream::queue::QueueEnd;
    let f = DecoderFixture::start("single_track", &["t0_short.wav"], 0);
    assert_eq!(f.run(1), (f.tracks[0].clone(), None));
    assert_eq!(f.run(2), (f.tracks[0].clone(), None));
    assert_eq!(f.run(3), (f.tracks[0].clone(), None));
    if let Ok(cmd) = f.rx.recv_timeout(Duration::from_millis(300)) {
        panic!("a one-track queue's wrap sent {:?}", cmd);
    }
    assert_eq!(*f.controller.current_track_index.lock().unwrap(), 0);
    assert_eq!(
        published_up_next(&f.state.read().unwrap()),
        (vec![], 0, Some(QueueEnd::Loops), Some("Track 0".to_string()))
    );
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
    // Track 1's file check is held while track 0 ends, so the stream has decided that end but
    // waits to start the next track. (Holding the web state would hold the end itself, which
    // reads the queue as it is then.)
    let hang = HeldPathCheck::default();
    let _release = hang.release_on_drop();
    let f = DecoderFixture::start_with(
        "seek_after_end",
        &["t0_short.wav", "t1.wav", "t2.wav"],
        0,
        |c, tracks| hang.install(c, &tracks[1..2]),
    );
    assert_eq!(f.run(1), (f.tracks[0].clone(), None));

    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(f.decoder.calls().len(), 1, "the stream waits for track 1's check");
    f.controller.seek_to(40.0);
    // Within the check's deadline, so track 1 isn't passed over.
    hang.release();
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
    // t0's file check is held while t1 ends, so the stream waits to start the next track until
    // the UI has acted on the reshuffle. (Holding the web state would hold the end itself, which
    // reads the queue as it is then.)
    let hang = HeldPathCheck::default();
    let _release = hang.release_on_drop();
    let f = DecoderFixture::start_with("wrap_front", &["t0.wav", "t1_short.wav"], 1, |c, tracks| {
        hang.install(c, &tracks[..1])
    });
    assert_eq!(f.run(1), (f.tracks[1].clone(), None));

    match f.rx.recv_timeout(Duration::from_secs(10)) {
        Ok(WebCommand::StreamReshuffle) => {}
        other => panic!("expected StreamReshuffle after the queue wrapped, got {:?}", other),
    }
    {
        // What one UI tick does with the reshuffle: follow the stream's index, shuffle with that
        // track in front, publish.
        let mut held = f.state.write().unwrap();
        let front = *f.controller.current_track_index.lock().unwrap();
        let kept = held.queue.remove(front);
        held.queue.insert(0, kept);
    }
    // A few of the stream's passes later, so none that copied the queue before the shuffle is
    // still looking, and within the check's deadline, so t0 isn't passed over.
    std::thread::sleep(Duration::from_millis(200));
    hang.release();
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

    cmd_tx.send(WebCommand::StreamReshuffle).unwrap();
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

/// `z` while the continuous stream plays: the shuffle puts the playing track in front, and the
/// stream's index goes with it, so the event loop's sync doesn't move the queue back to the row
/// the track had before (PR #127 review).
#[test]
fn test_shuffle_while_the_continuous_stream_plays_keeps_its_track_current() {
    common::isolate();
    let dir = temp_dir("shuffle_stream_index");
    let tracks: Vec<PathBuf> = (0..8).map(|i| dir.join(format!("t{}.wav", i))).collect();
    let settings = SettingsManager::with_path(dir.join("config.json"));
    let mut app = AppUI::new(Some(dir.clone()), settings);
    app.queue.add_tracks(
        tracks
            .iter()
            .enumerate()
            .map(|(i, p)| track_meta(p, &format!("Track {}", i), 60.0))
            .collect(),
    );
    app.queue.set_current_index(5);
    let (tx, _web_rx) = mpsc::channel();
    let handle = WebServer::start(0, app.web_state.clone(), tx).expect("start web server");
    let controller = handle.stream_state().continuous.clone();
    controller.is_active.store(true, Ordering::SeqCst);
    *controller.current_track_index.lock().unwrap() = 5;
    app.web_server = Some(handle);
    app.update_web_state();

    // A skip the stream hasn't acted on yet names a track by its row too.
    controller.skip_to_index(6);
    controller.skip_to_index(5);
    let (cmd_tx, cmd_rx) = mpsc::channel();
    app.web_cmd_rx = Some(cmd_rx);
    for round in 0..4 {
        if round % 2 == 0 {
            app.handle_input_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('z'),
                crossterm::event::KeyModifiers::NONE,
            ))
            .unwrap();
        } else {
            cmd_tx.send(WebCommand::ShuffleQueue).unwrap();
            app.handle_web_commands();
        }
        assert_eq!(*controller.current_track_index.lock().unwrap(), 0, "round {round}");
        assert_eq!(
            controller.pending_request().map(|r| r.index),
            Some(0),
            "round {round}: the pending skip follows its track"
        );
        app.sync_continuous_stream_index();
        assert_eq!(app.queue.current_index(), 0, "round {round}");
        assert_eq!(
            app.queue.current_track().map(|t| t.filepath.clone()),
            Some(tracks[5].clone()),
            "round {round}: the playing track stays current"
        );
    }

    if let Some(handle) = app.web_server.take() {
        handle.stream_state().close_active_streams();
        handle.stop();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Removing queue rows before the continuous stream's track (`d` in the queue pane, the web
/// dashboard's remove) moves the stream's index with its track, so the event loop's sync, the
/// web state's ▶ and a pending skip stay on the track playing, not the one after it (#136).
#[test]
fn test_removing_a_row_before_the_continuous_streams_track_keeps_it_current() {
    common::isolate();
    let dir = temp_dir("remove_before_stream_index");
    let tracks: Vec<PathBuf> = (0..8).map(|i| dir.join(format!("t{}.wav", i))).collect();
    let settings = SettingsManager::with_path(dir.join("config.json"));
    let mut app = AppUI::new(Some(dir.clone()), settings);
    app.queue.add_tracks(
        tracks
            .iter()
            .enumerate()
            .map(|(i, p)| track_meta(p, &format!("Track {}", i), 60.0))
            .collect(),
    );
    app.queue.set_current_index(5);
    let (tx, _web_rx) = mpsc::channel();
    let handle = WebServer::start(0, app.web_state.clone(), tx).expect("start web server");
    let controller = handle.stream_state().continuous.clone();
    controller.is_active.store(true, Ordering::SeqCst);
    *controller.current_track_index.lock().unwrap() = 5;
    app.web_server = Some(handle);
    app.update_web_state();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    app.web_cmd_rx = Some(cmd_rx);
    app.active_tab = 1;
    app.queue_focus = true;

    // Each round removes one row before the playing track: from the terminal, then the web.
    for (round, row) in [2usize, 0, 1, 0].into_iter().enumerate() {
        let expected = 4 - round;
        if round % 2 == 0 {
            app.queue_cursor = row;
            app.handle_input_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('d'),
                crossterm::event::KeyModifiers::NONE,
            ))
            .unwrap();
        } else {
            // A skip the stream hasn't acted on yet names a track by its row too.
            controller.skip_to_index(expected + 1);
            controller.skip_to_index(expected + 1);
            cmd_tx.send(WebCommand::RemoveIndex(row)).unwrap();
            app.handle_web_commands();
            assert_eq!(
                controller.pending_request().map(|r| r.index),
                Some(expected),
                "round {round}: the pending skip follows its track"
            );
        }
        assert_eq!(app.queue.len(), 7 - round, "round {round}");
        assert_eq!(
            *controller.current_track_index.lock().unwrap(),
            expected,
            "round {round}"
        );
        app.sync_continuous_stream_index();
        assert_eq!(app.queue.current_index(), expected, "round {round}");
        assert_eq!(
            app.queue.current_track().map(|t| t.filepath.clone()),
            Some(tracks[5].clone()),
            "round {round}: the playing track stays current"
        );
        let state = app.web_state.read().unwrap();
        assert_eq!(state.queue_index, expected, "round {round}");
        let marked: Vec<&str> = state
            .queue
            .iter()
            .filter(|t| t.is_current)
            .map(|t| t.filepath.as_str())
            .collect();
        assert_eq!(
            marked,
            vec![tracks[5].to_string_lossy().as_ref()],
            "round {round}: the web ▶ stays on the playing track"
        );
    }

    if let Some(handle) = app.web_server.take() {
        handle.stream_state().close_active_streams();
        handle.stop();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The terminal app with `names` queued (files in a fresh folder) and `first` current, and its
/// web server's continuous stream decoding them through a [`FakeDecoder`] to a renderer that
/// keeps reading: queue edits reach the stream the real way (`AppUI::remove_queue_track`,
/// `update_web_state`), not hand-simulated.
#[cfg(unix)]
struct AppStream {
    app: AppUI,
    dir: PathBuf,
    tracks: Vec<PathBuf>,
    controller: Arc<ContinuousStreamController>,
    /// What the web server sends the UI (the stream's `StreamReshuffle` on a wrap).
    rx: mpsc::Receiver<WebCommand>,
    /// Feeds the app's `handle_web_commands`, as the web dashboard's actions do.
    cmd_tx: mpsc::Sender<WebCommand>,
    decoder: FakeDecoder,
    renderer: Option<Renderer>,
}

#[cfg(unix)]
impl AppStream {
    fn start(name: &str, names: &[&str], first: usize) -> Self {
        let dir = temp_dir(name);
        let decoder = FakeDecoder::new(&dir);
        let tracks: Vec<PathBuf> = names.iter().map(|n| dir.join(n)).collect();
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
        app.queue.set_current_index(first);
        app.queue_cursor = first;
        let (tx, rx) = mpsc::channel();
        let handle = WebServer::start(0, app.web_state.clone(), tx).expect("start web server");
        let controller = handle.stream_state().continuous.clone();
        controller.set_decoder(&decoder.binary);
        *controller.current_track_index.lock().unwrap() = first;
        let path = handle.stream_state().arm_continuous_session();
        let port = handle.port;
        app.web_server = Some(handle);
        app.update_web_state();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        app.web_cmd_rx = Some(cmd_rx);
        let renderer = Some(Renderer::connect(port, &path));
        assert!(app.continuous_stream_active());
        Self {
            app,
            dir,
            tracks,
            controller,
            rx,
            cmd_tx,
            decoder,
            renderer,
        }
    }

    /// Press `d` on queue row `row` in the queue pane.
    fn remove_in_terminal(&mut self, row: usize) {
        self.app.active_tab = 1;
        self.app.queue_focus = true;
        self.app.queue_cursor = row;
        self.app
            .handle_input_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('d'),
                crossterm::event::KeyModifiers::NONE,
            ))
            .unwrap();
    }

    /// Remove queue row `row` from the web dashboard.
    fn remove_from_web(&mut self, row: usize) {
        self.cmd_tx.send(WebCommand::RemoveIndex(row)).unwrap();
        self.app.handle_web_commands();
    }

    /// Whether the stream asks the UI for a reshuffle within `window`.
    fn reshuffle_within(&self, window: Duration) -> bool {
        let deadline = Instant::now() + window;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.rx.recv_timeout(left) {
                Ok(WebCommand::StreamReshuffle) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
    }
}

#[cfg(unix)]
impl Drop for AppStream {
    fn drop(&mut self) {
        if let Some(handle) = self.app.web_server.take() {
            handle.stream_state().close_active_streams();
            if let Some(renderer) = self.renderer.take() {
                renderer.hang_up();
            }
            handle.stop();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A row removed before the playing track (`AppUI::remove_queue_track`): the stream goes on to
/// the track after the one playing, not to the one two rows on (#136).
#[cfg(unix)]
#[test]
fn test_continuous_stream_goes_on_after_a_row_removed_before_its_track() {
    common::isolate();
    let mut f = AppStream::start(
        "follow_removal",
        &["t0.wav", "t1.wav", "t2.wav", "t3_long.wav", "t4.wav", "t5.wav"],
        3,
    );
    assert_eq!(f.decoder.run(1), (f.tracks[3].clone(), None));
    f.remove_in_terminal(1);
    assert_eq!(*f.controller.current_track_index.lock().unwrap(), 2);
    f.app.sync_continuous_stream_index();
    assert_eq!(
        f.app.queue.current_track().map(|t| t.filepath.clone()),
        Some(f.tracks[3].clone())
    );
    assert_eq!(
        f.decoder.run(2),
        (f.tracks[4].clone(), None),
        "the track after the playing one"
    );
    assert!(
        !f.reshuffle_within(Duration::from_millis(100)),
        "the stream didn't wrap"
    );
}

/// The stream's track is the last row when a row before it is removed (from the web
/// dashboard): its end is the queue's end, so the stream reshuffles and starts over at the top,
/// as at any wrap, rather than reading the end from the longer queue the track started with
/// (PR #137 review).
#[cfg(unix)]
#[test]
fn test_continuous_stream_wraps_after_its_last_track_when_a_row_before_it_is_removed() {
    common::isolate();
    let mut f = AppStream::start("remove_before_last", &["t0.wav", "t1.wav", "t2_long.wav"], 2);
    assert_eq!(f.decoder.run(1), (f.tracks[2].clone(), None));
    f.remove_from_web(0);
    assert_eq!(f.app.queue.len(), 2);
    assert_eq!(*f.controller.current_track_index.lock().unwrap(), 1);
    assert!(
        f.reshuffle_within(Duration::from_secs(10)),
        "the last track's end asks for a reshuffle"
    );
    assert_eq!(*f.controller.current_track_index.lock().unwrap(), 0);
    assert_eq!(f.decoder.run(2), (f.tracks[1].clone(), None), "starts over at the top");
}

/// The row the stream is decoding is removed and no skip follows (the local player, which
/// would restart on the track that took its place, isn't playing): at the end of the removed
/// track the stream goes on to the track that took its row, not the one after it (PR #137
/// review).
#[cfg(unix)]
#[test]
fn test_continuous_stream_goes_on_to_the_track_that_took_a_removed_tracks_row() {
    common::isolate();
    let mut f = AppStream::start(
        "remove_decoded",
        &["t0.wav", "t1.wav", "t2_long.wav", "t3.wav", "t4.wav"],
        2,
    );
    assert_eq!(f.decoder.run(1), (f.tracks[2].clone(), None));
    f.remove_in_terminal(2);
    assert!(f.controller.pending_request().is_none(), "no skip followed the removal");
    assert_eq!(f.app.queue_cursor, 2, "the cursor is on the track that took the row");
    assert_eq!(
        f.decoder.run(2),
        (f.tracks[3].clone(), None),
        "the track that took the removed track's row"
    );
    assert!(
        !f.reshuffle_within(Duration::from_millis(100)),
        "the stream didn't wrap"
    );
}

/// After a shuffle in the UI, the track after the playing one is the one the shuffle put after
/// it, not the one at the row after its old row; and a track that was last before the shuffle
/// doesn't make the stream wrap and reshuffle again when it ends (PR #127 review).
#[cfg(unix)]
#[test]
fn test_continuous_stream_goes_on_in_the_order_a_shuffle_left() {
    common::isolate();
    let f = DecoderFixture::start("follow_shuffle", &["t0.wav", "t1.wav", "t2.wav", "t3_long.wav"], 3);
    assert_eq!(f.run(1), (f.tracks[3].clone(), None));

    {
        // What the UI's shuffle does: publish the new order and tell the stream where each
        // track went, under the web state's lock.
        let mut s = f.state.write().unwrap();
        let old = s.queue.clone();
        // New order t3, t1, t0, t2: the track at old row `i` went to row `moved[i]`.
        let moved = [2, 1, 3, 0];
        for (from, &to) in moved.iter().enumerate() {
            s.queue[to] = old[from].clone();
            s.queue[to].index = to;
        }
        f.controller.queue_reordered(&RowMap::moved(moved.to_vec()));
    }
    assert_eq!(*f.controller.current_track_index.lock().unwrap(), 0);
    assert_eq!(
        f.run(2),
        (f.tracks[1].clone(), None),
        "the track the shuffle put after the playing one"
    );
    assert!(
        !matches!(f.rx.try_recv(), Ok(WebCommand::StreamReshuffle)),
        "the stream didn't wrap"
    );
}

/// What the UI's shuffle does to the stream: put the track at row `i` of the published queue at
/// row `moved[i]` and tell the stream where each track went, under the web state's lock, after
/// `edit` (on the new order) under the same lock.
#[cfg(unix)]
fn publish_reorder(f: &DecoderFixture, moved: &[usize], edit: impl FnOnce(&mut NowPlayingState)) {
    let mut s = f.state.write().unwrap();
    let old = s.queue.clone();
    for (from, &to) in moved.iter().enumerate() {
        s.queue[to] = old[from].clone();
        s.queue[to].index = to;
    }
    f.controller.queue_reordered(&RowMap::moved(moved.to_vec()));
    edit(&mut s);
}

/// Two shuffles while one track plays, before the stream has followed either: it goes on to
/// the track the second one put after the playing one (PR #127 review).
#[cfg(unix)]
#[test]
fn test_continuous_stream_follows_two_shuffles_during_one_track() {
    common::isolate();
    let f = DecoderFixture::start("two_shuffles", &["t0.wav", "t1.wav", "t2.wav", "t3_long.wav"], 3);
    assert_eq!(f.run(1), (f.tracks[3].clone(), None));
    // t3, t1, t0, t2; then t3, t0, t2, t1.
    publish_reorder(&f, &[2, 1, 3, 0], |_| {});
    publish_reorder(&f, &[0, 3, 1, 2], |_| {});
    assert_eq!(*f.controller.current_track_index.lock().unwrap(), 0);
    assert_eq!(
        f.run(2),
        (f.tracks[0].clone(), None),
        "the track the second shuffle put after the playing one"
    );
}

/// The track being decoded keeps its row through a shuffle: once the UI publishes its length at
/// its new row, `learn_published_duration` takes it (PR #127 review).
#[cfg(unix)]
#[test]
fn test_continuous_decoded_track_row_follows_a_shuffle() {
    common::isolate();
    let f = DecoderFixture::start_editing(
        "decoded_row",
        &["t0.wav", "t1.wav", "t2_live.wav", "t3.wav"],
        2,
        |queue| queue[2].duration_seconds = 0.0,
    );
    assert_eq!(f.run(1), (f.tracks[2].clone(), None));
    assert_eq!(*f.controller.current_duration.lock().unwrap(), 0.0);
    // t2, t0, t1, t3, with t2's length read meanwhile.
    publish_reorder(&f, &[1, 2, 0, 3], |s| {
        s.queue[0].duration_seconds = 60.0;
        assert!(
            f.controller.learn_published_duration(s),
            "learned at the row the shuffle moved it to"
        );
    });
    assert_eq!(*f.controller.current_duration.lock().unwrap(), 60.0);
    assert_eq!(f.state.read().unwrap().duration_seconds, 60.0);
}

/// The stream's own check while it writes finds the decoded track at the row a shuffle moved it
/// to too, so its length shows even when the UI never offers it (PR #127 review).
#[cfg(unix)]
#[test]
fn test_continuous_stream_learns_a_length_after_a_shuffle_while_it_writes() {
    common::isolate();
    let f = DecoderFixture::start_editing(
        "writer_length",
        &["t0.wav", "t1.wav", "t2_live.wav", "t3.wav"],
        2,
        |queue| queue[2].duration_seconds = 0.0,
    );
    assert_eq!(f.run(1), (f.tracks[2].clone(), None));
    publish_reorder(&f, &[1, 2, 0, 3], |s| s.queue[0].duration_seconds = 60.0);
    wait_until("the write loop never learned the length", || {
        *f.controller.current_duration.lock().unwrap() == 60.0
    });
    assert_eq!(f.state.read().unwrap().duration_seconds, 60.0);
}

/// A track tried while nothing gives audio is shown only once it gives some: a shuffle in
/// between moves the row it is shown at with it (PR #127 review).
#[cfg(unix)]
#[test]
fn test_continuous_track_shown_after_a_shuffle_is_at_its_new_row() {
    common::isolate();
    let f = DecoderFixture::start("shown_row", &["t0_bad.wav", "t1_late.wav", "t2.wav", "t3.wav"], 0);
    assert_eq!(f.run(1), (f.tracks[0].clone(), None));
    // t1_late gives no audio for a second: tried, not shown yet.
    assert_eq!(f.run(2), (f.tracks[1].clone(), None));
    // t1_late, t3, t0_bad, t2.
    publish_reorder(&f, &[2, 0, 3, 1], |_| {});
    wait_until("nothing was shown after t0_bad", || {
        f.state.read().unwrap().filepath.as_deref() != Some(f.tracks[0].as_path())
    });
    let s = f.state.read().unwrap();
    assert_eq!(s.filepath.as_deref(), Some(f.tracks[1].as_path()), "t1_late is shown");
    assert_eq!(s.queue_index, 0, "at its new row");
    assert_eq!(*f.controller.current_track_index.lock().unwrap(), 0);
}

/// A shuffle while the stream sends silence on a queue whose files are all missing: once they
/// are back, it goes on from the row the shuffle moved its track to (PR #127 review).
#[cfg(unix)]
#[test]
fn test_continuous_stream_idle_on_missing_files_goes_on_from_a_shuffled_row() {
    common::isolate();
    let names = ["t0_live.wav", "t1_live.wav", "t2_live.wav", "t3_live.wav"];
    let f = DecoderFixture::start_editing("idle_shuffle", &names, 1, |queue| {
        for (item, name) in queue.iter_mut().zip(names) {
            item.filepath = format!(
                "{}/gone/{}",
                Path::new(&item.filepath).parent().unwrap().display(),
                name
            );
        }
    });
    let gone = f.dir.join("gone");
    // Silence follows the pass that found every file missing and will go on from row 1.
    wait_until("no silence while every file is missing", || {
        f.renderer.as_ref().unwrap().received.load(Ordering::SeqCst) > 0
    });
    // t0, t2, t3, t1.
    publish_reorder(&f, &[0, 3, 1, 2], |_| {});
    // Every file comes back at once, so the stream never finds only some of them.
    let staging = f.dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    for name in names {
        std::fs::write(staging.join(name), b"back").unwrap();
    }
    std::fs::rename(&staging, &gone).unwrap();
    assert_eq!(
        f.run(1),
        (gone.join("t1_live.wav"), None),
        "the track that was at row 1, now at row 3"
    );
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

/// While the decoder gives nothing for seconds after it starts (the share hangs as ffmpeg opens
/// or reads the file), the renderer still gets silence at real time, no more than the lead ahead;
/// once the decoder gives audio, it plays and is shown as playing (#133).
#[cfg(unix)]
#[test]
fn test_continuous_stream_sends_silence_while_the_decoder_gives_nothing() {
    common::isolate();
    let f = DecoderFixture::armed("hung_decoder", &["t0_waking.wav", "t1_live.wav"], 0, |_, _| {});
    let renderer = RecordingRenderer::connect(f.port, &f.path);
    let connected = Instant::now();
    assert_eq!(f.run(1), (f.tracks[0].clone(), None));
    // The decoder waits 2 s from its log line: measured from 0.7 s to 1.7 s into its wait.
    std::thread::sleep(Duration::from_millis(700));
    let (from, before) = (Instant::now(), renderer.bytes.lock().unwrap().len());
    std::thread::sleep(Duration::from_millis(1000));
    let window = renderer.bytes.lock().unwrap()[before..].to_vec();
    let rate = window.len() as f64 / from.elapsed().as_secs_f64();
    assert!(
        window.iter().all(|&b| b == 0),
        "silence while the decoder gives nothing"
    );
    assert_about_real_time(rate, "while the decoder gives nothing");
    let received = renderer.bytes.lock().unwrap().len() as f64;
    let lead = 0.1 * REAL_TIME_BYTES_PER_SEC;
    assert!(
        received <= (connected.elapsed().as_secs_f64() + 0.1) * REAL_TIME_BYTES_PER_SEC + lead,
        "silence runs at most the lead ahead of real time: {:.0} bytes after {:?}",
        received,
        connected.elapsed()
    );

    wait_until("the decoder's audio never reached the renderer", || {
        renderer.bytes.lock().unwrap().contains(&1)
    });
    wait_until("the track was never shown as playing", || {
        f.state.read().unwrap().title == "Track 0"
    });
    assert_eq!(*f.controller.current_track_index.lock().unwrap(), 0);
    assert_eq!(f.decoder.calls().len(), 1, "the same decoder run, not a restart");
    f.handle.as_ref().unwrap().stream_state().close_active_streams();
    renderer.hang_up();
}

/// A decoder that stops giving PCM in the middle of a track (the share hangs as ffmpeg reads the
/// file) is covered by silence at real time as well, once the renderer runs out of what it was
/// sent; when the decoder goes on, its audio follows in the same run (#133).
#[cfg(unix)]
#[test]
fn test_continuous_stream_sends_silence_while_the_decoder_stalls_mid_track() {
    common::isolate();
    let f = DecoderFixture::armed("stalled_decoder", &["t0_stall.wav"], 0, |_, _| {});
    let renderer = RecordingRenderer::connect(f.port, &f.path);
    let connected = Instant::now();
    wait_until("the decoder's audio never reached the renderer", || {
        renderer.bytes.lock().unwrap().contains(&1)
    });
    let audio_from = Instant::now();
    // 0.5 s of audio, then 2.5 s of nothing: measured from 0.9 s to 1.9 s into the stall.
    std::thread::sleep(Duration::from_millis(1400));
    let (from, before) = (Instant::now(), renderer.bytes.lock().unwrap().len());
    std::thread::sleep(Duration::from_millis(1000));
    let window = renderer.bytes.lock().unwrap()[before..].to_vec();
    let rate = window.len() as f64 / from.elapsed().as_secs_f64();
    assert!(
        audio_from.elapsed() < Duration::from_millis(2900),
        "the window ran past the stall, the machine is too slow for this test"
    );
    assert!(window.iter().all(|&b| b == 0), "silence while the decoder stalls");
    assert_about_real_time(rate, "while the decoder stalls");
    let received = renderer.bytes.lock().unwrap().len() as f64;
    let lead = 0.1 * REAL_TIME_BYTES_PER_SEC;
    assert!(
        received <= (connected.elapsed().as_secs_f64() + 0.1) * REAL_TIME_BYTES_PER_SEC + lead,
        "silence runs at most the lead ahead of real time: {:.0} bytes after {:?}",
        received,
        connected.elapsed()
    );

    let silent_until = renderer.bytes.lock().unwrap().len();
    wait_until("the decoder's audio never came back", || {
        renderer.bytes.lock().unwrap()[silent_until..].contains(&1)
    });
    assert_eq!(f.decoder.calls().len(), 1, "the same decoder run, not a restart");
    f.handle.as_ref().unwrap().stream_state().close_active_streams();
    renderer.hang_up();
}

/// A decoder that, after a stall silence covered, gives a lone chunk of PCM now and then (a share
/// that answers a read every second or so) keeps the renderer fed: silence follows each chunk as
/// the renderer runs out of it, not only after a fresh `DECODER_GRACE` (#133).
#[cfg(unix)]
#[test]
fn test_continuous_stream_keeps_covering_a_decoder_that_gives_lone_chunks() {
    common::isolate();
    let f = DecoderFixture::armed("lone_chunks", &["t0_lone.wav"], 0, |_, _| {});
    let renderer = RecordingRenderer::connect(f.port, &f.path);
    // The first chunk, the stall after it covered, then four lone chunks.
    wait_within(Duration::from_secs(30), "the lone chunks never came", || {
        audio_runs(&renderer.bytes.lock().unwrap()).len() >= 5
    });
    let runs = audio_runs(&renderer.bytes.lock().unwrap());
    let dry = renderer.dry_after(renderer.arrival_of(runs[1]));
    assert!(
        dry < Duration::from_millis(250),
        "the renderer ran dry for {:?} over the three lone chunks after a covered stall",
        dry
    );
    assert_eq!(f.decoder.calls().len(), 1, "the same decoder run, not a restart");
    f.handle.as_ref().unwrap().stream_state().close_active_streams();
    renderer.hang_up();
}

/// A renderer that was paused for seconds (it stopped reading, so the stream's writes blocked)
/// still holds what it was sent before: a next track whose decoder takes a second to start
/// follows the one before with no silence between them (#133).
#[cfg(unix)]
#[test]
fn test_continuous_stream_adds_no_silence_after_a_renderer_pause() {
    common::isolate();
    // 0.3 s of its own buffer, besides what the sockets hold.
    let (silence, first_track) = silence_after_a_renderer_pause("paused_renderer", "t0_flood.wav", 0.3);
    assert_eq!(
        silence, 0,
        "{} bytes of silence between the tracks, although the renderer held seconds of audio",
        silence
    );
    assert_eq!(first_track, 882_000, "the whole first track, then the second");
}

/// A renderer pause stays counted while the decoder, after it, gives its audio no faster than
/// real time (a slow share): those writes don't go faster than their audio because the renderer
/// read ahead, so they don't take back the time it was paused. A next track whose decoder takes a
/// second to start then still follows with no silence (#133).
#[cfg(unix)]
#[test]
fn test_continuous_stream_keeps_a_renderer_pause_through_audio_at_real_time() {
    common::isolate();
    let (silence, first_track) = silence_after_a_renderer_pause("paused_then_paced", "t0_surge.wav", 1.0);
    assert_eq!(
        silence, 0,
        "{} bytes of silence between the tracks, although the renderer held seconds of audio",
        silence
    );
    assert_eq!(first_track, 1_940_400, "the whole first track, then the second");
}

/// Stream `first` (`0x01` bytes) and then `sleepy` (`0x02` bytes after 1 s) to a renderer playing
/// out of `buffer_secs` of its own buffer, paused for 4 s 0.5 s into the first track. Returns the
/// silence bytes the renderer got between the first track's audio and the second's, and the bytes
/// from the first track's first byte to the second's.
#[cfg(unix)]
fn silence_after_a_renderer_pause(name: &str, first: &str, buffer_secs: f64) -> (usize, usize) {
    let f = DecoderFixture::armed(name, &[first, "t1_sleepy.wav"], 0, |_, _| {});
    let capacity = (buffer_secs * REAL_TIME_BYTES_PER_SEC) as u64 / 4 * 4;
    let renderer = PlayingRenderer::connect(f.port, &f.path, capacity);
    wait_until("the first track never reached the renderer", || {
        renderer.bytes.lock().unwrap().contains(&1)
    });
    std::thread::sleep(Duration::from_millis(500));
    renderer.paused.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_secs(4));
    renderer.paused.store(false, Ordering::SeqCst);
    wait_within(
        Duration::from_secs(20),
        "the second track never reached the renderer",
        || renderer.bytes.lock().unwrap().contains(&2),
    );
    let bytes = renderer.bytes.lock().unwrap().clone();
    let first_one = bytes.iter().position(|&b| b == 1).unwrap();
    let first_two = bytes.iter().position(|&b| b == 2).unwrap();
    let silence = bytes[first_one..first_two].iter().filter(|&&b| b == 0).count();
    f.handle.as_ref().unwrap().stream_state().close_active_streams();
    renderer.hang_up();
    (silence, first_two - first_one)
}

/// A renderer that reads in bursts (it blocks the stream's writes while it plays from its own
/// buffer, then takes a second of audio at once) is not taken for one that paused: when the next
/// track's decoder then hangs, silence starts before the renderer runs out of audio (#117, #133).
#[cfg(unix)]
#[test]
fn test_continuous_stream_renderer_reading_in_bursts_never_runs_dry() {
    common::isolate();
    let f = DecoderFixture::armed("bursty_renderer", &["t0_flood.wav", "t1_hang.wav"], 0, |_, _| {});
    let bytes_for = |secs: f64| (secs * REAL_TIME_BYTES_PER_SEC) as u64 / 4 * 4;
    let renderer = PlayingRenderer::connect_bursty(f.port, &f.path, bytes_for(1.2), bytes_for(0.1));
    wait_until("the first track never reached the renderer", || {
        renderer.bytes.lock().unwrap().contains(&1)
    });
    // All of the first track gets through, then the hung decoder's silence for a while.
    wait_within(Duration::from_secs(20), "the first track never got through", || {
        let bytes = renderer.bytes.lock().unwrap();
        bytes.iter().filter(|&&b| b == 1).count() == 882_000 && bytes.ends_with(&[0u8; 176_400])
    });
    std::thread::sleep(Duration::from_secs(1));
    let dry = *renderer.dry.lock().unwrap();
    assert!(dry < Duration::from_millis(300), "the renderer ran dry for {:?}", dry);
    f.handle.as_ref().unwrap().stream_state().close_active_streams();
    renderer.hang_up();
}

/// Re-arming the session while a decoder gives nothing ends the old session's connection: it
/// stops sending silence and closes, and its decoder is stopped (#133).
#[cfg(unix)]
#[test]
fn test_continuous_stream_rearmed_session_ends_a_hung_decoders_silence() {
    common::isolate();
    let mut f = DecoderFixture::armed("rearm_hung_decoder", &["t0_hang.wav"], 0, |_, _| {});
    f.renderer = Some(Renderer::connect(f.port, &f.path));
    assert_eq!(f.run(1), (f.tracks[0].clone(), None));
    // Past the decoder's start grace: the stream sends silence for it.
    std::thread::sleep(Duration::from_millis(1000));
    let renderer = f.renderer.as_ref().unwrap();
    assert_about_real_time(
        renderer.rate_over(Duration::from_millis(500)),
        "silence while the decoder hangs",
    );
    let _next = f.handle.as_ref().unwrap().stream_state().arm_continuous_session();
    assert!(
        renderer.closed_within(Duration::from_secs(2)),
        "the old session's connection kept sending silence"
    );
    let pid = f.decoder.pid(1);
    wait_until("the old session's decoder was left running", || !pid_alive(&pid));
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

/// A shuffle made while the stream waits on the check of the next file is followed, also when
/// the next file in the new order is still being checked too: the stream goes on from the row
/// the shuffle moved the ended track to, not the one it was on (merge of #127 and #132).
#[cfg(unix)]
#[test]
fn test_continuous_shuffle_while_a_file_check_hangs_is_followed() {
    common::isolate();
    let hang = HeldPathCheck::default();
    let names = ["t0_short.wav", "t1.wav", "t2.wav", "t3.wav", "t4.wav"];
    let f = DecoderFixture::start_with("hung_shuffle", &names, 0, |c, tracks| {
        hang.install(c, &[tracks[1].clone(), tracks[2].clone()])
    });
    let _release = hang.release_on_drop();
    assert_eq!(f.run(1), (f.tracks[0].clone(), None));
    // t0 ends after about 0.5 s; the look for t1 then waits on its check.
    std::thread::sleep(Duration::from_millis(900));
    assert_eq!(f.decoder.calls().len(), 1, "nothing decoded while t1's check hangs");
    // New order t3, t4, t0, t2, t1: the track after t0 is t2, whose check hangs too.
    publish_reorder(&f, &[2, 4, 3, 0, 1], |_| {});
    std::thread::sleep(Duration::from_millis(300));
    hang.release();
    assert_eq!(
        f.run(2),
        (f.tracks[2].clone(), None),
        "the track the shuffle put after the one that ended"
    );
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

/// A renderer that keeps every byte of the stream after the WAV header, and when each read got
/// its bytes (`arrivals`: the time and the bytes got so far).
struct RecordingRenderer {
    bytes: Arc<Mutex<Vec<u8>>>,
    arrivals: Arc<Mutex<Vec<(Instant, usize)>>>,
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
        let arrivals = Arc::new(Mutex::new(Vec::new()));
        let reading = Arc::new(AtomicBool::new(true));
        let (bytes_t, arrivals_t, reading_t) = (bytes.clone(), arrivals.clone(), reading.clone());
        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while reading_t.load(Ordering::SeqCst) {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let at = Instant::now();
                        let mut bytes = bytes_t.lock().unwrap();
                        bytes.extend_from_slice(&buf[..n]);
                        arrivals_t.lock().unwrap().push((at, bytes.len()));
                    }
                }
            }
        });
        Self {
            bytes,
            arrivals,
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

    /// When the read that got byte `index` (from 0) arrived.
    fn arrival_of(&self, index: usize) -> Instant {
        let arrivals = self.arrivals.lock().unwrap();
        arrivals
            .iter()
            .find(|&&(_, got)| got > index)
            .unwrap_or_else(|| panic!("byte {} never arrived", index))
            .0
    }

    /// How long a renderer playing at real time from the first byte it got, each byte from when
    /// it arrived, had nothing to play after `since`.
    fn dry_after(&self, since: Instant) -> Duration {
        let arrivals = self.arrivals.lock().unwrap();
        let Some(&(mut clock, _)) = arrivals.first() else {
            return Duration::ZERO;
        };
        let secs = |bytes: usize| Duration::from_secs_f64(bytes as f64 / REAL_TIME_BYTES_PER_SEC);
        let (mut played, mut got, mut dry) = (Duration::ZERO, 0, Duration::ZERO);
        for &(at, total) in arrivals.iter() {
            let held = secs(got) - played;
            if at - clock > held {
                // It ran dry `held` after `clock`, until these bytes came.
                let from = (clock + held).max(since);
                dry += at.saturating_duration_since(from);
                played = secs(got);
            } else {
                played += at - clock;
            }
            (clock, got) = (at, total);
        }
        dry
    }
}

/// Where each run of `0x01` bytes starts in `bytes` (the first, and each after other bytes).
fn audio_runs(bytes: &[u8]) -> Vec<usize> {
    (0..bytes.len())
        .filter(|&i| bytes[i] == 1 && (i == 0 || bytes[i - 1] != 1))
        .collect()
}

/// A connection to 127.0.0.1:`port` whose receive buffer is set small before it connects, so a
/// writer to it blocks soon after its reader stops reading (the kernel would otherwise take
/// seconds of PCM).
#[cfg(unix)]
fn connect_small_receive_buffer(port: u16) -> TcpStream {
    use std::os::unix::io::FromRawFd;
    // SAFETY: plain socket calls on a descriptor this function owns; it is handed to the
    // `TcpStream` (which closes it) once connected, or closed here on failure.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        assert!(fd >= 0, "socket");
        let size: libc::c_int = 16384;
        let set = libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &size as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        assert_eq!(set, 0, "SO_RCVBUF");
        let mut addr: libc::sockaddr_in = std::mem::zeroed();
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
        {
            addr.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
        }
        addr.sin_family = libc::AF_INET as libc::sa_family_t;
        addr.sin_port = port.to_be();
        addr.sin_addr = libc::in_addr {
            s_addr: u32::from(Ipv4Addr::LOCALHOST).to_be(),
        };
        let connected = libc::connect(
            fd,
            &addr as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        );
        if connected != 0 {
            libc::close(fd);
            panic!("connect to 127.0.0.1:{}", port);
        }
        TcpStream::from_raw_fd(fd)
    }
}

/// A renderer that plays at real time out of a buffer holding `capacity` bytes and that can be
/// paused: while `paused` it neither plays nor reads, as when its user presses pause. It reads
/// only while it has room (so the stream's writes block at real time once it is full) and, once
/// full, not again until it holds `low_water` bytes or less: with `low_water` well under
/// `capacity` it reads in bursts, as renderers with a large buffer do. Records the bytes it got
/// after the WAV header, and `dry`, how long it had nothing to play while not paused once audio
/// had started.
#[cfg(unix)]
struct PlayingRenderer {
    bytes: Arc<Mutex<Vec<u8>>>,
    paused: Arc<AtomicBool>,
    dry: Arc<Mutex<Duration>>,
    reading: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl PlayingRenderer {
    fn connect(port: u16, path: &str, capacity: u64) -> Self {
        Self::connect_bursty(port, path, capacity, capacity)
    }

    fn connect_bursty(port: u16, path: &str, capacity: u64, low_water: u64) -> Self {
        let (status, mut reader) = open_on(connect_small_receive_buffer(port), port, path);
        assert_eq!(status, 200);
        let mut wav_hdr = [0u8; 44];
        reader.read_exact(&mut wav_hdr).unwrap();
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let paused = Arc::new(AtomicBool::new(false));
        let dry = Arc::new(Mutex::new(Duration::ZERO));
        let reading = Arc::new(AtomicBool::new(true));
        let (bytes_t, paused_t, dry_t, reading_t) = (bytes.clone(), paused.clone(), dry.clone(), reading.clone());
        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            // What it has played (time spent playing) and got.
            let mut played = Duration::ZERO;
            let mut got: u64 = 0;
            let mut last = Instant::now();
            let mut filling = true;
            while reading_t.load(Ordering::SeqCst) {
                let now = Instant::now();
                let paused = paused_t.load(Ordering::SeqCst);
                if !paused {
                    played += now - last;
                }
                last = now;
                // It can't play what it hasn't got: it runs dry instead.
                let got_time = Duration::from_secs_f64(got as f64 / REAL_TIME_BYTES_PER_SEC);
                if played > got_time {
                    if got > 0 {
                        *dry_t.lock().unwrap() += played - got_time;
                    }
                    played = got_time;
                }
                let held = got - (played.as_secs_f64() * REAL_TIME_BYTES_PER_SEC) as u64;
                if held >= capacity {
                    filling = false;
                } else if held <= low_water {
                    filling = true;
                }
                if paused || !filling {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                let room = ((capacity - held) as usize).min(buf.len());
                match reader.read(&mut buf[..room]) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        got += n as u64;
                        bytes_t.lock().unwrap().extend_from_slice(&buf[..n]);
                    }
                }
            }
        });
        Self {
            bytes,
            paused,
            dry,
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

/// A next file found missing as the track before it started is checked again as that track
/// ends: one back on the share meanwhile (a remount) plays, rather than being passed over on an
/// answer seconds old (#117).
#[cfg(unix)]
#[test]
fn test_continuous_file_back_while_the_track_before_plays_is_played() {
    common::isolate();
    let checks = SlowPathCheck::default();
    let f = DecoderFixture::start_with(
        "remount_ahead",
        &["a_longones.wav", "b_live.wav", "c_live.wav"],
        0,
        |c, t| {
            std::fs::remove_file(&t[1]).unwrap();
            checks.install(c, &[], Duration::ZERO)
        },
    );
    assert_eq!(f.run(1), (f.tracks[0].clone(), None));
    wait_until("the next file was never checked ahead", || {
        checks.answered(&f.tracks[1]) == 1
    });
    std::fs::write(&f.tracks[1], b"back").unwrap();
    assert_eq!(
        f.run(2),
        (f.tracks[1].clone(), None),
        "the file back on the share plays"
    );
    assert_eq!(
        checks.started(&f.tracks[1]),
        2,
        "checked again as the track before ended"
    );
}

/// A late answer still unused when the track before its file starts is kept for that track's
/// end: the check ahead made as the track starts doesn't use it up, so the file plays rather than
/// being checked again only to miss the deadline once more (#117).
#[cfg(unix)]
#[test]
fn test_continuous_late_answer_is_not_used_up_by_the_check_ahead() {
    common::isolate();
    let slow = SlowPathCheck::default();
    let f = DecoderFixture::start_with(
        "late_ahead",
        &["a_longones.wav", "b_live.wav", "c_live.wav"],
        1,
        |c, t| slow.install(c, &t[1..2], Duration::from_millis(2500)),
    );
    assert_eq!(f.run(1), (f.tracks[2].clone(), None), "passed over while unanswered");
    wait_until("the slow check never answered", || slow.answered(&f.tracks[1]) == 1);
    f.controller.skip_to_index(0);
    assert_eq!(f.run(2), (f.tracks[0].clone(), None));
    assert_eq!(f.run(3), (f.tracks[1].clone(), None), "played on its late answer");
    assert_eq!(slow.started(&f.tracks[1]), 1, "not checked again");
}

/// A late answer the check ahead found as the track before its file started is kept for that
/// track's end however long the track plays, not only while it is younger than the keep (here
/// 1 s, under the 3 s track), so the file plays rather than being checked again only to miss the
/// deadline once more (#117).
#[cfg(unix)]
#[test]
fn test_continuous_late_answer_is_kept_for_a_track_longer_than_the_keep() {
    common::isolate();
    let slow = SlowPathCheck::default();
    let f = DecoderFixture::start_with(
        "late_kept",
        &["a_longones.wav", "b_live.wav", "c_live.wav"],
        1,
        |c, t| {
            c.set_late_answer_keep(Duration::from_secs(1));
            slow.install(c, &t[1..2], Duration::from_millis(2500))
        },
    );
    assert_eq!(f.run(1), (f.tracks[2].clone(), None), "passed over while unanswered");
    wait_until("the slow check never answered", || slow.answered(&f.tracks[1]) == 1);
    f.controller.skip_to_index(0);
    assert_eq!(f.run(2), (f.tracks[0].clone(), None));
    assert_eq!(f.run(3), (f.tracks[1].clone(), None), "played on its late answer");
    assert_eq!(slow.started(&f.tracks[1]), 1, "not checked again");
}

/// A next file whose check ahead answered late that it was missing is checked again as the track
/// before it ends, like one found missing in time: one back on the share by then plays (#117).
#[cfg(unix)]
#[test]
fn test_continuous_next_file_found_missing_late_is_checked_again_at_the_track_end() {
    common::isolate();
    let started = Arc::new(Mutex::new(0usize));
    let f = DecoderFixture::start_with(
        "late_missing",
        &["a_longones.wav", "b_live.wav", "c_live.wav"],
        0,
        |c, t| {
            std::fs::remove_file(&t[1]).unwrap();
            let (b, started) = (t[1].clone(), started.clone());
            c.set_path_check(move |path: &Path| {
                if path != b {
                    return path.exists();
                }
                let first = {
                    let mut n = started.lock().unwrap();
                    *n += 1;
                    *n == 1
                };
                if first {
                    // Past the deadline, then missing: the file is back just after it answers.
                    std::thread::sleep(Duration::from_millis(2200));
                    let exists = path.exists();
                    std::fs::write(path, b"back").unwrap();
                    return exists;
                }
                path.exists()
            });
        },
    );
    assert_eq!(f.run(1), (f.tracks[0].clone(), None));
    assert_eq!(
        f.run(2),
        (f.tracks[1].clone(), None),
        "the file back on the share plays"
    );
    assert_eq!(*started.lock().unwrap(), 2, "checked again as the track before ended");
}

/// A late answer used by the look as a track ends counts once, like one used by any look: a
/// later look asks afresh rather than reusing it (#117).
#[cfg(unix)]
#[test]
fn test_continuous_late_answer_used_at_a_track_end_counts_once() {
    common::isolate();
    let started = Arc::new(Mutex::new(0usize));
    let f = DecoderFixture::start_with(
        "late_once",
        &["a_longones.wav", "b_live.wav", "c_live.wav"],
        0,
        |c, t| {
            let (b, started) = (t[1].clone(), started.clone());
            c.set_path_check(move |path: &Path| {
                if path == b {
                    let first = {
                        let mut n = started.lock().unwrap();
                        *n += 1;
                        *n == 1
                    };
                    if first {
                        // Past the deadline, but in time for the track before to end.
                        std::thread::sleep(Duration::from_millis(2200));
                    }
                }
                path.exists()
            });
        },
    );
    assert_eq!(f.run(1), (f.tracks[0].clone(), None));
    assert_eq!(f.run(2), (f.tracks[1].clone(), None), "played on its late answer");
    f.controller.skip_to_index(1);
    assert_eq!(f.run(3), (f.tracks[1].clone(), None));
    assert_eq!(*started.lock().unwrap(), 2, "checked afresh for the skip");
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
