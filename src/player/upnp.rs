//! UPnP / DLNA MediaRenderer playback implementation using SOAP AVTransport and RenderingControl.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::discovery::{extract_xml_tag, get_local_ip_for_target, DLNADevice};
use crate::metadata::{extract_metadata, get_audio_mime_type};
use crate::player::AudioPlayer;

pub const AV_TRANSPORT_SERVICE: &str = "urn:schemas-upnp-org:service:AVTransport:1";
pub const RENDERING_CONTROL_SERVICE: &str = "urn:schemas-upnp-org:service:RenderingControl:1";

/// Format seconds into UPnP time string "HH:MM:SS".
pub fn format_seconds_to_upnp_time(seconds: f64) -> String {
    let total_secs = seconds.max(0.0) as u64;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

/// Parse UPnP time string "HH:MM:SS" or "HH:MM:SS.xxx" to seconds.
pub fn parse_upnp_time_to_seconds(time_str: &str) -> f64 {
    let trimmed = time_str.trim();
    if trimmed.is_empty() || trimmed == "NOT_IMPLEMENTED" || trimmed == "--:--:--" {
        return 0.0;
    }
    let parts: Vec<&str> = trimmed.split(':').collect();
    if parts.len() == 3 {
        let h: f64 = parts[0].parse().unwrap_or(0.0);
        let m: f64 = parts[1].parse().unwrap_or(0.0);
        let s: f64 = parts[2].parse().unwrap_or(0.0);
        h * 3600.0 + m * 60.0 + s
    } else if parts.len() == 2 {
        let m: f64 = parts[0].parse().unwrap_or(0.0);
        let s: f64 = parts[1].parse().unwrap_or(0.0);
        m * 60.0 + s
    } else {
        0.0
    }
}

/// Basic XML character escaping.
pub fn quick_xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Percent-encode string for safe URL path components.
pub fn url_encode(input: &str) -> String {
    let mut out = String::new();
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Percent-decode URL path component.
pub fn url_decode(input: &str) -> String {
    let mut bytes = Vec::new();
    let input_bytes = input.as_bytes();
    let mut i = 0;
    while i < input_bytes.len() {
        if input_bytes[i] == b'%' && i + 2 < input_bytes.len() {
            if let Ok(val) = u8::from_str_radix(std::str::from_utf8(&input_bytes[i + 1..i + 3]).unwrap_or(""), 16) {
                bytes.push(val);
                i += 3;
                continue;
            }
        } else if input_bytes[i] == b'+' {
            bytes.push(b' ');
            i += 1;
            continue;
        }
        bytes.push(input_bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&bytes).to_string()
}

/// Generate standard UPnP DIDL-Lite metadata XML for SetAVTransportURI.
pub fn create_didl_lite_xml(
    stream_url: &str,
    title: &str,
    artist: &str,
    album: &str,
    duration_sec: f64,
    mime_type: &str,
) -> String {
    let album_xml = if !album.is_empty() {
        format!("<upnp:album>{}</upnp:album>", quick_xml_escape(album))
    } else {
        String::new()
    };
    let duration_attr = if duration_sec > 0.0 {
        format!(" duration=\"{}\"", format_seconds_to_upnp_time(duration_sec))
    } else {
        String::new()
    };

    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
        xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
        xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
        <item id=\"0\" parentID=\"-1\" restricted=\"false\">\
        <dc:title>{}</dc:title>\
        <dc:creator>{}</dc:creator>\
        {}\
        <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
        <res protocolInfo=\"http-get:*:{}:*\"{}>{}</res>\
        </item></DIDL-Lite>",
        quick_xml_escape(title),
        quick_xml_escape(artist),
        album_xml,
        mime_type,
        duration_attr,
        quick_xml_escape(stream_url)
    )
}

/// SOAP XML client controlling DLNA MediaRenderers.
pub struct UPnPClient {
    pub device: DLNADevice,
    pub timeout: Duration,
    pub last_error: Arc<Mutex<String>>,
}

impl UPnPClient {
    pub fn new(device: DLNADevice) -> Self {
        Self {
            device,
            timeout: Duration::from_millis(4000),
            last_error: Arc::new(Mutex::new(String::new())),
        }
    }

    /// Send a UPnP SOAP POST action using default timeout.
    pub fn send_soap_action(
        &self,
        control_url: &str,
        service_type: &str,
        action: &str,
        arguments: &[(&str, &str)],
    ) -> Option<String> {
        self.send_soap_action_with_timeout(control_url, service_type, action, arguments, self.timeout)
    }

    /// Send a UPnP SOAP POST action with custom timeout and return response XML body.
    pub fn send_soap_action_with_timeout(
        &self,
        control_url: &str,
        service_type: &str,
        action: &str,
        arguments: &[(&str, &str)],
        timeout: Duration,
    ) -> Option<String> {
        if control_url.is_empty() {
            *self.last_error.lock().unwrap() = "Missing control URL".to_string();
            return None;
        }

        let mut args_xml = String::new();
        for (k, v) in arguments {
            args_xml.push_str(&format!("<{}>{}</{}>", k, quick_xml_escape(v), k));
        }

        let body = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n\
            <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\r\n\
              <s:Body>\r\n\
                <u:{} xmlns:u=\"{}\">\r\n\
                  {}\r\n\
                </u:{}>\r\n\
              </s:Body>\r\n\
            </s:Envelope>",
            action, service_type, args_xml, action
        );

        let url_trimmed = control_url.trim();
        let without_proto = match url_trimmed.strip_prefix("http://") {
            Some(p) => p,
            None => {
                *self.last_error.lock().unwrap() = format!("Invalid control URL: {}", control_url);
                return None;
            }
        };
        let (host_port, path) = if let Some(slash) = without_proto.find('/') {
            (&without_proto[..slash], &without_proto[slash..])
        } else {
            (without_proto, "/")
        };

        let (host, port) = if let Some(colon) = host_port.find(':') {
            let h = &host_port[..colon];
            match host_port[colon + 1..].parse::<u16>() {
                Ok(p) => (h, p),
                Err(e) => {
                    *self.last_error.lock().unwrap() = format!("Invalid port in {}: {}", control_url, e);
                    return None;
                }
            }
        } else {
            (host_port, 80)
        };

        let addrs = match (host, port).to_socket_addrs() {
            Ok(a) => a,
            Err(e) => {
                *self.last_error.lock().unwrap() = format!("Address resolution failed for {}:{}: {}", host, port, e);
                return None;
            }
        };

        let mut stream = None;
        for addr in addrs {
            if let Ok(s) = TcpStream::connect_timeout(&addr, timeout) {
                let _ = s.set_read_timeout(Some(timeout));
                let _ = s.set_write_timeout(Some(timeout));
                stream = Some(s);
                break;
            }
        }
        let mut stream = match stream {
            Some(s) => s,
            None => {
                *self.last_error.lock().unwrap() = format!("Connection timed out to {}:{}", host, port);
                return None;
            }
        };

        let request = format!(
            "POST {} HTTP/1.1\r\n\
            Host: {}\r\n\
            Content-Type: text/xml; charset=\"utf-8\"\r\n\
            SOAPAction: \"{}#{}\"\r\n\
            User-Agent: Quickstream/0.2.0 UPnP/1.1\r\n\
            Content-Length: {}\r\n\
            Connection: close\r\n\r\n\
            {}",
            path, host_port, service_type, action, body.len(), body
        );

        if let Err(e) = stream.write_all(request.as_bytes()) {
            *self.last_error.lock().unwrap() = format!("Write failed: {}", e);
            return None;
        }
        let _ = stream.flush();

        let mut response = Vec::new();
        if let Err(e) = stream.read_to_end(&mut response) {
            *self.last_error.lock().unwrap() = format!("SOAP read timeout/error: {}", e);
            return None;
        }
        let resp_str = String::from_utf8_lossy(&response).to_string();

        if let Some(pos) = resp_str.find("\r\n\r\n") {
            let header_part = &resp_str[..pos];
            let body_part = &resp_str[pos + 4..];

            if let Some(status_line) = header_part.lines().next() {
                if !status_line.contains("200") {
                    let err = extract_xml_tag(body_part, "errorDescription")
                        .or_else(|| extract_xml_tag(body_part, "errorCode"))
                        .unwrap_or_else(|| status_line.to_string());
                    *self.last_error.lock().unwrap() = err;
                    return None;
                }
            }
            Some(body_part.to_string())
        } else {
            Some(resp_str)
        }
    }

    /// Call SetAVTransportURI to point the renderer to our stream URL (with 20s timeout for renderer media probe).
    pub fn set_transport_uri(
        &self,
        stream_url: &str,
        title: &str,
        artist: &str,
        album: &str,
        duration_sec: f64,
        mime_type: &str,
    ) -> bool {
        let didl = create_didl_lite_xml(
            stream_url,
            title,
            artist,
            album,
            duration_sec,
            mime_type,
        );

        crate::log_info!("upnp", "Calling SetAVTransportURI on {} with stream URL: {}", self.device.friendly_name, stream_url);

        for attempt in 0..3 {
            let res = self.send_soap_action_with_timeout(
                &self.device.av_transport_control_url,
                AV_TRANSPORT_SERVICE,
                "SetAVTransportURI",
                &[
                    ("InstanceID", "0"),
                    ("CurrentURI", stream_url),
                    ("CurrentURIMetaData", &didl),
                ],
                Duration::from_secs(20),
            );

            if res.is_some() {
                crate::log_info!("upnp", "SetAVTransportURI with DIDL succeeded on {}", self.device.friendly_name);
                return true;
            }

            let err = self.last_error.lock().unwrap().clone();
            if (err.contains("701") || err.contains("Transition not available")) && attempt < 2 {
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
            break;
        }

        crate::log_info!(
            "upnp",
            "SetAVTransportURI with DIDL failed on {} ('{}'), retrying with empty metadata...",
            self.device.friendly_name,
            self.last_error.lock().unwrap()
        );

        // Fallback: Retry with empty CurrentURIMetaData (many stereos reject custom DIDL)
        let res_fallback = self.send_soap_action_with_timeout(
            &self.device.av_transport_control_url,
            AV_TRANSPORT_SERVICE,
            "SetAVTransportURI",
            &[
                ("InstanceID", "0"),
                ("CurrentURI", stream_url),
                ("CurrentURIMetaData", ""),
            ],
            Duration::from_secs(20),
        );

        if res_fallback.is_some() {
            crate::log_info!("upnp", "SetAVTransportURI with empty metadata succeeded on {}", self.device.friendly_name);
            true
        } else {
            crate::log_info!(
                "upnp",
                "SetAVTransportURI fallback also failed on {}: {}",
                self.device.friendly_name,
                self.last_error.lock().unwrap()
            );
            false
        }
    }

    /// Send Play command with retries if renderer is transitioning.
    pub fn play(&self, speed: &str) -> bool {
        for _ in 0..3 {
            let res = self.send_soap_action(
                &self.device.av_transport_control_url,
                AV_TRANSPORT_SERVICE,
                "Play",
                &[("InstanceID", "0"), ("Speed", speed)],
            );
            if res.is_some() {
                return true;
            }
            let err = self.last_error.lock().unwrap().clone();
            if err.contains("701") || err.contains("Transition not available") {
                std::thread::sleep(Duration::from_millis(300));
                continue;
            }
            break;
        }
        false
    }

    /// Send Pause command with fallback to Stop if unsupported.
    pub fn pause(&self) -> bool {
        let res = self.send_soap_action(
            &self.device.av_transport_control_url,
            AV_TRANSPORT_SERVICE,
            "Pause",
            &[("InstanceID", "0")],
        );
        if res.is_some() {
            return true;
        }
        let err = self.last_error.lock().unwrap().clone();
        if err.contains("401") || err.contains("Invalid Action") || err.contains("501") || err.contains("Not Implemented") {
            return self.stop();
        }
        // Fallback: If Pause failed for any reason on a progressive stream, try Stop
        self.stop()
    }

    /// Send Stop command.
    pub fn stop(&self) -> bool {
        let res = self.send_soap_action(
            &self.device.av_transport_control_url,
            AV_TRANSPORT_SERVICE,
            "Stop",
            &[("InstanceID", "0")],
        );
        if res.is_some() {
            return true;
        }
        let err = self.last_error.lock().unwrap().clone();
        if err.contains("701") || err.contains("Transition not available") {
            return true;
        }
        false
    }

    /// Seek to specific position in track (REL_TIME).
    pub fn seek(&self, target_seconds: f64) -> bool {
        let target_str = format_seconds_to_upnp_time(target_seconds);
        let res = self.send_soap_action(
            &self.device.av_transport_control_url,
            AV_TRANSPORT_SERVICE,
            "Seek",
            &[
                ("InstanceID", "0"),
                ("Unit", "REL_TIME"),
                ("Target", &target_str),
            ],
        );
        res.is_some()
    }

    /// Get current playback progress and duration (returns (rel_time_seconds, duration_seconds)).
    pub fn get_position_info(&self) -> (f64, f64) {
        let res = self.send_soap_action(
            &self.device.av_transport_control_url,
            AV_TRANSPORT_SERVICE,
            "GetPositionInfo",
            &[("InstanceID", "0")],
        );
        if let Some(xml) = res {
            let rel_time = extract_xml_tag(&xml, "RelTime").unwrap_or_default();
            let duration = extract_xml_tag(&xml, "TrackDuration").unwrap_or_default();
            (
                parse_upnp_time_to_seconds(&rel_time),
                parse_upnp_time_to_seconds(&duration),
            )
        } else {
            (0.0, 0.0)
        }
    }

    /// Get current transport state (e.g. "PLAYING", "PAUSED_PLAYBACK", "STOPPED").
    pub fn get_transport_info(&self) -> String {
        let res = self.send_soap_action(
            &self.device.av_transport_control_url,
            AV_TRANSPORT_SERVICE,
            "GetTransportInfo",
            &[("InstanceID", "0")],
        );
        if let Some(xml) = res {
            extract_xml_tag(&xml, "CurrentTransportState").unwrap_or_else(|| "STOPPED".to_string())
        } else {
            "STOPPED".to_string()
        }
    }

    /// Set volume (0..100) via RenderingControl.
    pub fn set_volume(&self, volume: u8) -> bool {
        if self.device.rendering_control_url.is_empty() {
            return false;
        }
        let vol_str = volume.min(100).to_string();
        let res = self.send_soap_action(
            &self.device.rendering_control_url,
            RENDERING_CONTROL_SERVICE,
            "SetVolume",
            &[
                ("InstanceID", "0"),
                ("Channel", "Master"),
                ("DesiredVolume", &vol_str),
            ],
        );
        res.is_some()
    }

    /// Get current volume (0..100) via RenderingControl.
    pub fn get_volume(&self) -> Option<u8> {
        if self.device.rendering_control_url.is_empty() {
            return None;
        }
        let res = self.send_soap_action(
            &self.device.rendering_control_url,
            RENDERING_CONTROL_SERVICE,
            "GetVolume",
            &[("InstanceID", "0"), ("Channel", "Master")],
        );
        if let Some(xml) = res {
            if let Some(val_str) = extract_xml_tag(&xml, "CurrentVolume") {
                return val_str.parse::<u8>().ok();
            }
        }
        None
    }
}

/// An `AudioPlayer` implementation streaming audio over DLNA/UPnP to remote renderers.
pub struct UPnPPlayer {
    pub device: DLNADevice,
    client: Arc<UPnPClient>,
    web_port: u16,
    stream_state: Option<Arc<crate::web::StreamServerState>>,
    is_playing: Arc<AtomicBool>,
    is_paused: Arc<AtomicBool>,
    position_seconds: Arc<Mutex<f64>>,
    duration_seconds: Arc<Mutex<f64>>,
    volume: Arc<Mutex<u8>>,
    track_finished: Arc<AtomicBool>,
    poll_stop_flag: Arc<AtomicBool>,
    current_filepath: Arc<Mutex<Option<PathBuf>>>,
    playback_stream_offset: Arc<Mutex<f64>>,
    last_seek_time: Arc<Mutex<Instant>>,
}

impl UPnPPlayer {
    pub fn new(device: DLNADevice, web_port: u16) -> Self {
        Self::new_with_stream_state(device, web_port, None)
    }

    pub fn new_with_stream_state(
        device: DLNADevice,
        web_port: u16,
        stream_state: Option<Arc<crate::web::StreamServerState>>,
    ) -> Self {
        let client = Arc::new(UPnPClient::new(device.clone()));
        let init_vol = client.get_volume().unwrap_or(20);
        Self {
            device,
            client,
            web_port,
            stream_state,
            is_playing: Arc::new(AtomicBool::new(false)),
            is_paused: Arc::new(AtomicBool::new(false)),
            position_seconds: Arc::new(Mutex::new(0.0)),
            duration_seconds: Arc::new(Mutex::new(0.0)),
            volume: Arc::new(Mutex::new(init_vol)),
            track_finished: Arc::new(AtomicBool::new(false)),
            poll_stop_flag: Arc::new(AtomicBool::new(false)),
            current_filepath: Arc::new(Mutex::new(None)),
            playback_stream_offset: Arc::new(Mutex::new(0.0)),
            last_seek_time: Arc::new(Mutex::new(Instant::now() - Duration::from_secs(10))),
        }
    }

    pub fn with_stream_state(mut self, stream_state: Option<Arc<crate::web::StreamServerState>>) -> Self {
        self.stream_state = stream_state;
        self
    }

    pub fn set_stream_state(&mut self, stream_state: Option<Arc<crate::web::StreamServerState>>) {
        self.stream_state = stream_state;
    }

    fn start_polling_loop(&mut self) {
        self.poll_stop_flag.store(true, Ordering::SeqCst);
        self.poll_stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag = self.poll_stop_flag.clone();
        let client = self.client.clone();
        let pos_ref = self.position_seconds.clone();
        let dur_ref = self.duration_seconds.clone();
        let is_playing_ref = self.is_playing.clone();
        let is_paused_ref = self.is_paused.clone();
        let finished_ref = self.track_finished.clone();
        let offset_ref = self.playback_stream_offset.clone();
        let last_seek_ref = self.last_seek_time.clone();

        std::thread::spawn(move || {
            let mut consecutive_stopped = 0;
            while !stop_flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(900));
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }

                let (rel_sec, dur_sec) = client.get_position_info();
                if dur_sec > 0.0 {
                    *dur_ref.lock().unwrap() = dur_sec;
                }

                // If user just sought within 2.5s, ignore stale renderer reports to prevent progress bar jitter
                let time_since_seek = last_seek_ref.lock().unwrap().elapsed();
                if time_since_seek >= Duration::from_millis(2500) {
                    let stream_offset = *offset_ref.lock().unwrap();
                    let actual_pos = if stream_offset > 0.0 {
                        stream_offset + rel_sec
                    } else {
                        rel_sec
                    };
                    *pos_ref.lock().unwrap() = if dur_sec > 0.0 {
                        actual_pos.min(dur_sec)
                    } else {
                        actual_pos
                    };
                }

                let state = client.get_transport_info();
                match state.as_str() {
                    "PLAYING" => {
                        consecutive_stopped = 0;
                        is_playing_ref.store(true, Ordering::SeqCst);
                        is_paused_ref.store(false, Ordering::SeqCst);
                    }
                    "PAUSED_PLAYBACK" => {
                        consecutive_stopped = 0;
                        is_playing_ref.store(false, Ordering::SeqCst);
                        is_paused_ref.store(true, Ordering::SeqCst);
                    }
                    "STOPPED" | "NO_MEDIA_PRESENT" => {
                        // If we are within 2.5s of a seek, ignore STOPPED states (renderer is transitioning)
                        if time_since_seek < Duration::from_millis(2500) {
                            consecutive_stopped = 0;
                            continue;
                        }

                        // If user has explicitly paused playback, the renderer being STOPPED
                        // (e.g. pause-via-stop fallback or socket close) is expected and does NOT mean track finished!
                        if is_paused_ref.load(Ordering::SeqCst) {
                            consecutive_stopped = 0;
                            continue;
                        }

                        consecutive_stopped += 1;
                        let dur = *dur_ref.lock().unwrap();
                        let pos = *pos_ref.lock().unwrap();
                        let near_end = dur > 0.0 && pos >= (dur - 5.0).max(0.0);

                        if consecutive_stopped >= 2
                            && is_playing_ref.load(Ordering::SeqCst)
                            && !is_paused_ref.load(Ordering::SeqCst)
                            && near_end
                        {
                            // Track has genuinely finished playing on the remote renderer
                            is_playing_ref.store(false, Ordering::SeqCst);
                            is_paused_ref.store(false, Ordering::SeqCst);
                            finished_ref.store(true, Ordering::SeqCst);
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    fn seek_via_stream_offset(&mut self, target: f64) -> anyhow::Result<()> {
        let filepath = match self.current_filepath.lock().unwrap().clone() {
            Some(p) => p,
            None => anyhow::bail!("No track currently loaded"),
        };

        if self.poll_stop_flag.load(Ordering::Relaxed) {
            self.start_polling_loop();
        }

        // Close prior active streaming TCP connection so renderer resets its HTTP buffer
        if let Some(ref ss) = self.stream_state {
            ss.close_active_streams();
            ss.register_file(&filepath);
        }

        // Stop current renderer playback
        let _ = self.client.stop();
        std::thread::sleep(Duration::from_millis(50));

        // Build stream URL with pos offset parameter
        let host_ip = get_local_ip_for_target(&self.device.host, self.device.port);
        let file_name = filepath
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "track".to_string());
        let encoded_name = url_encode(&file_name);
        let stream_url = format!(
            "http://{}:{}/stream/{}?pos={:.2}",
            host_ip, self.web_port, encoded_name, target
        );
        crate::log_info!("player", "Reloading UPnP transport URI with seek offset: {}", stream_url);

        let mime = get_audio_mime_type(&filepath);
        let (title, artist, album, duration) = if let Ok(m) = extract_metadata(&filepath) {
            (m.title, m.artist, m.album, m.duration_seconds)
        } else {
            (file_name.clone(), String::new(), String::new(), 0.0)
        };

        // Set transport URI with offset URL
        if !self.client.set_transport_uri(&stream_url, &title, &artist, &album, duration, mime) {
            let err = self.client.last_error.lock().unwrap().clone();
            crate::log_warn!("player", "SetAVTransportURI with seek offset failed: {}", err);
            anyhow::bail!("SetAVTransportURI offset failed: {}", err);
        }

        // Send Play command
        if !self.client.play("1") {
            let err = self.client.last_error.lock().unwrap().clone();
            crate::log_warn!("player", "Play after seek offset failed: {}", err);
            anyhow::bail!("Play after seek offset failed: {}", err);
        }

        *self.playback_stream_offset.lock().unwrap() = target;
        *self.position_seconds.lock().unwrap() = target;
        *self.last_seek_time.lock().unwrap() = Instant::now();
        self.is_playing.store(true, Ordering::SeqCst);
        self.is_paused.store(false, Ordering::SeqCst);

        crate::log_info!("player", "UPnP stream offset seek to {:.2}s successfully started", target);
        Ok(())
    }
}

impl Drop for UPnPPlayer {
    fn drop(&mut self) {
        self.poll_stop_flag.store(true, Ordering::SeqCst);
        if let Some(ref ss) = self.stream_state {
            ss.close_active_streams();
        }
        let _ = self.client.stop();
    }
}

impl AudioPlayer for UPnPPlayer {
    fn play(&mut self, filepath: &Path) -> anyhow::Result<()> {
        let _timer = crate::log_perf!(
            "player",
            "UPnPPlayer::play '{}' on '{}'",
            filepath.display(),
            self.device.friendly_name
        );

        // 1. Terminate any previous polling thread, close streaming sockets, & stop renderer
        self.poll_stop_flag.store(true, Ordering::SeqCst);
        if let Some(ref ss) = self.stream_state {
            ss.close_active_streams();
            ss.register_file(filepath);
        }
        let _ = self.client.stop();
        std::thread::sleep(Duration::from_millis(50));

        // 2. Resolve local IP routable to renderer and construct stream URL
        let host_ip = get_local_ip_for_target(&self.device.host, self.device.port);
        let file_name = filepath
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "track".to_string());
        let encoded_name = url_encode(&file_name);
        let stream_url = format!("http://{}:{}/stream/{}", host_ip, self.web_port, encoded_name);
        crate::log_info!("player", "UPnP stream URL for '{}': {}", filepath.display(), stream_url);

        // 3. Extract audio metadata & MIME type
        let mime = get_audio_mime_type(filepath);
        let (title, artist, album, duration) = if let Ok(m) = extract_metadata(filepath) {
            (m.title, m.artist, m.album, m.duration_seconds)
        } else {
            (file_name.clone(), String::new(), String::new(), 0.0)
        };

        *self.duration_seconds.lock().unwrap() = duration;
        *self.position_seconds.lock().unwrap() = 0.0;
        *self.playback_stream_offset.lock().unwrap() = 0.0;
        *self.last_seek_time.lock().unwrap() = Instant::now() - Duration::from_secs(10);
        *self.current_filepath.lock().unwrap() = Some(filepath.to_path_buf());
        self.track_finished.store(false, Ordering::SeqCst);

        // 4. Set URI on stereo
        if !self.client.set_transport_uri(&stream_url, &title, &artist, &album, duration, mime) {
            let err = self.client.last_error.lock().unwrap().clone();
            crate::log_info!("player", "SetAVTransportURI failed on {}: {}", self.device.friendly_name, err);
            anyhow::bail!("SetAVTransportURI failed on {}: {}", self.device.friendly_name, err);
        }

        // 5. Send Play command
        if !self.client.play("1") {
            let err = self.client.last_error.lock().unwrap().clone();
            crate::log_info!("player", "Play command failed on {}: {}", self.device.friendly_name, err);
            anyhow::bail!("Play command failed on {}: {}", self.device.friendly_name, err);
        }

        crate::log_info!("player", "UPnP playback successfully started on {}", self.device.friendly_name);

        self.is_playing.store(true, Ordering::SeqCst);
        self.is_paused.store(false, Ordering::SeqCst);

        // 6. Spawn polling loop to track playback state
        self.start_polling_loop();

        Ok(())
    }

    fn pause(&mut self) -> anyhow::Result<()> {
        let (rel_sec, _) = self.client.get_position_info();
        if rel_sec > 0.0 {
            let offset = *self.playback_stream_offset.lock().unwrap();
            let current = if offset > 0.0 { offset + rel_sec } else { rel_sec };
            let dur = *self.duration_seconds.lock().unwrap();
            let clamped = if dur > 0.0 { current.min(dur) } else { current };
            *self.position_seconds.lock().unwrap() = clamped;
        }

        let paused = self.client.pause();
        self.is_playing.store(false, Ordering::SeqCst);
        self.is_paused.store(true, Ordering::SeqCst);
        if paused {
            crate::log_info!("player", "UPnPPlayer paused at {:.2}s", *self.position_seconds.lock().unwrap());
            Ok(())
        } else {
            let err = self.client.last_error.lock().unwrap().clone();
            anyhow::bail!("Pause failed: {}", err);
        }
    }

    fn resume(&mut self) -> anyhow::Result<()> {
        let current_pos = *self.position_seconds.lock().unwrap();
        crate::log_info!("player", "UPnPPlayer::resume at {:.2}s", current_pos);

        // Check current transport state of the renderer
        let state = self.client.get_transport_info();
        if state == "PAUSED_PLAYBACK" && self.client.play("1") {
            // Verify that the renderer actually entered PLAYING
            std::thread::sleep(Duration::from_millis(150));
            let new_state = self.client.get_transport_info();
            if new_state == "PLAYING" {
                self.is_playing.store(true, Ordering::SeqCst);
                self.is_paused.store(false, Ordering::SeqCst);
                crate::log_info!("player", "Native Play succeeded; renderer in PLAYING state");
                return Ok(());
            }
            crate::log_info!(
                "player",
                "Renderer did not enter PLAYING after native Play (state: '{}'); reloading via stream offset",
                new_state
            );
        } else {
            crate::log_info!(
                "player",
                "Renderer state is '{}' (not PAUSED_PLAYBACK or play rejected); resuming via stream offset at {:.2}s",
                state,
                current_pos
            );
        }

        // Seamless resume: reload stream with ?pos=current_pos
        self.seek_via_stream_offset(current_pos)
    }

    fn toggle_pause(&mut self) -> anyhow::Result<bool> {
        if self.is_paused() {
            self.resume()?;
            Ok(false)
        } else if self.is_playing() {
            self.pause()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        self.poll_stop_flag.store(true, Ordering::SeqCst);
        if let Some(ref ss) = self.stream_state {
            ss.close_active_streams();
        }
        self.is_playing.store(false, Ordering::SeqCst);
        self.is_paused.store(false, Ordering::SeqCst);
        *self.position_seconds.lock().unwrap() = 0.0;
        *self.playback_stream_offset.lock().unwrap() = 0.0;
        *self.current_filepath.lock().unwrap() = None;
        let _ = self.client.stop();
        Ok(())
    }

    fn seek(&mut self, delta_seconds: f64) -> anyhow::Result<()> {
        let current = *self.position_seconds.lock().unwrap();
        let target = (current + delta_seconds).max(0.0);
        self.seek_to(target)
    }

    fn seek_to(&mut self, position_seconds: f64) -> anyhow::Result<()> {
        let dur = *self.duration_seconds.lock().unwrap();
        let target = if dur > 0.0 {
            position_seconds.clamp(0.0, dur)
        } else {
            position_seconds.max(0.0)
        };

        crate::log_info!(
            "player",
            "UPnPPlayer::seek_to target={:.2}s (current={:.2}s) on '{}'",
            target,
            *self.position_seconds.lock().unwrap(),
            self.device.friendly_name
        );
        *self.last_seek_time.lock().unwrap() = Instant::now();
        *self.position_seconds.lock().unwrap() = target;

        // DLNA/UPnP HTTP progressive streams must be sought via stream offset reloading (?pos=),
        // because network stereos either reject SOAP REL_TIME Seek or silently ignore it (returning 200 OK without seeking).
        self.seek_via_stream_offset(target)
    }

    fn set_volume(&mut self, volume: u8) -> anyhow::Result<()> {
        let vol = volume.min(100);
        *self.volume.lock().unwrap() = vol;
        let _ = self.client.set_volume(vol);
        Ok(())
    }

    fn volume(&self) -> u8 {
        *self.volume.lock().unwrap()
    }

    fn is_playing(&self) -> bool {
        self.is_playing.load(Ordering::SeqCst)
    }

    fn is_paused(&self) -> bool {
        self.is_paused.load(Ordering::SeqCst)
    }

    fn position_seconds(&self) -> f64 {
        *self.position_seconds.lock().unwrap()
    }

    fn duration_seconds(&self) -> f64 {
        *self.duration_seconds.lock().unwrap()
    }

    fn is_track_finished(&mut self) -> bool {
        self.track_finished.load(Ordering::SeqCst)
    }

    fn reset_finished(&mut self) {
        self.track_finished.store(false, Ordering::SeqCst);
    }
}
