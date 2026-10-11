//! Local audio playback implementation (supporting mpv IPC and ffplay fallback).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use super::AudioPlayer;

pub struct LocalPlayer {
    backend: String,
    child: Option<Child>,
    current_file: Option<PathBuf>,
    is_paused: bool,
    start_time: Option<Instant>,
    pause_start: Option<Instant>,
    accumulated_offset: f64,
    duration: f64,
    volume: u8,
}

impl LocalPlayer {
    pub fn new(backend: &str) -> Self {
        Self {
            backend: backend.to_string(),
            child: None,
            current_file: None,
            is_paused: false,
            start_time: None,
            pause_start: None,
            accumulated_offset: 0.0,
            duration: 0.0,
            volume: 50,
        }
    }

    fn spawn_process(&mut self, filepath: &Path, start_pos: f64) -> anyhow::Result<()> {
        self.stop_child();

        let mut cmd = if self.backend == "mpv" {
            let mut c = Command::new("mpv");
            c.arg("--no-video")
                .arg(format!("--volume={}", self.volume))
                .arg(format!("--start={}", start_pos))
                .arg(filepath);
            c
        } else {
            let mut c = Command::new("ffplay");
            c.arg("-nodisp")
                .arg("-autoexit")
                .arg("-loglevel")
                .arg("error")
                .arg("-ss")
                .arg(format!("{:.2}", start_pos))
                .arg(filepath);
            c
        };

        cmd.stdout(Stdio::null()).stderr(Stdio::null());

        let child = cmd.spawn()?;
        self.child = Some(child);
        self.start_time = Some(Instant::now());
        self.pause_start = None;
        self.accumulated_offset = start_pos;
        self.is_paused = false;

        Ok(())
    }

    fn stop_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for LocalPlayer {
    fn drop(&mut self) {
        self.stop_child();
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
        self.spawn_process(filepath, 0.0)
    }

    fn pause(&mut self) -> anyhow::Result<()> {
        if !self.is_paused && self.child.is_some() {
            self.is_paused = true;
            self.pause_start = Some(Instant::now());
            // Pause child process with SIGSTOP if on unix
            #[cfg(unix)]
            if let Some(child) = &self.child {
                unsafe {
                    libc::kill(child.id() as i32, libc::SIGSTOP);
                }
            }
        }
        Ok(())
    }

    fn resume(&mut self) -> anyhow::Result<()> {
        if self.is_paused && self.child.is_some() {
            self.is_paused = false;
            if let Some(paused) = self.pause_start.take() {
                // Resume child with SIGCONT
                #[cfg(unix)]
                if let Some(child) = &self.child {
                    unsafe {
                        libc::kill(child.id() as i32, libc::SIGCONT);
                    }
                }
                // Adjust start time by paused duration
                let pause_duration = paused.elapsed();
                if let Some(start) = self.start_time.as_mut() {
                    *start += pause_duration;
                }
            }
        }
        Ok(())
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
        self.stop_child();
        self.current_file = None;
        self.is_paused = false;
        self.start_time = None;
        self.pause_start = None;
        self.accumulated_offset = 0.0;
        self.duration = 0.0;
        Ok(())
    }

    fn seek(&mut self, delta_seconds: f64) -> anyhow::Result<()> {
        let current_pos = self.position_seconds();
        let new_pos = (current_pos + delta_seconds).max(0.0);
        if let Some(file) = self.current_file.clone() {
            self.spawn_process(&file, new_pos)?;
        }
        Ok(())
    }

    fn set_volume(&mut self, volume: u8) -> anyhow::Result<()> {
        self.volume = volume.min(100);
        Ok(())
    }

    fn is_playing(&self) -> bool {
        self.child.is_some() && !self.is_paused
    }

    fn is_paused(&self) -> bool {
        self.is_paused
    }

    fn position_seconds(&self) -> f64 {
        if let Some(start) = self.start_time {
            if self.is_paused {
                if let Some(paused) = self.pause_start {
                    let elapsed = paused.duration_since(start).as_secs_f64();
                    return (self.accumulated_offset + elapsed).min(self.duration);
                }
            }
            let elapsed = start.elapsed().as_secs_f64();
            return (self.accumulated_offset + elapsed).min(self.duration);
        }
        self.accumulated_offset
    }

    fn duration_seconds(&self) -> f64 {
        self.duration
    }
}
