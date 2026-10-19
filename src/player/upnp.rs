//! UPnP / DLNA MediaRenderer playback implementation using SOAP AVTransport and RenderingControl.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::discovery::{
    extract_xml_tag, get_local_ip_for_target, http_exchange, parse_http_url, DLNADevice, HttpError,
};
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

/// Largest SOAP response accepted from a renderer. Real replies, including
/// GetPositionInfo with its DIDL-Lite track metadata, are a few KiB.
pub const MAX_SOAP_RESPONSE_BYTES: usize = 64 * 1024;

/// Why a SOAP action failed.
#[derive(Debug, Clone, PartialEq)]
pub enum SoapError {
    /// Bad or missing control URL.
    InvalidUrl(String),
    /// Connection, timeout or framing failure before a response was read.
    Transport(HttpError),
    /// The renderer answered with a non-2xx status, usually carrying a UPnP fault.
    Fault {
        http_status: u16,
        upnp_code: Option<u32>,
        description: Option<String>,
    },
}

impl SoapError {
    /// Numeric UPnP `errorCode` from the fault body, if any.
    pub fn upnp_code(&self) -> Option<u32> {
        match self {
            SoapError::Fault { upnp_code, .. } => *upnp_code,
            _ => None,
        }
    }

    /// For faults without a numeric code, match the standard description text.
    fn description_matches(&self, phrases: &[&str]) -> bool {
        match self {
            SoapError::Fault {
                upnp_code: None,
                description: Some(desc),
                ..
            } => {
                let d = desc.to_ascii_lowercase();
                phrases.iter().any(|p| d.contains(p))
            }
            _ => false,
        }
    }

    /// UPnP 701 "Transition not available".
    pub fn is_transition_unavailable(&self) -> bool {
        self.upnp_code() == Some(701) || self.description_matches(&["transition not available"])
    }

    /// Errors worth retrying a Play for: the renderer is busy or transitioning
    /// (501 Action Failed, 701 Transition not available, 705 Transport locked)
    /// or the connection timed out / dropped.
    pub fn is_transient(&self) -> bool {
        match self {
            SoapError::Transport(e) => e.is_transient(),
            SoapError::Fault {
                upnp_code: Some(code), ..
            } => matches!(code, 501 | 701 | 705),
            SoapError::Fault { .. } => {
                self.description_matches(&["transition not available", "action failed", "transport locked"])
            }
            SoapError::InvalidUrl(_) => false,
        }
    }
}

impl std::fmt::Display for SoapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SoapError::InvalidUrl(m) => write!(f, "{}", m),
            SoapError::Transport(e) => write!(f, "{}", e),
            SoapError::Fault {
                http_status,
                upnp_code,
                description,
            } => match (upnp_code, description) {
                (Some(c), Some(d)) => write!(f, "UPnP error {}: {}", c, d),
                (Some(c), None) => write!(f, "UPnP error {}", c),
                (None, Some(d)) => write!(f, "{} (HTTP {})", d, http_status),
                (None, None) => write!(f, "HTTP {}", http_status),
            },
        }
    }
}

/// Parse a UPnP fault (`errorCode` / `errorDescription`) from a SOAP error body.
pub fn parse_soap_fault(http_status: u16, body: &str) -> SoapError {
    let upnp_code = extract_xml_tag(body, "errorCode").and_then(|c| c.trim().parse::<u32>().ok());
    let description = extract_xml_tag(body, "errorDescription").filter(|d| !d.is_empty());
    SoapError::Fault {
        http_status,
        upnp_code,
        description,
    }
}

/// SOAP XML client controlling DLNA MediaRenderers.
pub struct UPnPClient {
    pub device: DLNADevice,
    pub timeout: Duration,
    /// Human-readable description of the last failure.
    pub last_error: Arc<Mutex<String>>,
    /// Structured form of the last failure.
    pub last_fault: Arc<Mutex<Option<SoapError>>>,
}

impl UPnPClient {
    pub fn new(device: DLNADevice) -> Self {
        Self {
            device,
            timeout: Duration::from_millis(4000),
            last_error: Arc::new(Mutex::new(String::new())),
            last_fault: Arc::new(Mutex::new(None)),
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
    /// On failure, `last_error` / `last_fault` describe what went wrong.
    pub fn send_soap_action_with_timeout(
        &self,
        control_url: &str,
        service_type: &str,
        action: &str,
        arguments: &[(&str, &str)],
        timeout: Duration,
    ) -> Option<String> {
        self.call_action(control_url, service_type, action, arguments, timeout)
            .ok()
    }

    /// Send a UPnP SOAP POST action. `timeout` bounds the whole request (connect,
    /// send and receive), not each individual read.
    pub fn call_action(
        &self,
        control_url: &str,
        service_type: &str,
        action: &str,
        arguments: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<String, SoapError> {
        let result = self.call_action_inner(control_url, service_type, action, arguments, timeout);
        if let Err(ref e) = result {
            *self.last_error.lock().unwrap() = e.to_string();
            *self.last_fault.lock().unwrap() = Some(e.clone());
        }
        result
    }

    fn call_action_inner(
        &self,
        control_url: &str,
        service_type: &str,
        action: &str,
        arguments: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<String, SoapError> {
        let deadline = Instant::now() + timeout;
        if control_url.is_empty() {
            return Err(SoapError::InvalidUrl("Missing control URL".to_string()));
        }
        let url = parse_http_url(control_url)
            .ok_or_else(|| SoapError::InvalidUrl(format!("Invalid control URL: {}", control_url)))?;
        // Host names would need a DNS lookup that the request deadline cannot bound.
        if url.host.parse::<std::net::IpAddr>().is_err() {
            return Err(SoapError::InvalidUrl(format!(
                "Control URL host is not an IP address: {}",
                control_url
            )));
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

        let request = format!(
            "POST {} HTTP/1.1\r\n\
            Host: {}\r\n\
            Content-Type: text/xml; charset=\"utf-8\"\r\n\
            SOAPAction: \"{}#{}\"\r\n\
            User-Agent: Quickstream/0.2.0 UPnP/1.1\r\n\
            Content-Length: {}\r\n\
            Connection: close\r\n\r\n\
            {}",
            url.path,
            url.host_port,
            service_type,
            action,
            body.len(),
            body
        );

        let response = http_exchange(
            &url.host,
            url.port,
            request.as_bytes(),
            deadline,
            MAX_SOAP_RESPONSE_BYTES,
        )
        .map_err(SoapError::Transport)?;

        let body_text = response.body_text();
        if (200..300).contains(&response.status) {
            Ok(body_text)
        } else {
            Err(parse_soap_fault(response.status, &body_text))
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
        let didl = create_didl_lite_xml(stream_url, title, artist, album, duration_sec, mime_type);

        crate::log_info!(
            "upnp",
            "Calling SetAVTransportURI on {} with stream URL: {}",
            self.device.friendly_name,
            stream_url
        );

        for attempt in 0..3 {
            let res = self.call_action(
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

            match res {
                Ok(_) => {
                    crate::log_info!(
                        "upnp",
                        "SetAVTransportURI with DIDL succeeded on {}",
                        self.device.friendly_name
                    );
                    return true;
                }
                Err(e) if e.is_transition_unavailable() && attempt < 2 => {
                    std::thread::sleep(Duration::from_millis(250));
                    continue;
                }
                Err(_) => break,
            }
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
            crate::log_info!(
                "upnp",
                "SetAVTransportURI with empty metadata succeeded on {}",
                self.device.friendly_name
            );
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

    /// Send Play command with retries if renderer is transitioning, busy, or buffering.
    pub fn play(&self, speed: &str) -> bool {
        for attempt in 0..5 {
            let res = self.call_action(
                &self.device.av_transport_control_url,
                AV_TRANSPORT_SERVICE,
                "Play",
                &[("InstanceID", "0"), ("Speed", speed)],
                Duration::from_millis(6000),
            );
            match res {
                Ok(_) => return true,
                Err(e) if e.is_transient() => {
                    crate::log_info!(
                        "upnp",
                        "Renderer returned '{}' on Play (attempt {}/5); retrying in 400ms...",
                        e,
                        attempt + 1
                    );
                    std::thread::sleep(Duration::from_millis(400));
                }
                Err(_) => break,
            }
        }
        false
    }

    /// Send Pause command, falling back to Stop if the renderer rejects it
    /// (many reject Pause on progressive HTTP streams, e.g. 401 Invalid Action).
    pub fn pause(&self) -> bool {
        match self.call_action(
            &self.device.av_transport_control_url,
            AV_TRANSPORT_SERVICE,
            "Pause",
            &[("InstanceID", "0")],
            self.timeout,
        ) {
            Ok(_) => true,
            Err(e) => {
                crate::log_info!("upnp", "Pause failed ('{}'); falling back to Stop", e);
                self.stop()
            }
        }
    }

    /// Send Stop command. A 701 reply means the renderer is already stopped.
    pub fn stop(&self) -> bool {
        match self.call_action(
            &self.device.av_transport_control_url,
            AV_TRANSPORT_SERVICE,
            "Stop",
            &[("InstanceID", "0")],
            self.timeout,
        ) {
            Ok(_) => true,
            Err(e) => e.is_transition_unavailable(),
        }
    }

    /// Seek to specific position in track (REL_TIME).
    pub fn seek(&self, target_seconds: f64) -> bool {
        let target_str = format_seconds_to_upnp_time(target_seconds);
        let res = self.send_soap_action(
            &self.device.av_transport_control_url,
            AV_TRANSPORT_SERVICE,
            "Seek",
            &[("InstanceID", "0"), ("Unit", "REL_TIME"), ("Target", &target_str)],
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
            &[("InstanceID", "0"), ("Channel", "Master"), ("DesiredVolume", &vol_str)],
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

/// Coordinates the background poll thread with player commands.
///
/// Every command (play, pause, resume, seek, stop) bumps `epoch` when it starts and
/// again when it ends, and counts itself in `active_commands` meanwhile. The poll thread
/// snapshots the epoch before its SOAP calls and applies what it observed only if the
/// epoch is unchanged and no command is running, checked and written under this lock.
/// A poll thread from an earlier session, or one whose SOAP call overlapped a command,
/// therefore cannot write stale state (e.g. `track_finished`) into the current session.
#[derive(Default)]
struct PollControl {
    epoch: u64,
    active_commands: u32,
}

fn lock_poll_control(ctl: &Mutex<PollControl>) -> std::sync::MutexGuard<'_, PollControl> {
    ctl.lock().unwrap_or_else(|e| e.into_inner())
}

/// RAII marker for a running player command; see [`PollControl`].
struct CommandGuard {
    ctl: Arc<Mutex<PollControl>>,
}

impl CommandGuard {
    fn begin(ctl: &Arc<Mutex<PollControl>>) -> Self {
        {
            let mut c = lock_poll_control(ctl);
            c.epoch = c.epoch.wrapping_add(1);
            c.active_commands += 1;
        }
        Self { ctl: ctl.clone() }
    }
}

impl Drop for CommandGuard {
    fn drop(&mut self) {
        let mut c = lock_poll_control(&self.ctl);
        c.epoch = c.epoch.wrapping_add(1);
        c.active_commands = c.active_commands.saturating_sub(1);
    }
}

/// What one poll tick learned from the renderer.
enum PollObservation {
    /// Continuous (gapless) stream: only the transport state is tracked.
    Continuous {
        state: String,
    },
    Track {
        rel_sec: f64,
        dur_sec: f64,
        state: String,
    },
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
    poll_control: Arc<Mutex<PollControl>>,
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
            poll_control: Arc::new(Mutex::new(PollControl::default())),
        }
    }

    /// Mark a player command as running until the returned guard is dropped.
    fn begin_command(&self) -> CommandGuard {
        CommandGuard::begin(&self.poll_control)
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

        let dev_name = self.device.friendly_name.clone();
        let stream_state = self.stream_state.clone();
        let poll_control = self.poll_control.clone();

        std::thread::spawn(move || {
            let mut consecutive_stopped = 0;
            let mut max_observed_pos: f64 = 0.0;
            let mut total_playing_seconds: f64 = 0.0;
            let mut last_seek_instant = *last_seek_ref.lock().unwrap();

            while !stop_flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(900));
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }

                // Snapshot the command epoch before querying the renderer. Skip the tick
                // entirely while a command is changing the renderer's state.
                let epoch = {
                    let ctl = lock_poll_control(&poll_control);
                    if ctl.active_commands > 0 {
                        continue;
                    }
                    ctl.epoch
                };
                let current_seek_instant = *last_seek_ref.lock().unwrap();

                let continuous = stream_state
                    .as_ref()
                    .is_some_and(|ss| ss.continuous.is_active.load(Ordering::SeqCst));
                let observation = if continuous {
                    PollObservation::Continuous {
                        state: client.get_transport_info(),
                    }
                } else {
                    let (rel_sec, dur_sec) = client.get_position_info();
                    let state = client.get_transport_info();
                    PollObservation::Track {
                        rel_sec,
                        dur_sec,
                        state,
                    }
                };

                // The SOAP calls above can take seconds. Apply their result only if this
                // thread still belongs to the current session and no command ran in the
                // meantime; hold the lock while writing so no command can start mid-write.
                let ctl = lock_poll_control(&poll_control);
                if stop_flag.load(Ordering::SeqCst) {
                    break;
                }
                if ctl.epoch != epoch || ctl.active_commands > 0 {
                    continue;
                }

                let (rel_sec, dur_sec, state) = match observation {
                    PollObservation::Continuous { state } => {
                        if state == "PLAYING" {
                            consecutive_stopped = 0;
                            is_playing_ref.store(true, Ordering::SeqCst);
                            is_paused_ref.store(false, Ordering::SeqCst);
                        } else if state == "PAUSED_PLAYBACK" {
                            consecutive_stopped = 0;
                            is_playing_ref.store(false, Ordering::SeqCst);
                            is_paused_ref.store(true, Ordering::SeqCst);
                        }
                        continue;
                    }
                    PollObservation::Track {
                        rel_sec,
                        dur_sec,
                        state,
                    } => (rel_sec, dur_sec, state),
                };

                // If user sought, sync last_seek_instant and reset max_observed_pos / total_playing_seconds
                if current_seek_instant != last_seek_instant {
                    last_seek_instant = current_seek_instant;
                    let target_pos = *pos_ref.lock().unwrap();
                    max_observed_pos = target_pos;
                    total_playing_seconds = target_pos;
                }

                let time_since_seek = current_seek_instant.elapsed();

                if dur_sec > 0.0 {
                    *dur_ref.lock().unwrap() = dur_sec;
                }

                match state.as_str() {
                    "PLAYING" => {
                        consecutive_stopped = 0;
                        is_playing_ref.store(true, Ordering::SeqCst);
                        is_paused_ref.store(false, Ordering::SeqCst);

                        // If user just sought within 2.5s, ignore stale renderer reports to prevent progress bar jitter
                        if time_since_seek >= Duration::from_millis(2500) {
                            total_playing_seconds += 0.9;
                            let stream_offset = *offset_ref.lock().unwrap();
                            let actual_pos = if stream_offset > 0.0 {
                                stream_offset + rel_sec
                            } else {
                                rel_sec
                            };
                            let dur = *dur_ref.lock().unwrap();
                            let clamped_pos = if dur > 0.0 { actual_pos.min(dur) } else { actual_pos };

                            // Guard against transient zero-readings near end of playback
                            if clamped_pos > 0.0 || max_observed_pos < 5.0 {
                                *pos_ref.lock().unwrap() = clamped_pos;
                                max_observed_pos = max_observed_pos.max(clamped_pos);
                            }
                        }
                    }
                    "PAUSED_PLAYBACK" => {
                        consecutive_stopped = 0;
                        is_playing_ref.store(false, Ordering::SeqCst);
                        is_paused_ref.store(true, Ordering::SeqCst);

                        if time_since_seek >= Duration::from_millis(2500) && rel_sec > 0.0 {
                            let stream_offset = *offset_ref.lock().unwrap();
                            let actual_pos = if stream_offset > 0.0 {
                                stream_offset + rel_sec
                            } else {
                                rel_sec
                            };
                            let dur = *dur_ref.lock().unwrap();
                            let clamped_pos = if dur > 0.0 { actual_pos.min(dur) } else { actual_pos };
                            *pos_ref.lock().unwrap() = clamped_pos;
                            max_observed_pos = max_observed_pos.max(clamped_pos);
                        }
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

                        // If playback was already stopped, ignore
                        if !is_playing_ref.load(Ordering::SeqCst) {
                            consecutive_stopped = 0;
                            continue;
                        }

                        consecutive_stopped += 1;
                        let dur = *dur_ref.lock().unwrap();
                        let current_pos = *pos_ref.lock().unwrap();

                        let near_end = if dur > 0.0 {
                            let threshold = if dur > 30.0 {
                                (dur - 15.0).max(dur * 0.85)
                            } else if dur > 10.0 {
                                (dur - 4.0).max(dur * 0.75)
                            } else {
                                (dur - 2.0).max(dur * 0.60)
                            };
                            max_observed_pos >= threshold
                                || current_pos >= threshold
                                || total_playing_seconds >= threshold
                        } else {
                            (max_observed_pos >= 15.0 || total_playing_seconds >= 15.0) && consecutive_stopped >= 3
                        };

                        if consecutive_stopped >= 1 && near_end {
                            // Track has genuinely finished playing on the remote renderer
                            crate::log_info!(
                                "player",
                                "Track finished playing on '{}' (max_pos={:.1}s, current_pos={:.1}s, dur={:.1}s). Triggering auto-advance.",
                                dev_name,
                                max_observed_pos,
                                current_pos,
                                dur
                            );
                            if dur > 0.0 {
                                *pos_ref.lock().unwrap() = dur;
                            }
                            is_playing_ref.store(false, Ordering::SeqCst);
                            is_paused_ref.store(false, Ordering::SeqCst);
                            finished_ref.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    fn seek_via_stream_offset(&mut self, target: f64) -> anyhow::Result<()> {
        if let Some(ref ss) = self.stream_state {
            if ss.continuous.is_active.load(Ordering::SeqCst) {
                ss.continuous.seek_to(target);
                self.is_playing.store(true, Ordering::SeqCst);
                self.is_paused.store(false, Ordering::SeqCst);
                let _ = self.client.play("1");
                return Ok(());
            }
        }

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
        std::thread::sleep(Duration::from_millis(100));

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
        crate::log_info!(
            "player",
            "Reloading UPnP transport URI with seek offset: {}",
            stream_url
        );

        let mime = get_audio_mime_type(&filepath);
        let (title, artist, album, duration) = if let Ok(m) = extract_metadata(&filepath) {
            (m.title, m.artist, m.album, m.duration_seconds)
        } else {
            (file_name.clone(), String::new(), String::new(), 0.0)
        };

        // Set transport URI with offset URL
        if !self
            .client
            .set_transport_uri(&stream_url, &title, &artist, &album, duration, mime)
        {
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

        crate::log_info!(
            "player",
            "UPnP stream offset seek to {:.2}s successfully started",
            target
        );
        Ok(())
    }
}

impl Drop for UPnPPlayer {
    fn drop(&mut self) {
        let _cmd = self.begin_command();
        self.poll_stop_flag.store(true, Ordering::SeqCst);
        if let Some(ref ss) = self.stream_state {
            ss.close_active_streams();
        }
        let _ = self.client.stop();
    }
}

impl AudioPlayer for UPnPPlayer {
    fn detach_stream_state(&mut self) {
        self.stream_state = None;
    }

    fn play(&mut self, filepath: &Path) -> anyhow::Result<()> {
        self.play_at(filepath, 0.0)
    }

    fn play_at(&mut self, filepath: &Path, position_seconds: f64) -> anyhow::Result<()> {
        let _timer = crate::log_perf!(
            "player",
            "UPnPPlayer::play_at '{}' at {:.2}s on '{}'",
            filepath.display(),
            position_seconds,
            self.device.friendly_name
        );
        let _cmd = self.begin_command();

        // 1. Terminate any previous polling thread, close streaming sockets, & stop renderer.
        // The old thread is not joined (it may be inside a multi-second SOAP call); the
        // command epoch bumped above makes it drop whatever it observes from now on.
        // `__continuous__.wav` is a placeholder that selects the gapless continuous stream.
        let is_continuous = filepath.file_name().is_some_and(|f| f == "__continuous__.wav");
        self.poll_stop_flag.store(true, Ordering::SeqCst);
        if let Some(ref ss) = self.stream_state {
            // Also ends any previous continuous session.
            ss.close_active_streams();
            if !is_continuous {
                ss.register_file(filepath);
            }
        }
        let _ = self.client.stop();
        std::thread::sleep(Duration::from_millis(100));

        // 2. Resolve local IP routable to renderer and construct stream URL
        let host_ip = get_local_ip_for_target(&self.device.host, self.device.port);
        let file_name = filepath
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "track".to_string());
        let encoded_name = url_encode(&file_name);
        let stream_url = if is_continuous {
            // Each continuous session is served at its own unguessable path.
            let Some(ref ss) = self.stream_state else {
                anyhow::bail!("Continuous streaming needs the streaming server");
            };
            format!("http://{}:{}{}", host_ip, self.web_port, ss.arm_continuous_session())
        } else if position_seconds > 0.0 {
            format!(
                "http://{}:{}/stream/{}?pos={:.2}",
                host_ip, self.web_port, encoded_name, position_seconds
            )
        } else {
            format!("http://{}:{}/stream/{}", host_ip, self.web_port, encoded_name)
        };
        crate::log_info!("player", "UPnP stream URL for '{}': {}", filepath.display(), stream_url);

        // 3. Extract audio metadata & MIME type
        let mime = if is_continuous {
            "audio/x-wav"
        } else {
            get_audio_mime_type(filepath)
        };
        let (title, artist, album, duration) = if is_continuous {
            (
                "Autopilot (Gapless Stream)".to_string(),
                "Quickstream".to_string(),
                "Autopilot".to_string(),
                0.0,
            )
        } else if let Ok(m) = extract_metadata(filepath) {
            (m.title, m.artist, m.album, m.duration_seconds)
        } else {
            (file_name.clone(), String::new(), String::new(), 0.0)
        };

        *self.duration_seconds.lock().unwrap() = duration;
        *self.position_seconds.lock().unwrap() = position_seconds;
        *self.playback_stream_offset.lock().unwrap() = position_seconds;
        *self.last_seek_time.lock().unwrap() = Instant::now() - Duration::from_secs(10);
        *self.current_filepath.lock().unwrap() = Some(filepath.to_path_buf());
        self.track_finished.store(false, Ordering::SeqCst);

        // 4. Set URI on stereo
        if !self
            .client
            .set_transport_uri(&stream_url, &title, &artist, &album, duration, mime)
        {
            let err = self.client.last_error.lock().unwrap().clone();
            crate::log_info!(
                "player",
                "SetAVTransportURI failed on {}: {}",
                self.device.friendly_name,
                err
            );
            anyhow::bail!("SetAVTransportURI failed on {}: {}", self.device.friendly_name, err);
        }

        // Allow renderer audio pipeline to connect and establish initial stream buffer
        if is_continuous {
            std::thread::sleep(Duration::from_millis(250));
        }

        // 5. Send Play command (UPnPClient::play automatically retries on 501 Action Failed / 701 Transition not available / timeout)
        if !self.client.play("1") {
            let err = self.client.last_error.lock().unwrap().clone();
            crate::log_info!(
                "player",
                "Play command failed on {}: {}",
                self.device.friendly_name,
                err
            );
            anyhow::bail!("Play command failed on {}: {}", self.device.friendly_name, err);
        }

        crate::log_info!(
            "player",
            "UPnP playback successfully started on {}",
            self.device.friendly_name
        );

        self.is_playing.store(true, Ordering::SeqCst);
        self.is_paused.store(false, Ordering::SeqCst);

        // 6. Spawn polling loop to track playback state
        self.start_polling_loop();

        Ok(())
    }

    fn pause(&mut self) -> anyhow::Result<()> {
        let _cmd = self.begin_command();
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
            crate::log_info!(
                "player",
                "UPnPPlayer paused at {:.2}s",
                *self.position_seconds.lock().unwrap()
            );
            Ok(())
        } else {
            let err = self.client.last_error.lock().unwrap().clone();
            anyhow::bail!("Pause failed: {}", err);
        }
    }

    fn resume(&mut self) -> anyhow::Result<()> {
        let _cmd = self.begin_command();
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
        let _cmd = self.begin_command();
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
        let _cmd = self.begin_command();
        if let Some(ref ss) = self.stream_state {
            if ss.continuous.is_active.load(Ordering::SeqCst) {
                // Clamped to the track's length once the stream knows it; from 0 up until then.
                let duration = *ss.continuous.current_duration.lock().unwrap();
                let target = position_seconds.max(0.0);
                let target = if duration > 0.0 { target.min(duration) } else { target };
                ss.continuous.seek_to(target);
                return Ok(());
            }
        }
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
        if let Some(ref ss) = self.stream_state {
            if ss.continuous.is_active.load(Ordering::SeqCst) {
                return *ss.continuous.current_position.lock().unwrap();
            }
        }
        *self.position_seconds.lock().unwrap()
    }

    fn duration_seconds(&self) -> f64 {
        if let Some(ref ss) = self.stream_state {
            if ss.continuous.is_active.load(Ordering::SeqCst) {
                return *ss.continuous.current_duration.lock().unwrap();
            }
        }
        *self.duration_seconds.lock().unwrap()
    }

    fn is_track_finished(&mut self) -> bool {
        self.track_finished.load(Ordering::SeqCst)
    }

    fn reset_finished(&mut self) {
        self.track_finished.store(false, Ordering::SeqCst);
    }
}
