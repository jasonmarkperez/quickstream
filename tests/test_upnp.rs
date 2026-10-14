use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use quickstream::discovery::DLNADevice;
use quickstream::player::upnp::{
    create_didl_lite_xml, format_seconds_to_upnp_time, parse_upnp_time_to_seconds,
    quick_xml_escape, url_decode, url_encode, UPnPClient, UPnPPlayer,
};
use quickstream::player::AudioPlayer;

#[test]
fn test_upnp_time_conversions() {
    assert_eq!(format_seconds_to_upnp_time(0.0), "00:00:00");
    assert_eq!(format_seconds_to_upnp_time(65.0), "00:01:05");
    assert_eq!(format_seconds_to_upnp_time(3665.0), "01:01:05");

    assert_eq!(parse_upnp_time_to_seconds("00:01:05"), 65.0);
    assert_eq!(parse_upnp_time_to_seconds("01:01:05"), 3665.0);
    assert_eq!(parse_upnp_time_to_seconds("00:01:05.500"), 65.5);
    assert_eq!(parse_upnp_time_to_seconds("NOT_IMPLEMENTED"), 0.0);
    assert_eq!(parse_upnp_time_to_seconds("--:--:--"), 0.0);
}

#[test]
fn test_upnp_xml_escape_and_url_encoding() {
    let unescaped = "<Rock & Roll \"Greatest\" 'Hits'>";
    let escaped = quick_xml_escape(unescaped);
    assert_eq!(
        escaped,
        "&lt;Rock &amp; Roll &quot;Greatest&quot; &apos;Hits&apos;&gt;"
    );

    let raw = "/Music/Rock & Roll/01. Track.flac";
    let encoded = url_encode(raw);
    assert!(!encoded.contains(' '));
    assert!(!encoded.contains('/'));
    assert!(!encoded.contains('&'));
    let decoded = url_decode(&encoded);
    assert_eq!(decoded, raw);
}

#[test]
fn test_didl_lite_metadata_generation() {
    let xml = create_didl_lite_xml(
        "http://192.168.1.50:8080/stream/song.flac",
        "A Nice Song & Melody",
        "The Band",
        "First Record",
        185.0,
        "audio/flac",
    );

    assert!(xml.contains("<DIDL-Lite"));
    assert!(xml.contains("<dc:title>A Nice Song &amp; Melody</dc:title>"));
    assert!(xml.contains("<dc:creator>The Band</dc:creator>"));
    assert!(xml.contains("<upnp:album>First Record</upnp:album>"));
    assert!(xml.contains("http://192.168.1.50:8080/stream/song.flac"));
    assert!(xml.contains("protocolInfo=\"http-get:*:audio/flac:*\""));
    assert!(xml.contains("duration=\"00:03:05\""));
}

/// Spawns a mock UPnP MediaRenderer listening for SOAP HTTP POST requests.
fn spawn_mock_upnp_renderer() -> (u16, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock renderer");
    let port = listener.local_addr().unwrap().port();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_thread = stop_flag.clone();

    let handle = std::thread::spawn(move || {
        while !stop_flag_thread.load(Ordering::SeqCst) {
            listener
                .set_nonblocking(true)
                .expect("set nonblocking");
            let stream_opt = match listener.accept() {
                Ok((s, _)) => Some(s),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(15));
                    continue;
                }
                Err(_) => break,
            };

            if let Some(mut stream) = stream_opt {
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
                    continue;
                }

                let mut soap_action = String::new();
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                        break;
                    }
                    let trimmed = line.trim();
                    if trimmed.to_lowercase().starts_with("soapaction:") {
                        soap_action = trimmed.split(':').skip(1).collect::<Vec<_>>().join(":");
                    } else if trimmed.to_lowercase().starts_with("content-length:") {
                        if let Some(v) = trimmed.split(':').nth(1) {
                            content_length = v.trim().parse().unwrap_or(0);
                        }
                    }
                }

                let mut body = vec![0u8; content_length];
                if content_length > 0 {
                    let _ = reader.read_exact(&mut body);
                }

                // Generate appropriate mock SOAP response
                let resp_body = if soap_action.contains("GetTransportInfo") {
                    r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:GetTransportInfoResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><CurrentTransportState>PLAYING</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed></u:GetTransportInfoResponse></s:Body></s:Envelope>"#
                } else if soap_action.contains("GetPositionInfo") {
                    r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:GetPositionInfoResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><TrackDuration>00:03:05</TrackDuration><RelTime>00:00:45</RelTime></u:GetPositionInfoResponse></s:Body></s:Envelope>"#
                } else if soap_action.contains("GetVolume") {
                    r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:GetVolumeResponse xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1"><CurrentVolume>65</CurrentVolume></u:GetVolumeResponse></s:Body></s:Envelope>"#
                } else if soap_action.contains("SetAVTransportURI") {
                    r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:SetAVTransportURIResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1" /></s:Body></s:Envelope>"#
                } else if soap_action.contains("Play") {
                    r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:PlayResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1" /></s:Body></s:Envelope>"#
                } else if soap_action.contains("Pause") {
                    r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:PauseResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1" /></s:Body></s:Envelope>"#
                } else if soap_action.contains("Stop") {
                    r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:StopResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1" /></s:Body></s:Envelope>"#
                } else if soap_action.contains("Seek") {
                    r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:SeekResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1" /></s:Body></s:Envelope>"#
                } else if soap_action.contains("SetVolume") {
                    r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:SetVolumeResponse xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1" /></s:Body></s:Envelope>"#
                } else {
                    r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:SuccessResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1" /></s:Body></s:Envelope>"#
                };

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    resp_body.len(),
                    resp_body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        }
    });

    (port, stop_flag, handle)
}

#[test]
fn test_upnp_client_soap_methods() {
    let (port, stop_flag, handle) = spawn_mock_upnp_renderer();

    let device = DLNADevice {
        udn: "uuid:test-stereo-123".to_string(),
        friendly_name: "Mock HiFi Stereo".to_string(),
        model_name: "MockRenderer".to_string(),
        manufacturer: "Quickstream".to_string(),
        host: "127.0.0.1".to_string(),
        port,
        location: format!("http://127.0.0.1:{}/desc.xml", port),
        av_transport_control_url: format!("http://127.0.0.1:{}/ctrl/AVTransport", port),
        rendering_control_url: format!("http://127.0.0.1:{}/ctrl/RenderingControl", port),
    };

    let client = UPnPClient::new(device);

    // 1. SetAVTransportURI
    assert!(client.set_transport_uri(
        "http://127.0.0.1:8080/stream/song.mp3",
        "Test Track",
        "Test Artist",
        "Test Album",
        185.0,
        "audio/mpeg"
    ));

    // 2. Play
    assert!(client.play("1"));

    // 3. Pause
    assert!(client.pause());

    // 4. Seek
    assert!(client.seek(45.0));

    // 5. GetPositionInfo
    let (rel, dur) = client.get_position_info();
    assert_eq!(rel, 45.0);
    assert_eq!(dur, 185.0);

    // 6. GetTransportInfo
    assert_eq!(client.get_transport_info(), "PLAYING");

    // 7. Volume
    assert_eq!(client.get_volume(), Some(65));
    assert!(client.set_volume(80));

    // 8. Stop
    assert!(client.stop());

    stop_flag.store(true, Ordering::SeqCst);
    let _ = TcpStream::connect(format!("127.0.0.1:{}", port));
    let _ = handle.join();
}

#[test]
fn test_upnp_player_audio_player_trait_lifecycle() {
    let (port, stop_flag, handle) = spawn_mock_upnp_renderer();

    let device = DLNADevice {
        udn: "uuid:test-stereo-456".to_string(),
        friendly_name: "Denon AVR".to_string(),
        model_name: "AVR-X".to_string(),
        manufacturer: "Denon".to_string(),
        host: "127.0.0.1".to_string(),
        port,
        location: format!("http://127.0.0.1:{}/desc.xml", port),
        av_transport_control_url: format!("http://127.0.0.1:{}/ctrl/AVTransport", port),
        rendering_control_url: format!("http://127.0.0.1:{}/ctrl/RenderingControl", port),
    };

    let mut player = UPnPPlayer::new(device, 8080);

    // Create a dummy audio file
    let temp_file = std::env::temp_dir().join("test_upnp_track.mp3");
    let _ = std::fs::write(&temp_file, b"DUMMY_AUDIO_DATA");

    // Play track via AudioPlayer trait
    assert!(player.play(&temp_file).is_ok());
    assert!(player.is_playing());
    assert!(!player.is_paused());

    // Pause
    assert!(player.pause().is_ok());
    assert!(!player.is_playing());
    assert!(player.is_paused());

    // Resume
    assert!(player.resume().is_ok());
    assert!(player.is_playing());
    assert!(!player.is_paused());

    // Toggle pause
    assert!(player.toggle_pause().is_ok());
    assert!(player.is_paused());
    assert!(player.toggle_pause().is_ok());
    assert!(player.is_playing());

    // Volume
    assert!(player.set_volume(70).is_ok());
    assert_eq!(player.volume(), 70);

    // Seek
    assert!(player.seek_to(30.0).is_ok());
    assert_eq!(player.position_seconds(), 30.0);

    // Stop
    assert!(player.stop().is_ok());
    assert!(!player.is_playing());
    assert!(!player.is_paused());

    let _ = std::fs::remove_file(temp_file);
    stop_flag.store(true, Ordering::SeqCst);
    let _ = TcpStream::connect(format!("127.0.0.1:{}", port));
    let _ = handle.join();
}

#[test]
fn test_upnp_player_seek_mode_not_supported_fallback() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock renderer");
    let port = listener.local_addr().unwrap().port();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_thread = stop_flag.clone();
    let last_set_uri = Arc::new(std::sync::Mutex::new(String::new()));
    let last_set_uri_thread = last_set_uri.clone();

    let handle = std::thread::spawn(move || {
        while !stop_flag_thread.load(Ordering::SeqCst) {
            listener.set_nonblocking(true).expect("set nonblocking");
            let stream_opt = match listener.accept() {
                Ok((s, _)) => Some(s),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(15));
                    continue;
                }
                Err(_) => break,
            };

            if let Some(mut stream) = stream_opt {
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
                    continue;
                }

                let mut soap_action = String::new();
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                        break;
                    }
                    let trimmed = line.trim();
                    if trimmed.to_lowercase().starts_with("soapaction:") {
                        soap_action = trimmed.split(':').skip(1).collect::<Vec<_>>().join(":");
                    } else if trimmed.to_lowercase().starts_with("content-length:") {
                        if let Some(v) = trimmed.split(':').nth(1) {
                            content_length = v.trim().parse().unwrap_or(0);
                        }
                    }
                }

                let mut body = vec![0u8; content_length];
                if content_length > 0 {
                    let _ = reader.read_exact(&mut body);
                }
                let body_str = String::from_utf8_lossy(&body);

                if soap_action.contains("SetAVTransportURI") {
                    if let Some(start) = body_str.find("<CurrentURI>") {
                        if let Some(end) = body_str[start..].find("</CurrentURI>") {
                            let uri = &body_str[start + 12..start + end];
                            *last_set_uri_thread.lock().unwrap() = uri.to_string();
                        }
                    }
                }

                let (status_line, resp_body) = if soap_action.contains("Seek") {
                    // Simulate Primare / DLNA renderer returning UPnP 710 Seek Mode Not Supported
                    (
                        "HTTP/1.1 500 Internal Server Error",
                        r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>710</errorCode><errorDescription>Seek Mode Not Supported</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#.to_string()
                    )
                } else if soap_action.contains("GetTransportInfo") {
                    ("HTTP/1.1 200 OK", r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:GetTransportInfoResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><CurrentTransportState>PLAYING</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed></u:GetTransportInfoResponse></s:Body></s:Envelope>"#.to_string())
                } else if soap_action.contains("GetPositionInfo") {
                    ("HTTP/1.1 200 OK", r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:GetPositionInfoResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><TrackDuration>00:03:05</TrackDuration><RelTime>00:00:02</RelTime></u:GetPositionInfoResponse></s:Body></s:Envelope>"#.to_string())
                } else {
                    ("HTTP/1.1 200 OK", r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:SuccessResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1" /></s:Body></s:Envelope>"#.to_string())
                };

                let response = format!(
                    "{}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status_line,
                    resp_body.len(),
                    resp_body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        }
    });

    let device = DLNADevice {
        udn: "uuid:test-primare-710".to_string(),
        friendly_name: "Primare I15".to_string(),
        model_name: "I15 Prisma".to_string(),
        manufacturer: "Primare".to_string(),
        host: "127.0.0.1".to_string(),
        port,
        location: format!("http://127.0.0.1:{}/desc.xml", port),
        av_transport_control_url: format!("http://127.0.0.1:{}/ctrl/AVTransport", port),
        rendering_control_url: format!("http://127.0.0.1:{}/ctrl/RenderingControl", port),
    };

    let mut player = UPnPPlayer::new(device, 8080);
    let temp_file = std::env::temp_dir().join("test_upnp_fallback_track.mp3");
    let _ = std::fs::write(&temp_file, b"DUMMY_AUDIO_DATA");

    assert!(player.play(&temp_file).is_ok());
    assert!(player.is_playing());

    // Initial SetAVTransportURI should NOT have ?pos=
    let init_uri = last_set_uri.lock().unwrap().clone();
    assert!(!init_uri.contains("?pos="), "Initial URI should not have ?pos=");

    // Perform seek_to(45.0) which fails with 710 on SOAP Seek, triggering reload
    let seek_res = player.seek_to(45.0);
    assert!(seek_res.is_ok(), "seek_to should succeed via stream offset fallback: {:?}", seek_res);

    // Verify URI was reloaded with ?pos=45.00
    let reloaded_uri = last_set_uri.lock().unwrap().clone();
    assert!(
        reloaded_uri.contains("?pos=45.00") || reloaded_uri.contains("?pos=45"),
        "Reloaded URI must contain ?pos=45.00, got: {}",
        reloaded_uri
    );
    assert_eq!(player.position_seconds(), 45.0);
    assert!(player.is_playing());

    let _ = player.stop();
    let _ = std::fs::remove_file(temp_file);
    stop_flag.store(true, Ordering::SeqCst);
    let _ = TcpStream::connect(format!("127.0.0.1:{}", port));
    let _ = handle.join();
}
