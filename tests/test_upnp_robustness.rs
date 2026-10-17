//! Robustness tests for the UPnP player and SOAP client against misbehaving or slow
//! renderers. Every mock binds 127.0.0.1 and serves each connection on its own thread,
//! so a deliberately slow reply never blocks the player's other SOAP calls.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quickstream::discovery::DLNADevice;
use quickstream::player::upnp::UPnPPlayer;
use quickstream::player::AudioPlayer;

type Handler = Arc<dyn Fn(&str, &str, &mut TcpStream) + Send + Sync>;

struct MockRenderer {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockRenderer {
    fn device(&self, name: &str) -> DLNADevice {
        DLNADevice {
            udn: format!("uuid:{}", name),
            friendly_name: name.to_string(),
            model_name: "Mock".to_string(),
            manufacturer: "Quickstream".to_string(),
            host: "127.0.0.1".to_string(),
            port: self.port,
            location: format!("http://127.0.0.1:{}/desc.xml", self.port),
            av_transport_control_url: format!("http://127.0.0.1:{}/ctrl/AVTransport", self.port),
            rendering_control_url: format!("http://127.0.0.1:{}/ctrl/RenderingControl", self.port),
        }
    }
}

impl Drop for MockRenderer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Spawn a mock renderer on 127.0.0.1. `handler(action, body, stream)` writes the reply.
fn spawn_mock(handler: Handler) -> MockRenderer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock renderer");
    listener.set_nonblocking(true).expect("set nonblocking");
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let handle = std::thread::spawn(move || {
        while !stop_thread.load(Ordering::SeqCst) {
            let stream = match listener.accept() {
                Ok((s, _)) => s,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(_) => break,
            };
            let handler = handler.clone();
            std::thread::spawn(move || serve_connection(stream, handler));
        }
    });
    MockRenderer {
        port,
        stop,
        handle: Some(handle),
    }
}

fn serve_connection(mut stream: TcpStream, handler: Handler) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
        return;
    }
    let mut action = String::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        let trimmed = line.trim();
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("soapaction") {
                let v = value.trim().trim_matches('"');
                action = v.rsplit('#').next().unwrap_or("").to_string();
            } else if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        let _ = reader.read_exact(&mut body);
    }
    let body = String::from_utf8_lossy(&body).to_string();
    handler(&action, &body, &mut stream);
    let _ = stream.flush();
}

fn ok_xml(inner: &str) -> String {
    format!(
        r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>{}</s:Body></s:Envelope>"#,
        inner
    )
}

fn write_ok(stream: &mut TcpStream, inner: &str) {
    let body = ok_xml(inner);
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes());
}

fn transport_info_xml(state: &str) -> String {
    format!(
        r#"<u:GetTransportInfoResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><CurrentTransportState>{}</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed></u:GetTransportInfoResponse>"#,
        state
    )
}

fn position_info_xml(rel: &str, dur: &str) -> String {
    format!(
        r#"<u:GetPositionInfoResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><TrackDuration>{}</TrackDuration><RelTime>{}</RelTime></u:GetPositionInfoResponse>"#,
        dur, rel
    )
}

/// Write a valid 8 kHz mono 16-bit PCM WAV file of the given length so the player
/// reads a real (non-zero) duration from it.
fn write_wav(path: &Path, seconds: u32) {
    let sample_rate: u32 = 8000;
    let data_len: u32 = sample_rate * 2 * seconds;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.resize(44 + data_len as usize, 0);
    std::fs::write(path, out).expect("write wav");
}

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(name)
}

fn wait_for(flag: &AtomicBool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if flag.load(Ordering::SeqCst) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    flag.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// #11: an old poll thread must not write into the new session's state
// ---------------------------------------------------------------------------

/// The old poller is inside GetTransportInfo when the user skips to the next track.
/// The renderer reports STOPPED (because play_at sent Stop) only after the new track's
/// Play. The old poller saw position 175/180 on the previous track, so it would decide
/// "track finished" and flag the *new* track as finished, causing a spurious skip.
#[test]
fn test_upnp_stale_poller_cannot_mark_new_track_finished() {
    struct State {
        transport: Mutex<String>,
        rel: Mutex<String>,
        slow_gti_armed: AtomicBool,
        gti_in_flight: AtomicBool,
        play_after_arm: AtomicBool,
    }
    let st = Arc::new(State {
        transport: Mutex::new("PLAYING".to_string()),
        rel: Mutex::new("00:02:55".to_string()),
        slow_gti_armed: AtomicBool::new(false),
        gti_in_flight: AtomicBool::new(false),
        play_after_arm: AtomicBool::new(false),
    });
    let st_h = st.clone();
    let mock = spawn_mock(Arc::new(move |action, _body, stream| match action {
        "GetTransportInfo" => {
            if st_h.slow_gti_armed.swap(false, Ordering::SeqCst) {
                st_h.gti_in_flight.store(true, Ordering::SeqCst);
                wait_for(&st_h.play_after_arm, Duration::from_secs(5));
                std::thread::sleep(Duration::from_millis(50));
                write_ok(stream, &transport_info_xml("STOPPED"));
            } else {
                let s = st_h.transport.lock().unwrap().clone();
                write_ok(stream, &transport_info_xml(&s));
            }
        }
        "GetPositionInfo" => {
            let rel = st_h.rel.lock().unwrap().clone();
            write_ok(stream, &position_info_xml(&rel, "00:03:00"));
        }
        "Stop" => {
            *st_h.transport.lock().unwrap() = "STOPPED".to_string();
            write_ok(stream, "<u:StopResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"/>");
        }
        "Play" => {
            *st_h.transport.lock().unwrap() = "PLAYING".to_string();
            if st_h.gti_in_flight.load(Ordering::SeqCst) {
                st_h.play_after_arm.store(true, Ordering::SeqCst);
            }
            write_ok(stream, "<u:PlayResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"/>");
        }
        _ => write_ok(stream, "<u:OkResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"/>"),
    }));

    let track_a = temp_path("qs_test_stale_poller_a.wav");
    let track_b = temp_path("qs_test_stale_poller_b.wav");
    write_wav(&track_a, 1);
    write_wav(&track_b, 1);

    let mut player = UPnPPlayer::new(mock.device("stale-poller"), 8080);
    assert!(player.play(&track_a).is_ok());

    // Let the poller observe the previous track near its end (175s of 180s).
    std::thread::sleep(Duration::from_millis(1300));
    assert_eq!(player.position_seconds(), 175.0);

    // Hold the poller's next GetTransportInfo until the new track's Play has gone out.
    st.slow_gti_armed.store(true, Ordering::SeqCst);
    assert!(wait_for(&st.gti_in_flight, Duration::from_secs(3)), "poller never queried transport state");
    *st.rel.lock().unwrap() = "00:00:00".to_string();

    assert!(player.play(&track_b).is_ok());
    // Give the stale reply time to land and be (mis)applied.
    std::thread::sleep(Duration::from_millis(400));

    assert!(
        !player.is_track_finished(),
        "a poll thread from the previous track must not mark the new track finished"
    );
    assert!(player.is_playing(), "the new track must still be playing");

    let _ = player.stop();
    let _ = std::fs::remove_file(track_a);
    let _ = std::fs::remove_file(track_b);
}

/// #34: the poller computes "time since seek" before its SOAP calls. If a seek happens
/// while GetPositionInfo is in flight, it applies the renderer's pre-seek position on
/// top of the new stream offset and the progress bar jumps away from the seek target.
#[test]
fn test_upnp_stale_position_report_is_dropped_after_seek() {
    struct State {
        transport: Mutex<String>,
        slow_gpi_armed: AtomicBool,
        gpi_in_flight: AtomicBool,
        play_after_arm: AtomicBool,
    }
    let st = Arc::new(State {
        transport: Mutex::new("PLAYING".to_string()),
        slow_gpi_armed: AtomicBool::new(false),
        gpi_in_flight: AtomicBool::new(false),
        play_after_arm: AtomicBool::new(false),
    });
    let st_h = st.clone();
    let mock = spawn_mock(Arc::new(move |action, _body, stream| match action {
        "GetPositionInfo" => {
            if st_h.slow_gpi_armed.swap(false, Ordering::SeqCst) {
                st_h.gpi_in_flight.store(true, Ordering::SeqCst);
                wait_for(&st_h.play_after_arm, Duration::from_secs(5));
                std::thread::sleep(Duration::from_millis(50));
                // Position of the *old* stream, measured before the seek.
                write_ok(stream, &position_info_xml("00:00:20", "00:03:00"));
            } else {
                write_ok(stream, &position_info_xml("00:00:00", "00:03:00"));
            }
        }
        "GetTransportInfo" => {
            let s = st_h.transport.lock().unwrap().clone();
            write_ok(stream, &transport_info_xml(&s));
        }
        "Stop" => {
            *st_h.transport.lock().unwrap() = "STOPPED".to_string();
            write_ok(stream, "<u:StopResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"/>");
        }
        "Play" => {
            *st_h.transport.lock().unwrap() = "PLAYING".to_string();
            if st_h.gpi_in_flight.load(Ordering::SeqCst) {
                st_h.play_after_arm.store(true, Ordering::SeqCst);
            }
            write_ok(stream, "<u:PlayResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"/>");
        }
        _ => write_ok(stream, "<u:OkResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"/>"),
    }));

    let track = temp_path("qs_test_stale_seek.mp3");
    let _ = std::fs::write(&track, b"DUMMY_AUDIO_DATA");

    let mut player = UPnPPlayer::new(mock.device("stale-seek"), 8080);
    assert!(player.play(&track).is_ok());

    st.slow_gpi_armed.store(true, Ordering::SeqCst);
    assert!(wait_for(&st.gpi_in_flight, Duration::from_secs(3)), "poller never queried position");

    assert!(player.seek_to(50.0).is_ok());
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        player.position_seconds(),
        50.0,
        "a position report requested before the seek must not overwrite the seek target"
    );

    let _ = player.stop();
    let _ = std::fs::remove_file(track);
}
