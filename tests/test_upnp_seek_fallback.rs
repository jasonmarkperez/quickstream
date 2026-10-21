//! A file the web server can't seek (a WAV or AIFF whose header it can't use, which it serves
//! whole from 0:00, issue #116): a seek while it plays is refused, so playback and the position
//! stay as they are, and a start at a position (resume, output switch) loads it from 0:00 with
//! the position at 0. Either way the reason reaches the status line through `take_error`, and only
//! once the load worked; a file the check can't read is not reported as unseekable.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use quickstream::discovery::DLNADevice;
use quickstream::player::upnp::UPnPPlayer;
use quickstream::player::AudioPlayer;

/// The URIs a mock renderer was given with `SetAVTransportURI`, in order.
type Uris = Arc<Mutex<Vec<String>>>;

/// A mock renderer that answers every SOAP action and records each `SetAVTransportURI` URI.
fn spawn_renderer() -> (u16, Arc<AtomicBool>, Uris, std::thread::JoinHandle<()>) {
    spawn_renderer_failing_uri(Arc::new(AtomicBool::new(false)))
}

/// As [`spawn_renderer`], but `SetAVTransportURI` answers with a UPnP fault while `fail_uri` is set.
fn spawn_renderer_failing_uri(fail_uri: Arc<AtomicBool>) -> (u16, Arc<AtomicBool>, Uris, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock renderer");
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).expect("set nonblocking");
    let stop = Arc::new(AtomicBool::new(false));
    let uris = Arc::new(Mutex::new(Vec::new()));
    let (stop_thread, uris_thread) = (stop.clone(), uris.clone());
    let handle = std::thread::spawn(move || {
        while !stop_thread.load(Ordering::SeqCst) {
            let mut stream = match listener.accept() {
                Ok((s, _)) => {
                    // An accepted socket inherits the listener's non-blocking mode on macOS.
                    s.set_nonblocking(false).expect("set blocking");
                    s
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(15));
                    continue;
                }
                Err(_) => break,
            };
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line.is_empty() {
                continue;
            }
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            let _ = reader.read_exact(&mut body);
            let body = String::from_utf8_lossy(&body);
            if let (Some(start), Some(end)) = (body.find("<CurrentURI>"), body.find("</CurrentURI>")) {
                uris_thread.lock().unwrap().push(body[start + 12..end].to_string());
            }
            let (status, resp) = if fail_uri.load(Ordering::SeqCst) && body.contains("SetAVTransportURI") {
                (
                    "500 Internal Server Error",
                    r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><detail><UPnPError><errorCode>714</errorCode><errorDescription>Illegal MIME-type</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#,
                )
            } else {
                (
                    "200 OK",
                    r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:SuccessResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1" /></s:Body></s:Envelope>"#,
                )
            };
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    resp.len(),
                    resp
                )
                .as_bytes(),
            );
        }
    });
    (port, stop, uris, handle)
}

fn device(port: u16) -> DLNADevice {
    DLNADevice {
        udn: "uuid:test-seek-fallback".to_string(),
        friendly_name: "Mock Renderer".to_string(),
        model_name: "Mock".to_string(),
        manufacturer: "Test".to_string(),
        host: "127.0.0.1".to_string(),
        port,
        location: format!("http://127.0.0.1:{}/desc.xml", port),
        av_transport_control_url: format!("http://127.0.0.1:{}/ctrl/AVTransport", port),
        rendering_control_url: format!("http://127.0.0.1:{}/ctrl/RenderingControl", port),
    }
}

/// A 16-bit stereo 8 kHz WAV of `seconds`.
fn wav(seconds: usize) -> Vec<u8> {
    let audio = vec![0x11u8; 8000 * 4 * seconds];
    let mut w = b"RIFF".to_vec();
    w.extend_from_slice(&(36 + audio.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    for field in [
        &1u16.to_le_bytes()[..],
        &2u16.to_le_bytes(),
        &8000u32.to_le_bytes(),
        &32000u32.to_le_bytes(),
        &4u16.to_le_bytes(),
        &16u16.to_le_bytes(),
    ] {
        w.extend_from_slice(field);
    }
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(audio.len() as u32).to_le_bytes());
    w.extend_from_slice(&audio);
    w
}

fn last_uri(uris: &Mutex<Vec<String>>) -> String {
    uris.lock().unwrap().last().cloned().unwrap_or_default()
}

/// Seeking `path` to 45 s while it plays is refused (no reload, position and playback kept) and
/// says why; starting it at 30 s plays it from the start, with position 0, and says so.
fn assert_plays_from_start(player: &mut UPnPPlayer, uris: &Mutex<Vec<String>>, path: &Path) {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    player.play(path).expect("play");
    assert_eq!(player.take_error(), None, "{}: nothing to report before a seek", name);

    let loads = uris.lock().unwrap().len();
    player.seek_to(45.0).expect("seek");
    assert_eq!(
        uris.lock().unwrap().len(),
        loads,
        "{}: a refused seek doesn't reload the track",
        name
    );
    assert_eq!(player.position_seconds(), 0.0, "{}: the position stays", name);
    assert!(player.is_playing(), "{}: playback goes on", name);
    let err = player
        .take_error()
        .unwrap_or_else(|| panic!("{}: the refusal is reported", name));
    assert!(
        err.starts_with(&format!("Can't seek {} (", name)) && !err.contains("from the start"),
        "{}: {}",
        name,
        err
    );
    assert_eq!(player.take_error(), None, "{}: reported once", name);
    player.seek(10.0).expect("relative seek");
    assert_eq!(uris.lock().unwrap().len(), loads, "{}: `]` doesn't reload either", name);
    assert!(player.take_error().is_some_and(|e| e.contains("Can't seek")));

    player.play_at(path, 30.0).expect("play_at");
    let uri = last_uri(uris);
    assert!(
        uri.contains(&name) && !uri.contains("pos="),
        "{}: started from the start: {}",
        name,
        uri
    );
    assert_eq!(player.position_seconds(), 0.0);
    let err = player
        .take_error()
        .unwrap_or_else(|| panic!("{}: play_at reports it", name));
    assert!(
        err.contains(&format!("Can't seek {} (", name)) && err.ends_with("; playing it from the start"),
        "{}: {}",
        name,
        err
    );
}

#[test]
fn test_upnp_seek_in_unseekable_file_plays_from_start_and_says_so() {
    common::isolate();
    let dir = common::isolate().join("upnp_seek_fallback");
    std::fs::create_dir_all(&dir).unwrap();
    let (port, stop, uris, handle) = spawn_renderer();
    let mut player = UPnPPlayer::new(device(port), 8080);

    // A WAV whose fmt chunk is cut short, one with no data chunk, a compressed AIFC.
    let mut short_fmt = wav(2);
    short_fmt[16..20].copy_from_slice(&14u32.to_le_bytes());
    let mut no_data = wav(2);
    let data_at = no_data.windows(4).position(|w| w == b"data").unwrap();
    no_data[data_at..data_at + 4].copy_from_slice(b"junk");
    let mut ima4 = b"FORM\x00\x00\x00\x34AIFCCOMM\x00\x00\x00\x18".to_vec();
    ima4.extend_from_slice(&[0, 2, 0, 0, 0x1F, 0x40, 0, 16, 0x40, 0x0B, 0xFA, 0, 0, 0, 0, 0, 0, 0]);
    ima4.extend_from_slice(b"ima4\x00\x00SSND\x00\x00\x00\x08\x00\x00\x00\x00\x00\x00\x00\x00");
    for (name, bytes) in [
        ("short_fmt.wav", short_fmt),
        ("no_data.wav", no_data),
        ("ima4.aifc", ima4),
    ] {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        assert_plays_from_start(&mut player, &uris, &path);
    }

    // A WAV the server can seek keeps its target, and reports nothing.
    let good = dir.join("good.wav");
    std::fs::write(&good, wav(60)).unwrap();
    player.play(&good).expect("play");
    player.seek_to(45.0).expect("seek");
    assert!(last_uri(&uris).contains("?pos=45.00"), "{}", last_uri(&uris));
    assert_eq!(player.position_seconds(), 45.0);
    player.play_at(&good, 30.0).expect("play_at");
    assert!(last_uri(&uris).contains("?pos=30.00"), "{}", last_uri(&uris));
    assert_eq!(player.position_seconds(), 30.0);
    assert_eq!(player.take_error(), None);

    let _ = player.stop();
    drop(player);
    stop.store(true, Ordering::SeqCst);
    let _ = TcpStream::connect(("127.0.0.1", port));
    let _ = handle.join();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_upnp_start_from_zero_reports_nothing_when_the_load_fails() {
    common::isolate();
    let dir = common::isolate().join("upnp_seek_fallback_load_fails");
    std::fs::create_dir_all(&dir).unwrap();
    let fail_uri = Arc::new(AtomicBool::new(false));
    let (port, stop, uris, handle) = spawn_renderer_failing_uri(fail_uri.clone());
    let mut player = UPnPPlayer::new(device(port), 8080);
    let mut short_fmt = wav(2);
    short_fmt[16..20].copy_from_slice(&14u32.to_le_bytes());
    let path = dir.join("short_fmt.wav");
    std::fs::write(&path, short_fmt).unwrap();

    // The renderer refuses the URI: the transport error is what fails, and nothing claims the
    // track plays from the start or leaves the position at the target.
    fail_uri.store(true, Ordering::SeqCst);
    let err = player.play_at(&path, 30.0).expect_err("the load fails");
    assert!(err.to_string().contains("Illegal MIME-type"), "{}", err);
    assert_eq!(player.take_error(), None, "no fallback note over the transport error");
    assert_eq!(player.position_seconds(), 0.0, "not left at the target");
    assert!(!last_uri(&uris).contains("pos="));

    let _ = player.stop();
    drop(player);
    stop.store(true, Ordering::SeqCst);
    let _ = TcpStream::connect(("127.0.0.1", port));
    let _ = handle.join();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_upnp_seek_check_read_error_is_not_unseekable() {
    common::isolate();
    let dir = common::isolate().join("upnp_seek_fallback_read_error");
    std::fs::create_dir_all(&dir).unwrap();
    let (port, stop, uris, handle) = spawn_renderer();
    let mut player = UPnPPlayer::new(device(port), 8080);
    let path = dir.join("gone.wav");
    std::fs::write(&path, wav(60)).unwrap();
    player.play_at(&path, 20.0).expect("play_at");
    assert!(last_uri(&uris).contains("?pos=20.00"));
    assert_eq!(player.take_error(), None);

    // The file can't be read (a share that went away): a seek while it plays is refused with the
    // read error, not a claim that the file can't be seeked.
    std::fs::remove_file(&path).unwrap();
    let loads = uris.lock().unwrap().len();
    player.seek_to(45.0).expect("seek");
    assert_eq!(uris.lock().unwrap().len(), loads, "no reload");
    assert_eq!(player.position_seconds(), 20.0, "the position stays");
    let err = player.take_error().expect("the read error is reported");
    assert!(
        err.starts_with("Couldn't read gone.wav (") && !err.contains("Can't seek"),
        "{}",
        err
    );

    // Nothing plays to keep at a start: the URL asks for the position and the server answers.
    player.play_at(&path, 30.0).expect("play_at");
    assert!(last_uri(&uris).contains("?pos=30.00"), "{}", last_uri(&uris));
    assert_eq!(player.position_seconds(), 30.0);
    assert!(
        player.take_error().is_none_or(|e| !e.contains("Can't seek")),
        "not reported as unseekable"
    );

    let _ = player.stop();
    drop(player);
    stop.store(true, Ordering::SeqCst);
    let _ = TcpStream::connect(("127.0.0.1", port));
    let _ = handle.join();
    let _ = std::fs::remove_dir_all(&dir);
}
