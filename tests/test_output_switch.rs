//! Switching output near the end of a track (issue #48): the new output must not be asked to
//! start at or past the track's end, which left mpv stopped and the queue stuck. Inside the last
//! second the track counts as over and the queue moves on as at a natural end.
//!
//! The output a switch moves to is a [`FakePlayer`] (`AppUI::set_output_player_factory`), so
//! these tests start no mpv and talk to no renderer.
mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use common::fake_player::FakePlayer;
use quickstream::discovery::DLNADevice;
use quickstream::metadata::AudioMetadata;
use quickstream::player::{resume_position, Resume};
use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;
use quickstream::web::WebCommand;

#[test]
fn test_resume_position_mid_track_resumes_there() {
    common::isolate();
    assert_eq!(resume_position(42.0, 180.0), Resume::At(42.0));
    assert_eq!(resume_position(178.9, 180.0), Resume::At(178.9));
    assert_eq!(resume_position(0.0, 180.0), Resume::At(0.0));
}

#[test]
fn test_resume_position_in_the_last_second_moves_to_the_next_track() {
    common::isolate();
    assert_eq!(resume_position(179.0, 180.0), Resume::NextTrack);
    assert_eq!(resume_position(179.6, 180.0), Resume::NextTrack);
    assert_eq!(resume_position(180.0, 180.0), Resume::NextTrack);
}

#[test]
fn test_resume_position_past_the_end_moves_to_the_next_track() {
    common::isolate();
    assert_eq!(resume_position(200.0, 180.0), Resume::NextTrack);
}

#[test]
fn test_resume_position_with_unknown_length_passes_the_position_through() {
    common::isolate();
    // The continuous stream can report a length of 0 while it is still unknown.
    assert_eq!(resume_position(200.0, 0.0), Resume::At(200.0));
    assert_eq!(resume_position(200.0, f64::NAN), Resume::At(200.0));
    assert_eq!(resume_position(200.0, -1.0), Resume::At(200.0));
}

#[test]
fn test_resume_position_never_skips_a_track_shorter_than_a_second_from_its_start() {
    common::isolate();
    assert_eq!(resume_position(0.0, 0.5), Resume::At(0.0));
}

#[test]
fn test_resume_position_cleans_up_bad_positions() {
    common::isolate();
    assert_eq!(resume_position(-3.0, 180.0), Resume::At(0.0));
    assert_eq!(resume_position(f64::NAN, 180.0), Resume::At(0.0));
    assert_eq!(resume_position(f64::INFINITY, 180.0), Resume::At(0.0));
}

/// A renderer on loopback. The switch builds its player with the test's factory, so nothing
/// ever connects to it.
fn renderer() -> DLNADevice {
    DLNADevice {
        udn: "uuid:switch-test".to_string(),
        friendly_name: "Switch Test Renderer".to_string(),
        model_name: "Test".to_string(),
        manufacturer: "Test".to_string(),
        host: "127.0.0.1".to_string(),
        port: 9,
        location: "http://127.0.0.1:9/desc.xml".to_string(),
        av_transport_control_url: "http://127.0.0.1:9/ctrl/AVTransport".to_string(),
        rendering_control_url: "http://127.0.0.1:9/ctrl/RenderingControl".to_string(),
    }
}

fn track(dir: &Path, title: &str, secs: f64) -> AudioMetadata {
    AudioMetadata {
        filepath: dir.join(format!("{title}.mp3")),
        filename: format!("{title}.mp3"),
        title: title.to_string(),
        artist: "Mira Lenz".to_string(),
        duration_seconds: secs,
        ..Default::default()
    }
}

/// An app in `mode` with `tracks` queued (the first current), its player `player`. Every output
/// it switches to is a fresh [`FakePlayer`]; the returned log records what they were asked to do.
fn app(tag: &str, mode: &str, tracks: &[(&str, f64)], player: FakePlayer) -> (AppUI, PathBuf, Arc<Mutex<Vec<String>>>) {
    let dir = std::env::temp_dir().join(format!("quickstream_switch_{}_{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut settings_mgr = SettingsManager::with_path(dir.join("config.json"));
    settings_mgr.settings.web_server_enabled = false;
    settings_mgr.settings.web_server_port = 0;
    settings_mgr.settings.queue_mode = mode.to_string();
    let mut app = AppUI::new(Some(dir.clone()), settings_mgr);
    for (title, secs) in tracks {
        app.queue.add_track(track(&dir, title, *secs));
    }
    app.queue.set_current_index(0);
    app.player = Box::new(player);
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let shared = log.clone();
    app.set_output_player_factory(Box::new(move || {
        Box::new(FakePlayer {
            calls: shared.clone(),
            ..FakePlayer::default()
        })
    }));
    (app, dir, log)
}

fn calls(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    log.lock().unwrap().clone()
}

/// `player` paused at `position` seconds into a `duration`-second track.
fn paused_at(position: f64, duration: f64) -> FakePlayer {
    FakePlayer {
        paused: true,
        position,
        duration,
        ..FakePlayer::default()
    }
}

#[test]
fn test_switch_to_renderer_mid_track_resumes_at_the_position() {
    common::isolate();
    let (mut app, dir, log) = app(
        "mid",
        "loop",
        &[("First", 180.0), ("Second", 200.0)],
        FakePlayer::playing_at(42.0, 180.0),
    );
    app.switch_to_upnp_player(renderer());
    assert_eq!(app.queue.current_index(), 0);
    assert_eq!(calls(&log), vec!["play_at First.mp3 42"]);
    assert!(app.player.is_playing());
    assert_eq!(app.status_message, "Active output: Switch Test Renderer");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_to_renderer_in_the_last_second_plays_the_next_track() {
    common::isolate();
    let (mut app, dir, log) = app(
        "last_second",
        "loop",
        &[("First", 180.0), ("Second", 200.0)],
        FakePlayer::playing_at(179.6, 180.0),
    );
    app.switch_to_upnp_player(renderer());
    assert_eq!(app.queue.current_index(), 1, "the queue moves on to the next track");
    assert_eq!(
        calls(&log),
        vec!["play Second.mp3"],
        "the next track starts from its beginning"
    );
    assert!(app.player.is_playing());
    assert_eq!(app.status_message, "Active output: Switch Test Renderer");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_past_the_tag_length_uses_the_track_length_when_the_player_has_none() {
    common::isolate();
    // The old output never reported a length: the queue's own length decides.
    let (mut app, dir, log) = app(
        "tag_length",
        "loop",
        &[("First", 180.0), ("Second", 200.0)],
        FakePlayer::playing_at(185.0, 0.0),
    );
    app.switch_to_upnp_player(renderer());
    assert_eq!(app.queue.current_index(), 1);
    assert_eq!(calls(&log), vec!["play Second.mp3"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_with_no_known_length_resumes_at_the_position() {
    common::isolate();
    let (mut app, dir, log) = app(
        "unknown_length",
        "loop",
        &[("First", 0.0), ("Second", 200.0)],
        FakePlayer::playing_at(185.0, 0.0),
    );
    app.switch_to_upnp_player(renderer());
    assert_eq!(app.queue.current_index(), 0);
    assert_eq!(calls(&log), vec!["play_at First.mp3 185"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_in_the_last_second_in_repeat_one_replays_the_track() {
    common::isolate();
    let (mut app, dir, log) = app(
        "repeat_one",
        "repeat_one",
        &[("First", 180.0), ("Second", 200.0)],
        FakePlayer::playing_at(179.6, 180.0),
    );
    app.switch_to_upnp_player(renderer());
    assert_eq!(app.queue.current_index(), 0, "repeat-one stays on the track");
    assert_eq!(calls(&log), vec!["play First.mp3"], "it starts over from 0:00");
    assert!(app.player.is_playing());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_in_the_last_second_of_the_last_track_in_stop_mode_stops() {
    common::isolate();
    let (mut app, dir, log) = app(
        "stop",
        "stop",
        &[("First", 180.0)],
        FakePlayer::playing_at(179.6, 180.0),
    );
    app.status_message = "Seek +10s (00:02:59)".to_string();
    app.switch_to_upnp_player(renderer());
    assert_eq!(app.queue.current_index(), 0);
    assert!(calls(&log).is_empty(), "nothing plays after the end of the queue");
    assert!(!app.player.is_playing());
    assert_eq!(
        app.status_message, "Active output: Switch Test Renderer",
        "an older status doesn't outlive the switch"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_while_paused_in_the_last_second_loads_the_next_track_paused() {
    common::isolate();
    let (mut app, dir, log) = app(
        "paused",
        "loop",
        &[("First", 180.0), ("Second", 200.0)],
        paused_at(179.6, 180.0),
    );
    app.switch_to_upnp_player(renderer());
    assert_eq!(app.queue.current_index(), 1);
    assert_eq!(calls(&log), vec!["play Second.mp3", "pause"]);
    assert!(app.player.is_paused(), "a paused switch stays paused");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_to_local_in_the_last_second_plays_the_next_track() {
    common::isolate();
    let (mut app, dir, log) = app(
        "local",
        "loop",
        &[("First", 180.0), ("Second", 200.0)],
        FakePlayer::playing_at(179.6, 180.0),
    );
    app.switch_to_local_player();
    assert_eq!(app.queue.current_index(), 1, "the queue moves on to the next track");
    assert_eq!(calls(&log), vec!["play Second.mp3"]);
    assert_eq!(app.status_message, "Active output: Local Audio");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_after_an_unhandled_track_end_plays_the_next_track() {
    common::isolate();
    // The old output reported the track finished (and no longer playing) after the event loop's
    // last check: retiring it must not lose that end.
    let mut ended = FakePlayer::playing_at(180.0, 180.0);
    ended.playing = false;
    ended.finished = true;
    let (mut app, dir, log) = app("ended", "loop", &[("First", 180.0), ("Second", 200.0)], ended);
    app.switch_to_upnp_player(renderer());
    assert_eq!(app.queue.current_index(), 1, "the queue moves on to the next track");
    assert_eq!(calls(&log), vec!["play Second.mp3"]);
    assert!(app.player.is_playing());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_while_stopped_near_the_end_starts_nothing() {
    common::isolate();
    let mut stopped = FakePlayer::playing_at(179.6, 180.0);
    stopped.playing = false;
    let (mut app, dir, log) = app("stopped", "loop", &[("First", 180.0), ("Second", 200.0)], stopped);
    app.switch_to_upnp_player(renderer());
    assert_eq!(app.queue.current_index(), 0);
    assert!(
        calls(&log).is_empty(),
        "a stopped player stays stopped on the new output"
    );
    assert!(!app.player.is_playing());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// A debounced skip or seek still pending when the output changes (PR #76 review)
// ---------------------------------------------------------------------------------------------

#[test]
fn test_switch_with_a_pending_skip_plays_the_skips_target() {
    common::isolate();
    // Paused in the last second, then Next: the queue already points at Second, but the player
    // still holds First at 179.6 s. The skip takes effect on the new output; it must not count
    // First's end on top of it and play Third.
    let (mut app, dir, log) = app(
        "pending_skip",
        "loop",
        &[("First", 180.0), ("Second", 200.0), ("Third", 210.0)],
        paused_at(179.6, 180.0),
    );
    app.skip_next_track_debounced();
    assert!(app.pending_track_skip);
    app.switch_to_upnp_player(renderer());
    assert_eq!(app.queue.current_index(), 1, "the skip's target is current");
    assert_eq!(
        calls(&log),
        vec!["play_at Second.mp3 0"],
        "the target plays from its start"
    );
    assert!(!app.pending_track_skip, "the skip was applied");
    assert!(app.player.is_playing(), "an applied skip plays, as on any other key");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_with_a_pending_skip_while_playing_plays_the_skips_target() {
    common::isolate();
    // A skip while playing stops the old player at once: the switch still plays the target.
    let (mut app, dir, log) = app(
        "pending_skip_playing",
        "loop",
        &[("First", 180.0), ("Second", 200.0), ("Third", 210.0)],
        FakePlayer::playing_at(42.0, 180.0),
    );
    app.skip_next_track_debounced();
    app.switch_to_local_player();
    assert_eq!(app.queue.current_index(), 1);
    assert_eq!(calls(&log), vec!["play_at Second.mp3 0"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_with_a_pending_seek_back_resumes_at_the_seek_target() {
    common::isolate();
    // In the last second, then seek back 30 s (still debounced): the track is not over.
    let (mut app, dir, log) = app(
        "pending_seek",
        "loop",
        &[("First", 180.0), ("Second", 200.0)],
        FakePlayer::playing_at(179.5, 180.0),
    );
    app.accumulate_seek(-30.0);
    assert_eq!(app.pending_seek_target, Some(149.5));
    app.switch_to_upnp_player(renderer());
    assert_eq!(app.queue.current_index(), 0);
    assert_eq!(calls(&log), vec!["play_at First.mp3 149.5"]);
    assert_eq!(app.pending_seek_target, None, "the seek was applied");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_with_a_pending_seek_into_the_last_second_plays_the_next_track() {
    common::isolate();
    let (mut app, dir, log) = app(
        "pending_seek_end",
        "loop",
        &[("First", 180.0), ("Second", 200.0)],
        FakePlayer::playing_at(170.0, 180.0),
    );
    app.accumulate_seek(10.0);
    app.switch_to_upnp_player(renderer());
    assert_eq!(app.queue.current_index(), 1);
    assert_eq!(calls(&log), vec!["play Second.mp3"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Sends `cmd` to `app` as the web dashboard would and runs the UI's handler.
fn web_command(app: &mut AppUI, cmd: WebCommand) {
    let (tx, rx) = std::sync::mpsc::channel();
    app.web_cmd_rx = Some(rx);
    tx.send(cmd).unwrap();
    app.handle_web_commands();
}

#[test]
fn test_web_switch_with_a_pending_skip_plays_the_skips_target() {
    common::isolate();
    let (mut app, dir, log) = app(
        "web_pending_skip",
        "loop",
        &[("First", 180.0), ("Second", 200.0), ("Third", 210.0)],
        paused_at(179.6, 180.0),
    );
    app.skip_next_track_debounced();
    web_command(&mut app, WebCommand::SelectDevice(0));
    assert_eq!(app.queue.current_index(), 1);
    assert_eq!(calls(&log), vec!["play_at Second.mp3 0"]);
    assert_eq!(app.status_message, "Switched to Local Audio (web)");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_web_switch_with_a_pending_seek_resumes_at_the_seek_target() {
    common::isolate();
    let (mut app, dir, log) = app(
        "web_pending_seek",
        "loop",
        &[("First", 180.0), ("Second", 200.0)],
        FakePlayer::playing_at(179.5, 180.0),
    );
    app.accumulate_seek(-30.0);
    web_command(&mut app, WebCommand::SelectDevice(0));
    assert_eq!(app.queue.current_index(), 0);
    assert_eq!(calls(&log), vec!["play_at First.mp3 149.5"]);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// The player no longer holds the queue's current track (PR #76 review)
// ---------------------------------------------------------------------------------------------

/// Plays the queue's current track through the app (so it knows what the player holds), then
/// stands in `later` for the player as it is some time on.
fn play_current_then(app: &mut AppUI, later: FakePlayer) {
    app.play_current_track();
    app.player = Box::new(later);
}

#[test]
fn test_switch_after_the_ended_current_track_was_removed_plays_the_new_current_track() {
    common::isolate();
    // First ended (the event loop hasn't handled it yet) and was then removed: Second took its
    // place. First's end must not move the queue past Second.
    let (mut app, dir, log) = app(
        "removed",
        "loop",
        &[("First", 180.0), ("Second", 200.0), ("Third", 210.0)],
        FakePlayer::default(),
    );
    let mut ended = FakePlayer::playing_at(180.0, 180.0);
    ended.playing = false;
    ended.finished = true;
    play_current_then(&mut app, ended);
    app.remove_queue_track(0);
    assert_eq!(app.queue.current_track().unwrap().title, "Second");
    app.switch_to_upnp_player(renderer());
    assert_eq!(
        app.queue.current_track().unwrap().title,
        "Second",
        "Second is not skipped"
    );
    assert_eq!(calls(&log), vec!["play_at Second.mp3 0"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_after_the_queue_was_cleared_and_refilled_plays_the_new_current_track() {
    common::isolate();
    // `c` clears the queue while First plays on, near its end; then new tracks are queued.
    // First's position and length say nothing about New One.
    let (mut app, dir, log) = app("refilled", "loop", &[("First", 180.0)], FakePlayer::default());
    play_current_then(&mut app, FakePlayer::playing_at(179.6, 180.0));
    app.queue.clear();
    app.queue.add_track(track(&dir, "New One", 240.0));
    app.queue.add_track(track(&dir, "New Two", 250.0));
    app.queue.set_current_index(0);
    app.switch_to_upnp_player(renderer());
    assert_eq!(app.queue.current_index(), 0, "New One is not skipped");
    assert_eq!(
        calls(&log),
        vec!["play_at New One.mp3 0"],
        "New One plays from its start"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_keeps_the_playback_error_of_the_next_track() {
    common::isolate();
    // Moving on to the next track failed on the new output: the switch keeps that error in the
    // status instead of covering it with "Active output: …".
    let (mut app, dir, _log) = app(
        "next_error",
        "loop",
        &[("First", 180.0), ("Second", 200.0)],
        FakePlayer::playing_at(179.6, 180.0),
    );
    app.set_output_player_factory(Box::new(|| Box::new(FailingPlayer::default())));
    app.switch_to_upnp_player(renderer());
    assert_eq!(app.queue.current_index(), 1);
    assert_eq!(app.status_message, "Playback error: renderer unreachable");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A player whose `play` always fails.
#[derive(Default)]
struct FailingPlayer(FakePlayer);

impl quickstream::player::AudioPlayer for FailingPlayer {
    fn play(&mut self, _filepath: &Path) -> anyhow::Result<()> {
        anyhow::bail!("renderer unreachable")
    }
    fn pause(&mut self) -> anyhow::Result<()> {
        self.0.pause()
    }
    fn resume(&mut self) -> anyhow::Result<()> {
        self.0.resume()
    }
    fn toggle_pause(&mut self) -> anyhow::Result<bool> {
        self.0.toggle_pause()
    }
    fn stop(&mut self) -> anyhow::Result<()> {
        self.0.stop()
    }
    fn seek(&mut self, delta_seconds: f64) -> anyhow::Result<()> {
        self.0.seek(delta_seconds)
    }
    fn seek_to(&mut self, position_seconds: f64) -> anyhow::Result<()> {
        self.0.seek_to(position_seconds)
    }
    fn set_volume(&mut self, volume: u8) -> anyhow::Result<()> {
        self.0.set_volume(volume)
    }
    fn volume(&self) -> u8 {
        self.0.volume()
    }
    fn is_playing(&self) -> bool {
        self.0.is_playing()
    }
    fn is_paused(&self) -> bool {
        self.0.is_paused()
    }
    fn position_seconds(&self) -> f64 {
        self.0.position_seconds()
    }
    fn duration_seconds(&self) -> f64 {
        self.0.duration_seconds()
    }
}
