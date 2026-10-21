//! Byte-range file streaming (`/stream/<name>`) and the continuous WAV stream for UPnP renderers.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use super::file_checks::{FileChecks, LookOutcome};
use super::server::{write_not_found, write_server_error};
use super::state::{NowPlayingState, WebCommand};
use crate::queue::RowMap;

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
    /// Where each queued track went in reorders and removals the stream hasn't followed yet.
    /// Set by [`Self::queue_reordered`], taken by the stream wherever it holds a row of its own
    /// (lock order: the web state, then `request`, then `current_track_index`, then this).
    reorder: Mutex<Option<RowMap>>,
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
        *self.reorder.lock().unwrap() = None;
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

    /// The stream has started `index` (`path`) with `duration` (0 when unknown): set both
    /// together, so a length learned from the published queue is always the one of the track
    /// being decoded.
    fn start_track(&self, index: usize, path: &Path, duration: f64) {
        let mut current = self.current_duration.lock().unwrap();
        *current = duration;
        *self.decoding.lock().unwrap() = Some((index, path.to_path_buf()));
    }

    /// Whoever publishes the queue into `state` (the UI's `update_web_state`) offers the stream
    /// the lengths it now knows: if the track being decoded started with an unknown length
    /// (placeholder metadata, tags not read yet) and `state`'s queue has it at the same index and
    /// path, it is learned now, whatever the stream's socket is doing (a renderer reading at real
    /// time can keep the stream's writer blocked for seconds). Never blocks on the socket. The
    /// stream's write loop calls it too, after its writes. The index is the decoded track's
    /// (`decoding`), which follows a reorder at once. Returns whether it was learned now.
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

    /// The UI reordered the queue (a shuffle) or removed rows from it: `rows` says where each
    /// row went. Called by whoever publishes the new queue into the web state, with that lock
    /// held, so the stream reads the new queue and the new rows together. The track playing, a
    /// pending skip or seek and the track the stream goes on from all follow their tracks (a
    /// removed one's to the track that took its place), so the event loop's sync keeps the queue
    /// on the track playing and the stream goes on in the new order.
    pub fn queue_reordered(&self, rows: &RowMap) {
        if !self.is_active.load(Ordering::SeqCst) {
            return;
        }
        let mut request = self.request.lock().unwrap();
        if let Some(pending) = request.as_mut() {
            pending.index = rows.row(pending.index);
        }
        let mut current = self.current_track_index.lock().unwrap();
        *current = rows.row(*current);
        let mut reorder = self.reorder.lock().unwrap();
        *reorder = Some(match reorder.take() {
            Some(earlier) => earlier.then(rows),
            None => rows.clone(),
        });
        drop(reorder);
        drop(current);
        drop(request);
        let _duration = self.current_duration.lock().unwrap();
        if let Some((index, _)) = self.decoding.lock().unwrap().as_mut() {
            *index = rows.row(*index);
        }
    }

    /// Row `index` after the changes the stream hasn't followed yet (taking them), and whether
    /// its track was removed (the row is then the track's that took its place).
    fn follow_reorder(&self, index: usize) -> (usize, bool) {
        match self.reorder.lock().unwrap().take() {
            Some(rows) => (rows.row(index), rows.was_removed(index)),
            None => (index, false),
        }
    }

    /// Where the stream goes on (`next`: a row and how many rows after it), after the changes it
    /// hasn't followed yet (taking them): the row moves with its track and the step stays; from
    /// a removed track's place one step less, as the track that took it came after it.
    fn follow_next(&self, next: Option<(usize, usize)>) -> Option<(usize, usize)> {
        let rows = self.reorder.lock().unwrap().take();
        next.map(|(row, step)| rows.as_ref().map_or((row, step), |rows| rows.follow_step(row, step)))
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
    /// in `state` as now playing. Returns its index, which a reorder since it was picked moves,
    /// and whether it was removed since (the index is then the track's that took its place).
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
    ) -> (usize, bool) {
        let Ok(mut s) = state.write() else {
            return (index, false);
        };
        let (index, removed) = {
            let request = self.request.lock().unwrap();
            let followed = self.follow_reorder(index);
            if request.is_none() && self.owns(cid) && self.is_session(session) {
                *self.current_track_index.lock().unwrap() = followed.0;
            }
            followed
        };
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
        // Up Next for this track, in the same write and from the same published queue, index
        // and mode, through the function the UI's tick uses (which then republishes the same
        // values), so the track now playing is never also listed as the next one.
        s.publish_up_next(true);
        (index, removed)
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

    /// Keep an answer that came after its deadline for `keep` instead of 60 s (the test seam for
    /// a track that plays longer than that).
    pub fn set_late_answer_keep(&self, keep: Duration) {
        self.file_checks.set_late_answer_keep(keep);
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
    /// Files whose `?pos=` seek failed and was logged, with the reason, so each failure is logged
    /// once.
    seek_failures: Mutex<HashSet<(PathBuf, &'static str)>>,
}

impl Default for StreamServerState {
    fn default() -> Self {
        Self {
            active_sockets: Mutex::new(HashMap::new()),
            file_registry: RwLock::new(HashMap::new()),
            continuous: Arc::new(ContinuousStreamController::new()),
            next_conn_id: AtomicU64::new(1),
            seek_failures: Mutex::new(HashSet::new()),
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

    /// Note that a `?pos=` seek of `path` failed because of `why`: true the first time for that
    /// file and reason.
    fn first_seek_failure(&self, path: &Path, why: &'static str) -> bool {
        self.seek_failures.lock().unwrap().insert((path.to_path_buf(), why))
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

/// The body of an RF64 `ds64` chunk without its table: RIFF size, data size and sample count
/// (64 bits each), and the table's entry count.
const DS64_BODY: u32 = 28;

/// Why a `?pos=` seek of a WAV or AIFF can't start where it asks.
#[derive(Debug)]
pub enum StreamSeekError {
    /// The file couldn't be opened or read (gone, or a share that failed): nothing is known about
    /// its layout, so this says nothing about whether it can be seeked.
    Io(std::io::Error),
    /// Its header can't be used (corrupt, compressed AIFC, an RF64 without a data size): the
    /// server serves the whole file from 0:00 instead.
    Unseekable(&'static str),
}

use StreamSeekError::Unseekable;

impl From<std::io::Error> for StreamSeekError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::fmt::Display for StreamSeekError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{}", e),
            Self::Unseekable(why) => write!(f, "{}", why),
        }
    }
}

/// Fill `buf` from `file`: a file that ends first is unseekable (`why`), any other failure an I/O
/// error.
fn read_field(file: &mut std::fs::File, buf: &mut [u8], why: &'static str) -> Result<(), StreamSeekError> {
    use std::io::Read;
    match file.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(Unseekable(why)),
        Err(e) => Err(StreamSeekError::Io(e)),
    }
}

/// Read a chunk header at `pos`: `None` when the file ends first.
fn read_chunk_header(file: &mut std::fs::File, pos: u64) -> Result<Option<[u8; 8]>, StreamSeekError> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(pos))?;
    let mut chunk_hdr = [0u8; 8];
    match file.read_exact(&mut chunk_hdr) {
        Ok(()) => Ok(Some(chunk_hdr)),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(StreamSeekError::Io(e)),
    }
}

/// The chunks of a WAV file that a seek needs.
struct WavLayout {
    /// An RF64 / BW64, whose sizes may need 64 bits.
    rf64: bool,
    /// The `fmt ` chunk's body, as it is in the file.
    fmt: Vec<u8>,
    /// Where the `data` chunk's audio starts and ends in the file (clamped to the file).
    data_start: u64,
    data_end: u64,
    /// A RIFF's `data` size was 0 or `0xFFFFFFFF`: the audio runs to the end of the file. (An
    /// RF64 without a `ds64` size for it is unseekable: its trailing chunks would be sent as audio.)
    size_unknown: bool,
}

/// Walk the chunks of a WAV file of `file_len` bytes, with every size bounded by the file: a
/// RIFF/WAVE, or an RF64 / BW64 (EBU Tech 3306 / ITU-R BS.2088), whose `ds64` chunk holds the
/// real data size behind a 32-bit `0xFFFFFFFF`.
fn read_wav_layout(file: &mut std::fs::File, file_len: u64) -> Result<WavLayout, StreamSeekError> {
    use std::io::{Seek, SeekFrom};
    let mut riff_hdr = [0u8; 12];
    file.seek(SeekFrom::Start(0))?;
    read_field(file, &mut riff_hdr, "not a RIFF/WAVE file")?;
    if &riff_hdr[8..12] != b"WAVE" {
        return Err(Unseekable("not a RIFF/WAVE file"));
    }
    let rf64 = match &riff_hdr[0..4] {
        b"RIFF" => false,
        b"RF64" | b"BW64" => true,
        _ => return Err(Unseekable("not a RIFF/WAVE file")),
    };
    let mut fmt: Option<Vec<u8>> = None;
    let mut data: Option<(u64, u64, bool)> = None;
    // The `ds64` chunk's data size: 0 or all ones (unknown) are left out.
    let mut ds64_data: Option<u64> = None;
    let mut pos = 12u64;
    while pos.saturating_add(8) <= file_len {
        let Some(chunk_hdr) = read_chunk_header(file, pos)? else {
            break;
        };
        let size = u32::from_le_bytes(chunk_hdr[4..8].try_into().unwrap());
        let body = pos + 8;
        let available = file_len - body;
        // The chunk's length: a `data` chunk's 32-bit `0xFFFFFFFF` defers to `ds64`.
        let mut len = u64::from(size);
        match &chunk_hdr[0..4] {
            b"ds64" if rf64 && ds64_data.is_none() => {
                if size < DS64_BODY || u64::from(size) > available {
                    return Err(Unseekable("ds64 chunk shorter than 28 bytes or larger than the file"));
                }
                let mut bytes = [0u8; 16];
                read_field(file, &mut bytes, "can't read the ds64 chunk")?;
                let data_size = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
                ds64_data = Some(data_size).filter(|&s| s != 0 && s != u64::MAX);
            }
            b"fmt " if fmt.is_none() => {
                if size < 16 {
                    return Err(Unseekable("fmt chunk shorter than 16 bytes"));
                }
                if u64::from(size) > MAX_WAV_FMT_CHUNK.min(available) {
                    return Err(Unseekable("fmt chunk larger than 64 KiB or the file"));
                }
                let mut bytes = vec![0u8; size as usize];
                read_field(file, &mut bytes, "can't read the fmt chunk")?;
                fmt = Some(bytes);
            }
            b"data" if data.is_none() => {
                let real_size = match ds64_data {
                    Some(ds64) if size == WAV_UNKNOWN_SIZE => Some(ds64),
                    _ if size == 0 || size == WAV_UNKNOWN_SIZE => None,
                    _ => Some(u64::from(size)),
                };
                if rf64 && real_size.is_none() {
                    return Err(Unseekable("RF64 without a ds64 data size"));
                }
                let size_unknown = real_size.is_none();
                len = real_size.unwrap_or(available);
                data = Some((body, body + len.min(available), size_unknown));
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
        pos = body.saturating_add(len).saturating_add(len & 1);
    }
    let fmt = fmt.ok_or(Unseekable("no fmt chunk before the audio"))?;
    let (data_start, data_end, size_unknown) = data.ok_or(Unseekable("no data chunk"))?;
    Ok(WavLayout {
        rf64,
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

/// A WAV's layout and the numbers its seek goes by, checked.
struct WavSeek {
    layout: WavLayout,
    byte_rate: u64,
    block_align: u64,
}

/// Read and check what a seek in a WAV file of `file_len` bytes needs, without building a header.
fn wav_seek(file: &mut std::fs::File, file_len: u64) -> Result<WavSeek, StreamSeekError> {
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
        return Err(Unseekable("zero byte rate or block align"));
    }
    Ok(WavSeek {
        layout,
        byte_rate,
        block_align,
    })
}

/// Seek a WAV file of `file_len` bytes to `offset_seconds`: the audio from the block at or before
/// that time to the last whole block of the `data` chunk, behind a new RIFF header with the file's
/// own `fmt ` chunk (so WAVE_FORMAT_EXTENSIBLE and 24-bit files stay decodable) and sizes that
/// match it. An RF64 source whose known audio is too long for 32-bit sizes gets an RF64 header
/// with its sizes in a `ds64` chunk instead; a plain RIFF keeps a RIFF header (both sizes
/// `0xFFFFFFFF` when they don't fit, issue #112), which every renderer reads.
fn find_wav_pcm_offset(
    file: &mut std::fs::File,
    offset_seconds: f64,
    file_len: u64,
) -> Result<StreamOffset, StreamSeekError> {
    let WavSeek {
        layout,
        byte_rate,
        block_align,
    } = wav_seek(file, file_len)?;
    let fmt = &layout.fmt;

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
    let rf64 = layout.rf64 && data_size == WAV_UNKNOWN_SIZE;

    let mut hdr = Vec::with_capacity(28 + 8 + DS64_BODY as usize + fmt.len() + 1);
    if rf64 {
        let riff64 = 4 + 8 + u64::from(DS64_BODY) + 8 + fmt_len + fmt_pad + 8 + remaining;
        hdr.extend_from_slice(b"RF64");
        hdr.extend_from_slice(&WAV_UNKNOWN_SIZE.to_le_bytes());
        hdr.extend_from_slice(b"WAVEds64");
        hdr.extend_from_slice(&DS64_BODY.to_le_bytes());
        hdr.extend_from_slice(&riff64.to_le_bytes());
        hdr.extend_from_slice(&remaining.to_le_bytes());
        hdr.extend_from_slice(&(remaining / block_align).to_le_bytes());
        // No table: no other chunk is over 4 GiB.
        hdr.extend_from_slice(&0u32.to_le_bytes());
    } else {
        hdr.extend_from_slice(b"RIFF");
        hdr.extend_from_slice(&riff_size.to_le_bytes());
        hdr.extend_from_slice(b"WAVE");
    }
    hdr.extend_from_slice(b"fmt ");
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
        trailer: None,
    })
}

/// Largest `COMM` chunk an AIFF seek reads and copies; an AIFC's holds a compression name of at
/// most 255 bytes.
const MAX_AIFF_COMM_CHUNK: u64 = 64 * 1024;

/// The `FVER` chunk an AIFC must have: its one version, AIFC Version 1 (0xA2805140).
const AIFC_FVER: [u8; 12] = [b'F', b'V', b'E', b'R', 0, 0, 0, 4, 0xA2, 0x80, 0x51, 0x40];

/// The chunks of an AIFF or AIFC file that a seek needs.
struct AiffLayout {
    /// An AIFC (`FORM` type `AIFC`), whose `COMM` chunk names a compression type.
    aifc: bool,
    /// The `COMM` chunk's body, as it is in the file.
    comm: Vec<u8>,
    /// Where the `SSND` chunk's sound data starts (after its offset field's filler) and ends.
    data_start: u64,
    data_end: u64,
}

/// Walk the big-endian chunks of an AIFF or AIFC file of `file_len` bytes, with every size
/// bounded by the file.
fn read_aiff_layout(file: &mut std::fs::File, file_len: u64) -> Result<AiffLayout, StreamSeekError> {
    use std::io::{Seek, SeekFrom};
    let mut form_hdr = [0u8; 12];
    file.seek(SeekFrom::Start(0))?;
    read_field(file, &mut form_hdr, "not an AIFF file")?;
    if &form_hdr[0..4] != b"FORM" {
        return Err(Unseekable("not an AIFF file"));
    }
    let aifc = match &form_hdr[8..12] {
        b"AIFF" => false,
        b"AIFC" => true,
        _ => return Err(Unseekable("not an AIFF file")),
    };
    let mut comm: Option<Vec<u8>> = None;
    let mut data: Option<(u64, u64)> = None;
    let mut pos = 12u64;
    while pos.saturating_add(8) <= file_len {
        let Some(chunk_hdr) = read_chunk_header(file, pos)? else {
            break;
        };
        let size = u64::from(u32::from_be_bytes(chunk_hdr[4..8].try_into().unwrap()));
        let body = pos + 8;
        let available = file_len - body;
        match &chunk_hdr[0..4] {
            b"COMM" if comm.is_none() => {
                if size < 18 {
                    return Err(Unseekable("COMM chunk shorter than 18 bytes"));
                }
                if size > MAX_AIFF_COMM_CHUNK.min(available) {
                    return Err(Unseekable("COMM chunk larger than 64 KiB or the file"));
                }
                let mut bytes = vec![0u8; size as usize];
                read_field(file, &mut bytes, "can't read the COMM chunk")?;
                comm = Some(bytes);
            }
            b"SSND" if data.is_none() => {
                // A truncated file's SSND chunk ends with the file.
                let len = size.min(available);
                let mut fields = [0u8; 8];
                if len < 8 {
                    return Err(Unseekable("SSND chunk shorter than 8 bytes"));
                }
                read_field(file, &mut fields, "SSND chunk shorter than 8 bytes")?;
                let offset = u64::from(u32::from_be_bytes(fields[0..4].try_into().unwrap()));
                if offset > len - 8 {
                    return Err(Unseekable("SSND offset past the chunk's end"));
                }
                data = Some((body + 8 + offset, body + len));
            }
            _ => {}
        }
        if comm.is_some() && data.is_some() {
            break;
        }
        // Chunks are padded to an even length.
        pos = body + size + (size & 1);
    }
    let comm = comm.ok_or(Unseekable("no COMM chunk"))?;
    let (data_start, data_end) = data.ok_or(Unseekable("no SSND chunk"))?;
    Ok(AiffLayout {
        aifc,
        comm,
        data_start,
        data_end,
    })
}

/// An 80-bit IEEE 754 extended float, as `COMM` stores the sample rate.
fn extended_to_f64(bytes: &[u8]) -> f64 {
    let sign_exp = u16::from_be_bytes([bytes[0], bytes[1]]);
    let exp = i32::from(sign_exp & 0x7FFF);
    let mantissa = u64::from_be_bytes(bytes[2..10].try_into().unwrap());
    if exp == 0x7FFF {
        return f64::NAN;
    }
    // The mantissa's integer bit is explicit: the value is mantissa × 2^(exp − bias − 63).
    let value = mantissa as f64 * 2f64.powi(exp - 16383 - 63);
    if sign_exp & 0x8000 != 0 {
        -value
    } else {
        value
    }
}

/// The sample rates an AIFF seek takes: a `COMM` rate outside them is corrupt, and seeking by it
/// would land nowhere near the time asked for.
const AIFF_RATES: std::ops::RangeInclusive<f64> = 1.0..=10_000_000.0;

/// An AIFF's layout and the numbers its seek goes by, checked.
struct AiffSeek {
    layout: AiffLayout,
    /// Bytes per sample frame.
    frame: u64,
    sample_rate: f64,
}

/// Read and check what a seek in an AIFF or AIFC file of `file_len` bytes needs. Compressed AIFC
/// types, which have no fixed frame size, are refused.
fn aiff_seek(file: &mut std::fs::File, file_len: u64) -> Result<AiffSeek, StreamSeekError> {
    let layout = read_aiff_layout(file, file_len)?;
    let comm = &layout.comm;
    let channels = u64::from(u16::from_be_bytes([comm[0], comm[1]]));
    let sample_bits = u64::from(u16::from_be_bytes([comm[6], comm[7]]));
    let sample_rate = extended_to_f64(&comm[8..18]);
    let bytes_per_sample = if layout.aifc {
        if comm.len() < 22 {
            return Err(Unseekable("AIFC COMM chunk without a compression type"));
        }
        match &comm[18..22] {
            b"NONE" | b"twos" | b"sowt" | b"raw " | b"in24" | b"42ni" | b"in32" | b"23ni" => sample_bits.div_ceil(8),
            b"fl32" | b"FL32" => 4,
            b"fl64" | b"FL64" => 8,
            b"ulaw" | b"ULAW" | b"alaw" | b"ALAW" => 1,
            _ => return Err(Unseekable("compressed AIFC audio")),
        }
    } else {
        sample_bits.div_ceil(8)
    };
    let frame = channels * bytes_per_sample;
    if frame == 0 {
        return Err(Unseekable("zero channels or sample size"));
    }
    if !AIFF_RATES.contains(&sample_rate) {
        return Err(Unseekable("sample rate zero, invalid or outside 1 Hz to 10 MHz"));
    }
    Ok(AiffSeek {
        layout,
        frame,
        sample_rate,
    })
}

/// What an AIFF seek to a time sends: the file's bytes `start..end` and the sizes its header
/// gives them.
struct AiffSpan {
    start: u64,
    end: u64,
    form_size: u32,
    ssnd_size: u32,
    frames: u32,
}

/// The sound data from the frame at or before `offset_seconds` to the last whole frame, and its
/// header's sizes, which must fit in 32 bits.
fn aiff_span(seek: &AiffSeek, offset_seconds: f64) -> Result<AiffSpan, StreamSeekError> {
    let AiffSeek {
        layout,
        frame,
        sample_rate,
    } = seek;
    let frame = *frame;
    // Whole frames only, at both ends, as for a WAV.
    let data_len = (layout.data_end - layout.data_start) / frame * frame;
    // The casts saturate, so a `pos` past the end lands at the end of the last whole frame.
    let wanted_frames = (offset_seconds * sample_rate) as u64;
    let skip = wanted_frames.saturating_mul(frame).min(data_len);
    let start = layout.data_start + skip;
    let end = layout.data_start + data_len;
    let remaining = end - start;

    let comm_len = layout.comm.len() as u64;
    let fver_len = if layout.aifc { AIFC_FVER.len() as u64 } else { 0 };
    // The SSND chunk: its offset and block size fields, then the audio and a pad byte when odd.
    let ssnd_size = 8 + remaining;
    let form_size = 4 + fver_len + 8 + comm_len + (comm_len & 1) + 8 + ssnd_size + (remaining & 1);
    let (Ok(form_size), Ok(ssnd_size), Ok(frames)) = (
        u32::try_from(form_size),
        u32::try_from(ssnd_size),
        u32::try_from(remaining / frame),
    ) else {
        return Err(Unseekable("audio too long for an AIFF header"));
    };
    Ok(AiffSpan {
        start,
        end,
        form_size,
        ssnd_size,
        frames,
    })
}

/// Seek an AIFF or AIFC file of `file_len` bytes to `offset_seconds`: the sound data from the
/// frame at or before that time to the last whole frame, behind a new `FORM` header with the
/// file's own `COMM` chunk (its frame count set to what is sent) and an `SSND` chunk with offset
/// and block size 0.
fn find_aiff_offset(
    file: &mut std::fs::File,
    offset_seconds: f64,
    file_len: u64,
) -> Result<StreamOffset, StreamSeekError> {
    let seek = aiff_seek(file, file_len)?;
    let AiffSpan {
        start,
        end,
        form_size,
        ssnd_size,
        frames,
    } = aiff_span(&seek, offset_seconds)?;
    let layout = &seek.layout;
    let comm = &layout.comm;
    let comm_len = comm.len() as u64;
    let pad = (end - start) & 1;

    let mut hdr = Vec::with_capacity(12 + AIFC_FVER.len() + 8 + comm.len() + 1 + 16);
    hdr.extend_from_slice(b"FORM");
    hdr.extend_from_slice(&form_size.to_be_bytes());
    hdr.extend_from_slice(if layout.aifc { b"AIFC" } else { b"AIFF" });
    if layout.aifc {
        hdr.extend_from_slice(&AIFC_FVER);
    }
    hdr.extend_from_slice(b"COMM");
    hdr.extend_from_slice(&(comm_len as u32).to_be_bytes());
    let comm_at = hdr.len();
    hdr.extend_from_slice(comm);
    hdr[comm_at + 2..comm_at + 6].copy_from_slice(&frames.to_be_bytes());
    if comm_len & 1 == 1 {
        hdr.push(0);
    }
    hdr.extend_from_slice(b"SSND");
    hdr.extend_from_slice(&ssnd_size.to_be_bytes());
    hdr.extend_from_slice(&[0u8; 8]);
    Ok(StreamOffset {
        start,
        end,
        header: Some(hdr),
        trailer: (pad == 1).then(|| vec![0]),
    })
}

/// What a `/stream/<name>?pos=` response sends: `header`, the file's bytes `start..end`, then
/// `trailer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StreamOffset {
    /// The first file byte sent.
    pub(super) start: u64,
    /// One past the last file byte sent: the file's length, or the end of a WAV's or AIFF's last
    /// whole frame.
    pub(super) end: u64,
    /// A header sent before the file's bytes (a seeked WAV's or AIFF's new header).
    pub(super) header: Option<Vec<u8>>,
    /// Bytes sent after the file's (the pad byte of a seeked AIFF's odd `SSND` chunk).
    pub(super) trailer: Option<Vec<u8>>,
}

/// `path`'s extension in lowercase, which picks how a seek finds its offset.
fn lowercase_extension(path: &Path) -> String {
    path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase()
}

/// A container whose seek sends a new header before the audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeekContainer {
    Wav,
    Aiff,
}

impl SeekContainer {
    /// The container a file with the lowercase extension `ext` is.
    fn of(ext: &str) -> Option<Self> {
        match ext {
            "wav" | "wave" => Some(Self::Wav),
            "aif" | "aiff" | "aifc" => Some(Self::Aiff),
            _ => None,
        }
    }

    fn seek(
        self,
        file: &mut std::fs::File,
        offset_seconds: f64,
        file_len: u64,
    ) -> Result<StreamOffset, StreamSeekError> {
        match self {
            Self::Wav => find_wav_pcm_offset(file, offset_seconds, file_len),
            Self::Aiff => find_aiff_offset(file, offset_seconds, file_len),
        }
    }

    /// Whether [`seek`](Self::seek) would work, read and checked the same way but without
    /// building the header.
    fn check(self, file: &mut std::fs::File, offset_seconds: f64, file_len: u64) -> Result<(), StreamSeekError> {
        match self {
            Self::Wav => wav_seek(file, file_len).map(|_| ()),
            Self::Aiff => aiff_seek(file, file_len).and_then(|seek| aiff_span(&seek, offset_seconds).map(|_| ())),
        }
    }
}

/// Whether a `/stream/<name>?pos=<offset_seconds>` request for `path` starts where it asks.
/// `Unseekable` when `path` is a WAV or AIFF whose header the seek can't use (corrupt, compressed
/// AIFC, an RF64 without a data size), which the server answers with the whole file from 0:00, so
/// the player can say so instead of showing a position the audio isn't at (issue #116). `Io` when
/// the file couldn't be opened or read, which says nothing about its header. Reads the file's
/// header: call it off the UI thread.
pub fn check_stream_seek(path: &Path, offset_seconds: f64) -> Result<(), StreamSeekError> {
    if offset_seconds <= 0.0 {
        return Ok(());
    }
    let Some(container) = SeekContainer::of(&lowercase_extension(path)) else {
        return Ok(());
    };
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    container.check(&mut file, offset_seconds, len)
}

/// Where a `?pos=` response for `filepath` starts. A WAV or AIFF the seek can't use is served
/// whole from 0:00 (logged once per file and reason); an I/O error reading its header is returned,
/// so the response fails instead of playing the file from a time the player didn't ask for.
pub(super) fn calculate_stream_offset(
    filepath: &std::path::Path,
    file: &mut std::fs::File,
    offset_seconds: f64,
    total_size: u64,
    duration_hint: f64,
    stream_state: &StreamServerState,
) -> std::io::Result<StreamOffset> {
    let from_start = StreamOffset {
        start: 0,
        end: total_size,
        header: None,
        trailer: None,
    };
    if offset_seconds <= 0.0 || total_size == 0 {
        return Ok(from_start);
    }
    let ext = lowercase_extension(filepath);

    if let Some(container) = SeekContainer::of(&ext) {
        return match container.seek(file, offset_seconds, total_size) {
            Ok(offset) => Ok(offset),
            Err(StreamSeekError::Io(e)) => Err(e),
            Err(Unseekable(why)) => {
                // UPnPPlayer checks first (`check_stream_seek`) and asks for the start instead;
                // a renderer still retries a URL with HEAD and Range requests, so say it once
                // per file and reason.
                if stream_state.first_seek_failure(filepath, why) {
                    crate::log_warn!(
                        "web",
                        "Can't seek '{}' to {:.2}s ({}); serving it from the start",
                        filepath.display(),
                        offset_seconds,
                        why
                    );
                }
                Ok(from_start)
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
    Ok(StreamOffset {
        start,
        end: total_size,
        header: None,
        trailer: None,
    })
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
    //    A seeked WAV or AIFF sends a new header, then only its audio from the seek point (and an
    //    AIFF's pad byte).
    let offset = calculate_stream_offset(
        &filepath,
        &mut file,
        offset_seconds,
        total_size,
        current_duration,
        stream_state,
    );
    let StreamOffset {
        start: base_offset,
        end: body_end,
        header: synthesized_hdr,
        trailer,
    } = match offset {
        Ok(offset) => offset,
        Err(e) => {
            crate::log_warn!(
                "web",
                "Stream 500: Couldn't read '{}' to seek it to {:.2}s: {}",
                filepath.display(),
                offset_seconds,
                e
            );
            return write_server_error(&mut stream, route, head_only);
        }
    };

    let header = synthesized_hdr.unwrap_or_default();
    let trailer = trailer.unwrap_or_default();
    let header_len = header.len() as u64;
    let file_part = body_end.saturating_sub(base_offset);
    let effective_total = header_len + file_part + trailer.len() as u64;

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

    // 5. Stream the body: the requested part of the header, the file's bytes and the trailer.
    use std::io::Seek;
    let mut streamed_count = 0u64;
    let range_end = req_start + content_length;
    // The requested part of `bytes`, which sit at `at` in the body.
    let part = |bytes: &[u8], at: u64| -> std::ops::Range<usize> {
        let end = at + bytes.len() as u64;
        let (from, to) = (req_start.clamp(at, end), range_end.clamp(at, end));
        (from - at) as usize..(to - at) as usize
    };

    let header_part = &header[part(&header, 0)];
    if !header_part.is_empty() {
        if stream.write_all(header_part).is_err() {
            crate::log_info!(
                "web",
                "Client {} disconnected during synthesized header write after {} bytes",
                client_ip,
                streamed_count
            );
            return Ok(());
        }
        streamed_count += header_part.len() as u64;
    }

    let (file_from, file_to) = (
        req_start.clamp(header_len, header_len + file_part),
        range_end.clamp(header_len, header_len + file_part),
    );
    if file_from < file_to {
        file.seek(std::io::SeekFrom::Start(base_offset + (file_from - header_len)))?;
        let mut buf = [0u8; 65536];
        let mut remaining = file_to - file_from;
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

    // Only after everything before it: a short read must leave the body short, not shifted.
    let trailer_at = header_len + file_part;
    let trailer_part = &trailer[part(&trailer, trailer_at)];
    if !trailer_part.is_empty() && streamed_count == range_end.min(trailer_at).saturating_sub(req_start) {
        if stream.write_all(trailer_part).is_err() {
            return Ok(());
        }
        streamed_count += trailer_part.len() as u64;
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
/// How long a decoder is left to give PCM, as it starts or after it has given some, before
/// silence covers for it (#133): a decoder that is merely starting adds no silence between two
/// tracks, and one that gives PCM at about real time adds none between its chunks.
const DECODER_GRACE: Duration = Duration::from_millis(500);
/// How many chunks of PCM the decoder's reader thread holds for the stream.
const DECODER_CHUNKS_AHEAD: usize = 2;
/// The most PCM one read from the decoder takes.
const DECODER_CHUNK: usize = 32768;

/// The PCM bytes in `duration`, whole 4-byte frames.
fn pcm_bytes(duration: Duration) -> u64 {
    (duration.as_secs_f64() * PCM_BYTES_PER_SEC) as u64 / 4 * 4
}

/// How long `bytes` of PCM play.
fn pcm_duration(bytes: u64) -> Duration {
    Duration::from_secs_f64(bytes as f64 / PCM_BYTES_PER_SEC)
}

/// What the renderer still holds of the PCM sent to it (audio and silence): how far the stream
/// runs ahead of a renderer playing at real time since the connection began (#117, #133), with
/// two corrections, so the estimate doesn't fall behind what the renderer really holds.
///
/// It can't play what it wasn't sent: when it runs dry, it plays on from there, so a stall leaves
/// no debt the stream's later writes must make up.
///
/// And a write that blocks longer than its audio takes to play (by more than a tick) means the
/// renderer stopped reading for the rest of that time (a pause), during which it played nothing
/// either. A renderer that reads in bursts blocks a write in the same way while it plays from its
/// own buffer, then takes the next few at once: such a pause is taken back by the writes that go
/// at least twice as fast as their audio right after it, with no wait between them, so that
/// renderer is not credited with audio it never held. The first write after it that goes slower,
/// or comes after a wait (the decoder gives its audio at real time or stalls), ends that, and the
/// pause stays counted.
struct RendererLead {
    /// When a renderer playing at real time would have started the PCM sent: the connection's
    /// start, moved on by what it didn't play.
    reference: Instant,
    /// The PCM bytes sent since the connection began.
    written: u64,
    /// How much of the last pause the writes right after it may still take back.
    takeback: Duration,
    /// When the last write ended.
    last_write: Instant,
}

impl RendererLead {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            reference: now,
            written: 0,
            takeback: Duration::ZERO,
            last_write: now,
        }
    }

    /// What the renderer still holds.
    fn lead(&mut self) -> Duration {
        let sent = pcm_duration(self.written);
        let played = self.reference.elapsed();
        if played > sent {
            self.reference += played - sent;
            Duration::ZERO
        } else {
            sent - played
        }
    }

    /// Whether the renderer is about to run out of what it was sent, so the stream must send
    /// silence rather than wait.
    fn running_dry(&mut self) -> bool {
        self.lead() <= SILENCE_LEAD + SILENCE_TICK
    }

    /// Write `pcm` to the renderer, counting it and how long the write blocked.
    fn write(&mut self, stream: &mut impl Write, pcm: &[u8]) -> std::io::Result<()> {
        // A renderer that ran dry before this write played nothing more meanwhile.
        self.lead();
        let started = Instant::now();
        if started.duration_since(self.last_write) > SILENCE_TICK {
            self.takeback = Duration::ZERO;
        }
        let result = stream.write_all(pcm);
        self.last_write = Instant::now();
        result?;
        let took = self.last_write.duration_since(started);
        let audio = pcm_duration(pcm.len() as u64);
        self.written += pcm.len() as u64;
        if took > audio + SILENCE_TICK {
            let stopped = took - audio;
            self.reference += stopped;
            self.takeback += stopped;
        } else if took * 2 < audio {
            // `reference` was moved on by at least `takeback`, so this never goes before the
            // connection's start.
            let back = (audio - took).min(self.takeback);
            self.takeback -= back;
            self.reference -= back;
        } else {
            self.takeback = Duration::ZERO;
        }
        Ok(())
    }
}

/// Sends silence at the stream's real-time byte rate while there is nothing to decode (empty
/// queue, every file missing, a file check not answered yet, a decoder that gives nothing).
/// Measured from when it started, so sleep overshoot doesn't add up.
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

    /// Write the silence due by now (plus `SILENCE_LEAD`), counted in `lead`, then wait until one
    /// tick after `pass_started`, so time the pass spent waiting on file checks or the decoder
    /// doesn't add to the gap between writes. A write error means the renderer has gone.
    fn send(&mut self, stream: &mut impl Write, lead: &mut RendererLead, pass_started: Instant) -> std::io::Result<()> {
        let due = pcm_bytes(self.started.elapsed() + SILENCE_LEAD);
        let mut owed = due.saturating_sub(self.sent).min(pcm_bytes(SILENCE_MAX_BURST));
        self.sent = due;
        let chunk = [0u8; 8820];
        while owed > 0 {
            let n = owed.min(chunk.len() as u64) as usize;
            lead.write(stream, &chunk[..n])?;
            owed -= n as u64;
        }
        wait_out_tick(pass_started);
        Ok(())
    }
}

/// Send the silence due now through `silence`, started if it wasn't running. Returns false, and
/// logs it with `doing`, when the write failed: the renderer has gone.
fn send_silence(
    silence: &mut Option<SilencePacer>,
    stream: &mut impl Write,
    lead: &mut RendererLead,
    pass_started: Instant,
    doing: &str,
) -> bool {
    match silence
        .get_or_insert_with(SilencePacer::new)
        .send(stream, lead, pass_started)
    {
        Ok(()) => true,
        Err(e) => {
            crate::log_info!("web", "Continuous stream socket closed {}: {}", doing, e);
            false
        }
    }
}

/// Sleep until one `SILENCE_TICK` after `pass_started`.
fn wait_out_tick(pass_started: Instant) {
    std::thread::sleep((pass_started + SILENCE_TICK).saturating_duration_since(Instant::now()));
}

/// What the decoder's reader thread hands the continuous stream.
enum DecoderOutput {
    Pcm(Vec<u8>),
    /// The decoder's output ended: it exited, or was killed.
    End,
    Failed(std::io::Error),
}

/// Read the decoder's `stdout` on a helper thread, so the stream never blocks on a decoder stuck
/// opening or reading a file on a share that hangs and can send silence meanwhile (#133). The
/// thread ends with the decoder's output (it exits or is killed) or once the receiver is dropped.
/// It is not joined: the stream kills the decoder as it leaves it, which ends the thread's read,
/// but a decoder stuck in a read on a hung share may only die once that read returns, and the
/// stream must not wait for it.
fn spawn_decoder_reader(mut stdout: impl Read + Send + 'static) -> std::io::Result<mpsc::Receiver<DecoderOutput>> {
    let (tx, rx) = mpsc::sync_channel(DECODER_CHUNKS_AHEAD);
    std::thread::Builder::new()
        .name("continuous-decoder-read".into())
        .spawn(move || {
            loop {
                // Read into a fresh buffer that is then handed over as it is, not copied.
                let mut buf = vec![0u8; DECODER_CHUNK];
                let output = match stdout.read(&mut buf) {
                    Ok(0) => DecoderOutput::End,
                    Ok(n) => {
                        buf.truncate(n);
                        DecoderOutput::Pcm(buf)
                    }
                    // A read a signal interrupted is tried again.
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => DecoderOutput::Failed(e),
                };
                let last = !matches!(output, DecoderOutput::Pcm(_));
                if tx.send(output).is_err() || last {
                    return;
                }
            }
        })?;
    Ok(rx)
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
    // What the renderer still holds of the PCM sent since the connection began.
    let mut lead = RendererLead::new();

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
    // Where the stream goes on when no request is pending, as a row and how many rows after it:
    // the track after one that ended, the top of the queue after a wrap, or the same one after
    // a read error. The row follows a reorder in the UI (a shuffle), so the stream goes on in
    // the new order. `None`: from `current_track_index`.
    let mut next_index: Option<(usize, usize)> = None;
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
                    controller.follow_next(None);
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
                // Followed whatever is asked: a reorder's rows are the queue's read here.
                // Kept as followed, so a look still waiting on checks goes on from it next pass.
                next_index = controller.follow_next(next_index);
                let next = next_index.map(|(row, step)| row + step);
                let index = match request {
                    Some(r) => r.index,
                    None => next.unwrap_or_else(|| *controller.current_track_index.lock().unwrap()),
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
                next_index = Some((index, 0));
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
        let Some((mut track_index, track_path, track_duration, start_pos, request, tentative, tracks)) = picked else {
            // A check that is merely slow (a track's end on a slow share) writes no silence while
            // the renderer still has the audio before it to play: silence only as that runs out,
            // so the next track follows gaplessly once the check answers.
            if waiting_on_checks && !lead.running_dry() {
                wait_out_tick(pass_started);
                continue;
            }
            if !send_silence(&mut silence, &mut stream, &mut lead, pass_started, "while idle") {
                return Ok(());
            }
            continue;
        };
        let queue_len = tracks.len();
        next_index = None;
        // Whether the UI removed the track since it was picked (`track_index` then names the
        // track that took its place).
        let mut track_removed = false;

        // A file tried while nothing gives audio is shown once it gives some (below).
        if !tentative {
            (track_index, track_removed) =
                controller.publish_track(state, cid, session, track_index, &track_path, track_duration, start_pos);
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
        let stdout = match cmd.spawn() {
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

        // 3. Stream PCM chunks, read from the decoder on a helper thread so the stream goes on
        // sending silence while the decoder gives nothing (#133).
        let output = match stdout.map(spawn_decoder_reader) {
            Some(Ok(output)) => Some(output),
            Some(Err(e)) => {
                crate::log_warn!("web", "Couldn't start reading the continuous stream's decoder: {}", e);
                ended = true;
                None
            }
            None => None,
        };
        let mut bytes_streamed: u64 = (start_pos * PCM_BYTES_PER_SEC) as u64;
        let mut last_pos_update = std::time::Instant::now();
        // When the decoder last gave PCM (or started): silence covers for it only once it has
        // given none for `DECODER_GRACE`.
        let mut last_output = decoder_started;
        // Whether silence has covered for this decoder run (or ran on into it): from then on it
        // covers as soon as the renderer runs out of what it was sent, without a fresh grace
        // after each chunk, so a decoder that gives a lone chunk now and then never lets the
        // renderer run dry for the rest of a grace after each.
        let mut covered = silence.is_some();

        while let Some(output) = output.as_ref() {
            if !controller.is_active.load(Ordering::SeqCst) {
                controller.kill_active_decoder();
                return Ok(());
            }

            if !controller.is_session(session) {
                crate::log_info!(
                    "web",
                    "Continuous stream connection {} outlived its session, closing",
                    cid
                );
                controller.kill_decoder_of(cid);
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

            let waited_from = Instant::now();
            match output.recv_timeout(SILENCE_TICK) {
                Ok(DecoderOutput::End) => {
                    ended = true;
                    break;
                }
                Ok(DecoderOutput::Pcm(chunk)) => {
                    let n = chunk.len();
                    if !streamed {
                        streamed = true;
                        streamed_since_wrap = true;
                        spawn_failure_logged = false;
                        if tentative {
                            let removed;
                            (track_index, removed) = controller.publish_track(
                                state,
                                cid,
                                session,
                                track_index,
                                &track_path,
                                track_duration,
                                start_pos,
                            );
                            track_removed |= removed;
                        }
                    }
                    // Audio again: silence covering for the decoder stops.
                    silence = None;
                    if let Err(e) = lead.write(&mut stream, &chunk) {
                        crate::log_info!("web", "Continuous stream socket closed: {}", e);
                        controller.kill_decoder_of(cid);
                        return Ok(());
                    }
                    last_output = Instant::now();
                    bytes_streamed += n as u64;

                    if last_pos_update.elapsed() >= Duration::from_millis(250) {
                        last_pos_update = std::time::Instant::now();
                        let pos = bytes_streamed as f64 / PCM_BYTES_PER_SEC;
                        *controller.current_position.lock().unwrap() = pos;
                        if let Ok(mut s) = state.write() {
                            s.position_seconds = pos;
                            // The decoded track's row, which a shuffle moves at once; this
                            // loop's `track_index` follows it only when the track ends.
                            controller.learn_published_duration(&mut s);
                        }
                    }
                }
                Ok(DecoderOutput::Failed(e)) => {
                    crate::log_warn!("web", "Continuous stream read error: {}", e);
                    break;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    crate::log_warn!("web", "Continuous stream decoder reader stopped");
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // No PCM: the decoder is starting, or stuck opening or reading the file on a
                    // share that hangs, before its first PCM or in the middle of the track
                    // (#133). Once it has given none for `DECODER_GRACE` (or silence has covered
                    // for it before) and the renderer is running out of what it was sent,
                    // silence at real time keeps it fed (and goes on, paced, from silence the
                    // stream was already sending) until the decoder gives PCM again.
                    let covers = silence.is_some()
                        || ((covered || last_output.elapsed() >= DECODER_GRACE) && lead.running_dry());
                    covered |= covers;
                    if covers
                        && !send_silence(
                            &mut silence,
                            &mut stream,
                            &mut lead,
                            waited_from,
                            "while the decoder gives nothing",
                        )
                    {
                        controller.kill_decoder_of(cid);
                        return Ok(());
                    }
                }
            }
        }

        controller.kill_decoder_of(cid);

        if streamed {
            silent_runs = 0;
        }
        // A skip or seek kills the decoder, which also ends its output: only an end with no
        // request pending is the end of the track. Decided under the request lock, so a request
        // that arrives later is for whatever this end decides. The queue is read as it is now,
        // under the web state's lock, together with the changes the UI made while the track
        // played (a shuffle, a removed row), which it hands over under that lock: a removal
        // shortens the queue, so the end that counts is the end of the queue now, not of the one
        // the track started with (lock order: the web state, then the request).
        let s = state.read().unwrap();
        let pending = controller.request.lock().unwrap();
        let (track_index, removed_now) = controller.follow_reorder(track_index);
        let removed = track_removed || removed_now;
        let tracks: Vec<(String, f64)> = s
            .queue
            .iter()
            .map(|t| (t.filepath.clone(), t.duration_seconds))
            .collect();
        drop(s);
        let queue_len = tracks.len();
        if ended && pending.is_none() {
            let seq = controller.request_seq.load(Ordering::SeqCst);
            // The track after the one that ended, or for a removed one the track that took its
            // place.
            let next_row = if removed { track_index } else { track_index + 1 };
            let wraps = next_row >= queue_len;
            // Reshuffle once per wrap that played something, and only from the current
            // connection of the armed session.
            let owns_session = controller.is_session(session) && controller.owns(cid);
            if wraps && crate::queue::reshuffles_on_wrap(queue_len) && owns_session && streamed_since_wrap {
                // The UI's shuffle keeps the track `current_track_index` names in front, so it
                // names the first track before the UI is asked: never the one that just ended,
                // which would play again. A seek from here on is for that first track, since
                // after the reshuffle no index names the track that ended.
                *controller.current_track_index.lock().unwrap() = 0;
                let _ = cmd_tx.send(WebCommand::StreamReshuffle);
            }
            drop(pending);
            if wraps {
                streamed_since_wrap = false;
            }
            // Otherwise `current_track_index` moves when the next track starts, so a seek that
            // arrives before then is still for this one.
            next_index = Some(if wraps {
                (0, 0)
            } else if removed {
                (track_index, 0)
            } else {
                (track_index, 1)
            });

            if !streamed {
                silent_runs += 1;
            }
            if queue_len == 0 {
                // The next pass idles on the empty queue.
                nothing_playable = None;
            } else if silent_runs >= queue_len {
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
                // found the file. A missing one is asked again, even one that came late: the
                // track may have played for minutes, long enough for the share to come back.
                let next = Path::new(&tracks[next_row % queue_len].0);
                let since = controller.file_checks.since_for_track_end(next, decoder_started);
                look = Some((since, seq));
            }
        } else {
            drop(pending);
            // The next pass takes the request, or after a read error tries the track again.
            next_index = Some((track_index, 0));
            nothing_playable = None;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gives `Interrupted` first, then `data`, then its end.
    struct InterruptedOnce {
        interrupted: bool,
        data: std::io::Cursor<Vec<u8>>,
    }

    impl Read for InterruptedOnce {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            self.data.read(buf)
        }
    }

    /// A read the decoder's reader thread was interrupted in (a signal) is tried again, not taken
    /// for the decoder failing.
    #[test]
    fn test_decoder_reader_retries_an_interrupted_read() {
        let rx = spawn_decoder_reader(InterruptedOnce {
            interrupted: false,
            data: std::io::Cursor::new(vec![1u8; 100]),
        })
        .unwrap();
        let next = || rx.recv_timeout(Duration::from_secs(5)).expect("the reader answers");
        match next() {
            DecoderOutput::Pcm(pcm) => assert_eq!(pcm, vec![1u8; 100]),
            DecoderOutput::End => panic!("End instead of the PCM"),
            DecoderOutput::Failed(e) => panic!("an interrupted read failed the decoder: {}", e),
        }
        assert!(matches!(next(), DecoderOutput::End));
    }
}
