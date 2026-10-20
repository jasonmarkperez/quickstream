//! High-performance, glitch-free audio playback using an async mpv JSON IPC client with ffplay fallback.

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::AudioPlayer;

#[derive(Debug, Default)]
struct SharedState {
    position: Mutex<f64>,
    duration: Mutex<f64>,
    is_paused: AtomicBool,
    is_playing: AtomicBool,
    is_finished: AtomicBool,
    /// Which mpv playlist entry the newest load is, so the end of a load it replaced is never
    /// taken for its own (issue #48).
    load: Mutex<MpvLoadTracker>,
    /// A load mpv couldn't play, for [`AudioPlayer::take_error`].
    last_error: Mutex<Option<String>>,
    /// The newest seek mpv hasn't confirmed yet, so a `time-pos` it sent before handling the
    /// seek never overwrites the target (issue #92). Locked before `position`.
    seek: Mutex<SeekGate>,
}

/// mpv request ids at and above this one are seeks (`SeekGate`); loads count up from 1
/// (`MpvLoadTracker`), so the two never share one.
const SEEK_REQUEST_BASE: u64 = 1 << 32;
/// How long `time-pos` waits for mpv to confirm a seek before it is taken again anyway. mpv
/// handles a seek command within milliseconds (and reports the target as `time-pos` until it
/// restarts), so the bound only matters for an mpv that never answers request ids.
const SEEK_CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);

/// Holds back mpv's `time-pos` reports while the newest seek is unconfirmed (issue #92).
///
/// `seek_to` writes the target as the position and sends the seek with a request id. Reports
/// mpv sent before handling it would put the old position back, so `time-pos` is ignored until
/// mpv has answered that request (mpv 0.41: the answer, `seek`, `time-pos` at the target, then
/// `playback-restart`, playing or paused) and then restarted playback. An error answer (no file
/// loaded, or the file not loaded yet), an `end-file` after the answer, a new load, `stop`, or
/// [`SEEK_CONFIRM_TIMEOUT`] without either also ends the wait, so it never sticks. Answers and
/// restarts for an older seek don't count: only the newest target is kept.
#[derive(Debug)]
struct SeekGate {
    /// Request id of the newest seek sent (`SEEK_REQUEST_BASE` before the first).
    generation: u64,
    /// The newest seek is still unconfirmed, since this instant.
    pending_since: Option<Instant>,
    /// mpv answered the newest seek with success; its `playback-restart` is next.
    answered: bool,
    timeout: Duration,
}

impl Default for SeekGate {
    fn default() -> Self {
        Self {
            generation: SEEK_REQUEST_BASE,
            pending_since: None,
            answered: false,
            timeout: SEEK_CONFIRM_TIMEOUT,
        }
    }
}

impl SeekGate {
    /// A seek is about to be sent: returns the request id it must carry.
    fn begin(&mut self) -> u64 {
        self.generation += 1;
        self.pending_since = Some(Instant::now());
        self.answered = false;
        self.generation
    }

    fn clear(&mut self) {
        self.pending_since = None;
        self.answered = false;
    }

    /// Read one message from mpv.
    fn on_message(&mut self, msg: &serde_json::Value) {
        if self.pending_since.is_none() {
            return;
        }
        if msg.get("request_id").and_then(|r| r.as_u64()) == Some(self.generation) {
            if msg.get("error").and_then(|e| e.as_str()) == Some("success") {
                self.answered = true;
            } else {
                self.clear();
            }
            return;
        }
        match msg.get("event").and_then(|e| e.as_str()) {
            Some("playback-restart" | "end-file") if self.answered => self.clear(),
            _ => {}
        }
    }

    /// Whether a `time-pos` report may be taken now.
    fn accepts_position(&mut self) -> bool {
        match self.pending_since {
            None => true,
            Some(since) if since.elapsed() >= self.timeout => {
                self.clear();
                true
            }
            Some(_) => false,
        }
    }
}

/// The `file_error` mpv reports for a load started at or past the file's real end (mpv 0.41).
const NO_DATA_PLAYED: &str = "no audio or video data played";

/// How the newest mpv load ended, as [`MpvLoadTracker::on_message`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MpvLoadEnd {
    /// The track played to its end, or was started at or past it.
    Finished,
    /// mpv couldn't play the file (missing, unreadable, not audio): the message says which.
    Failed(String),
}

/// Follows mpv's events for the newest `loadfile`, so a late `end-file` from a load it replaced
/// is never taken for its own (issue #48). Each `loadfile` carries a `request_id`; mpv answers
/// it with the new playlist entry's id (`playlist_entry_id`), and only an `end-file` for that
/// entry counts. Without an id in the answer (mpv 0.33 to 0.37), the latest `start-file` after
/// the answer names it. An mpv that names no entries at all (older than 0.33) has its events
/// taken for the newest load from the first `start-file` after the answer, as before entries had
/// ids, so a track's end is never missed.
///
/// An `end-file` with reason `error` counts as the track finishing only when the load started
/// past 0:00, mpv got as far as loading the file, and it reports that nothing played
/// (`file_error` `no audio or video data played`, or none): a start at or past the file's real
/// end, which a tag length (an estimate) can ask for. Every other error (`loading failed`,
/// `unrecognized file format`, …) is a failure. Public only so tests can feed it events.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct MpvLoadTracker {
    /// The newest load's `request_id` (0 before the first load; mpv answers our other commands
    /// with 0).
    generation: u64,
    started_past_zero: bool,
    file_name: String,
    /// mpv has answered the newest `loadfile`.
    answered: bool,
    /// The newest load's playlist entry, once known.
    entry: Option<i64>,
    /// `entry` came from mpv's answer (else from the latest `start-file`).
    entry_from_answer: bool,
    /// The entry of the latest `start-file`.
    started: Option<i64>,
    /// mpv reported `file-loaded` for the newest load's entry.
    file_loaded: bool,
    /// mpv names no playlist entries (older than 0.33), and the newest load's `start-file` came.
    unnamed_entries: bool,
}

impl MpvLoadTracker {
    /// A new load of `file` is about to be sent, starting past 0:00 when `started_past_zero`.
    /// Returns the `request_id` its `loadfile` must carry.
    pub fn begin(&mut self, file: &Path, started_past_zero: bool) -> u64 {
        self.generation += 1;
        self.started_past_zero = started_past_zero;
        self.file_name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.display().to_string());
        self.answered = false;
        self.entry = None;
        self.entry_from_answer = false;
        self.file_loaded = false;
        self.unnamed_entries = false;
        self.generation
    }

    /// Whether mpv's events now concern the newest load (its entry is known, or mpv names no
    /// entries and the load has started).
    pub fn is_current(&self) -> bool {
        self.answered && (self.entry.is_some() || self.unnamed_entries)
    }

    /// Read one message from mpv. Returns how the newest load ended when this message says so.
    pub fn on_message(&mut self, msg: &serde_json::Value) -> Option<MpvLoadEnd> {
        if self.generation > 0 && msg.get("request_id").and_then(|r| r.as_u64()) == Some(self.generation) {
            self.answered = true;
            let id = msg.pointer("/data/playlist_entry_id").and_then(|i| i.as_i64());
            self.entry_from_answer = id.is_some();
            self.entry = id;
            return None;
        }
        let event = msg.get("event").and_then(|e| e.as_str())?;
        let id = msg.get("playlist_entry_id").and_then(|i| i.as_i64());
        match event {
            "start-file" => {
                self.started = id;
                if self.answered && !self.entry_from_answer {
                    self.entry = id;
                    self.unnamed_entries = id.is_none();
                }
                if self.is_current() && self.entry == id {
                    self.file_loaded = false;
                }
                None
            }
            "file-loaded" => {
                if self.is_current() && self.started == self.entry {
                    self.file_loaded = true;
                }
                None
            }
            "end-file" => {
                if !self.is_current() || id != self.entry {
                    return None;
                }
                match msg.get("reason").and_then(|r| r.as_str()) {
                    Some("eof") => Some(MpvLoadEnd::Finished),
                    Some("error") => {
                        let file_error = msg.get("file_error").and_then(|e| e.as_str());
                        if self.started_past_zero && self.file_loaded && file_error.is_none_or(|e| e == NO_DATA_PLAYED)
                        {
                            Some(MpvLoadEnd::Finished)
                        } else {
                            Some(MpvLoadEnd::Failed(format!(
                                "couldn't play {} ({})",
                                self.file_name,
                                file_error.unwrap_or("error")
                            )))
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

/// File name of the mpv IPC socket inside the player's private directory.
const IPC_SOCKET_NAME: &str = "mpv.sock";
/// Random bytes in the private directory name (hex encoded, so twice as many chars).
const IPC_DIR_RANDOM_BYTES: usize = 8;
/// `sockaddr_un.sun_path` is 104 bytes on macOS and 108 on Linux (including the NUL).
const MAX_SOCKET_PATH_LEN: usize = 100;

fn current_uid() -> u32 {
    // SAFETY: getuid() has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

fn permission_error(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, msg)
}

/// Create `path` as a brand-new directory with mode 0700, owned by the current user.
///
/// Refuses (returns an error) if anything already exists at `path`, including a symlink:
/// `mkdir(2)` never follows a symlink in the final component, and an existing entry is
/// never adopted or re-permissioned.
pub fn create_private_ipc_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_dir() || meta.uid() != current_uid() {
        return Err(permission_error(format!(
            "{:?} is not a directory owned by the current user",
            path
        )));
    }
    // The umask can only clear bits; make sure the owner still has full access.
    if meta.mode() & 0o777 != 0o700 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Check that `path` is a unix socket (not a symlink or other file) owned by the current user.
pub fn verify_ipc_socket(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_socket() {
        return Err(permission_error(format!("{:?} is not a unix socket", path)));
    }
    if meta.uid() != current_uid() {
        return Err(permission_error(format!(
            "{:?} is owned by uid {}, expected {}",
            path,
            meta.uid(),
            current_uid()
        )));
    }
    Ok(())
}

fn random_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    let from_os = fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !from_os {
        // RandomState is seeded from the OS RNG; mix in time and a counter per chunk.
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        for chunk in buf.chunks_mut(8) {
            let mut h = RandomState::new().build_hasher();
            h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
            h.write_u128(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
            );
            let bytes = h.finish().to_ne_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Candidate parent directories for the private IPC directory, in order of preference.
fn ipc_base_dirs() -> Vec<PathBuf> {
    let mut bases = Vec::new();
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        let runtime = PathBuf::from(runtime);
        if runtime.is_absolute() {
            bases.push(runtime);
        }
    }
    bases.push(std::env::temp_dir());
    bases.push(PathBuf::from("/tmp"));
    bases
}

/// A base is unusable if it is missing or if other users could rename entries in it
/// (world-writable without the sticky bit).
fn usable_ipc_base(base: &Path) -> bool {
    match fs::metadata(base) {
        Ok(meta) => meta.is_dir() && !(meta.mode() & 0o002 != 0 && meta.mode() & 0o1000 == 0),
        Err(_) => false,
    }
}

/// Create a fresh private directory (mode 0700, unpredictable name) to hold the mpv socket.
fn create_ipc_dir() -> io::Result<PathBuf> {
    let name_len = "qs-".len() + IPC_DIR_RANDOM_BYTES * 2;
    let mut last_err = io::Error::new(io::ErrorKind::NotFound, "no usable directory for the mpv IPC socket");
    for base in ipc_base_dirs() {
        let path_len = base.as_os_str().len() + 1 + name_len + 1 + IPC_SOCKET_NAME.len();
        if path_len > MAX_SOCKET_PATH_LEN || !usable_ipc_base(&base) {
            continue;
        }
        for _ in 0..8 {
            let dir = base.join(format!("qs-{}", random_hex(IPC_DIR_RANDOM_BYTES)));
            match create_private_ipc_dir(&dir) {
                Ok(()) => return Ok(dir),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last_err = e,
                Err(e) => {
                    last_err = e;
                    break;
                }
            }
        }
    }
    Err(last_err)
}

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

pub struct LocalPlayer {
    backend: String,
    mpv_binary: PathBuf,
    mpv_child: Option<Child>,
    ipc_writer: Option<UnixStream>,
    /// Private 0700 directory holding the IPC socket; created on first mpv start.
    ipc_dir: Option<PathBuf>,
    socket_path: Option<PathBuf>,
    current_file: Option<PathBuf>,
    volume: u8,
    shared_state: Arc<SharedState>,
    stop_signal: Arc<AtomicBool>,
    reader_thread: Option<thread::JoinHandle<()>>,
    /// The backend that played the last track: `"ffplay"` after a fallback from mpv. Before
    /// anything plays, the configured one.
    active_backend: &'static str,
    // ffplay fallback state
    ffplay_child: Option<Child>,
    start_time: Option<Instant>,
    accumulated_offset: f64,
}

impl LocalPlayer {
    pub fn new(backend: &str) -> Self {
        Self::with_mpv_binary(backend, "mpv")
    }

    /// Path of the mpv IPC socket, once mpv has been started.
    pub fn ipc_socket_path(&self) -> Option<&Path> {
        self.socket_path.as_deref()
    }

    /// Like [`LocalPlayer::new`], but launches `mpv_binary` instead of `mpv` from `PATH`.
    pub fn with_mpv_binary(backend: &str, mpv_binary: impl Into<PathBuf>) -> Self {
        Self {
            active_backend: if backend == "mpv" { "mpv" } else { "ffplay" },
            backend: backend.to_string(),
            mpv_binary: mpv_binary.into(),
            mpv_child: None,
            ipc_writer: None,
            // Each instance gets its own directory, so an old player still shutting down on
            // another thread never removes a new player's socket.
            ipc_dir: None,
            socket_path: None,
            current_file: None,
            volume: 50,
            shared_state: Arc::new(SharedState::default()),
            stop_signal: Arc::new(AtomicBool::new(false)),
            reader_thread: None,
            ffplay_child: None,
            start_time: None,
            accumulated_offset: 0.0,
        }
    }

    /// How long a seek mpv never confirms holds back its position reports (default 2 s). Test
    /// seam (issue #92).
    #[doc(hidden)]
    pub fn set_seek_confirm_timeout(&mut self, timeout: Duration) {
        self.shared_state.seek.lock().unwrap().timeout = timeout;
    }

    /// Set the position to `seconds`, ending the wait for any seek (a new load or a stop).
    fn reset_position(&self, seconds: f64) {
        let mut seek = self.shared_state.seek.lock().unwrap();
        seek.clear();
        *self.shared_state.position.lock().unwrap() = seconds;
    }

    fn ensure_mpv_daemon(&mut self) -> anyhow::Result<()> {
        let is_running = self
            .mpv_child
            .as_mut()
            .map(|c| c.try_wait().ok().flatten().is_none())
            .unwrap_or(false);

        if is_running && self.ipc_writer.is_some() {
            return Ok(());
        }

        // Clean up old resources if any without blocking UI thread on socket join
        self.stop_signal.store(true, Ordering::SeqCst);
        self.reader_thread = None;
        self.ipc_writer = None;

        // Never let a previous mpv outlive its handle: stop and reap it before respawning.
        if let Some(mut old) = self.mpv_child.take() {
            kill_and_reap(&mut old);
        }

        let socket_path = self.prepare_ipc_socket_path()?;

        // Spawn mpv in headless idle mode with IPC socket server
        let mut cmd = Command::new(&self.mpv_binary);
        cmd.arg("--no-video")
            .arg("--idle")
            .arg("--gapless-audio=yes")
            .arg(format!("--input-ipc-server={}", socket_path.display()))
            .arg(format!("--volume={}", self.volume))
            .arg("--audio-display=no")
            .arg("--terminal=no")
            .arg("--msg-level=all=no")
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let mut child = cmd.spawn()?;

        let mut writer = match Self::connect_ipc(&socket_path, &mut child) {
            Ok(w) => w,
            Err(e) => {
                kill_and_reap(&mut child);
                let _ = fs::remove_file(&socket_path);
                return Err(e);
            }
        };
        self.mpv_child = Some(child);

        let reader_stream = writer.try_clone()?;
        let _ = reader_stream.set_read_timeout(Some(Duration::from_millis(400)));

        // Send observe_property commands so mpv streams property changes asynchronously
        let _ = writer.write_all(b"{\"command\": [\"observe_property\", 1, \"time-pos\"]}\n");
        let _ = writer.write_all(b"{\"command\": [\"observe_property\", 2, \"pause\"]}\n");
        let _ = writer.write_all(b"{\"command\": [\"observe_property\", 3, \"eof-reached\"]}\n");
        let _ = writer.write_all(b"{\"command\": [\"observe_property\", 4, \"duration\"]}\n");
        let _ = writer.flush();

        self.ipc_writer = Some(writer);
        self.stop_signal = Arc::new(AtomicBool::new(false));

        // Spawn dedicated background reader thread
        let shared = Arc::clone(&self.shared_state);
        let stop = Arc::clone(&self.stop_signal);

        self.reader_thread = Some(thread::spawn(move || {
            let mut reader = BufReader::new(reader_stream);
            let mut line = String::new();

            while !stop.load(Ordering::Relaxed) {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break, // Socket closed
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }

                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
                            let (end, current) = {
                                let mut load = shared.load.lock().unwrap();
                                (load.on_message(&val), load.is_current())
                            };
                            shared.seek.lock().unwrap().on_message(&val);
                            match end {
                                Some(MpvLoadEnd::Finished) => {
                                    shared.is_finished.store(true, Ordering::SeqCst);
                                    shared.is_playing.store(false, Ordering::SeqCst);
                                }
                                Some(MpvLoadEnd::Failed(err)) => {
                                    crate::log_warn!("player", "mpv: {}", err);
                                    shared.is_finished.store(false, Ordering::SeqCst);
                                    shared.is_playing.store(false, Ordering::SeqCst);
                                    *shared.last_error.lock().unwrap() = Some(err);
                                }
                                None => {}
                            }
                            if let Some(event) = val.get("event").and_then(|e| e.as_str()) {
                                match event {
                                    "property-change" => {
                                        if let Some(name) = val.get("name").and_then(|n| n.as_str()) {
                                            match name {
                                                "time-pos" => {
                                                    if let Some(pos) = val.get("data").and_then(|d| d.as_f64()) {
                                                        // Under the seek lock, so a seek_to between
                                                        // the check and the write can't be undone.
                                                        let mut seek = shared.seek.lock().unwrap();
                                                        if seek.accepts_position() {
                                                            *shared.position.lock().unwrap() = pos;
                                                        }
                                                    }
                                                }
                                                "duration" => {
                                                    if let Some(dur) = val.get("data").and_then(|d| d.as_f64()) {
                                                        *shared.duration.lock().unwrap() = dur;
                                                    }
                                                }
                                                "pause" => {
                                                    if let Some(p) = val.get("data").and_then(|d| d.as_bool()) {
                                                        shared.is_paused.store(p, Ordering::SeqCst);
                                                    }
                                                }
                                                "eof-reached" => {
                                                    if let Some(eof) = val.get("data").and_then(|d| d.as_bool()) {
                                                        if eof && current {
                                                            shared.is_finished.store(true, Ordering::SeqCst);
                                                            shared.is_playing.store(false, Ordering::SeqCst);
                                                        }
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                    "playback-restart" | "file-loaded" => {
                                        shared.is_playing.store(true, Ordering::SeqCst);
                                        shared.is_finished.store(false, Ordering::SeqCst);
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        continue;
                    }
                    Err(_) => break,
                }
            }
        }));

        Ok(())
    }

    /// Ensure the private IPC directory exists and return a socket path inside it, with any
    /// socket left over from a previous mpv in this directory removed.
    fn prepare_ipc_socket_path(&mut self) -> anyhow::Result<PathBuf> {
        let dir = match &self.ipc_dir {
            Some(dir) => {
                let meta = fs::symlink_metadata(dir)?;
                if !meta.file_type().is_dir() || meta.uid() != current_uid() || meta.mode() & 0o077 != 0 {
                    anyhow::bail!("mpv IPC directory {:?} is no longer private", dir);
                }
                dir.clone()
            }
            None => {
                let dir = create_ipc_dir()?;
                self.ipc_dir = Some(dir.clone());
                dir
            }
        };
        let socket_path = dir.join(IPC_SOCKET_NAME);
        match fs::remove_file(&socket_path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        self.socket_path = Some(socket_path.clone());
        Ok(socket_path)
    }

    /// Wait (up to 3s) for `child` to create its IPC socket, verify it, and connect.
    fn connect_ipc(socket_path: &Path, child: &mut Child) -> anyhow::Result<UnixStream> {
        for _ in 0..60 {
            thread::sleep(Duration::from_millis(50));
            if let Ok(Some(status)) = child.try_wait() {
                anyhow::bail!("mpv exited before opening its IPC socket ({})", status);
            }
            match fs::symlink_metadata(socket_path) {
                Ok(_) => {
                    verify_ipc_socket(socket_path)?;
                    if let Ok(stream) = UnixStream::connect(socket_path) {
                        return Ok(stream);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        anyhow::bail!("Failed to connect to mpv IPC socket at {:?}", socket_path)
    }

    fn send_command(&mut self, json_str: &str) -> anyhow::Result<()> {
        if let Some(stream) = self.ipc_writer.as_mut() {
            stream.write_all(json_str.as_bytes())?;
            stream.write_all(b"\n")?;
            stream.flush()?;
            Ok(())
        } else {
            anyhow::bail!("No active mpv IPC connection");
        }
    }

    pub fn is_track_finished(&mut self) -> bool {
        if let Some(child) = self.ffplay_child.as_mut() {
            child.try_wait().ok().flatten().is_some()
        } else if self.backend == "mpv" {
            self.shared_state.is_finished.load(Ordering::SeqCst)
        } else {
            false
        }
    }

    pub fn reset_finished(&self) {
        self.shared_state.is_finished.store(false, Ordering::SeqCst);
    }

    fn play_ffplay(&mut self, filepath: &Path, start_seconds: f64) -> anyhow::Result<()> {
        if let Some(mut child) = self.ffplay_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let mut cmd = Command::new("ffplay");
        cmd.arg("-nodisp")
            .arg("-autoexit")
            .arg("-loglevel")
            .arg("error")
            .arg("-volume")
            .arg(format!("{}", self.volume));
        if start_seconds > 0.0 {
            cmd.arg("-ss").arg(format!("{:.3}", start_seconds));
        }
        cmd.arg(filepath);
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        self.ffplay_child = Some(cmd.spawn()?);
        self.start_time = Some(Instant::now());
        self.accumulated_offset = start_seconds;
        self.active_backend = "ffplay";

        // ffplay has no IPC to report duration, so read it from the file's tags/headers.
        // This runs on AsyncPlayer's worker thread, never the UI thread.
        let duration = crate::metadata::extract_metadata(filepath)
            .map(|m| m.duration_seconds)
            .unwrap_or(0.0);
        *self.shared_state.duration.lock().unwrap() = if duration.is_finite() && duration > 0.0 {
            duration
        } else {
            0.0
        };
        Ok(())
    }
}

impl Drop for LocalPlayer {
    fn drop(&mut self) {
        self.stop_signal.store(true, Ordering::SeqCst);
        if self.backend == "mpv" {
            let _ = self.send_command(r#"{"command": ["quit"]}"#);
        }
        if let Some(mut child) = self.mpv_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(mut child) = self.ffplay_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(socket_path) = self.socket_path.take() {
            let _ = fs::remove_file(socket_path);
        }
        if let Some(dir) = self.ipc_dir.take() {
            // remove_dir (not remove_dir_all): only ever deletes our own, now empty, directory.
            let _ = fs::remove_dir(dir);
        }
    }
}

impl LocalPlayer {
    /// Load `filepath` and start playing at `start_seconds`.
    ///
    /// The start position is part of the load (mpv's `start` option, ffplay's `-ss`) rather than
    /// a seek afterwards: on a slow source such as an SMB share, mpv rejects a seek sent before
    /// the file has finished loading, and playback would begin at 0:00 (issue #44).
    fn play_from(&mut self, filepath: &Path, start_seconds: f64) -> anyhow::Result<()> {
        let start_seconds = if start_seconds.is_finite() {
            start_seconds.max(0.0)
        } else {
            0.0
        };
        let _timer = crate::log_perf!(
            "player",
            "LocalPlayer::play '{}' at {:.2}s via {}",
            filepath.display(),
            start_seconds,
            self.backend
        );
        let abs_path = if filepath.is_absolute() {
            filepath.to_path_buf()
        } else {
            filepath.canonicalize().unwrap_or_else(|_| filepath.to_path_buf())
        };
        self.current_file = Some(abs_path.clone());

        // Duration is streamed asynchronously via IPC ("observe_property", "duration") for mpv
        // and read from metadata for ffplay; clear the previous track's until then.
        self.reset_position(start_seconds);
        *self.shared_state.duration.lock().unwrap() = 0.0;
        self.accumulated_offset = start_seconds;
        self.start_time = None;
        self.shared_state.is_finished.store(false, Ordering::SeqCst);
        self.shared_state.is_playing.store(true, Ordering::SeqCst);
        self.shared_state.is_paused.store(false, Ordering::SeqCst);

        if self.backend == "mpv" {
            let mpv_res = (|| -> anyhow::Result<()> {
                self.ensure_mpv_daemon()?;
                let file_str = abs_path.to_string_lossy();
                // `start` applies to every following load, so it is always set (or cleared).
                let start = if start_seconds > 0.0 {
                    format!("{:.3}", start_seconds)
                } else {
                    "none".to_string()
                };
                let start_cmd = serde_json::json!({
                    "command": ["set_property", "start", start]
                })
                .to_string();
                self.send_command(&start_cmd)?;
                // The request id ties mpv's answer, and so the new playlist entry, to this load.
                let request_id = self
                    .shared_state
                    .load
                    .lock()
                    .unwrap()
                    .begin(&abs_path, start_seconds > 0.0);
                let json_cmd = serde_json::json!({
                    "command": ["loadfile", file_str, "replace"],
                    "request_id": request_id
                })
                .to_string();
                self.send_command(&json_cmd)?;
                self.send_command(r#"{"command": ["set_property", "pause", false]}"#)?;
                Ok(())
            })();

            if let Err(e) = &mpv_res {
                // If mpv fails to spawn or connect, seamlessly fall back to ffplay
                crate::log_warn!("player", "mpv playback error ({:?}), falling back to ffplay", e);
                return self.play_ffplay(&abs_path, start_seconds);
            }
            self.active_backend = "mpv";
            mpv_res
        } else {
            self.play_ffplay(&abs_path, start_seconds)
        }
    }
}

impl AudioPlayer for LocalPlayer {
    fn backend_name(&self) -> Option<&'static str> {
        Some(self.active_backend)
    }

    fn play(&mut self, filepath: &Path) -> anyhow::Result<()> {
        self.play_from(filepath, 0.0)
    }

    fn play_at(&mut self, filepath: &Path, position_seconds: f64) -> anyhow::Result<()> {
        self.play_from(filepath, position_seconds)
    }

    fn pause(&mut self) -> anyhow::Result<()> {
        if !self.is_playing() {
            return Ok(());
        }
        self.shared_state.is_paused.store(true, Ordering::SeqCst);

        if self.ffplay_child.is_some() || self.backend != "mpv" {
            self.accumulated_offset = self.position_seconds();
            self.start_time = None;
            if let Some(mut child) = self.ffplay_child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        } else if self.backend == "mpv" {
            self.send_command(r#"{"command": ["set_property", "pause", true]}"#)?;
        }
        Ok(())
    }

    fn resume(&mut self) -> anyhow::Result<()> {
        self.shared_state.is_paused.store(false, Ordering::SeqCst);

        if self.ffplay_child.is_some() || self.backend != "mpv" {
            if let Some(file) = self.current_file.clone() {
                let mut cmd = Command::new("ffplay");
                cmd.arg("-nodisp")
                    .arg("-autoexit")
                    .arg("-loglevel")
                    .arg("error")
                    .arg("-ss")
                    .arg(format!("{:.2}", self.accumulated_offset))
                    .arg("-volume")
                    .arg(format!("{}", self.volume))
                    .arg(file);
                cmd.stdout(Stdio::null()).stderr(Stdio::null());
                self.ffplay_child = Some(cmd.spawn()?);
                self.start_time = Some(Instant::now());
            }
            Ok(())
        } else {
            self.send_command(r#"{"command": ["set_property", "pause", false]}"#)?;
            Ok(())
        }
    }

    fn toggle_pause(&mut self) -> anyhow::Result<bool> {
        if self.is_paused() {
            self.resume()?;
            Ok(false)
        } else {
            self.pause()?;
            Ok(true)
        }
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        self.current_file = None;
        self.shared_state.is_playing.store(false, Ordering::SeqCst);
        self.shared_state.is_paused.store(false, Ordering::SeqCst);
        self.shared_state.is_finished.store(false, Ordering::SeqCst);
        self.reset_position(0.0);
        self.accumulated_offset = 0.0;
        self.start_time = None;

        if let Some(mut child) = self.ffplay_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if self.backend == "mpv" {
            let _ = self.send_command(r#"{"command": ["stop"]}"#);
        }
        Ok(())
    }

    fn seek(&mut self, delta_seconds: f64) -> anyhow::Result<()> {
        let new_pos = (self.position_seconds() + delta_seconds).max(0.0);
        self.seek_to(new_pos)
    }

    fn seek_to(&mut self, position_seconds: f64) -> anyhow::Result<()> {
        let dur = self.duration_seconds();
        let target = if dur > 0.0 {
            position_seconds.clamp(0.0, dur)
        } else {
            position_seconds.max(0.0)
        };

        crate::log_info!(
            "player",
            "LocalPlayer::seek_to target={:.2}s via {}",
            target,
            self.backend
        );

        if self.current_file.is_none() {
            self.accumulated_offset = target;
            *self.shared_state.position.lock().unwrap() = target;
            return Ok(());
        }

        if self.ffplay_child.is_some() || self.backend != "mpv" {
            self.accumulated_offset = target;
            if !self.is_paused() {
                if let Some(file) = self.current_file.clone() {
                    if let Some(mut child) = self.ffplay_child.take() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    let mut cmd = Command::new("ffplay");
                    cmd.arg("-nodisp")
                        .arg("-autoexit")
                        .arg("-loglevel")
                        .arg("error")
                        .arg("-ss")
                        .arg(format!("{:.2}", target))
                        .arg("-volume")
                        .arg(format!("{}", self.volume))
                        .arg(file);
                    cmd.stdout(Stdio::null()).stderr(Stdio::null());
                    self.ffplay_child = Some(cmd.spawn()?);
                    self.start_time = Some(Instant::now());
                }
            } else {
                self.start_time = None;
            }
            Ok(())
        } else if self.backend == "mpv" {
            // The target shows at once, and mpv's reports from before it handles the seek are
            // held back until it confirms (issue #92). Set before sending, so the answer can't
            // arrive first.
            let request_id = {
                let mut seek = self.shared_state.seek.lock().unwrap();
                *self.shared_state.position.lock().unwrap() = target;
                seek.begin()
            };
            let json_cmd = serde_json::json!({
                "command": ["seek", target, "absolute"],
                "request_id": request_id
            })
            .to_string();
            if let Err(e) = self.send_command(&json_cmd) {
                self.shared_state.seek.lock().unwrap().clear();
                return Err(e);
            }
            Ok(())
        } else {
            Ok(())
        }
    }

    fn set_volume(&mut self, volume: u8) -> anyhow::Result<()> {
        self.volume = volume.min(100);
        if self.backend == "mpv" && self.ffplay_child.is_none() {
            let json_cmd = serde_json::json!({
                "command": ["set_property", "volume", self.volume]
            })
            .to_string();
            let _ = self.send_command(&json_cmd);
        }
        Ok(())
    }

    fn volume(&self) -> u8 {
        self.volume
    }

    fn is_playing(&self) -> bool {
        if self.ffplay_child.is_some() {
            !self.is_paused()
        } else {
            self.shared_state.is_playing.load(Ordering::SeqCst) && !self.is_paused()
        }
    }

    fn is_paused(&self) -> bool {
        self.shared_state.is_paused.load(Ordering::SeqCst)
    }

    fn position_seconds(&self) -> f64 {
        if self.ffplay_child.is_some() {
            if let Some(start) = self.start_time {
                let elapsed = start.elapsed().as_secs_f64();
                let dur = *self.shared_state.duration.lock().unwrap();
                if dur > 0.0 {
                    (self.accumulated_offset + elapsed).min(dur)
                } else {
                    self.accumulated_offset + elapsed
                }
            } else {
                self.accumulated_offset
            }
        } else if self.backend != "mpv" {
            self.accumulated_offset
        } else if self.backend == "mpv" {
            *self.shared_state.position.lock().unwrap()
        } else {
            0.0
        }
    }

    fn duration_seconds(&self) -> f64 {
        *self.shared_state.duration.lock().unwrap()
    }

    fn is_track_finished(&mut self) -> bool {
        LocalPlayer::is_track_finished(self)
    }

    fn reset_finished(&mut self) {
        LocalPlayer::reset_finished(self);
    }

    fn take_error(&mut self) -> Option<String> {
        self.shared_state.last_error.lock().unwrap().take()
    }
}
