//! Player abstraction and local audio playback implementations.

pub mod local;

use std::path::Path;

pub trait AudioPlayer: Send + Sync {
    fn play(&mut self, filepath: &Path) -> anyhow::Result<()>;
    fn pause(&mut self) -> anyhow::Result<()>;
    fn resume(&mut self) -> anyhow::Result<()>;
    fn toggle_pause(&mut self) -> anyhow::Result<bool>;
    fn stop(&mut self) -> anyhow::Result<()>;
    fn seek(&mut self, delta_seconds: f64) -> anyhow::Result<()>;
    fn seek_to(&mut self, position_seconds: f64) -> anyhow::Result<()>;
    fn set_volume(&mut self, volume: u8) -> anyhow::Result<()>;
    fn volume(&self) -> u8;
    fn is_playing(&self) -> bool;
    fn is_paused(&self) -> bool;
    fn position_seconds(&self) -> f64;
    fn duration_seconds(&self) -> f64;
}
