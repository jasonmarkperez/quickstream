//! Embedded local Now Playing web server with live metadata, cover art, and remote control.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::metadata::{default_cover_svg, extract_cover_art};

/// Individual track representation in the web queue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebQueueItem {
    pub index: usize,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_seconds: f64,
    pub filepath: String,
    pub is_current: bool,
}

/// Output device representation for web rendering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebDeviceItem {
    pub id: usize,
    pub name: String,
    pub host_port: String,
    pub is_active: bool,
}

/// Directory entry for filesystem browsing via web.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebBrowserEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

/// Full response payload for GET /api/browser.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebBrowserResponse {
    pub current_path: String,
    pub parent_path: Option<String>,
    pub bookmarks: Vec<String>,
    pub entries: Vec<WebBrowserEntry>,
}

/// Live playback state shared between Quickstream and the web server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NowPlayingState {
    pub is_playing: bool,
    pub is_paused: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub format: String,
    pub duration_seconds: f64,
    pub position_seconds: f64,
    pub bitrate_kbps: u32,
    pub sample_rate_hz: u32,
    pub channels: u8,
    pub volume: u8,
    pub output_device: String,
    pub filepath: Option<PathBuf>,
    pub queue_index: usize,
    pub queue_total: usize,
    pub has_cover: bool,
    pub next_track_title: Option<String>,
    #[serde(default)]
    pub queue: Vec<WebQueueItem>,
    #[serde(default)]
    pub devices: Vec<WebDeviceItem>,
    #[serde(default)]
    pub queue_mode: String,
    #[serde(default)]
    pub is_scanning: bool,
}

impl Default for NowPlayingState {
    fn default() -> Self {
        Self {
            is_playing: false,
            is_paused: false,
            title: "No Track Loaded".to_string(),
            artist: String::new(),
            album: String::new(),
            genre: String::new(),
            format: String::new(),
            duration_seconds: 0.0,
            position_seconds: 0.0,
            bitrate_kbps: 0,
            sample_rate_hz: 0,
            channels: 2,
            volume: 100,
            output_device: "Local Audio".to_string(),
            filepath: None,
            queue_index: 0,
            queue_total: 0,
            has_cover: false,
            next_track_title: None,
            queue: Vec::new(),
            devices: Vec::new(),
            queue_mode: "loop".to_string(),
            is_scanning: false,
        }
    }
}

/// Commands sent from the web interface to Quickstream's playback engine.
#[derive(Debug, Clone, PartialEq)]
pub enum WebCommand {
    TogglePause,
    Play,
    Pause,
    Next,
    Prev,
    SetVolume(u8),
    SeekRelative(f64),
    SeekTo(f64),
    PlayIndex(usize),
    RemoveIndex(usize),
    ClearQueue,
    AddFile(PathBuf),
    AddFolder(PathBuf),
    SelectDevice(usize),
    RescanDevices,
    SetQueueMode(String),
}

#[derive(Deserialize)]
struct ActionPayload {
    action: String,
    #[serde(default)]
    value: Option<f64>,
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<usize>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    mode: Option<String>,
}

/// Manages registered media files and active streaming TCP connections.
pub struct StreamServerState {
    pub active_sockets: Mutex<HashMap<u64, TcpStream>>,
    pub file_registry: RwLock<HashMap<String, PathBuf>>,
    next_conn_id: AtomicU64,
}

impl Default for StreamServerState {
    fn default() -> Self {
        Self {
            active_sockets: Mutex::new(HashMap::new()),
            file_registry: RwLock::new(HashMap::new()),
            next_conn_id: AtomicU64::new(1),
        }
    }
}

impl StreamServerState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a file on disk so that /stream/<filename> resolves to its exact path.
    pub fn register_file(&self, path: &Path) {
        if let Some(filename) = path.file_name().and_then(|f| f.to_str()) {
            let mut reg = self.file_registry.write().unwrap();
            reg.insert(filename.to_string(), path.to_path_buf());
            reg.insert(filename.to_lowercase(), path.to_path_buf());
        }
    }

    /// Look up a file by exact or case-insensitive filename.
    pub fn lookup_file(&self, name: &str) -> Option<PathBuf> {
        let reg = self.file_registry.read().unwrap();
        if let Some(p) = reg.get(name) {
            return Some(p.clone());
        }
        let lower = name.to_lowercase();
        if let Some(p) = reg.get(&lower) {
            return Some(p.clone());
        }
        None
    }

    /// Register an active streaming socket clone for clean abort/termination.
    pub fn register_socket(&self, stream: &TcpStream) -> Option<u64> {
        if let Ok(cloned) = stream.try_clone() {
            let id = self.next_conn_id.fetch_add(1, Ordering::SeqCst);
            self.active_sockets.lock().unwrap().insert(id, cloned);
            Some(id)
        } else {
            None
        }
    }

    /// Untrack an active streaming socket when the request finishes.
    pub fn unregister_socket(&self, id: u64) {
        self.active_sockets.lock().unwrap().remove(&id);
    }

    /// Immediately abort and close all active streaming TCP connections to stereos.
    pub fn close_active_streams(&self) {
        let mut sockets = self.active_sockets.lock().unwrap();
        let count = sockets.len();
        if count > 0 {
            crate::log_info!("web", "Closing {} active streaming TCP socket(s)", count);
            for (_, sock) in sockets.drain() {
                let _ = sock.shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

struct SocketGuard {
    stream_state: Arc<StreamServerState>,
    conn_id: Option<u64>,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Some(id) = self.conn_id {
            self.stream_state.unregister_socket(id);
        }
    }
}

/// Handle to the background web server process.
pub struct WebServerHandle {
    pub port: u16,
    stop_flag: Arc<AtomicBool>,
    stream_state: Arc<StreamServerState>,
    join_handle: Option<std::thread::JoinHandle<()>>,
}

impl WebServerHandle {
    pub fn stream_state(&self) -> Arc<StreamServerState> {
        self.stream_state.clone()
    }

    pub fn close_active_streams(&self) {
        self.stream_state.close_active_streams();
    }

    pub fn register_stream_file(&self, path: &Path) {
        self.stream_state.register_file(path);
    }

    pub fn stop(mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        self.stream_state.close_active_streams();
        let _ = TcpStream::connect(format!("127.0.0.1:{}", self.port));
        if let Some(h) = self.join_handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for WebServerHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        self.stream_state.close_active_streams();
        // Ping socket to unblock accept()
        let _ = TcpStream::connect(format!("127.0.0.1:{}", self.port));
        if let Some(h) = self.join_handle.take() {
            let _ = h.join();
        }
    }
}

pub struct WebServer;

impl WebServer {
    /// Start the local web server on the requested port (or fallback to an available port).
    pub fn start(
        port: u16,
        state: Arc<RwLock<NowPlayingState>>,
        cmd_tx: mpsc::Sender<WebCommand>,
    ) -> anyhow::Result<WebServerHandle> {
        Self::start_with_stream_state(port, state, cmd_tx, Arc::new(StreamServerState::new()))
    }

    /// Start the local web server sharing an existing StreamServerState.
    pub fn start_with_stream_state(
        port: u16,
        state: Arc<RwLock<NowPlayingState>>,
        cmd_tx: mpsc::Sender<WebCommand>,
        stream_state: Arc<StreamServerState>,
    ) -> anyhow::Result<WebServerHandle> {
        let candidate_ports = [port, 8080, 8090, 8091, 0];
        let mut listener_opt = None;
        for &p in &candidate_ports {
            if let Ok(l) = TcpListener::bind(format!("0.0.0.0:{}", p)) {
                listener_opt = Some(l);
                break;
            }
            if let Ok(l) = TcpListener::bind(format!("127.0.0.1:{}", p)) {
                listener_opt = Some(l);
                break;
            }
        }
        let listener = listener_opt.ok_or_else(|| anyhow::anyhow!("Failed to bind web server to any port"))?;
        let actual_port = listener.local_addr()?.port();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_thread = stop_flag.clone();
        let stream_state_thread = stream_state.clone();

        let join_handle = std::thread::Builder::new()
            .name("QuickstreamWebServer".to_string())
            .spawn(move || {
                while !stop_flag_thread.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if stop_flag_thread.load(Ordering::SeqCst) {
                                break;
                            }
                            let state_clone = state.clone();
                            let cmd_tx_clone = cmd_tx.clone();
                            let stream_state_clone = stream_state_thread.clone();
                            std::thread::spawn(move || {
                                let _ = handle_client(stream, &state_clone, &cmd_tx_clone, &stream_state_clone);
                            });
                        }
                        Err(_) => {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                    }
                }
            })?;

        Ok(WebServerHandle {
            port: actual_port,
            stop_flag,
            stream_state,
            join_handle: Some(join_handle),
        })
    }
}

fn handle_client(
    mut stream: TcpStream,
    state: &Arc<RwLock<NowPlayingState>>,
    cmd_tx: &mpsc::Sender<WebCommand>,
    stream_state: &Arc<StreamServerState>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(4)))?;
    stream.set_write_timeout(Some(Duration::from_secs(4)))?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return Ok(());
    }

    let method = parts[0];
    let path = parts[1];

    // Read remaining headers
    let mut content_length = 0usize;
    let mut range_header = None;
    loop {
        let mut header_line = String::new();
        if reader.read_line(&mut header_line)? == 0 {
            break;
        }
        let trimmed = header_line.trim();
        if trimmed.is_empty() {
            break;
        }
        if trimmed.to_lowercase().starts_with("content-length:") {
            if let Some(val_str) = trimmed.split(':').nth(1) {
                content_length = val_str.trim().parse::<usize>().unwrap_or(0);
            }
        } else if trimmed.to_lowercase().starts_with("range:") {
            if let Some(val_str) = trimmed.split(':').nth(1) {
                range_header = Some(val_str.trim().to_string());
            }
        }
    }

    // Route dispatch
    if method == "GET" || method == "HEAD" {
        let is_head = method == "HEAD";
        if path == "/" || path == "/index.html" {
            // Serve single-page dashboard
            let body = NOW_PLAYING_HTML.as_bytes();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes())?;
            if !is_head {
                stream.write_all(body)?;
            }
            stream.flush()?;
        } else if path.starts_with("/api/status") {
            // Serve JSON playback status
            let current = {
                let s = state.read().unwrap();
                s.clone()
            };
            let json = serde_json::to_string(&current).unwrap_or_else(|_| "{}".to_string());
            let body = json.as_bytes();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-cache\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes())?;
            if !is_head {
                stream.write_all(body)?;
            }
            stream.flush()?;
        } else if path.starts_with("/api/cover") {
            // Serve cover art (JPEG/PNG) or SVG placeholder
            let filepath = {
                let s = state.read().unwrap();
                s.filepath.clone()
            };

            let art_opt = filepath.as_deref().and_then(extract_cover_art);
            if let Some((bytes, mime)) = art_opt {
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: public, max-age=10\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    mime,
                    bytes.len()
                );
                stream.write_all(header.as_bytes())?;
                if !is_head {
                    stream.write_all(&bytes)?;
                }
                stream.flush()?;
            } else {
                let svg = default_cover_svg().as_bytes();
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: image/svg+xml\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: public, max-age=10\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    svg.len()
                );
                stream.write_all(header.as_bytes())?;
                if !is_head {
                    stream.write_all(svg)?;
                }
                stream.flush()?;
            }
        } else if path.starts_with("/api/browser") {
            let requested_path = if let Some(query_idx) = path.find('?') {
                let query_str = &path[query_idx + 1..];
                let mut p_val = None;
                for pair in query_str.split('&') {
                    let mut kv = pair.splitn(2, '=');
                    if let (Some(k), Some(v)) = (kv.next(), kv.next()) {
                        if k == "path" {
                            p_val = Some(crate::player::upnp::url_decode(v));
                            break;
                        }
                    }
                }
                p_val
            } else {
                None
            };

            let target_dir = if let Some(ref p) = requested_path {
                let pb = PathBuf::from(p);
                if pb.is_dir() {
                    pb
                } else {
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                }
            } else {
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
            };

            let parent_path = target_dir.parent().map(|p| p.to_string_lossy().to_string());

            let bookmarks_file = crate::paths::get_bookmarks_path();
            let bookmarks: Vec<String> = if bookmarks_file.exists() {
                std::fs::read_to_string(&bookmarks_file)
                    .ok()
                    .and_then(|c| serde_json::from_str(&c).ok())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            let mut dirs = Vec::new();
            let mut files = Vec::new();

            if let Ok(read_entries) = std::fs::read_dir(&target_dir) {
                for entry in read_entries.filter_map(|e| e.ok()) {
                    let entry_path = entry.path();
                    let filename = entry.file_name().to_string_lossy().to_string();
                    if filename.starts_with('.') {
                        continue;
                    }
                    if entry_path.is_dir() {
                        dirs.push(WebBrowserEntry {
                            name: filename,
                            path: entry_path.to_string_lossy().to_string(),
                            is_dir: true,
                        });
                    } else if crate::metadata::is_supported_audio_file(&entry_path) {
                        files.push(WebBrowserEntry {
                            name: filename,
                            path: entry_path.to_string_lossy().to_string(),
                            is_dir: false,
                        });
                    }
                }
            }

            dirs.sort_by_key(|a| a.name.to_lowercase());
            files.sort_by_key(|a| a.name.to_lowercase());

            let mut all_entries = dirs;
            all_entries.extend(files);

            let resp_data = WebBrowserResponse {
                current_path: target_dir.to_string_lossy().to_string(),
                parent_path,
                bookmarks,
                entries: all_entries,
            };

            let json = serde_json::to_string(&resp_data).unwrap_or_else(|_| "{}".to_string());
            let body = json.as_bytes();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-cache\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes())?;
            if !is_head {
                stream.write_all(body)?;
            }
            stream.flush()?;
        } else if path.starts_with("/stream") {
            // Serve DLNA audio streaming with HTTP byte ranges
            serve_audio_stream(stream, path, range_header.as_deref(), is_head, state, stream_state)?;
            return Ok(());
        } else {
            let not_found = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found";
            stream.write_all(not_found)?;
            stream.flush()?;
        }
    } else if method == "POST" && path.starts_with("/api/action") {
        let mut body_bytes = vec![0u8; content_length];
        if content_length > 0 {
            let _ = reader.read_exact(&mut body_bytes);
        }

        if let Ok(payload) = serde_json::from_slice::<ActionPayload>(&body_bytes) {
            match payload.action.as_str() {
                "toggle_pause" => {
                    let _ = cmd_tx.send(WebCommand::TogglePause);
                }
                "play" => {
                    let _ = cmd_tx.send(WebCommand::Play);
                }
                "pause" => {
                    let _ = cmd_tx.send(WebCommand::Pause);
                }
                "next" => {
                    let _ = cmd_tx.send(WebCommand::Next);
                }
                "prev" => {
                    let _ = cmd_tx.send(WebCommand::Prev);
                }
                "volume" => {
                    if let Some(vol) = payload.value {
                        let _ = cmd_tx.send(WebCommand::SetVolume(vol.clamp(0.0, 100.0) as u8));
                    }
                }
                "seek" => {
                    if let Some(target) = payload.value {
                        let _ = cmd_tx.send(WebCommand::SeekTo(target));
                    }
                }
                "seek_relative" => {
                    if let Some(delta) = payload.value {
                        let _ = cmd_tx.send(WebCommand::SeekRelative(delta));
                    }
                }
                "play_index" => {
                    if let Some(idx) = payload.index {
                        let _ = cmd_tx.send(WebCommand::PlayIndex(idx));
                    }
                }
                "remove_index" => {
                    if let Some(idx) = payload.index {
                        let _ = cmd_tx.send(WebCommand::RemoveIndex(idx));
                    }
                }
                "clear_queue" => {
                    let _ = cmd_tx.send(WebCommand::ClearQueue);
                }
                "add_file" | "add_track" => {
                    if let Some(p) = payload.path {
                        let _ = cmd_tx.send(WebCommand::AddFile(PathBuf::from(p)));
                    }
                }
                "add_folder" => {
                    if let Some(p) = payload.path {
                        let _ = cmd_tx.send(WebCommand::AddFolder(PathBuf::from(p)));
                    }
                }
                "select_device" => {
                    if let Some(id) = payload.id.or(payload.index) {
                        let _ = cmd_tx.send(WebCommand::SelectDevice(id));
                    }
                }
                "rescan_devices" => {
                    let _ = cmd_tx.send(WebCommand::RescanDevices);
                }
                "set_queue_mode" => {
                    if let Some(mode) = payload.mode {
                        let _ = cmd_tx.send(WebCommand::SetQueueMode(mode));
                    }
                }
                _ => {}
            }
        }

        let resp = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}";
        stream.write_all(resp)?;
        stream.flush()?;
    } else {
        let not_found = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found";
        stream.write_all(not_found)?;
        stream.flush()?;
    }

    Ok(())
}
fn find_mp3_frame_sync(file: &mut std::fs::File, start_offset: u64, max_scan: u64) -> u64 {
    use std::io::{Read, Seek, SeekFrom};
    if file.seek(SeekFrom::Start(start_offset)).is_err() {
        return start_offset;
    }
    let mut buf = vec![0u8; max_scan as usize];
    let n = match file.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return start_offset,
    };
    if n < 4 {
        return start_offset;
    }
    for i in 0..(n - 3) {
        // Frame sync: 11 set bits (0xFF, 0xEx)
        if buf[i] == 0xFF && (buf[i + 1] & 0xE0) == 0xE0 {
            // Check MPEG version (01 reserved)
            if (buf[i + 1] & 0x18) == 0x08 {
                continue;
            }
            // Check bitrate index (1111 invalid)
            if (buf[i + 2] & 0xF0) == 0xF0 {
                continue;
            }
            // Check sampling frequency (11 reserved)
            if (buf[i + 2] & 0x0C) == 0x0C {
                continue;
            }
            return start_offset + i as u64;
        }
    }
    start_offset
}

fn get_id3v2_size(file: &mut std::fs::File) -> u64 {
    use std::io::{Read, Seek, SeekFrom};
    let _ = file.seek(SeekFrom::Start(0));
    let mut hdr = [0u8; 10];
    if file.read_exact(&mut hdr).is_ok() && &hdr[0..3] == b"ID3" {
        let size = ((hdr[6] as u64 & 0x7F) << 21)
            | ((hdr[7] as u64 & 0x7F) << 14)
            | ((hdr[8] as u64 & 0x7F) << 7)
            | (hdr[9] as u64 & 0x7F);
        return 10 + size;
    }
    0
}

fn find_mp3_stream_offset(file: &mut std::fs::File, offset_seconds: f64, duration: f64, total_size: u64) -> u64 {
    let id3_size = get_id3v2_size(file);
    let audio_size = total_size.saturating_sub(id3_size);
    let dur = if duration > 0.0 { duration } else { 1.0 };
    let ratio = (offset_seconds / dur).clamp(0.0, 0.995);
    let est_offset = id3_size + (audio_size as f64 * ratio) as u64;
    find_mp3_frame_sync(file, est_offset, 4096)
}

fn find_wav_pcm_offset(file: &mut std::fs::File, offset_seconds: f64) -> (u64, Option<Vec<u8>>) {
    use std::io::{Read, Seek, SeekFrom};
    let _ = file.seek(SeekFrom::Start(0));
    let mut riff_hdr = [0u8; 12];
    if file.read_exact(&mut riff_hdr).is_err() || &riff_hdr[0..4] != b"RIFF" || &riff_hdr[8..12] != b"WAVE" {
        return (0, None);
    }
    let mut fmt_info = None;
    let mut data_offset = 0u64;
    let mut data_size = 0u32;

    while let Ok(chunk_hdr) = (|| -> std::io::Result<[u8; 8]> {
        let mut h = [0u8; 8];
        file.read_exact(&mut h)?;
        Ok(h)
    })() {
        let cid = &chunk_hdr[0..4];
        let csize = u32::from_le_bytes(chunk_hdr[4..8].try_into().unwrap());
        if cid == b"fmt " {
            let mut fmt_bytes = vec![0u8; csize as usize];
            if file.read_exact(&mut fmt_bytes).is_ok() && fmt_bytes.len() >= 16 {
                let audio_fmt = u16::from_le_bytes(fmt_bytes[0..2].try_into().unwrap());
                let channels = u16::from_le_bytes(fmt_bytes[2..4].try_into().unwrap());
                let srate = u32::from_le_bytes(fmt_bytes[4..8].try_into().unwrap());
                let brate = u32::from_le_bytes(fmt_bytes[8..12].try_into().unwrap());
                let align = u16::from_le_bytes(fmt_bytes[12..14].try_into().unwrap());
                let bps = u16::from_le_bytes(fmt_bytes[14..16].try_into().unwrap());
                fmt_info = Some((audio_fmt, channels, srate, brate, align, bps));
            }
        } else if cid == b"data" {
            data_offset = file.stream_position().unwrap_or(0);
            data_size = csize;
            break;
        } else {
            let pad = (csize % 2) as i64;
            let _ = file.seek(SeekFrom::Current(csize as i64 + pad));
        }
    }

    if let (Some((audio_fmt, channels, srate, brate, align, bps)), true) = (fmt_info, data_offset > 0 && data_size > 0) {
        let pcm_offset = (offset_seconds * brate as f64) as u64;
        let aligned_pcm = if align > 0 {
            (pcm_offset / align as u64) * align as u64
        } else {
            pcm_offset
        };
        let clamped_pcm = aligned_pcm.min(data_size as u64);
        let file_seek_pos = data_offset + clamped_pcm;
        let remaining_pcm = (data_size as u64).saturating_sub(clamped_pcm) as u32;

        let mut hdr = Vec::with_capacity(44);
        hdr.extend_from_slice(b"RIFF");
        hdr.extend_from_slice(&(36u32 + remaining_pcm).to_le_bytes());
        hdr.extend_from_slice(b"WAVEfmt ");
        hdr.extend_from_slice(&16u32.to_le_bytes());
        hdr.extend_from_slice(&audio_fmt.to_le_bytes());
        hdr.extend_from_slice(&channels.to_le_bytes());
        hdr.extend_from_slice(&srate.to_le_bytes());
        hdr.extend_from_slice(&brate.to_le_bytes());
        hdr.extend_from_slice(&align.to_le_bytes());
        hdr.extend_from_slice(&bps.to_le_bytes());
        hdr.extend_from_slice(b"data");
        hdr.extend_from_slice(&remaining_pcm.to_le_bytes());
        return (file_seek_pos, Some(hdr));
    }
    (0, None)
}

pub fn calculate_stream_offset(
    filepath: &std::path::Path,
    file: &mut std::fs::File,
    offset_seconds: f64,
    total_size: u64,
    duration_hint: f64,
) -> (u64, Option<Vec<u8>>) {
    if offset_seconds <= 0.0 || total_size == 0 {
        return (0, None);
    }
    let ext = filepath
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let duration = if duration_hint > 0.0 {
        duration_hint
    } else {
        crate::metadata::extract_metadata(filepath)
            .map(|m| m.duration_seconds)
            .unwrap_or(0.0)
    };

    if ext == "wav" || ext == "wave" {
        find_wav_pcm_offset(file, offset_seconds)
    } else if ext == "mp3" {
        let off = find_mp3_stream_offset(file, offset_seconds, duration, total_size);
        (off, None)
    } else {
        let dur = if duration > 0.0 { duration } else { 1.0 };
        let ratio = (offset_seconds / dur).clamp(0.0, 0.995);
        let seek_pos = (total_size as f64 * ratio) as u64;
        (seek_pos, None)
    }
}

fn serve_audio_stream(
    mut stream: TcpStream,
    path: &str,
    range_header: Option<&str>,
    head_only: bool,
    state: &Arc<RwLock<NowPlayingState>>,
    stream_state: &Arc<StreamServerState>,
) -> std::io::Result<()> {
    let client_ip = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    let conn_id = stream_state.register_socket(&stream);
    let _socket_guard = SocketGuard {
        stream_state: stream_state.clone(),
        conn_id,
    };

    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));

    // 1. Resolve target file and optional seek offset query parameter
    let raw_encoded = path.strip_prefix("/stream/").unwrap_or("");
    let encoded_path = raw_encoded.split('?').next().unwrap_or(raw_encoded);
    let decoded_path = crate::player::upnp::url_decode(encoded_path);
    let candidate = PathBuf::from(&decoded_path);

    let query_str = path.split_once('?').map(|(_, q)| q);
    let mut offset_seconds: f64 = 0.0;
    if let Some(query) = query_str {
        for param in query.split('&') {
            if let Some((k, v)) = param.split_once('=') {
                if k == "pos" || k == "offset" {
                    if let Ok(sec) = v.parse::<f64>() {
                        offset_seconds = sec.max(0.0);
                    }
                }
            }
        }
    }

    let (current_filepath, current_duration) = {
        let s = state.read().unwrap();
        (s.filepath.clone(), s.duration_seconds)
    };

    let filepath = if candidate.is_absolute() && candidate.exists() && candidate.is_file() {
        candidate
    } else if let Some(reg_path) = stream_state.lookup_file(&decoded_path) {
        if reg_path.exists() && reg_path.is_file() {
            reg_path
        } else if let Some(ref p) = current_filepath {
            if p.exists() && p.is_file() {
                p.clone()
            } else {
                crate::log_warn!(
                    "web",
                    "Stream 404: Registered file '{}' missing on disk ({})",
                    decoded_path,
                    reg_path.display()
                );
                let not_found = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found";
                stream.write_all(not_found)?;
                stream.flush()?;
                return Ok(());
            }
        } else {
            crate::log_warn!(
                "web",
                "Stream 404: Registered file '{}' missing on disk ({})",
                decoded_path,
                reg_path.display()
            );
            let not_found = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found";
            stream.write_all(not_found)?;
            stream.flush()?;
            return Ok(());
        }
    } else if candidate.exists() && candidate.is_file() {
        candidate
    } else if let Some(ref p) = current_filepath {
        if p.exists() && p.is_file() {
            p.clone()
        } else {
            crate::log_warn!(
                "web",
                "Stream 404: Candidate '{}' not found and current_filepath is invalid",
                decoded_path
            );
            let not_found = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found";
            stream.write_all(not_found)?;
            stream.flush()?;
            return Ok(());
        }
    } else {
        crate::log_warn!(
            "web",
            "Stream 404: File '{}' not found in registry and no current_filepath",
            decoded_path
        );
        let not_found = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found";
        stream.write_all(not_found)?;
        stream.flush()?;
        return Ok(());
    };

    let mut file = match std::fs::File::open(&filepath) {
        Ok(f) => f,
        Err(e) => {
            crate::log_warn!("web", "Stream 404: Failed to open '{}': {}", filepath.display(), e);
            let not_found = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found";
            stream.write_all(not_found)?;
            stream.flush()?;
            return Ok(());
        }
    };

    let total_size = file.metadata()?.len();
    let mime = crate::metadata::get_audio_mime_type(&filepath);

    // 2. Calculate base byte offset for seek queries (e.g. ?pos=12.5)
    let (base_offset, synthesized_hdr) =
        calculate_stream_offset(&filepath, &mut file, offset_seconds, total_size, current_duration);

    let effective_total = if let Some(ref hdr) = synthesized_hdr {
        hdr.len() as u64 + total_size.saturating_sub(base_offset)
    } else {
        total_size.saturating_sub(base_offset)
    };

    // 3. Parse Range header if present
    let mut req_start = 0u64;
    let mut req_end = effective_total.saturating_sub(1);
    let mut is_range = false;

    if let Some(range) = range_header {
        if let Some(spec) = range.strip_prefix("bytes=") {
            let parts: Vec<&str> = spec.split('-').collect();
            if parts.len() == 2 {
                if let Ok(s) = parts[0].parse::<u64>() {
                    req_start = s;
                    is_range = true;
                    if let Ok(e) = parts[1].parse::<u64>() {
                        req_end = e.min(effective_total.saturating_sub(1));
                    }
                }
            }
        }
    }

    if req_start >= effective_total || req_start > req_end {
        crate::log_warn!(
            "web",
            "Stream 416 Range Not Satisfiable: bytes={}-{}/{} for client {}",
            req_start,
            req_end,
            effective_total,
            client_ip
        );
        let err_resp = format!(
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nConnection: close\r\n\r\n",
            effective_total
        );
        stream.write_all(err_resp.as_bytes())?;
        stream.flush()?;
        return Ok(());
    }

    let content_length = req_end - req_start + 1;

    // 4. Write DLNA HTTP headers
    let status_line = if is_range {
        "HTTP/1.1 206 Partial Content"
    } else {
        "HTTP/1.1 200 OK"
    };

    let display_name = filepath
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("track");

    crate::log_info!(
        "web",
        "Serving '{}' to {} (method: {}, range: {:?}, offset: {:.2}s, bytes: {}-{}/{}, status: {})",
        display_name,
        client_ip,
        if head_only { "HEAD" } else { "GET" },
        range_header,
        offset_seconds,
        req_start,
        req_end,
        effective_total,
        status_line
    );

    let mut headers = format!(
        "{}\r\n\
        Content-Type: {}\r\n\
        Content-Length: {}\r\n\
        Accept-Ranges: bytes\r\n\
        transferMode.dlna.org: Streaming\r\n\
        contentFeatures.dlna.org: DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000\r\n\
        Connection: keep-alive\r\n",
        status_line, mime, content_length
    );

    if is_range {
        headers.push_str(&format!(
            "Content-Range: bytes {}-{}/{}\r\n",
            req_start, req_end, effective_total
        ));
    }
    headers.push_str("\r\n");

    stream.write_all(headers.as_bytes())?;
    stream.flush()?;

    if head_only {
        return Ok(());
    }

    // 5. Stream audio file
    use std::io::Seek;
    let mut streamed_count = 0u64;

    if let Some(hdr) = synthesized_hdr {
        let hdr_len = hdr.len() as u64;

        // If request starts inside the synthesized header
        if req_start < hdr_len {
            let hdr_slice_start = req_start as usize;
            let hdr_slice_end = (req_end + 1).min(hdr_len) as usize;
            let slice = &hdr[hdr_slice_start..hdr_slice_end];
            if stream.write_all(slice).is_err() {
                crate::log_info!(
                    "web",
                    "Client {} disconnected during synthesized header write after {} bytes",
                    client_ip,
                    streamed_count
                );
                return Ok(());
            }
            streamed_count += slice.len() as u64;
        }

        // Remaining bytes from file
        if req_end >= hdr_len {
            let file_start_offset = if req_start > hdr_len {
                base_offset + (req_start - hdr_len)
            } else {
                base_offset
            };
            file.seek(std::io::SeekFrom::Start(file_start_offset))?;
            let mut remaining = content_length.saturating_sub(streamed_count);
            let mut buf = [0u8; 65536];
            while remaining > 0 {
                let to_read = remaining.min(buf.len() as u64) as usize;
                let n = match file.read(&mut buf[..to_read]) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                if stream.write_all(&buf[..n]).is_err() {
                    crate::log_info!(
                        "web",
                        "Client {} disconnected or stream aborted after {} bytes for '{}'",
                        client_ip,
                        streamed_count,
                        display_name
                    );
                    return Ok(());
                }
                remaining -= n as u64;
                streamed_count += n as u64;
            }
        }
    } else {
        let file_start = base_offset + req_start;
        file.seek(std::io::SeekFrom::Start(file_start))?;

        let mut buf = [0u8; 65536];
        let mut remaining = content_length;
        while remaining > 0 {
            let to_read = remaining.min(buf.len() as u64) as usize;
            let n = match file.read(&mut buf[..to_read]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            if stream.write_all(&buf[..n]).is_err() {
                crate::log_info!(
                    "web",
                    "Client {} disconnected or stream aborted after {} bytes for '{}'",
                    client_ip,
                    streamed_count,
                    display_name
                );
                return Ok(());
            }
            remaining -= n as u64;
            streamed_count += n as u64;
        }
    }
    let _ = stream.flush();

    crate::log_info!(
        "web",
        "Streamed {} bytes to {} for '{}' (requested: {})",
        streamed_count,
        client_ip,
        display_name,
        content_length
    );

    Ok(())
}

/// Embedded responsive HTML/CSS/JavaScript web dashboard.
const NOW_PLAYING_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0, maximum-scale=1.0, user-scalable=no">
  <title>Quickstream · Control & Now Playing</title>
  <style>
    :root {
      --bg: #0d0f14;
      --card-bg: rgba(22, 26, 36, 0.85);
      --card-border: rgba(255, 255, 255, 0.08);
      --accent: #00d2ff;
      --accent-grad: linear-gradient(135deg, #00d2ff 0%, #0072ff 100%);
      --text: #f0f4f8;
      --text-muted: #8c9ba8;
      --text-dim: #5c6878;
      --playing: #00e676;
      --paused: #ff9100;
      --danger: #ff5252;
      --font: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
    }

    * { box-sizing: border-box; margin: 0; padding: 0; }

    body {
      background-color: var(--bg);
      color: var(--text);
      font-family: var(--font);
      min-height: 100vh;
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
      padding: 1.5rem;
      overflow-x: hidden;
      position: relative;
    }

    /* Ambient blurred backdrop */
    .ambient-glow {
      position: fixed;
      top: 50%;
      left: 50%;
      transform: translate(-50%, -50%);
      width: 650px;
      height: 650px;
      background: radial-gradient(circle, rgba(0, 210, 255, 0.14) 0%, rgba(0, 114, 255, 0.04) 50%, transparent 70%);
      filter: blur(90px);
      z-index: 0;
      pointer-events: none;
      transition: background 1s ease;
    }

    .container {
      position: relative;
      z-index: 1;
      width: 100%;
      max-width: 480px;
      background: var(--card-bg);
      backdrop-filter: blur(24px);
      -webkit-backdrop-filter: blur(24px);
      border: 1px solid var(--card-border);
      border-radius: 28px;
      padding: 1.75rem;
      box-shadow: 0 24px 64px rgba(0, 0, 0, 0.6), 0 4px 16px rgba(0, 0, 0, 0.4);
      display: flex;
      flex-direction: column;
      align-items: center;
    }

    /* Tab Bar Navigation */
    .tabs {
      display: flex;
      gap: 4px;
      padding: 4px;
      background: rgba(0, 0, 0, 0.35);
      border-radius: 14px;
      margin-bottom: 1.5rem;
      width: 100%;
      border: 1px solid rgba(255, 255, 255, 0.05);
    }

    .tab-btn {
      flex: 1;
      padding: 0.55rem 0.2rem;
      font-size: 0.78rem;
      font-weight: 600;
      border: none;
      background: transparent;
      color: var(--text-muted);
      border-radius: 10px;
      cursor: pointer;
      transition: all 0.2s ease;
      display: flex;
      align-items: center;
      justify-content: center;
      gap: 4px;
      -webkit-tap-highlight-color: transparent;
    }
    .tab-btn:hover {
      color: var(--text);
    }
    .tab-btn.active {
      background: rgba(255, 255, 255, 0.12);
      color: #ffffff;
      box-shadow: 0 2px 8px rgba(0, 0, 0, 0.3);
    }

    .tab-badge {
      background: rgba(0, 210, 255, 0.2);
      color: var(--accent);
      border-radius: 10px;
      padding: 1px 6px;
      font-size: 0.68rem;
      font-weight: 700;
    }

    /* Tab Panels */
    .tab-panel {
      width: 100%;
      display: flex;
      flex-direction: column;
    }
    .tab-panel.centered {
      align-items: center;
      text-align: center;
    }

    /* Header Bar */
    .header {
      width: 100%;
      display: flex;
      justify-content: space-between;
      align-items: center;
      margin-bottom: 1.5rem;
      font-size: 0.8rem;
      text-transform: uppercase;
      letter-spacing: 0.08em;
      color: var(--text-dim);
    }

    .logo {
      font-weight: 800;
      color: var(--text);
      display: flex;
      align-items: center;
      gap: 0.4rem;
    }
    .logo span {
      background: var(--accent-grad);
      -webkit-background-clip: text;
      -webkit-text-fill-color: transparent;
    }

    .status-badge {
      display: flex;
      align-items: center;
      gap: 0.35rem;
      padding: 0.25rem 0.65rem;
      border-radius: 20px;
      background: rgba(255, 255, 255, 0.05);
      border: 1px solid rgba(255, 255, 255, 0.08);
      font-weight: 600;
    }
    .status-dot {
      width: 7px;
      height: 7px;
      border-radius: 50%;
      background: var(--text-dim);
    }
    .status-dot.playing {
      background: var(--playing);
      box-shadow: 0 0 8px var(--playing);
      animation: pulse 2s infinite;
    }
    .status-dot.paused {
      background: var(--paused);
    }

    @keyframes pulse {
      0%, 100% { opacity: 1; transform: scale(1); }
      50% { opacity: 0.6; transform: scale(0.85); }
    }

    /* Cover Artwork */
    .artwork-container {
      position: relative;
      width: 100%;
      aspect-ratio: 1 / 1;
      max-width: 320px;
      border-radius: 20px;
      overflow: hidden;
      box-shadow: 0 16px 40px rgba(0, 0, 0, 0.6);
      margin-bottom: 1.5rem;
      background: #141720;
    }

    .artwork {
      width: 100%;
      height: 100%;
      object-fit: cover;
      display: block;
      transition: transform 0.4s ease, opacity 0.3s ease;
    }
    .artwork-container:hover .artwork {
      transform: scale(1.02);
    }

    /* Track Metadata */
    .track-info {
      width: 100%;
      margin-bottom: 1.25rem;
    }

    .title {
      font-size: 1.35rem;
      font-weight: 700;
      color: var(--text);
      line-height: 1.25;
      margin-bottom: 0.4rem;
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }

    .artist {
      font-size: 1.05rem;
      font-weight: 500;
      color: var(--accent);
      margin-bottom: 0.25rem;
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }

    .album {
      font-size: 0.85rem;
      color: var(--text-muted);
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }

    /* Badges */
    .badges {
      display: flex;
      flex-wrap: wrap;
      justify-content: center;
      gap: 0.4rem;
      margin-bottom: 1.25rem;
    }

    .badge {
      font-size: 0.72rem;
      font-weight: 600;
      padding: 0.2rem 0.55rem;
      border-radius: 6px;
      background: rgba(255, 255, 255, 0.05);
      border: 1px solid rgba(255, 255, 255, 0.08);
      color: var(--text-muted);
      letter-spacing: 0.03em;
    }
    .badge.format {
      background: rgba(0, 210, 255, 0.12);
      border-color: rgba(0, 210, 255, 0.25);
      color: #7de7ff;
    }

    /* Progress Bar */
    .progress-container {
      width: 100%;
      margin-bottom: 1.25rem;
      cursor: pointer;
      padding: 10px 0;
      user-select: none;
      -webkit-user-select: none;
      touch-action: none;
    }

    .progress-bar-bg {
      width: 100%;
      height: 6px;
      background: rgba(255, 255, 255, 0.08);
      border-radius: 4px;
      position: relative;
      transition: height 0.15s ease;
    }
    .progress-container:hover .progress-bar-bg,
    .progress-container.scrubbing .progress-bar-bg {
      height: 8px;
    }

    .progress-bar-fill {
      height: 100%;
      width: 0%;
      background: var(--accent-grad);
      border-radius: 4px;
      position: relative;
    }
    .progress-bar-fill::after {
      content: '';
      position: absolute;
      right: -6px;
      top: 50%;
      transform: translateY(-50%) scale(0);
      width: 14px;
      height: 14px;
      background: #ffffff;
      border-radius: 50%;
      box-shadow: 0 0 10px rgba(0, 210, 255, 0.9);
      transition: transform 0.15s ease;
    }
    .progress-container:hover .progress-bar-fill::after,
    .progress-container.scrubbing .progress-bar-fill::after {
      transform: translateY(-50%) scale(1);
    }

    .time-labels {
      display: flex;
      justify-content: space-between;
      margin-top: 0.5rem;
      font-size: 0.78rem;
      color: var(--text-dim);
      font-variant-numeric: tabular-nums;
      font-weight: 500;
    }

    /* Controls */
    .controls {
      display: flex;
      align-items: center;
      justify-content: center;
      gap: 1.25rem;
      margin-bottom: 1.25rem;
      width: 100%;
    }

    .btn {
      background: none;
      border: none;
      color: var(--text);
      cursor: pointer;
      display: flex;
      align-items: center;
      justify-content: center;
      border-radius: 50%;
      transition: transform 0.15s ease, background 0.2s ease, color 0.2s ease;
      -webkit-tap-highlight-color: transparent;
    }
    .btn:active {
      transform: scale(0.92);
    }

    .btn-secondary {
      width: 44px;
      height: 44px;
      color: var(--text-muted);
      background: rgba(255, 255, 255, 0.04);
    }
    .btn-secondary:hover {
      background: rgba(255, 255, 255, 0.09);
      color: var(--text);
    }

    .btn-primary {
      width: 58px;
      height: 58px;
      background: var(--accent-grad);
      color: #0d0f14;
      box-shadow: 0 8px 24px rgba(0, 210, 255, 0.35);
    }
    .btn-primary:hover {
      box-shadow: 0 10px 28px rgba(0, 210, 255, 0.5);
      transform: scale(1.04);
    }

    /* Volume Row */
    .volume-row {
      display: flex;
      align-items: center;
      gap: 0.6rem;
      width: 80%;
      color: var(--text-dim);
      margin-bottom: 0.5rem;
    }
    .volume-row svg {
      width: 16px;
      height: 16px;
      flex-shrink: 0;
    }
    .volume-slider {
      flex: 1;
      height: 4px;
      accent-color: var(--accent);
      cursor: pointer;
    }

    /* Up Next Drawer */
    .up-next {
      margin-top: 1rem;
      padding-top: 1rem;
      border-top: 1px solid rgba(255, 255, 255, 0.06);
      width: 100%;
      font-size: 0.78rem;
      color: var(--text-dim);
      display: flex;
      justify-content: space-between;
      align-items: center;
    }
    .up-next-title {
      color: var(--text-muted);
      font-weight: 500;
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
      max-width: 260px;
    }

    /* Panel Header Common */
    .panel-header {
      width: 100%;
      display: flex;
      justify-content: space-between;
      align-items: center;
      margin-bottom: 1rem;
    }
    .panel-title {
      font-size: 1.05rem;
      font-weight: 700;
      color: var(--text);
    }
    .panel-subtitle {
      font-size: 0.78rem;
      color: var(--text-dim);
      font-weight: 500;
      margin-left: 0.4rem;
    }

    .btn-sm {
      padding: 0.35rem 0.75rem;
      border-radius: 8px;
      font-size: 0.75rem;
      font-weight: 600;
      border: 1px solid rgba(255, 255, 255, 0.1);
      background: rgba(255, 255, 255, 0.04);
      color: var(--text);
      cursor: pointer;
      display: inline-flex;
      align-items: center;
      gap: 0.35rem;
      transition: all 0.15s;
    }
    .btn-sm:hover {
      background: rgba(255, 255, 255, 0.09);
    }
    .btn-sm.accent {
      background: rgba(0, 210, 255, 0.15);
      border-color: rgba(0, 210, 255, 0.35);
      color: var(--accent);
    }
    .btn-sm.accent:hover {
      background: var(--accent);
      color: #0d0f14;
    }
    .btn-sm:disabled {
      opacity: 0.4;
      cursor: not-allowed;
    }

    .btn-text {
      background: none;
      border: none;
      font-size: 0.75rem;
      font-weight: 600;
      cursor: pointer;
      color: var(--text-muted);
    }
    .btn-text.danger {
      color: var(--danger);
    }
    .btn-text.danger:hover {
      text-decoration: underline;
    }

    /* Queue Tab Elements */
    .mode-bar {
      display: flex;
      align-items: center;
      gap: 0.4rem;
      margin-bottom: 0.85rem;
      width: 100%;
    }
    .mode-label {
      font-size: 0.75rem;
      color: var(--text-dim);
      font-weight: 600;
      margin-right: 0.2rem;
    }
    .mode-btn {
      padding: 0.28rem 0.65rem;
      border-radius: 8px;
      font-size: 0.72rem;
      font-weight: 600;
      border: 1px solid rgba(255, 255, 255, 0.08);
      background: rgba(255, 255, 255, 0.03);
      color: var(--text-muted);
      cursor: pointer;
      transition: all 0.15s;
    }
    .mode-btn:hover {
      background: rgba(255, 255, 255, 0.07);
      color: var(--text);
    }
    .mode-btn.active {
      background: rgba(0, 210, 255, 0.15);
      border-color: rgba(0, 210, 255, 0.4);
      color: var(--accent);
    }

    .queue-list {
      display: flex;
      flex-direction: column;
      gap: 0.4rem;
      max-height: 380px;
      overflow-y: auto;
      width: 100%;
      padding-right: 4px;
    }
    .queue-list::-webkit-scrollbar,
    .browser-list::-webkit-scrollbar {
      width: 5px;
    }
    .queue-list::-webkit-scrollbar-thumb,
    .browser-list::-webkit-scrollbar-thumb {
      background: rgba(255, 255, 255, 0.12);
      border-radius: 4px;
    }

    .queue-item {
      display: flex;
      align-items: center;
      gap: 0.6rem;
      padding: 0.55rem 0.75rem;
      border-radius: 12px;
      background: rgba(255, 255, 255, 0.02);
      border: 1px solid rgba(255, 255, 255, 0.05);
      transition: all 0.15s ease;
      cursor: pointer;
    }
    .queue-item:hover {
      background: rgba(255, 255, 255, 0.06);
    }
    .queue-item.current {
      background: rgba(0, 210, 255, 0.08);
      border-color: rgba(0, 210, 255, 0.3);
    }
    .queue-item.current .queue-num {
      color: #ffd600;
      font-weight: 700;
    }
    .queue-item.current .queue-item-title {
      color: #ffd600;
      font-weight: 600;
    }

    .queue-item-main {
      display: flex;
      align-items: center;
      gap: 0.6rem;
      flex: 1;
      min-width: 0;
    }
    .queue-num {
      font-size: 0.8rem;
      color: var(--text-dim);
      min-width: 1.5rem;
      font-variant-numeric: tabular-nums;
    }
    .queue-item-meta {
      flex: 1;
      min-width: 0;
      text-align: left;
    }
    .queue-item-title {
      font-size: 0.88rem;
      font-weight: 500;
      color: var(--text);
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }
    .queue-item-artist {
      font-size: 0.75rem;
      color: var(--text-muted);
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }
    .queue-item-dur {
      font-size: 0.75rem;
      color: var(--text-dim);
      font-variant-numeric: tabular-nums;
      margin-right: 0.2rem;
    }

    .btn-icon-sm {
      width: 26px;
      height: 26px;
      border-radius: 6px;
      border: none;
      background: transparent;
      color: var(--text-dim);
      cursor: pointer;
      display: flex;
      align-items: center;
      justify-content: center;
      transition: all 0.15s;
    }
    .btn-icon-sm:hover {
      background: rgba(255, 255, 255, 0.08);
      color: var(--text);
    }
    .btn-icon-sm.danger-hover:hover {
      background: rgba(255, 82, 82, 0.2);
      color: var(--danger);
    }

    /* Library / Browser Tab */
    .bookmarks-row {
      display: flex;
      gap: 0.4rem;
      overflow-x: auto;
      padding-bottom: 0.5rem;
      margin-bottom: 0.75rem;
      width: 100%;
    }
    .bookmarks-row::-webkit-scrollbar {
      height: 3px;
    }
    .chip-btn {
      flex-shrink: 0;
      padding: 0.28rem 0.65rem;
      border-radius: 12px;
      font-size: 0.75rem;
      font-weight: 600;
      border: 1px solid rgba(255, 214, 0, 0.25);
      background: rgba(255, 214, 0, 0.08);
      color: #ffd600;
      cursor: pointer;
      transition: all 0.15s;
    }
    .chip-btn:hover {
      background: rgba(255, 214, 0, 0.18);
    }

    .browser-nav-bar {
      display: flex;
      align-items: center;
      gap: 0.5rem;
      margin-bottom: 0.75rem;
      width: 100%;
    }
    .browser-current-path {
      flex: 1;
      font-family: monospace;
      font-size: 0.75rem;
      color: var(--text-muted);
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
      background: rgba(0, 0, 0, 0.2);
      padding: 0.35rem 0.6rem;
      border-radius: 8px;
      border: 1px solid rgba(255, 255, 255, 0.05);
      text-align: left;
    }

    .search-input {
      width: 100%;
      padding: 0.5rem 0.75rem;
      border-radius: 10px;
      border: 1px solid rgba(255, 255, 255, 0.08);
      background: rgba(0, 0, 0, 0.25);
      color: var(--text);
      font-size: 0.82rem;
      margin-bottom: 0.75rem;
      outline: none;
      transition: border-color 0.15s;
    }
    .search-input:focus {
      border-color: var(--accent);
    }

    .browser-list {
      display: flex;
      flex-direction: column;
      gap: 0.35rem;
      max-height: 360px;
      overflow-y: auto;
      width: 100%;
      padding-right: 4px;
    }
    .browser-item {
      display: flex;
      align-items: center;
      gap: 0.6rem;
      padding: 0.5rem 0.75rem;
      border-radius: 10px;
      background: rgba(255, 255, 255, 0.02);
      border: 1px solid rgba(255, 255, 255, 0.04);
      cursor: pointer;
      transition: all 0.15s;
    }
    .browser-item:hover {
      background: rgba(255, 255, 255, 0.06);
    }
    .browser-icon {
      font-size: 1rem;
      flex-shrink: 0;
    }
    .browser-name {
      flex: 1;
      font-size: 0.85rem;
      color: var(--text);
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
      text-align: left;
    }
    .browser-arrow {
      color: var(--text-dim);
      font-size: 1rem;
    }
    .add-btn {
      background: rgba(0, 210, 255, 0.12);
      color: var(--accent);
    }
    .add-btn:hover {
      background: var(--accent);
      color: #0d0f14;
    }

    /* Devices Tab */
    .devices-list {
      display: flex;
      flex-direction: column;
      gap: 0.5rem;
      width: 100%;
    }
    .device-item {
      display: flex;
      align-items: center;
      gap: 0.75rem;
      padding: 0.75rem 0.9rem;
      border-radius: 14px;
      background: rgba(255, 255, 255, 0.02);
      border: 1px solid rgba(255, 255, 255, 0.05);
      cursor: pointer;
      transition: all 0.2s;
    }
    .device-item:hover {
      background: rgba(255, 255, 255, 0.06);
    }
    .device-item.active {
      background: rgba(0, 230, 118, 0.08);
      border-color: rgba(0, 230, 118, 0.3);
    }
    .device-radio {
      font-size: 1.1rem;
      color: var(--text-dim);
      flex-shrink: 0;
    }
    .device-item.active .device-radio {
      color: var(--playing);
    }
    .device-info {
      flex: 1;
      text-align: left;
    }
    .device-name {
      font-size: 0.92rem;
      font-weight: 600;
      color: var(--text);
      display: flex;
      align-items: center;
      gap: 0.5rem;
    }
    .device-host {
      font-size: 0.75rem;
      color: var(--text-dim);
    }
    .active-tag {
      font-size: 0.65rem;
      font-weight: 700;
      color: var(--playing);
      background: rgba(0, 230, 118, 0.15);
      border-radius: 4px;
      padding: 1px 5px;
    }

    /* Empty States */
    .empty-state {
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
      padding: 2.5rem 1rem;
      color: var(--text-muted);
      text-align: center;
    }
    .empty-sub {
      font-size: 0.78rem;
      color: var(--text-dim);
      margin-top: 0.3rem;
    }

    /* Floating Toast */
    .toast {
      position: fixed;
      bottom: 24px;
      left: 50%;
      transform: translateX(-50%) translateY(30px);
      background: rgba(15, 20, 28, 0.95);
      border: 1px solid var(--accent);
      color: #ffffff;
      padding: 0.55rem 1.25rem;
      border-radius: 20px;
      font-size: 0.82rem;
      font-weight: 500;
      box-shadow: 0 8px 24px rgba(0, 0, 0, 0.6);
      pointer-events: none;
      opacity: 0;
      transition: all 0.25s ease;
      z-index: 100;
      white-space: nowrap;
    }
    .toast.show {
      transform: translateX(-50%) translateY(0);
      opacity: 1;
    }
  </style>
</head>
<body>
  <div class="ambient-glow" id="glow"></div>

  <div class="container">
    <!-- Top Segmented Tabs -->
    <div class="tabs">
      <button class="tab-btn active" data-tab="tab-now-playing">Now Playing</button>
      <button class="tab-btn" data-tab="tab-queue" id="tabBtnQueue">Queue <span id="queueBadgeTab" class="tab-badge">0</span></button>
      <button class="tab-btn" data-tab="tab-browser" id="tabBtnBrowser">Library</button>
      <button class="tab-btn" data-tab="tab-devices" id="tabBtnDevices">Devices</button>
    </div>

    <!-- TAB 1: Now Playing -->
    <div class="tab-panel centered active" id="tab-now-playing">
      <div class="header">
        <div class="logo">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5">
            <polygon points="5 3 19 12 5 21 5 3"></polygon>
          </svg>
          <span>QUICKSTREAM</span>
        </div>
        <div class="status-badge">
          <div class="status-dot" id="statusDot"></div>
          <span id="statusLabel">OFFLINE</span>
        </div>
      </div>

      <div class="artwork-container">
        <img id="coverImg" class="artwork" src="/api/cover" alt="Album Cover">
      </div>

      <div class="track-info">
        <div class="title" id="title">Loading...</div>
        <div class="artist" id="artist"></div>
        <div class="album" id="album"></div>
      </div>

      <div class="badges" id="badges">
        <span class="badge format" id="formatBadge">AUDIO</span>
        <span class="badge" id="outputBadge">Local Audio</span>
        <span class="badge" id="queueBadge">Track 0 of 0</span>
      </div>

      <div class="progress-container" id="progressContainer">
        <div class="progress-bar-bg" id="progressBar">
          <div class="progress-bar-fill" id="progressFill"></div>
        </div>
        <div class="time-labels">
          <span id="elapsedTime">00:00</span>
          <span id="durationTime">00:00</span>
        </div>
      </div>

      <div class="controls">
        <button class="btn btn-secondary" id="btnPrev" title="Previous Track">
          <svg width="20" height="20" viewBox="0 0 24 24" fill="currentColor">
            <polygon points="19 20 9 12 19 4 19 20"></polygon>
            <line x1="5" y1="19" x2="5" y2="5" stroke="currentColor" stroke-width="3"></line>
          </svg>
        </button>

        <button class="btn btn-secondary" id="btnSeekBack" title="Seek Backward 10s">
          <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"></path>
            <path d="M3 3v5h5"></path>
            <text x="12" y="15.5" font-size="8" font-family="sans-serif" font-weight="bold" fill="currentColor" stroke="none" text-anchor="middle">10</text>
          </svg>
        </button>

        <button class="btn btn-primary" id="btnPlayPause" title="Play / Pause">
          <svg id="playIcon" width="24" height="24" viewBox="0 0 24 24" fill="currentColor">
            <polygon points="6 4 20 12 6 20 6 4"></polygon>
          </svg>
          <svg id="pauseIcon" width="24" height="24" viewBox="0 0 24 24" fill="currentColor" style="display:none;">
            <rect x="6" y="4" width="4" height="16"></rect>
            <rect x="14" y="4" width="4" height="16"></rect>
          </svg>
        </button>

        <button class="btn btn-secondary" id="btnSeekFwd" title="Seek Forward 10s">
          <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M21 12a9 9 0 1 1-9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"></path>
            <path d="M21 3v5h-5"></path>
            <text x="12" y="15.5" font-size="8" font-family="sans-serif" font-weight="bold" fill="currentColor" stroke="none" text-anchor="middle">10</text>
          </svg>
        </button>

        <button class="btn btn-secondary" id="btnNext" title="Next Track">
          <svg width="20" height="20" viewBox="0 0 24 24" fill="currentColor">
            <polygon points="5 4 15 12 5 20 5 4"></polygon>
            <line x1="19" y1="5" x2="19" y2="19" stroke="currentColor" stroke-width="3"></line>
          </svg>
        </button>
      </div>

      <div class="volume-row">
        <svg viewBox="0 0 24 24" fill="currentColor">
          <polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5"></polygon>
        </svg>
        <input type="range" class="volume-slider" id="volumeSlider" min="0" max="100" value="100">
        <svg viewBox="0 0 24 24" fill="currentColor">
          <polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5"></polygon>
          <path d="M15.54 8.46a5 5 0 0 1 0 7.07" stroke="currentColor" stroke-width="2" fill="none"></path>
          <path d="M19.07 4.93a10 10 0 0 1 0 14.14" stroke="currentColor" stroke-width="2" fill="none"></path>
        </svg>
      </div>

      <div class="up-next" id="upNextRow" style="display:none;">
        <span>Up Next</span>
        <span class="up-next-title" id="upNextTitle"></span>
      </div>
    </div>

    <!-- TAB 2: Queue Management -->
    <div class="tab-panel" id="tab-queue" style="display:none;">
      <div class="panel-header">
        <div>
          <span class="panel-title">Queue</span>
          <span class="panel-subtitle" id="queueSummaryText">0 tracks</span>
        </div>
        <button class="btn-text danger" id="btnClearQueue">Clear Queue</button>
      </div>

      <div class="mode-bar">
        <span class="mode-label">Mode:</span>
        <button class="mode-btn" data-mode="loop">Continuous</button>
        <button class="mode-btn" data-mode="stop">Stop at End</button>
        <button class="mode-btn" data-mode="repeat_one">Repeat 1</button>
      </div>

      <div class="queue-list" id="queueList"></div>
    </div>

    <!-- TAB 3: Library / File Browser -->
    <div class="tab-panel" id="tab-browser" style="display:none;">
      <div class="bookmarks-row" id="bookmarksRow" style="display:none;"></div>

      <div class="browser-nav-bar">
        <button class="btn-sm" id="btnBrowserUp" title="Parent Directory">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><polyline points="15 18 9 12 15 6"></polyline></svg>
          Parent
        </button>
        <div class="browser-current-path" id="browserCurrentPath">/</div>
        <button class="btn-sm accent" id="btnBrowserAddAll" title="Add all audio files in this folder">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><line x1="12" y1="5" x2="12" y2="19"></line><line x1="5" y1="12" x2="19" y2="12"></line></svg>
          Add All
        </button>
      </div>

      <input type="text" id="browserSearchInput" class="search-input" placeholder="Filter files and folders...">

      <div class="browser-list" id="browserList"></div>
    </div>

    <!-- TAB 4: Output Devices -->
    <div class="tab-panel" id="tab-devices" style="display:none;">
      <div class="panel-header">
        <div>
          <span class="panel-title">Output Devices</span>
          <span class="panel-subtitle" id="deviceScanTag"></span>
        </div>
        <button class="btn-sm" id="btnRescanDevices">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><path d="M21.5 2v6h-6M21.34 15.57a10 10 0 1 1-.57-8.38l5.67-5.67"/></svg>
          Rescan
        </button>
      </div>

      <div class="devices-list" id="devicesList"></div>
    </div>
  </div>

  <div class="toast" id="toast"></div>

  <script>
    let localPosition = 0;
    let localDuration = 0;
    let isPlaying = false;
    let lastPollTime = Date.now();
    let currentTrackPath = "";
    let isScrubbing = false;
    let scrubPosition = 0;

    let currentBrowserPath = "";
    let currentBrowserData = null;
    let toastTimer = null;

    function formatTime(seconds) {
      if (!seconds || isNaN(seconds) || seconds < 0) return "00:00";
      const total = Math.floor(seconds);
      const mins = Math.floor(total / 60);
      const secs = total % 60;
      return `${String(mins).padStart(2, '0')}:${String(secs).padStart(2, '0')}`;
    }

    function escapeHtml(str) {
      if (!str) return '';
      return String(str)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
    }

    function escapeAttr(str) {
      if (!str) return '';
      return String(str).replace(/'/g, "\\'").replace(/"/g, '&quot;');
    }

    function showToast(msg) {
      const t = document.getElementById('toast');
      if (!t) return;
      t.textContent = msg;
      t.classList.add('show');
      clearTimeout(toastTimer);
      toastTimer = setTimeout(() => {
        t.classList.remove('show');
      }, 2200);
    }

    async function sendAction(action, value = null, extra = {}) {
      try {
        const payload = Object.assign({ action, value }, extra);
        await fetch('/api/action', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(payload)
        });
        setTimeout(pollStatus, 80);
      } catch (e) {
        console.error("Action error:", e);
      }
    }

    async function pollStatus() {
      try {
        const res = await fetch('/api/status?t=' + Date.now());
        if (!res.ok) throw new Error("Status failed");
        const data = await res.json();
        renderStatus(data);
      } catch (e) {
        document.getElementById('statusDot').className = "status-dot";
        document.getElementById('statusLabel').textContent = "OFFLINE";
      }
    }

    function renderStatus(data) {
      // 1. Now Playing interpolation
      if (!isScrubbing) {
        isPlaying = data.is_playing && !data.is_paused;
        localPosition = data.position_seconds || 0;
        localDuration = data.duration_seconds || 0;
        lastPollTime = Date.now();
      } else {
        if (data.duration_seconds) localDuration = data.duration_seconds;
      }

      // Track titles
      document.getElementById('title').textContent = data.title || "No Track Loaded";
      document.getElementById('artist').textContent = data.artist || "";
      document.getElementById('album').textContent = data.album || "";

      // Cover Art cache bust on track change
      const newPath = data.filepath || "";
      if (newPath !== currentTrackPath) {
        currentTrackPath = newPath;
        document.getElementById('coverImg').src = '/api/cover?t=' + Date.now();
      }

      // Play/Pause icon & status badge
      const dot = document.getElementById('statusDot');
      const label = document.getElementById('statusLabel');
      const playIcon = document.getElementById('playIcon');
      const pauseIcon = document.getElementById('pauseIcon');

      if (isPlaying) {
        dot.className = "status-dot playing";
        label.textContent = "PLAYING";
        playIcon.style.display = "none";
        pauseIcon.style.display = "block";
      } else if (data.is_paused) {
        dot.className = "status-dot paused";
        label.textContent = "PAUSED";
        playIcon.style.display = "block";
        pauseIcon.style.display = "none";
      } else {
        dot.className = "status-dot";
        label.textContent = "STOPPED";
        playIcon.style.display = "block";
        pauseIcon.style.display = "none";
      }

      // Badges
      let fmtText = data.format || "AUDIO";
      if (data.sample_rate_hz > 0) {
        fmtText += ` · ${(data.sample_rate_hz / 1000).toFixed(1)}kHz`;
      }
      if (data.bitrate_kbps > 0) {
        fmtText += ` · ${data.bitrate_kbps}kbps`;
      }
      document.getElementById('formatBadge').textContent = fmtText;
      document.getElementById('outputBadge').textContent = data.output_device || "Local Audio";

      if (data.queue_total > 0) {
        document.getElementById('queueBadge').textContent = `Track ${data.queue_index + 1} of ${data.queue_total}`;
      } else {
        document.getElementById('queueBadge').textContent = "Queue Empty";
      }

      // Volume slider
      if (document.activeElement !== document.getElementById('volumeSlider')) {
        document.getElementById('volumeSlider').value = data.volume;
      }

      // Up Next
      const upNextRow = document.getElementById('upNextRow');
      const upNextTitle = document.getElementById('upNextTitle');
      if (data.next_track_title) {
        upNextRow.style.display = "flex";
        upNextTitle.textContent = data.next_track_title;
      } else {
        upNextRow.style.display = "none";
      }

      updateProgressUI();

      // 2. Queue view sync
      renderQueue(data);

      // 3. Devices view sync
      renderDevices(data);
    }

    function updateProgressUI() {
      let currentPos = localPosition;
      if (isScrubbing) {
        currentPos = scrubPosition;
      } else if (isPlaying) {
        const elapsed = (Date.now() - lastPollTime) / 1000;
        currentPos = Math.min(localDuration, localPosition + elapsed);
      }

      const pct = localDuration > 0 ? (currentPos / localDuration) * 100 : 0;
      document.getElementById('progressFill').style.width = Math.min(100, Math.max(0, pct)) + "%";
      document.getElementById('elapsedTime').textContent = formatTime(currentPos);
      document.getElementById('durationTime').textContent = formatTime(localDuration);
    }

    // Scrubbing and seeking
    function getSeekPositionFromEvent(e) {
      const bar = document.getElementById('progressBar');
      const rect = bar.getBoundingClientRect();
      let clientX = e.clientX;
      if (clientX === undefined) {
        if (e.touches && e.touches.length > 0) clientX = e.touches[0].clientX;
        else if (e.changedTouches && e.changedTouches.length > 0) clientX = e.changedTouches[0].clientX;
        else clientX = 0;
      }
      const offsetX = Math.max(0, Math.min(clientX - rect.left, rect.width));
      const ratio = rect.width > 0 ? offsetX / rect.width : 0;
      return ratio * localDuration;
    }

    const progressContainer = document.getElementById('progressContainer');

    function onScrubStart(e) {
      if (localDuration <= 0) return;
      isScrubbing = true;
      progressContainer.classList.add('scrubbing');
      scrubPosition = getSeekPositionFromEvent(e);
      updateProgressUI();
    }

    function onScrubMove(e) {
      if (!isScrubbing) return;
      scrubPosition = getSeekPositionFromEvent(e);
      updateProgressUI();
    }

    function onScrubEnd(e) {
      if (!isScrubbing) return;
      isScrubbing = false;
      progressContainer.classList.remove('scrubbing');
      const target = getSeekPositionFromEvent(e);
      localPosition = target;
      lastPollTime = Date.now();
      updateProgressUI();
      sendAction('seek', target);
    }

    progressContainer.addEventListener('pointerdown', (e) => {
      e.preventDefault();
      onScrubStart(e);

      function onPointerMove(ev) {
        onScrubMove(ev);
      }
      function onPointerUp(ev) {
        window.removeEventListener('pointermove', onPointerMove);
        window.removeEventListener('pointerup', onPointerUp);
        window.removeEventListener('pointercancel', onPointerUp);
        onScrubEnd(ev);
      }

      window.addEventListener('pointermove', onPointerMove);
      window.addEventListener('pointerup', onPointerUp);
      window.addEventListener('pointercancel', onPointerUp);
    });

    // Seek buttons
    document.getElementById('btnSeekBack').addEventListener('click', () => sendAction('seek_relative', -10));
    document.getElementById('btnSeekFwd').addEventListener('click', () => sendAction('seek_relative', 10));

    // Interactive playback controls
    document.getElementById('btnPlayPause').addEventListener('click', () => sendAction('toggle_pause'));
    document.getElementById('btnNext').addEventListener('click', () => sendAction('next'));
    document.getElementById('btnPrev').addEventListener('click', () => sendAction('prev'));

    document.getElementById('volumeSlider').addEventListener('change', (e) => {
      sendAction('volume', parseFloat(e.target.value));
    });

    // --- TAB SWITCHING ---
    document.querySelectorAll('.tab-btn').forEach(btn => {
      btn.addEventListener('click', () => {
        document.querySelectorAll('.tab-btn').forEach(b => b.classList.remove('active'));
        document.querySelectorAll('.tab-panel').forEach(p => p.style.display = 'none');

        btn.classList.add('active');
        const targetId = btn.getAttribute('data-tab');
        const targetPanel = document.getElementById(targetId);
        if (targetPanel) {
          targetPanel.style.display = targetPanel.classList.contains('centered') ? 'flex' : 'block';
        }

        if (targetId === 'tab-browser' && !currentBrowserData) {
          loadBrowser();
        }
      });
    });

    // --- QUEUE MANAGEMENT ---
    function renderQueue(data) {
      const queue = data.queue || [];
      document.getElementById('queueBadgeTab').textContent = queue.length;

      let totalSecs = 0;
      for (const item of queue) {
        totalSecs += item.duration_seconds || 0;
      }
      document.getElementById('queueSummaryText').textContent = `${queue.length} track${queue.length === 1 ? '' : 's'} · ${formatTime(totalSecs)}`;

      // Mode buttons
      const mode = data.queue_mode || 'loop';
      document.querySelectorAll('.mode-btn').forEach(btn => {
        if (btn.getAttribute('data-mode') === mode) {
          btn.classList.add('active');
        } else {
          btn.classList.remove('active');
        }
      });

      const list = document.getElementById('queueList');
      if (queue.length === 0) {
        list.innerHTML = `
          <div class="empty-state">
            <svg width="40" height="40" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5">
              <path d="M9 18V5l12-2v13"></path>
              <circle cx="6" cy="18" r="3"></circle>
              <circle cx="18" cy="16" r="3"></circle>
            </svg>
            <div style="font-weight:600; margin-top:0.5rem;">Queue is empty</div>
            <div class="empty-sub">Add tracks or folders from the Library tab.</div>
          </div>`;
        return;
      }

      list.innerHTML = queue.map(item => `
        <div class="queue-item ${item.is_current ? 'current' : ''}">
          <div class="queue-item-main" onclick="playIndex(${item.index})">
            <span class="queue-num">${item.is_current ? '▶' : (item.index + 1) + '.'}</span>
            <div class="queue-item-meta">
              <div class="queue-item-title">${escapeHtml(item.title)}</div>
              <div class="queue-item-artist">${escapeHtml(item.artist || 'Unknown Artist')}</div>
            </div>
          </div>
          <span class="queue-item-dur">${formatTime(item.duration_seconds)}</span>
          <button class="btn-icon-sm danger-hover" onclick="event.stopPropagation(); removeIndex(${item.index})" title="Remove track">
            <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5">
              <line x1="18" y1="6" x2="6" y2="18"></line>
              <line x1="6" y1="6" x2="18" y2="18"></line>
            </svg>
          </button>
        </div>
      `).join('');
    }

    function playIndex(idx) {
      sendAction('play_index', null, { index: idx });
      showToast(`Playing track ${idx + 1}`);
    }

    function removeIndex(idx) {
      sendAction('remove_index', null, { index: idx });
      showToast('Track removed from queue');
    }

    document.getElementById('btnClearQueue').addEventListener('click', () => {
      if (confirm('Clear the entire playback queue?')) {
        sendAction('clear_queue');
        showToast('Queue cleared');
      }
    });

    document.querySelectorAll('.mode-btn').forEach(btn => {
      btn.addEventListener('click', () => {
        const mode = btn.getAttribute('data-mode');
        sendAction('set_queue_mode', null, { mode });
        showToast(`Queue mode: ${mode}`);
      });
    });

    // --- LIBRARY / FILE BROWSER ---
    async function loadBrowser(path = "") {
      try {
        const url = path ? '/api/browser?path=' + encodeURIComponent(path) : '/api/browser';
        const res = await fetch(url);
        if (!res.ok) throw new Error("Failed to load browser");
        const data = await res.json();
        currentBrowserData = data;
        currentBrowserPath = data.current_path;
        renderBrowser(data, document.getElementById('browserSearchInput').value.trim().toLowerCase());
      } catch (e) {
        console.error("Browser error:", e);
      }
    }

    function renderBrowser(data, filter = "") {
      document.getElementById('browserCurrentPath').textContent = data.current_path || "/";
      document.getElementById('btnBrowserUp').disabled = !data.parent_path;

      // Bookmarks chips
      const bmRow = document.getElementById('bookmarksRow');
      if (data.bookmarks && data.bookmarks.length > 0) {
        bmRow.style.display = "flex";
        bmRow.innerHTML = data.bookmarks.map(b => {
          const parts = b.split(/[/\\]/);
          const name = parts[parts.length - 1] || parts[parts.length - 2] || b;
          return `<button class="chip-btn" onclick="browseTo('${escapeAttr(b)}')">★ ${escapeHtml(name)}</button>`;
        }).join('');
      } else {
        bmRow.style.display = "none";
      }

      // Filter and render entries
      let entries = data.entries || [];
      if (filter) {
        entries = entries.filter(e => e.name.toLowerCase().includes(filter));
      }

      const list = document.getElementById('browserList');
      if (entries.length === 0) {
        list.innerHTML = `<div class="empty-sub" style="text-align:center; padding: 2rem 0;">No matching files or folders</div>`;
        return;
      }

      list.innerHTML = entries.map(item => {
        if (item.is_dir) {
          return `
            <div class="browser-item dir" onclick="browseTo('${escapeAttr(item.path)}')">
              <span class="browser-icon">📁</span>
              <span class="browser-name">${escapeHtml(item.name)}/</span>
              <span class="browser-arrow">›</span>
            </div>`;
        } else {
          return `
            <div class="browser-item file" onclick="addFile('${escapeAttr(item.path)}')">
              <span class="browser-icon">🎵</span>
              <span class="browser-name">${escapeHtml(item.name)}</span>
              <button class="btn-icon-sm add-btn" title="Add to queue" onclick="event.stopPropagation(); addFile('${escapeAttr(item.path)}')">
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5">
                  <line x1="12" y1="5" x2="12" y2="19"></line>
                  <line x1="5" y1="12" x2="19" y2="12"></line>
                </svg>
              </button>
            </div>`;
        }
      }).join('');
    }

    function browseTo(path) {
      document.getElementById('browserSearchInput').value = "";
      loadBrowser(path);
    }

    document.getElementById('btnBrowserUp').addEventListener('click', () => {
      if (currentBrowserData && currentBrowserData.parent_path) {
        browseTo(currentBrowserData.parent_path);
      }
    });

    document.getElementById('btnBrowserAddAll').addEventListener('click', () => {
      if (currentBrowserPath) {
        sendAction('add_folder', null, { path: currentBrowserPath });
        showToast('Adding all audio files in folder...');
      }
    });

    document.getElementById('browserSearchInput').addEventListener('input', (e) => {
      if (currentBrowserData) {
        renderBrowser(currentBrowserData, e.target.value.trim().toLowerCase());
      }
    });

    async function addFile(path) {
      await sendAction('add_file', null, { path });
      showToast('Added track to queue');
    }

    // --- DEVICES TAB ---
    function renderDevices(data) {
      const devices = data.devices || [];
      const scanTag = document.getElementById('deviceScanTag');
      if (data.is_scanning) {
        scanTag.textContent = "(Scanning...)";
      } else {
        scanTag.textContent = "";
      }

      const list = document.getElementById('devicesList');
      if (devices.length === 0) {
        list.innerHTML = `<div class="empty-sub" style="text-align:center; padding: 2rem 0;">No devices detected. Click Rescan to search.</div>`;
        return;
      }

      list.innerHTML = devices.map(dev => `
        <div class="device-item ${dev.is_active ? 'active' : ''}" onclick="selectDevice(${dev.id})">
          <div class="device-radio">${dev.is_active ? '●' : '○'}</div>
          <div class="device-info">
            <div class="device-name">
              ${escapeHtml(dev.name)}
              ${dev.is_active ? '<span class="active-tag">ACTIVE</span>' : ''}
            </div>
            <div class="device-host">${escapeHtml(dev.host_port)}</div>
          </div>
        </div>
      `).join('');
    }

    function selectDevice(id) {
      sendAction('select_device', null, { id });
      showToast('Switching audio output...');
    }

    document.getElementById('btnRescanDevices').addEventListener('click', () => {
      sendAction('rescan_devices');
      showToast('Scanning network for DLNA renderers...');
    });

    // Keyboard shortcuts
    window.addEventListener('keydown', (e) => {
      if (e.target && (e.target.tagName === 'INPUT' || e.target.tagName === 'TEXTAREA')) return;
      if (e.code === 'Space') {
        e.preventDefault();
        sendAction('toggle_pause');
      } else if (e.code === 'ArrowLeft') {
        e.preventDefault();
        sendAction('seek_relative', -10);
      } else if (e.code === 'ArrowRight') {
        e.preventDefault();
        sendAction('seek_relative', 10);
      }
    });

    // Smooth UI interpolation loop (100ms)
    setInterval(updateProgressUI, 100);

    // Polling loop (600ms)
    setInterval(pollStatus, 600);
    pollStatus();
  </script>
</body>
</html>
"##;
