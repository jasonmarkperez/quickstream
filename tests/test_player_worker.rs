mod common;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quickstream::player::worker::AsyncPlayer;
use quickstream::player::AudioPlayer;

/// Test double that blocks on every command, like a slow DLNA renderer.
struct SlowPlayer {
    delay: Duration,
    calls: Arc<Mutex<Vec<String>>>,
    finished: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
    fail_play: bool,
    playing: bool,
    paused: bool,
    position: f64,
    volume: u8,
}

#[derive(Clone, Default)]
struct Probe {
    calls: Arc<Mutex<Vec<String>>>,
    finished: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

impl SlowPlayer {
    fn new(delay: Duration, probe: &Probe) -> Self {
        Self {
            delay,
            calls: probe.calls.clone(),
            finished: probe.finished.clone(),
            dropped: probe.dropped.clone(),
            fail_play: false,
            playing: false,
            paused: false,
            position: 0.0,
            volume: 50,
        }
    }

    fn record(&self, call: &str) {
        std::thread::sleep(self.delay);
        self.calls.lock().unwrap().push(call.to_string());
    }
}

impl Drop for SlowPlayer {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

impl AudioPlayer for SlowPlayer {
    fn play(&mut self, _filepath: &Path) -> anyhow::Result<()> {
        self.record("play");
        if self.fail_play {
            anyhow::bail!("renderer rejected SetAVTransportURI");
        }
        self.playing = true;
        self.paused = false;
        self.position = 0.0;
        Ok(())
    }
    fn play_at(&mut self, _filepath: &Path, position_seconds: f64) -> anyhow::Result<()> {
        self.record(&format!("play_at {}", position_seconds));
        self.playing = true;
        self.paused = false;
        self.position = position_seconds;
        Ok(())
    }
    fn pause(&mut self) -> anyhow::Result<()> {
        self.record("pause");
        self.paused = true;
        Ok(())
    }
    fn resume(&mut self) -> anyhow::Result<()> {
        self.record("resume");
        self.paused = false;
        Ok(())
    }
    fn toggle_pause(&mut self) -> anyhow::Result<bool> {
        self.record("toggle_pause");
        self.paused = !self.paused;
        Ok(self.paused)
    }
    fn stop(&mut self) -> anyhow::Result<()> {
        self.record("stop");
        self.playing = false;
        self.paused = false;
        Ok(())
    }
    fn seek(&mut self, delta_seconds: f64) -> anyhow::Result<()> {
        self.record("seek");
        self.position = (self.position + delta_seconds).max(0.0);
        Ok(())
    }
    fn seek_to(&mut self, position_seconds: f64) -> anyhow::Result<()> {
        self.record("seek_to");
        self.position = position_seconds;
        Ok(())
    }
    fn set_volume(&mut self, volume: u8) -> anyhow::Result<()> {
        self.record(&format!("set_volume {}", volume));
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
        200.0
    }
    fn is_track_finished(&mut self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }
    fn reset_finished(&mut self) {
        self.finished.store(false, Ordering::SeqCst);
    }
}

fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cond()
}

#[test]
fn test_async_player_commands_do_not_block_caller() {
    common::isolate();
    let probe = Probe::default();
    let p = probe.clone();
    let mut player = AsyncPlayer::spawn(move || Box::new(SlowPlayer::new(Duration::from_millis(400), &p)), 50);

    let start = Instant::now();
    player.play(Path::new("/music/a.flac")).unwrap();
    player.pause().unwrap();
    player.set_volume(30).unwrap();
    assert!(
        start.elapsed() < Duration::from_millis(100),
        "commands blocked the caller for {:?}",
        start.elapsed()
    );
}

#[test]
fn test_async_player_constructs_inner_player_off_caller_thread() {
    common::isolate();
    let probe = Probe::default();
    let p = probe.clone();
    let start = Instant::now();
    let _player = AsyncPlayer::spawn(
        move || {
            std::thread::sleep(Duration::from_millis(400));
            Box::new(SlowPlayer::new(Duration::ZERO, &p))
        },
        50,
    );
    assert!(start.elapsed() < Duration::from_millis(100));
}

#[test]
fn test_async_player_applies_commands_in_order() {
    common::isolate();
    let probe = Probe::default();
    let p = probe.clone();
    let mut player = AsyncPlayer::spawn(move || Box::new(SlowPlayer::new(Duration::from_millis(20), &p)), 50);

    player.play_at(Path::new("/music/a.flac"), 42.0).unwrap();
    player.pause().unwrap();
    player.set_volume(30).unwrap();

    assert!(wait_until(Duration::from_secs(2), || probe.calls.lock().unwrap().len() == 3));
    assert_eq!(
        *probe.calls.lock().unwrap(),
        vec!["play_at 42".to_string(), "pause".to_string(), "set_volume 30".to_string()]
    );
    assert!(wait_until(Duration::from_secs(1), || player.is_paused()));
    assert!((player.position_seconds() - 42.0).abs() < 0.01);
}

#[test]
fn test_async_player_state_reflects_commands_immediately() {
    common::isolate();
    let probe = Probe::default();
    let p = probe.clone();
    let mut player = AsyncPlayer::spawn(move || Box::new(SlowPlayer::new(Duration::from_millis(300), &p)), 50);

    player.play(Path::new("/music/a.flac")).unwrap();
    assert!(player.is_playing());
    assert!(!player.is_paused());

    player.set_volume(30).unwrap();
    assert_eq!(player.volume(), 30);

    assert!(player.toggle_pause().unwrap());
    assert!(player.is_paused());

    player.seek_to(90.0).unwrap();
    assert!((player.position_seconds() - 90.0).abs() < 0.01);

    player.stop().unwrap();
    assert!(!player.is_playing());
}

#[test]
fn test_async_player_reports_errors_via_take_error() {
    common::isolate();
    let probe = Probe::default();
    let p = probe.clone();
    let mut player = AsyncPlayer::spawn(
        move || {
            let mut inner = SlowPlayer::new(Duration::ZERO, &p);
            inner.fail_play = true;
            Box::new(inner)
        },
        50,
    );

    assert!(player.play(Path::new("/music/a.flac")).is_ok());
    let mut err = None;
    assert!(wait_until(Duration::from_secs(2), || {
        err = player.take_error();
        err.is_some()
    }));
    assert!(err.unwrap().contains("rejected"));
    assert!(player.take_error().is_none());
}

#[test]
fn test_async_player_track_finished_round_trip() {
    common::isolate();
    let probe = Probe::default();
    let p = probe.clone();
    let mut player = AsyncPlayer::spawn(move || Box::new(SlowPlayer::new(Duration::ZERO, &p)), 50);

    assert!(!player.is_track_finished());
    probe.finished.store(true, Ordering::SeqCst);
    assert!(wait_until(Duration::from_secs(2), || player.is_track_finished()));

    player.reset_finished();
    assert!(!player.is_track_finished());
    assert!(wait_until(Duration::from_secs(2), || !probe.finished.load(Ordering::SeqCst)));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!player.is_track_finished());
}

#[test]
fn test_async_player_drop_releases_inner_player() {
    common::isolate();
    let probe = Probe::default();
    let p = probe.clone();
    let player = AsyncPlayer::spawn(move || Box::new(SlowPlayer::new(Duration::ZERO, &p)), 50);
    std::thread::sleep(Duration::from_millis(50));

    drop(player);
    assert!(
        probe.dropped.load(Ordering::SeqCst),
        "inner player (which kills mpv / stops the renderer) must be dropped before AsyncPlayer::drop returns"
    );
}

/// Accepts TCP connections and never answers, like a hung DLNA renderer.
fn spawn_unresponsive_renderer() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
    });
    port
}

#[test]
fn test_switching_to_unresponsive_renderer_does_not_block_ui() {
    common::isolate();
    use quickstream::discovery::DLNADevice;
    use quickstream::settings::SettingsManager;
    use quickstream::ui::AppUI;

    let port = spawn_unresponsive_renderer();
    let device = DLNADevice {
        udn: "uuid:hung-renderer".to_string(),
        friendly_name: "Hung Renderer".to_string(),
        model_name: "Hung".to_string(),
        manufacturer: "Test".to_string(),
        host: "127.0.0.1".to_string(),
        port,
        location: format!("http://127.0.0.1:{}/desc.xml", port),
        av_transport_control_url: format!("http://127.0.0.1:{}/ctrl/AVTransport", port),
        rendering_control_url: format!("http://127.0.0.1:{}/ctrl/RenderingControl", port),
    };

    let mut settings_mgr = SettingsManager::new();
    settings_mgr.settings.web_server_enabled = false;
    let mut app = AppUI::new(None, settings_mgr);

    let start = Instant::now();
    app.switch_to_upnp_player(device);
    app.player.set_volume(40).unwrap();
    app.player.play(Path::new("/music/a.flac")).unwrap();
    app.player.stop().unwrap();
    let elapsed = start.elapsed();
    assert!(elapsed < Duration::from_millis(500), "UI thread blocked for {:?}", elapsed);
}

#[test]
fn test_async_player_reports_backend_volume_while_commands_are_pending() {
    common::isolate();
    // Switching to a renderer: the proxy is created with a placeholder volume and play_at is
    // queued at once. The renderer's real volume must be visible as soon as the player exists,
    // or a volume key would send placeholder ± step to the speaker.
    let probe = Probe::default();
    let p = probe.clone();
    let mut player = AsyncPlayer::spawn(
        move || {
            let mut inner = SlowPlayer::new(Duration::from_millis(800), &p);
            inner.volume = 20;
            Box::new(inner)
        },
        80,
    );
    player.play_at(Path::new("/music/a.flac"), 10.0).unwrap();

    assert!(wait_until(Duration::from_millis(400), || player.volume() == 20));
    assert!(player.is_playing(), "optimistic play state is kept while play_at is pending");
}

#[test]
fn test_async_player_pending_volume_change_is_not_overwritten() {
    common::isolate();
    let probe = Probe::default();
    let p = probe.clone();
    let mut player = AsyncPlayer::spawn(
        move || {
            let mut inner = SlowPlayer::new(Duration::from_millis(400), &p);
            inner.volume = 20;
            Box::new(inner)
        },
        20,
    );
    player.play(Path::new("/music/a.flac")).unwrap();
    player.set_volume(35).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(player.volume(), 35);
}

#[test]
fn test_async_player_optimistic_pause_matches_backends() {
    common::isolate();
    let probe = Probe::default();
    let p = probe.clone();
    let mut player = AsyncPlayer::spawn(move || Box::new(SlowPlayer::new(Duration::from_millis(300), &p)), 50);
    player.play(Path::new("/music/a.flac")).unwrap();

    player.pause().unwrap();
    assert!(!player.is_playing(), "backends report playing=false while paused");
    assert!(player.is_paused());

    player.resume().unwrap();
    assert!(player.is_playing());
    assert!(!player.is_paused());

    assert!(player.toggle_pause().unwrap());
    assert!(!player.is_playing());
    assert!(!player.toggle_pause().unwrap());
    assert!(player.is_playing());
}

fn hung_device(port: u16, udn: &str) -> quickstream::discovery::DLNADevice {
    quickstream::discovery::DLNADevice {
        udn: udn.to_string(),
        friendly_name: "Hung Renderer".to_string(),
        model_name: "Hung".to_string(),
        manufacturer: "Test".to_string(),
        host: "127.0.0.1".to_string(),
        port,
        location: format!("http://127.0.0.1:{}/desc.xml", port),
        av_transport_control_url: format!("http://127.0.0.1:{}/ctrl/AVTransport", port),
        rendering_control_url: format!("http://127.0.0.1:{}/ctrl/RenderingControl", port),
    }
}

#[test]
fn test_switching_to_renderer_does_not_carry_local_volume() {
    common::isolate();
    use quickstream::settings::SettingsManager;
    use quickstream::ui::AppUI;

    let port = spawn_unresponsive_renderer();
    let mut settings_mgr = SettingsManager::new();
    settings_mgr.settings.web_server_enabled = false;
    let mut app = AppUI::new(None, settings_mgr);
    app.player.set_volume(80).unwrap();

    app.switch_to_upnp_player(hung_device(port, "uuid:hung-volume"));
    // The renderer's own volume is unknown until it answers; a volume key in this window
    // must not send 80 ± step to the speaker.
    assert_eq!(app.player.volume(), 0);
}

#[test]
fn test_reselecting_active_renderer_keeps_current_player() {
    common::isolate();
    use quickstream::settings::SettingsManager;
    use quickstream::ui::AppUI;

    let port = spawn_unresponsive_renderer();
    let mut settings_mgr = SettingsManager::new();
    settings_mgr.settings.web_server_enabled = false;
    let mut app = AppUI::new(None, settings_mgr);

    app.switch_to_upnp_player(hung_device(port, "uuid:hung-same"));
    let first = &*app.player as *const dyn quickstream::player::AudioPlayer as *const u8;
    app.switch_to_upnp_player(hung_device(port, "uuid:hung-same"));
    let second = &*app.player as *const dyn quickstream::player::AudioPlayer as *const u8;
    assert_eq!(first, second, "re-selecting the active renderer must not restart playback on it");
}
