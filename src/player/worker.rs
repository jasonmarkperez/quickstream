//! Runs an `AudioPlayer` on a dedicated thread so slow backends (DLNA SOAP calls, mpv startup)
//! never block the UI event loop.
//!
//! `AsyncPlayer` queues every command to the worker and returns immediately. Reads come from a
//! snapshot the worker refreshes after each command and every `REFRESH_INTERVAL`; commands update
//! the snapshot optimistically so the UI reflects them before the backend has finished.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use super::AudioPlayer;

const REFRESH_INTERVAL: Duration = Duration::from_millis(100);
/// Longer than LocalPlayer's mpv connect loop (60 × 50ms), so quitting while mpv is starting
/// still waits for the worker to kill it.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

enum Command {
    Play(PathBuf),
    PlayAt(PathBuf, f64),
    Pause,
    Resume,
    TogglePause,
    Stop,
    Seek(f64),
    SeekTo(f64),
    SetVolume(u8),
    ResetFinished,
    DetachStreamState,
}

#[derive(Clone, Default)]
struct Snapshot {
    volume: u8,
    playing: bool,
    paused: bool,
    position: f64,
    duration: f64,
    finished: bool,
    /// The backend the player reports (`AudioPlayer::backend_name`), refreshed with the rest but
    /// never optimistic.
    backend: Option<&'static str>,
    /// Commands sent but not yet applied by the worker. While non-zero the worker does not
    /// overwrite the optimistic snapshot with backend state.
    pending: usize,
    /// `SetVolume` commands not yet applied. Volume is refreshed from the backend whenever this
    /// is zero, even while other commands are pending: a renderer's volume must be known before
    /// a relative volume key computes `volume() ± step`.
    volume_pending: usize,
}

struct Shared {
    state: Mutex<Snapshot>,
    last_error: Mutex<Option<String>>,
}

/// An `AudioPlayer` proxy that owns the real player on a background thread.
pub struct AsyncPlayer {
    tx: Option<Sender<Command>>,
    shared: Arc<Shared>,
    done_rx: Mutex<Receiver<()>>,
    handle: Option<JoinHandle<()>>,
}

impl AsyncPlayer {
    /// Start a worker thread, build the real player on it with `factory`, and return a proxy.
    /// `initial_volume` is reported by `volume()` until the worker's first refresh.
    pub fn spawn<F>(factory: F, initial_volume: u8) -> Self
    where
        F: FnOnce() -> Box<dyn AudioPlayer> + Send + 'static,
    {
        let shared = Arc::new(Shared {
            state: Mutex::new(Snapshot {
                volume: initial_volume,
                ..Snapshot::default()
            }),
            last_error: Mutex::new(None),
        });
        let (tx, rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker_shared = shared.clone();
        let handle = std::thread::Builder::new()
            .name("player-worker".to_string())
            .spawn(move || {
                run_worker(factory, rx, &worker_shared);
                let _ = done_tx.send(());
            })
            .ok();

        Self {
            tx: Some(tx),
            shared,
            done_rx: Mutex::new(done_rx),
            handle,
        }
    }

    fn send(&self, cmd: Command, optimistic: impl FnOnce(&mut Snapshot)) -> anyhow::Result<()> {
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("player worker is shut down"))?;
        let mut state = self.shared.state.lock().unwrap();
        tx.send(cmd).map_err(|_| anyhow::anyhow!("player worker has stopped"))?;
        state.pending += 1;
        optimistic(&mut state);
        Ok(())
    }

    fn snapshot(&self) -> Snapshot {
        self.shared.state.lock().unwrap().clone()
    }
}

impl Drop for AsyncPlayer {
    fn drop(&mut self) {
        // Closing the channel tells the worker to drop the real player (kills mpv, stops renderers).
        self.tx.take();
        let finished = self.done_rx.lock().map(|rx| rx.recv_timeout(SHUTDOWN_TIMEOUT).is_ok());
        if finished.unwrap_or(false) {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        } else {
            crate::log_warn!("player", "Player worker did not stop within {:?}", SHUTDOWN_TIMEOUT);
        }
    }
}

fn run_worker<F>(factory: F, rx: Receiver<Command>, shared: &Shared)
where
    F: FnOnce() -> Box<dyn AudioPlayer>,
{
    let mut player = factory();
    refresh(player.as_mut(), shared);
    loop {
        match rx.recv_timeout(REFRESH_INTERVAL) {
            Ok(cmd) => {
                let is_volume = matches!(cmd, Command::SetVolume(_));
                let result = apply(player.as_mut(), cmd);
                {
                    let mut state = shared.state.lock().unwrap();
                    state.pending -= 1;
                    if is_volume {
                        state.volume_pending -= 1;
                    }
                }
                if let Err(e) = result {
                    crate::log_warn!("player", "Player command failed: {}", e);
                    *shared.last_error.lock().unwrap() = Some(e.to_string());
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        refresh(player.as_mut(), shared);
    }
}

fn apply(player: &mut dyn AudioPlayer, cmd: Command) -> anyhow::Result<()> {
    match cmd {
        Command::Play(path) => player.play(&path),
        Command::PlayAt(path, pos) => player.play_at(&path, pos),
        Command::Pause => player.pause(),
        Command::Resume => player.resume(),
        Command::TogglePause => player.toggle_pause().map(|_| ()),
        Command::Stop => player.stop(),
        Command::Seek(delta) => player.seek(delta),
        Command::SeekTo(pos) => player.seek_to(pos),
        Command::SetVolume(v) => player.set_volume(v),
        Command::ResetFinished => {
            player.reset_finished();
            Ok(())
        }
        Command::DetachStreamState => {
            player.detach_stream_state();
            Ok(())
        }
    }
}

fn refresh(player: &mut dyn AudioPlayer, shared: &Shared) {
    let fresh = Snapshot {
        volume: player.volume(),
        playing: player.is_playing(),
        paused: player.is_paused(),
        position: player.position_seconds(),
        duration: player.duration_seconds(),
        finished: player.is_track_finished(),
        backend: player.backend_name(),
        pending: 0,
        volume_pending: 0,
    };
    // An error the backend found on its own (mpv couldn't play a file), not from a command.
    if let Some(err) = player.take_error() {
        *shared.last_error.lock().unwrap() = Some(err);
    }
    let mut state = shared.state.lock().unwrap();
    state.backend = fresh.backend;
    if state.pending == 0 {
        *state = fresh;
    } else if state.volume_pending == 0 {
        state.volume = fresh.volume;
    }
}

impl AudioPlayer for AsyncPlayer {
    fn play(&mut self, filepath: &Path) -> anyhow::Result<()> {
        self.send(Command::Play(filepath.to_path_buf()), |s| {
            s.playing = true;
            s.paused = false;
            s.position = 0.0;
            s.duration = 0.0;
            s.finished = false;
        })
    }

    fn play_at(&mut self, filepath: &Path, position_seconds: f64) -> anyhow::Result<()> {
        self.send(Command::PlayAt(filepath.to_path_buf(), position_seconds), |s| {
            s.playing = true;
            s.paused = false;
            s.position = position_seconds.max(0.0);
            s.duration = 0.0;
            s.finished = false;
        })
    }

    // Backends report playing=false while paused; the optimistic state mirrors that.
    fn pause(&mut self) -> anyhow::Result<()> {
        self.send(Command::Pause, |s| {
            s.paused = true;
            s.playing = false;
        })
    }

    fn resume(&mut self) -> anyhow::Result<()> {
        self.send(Command::Resume, |s| {
            s.paused = false;
            s.playing = true;
        })
    }

    fn toggle_pause(&mut self) -> anyhow::Result<bool> {
        let mut now_paused = false;
        self.send(Command::TogglePause, |s| {
            if s.playing || s.paused {
                s.paused = !s.paused;
                s.playing = !s.paused;
            }
            now_paused = s.paused;
        })?;
        Ok(now_paused)
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        self.send(Command::Stop, |s| {
            s.playing = false;
            s.paused = false;
        })
    }

    fn seek(&mut self, delta_seconds: f64) -> anyhow::Result<()> {
        self.send(Command::Seek(delta_seconds), |s| {
            let mut target = (s.position + delta_seconds).max(0.0);
            if s.duration > 0.0 {
                target = target.min(s.duration);
            }
            s.position = target;
        })
    }

    fn seek_to(&mut self, position_seconds: f64) -> anyhow::Result<()> {
        self.send(Command::SeekTo(position_seconds), |s| {
            s.position = position_seconds.max(0.0);
        })
    }

    fn set_volume(&mut self, volume: u8) -> anyhow::Result<()> {
        self.send(Command::SetVolume(volume), |s| {
            s.volume = volume;
            s.volume_pending += 1;
        })
    }

    fn volume(&self) -> u8 {
        self.snapshot().volume
    }

    fn is_playing(&self) -> bool {
        self.snapshot().playing
    }

    fn is_paused(&self) -> bool {
        self.snapshot().paused
    }

    fn position_seconds(&self) -> f64 {
        self.snapshot().position
    }

    fn duration_seconds(&self) -> f64 {
        self.snapshot().duration
    }

    fn is_track_finished(&mut self) -> bool {
        self.snapshot().finished
    }

    fn reset_finished(&mut self) {
        let _ = self.send(Command::ResetFinished, |s| s.finished = false);
    }

    fn detach_stream_state(&mut self) {
        let _ = self.send(Command::DetachStreamState, |_| {});
    }

    fn backend_name(&self) -> Option<&'static str> {
        self.shared.state.lock().unwrap().backend
    }

    fn take_error(&mut self) -> Option<String> {
        self.shared.last_error.lock().unwrap().take()
    }
}
