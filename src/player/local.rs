//! High-performance, glitch-free audio playback using mpv JSON IPC with ffplay fallback.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::AudioPlayer;
use crate::paths;

pub struct LocalPlayer {
    backend: String,
    mpv_child: Option<Child>,
    ipc_stream: Option<UnixStream>,
    socket_path: PathBuf,
    current_file: Option<PathBuf>,
    duration: f64,
    volume: u8,
    is_paused: bool,
    is_stopped: bool,
    last_known_pos: f64,
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
            ipc_stream: None,
            socket_path,
            current_file: None,
            duration: 0.0,
            volume: 50,
            is_paused: false,
            is_stopped: true,
            last_known_pos: 0.0,
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

        if is_running && self.ipc_stream.is_some() {
            return Ok(());
        }

        // Clean up old socket file if present
        if self.socket_path.exists() {
            let _ = fs::remove_file(&self.socket_path);
        }
        if let Some(parent) = self.socket_path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        // Spawn mpv in headless idle mode with IPC server enabled
        let mut cmd = Command::new("mpv");
        cmd.arg("--no-video")
            .arg("--idle")
            .arg(format!("--input-ipc-server={}", self.socket_path.display()))
            .arg(format!("--volume={}", self.volume))
            .arg("--audio-display=no")
            .arg("--terminal=no")
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let child = cmd.spawn()?;
        self.mpv_child = Some(child);

        // Connect Unix domain socket with retry
        let mut stream = None;
        for _ in 0..15 {
            thread::sleep(Duration::from_millis(50));
            if self.socket_path.exists() {
                if let Ok(s) = UnixStream::connect(&self.socket_path) {
                    let _ = s.set_read_timeout(Some(Duration::from_millis(80)));
                    let _ = s.set_write_timeout(Some(Duration::from_millis(80)));
                    stream = Some(s);
                    break;
                }
            }
        }

        self.ipc_stream = stream;
        if self.ipc_stream.is_none() {
            anyhow::bail!("Failed to connect to mpv IPC socket at {:?}", self.socket_path);
        }

        Ok(())
    }

    fn send_mpv_command(&mut self, json_str: &str) -> Option<serde_json::Value> {
        let stream = self.ipc_stream.as_mut()?;
        let _ = stream.write_all(json_str.as_bytes());
        let _ = stream.write_all(b"\n");
        let _ = stream.flush();

        let mut reader = BufReader::new(stream.try_clone().ok()?);
        let mut line = String::new();
        while reader.read_line(&mut line).is_ok() {
            if line.trim().is_empty() {
                break;
            }
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                // If it's a response with "error" field, return it
                if val.get("error").is_some() {
                    return Some(val);
                }
            }
            line.clear();
        }
        None
    }

    pub fn is_track_finished(&mut self) -> bool {
        if self.is_stopped || self.current_file.is_none() {
            return false;
        }

        if self.backend == "mpv" {
            let cmd = r#"{"command": ["get_property", "idle-active"]}"#;
            if let Some(resp) = self.send_mpv_command(cmd) {
                if let Some(data) = resp.get("data").and_then(|d| d.as_bool()) {
                    return data;
                }
            }
            false
        } else {
            // ffplay check
            if let Some(child) = self.ffplay_child.as_mut() {
                child.try_wait().ok().flatten().is_some()
            } else {
                false
            }
        }
    }
}

impl Drop for LocalPlayer {
    fn drop(&mut self) {
        if self.backend == "mpv" {
            let _ = self.send_mpv_command(r#"{"command": ["quit"]}"#);
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
            self.duration = meta.duration_seconds;
        } else {
            self.duration = 0.0;
        }
        self.is_stopped = false;
        self.is_paused = false;
        self.last_known_pos = 0.0;

        if self.backend == "mpv" {
            self.ensure_mpv_daemon()?;
            let file_str = filepath.to_string_lossy();
            let json_cmd = serde_json::json!({
                "command": ["loadfile", file_str, "replace"]
            })
            .to_string();
            self.send_mpv_command(&json_cmd);
            self.send_mpv_command(r#"{"command": ["set_property", "pause", false]}"#);
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
        if self.is_stopped {
            return Ok(());
        }
        self.is_paused = true;
        if self.backend == "mpv" {
            self.send_mpv_command(r#"{"command": ["set_property", "pause", true]}"#);
        } else if let Some(mut child) = self.ffplay_child.take() {
            self.accumulated_offset = self.position_seconds();
            let _ = child.kill();
            let _ = child.wait();
        }
        Ok(())
    }

    fn resume(&mut self) -> anyhow::Result<()> {
        if self.is_stopped {
            return Ok(());
        }
        self.is_paused = false;
        if self.backend == "mpv" {
            self.send_mpv_command(r#"{"command": ["set_property", "pause", false]}"#);
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
        if self.is_paused {
            self.resume()?;
            Ok(false)
        } else {
            self.pause()?;
            Ok(true)
        }
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        self.is_stopped = true;
        self.is_paused = false;
        self.current_file = None;
        self.last_known_pos = 0.0;

        if self.backend == "mpv" {
            self.send_mpv_command(r#"{"command": ["stop"]}"#);
        }
        if let Some(mut child) = self.ffplay_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        Ok(())
    }

    fn seek(&mut self, delta_seconds: f64) -> anyhow::Result<()> {
        if self.is_stopped {
            return Ok(());
        }
        if self.backend == "mpv" {
            let json_cmd = serde_json::json!({
                "command": ["seek", delta_seconds, "relative"]
            })
            .to_string();
            self.send_mpv_command(&json_cmd);
            Ok(())
        } else {
            // ffplay: restart at new offset
            let new_pos = (self.position_seconds() + delta_seconds).max(0.0);
            self.accumulated_offset = new_pos;
            if !self.is_paused {
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
            self.send_mpv_command(&json_cmd);
        }
        Ok(())
    }

    fn volume(&self) -> u8 {
        self.volume
    }

    fn is_playing(&self) -> bool {
        !self.is_stopped && !self.is_paused
    }

    fn is_paused(&self) -> bool {
        self.is_paused
    }

    fn position_seconds(&self) -> f64 {
        if self.is_stopped {
            return 0.0;
        }

        if self.backend == "mpv" {
            // For position, send command if we have an active stream
            if let Some(stream) = &self.ipc_stream {
                let mut stream_clone = match stream.try_clone() {
                    Ok(s) => s,
                    Err(_) => return self.last_known_pos,
                };
                let _ = stream_clone.set_read_timeout(Some(Duration::from_millis(30)));
                let _ = stream_clone.set_write_timeout(Some(Duration::from_millis(30)));
                let cmd = b"{\"command\": [\"get_property\", \"time-pos\"]}\n";
                if stream_clone.write_all(cmd).is_ok() {
                    let mut reader = BufReader::new(stream_clone);
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_ok() {
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                            if let Some(pos) = val.get("data").and_then(|d| d.as_f64()) {
                                return pos;
                            }
                        }
                    }
                }
            }
            self.last_known_pos
        } else if let Some(start) = self.start_time {
            let elapsed = start.elapsed().as_secs_f64();
            (self.accumulated_offset + elapsed).min(self.duration)
        } else {
            self.accumulated_offset
        }
    }

    fn duration_seconds(&self) -> f64 {
        self.duration
    }
}
