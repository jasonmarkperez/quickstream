//! Byte-range file streaming (`/stream/<name>`) and the continuous WAV stream for UPnP renderers.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use super::file_checks::{FileChecks, LookOutcome};
use super::server::write_not_found;
use super::state::{NowPlayingState, WebCommand};

/// An armed continuous stream: the renderer must request `/stream/continuous-<nonce>.wav`.
#[derive(Debug)]
struct ContinuousSession {
    nonce: String,
    /// The first peer that connected; other hosts are refused for the rest of the session.
    peer: Option<IpAddr>,
}

/// Whether a request for a continuous stream path may be served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContinuousAdmission {
    /// Serve the stream for the session with this nonce.
    Admitted(String),
    /// No session is armed, or the path does not name it (404).
    NotFound,
    /// The session belongs to another host (409).
    Conflict,
}

const CONTINUOUS_PREFIX: &str = "/stream/continuous-";
const CONTINUOUS_SUFFIX: &str = ".wav";

/// The nonce in a `/stream/continuous-<32 hex>.wav` route, if `route` has that shape.
fn continuous_route_nonce(route: &str) -> Option<&str> {
    let nonce = route.strip_prefix(CONTINUOUS_PREFIX)?.strip_suffix(CONTINUOUS_SUFFIX)?;
    (nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_hexdigit())).then_some(nonce)
}

/// Whether `route` (path without query) addresses the continuous stream, armed or not.
pub(super) fn is_continuous_route(route: &str) -> bool {
    route == "/stream/continuous.wav" || route == "/stream/autopilot.wav" || continuous_route_nonce(route).is_some()
}

/// A skip or seek for the continuous stream (`skip_to_index` / `seek_to`), pending until the
/// stream picks its next track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamRequest {
    /// The queue index to play: the track skipped to, or for a seek the track it was asked on.
    pub index: usize,
    /// Where to start it, in seconds.
    pub position: f64,
}

/// Controls continuous PCM gapless streaming for DLNA renderers.
#[derive(Debug, Default)]
pub struct ContinuousStreamController {
    pub is_active: Arc<AtomicBool>,
    pub active_conn_id: Arc<AtomicU64>,
    /// The track the stream plays, or was last asked to play: set by `skip_to_index` at once and
    /// by the stream when a track starts giving audio (lock order: the web state, then `request`,
    /// then this).
    pub current_track_index: Arc<Mutex<usize>>,
    pub current_position: Arc<Mutex<f64>>,
    pub current_duration: Arc<Mutex<f64>>,
    /// The running decoder and the connection that started it.
    decoder_child: Mutex<Option<(u64, std::process::Child)>>,
    /// The skip or seek the stream hasn't acted on yet. Set before the decoder is killed and
    /// taken in one step when the stream picks its next track, so none is lost or applied twice.
    request: Mutex<Option<StreamRequest>>,
    /// Bumped with `request` held whenever anyone but the stream sets or clears it, so the stream
    /// tells a new request from the one it is looking for, the same skip asked again too.
    request_seq: AtomicU64,
    /// The ffmpeg the stream runs (`None`: `ffmpeg` on `PATH`); see [`Self::set_decoder`].
    decoder: Mutex<Option<PathBuf>>,
    session: Mutex<Option<ContinuousSession>>,
    /// The track being decoded: its queue index and path. Set with `current_duration` held, so
    /// the two always name the same track (lock order: the web state, then `current_duration`,
    /// then this).
    decoding: Mutex<Option<(usize, PathBuf)>>,
    /// Whether the queue's files are there, checked off the stream's write path (#117); see
    /// [`Self::set_path_check`].
    file_checks: FileChecks,
}

impl ContinuousStreamController {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a new continuous session (replacing any previous one) and return the URL path the
    /// renderer must request. It starts at `current_track_index`, with no request pending.
    pub fn arm_session(&self) -> String {
        let nonce = crate::web_access::generate_token();
        let path = format!("{}{}{}", CONTINUOUS_PREFIX, nonce, CONTINUOUS_SUFFIX);
        *self.session.lock().unwrap() = Some(ContinuousSession { nonce, peer: None });
        self.set_request(None);
        path
    }

    /// End the continuous session: its path stops being served, and a skip or seek it never
    /// acted on goes with it.
    pub fn disarm_session(&self) {
        *self.session.lock().unwrap() = None;
        self.set_request(None);
    }

    fn set_request(&self, request: Option<StreamRequest>) {
        let mut pending = self.request.lock().unwrap();
        *pending = request;
        self.request_seq.fetch_add(1, Ordering::SeqCst);
    }

    /// The URL path of the armed session, if any.
    pub fn session_path(&self) -> Option<String> {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| format!("{}{}{}", CONTINUOUS_PREFIX, s.nonce, CONTINUOUS_SUFFIX))
    }

    /// Decide whether `peer` may stream `route` (a path without query). The first admitted peer
    /// owns the session.
    pub fn admit(&self, route: &str, peer: IpAddr) -> ContinuousAdmission {
        let Some(requested) = continuous_route_nonce(route) else {
            return ContinuousAdmission::NotFound;
        };
        let mut guard = self.session.lock().unwrap();
        let Some(session) = guard.as_mut() else {
            return ContinuousAdmission::NotFound;
        };
        if !crate::web_access::constant_time_eq(requested, &session.nonce) {
            return ContinuousAdmission::NotFound;
        }
        match session.peer {
            Some(owner) if owner != peer => ContinuousAdmission::Conflict,
            _ => {
                session.peer = Some(peer);
                ContinuousAdmission::Admitted(session.nonce.clone())
            }
        }
    }

    /// Whether the session with `nonce` is still the armed one.
    fn is_session(&self, nonce: &str) -> bool {
        self.session.lock().unwrap().as_ref().is_some_and(|s| s.nonce == nonce)
    }

    /// If the length of the track being decoded (`path`, at `index` in the queue) was unknown
    /// when it started (placeholder metadata, tags not read yet), take it from `state`'s queue
    /// once the UI has published it there. Called while streaming, so the length (and the seek
    /// clamp that needs it) shows without waiting for the next track. Returns whether it was
    /// learned now.
    pub fn learn_duration(&self, state: &mut NowPlayingState, index: usize, path: &Path) -> bool {
        let current = self.current_duration.lock().unwrap();
        Self::learn_duration_locked(current, state, index, path)
    }

    /// The stream has started `index` (`path`) with `duration` (0 when unknown): set both
    /// together, so a length learned from the published queue is always the one of the track
    /// being decoded.
    fn start_track(&self, index: usize, path: &Path, duration: f64) {
        let mut current = self.current_duration.lock().unwrap();
        *current = duration;
        *self.decoding.lock().unwrap() = Some((index, path.to_path_buf()));
    }

    /// Whoever publishes the queue into `state` (the UI's `update_web_state`) offers the stream
    /// the lengths it now knows: if the track being decoded started with an unknown length and
    /// `state`'s queue has it at the same index and path, it is learned now, whatever the stream's
    /// socket is doing (a renderer reading at real time can keep the stream's writer blocked
    /// for seconds). Never blocks on the socket. Returns whether it was learned now.
    pub fn learn_published_duration(&self, state: &mut NowPlayingState) -> bool {
        if !self.is_active.load(Ordering::SeqCst) {
            return false;
        }
        let current = self.current_duration.lock().unwrap();
        let Some((index, path)) = self.decoding.lock().unwrap().clone() else {
            return false;
        };
        Self::learn_duration_locked(current, state, index, &path)
    }

    fn learn_duration_locked(
        mut current: std::sync::MutexGuard<'_, f64>,
        state: &mut NowPlayingState,
        index: usize,
        path: &Path,
    ) -> bool {
        if *current > 0.0 {
            return false;
        }
        let known = state
            .queue
            .get(index)
            .filter(|t| Path::new(&t.filepath) == path)
            .map(|t| t.duration_seconds)
            .filter(|d| *d > 0.0);
        let Some(duration) = known else {
            return false;
        };
        *current = duration;
        state.duration_seconds = duration;
        true
    }

    /// Play the track at `index` from its start. Replaces any pending request (a seek asked
    /// before it was for the track it leaves). `current_track_index` names it at once, so the UI
    /// following that index stays on it while the stream is busy (blocked writing to the
    /// renderer) and a second skip goes on from it.
    pub fn skip_to_index(&self, index: usize) {
        {
            let mut request = self.request.lock().unwrap();
            *request = Some(StreamRequest { index, position: 0.0 });
            self.request_seq.fetch_add(1, Ordering::SeqCst);
            *self.current_track_index.lock().unwrap() = index;
        }
        self.kill_active_decoder();
    }

    /// Restart the track at `position` seconds: the track skipped to when a skip is pending,
    /// else the one `current_track_index` names, so a seek that arrives as a track ends is never
    /// applied to the next one. A position below 0 or not a number starts it from 0.
    pub fn seek_to(&self, position: f64) {
        let position = if position.is_finite() && position > 0.0 {
            position
        } else {
            0.0
        };
        {
            let mut request = self.request.lock().unwrap();
            self.request_seq.fetch_add(1, Ordering::SeqCst);
            match request.as_mut() {
                Some(pending) => pending.position = position,
                None => {
                    let index = *self.current_track_index.lock().unwrap();
                    *request = Some(StreamRequest { index, position });
                }
            }
        }
        self.kill_active_decoder();
    }

    /// The skip or seek the stream hasn't acted on yet, if any.
    pub fn pending_request(&self) -> Option<StreamRequest> {
        *self.request.lock().unwrap()
    }

    fn has_request(&self) -> bool {
        self.request.lock().unwrap().is_some()
    }

    /// Whether connection `cid` still owns the stream (0: a connection that couldn't be tracked).
    fn owns(&self, cid: u64) -> bool {
        cid == 0 || self.active_conn_id.load(Ordering::SeqCst) == cid
    }

    /// Register `child` as connection `cid`'s decoder. A decoder of a connection `cid` has
    /// superseded is stopped first, never left running. If `cid` has been superseded itself,
    /// `child` is stopped instead, `taken` (the request this pass took) is handed back for the
    /// connection that took over, and it returns false.
    fn register_decoder(&self, cid: u64, mut child: std::process::Child, taken: Option<StreamRequest>) -> bool {
        {
            let mut slot = self.decoder_child.lock().unwrap();
            if self.owns(cid) {
                if let Some((_, mut superseded)) = slot.take() {
                    let _ = superseded.kill();
                    let _ = superseded.wait();
                }
                *slot = Some((cid, child));
                return true;
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        if let Some(taken) = taken {
            let mut pending = self.request.lock().unwrap();
            if pending.is_none() {
                *pending = Some(taken);
                self.request_seq.fetch_add(1, Ordering::SeqCst);
            }
        }
        false
    }

    /// Stop connection `cid`'s decoder, if it is the one running.
    fn kill_decoder_of(&self, cid: u64) {
        let mut slot = self.decoder_child.lock().unwrap();
        if slot.as_ref().is_some_and(|(owner, _)| *owner == cid) {
            if let Some((_, mut child)) = slot.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// Show the track the stream has started: its index (unless a skip or seek has come since,
    /// or the connection or session is no longer the stream's), length and start position, and
    /// in `state` as now playing.
    #[allow(clippy::too_many_arguments)]
    fn publish_track(
        &self,
        state: &RwLock<NowPlayingState>,
        cid: u64,
        session: &str,
        index: usize,
        path: &Path,
        duration: f64,
        position: f64,
        queue_len: usize,
    ) {
        let Ok(mut s) = state.write() else {
            return;
        };
        {
            let request = self.request.lock().unwrap();
            if request.is_none() && self.owns(cid) && self.is_session(session) {
                *self.current_track_index.lock().unwrap() = index;
            }
        }
        self.start_track(index, path, duration);
        *self.current_position.lock().unwrap() = position;

        s.queue_index = index;
        if let Some(t) = s.queue.get(index).cloned() {
            s.title = t.title;
            s.artist = t.artist;
            s.album = t.album;
            s.duration_seconds = t.duration_seconds;
            s.filepath = Some(PathBuf::from(&t.filepath));
            s.position_seconds = position;
            s.is_playing = true;
            s.is_paused = false;
        }
        if queue_len > 1 {
            let next_idx = (index + 1) % queue_len;
            s.next_track_title = s.queue.get(next_idx).map(|t| t.title.clone());
        }
    }

    /// Run `binary` as the stream's ffmpeg instead of `ffmpeg` on `PATH` (the test seam for a
    /// fake decoder).
    pub fn set_decoder(&self, binary: impl Into<PathBuf>) {
        *self.decoder.lock().unwrap() = Some(binary.into());
    }

    /// Tell whether a queued file is there with `check` instead of `Path::exists` (the test seam
    /// for a share that hangs). It runs on a helper thread, at most once at a time per path.
    pub fn set_path_check(&self, check: impl Fn(&Path) -> bool + Send + Sync + 'static) {
        self.file_checks.set_check(Arc::new(check));
    }

    fn decoder(&self) -> PathBuf {
        self.decoder
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| PathBuf::from("ffmpeg"))
    }

    pub fn kill_active_decoder(&self) {
        if let Some((_, mut child)) = self.decoder_child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Manages registered media files and active streaming TCP connections.
pub struct StreamServerState {
    pub active_sockets: Mutex<HashMap<u64, TcpStream>>,
    pub file_registry: RwLock<HashMap<String, PathBuf>>,
    pub continuous: Arc<ContinuousStreamController>,
    next_conn_id: AtomicU64,
}

impl Default for StreamServerState {
    fn default() -> Self {
        Self {
            active_sockets: Mutex::new(HashMap::new()),
            file_registry: RwLock::new(HashMap::new()),
            continuous: Arc::new(ContinuousStreamController::new()),
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
        // Lossy so names that are not valid UTF-8 match the lossily encoded URLs built by upnp.rs.
        if let Some(filename) = path.file_name().map(|f| f.to_string_lossy()) {
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

    /// Start a continuous session; see [`ContinuousStreamController::arm_session`].
    pub fn arm_continuous_session(&self) -> String {
        self.continuous.arm_session()
    }

    /// The URL path of the armed continuous session, if any.
    pub fn continuous_session_path(&self) -> Option<String> {
        self.continuous.session_path()
    }

    /// Immediately abort and close all active streaming TCP connections to stereos, and end any
    /// continuous session.
    pub fn close_active_streams(&self) {
        self.continuous.disarm_session();
        self.continuous.is_active.store(false, Ordering::SeqCst);
        self.continuous.kill_active_decoder();
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

/// Largest `fmt ` chunk a WAV seek reads and copies; the extensible form is 40 bytes.
const MAX_WAV_FMT_CHUNK: u64 = 64 * 1024;

/// A WAV size field that means "unknown length": what ffmpeg writes when it outputs to a pipe.
const WAV_UNKNOWN_SIZE: u32 = u32::MAX;

/// `fmt ` format tags whose byte rate a seek works out from the sample rate and block align.
const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// The chunks of a WAV file that a seek needs.
struct WavLayout {
    /// The `fmt ` chunk's body, as it is in the file.
    fmt: Vec<u8>,
    /// Where the `data` chunk's audio starts and ends in the file (clamped to the file).
    data_start: u64,
    data_end: u64,
    /// The `data` size field was 0 or `0xFFFFFFFF`: the audio runs to the end of the file.
    size_unknown: bool,
}

/// Walk the RIFF chunks of a WAV file of `file_len` bytes, with every size bounded by the file.
fn read_wav_layout(file: &mut std::fs::File, file_len: u64) -> Result<WavLayout, &'static str> {
    use std::io::{Read, Seek, SeekFrom};
    let mut riff_hdr = [0u8; 12];
    file.seek(SeekFrom::Start(0)).map_err(|_| "can't read the file")?;
    if file.read_exact(&mut riff_hdr).is_err() || &riff_hdr[0..4] != b"RIFF" || &riff_hdr[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file");
    }
    let mut fmt: Option<Vec<u8>> = None;
    let mut data: Option<(u64, u64, bool)> = None;
    let mut pos = 12u64;
    while pos.saturating_add(8) <= file_len {
        let mut chunk_hdr = [0u8; 8];
        if file.seek(SeekFrom::Start(pos)).is_err() || file.read_exact(&mut chunk_hdr).is_err() {
            break;
        }
        let size = u32::from_le_bytes(chunk_hdr[4..8].try_into().unwrap());
        let body = pos + 8;
        let available = file_len - body;
        match &chunk_hdr[0..4] {
            b"fmt " if fmt.is_none() => {
                if size < 16 {
                    return Err("fmt chunk shorter than 16 bytes");
                }
                if u64::from(size) > MAX_WAV_FMT_CHUNK.min(available) {
                    return Err("fmt chunk larger than 64 KiB or the file");
                }
                let mut bytes = vec![0u8; size as usize];
                file.read_exact(&mut bytes).map_err(|_| "can't read the fmt chunk")?;
                fmt = Some(bytes);
            }
            b"data" if data.is_none() => {
                let size_unknown = size == 0 || size == WAV_UNKNOWN_SIZE;
                let len = if size_unknown {
                    available
                } else {
                    u64::from(size).min(available)
                };
                data = Some((body, body + len, size_unknown));
                // Past audio of unknown length there are no chunk boundaries to follow.
                if size_unknown {
                    break;
                }
            }
            _ => {}
        }
        if fmt.is_some() && data.is_some() {
            break;
        }
        // Chunks are padded to an even length.
        pos = body + u64::from(size) + u64::from(size & 1);
    }
    let fmt = fmt.ok_or("no fmt chunk before the audio")?;
    let (data_start, data_end, size_unknown) = data.ok_or("no data chunk")?;
    Ok(WavLayout {
        fmt,
        data_start,
        data_end,
        size_unknown,
    })
}

/// The RIFF and `data` size fields of a seeked WAV whose `fmt ` chunk body is `fmt_len` bytes and
/// whose audio is `audio_len` bytes.
#[doc(hidden)]
pub fn wav_header_sizes(fmt_len: u64, audio_len: u64, size_unknown: bool) -> (u32, u32) {
    // "WAVE", the fmt chunk with its pad byte, the data chunk's header and the audio.
    let riff = 4 + 8 + fmt_len + (fmt_len & 1) + 8 + audio_len;
    // Both known or both unknown: the RIFF size is the larger, so it decides for both. It must
    // stay under WAV_UNKNOWN_SIZE, which would read as unknown.
    match (size_unknown, u32::try_from(riff)) {
        (false, Ok(riff)) if riff != WAV_UNKNOWN_SIZE => (riff, audio_len as u32),
        _ => (WAV_UNKNOWN_SIZE, WAV_UNKNOWN_SIZE),
    }
}

/// Seek a WAV file of `file_len` bytes to `offset_seconds`: the audio from the block at or before
/// that time to the last whole block of the `data` chunk, behind a new RIFF header with the file's
/// own `fmt ` chunk (so WAVE_FORMAT_EXTENSIBLE and 24-bit files stay decodable) and sizes that
/// match it.
fn find_wav_pcm_offset(
    file: &mut std::fs::File,
    offset_seconds: f64,
    file_len: u64,
) -> Result<StreamOffset, &'static str> {
    let layout = read_wav_layout(file, file_len)?;
    let fmt = &layout.fmt;
    let format_tag = u16::from_le_bytes(fmt[0..2].try_into().unwrap());
    let sample_rate = u64::from(u32::from_le_bytes(fmt[4..8].try_into().unwrap()));
    let block_align = u64::from(u16::from_le_bytes(fmt[12..14].try_into().unwrap()));
    // PCM and WAVE_FORMAT_EXTENSIBLE play a block per sample, so their byte rate follows from the
    // sample rate; some writers leave nAvgBytesPerSec wrong. Other formats only have that field.
    let byte_rate = match format_tag {
        WAVE_FORMAT_PCM | WAVE_FORMAT_EXTENSIBLE => sample_rate * block_align,
        _ => u64::from(u32::from_le_bytes(fmt[8..12].try_into().unwrap())),
    };
    if byte_rate == 0 || block_align == 0 {
        return Err("zero byte rate or block align");
    }

    // Whole blocks only, at both ends: a truncated file or an odd data size ends in a partial
    // frame, which is left out of the audio and the header's size.
    let whole_blocks = |len: u64| len / block_align * block_align;
    let data_len = whole_blocks(layout.data_end - layout.data_start);
    // The cast saturates, so a `pos` past the end lands at the end of the last whole block: no audio.
    let wanted = (offset_seconds * byte_rate as f64) as u64;
    let skip = whole_blocks(wanted).min(data_len);
    let start = layout.data_start + skip;
    let end = layout.data_start + data_len;
    let remaining = end - start;

    let fmt_len = fmt.len() as u64;
    let fmt_pad = fmt_len & 1;
    let (riff_size, data_size) = wav_header_sizes(fmt_len, remaining, layout.size_unknown);

    let mut hdr = Vec::with_capacity(28 + fmt.len() + 1);
    hdr.extend_from_slice(b"RIFF");
    hdr.extend_from_slice(&riff_size.to_le_bytes());
    hdr.extend_from_slice(b"WAVEfmt ");
    hdr.extend_from_slice(&(fmt_len as u32).to_le_bytes());
    hdr.extend_from_slice(fmt);
    if fmt_pad == 1 {
        hdr.push(0);
    }
    hdr.extend_from_slice(b"data");
    hdr.extend_from_slice(&data_size.to_le_bytes());
    Ok(StreamOffset {
        start,
        end,
        header: Some(hdr),
    })
}

/// What a `/stream/<name>?pos=` response sends: `header`, then the file's bytes `start..end`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StreamOffset {
    /// The first file byte sent.
    pub(super) start: u64,
    /// One past the last file byte sent: the file's length, or the end of a WAV's last whole block.
    pub(super) end: u64,
    /// A header sent before the file's bytes (a seeked WAV's new RIFF header).
    pub(super) header: Option<Vec<u8>>,
}

pub(super) fn calculate_stream_offset(
    filepath: &std::path::Path,
    file: &mut std::fs::File,
    offset_seconds: f64,
    total_size: u64,
    duration_hint: f64,
) -> StreamOffset {
    let from_start = StreamOffset {
        start: 0,
        end: total_size,
        header: None,
    };
    if offset_seconds <= 0.0 || total_size == 0 {
        return from_start;
    }
    let ext = filepath
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if ext == "wav" || ext == "wave" {
        return match find_wav_pcm_offset(file, offset_seconds, total_size) {
            Ok(offset) => offset,
            Err(why) => {
                crate::log_warn!(
                    "web",
                    "Can't seek '{}' to {:.2}s ({}); serving it from the start",
                    filepath.display(),
                    offset_seconds,
                    why
                );
                from_start
            }
        };
    }

    let duration = if duration_hint > 0.0 {
        duration_hint
    } else {
        crate::metadata::extract_metadata(filepath)
            .map(|m| m.duration_seconds)
            .unwrap_or(0.0)
    };
    let start = if ext == "mp3" {
        find_mp3_stream_offset(file, offset_seconds, duration, total_size)
    } else {
        let dur = if duration > 0.0 { duration } else { 1.0 };
        let ratio = (offset_seconds / dur).clamp(0.0, 0.995);
        (total_size as f64 * ratio) as u64
    };
    StreamOffset {
        start,
        end: total_size,
        header: None,
    }
}

/// Resolve a `/stream/<name>` request to a file the player has explicitly exposed.
///
/// Only files in the stream registry, or the current now-playing file matched by file
/// name, are served. Raw filesystem paths are never accepted, so a LAN client cannot
/// read arbitrary files from disk.
fn resolve_stream_file(name: &str, stream_state: &StreamServerState, current: Option<&Path>) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return None;
    }
    // The now-playing file wins over the registry: the registry is keyed by bare file name, so a
    // later registration of a same-named track from another album would otherwise replace it.
    let found = current
        .filter(|p| {
            p.file_name()
                .is_some_and(|f| f.to_string_lossy().to_lowercase() == name.to_lowercase())
        })
        .map(Path::to_path_buf)
        .or_else(|| stream_state.lookup_file(name))?;
    found.is_file().then_some(found)
}

/// How a `Range` request header applies to a representation of a given length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteRange {
    /// No usable range: send the whole representation with `200 OK`.
    Full,
    /// Send bytes `start..=end` with `206 Partial Content`.
    Partial { start: u64, end: u64 },
    /// No requested range overlaps the representation: send `416`.
    Unsatisfiable,
}

/// Evaluate a `Range` header value against a representation of `len` bytes (RFC 9110 §14).
///
/// - Invalid syntax or a unit other than `bytes` is ignored (`Full`), as RFC 9110 allows.
/// - Suffix ranges (`bytes=-N`) select the last N bytes.
/// - With several ranges, only the first satisfiable one is served, as a single-part 206 whose
///   `Content-Range` says which bytes it holds. Renderers do not send multi-range requests, and
///   this avoids multipart/byteranges responses; a 416 would wrongly claim nothing overlaps.
/// - `Unsatisfiable` only when no range overlaps the representation (including `first > last`).
pub fn parse_byte_range(header: &str, len: u64) -> ByteRange {
    fn decimal(s: &str) -> Option<u64> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        // Digits only, so a parse failure means overflow; any such value is past the end.
        Some(s.parse().unwrap_or(u64::MAX))
    }

    let Some((unit, set)) = header.trim().split_once('=') else {
        return ByteRange::Full;
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        return ByteRange::Full;
    }

    let mut any_spec = false;
    let mut first_satisfiable = None;
    for spec in set.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        any_spec = true;
        let Some((first, last)) = spec.split_once('-') else {
            return ByteRange::Full;
        };
        let range = if first.is_empty() {
            let Some(suffix) = decimal(last) else {
                return ByteRange::Full;
            };
            (suffix > 0 && len > 0).then(|| (len.saturating_sub(suffix), len - 1))
        } else {
            let Some(start) = decimal(first) else {
                return ByteRange::Full;
            };
            let end = if last.is_empty() {
                u64::MAX
            } else {
                match decimal(last) {
                    Some(end) => end,
                    None => return ByteRange::Full,
                }
            };
            (start <= end && start < len).then(|| (start, end.min(len - 1)))
        };
        // Keep parsing so a later malformed spec still makes the whole header invalid.
        if first_satisfiable.is_none() {
            first_satisfiable = range;
        }
    }

    match first_satisfiable {
        Some((start, end)) => ByteRange::Partial { start, end },
        None if any_spec => ByteRange::Unsatisfiable,
        None => ByteRange::Full,
    }
}

pub(super) fn serve_audio_stream(
    mut stream: TcpStream,
    route: &str,
    query: Option<&str>,
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
    let _ = stream.set_nodelay(true);

    // 1. Resolve target file and optional seek offset query parameter
    // `route` and `query` are split by the router, so this is the file the route matched.
    let encoded_path = route.strip_prefix("/stream/").unwrap_or("");
    let decoded_path = crate::player::upnp::url_decode(encoded_path);

    let mut offset_seconds: f64 = 0.0;
    if let Some(query) = query {
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

    let filepath = match resolve_stream_file(&decoded_path, stream_state, current_filepath.as_deref()) {
        Some(p) => p,
        None => {
            crate::log_warn!(
                "web",
                "Stream 404: '{}' is not a registered or now-playing file",
                decoded_path
            );
            return write_not_found(&mut stream, route, head_only);
        }
    };

    let mut file = match std::fs::File::open(&filepath) {
        Ok(f) => f,
        Err(e) => {
            crate::log_warn!("web", "Stream 404: Failed to open '{}': {}", filepath.display(), e);
            return write_not_found(&mut stream, route, head_only);
        }
    };

    let total_size = file.metadata()?.len();
    let mime = crate::metadata::get_audio_mime_type(&filepath);

    // 2. Calculate base byte offset for seek queries (e.g. ?pos=12.5)
    //    A seeked WAV sends a new header, then only its `data` chunk from the seek point.
    let StreamOffset {
        start: base_offset,
        end: body_end,
        header: synthesized_hdr,
    } = calculate_stream_offset(&filepath, &mut file, offset_seconds, total_size, current_duration);

    let header_len = synthesized_hdr.as_ref().map_or(0, |hdr| hdr.len() as u64);
    let effective_total = header_len + body_end.saturating_sub(base_offset);

    // 3. Parse Range header if present
    let range = range_header.map_or(ByteRange::Full, |h| parse_byte_range(h, effective_total));
    let (req_start, req_end, is_range) = match range {
        ByteRange::Full => (0, effective_total.saturating_sub(1), false),
        ByteRange::Partial { start, end } => (start, end, true),
        ByteRange::Unsatisfiable => {
            crate::log_warn!(
                "web",
                "Stream 416 Range Not Satisfiable: {:?} of {} bytes for client {}",
                range_header,
                effective_total,
                client_ip
            );
            let err_resp = format!(
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                effective_total
            );
            stream.write_all(err_resp.as_bytes())?;
            stream.flush()?;
            return Ok(());
        }
    };

    let content_length = if is_range {
        req_end - req_start + 1
    } else {
        effective_total
    };

    // 4. Write DLNA HTTP headers
    let status_line = if is_range {
        "HTTP/1.1 206 Partial Content"
    } else {
        "HTTP/1.1 200 OK"
    };

    let display_name = filepath.file_name().and_then(|f| f.to_str()).unwrap_or("track");

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
        Connection: close\r\n",
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

    if streamed_count < content_length {
        // The file shrank or failed to read after Content-Length was sent. Close at once so the
        // client sees an incomplete response instead of a silently short body.
        crate::log_warn!(
            "web",
            "Source for '{}' ended after {} of {} advertised bytes; closing connection to {}",
            display_name,
            streamed_count,
            content_length,
            client_ip
        );
        let _ = stream.shutdown(std::net::Shutdown::Both);
        return Ok(());
    }

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

/// Construct a standard 44-byte RIFF/WAV header for an infinite (live) 16-bit 44.1kHz stereo PCM stream.
pub fn make_continuous_wav_header() -> [u8; 44] {
    let mut hdr = [0u8; 44];
    hdr[0..4].copy_from_slice(b"RIFF");
    hdr[4..8].copy_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // Streaming/infinite length
    hdr[8..12].copy_from_slice(b"WAVE");
    hdr[12..16].copy_from_slice(b"fmt ");
    hdr[16..20].copy_from_slice(&16u32.to_le_bytes()); // Subchunk1 size: 16
    hdr[20..22].copy_from_slice(&1u16.to_le_bytes()); // Audio format: PCM (1)
    hdr[22..24].copy_from_slice(&2u16.to_le_bytes()); // Channels: 2 (Stereo)
    hdr[24..28].copy_from_slice(&44100u32.to_le_bytes()); // Sample rate: 44,100 Hz
    hdr[28..32].copy_from_slice(&(44100u32 * 2 * 2).to_le_bytes()); // Byte rate: 176,400 B/s
    hdr[32..34].copy_from_slice(&4u16.to_le_bytes()); // Block align: 4
    hdr[34..36].copy_from_slice(&16u16.to_le_bytes()); // Bits per sample: 16
    hdr[36..40].copy_from_slice(b"data");
    hdr[40..44].copy_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // Data size: infinite
    hdr
}

/// Bytes per second of the continuous stream's PCM (44.1 kHz, 16-bit, stereo).
const PCM_BYTES_PER_SEC: f64 = 176_400.0;
/// How often idle silence is sent.
const SILENCE_TICK: Duration = Duration::from_millis(50);
/// How far ahead of real time idle silence runs, so a renderer reading at real time never drains.
const SILENCE_LEAD: Duration = Duration::from_millis(100);
/// The most silence one tick sends after a write blocked: a stalled renderer is not owed it all.
const SILENCE_MAX_BURST: Duration = Duration::from_secs(1);
/// How often a queue whose files are all missing is looked through again.
const MISSING_RECHECK: Duration = Duration::from_secs(1);
/// The longest one pass waits for file checks before it sends silence instead (#117).
const FILE_CHECK_WAIT: Duration = Duration::from_millis(20);
// A pass's wait on file checks comes out of its tick, so silence still goes out every tick, well
// before the lead runs out.
const _: () = assert!(FILE_CHECK_WAIT.as_millis() < SILENCE_TICK.as_millis());
const _: () = assert!(SILENCE_TICK.as_millis() < SILENCE_LEAD.as_millis());

/// The PCM bytes in `duration`, whole 4-byte frames.
fn pcm_bytes(duration: Duration) -> u64 {
    (duration.as_secs_f64() * PCM_BYTES_PER_SEC) as u64 / 4 * 4
}

/// Sends silence at the stream's real-time byte rate while there is nothing to decode (empty
/// queue, every file missing, a file check not answered yet). Measured from when it started, so
/// sleep overshoot doesn't add up.
struct SilencePacer {
    started: Instant,
    sent: u64,
}

impl SilencePacer {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            sent: 0,
        }
    }

    /// Write the silence due by now (plus `SILENCE_LEAD`), then wait until one tick after
    /// `pass_started`, so time the pass spent waiting on file checks doesn't add to the gap
    /// between writes. Returns the bytes written, or the write error, which means the renderer
    /// has gone.
    fn send(&mut self, stream: &mut impl Write, pass_started: Instant) -> std::io::Result<u64> {
        let due = pcm_bytes(self.started.elapsed() + SILENCE_LEAD);
        let mut owed = due.saturating_sub(self.sent).min(pcm_bytes(SILENCE_MAX_BURST));
        let written = owed;
        self.sent = due;
        let chunk = [0u8; 8820];
        while owed > 0 {
            let n = owed.min(chunk.len() as u64) as usize;
            stream.write_all(&chunk[..n])?;
            owed -= n as u64;
        }
        wait_out_tick(pass_started);
        Ok(written)
    }
}

/// Sleep until one `SILENCE_TICK` after `pass_started`.
fn wait_out_tick(pass_started: Instant) {
    std::thread::sleep((pass_started + SILENCE_TICK).saturating_duration_since(Instant::now()));
}

/// How far the PCM written since `connected` (`written` bytes, audio and silence) runs ahead of
/// a renderer that has played it at real time since then: what its buffers still hold.
fn renderer_lead(connected: Instant, written: u64) -> Duration {
    Duration::from_secs_f64(written as f64 / PCM_BYTES_PER_SEC).saturating_sub(connected.elapsed())
}

/// Serve a continuous 16-bit 44.1kHz PCM WAV stream to DLNA renderers.
/// Slices tracks sequentially directly into the exact same TCP socket without drops or DAC resets.
/// `session` is the nonce of the armed session the request was admitted to; the stream ends when
/// that session ends.
pub(super) fn serve_continuous_stream(
    mut stream: TcpStream,
    is_head: bool,
    state: &Arc<RwLock<NowPlayingState>>,
    cmd_tx: &mpsc::Sender<WebCommand>,
    stream_state: &Arc<StreamServerState>,
    session: &str,
) -> std::io::Result<()> {
    let conn_id = stream_state.register_socket(&stream);
    let _socket_guard = SocketGuard {
        stream_state: stream_state.clone(),
        conn_id,
    };

    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_nodelay(true);

    crate::log_info!(
        "web",
        "Continuous gapless stream client connected (conn_id: {:?})",
        conn_id
    );

    let headers = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Type: audio/x-wav\r\n",
        "Accept-Ranges: none\r\n",
        "transferMode.dlna.org: Streaming\r\n",
        "contentFeatures.dlna.org: DLNA.ORG_OP=00;DLNA.ORG_CI=1;DLNA.ORG_FLAGS=01500000000000000000000000000000\r\n",
        "Connection: close\r\n",
        "\r\n"
    );
    if is_head {
        stream.write_all(headers.as_bytes())?;
        stream.flush()?;
        return Ok(());
    }

    // Mark the session active before the client can read anything, the response head included,
    // so a client that has the 200 always sees `is_active` (#79); the guard clears it if a write
    // fails.
    let cid = conn_id.unwrap_or(0);
    let controller = stream_state.continuous.clone();
    controller.active_conn_id.store(cid, Ordering::SeqCst);
    controller.is_active.store(true, Ordering::SeqCst);

    struct ContinuousActiveGuard {
        controller: Arc<ContinuousStreamController>,
        conn_id: u64,
    }
    impl Drop for ContinuousActiveGuard {
        fn drop(&mut self) {
            if self.conn_id == 0 || self.controller.active_conn_id.load(Ordering::SeqCst) == self.conn_id {
                self.controller.is_active.store(false, Ordering::SeqCst);
                self.controller.kill_active_decoder();
                crate::log_info!(
                    "web",
                    "Continuous gapless stream finished or disconnected (conn_id: {})",
                    self.conn_id
                );
            } else {
                crate::log_info!(
                    "web",
                    "Continuous stream connection {} closed (superseded by conn_id {})",
                    self.conn_id,
                    self.controller.active_conn_id.load(Ordering::SeqCst)
                );
            }
        }
    }
    let _active_guard = ContinuousActiveGuard {
        controller: controller.clone(),
        conn_id: cid,
    };

    stream.write_all(headers.as_bytes())?;
    let wav_hdr = make_continuous_wav_header();
    stream.write_all(&wav_hdr)?;
    stream.flush()?;
    // The PCM written since the connection began (audio and silence), for `renderer_lead`.
    let connected = Instant::now();
    let mut written: u64 = 0;

    // Silence paced at real time while there is nothing to decode; `None` while audio plays.
    let mut silence: Option<SilencePacer> = None;
    // Set when the whole queue gave nothing to play (no file found, or every decoder run ended
    // without audio): the queue's paths then, and when. Until the queue changes, a request
    // arrives or `MISSING_RECHECK` passes, the stream sends silence without looking again.
    let mut nothing_playable: Option<(Vec<String>, Instant)> = None;
    // Tracks in a row that gave no audio (missing files passed over, decoder runs that ended
    // without output: files ffmpeg can't decode). Once the whole queue has, the stream idles as
    // when every file is missing, then tries one file a recheck. A run that gave audio, a
    // request or a changed queue starts the count again.
    let mut silent_runs: usize = 0;
    // Where the stream goes on when no request is pending: the track after one that ended, or
    // the same one after a read error. `None`: from `current_track_index`.
    let mut next_index: Option<usize> = None;
    // Whether audio went out since the stream last wrapped: a wrap without any asks for no
    // reshuffle, so a queue that gives no audio is not reshuffled on every pass.
    let mut streamed_since_wrap = false;
    // Whether a decoder that couldn't start (no ffmpeg) was logged since audio last went out.
    let mut spawn_failure_logged = false;
    // The look through the queue still waiting on a file check: answers given since its
    // `Instant` are used as the stream looks again on the next passes, until a request newer than
    // its `request_seq` comes (which starts a fresh look). Set as a track ends too: from when its
    // decoder started (when the next file's check was made) if that check found the file, else
    // from the end. `None`: no look is waiting.
    let mut look: Option<(Instant, u64)> = None;

    'pass: while controller.is_active.load(Ordering::SeqCst) {
        let pass_started = Instant::now();
        // Whether this pass's look is still waiting on a file check.
        let mut waiting_on_checks = false;
        if !controller.is_session(session) {
            crate::log_info!(
                "web",
                "Continuous stream connection {} outlived its session, closing",
                cid
            );
            return Ok(());
        }
        if !controller.owns(cid) {
            crate::log_info!(
                "web",
                "Continuous stream connection {} superseded by newer connection {}, closing older stream",
                cid,
                controller.active_conn_id.load(Ordering::SeqCst)
            );
            return Ok(());
        }

        // 1. Pick the track: the first from the requested (else the next) index whose file
        // exists, and where to start it. `None`: nothing to decode yet, so send silence.
        let picked = 'pick: {
            let (index, request, seq, tracks, tentative, queue_changed) = {
                let s = state.read().unwrap();
                // A connection a reconnect has superseded stops here (and is checked again as the
                // request is taken below), so it never takes the request meant for the new one.
                let mut pending = controller.request.lock().unwrap();
                if !controller.owns(cid) {
                    crate::log_info!("web", "Continuous stream connection {} superseded, closing", cid);
                    return Ok(());
                }
                if s.queue.is_empty() {
                    // A skip or seek asked for a queue that is gone is not for the next one,
                    // which starts at its top.
                    *pending = None;
                    *controller.current_track_index.lock().unwrap() = 0;
                    next_index = None;
                    look = None;
                    break 'pick None;
                }
                // A request that came since the look began starts a fresh one: answers given
                // during a long look are not trusted for it (a share remounted meanwhile).
                let seq = controller.request_seq.load(Ordering::SeqCst);
                if look.is_some_and(|(_, look_seq)| look_seq != seq) {
                    look = None;
                }
                let mut queue_changed = false;
                if let Some((paths, checked)) = &nothing_playable {
                    let recheck = checked.elapsed() >= MISSING_RECHECK;
                    // Another length shows at once, other files at the next recheck or request
                    // (the UI publishes a new queue before it skips): an idle tick never compares
                    // the whole queue.
                    queue_changed = s.queue.len() != paths.len()
                        || ((recheck || pending.is_some()) && !s.queue.iter().map(|t| &t.filepath).eq(paths.iter()));
                    if !queue_changed && !recheck && pending.is_none() && look.is_none() {
                        break 'pick None;
                    }
                }
                // Read here and taken once the look through the queue below has decided (the
                // look can take several passes while file checks answer), so it shows as pending
                // until then.
                let request = *pending;
                // A changed queue or a request gets every file tried again.
                if queue_changed || request.is_some() {
                    silent_runs = 0;
                }
                // While nothing gives audio, a file tried without being asked for is not shown
                // as playing until it gives some.
                let tentative = request.is_none() && (silent_runs > 0 || nothing_playable.is_some());
                let index = match request {
                    Some(r) => r.index,
                    None => next_index.unwrap_or_else(|| *controller.current_track_index.lock().unwrap()),
                };
                drop(pending);
                let index = if index < s.queue.len() { index } else { 0 };
                let tracks: Vec<(String, f64)> = s
                    .queue
                    .iter()
                    .map(|t| (t.filepath.clone(), t.duration_seconds))
                    .collect();
                (index, request, seq, tracks, tentative, queue_changed)
            };
            // The paths were copied out, so a slow share never holds the web state's lock.
            let queue_len = tracks.len();
            // Logged once per queue, not again while the stream idles on it.
            let quiet = nothing_playable.is_some() && !queue_changed;
            // Files are checked on helper threads (a share that hangs can take seconds to stat),
            // waited for `FILE_CHECK_WAIT` at most per pass so the silence stays paced. A check
            // still unanswered then leaves the look to the next pass, the request (if any) still
            // pending; the answers given meanwhile carry over. The files after a slow one are
            // checked ahead, so hung ones cost about one deadline together, not one each.
            let since = look.get_or_insert((Instant::now(), seq)).0;
            let wait_until = Instant::now() + FILE_CHECK_WAIT;
            let at = |p: usize| (index + p) % queue_len;
            let result = controller
                .file_checks
                .look(queue_len, |p| Path::new(&tracks[at(p)].0), since, wait_until);
            let found = match result.outcome {
                LookOutcome::Found(p) => Some(at(p)),
                LookOutcome::NoneFound => None,
                LookOutcome::Pending => {
                    waiting_on_checks = true;
                    break 'pick None;
                }
            };
            {
                // Taken in one step with the decision it drives, so a request that arrives later
                // stays pending for the next pass. One that replaced it during the look gets a
                // fresh look. The connection is checked under the same lock, so one a reconnect
                // has superseded never takes the new one's request.
                let mut pending = controller.request.lock().unwrap();
                if !controller.owns(cid) {
                    crate::log_info!("web", "Continuous stream connection {} superseded, closing", cid);
                    return Ok(());
                }
                if controller.request_seq.load(Ordering::SeqCst) != seq {
                    look = None;
                    continue 'pass;
                }
                *pending = None;
            }
            look = None;
            if !quiet {
                for (i, unanswered) in result.passed_over.into_iter().map(|(p, u)| (at(p), u)) {
                    if unanswered {
                        crate::log_warn!("web", "Continuous stream track not answering: {}", tracks[i].0);
                    } else {
                        crate::log_warn!("web", "Continuous stream track missing: {}", tracks[i].0);
                    }
                }
            }
            let Some(found) = found else {
                if !quiet {
                    crate::log_warn!(
                        "web",
                        "All {} queued tracks are missing; sending silence until one is found",
                        queue_len
                    );
                }
                next_index = Some(index);
                nothing_playable = Some((tracks.into_iter().map(|(path, _)| path).collect(), Instant::now()));
                break 'pick None;
            };
            // The missing files passed over gave no audio either.
            silent_runs += (found + queue_len - index) % queue_len;
            // A seek is for the track it was asked on; one played in its place starts at 0.
            let start_pos = match request {
                Some(r) if found == index => r.position,
                _ => 0.0,
            };
            let (path, duration) = &tracks[found];
            let (path, duration) = (PathBuf::from(path), *duration);
            Some((found, path, duration, start_pos, request, tentative, tracks))
        };
        let Some((track_index, track_path, track_duration, start_pos, request, tentative, tracks)) = picked else {
            // A check that is merely slow (a track's end on a slow share) writes no silence while
            // the renderer still has the audio before it to play: silence only as that runs out,
            // so the next track follows gaplessly once the check answers.
            if waiting_on_checks && renderer_lead(connected, written) > SILENCE_LEAD + SILENCE_TICK {
                wait_out_tick(pass_started);
                continue;
            }
            match silence
                .get_or_insert_with(SilencePacer::new)
                .send(&mut stream, pass_started)
            {
                Ok(n) => written += n,
                Err(e) => {
                    crate::log_info!("web", "Continuous stream socket closed while idle: {}", e);
                    return Ok(());
                }
            }
            continue;
        };
        let queue_len = tracks.len();
        next_index = None;

        // A file tried while nothing gives audio is shown once it gives some (below).
        if !tentative {
            controller.publish_track(
                state,
                cid,
                session,
                track_index,
                &track_path,
                track_duration,
                start_pos,
                queue_len,
            );
        }

        crate::log_info!(
            "web",
            "Continuous stream decoding track [{}/{}]: '{}' (seek: {:.2}s)",
            track_index + 1,
            queue_len,
            track_path.display(),
            start_pos
        );

        // 2. Spawn ffmpeg to decode to 44.1kHz 16-bit LE stereo raw PCM
        let mut cmd = std::process::Command::new(controller.decoder());
        if start_pos > 0.0 {
            cmd.arg("-ss").arg(format!("{:.3}", start_pos));
        }
        cmd.arg("-v")
            .arg("quiet")
            .arg("-i")
            .arg(&track_path)
            .arg("-f")
            .arg("s16le")
            .arg("-ac")
            .arg("2")
            .arg("-ar")
            .arg("44100")
            .arg("-")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        // Whether the decoder's output ended. A decoder that can't start ends the track at once,
        // without audio, like one that can't decode the file.
        let mut ended = false;
        let mut streamed = false;
        // When the next file's check was asked for: answers given since then are used as this
        // track ends.
        let decoder_started = Instant::now();
        let mut stdout = match cmd.spawn() {
            Ok(mut child) => match child.stdout.take() {
                Some(stdout) => {
                    // Registered before the first request check below, so a request that arrived
                    // since the top of the pass (its kill found no decoder) still stops this one.
                    if !controller.register_decoder(cid, child, request) {
                        return Ok(());
                    }
                    // The next file is checked while this one plays, so a slow share has answered
                    // by the time it ends (#117).
                    controller
                        .file_checks
                        .prefetch(Path::new(&tracks[(track_index + 1) % queue_len].0), decoder_started);
                    Some(stdout)
                }
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    continue;
                }
            },
            Err(e) => {
                // Once until audio goes out again: a missing ffmpeg fails every try.
                if !spawn_failure_logged {
                    crate::log_warn!(
                        "web",
                        "Failed to spawn {} for continuous stream: {}",
                        controller.decoder().display(),
                        e
                    );
                    spawn_failure_logged = true;
                }
                ended = true;
                None
            }
        };

        // 3. Stream PCM chunks
        let mut buf = [0u8; 32768];
        let mut bytes_streamed: u64 = (start_pos * PCM_BYTES_PER_SEC) as u64;
        let mut last_pos_update = std::time::Instant::now();

        while let Some(stdout) = stdout.as_mut() {
            if !controller.is_active.load(Ordering::SeqCst) {
                controller.kill_active_decoder();
                return Ok(());
            }

            if !controller.owns(cid) {
                controller.kill_decoder_of(cid);
                return Ok(());
            }

            // The next pass takes the request.
            if controller.has_request() {
                break;
            }

            match stdout.read(&mut buf) {
                Ok(0) => {
                    ended = true;
                    break;
                }
                Ok(n) => {
                    if !streamed {
                        streamed = true;
                        streamed_since_wrap = true;
                        spawn_failure_logged = false;
                        silence = None;
                        if tentative {
                            controller.publish_track(
                                state,
                                cid,
                                session,
                                track_index,
                                &track_path,
                                track_duration,
                                start_pos,
                                queue_len,
                            );
                        }
                    }
                    if let Err(e) = stream.write_all(&buf[..n]) {
                        crate::log_info!("web", "Continuous stream socket closed: {}", e);
                        controller.kill_decoder_of(cid);
                        return Ok(());
                    }
                    bytes_streamed += n as u64;
                    written += n as u64;

                    if last_pos_update.elapsed() >= Duration::from_millis(250) {
                        last_pos_update = std::time::Instant::now();
                        let pos = bytes_streamed as f64 / PCM_BYTES_PER_SEC;
                        *controller.current_position.lock().unwrap() = pos;
                        if let Ok(mut s) = state.write() {
                            s.position_seconds = pos;
                            controller.learn_duration(&mut s, track_index, &track_path);
                        }
                    }
                }
                Err(e) => {
                    crate::log_warn!("web", "Continuous stream read error: {}", e);
                    break;
                }
            }
        }

        controller.kill_decoder_of(cid);

        if streamed {
            silent_runs = 0;
        }
        // A skip or seek kills the decoder, which also ends its output: only an end with no
        // request pending is the end of the track. Decided under the request lock, so a request
        // that arrives later is for whatever this end decides.
        let pending = controller.request.lock().unwrap();
        if ended && pending.is_none() {
            let seq = controller.request_seq.load(Ordering::SeqCst);
            let wraps = track_index + 1 >= queue_len;
            // Reshuffle once per wrap that played something, and only from the current
            // connection of the armed session.
            let owns_session = controller.is_session(session) && controller.owns(cid);
            if wraps && queue_len > 1 && owns_session && streamed_since_wrap {
                // The UI's shuffle keeps the track `current_track_index` names in front, so it
                // names the first track before the UI is asked: never the one that just ended,
                // which would play again. A seek from here on is for that first track, since
                // after the reshuffle no index names the track that ended.
                *controller.current_track_index.lock().unwrap() = 0;
                let _ = cmd_tx.send(WebCommand::ShuffleQueue);
            }
            drop(pending);
            if wraps {
                streamed_since_wrap = false;
            }
            // Otherwise `current_track_index` moves when the next track starts, so a seek that
            // arrives before then is still for this one.
            next_index = Some((track_index + 1) % queue_len);

            if !streamed {
                silent_runs += 1;
            }
            if silent_runs >= queue_len {
                if silent_runs == queue_len {
                    crate::log_warn!(
                        "web",
                        "None of the {} queued tracks gave any audio; sending silence until the queue changes",
                        queue_len
                    );
                }
                nothing_playable = Some((tracks.into_iter().map(|(path, _)| path).collect(), Instant::now()));
            } else {
                nothing_playable = None;
                // The next look takes the answer to the check made as this track started when it
                // found the file. A missing one is asked again: the track may have played for
                // minutes, long enough for the share to come back.
                let next = Path::new(&tracks[(track_index + 1) % queue_len].0);
                let since = if controller.file_checks.exists_since(next, decoder_started) {
                    decoder_started
                } else {
                    Instant::now()
                };
                look = Some((since, seq));
            }
        } else {
            drop(pending);
            // The next pass takes the request, or after a read error tries the track again.
            next_index = Some(track_index);
            nothing_playable = None;
        }
    }

    Ok(())
}
