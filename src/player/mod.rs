//! Player abstraction and local/DLNA audio playback implementations.

pub mod local;
pub mod upnp;
pub mod worker;

use std::path::Path;

pub trait AudioPlayer: Send + Sync {
    fn play(&mut self, filepath: &Path) -> anyhow::Result<()>;
    fn play_at(&mut self, filepath: &Path, position_seconds: f64) -> anyhow::Result<()> {
        self.play(filepath)?;
        if position_seconds > 0.0 {
            self.seek_to(position_seconds)?;
        }
        Ok(())
    }
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
    fn is_track_finished(&mut self) -> bool {
        false
    }
    fn reset_finished(&mut self) {}
    /// Stop using the web server's shared stream state. Called before a player is retired so its
    /// delayed stop/drop cannot close streams that now belong to the next renderer.
    fn detach_stream_state(&mut self) {}
    /// The local backend actually playing (`"mpv"`, or `"ffplay"` once mpv was missing or failed),
    /// for the header. `None` for players that are not local, and for a worker that has not
    /// reported yet. Must be cheap: it is read on every frame.
    fn backend_name(&self) -> Option<&'static str> {
        None
    }
    /// Return and clear the most recent asynchronous command error, if any.
    fn take_error(&mut self) -> Option<String> {
        None
    }
}

/// How close to its end, in seconds, a track counts as over when playback moves to another
/// output (issue #48).
pub const RESUME_END_MARGIN_SECONDS: f64 = 1.0;

/// Where the new output picks up a track when playback moves to it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Resume {
    /// Start the track at this many seconds.
    At(f64),
    /// The track is over: move on as at its natural end.
    NextTrack,
}

/// Where to resume a track `duration` seconds long that was at `position` when the output
/// changed. Inside the last [`RESUME_END_MARGIN_SECONDS`] (or past the end) the track is over,
/// so the new output never gets a start in its last moments: a renderer may refuse such a
/// start, and a tag length can be an estimate past the file's real end. Should a start still
/// land at or past the real end, `LocalPlayer` counts mpv's "nothing played" end of that load
/// as the track finishing (`MpvLoadTracker`), so the queue moves on either way. With an unknown
/// length (0, as the continuous stream can report) the position is kept.
pub fn resume_position(position: f64, duration: f64) -> Resume {
    let position = if position.is_finite() { position.max(0.0) } else { 0.0 };
    if duration.is_finite() && duration > 0.0 && position > 0.0 && position >= duration - RESUME_END_MARGIN_SECONDS {
        Resume::NextTrack
    } else {
        Resume::At(position)
    }
}
