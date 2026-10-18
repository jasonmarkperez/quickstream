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
                            if let Some(event) = val.get("event").and_then(|e| e.as_str()) {
                                match event {
                                    "property-change" => {
                                        if let Some(name) = val.get("name").and_then(|n| n.as_str()) {
                                            match name {
                                                "time-pos" => {
                                                    if let Some(pos) = val.get("data").and_then(|d| d.as_f64()) {
                                                        *shared.position.lock().unwrap() = pos;
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
                                                        if eof {
                                                            shared.is_finished.store(true, Ordering::SeqCst);
                                                            shared.is_playing.store(false, Ordering::SeqCst);
                                                        }
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                    "end-file" => {
                                        if let Some(reason) = val.get("reason").and_then(|r| r.as_str()) {
                                            if reason == "eof" {
                                                shared.is_finished.store(true, Ordering::SeqCst);
                                                shared.is_playing.store(false, Ordering::SeqCst);
                                            } else if reason == "error" {
                                                shared.is_finished.store(false, Ordering::SeqCst);
                                                shared.is_playing.store(false, Ordering::SeqCst);
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
        *self.shared_state.position.lock().unwrap() = start_seconds;
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
                let json_cmd = serde_json::json!({
                    "command": ["loadfile", file_str, "replace"]
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
        *self.shared_state.position.lock().unwrap() = 0.0;
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
            let json_cmd = serde_json::json!({
                "command": ["seek", target, "absolute"]
            })
            .to_string();
            self.send_command(&json_cmd)?;
            *self.shared_state.position.lock().unwrap() = target;
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
}
