//! Switching output near the end of a track (issue #48): the new output must not be asked to
//! start at or past the track's end, which left mpv stopped and the queue stuck. Inside the last
//! second the track counts as over and the queue moves on as at a natural end.
mod common;

use std::path::{Path, PathBuf};

use common::fake_player::FakePlayer;
use quickstream::discovery::DLNADevice;
use quickstream::metadata::AudioMetadata;
use quickstream::player::{resume_position, Resume};
use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;

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

/// A renderer that accepts connections and never answers, so the new output's worker stays busy
/// and its proxy keeps reporting exactly what the app last asked of it.
fn unresponsive_renderer() -> DLNADevice {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
    });
    DLNADevice {
        udn: format!("uuid:switch-test-{}", port),
        friendly_name: "Switch Test Renderer".to_string(),
        model_name: "Test".to_string(),
        manufacturer: "Test".to_string(),
        host: "127.0.0.1".to_string(),
        port,
        location: format!("http://127.0.0.1:{}/desc.xml", port),
        av_transport_control_url: format!("http://127.0.0.1:{}/ctrl/AVTransport", port),
        rendering_control_url: format!("http://127.0.0.1:{}/ctrl/RenderingControl", port),
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

/// An app in `mode` with `tracks` queued (the first current), its player `player`.
fn app(tag: &str, mode: &str, tracks: &[(&str, f64)], player: FakePlayer) -> (AppUI, PathBuf) {
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
    (app, dir)
}

#[test]
fn test_switch_to_renderer_mid_track_resumes_at_the_position() {
    common::isolate();
    let (mut app, dir) = app(
        "mid",
        "loop",
        &[("First", 180.0), ("Second", 200.0)],
        FakePlayer::playing_at(42.0, 180.0),
    );
    app.switch_to_upnp_player(unresponsive_renderer());
    assert_eq!(app.queue.current_index(), 0);
    assert!(app.player.is_playing());
    assert_eq!(app.player.position_seconds(), 42.0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_to_renderer_in_the_last_second_plays_the_next_track() {
    common::isolate();
    let (mut app, dir) = app(
        "last_second",
        "loop",
        &[("First", 180.0), ("Second", 200.0)],
        FakePlayer::playing_at(179.6, 180.0),
    );
    app.switch_to_upnp_player(unresponsive_renderer());
    assert_eq!(app.queue.current_index(), 1, "the queue moves on to the next track");
    assert!(app.player.is_playing());
    assert!(
        app.player.position_seconds() < 1.0,
        "the next track starts from its beginning, not at {:.1}s",
        app.player.position_seconds()
    );
    assert_eq!(app.status_message, "Active output: Switch Test Renderer");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_past_the_tag_length_uses_the_track_length_when_the_player_has_none() {
    common::isolate();
    // The old output never reported a length: the queue's own length decides.
    let (mut app, dir) = app(
        "tag_length",
        "loop",
        &[("First", 180.0), ("Second", 200.0)],
        FakePlayer::playing_at(185.0, 0.0),
    );
    app.switch_to_upnp_player(unresponsive_renderer());
    assert_eq!(app.queue.current_index(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_with_no_known_length_resumes_at_the_position() {
    common::isolate();
    let (mut app, dir) = app(
        "unknown_length",
        "loop",
        &[("First", 0.0), ("Second", 200.0)],
        FakePlayer::playing_at(185.0, 0.0),
    );
    app.switch_to_upnp_player(unresponsive_renderer());
    assert_eq!(app.queue.current_index(), 0);
    assert_eq!(app.player.position_seconds(), 185.0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_in_the_last_second_in_repeat_one_replays_the_track() {
    common::isolate();
    let (mut app, dir) = app(
        "repeat_one",
        "repeat_one",
        &[("First", 180.0), ("Second", 200.0)],
        FakePlayer::playing_at(179.6, 180.0),
    );
    app.switch_to_upnp_player(unresponsive_renderer());
    assert_eq!(app.queue.current_index(), 0, "repeat-one stays on the track");
    assert!(app.player.is_playing());
    assert!(app.player.position_seconds() < 1.0, "it starts over from 0:00");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_in_the_last_second_of_the_last_track_in_stop_mode_stops() {
    common::isolate();
    let (mut app, dir) = app(
        "stop",
        "stop",
        &[("First", 180.0)],
        FakePlayer::playing_at(179.6, 180.0),
    );
    app.switch_to_upnp_player(unresponsive_renderer());
    assert_eq!(app.queue.current_index(), 0);
    assert!(!app.player.is_playing(), "nothing plays after the end of the queue");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_while_paused_in_the_last_second_loads_the_next_track_paused() {
    common::isolate();
    let mut paused = FakePlayer::playing_at(179.6, 180.0);
    paused.playing = false;
    paused.paused = true;
    let (mut app, dir) = app("paused", "loop", &[("First", 180.0), ("Second", 200.0)], paused);
    app.switch_to_upnp_player(unresponsive_renderer());
    assert_eq!(app.queue.current_index(), 1);
    assert!(app.player.is_paused(), "a paused switch stays paused");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_switch_to_local_in_the_last_second_plays_the_next_track() {
    common::isolate();
    let (mut app, dir) = app(
        "local",
        "loop",
        &[("First", 180.0), ("Second", 200.0)],
        FakePlayer::playing_at(179.6, 180.0),
    );
    app.switch_to_local_player();
    assert_eq!(app.queue.current_index(), 1, "the queue moves on to the next track");
    assert!(app.player.position_seconds() < 1.0);
    assert_eq!(app.status_message, "Active output: Local Audio");
    let _ = std::fs::remove_dir_all(&dir);
}
