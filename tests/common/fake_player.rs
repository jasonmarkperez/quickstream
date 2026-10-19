//! An `AudioPlayer` that only holds the state a test gives it, for rendering tests that need a
//! player playing at a known position or reporting a given backend.

use std::path::Path;

use quickstream::player::AudioPlayer;

#[derive(Clone, Debug)]
pub struct FakePlayer {
    pub playing: bool,
    pub paused: bool,
    pub position: f64,
    pub duration: f64,
    /// What `is_track_finished` reports until `reset_finished`.
    pub finished: bool,
    pub volume: u8,
    /// What `backend_name` reports.
    pub backend: Option<&'static str>,
}

impl Default for FakePlayer {
    fn default() -> Self {
        Self {
            playing: false,
            paused: false,
            position: 0.0,
            duration: 0.0,
            finished: false,
            volume: 50,
            backend: None,
        }
    }
}

impl FakePlayer {
    /// Playing at `position` seconds into a `duration`-second track.
    pub fn playing_at(position: f64, duration: f64) -> Self {
        Self {
            playing: true,
            position,
            duration,
            ..Self::default()
        }
    }
}

impl AudioPlayer for FakePlayer {
    fn play(&mut self, _filepath: &Path) -> anyhow::Result<()> {
        self.playing = true;
        self.paused = false;
        self.position = 0.0;
        Ok(())
    }
    fn pause(&mut self) -> anyhow::Result<()> {
        self.playing = false;
        self.paused = true;
        Ok(())
    }
    fn resume(&mut self) -> anyhow::Result<()> {
        self.playing = true;
        self.paused = false;
        Ok(())
    }
    fn toggle_pause(&mut self) -> anyhow::Result<bool> {
        if self.paused {
            self.resume()?;
        } else {
            self.pause()?;
        }
        Ok(self.paused)
    }
    fn stop(&mut self) -> anyhow::Result<()> {
        self.playing = false;
        self.paused = false;
        Ok(())
    }
    fn seek(&mut self, delta_seconds: f64) -> anyhow::Result<()> {
        self.position = (self.position + delta_seconds).max(0.0);
        Ok(())
    }
    fn seek_to(&mut self, position_seconds: f64) -> anyhow::Result<()> {
        self.position = position_seconds.max(0.0);
        Ok(())
    }
    fn set_volume(&mut self, volume: u8) -> anyhow::Result<()> {
        self.volume = volume;
        Ok(())
    }
    fn volume(&self) -> u8 {
        self.volume
    }
    fn is_playing(&self) -> bool {
        self.playing
    }
    fn is_paused(&self) -> bool {
        self.paused
    }
    fn position_seconds(&self) -> f64 {
        self.position
    }
    fn duration_seconds(&self) -> f64 {
        self.duration
    }
    fn is_track_finished(&mut self) -> bool {
        self.finished
    }
    fn reset_finished(&mut self) {
        self.finished = false;
    }
    fn backend_name(&self) -> Option<&'static str> {
        self.backend
    }
}
