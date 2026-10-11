//! High-performance, glitch-free audio playback using an async mpv JSON IPC client with ffplay fallback.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::AudioPlayer;
use crate::paths;

#[derive(Debug, Default)]
struct SharedState {
    position: Mutex<f64>,
    duration: Mutex<f64>,
    is_paused: AtomicBool,
    is_playing: AtomicBool,
    is_finished: AtomicBool,
}

pub struct LocalPlayer {
    backend: String,
    mpv_child: Option<Child>,
    ipc_writer: Option<UnixStream>,
    socket_path: PathBuf,
    current_file: Option<PathBuf>,
    volume: u8,
    shared_state: Arc<SharedState>,
    stop_signal: Arc<AtomicBool>,
    reader_thread: Option<thread::JoinHandle<()>>,
    // ffplay fallback state
    ffplay_child: Option<Child>,
    start_time: Option<Instant>,
    accumulated_offset: f64,
}

impl LocalPlayer {
    pub fn new(backend: &str) -> Self {
        let sock_name = format!("mpv_{}.sock", std::process::id());
        let socket_path = paths::get_cache_dir().join(sock_name);

        Self {
            backend: backend.to_string(),
            mpv_child: None,
            ipc_writer: None,
            socket_path,
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

        // Clean up old resources if any
        self.stop_signal.store(true, Ordering::SeqCst);
        if let Some(t) = self.reader_thread.take() {
            let _ = t.join();
        }
        self.ipc_writer = None;

        if self.socket_path.exists() {
            let _ = fs::remove_file(&self.socket_path);
        }
        if let Some(parent) = self.socket_path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        // Spawn mpv in headless idle mode with IPC socket server
        let mut cmd = Command::new("mpv");
        cmd.arg("--no-video")
            .arg("--idle")
            .arg(format!("--input-ipc-server={}", self.socket_path.display()))
            .arg(format!("--volume={}", self.volume))
            .arg("--audio-display=no")
            .arg("--terminal=no")
            .arg("--msg-level=all=no")
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let child = cmd.spawn()?;
        self.mpv_child = Some(child);

        // Connect to socket with retry loop
        let mut writer_stream = None;
        for _ in 0..30 {
            thread::sleep(Duration::from_millis(50));
            if self.socket_path.exists() {
                if let Ok(s) = UnixStream::connect(&self.socket_path) {
                    writer_stream = Some(s);
                    break;
                }
            }
        }

        let mut writer = match writer_stream {
            Some(w) => w,
            None => anyhow::bail!("Failed to connect to mpv IPC socket at {:?}", self.socket_path),
        };

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
                    Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::WouldBlock => {
                        continue;
                    }
                    Err(_) => break,
                }
            }
        }));

        Ok(())
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
        if self.backend == "mpv" {
            self.shared_state.is_finished.load(Ordering::SeqCst)
        } else if let Some(child) = self.ffplay_child.as_mut() {
            child.try_wait().ok().flatten().is_some()
        } else {
            false
        }
    }

    pub fn reset_finished(&self) {
        self.shared_state.is_finished.store(false, Ordering::SeqCst);
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
        if self.socket_path.exists() {
            let _ = fs::remove_file(&self.socket_path);
        }
    }
}

impl AudioPlayer for LocalPlayer {
    fn play(&mut self, filepath: &Path) -> anyhow::Result<()> {
        self.current_file = Some(filepath.to_path_buf());
        if let Ok(meta) = crate::metadata::extract_metadata(filepath) {
            *self.shared_state.duration.lock().unwrap() = meta.duration_seconds;
        } else {
            *self.shared_state.duration.lock().unwrap() = 0.0;
        }

        *self.shared_state.position.lock().unwrap() = 0.0;
        self.shared_state.is_finished.store(false, Ordering::SeqCst);
        self.shared_state.is_playing.store(true, Ordering::SeqCst);
        self.shared_state.is_paused.store(false, Ordering::SeqCst);

        if self.backend == "mpv" {
            self.ensure_mpv_daemon()?;
            let file_str = filepath.to_string_lossy();
            let json_cmd = serde_json::json!({
                "command": ["loadfile", file_str, "replace"]
            })
            .to_string();
            self.send_command(&json_cmd)?;
            self.send_command(r#"{"command": ["set_property", "pause", false]}"#)?;
            Ok(())
        } else {
            // ffplay fallback
            if let Some(mut child) = self.ffplay_child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            let mut cmd = Command::new("ffplay");
            cmd.arg("-nodisp")
                .arg("-autoexit")
                .arg("-loglevel")
                .arg("error")
                .arg(filepath);
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
            self.ffplay_child = Some(cmd.spawn()?);
            self.start_time = Some(Instant::now());
            self.accumulated_offset = 0.0;
            Ok(())
        }
    }

    fn pause(&mut self) -> anyhow::Result<()> {
        if !self.is_playing() {
            return Ok(());
        }
        self.shared_state.is_paused.store(true, Ordering::SeqCst);

        if self.backend == "mpv" {
            self.send_command(r#"{"command": ["set_property", "pause", true]}"#)?;
        } else if let Some(mut child) = self.ffplay_child.take() {
            self.accumulated_offset = self.position_seconds();
            let _ = child.kill();
            let _ = child.wait();
        }
        Ok(())
    }

    fn resume(&mut self) -> anyhow::Result<()> {
        self.shared_state.is_paused.store(false, Ordering::SeqCst);

        if self.backend == "mpv" {
            self.send_command(r#"{"command": ["set_property", "pause", false]}"#)?;
            Ok(())
        } else if let Some(file) = self.current_file.clone() {
            let mut cmd = Command::new("ffplay");
            cmd.arg("-nodisp")
                .arg("-autoexit")
                .arg("-loglevel")
                .arg("error")
                .arg("-ss")
                .arg(format!("{:.2}", self.accumulated_offset))
                .arg(file);
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
            self.ffplay_child = Some(cmd.spawn()?);
            self.start_time = Some(Instant::now());
            Ok(())
        } else {
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

        if self.backend == "mpv" {
            let _ = self.send_command(r#"{"command": ["stop"]}"#);
        }
        if let Some(mut child) = self.ffplay_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        Ok(())
    }

    fn seek(&mut self, delta_seconds: f64) -> anyhow::Result<()> {
        if self.backend == "mpv" {
            let json_cmd = serde_json::json!({
                "command": ["seek", delta_seconds, "relative"]
            })
            .to_string();
            self.send_command(&json_cmd)?;
            Ok(())
        } else {
            let new_pos = (self.position_seconds() + delta_seconds).max(0.0);
            self.accumulated_offset = new_pos;
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
                        .arg(format!("{:.2}", new_pos))
                        .arg(file);
                    cmd.stdout(Stdio::null()).stderr(Stdio::null());
                    self.ffplay_child = Some(cmd.spawn()?);
                    self.start_time = Some(Instant::now());
                }
            }
            Ok(())
        }
    }

    fn set_volume(&mut self, volume: u8) -> anyhow::Result<()> {
        self.volume = volume.min(100);
        if self.backend == "mpv" {
            let json_cmd = serde_json::json!({
                "command": ["set_property", "volume", self.volume]
            })
            .to_string();
            self.send_command(&json_cmd)?;
        }
        Ok(())
    }

    fn volume(&self) -> u8 {
        self.volume
    }

    fn is_playing(&self) -> bool {
        self.shared_state.is_playing.load(Ordering::SeqCst) && !self.is_paused()
    }

    fn is_paused(&self) -> bool {
        self.shared_state.is_paused.load(Ordering::SeqCst)
    }

    fn position_seconds(&self) -> f64 {
        if self.backend == "mpv" {
            *self.shared_state.position.lock().unwrap()
        } else if let Some(start) = self.start_time {
            let elapsed = start.elapsed().as_secs_f64();
            let dur = *self.shared_state.duration.lock().unwrap();
            (self.accumulated_offset + elapsed).min(dur)
        } else {
            self.accumulated_offset
        }
    }

    fn duration_seconds(&self) -> f64 {
        *self.shared_state.duration.lock().unwrap()
    }
}
