mod common;

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use quickstream::player::local::LocalPlayer;
use quickstream::player::AudioPlayer;

/// 10s of silence, so running the suite never makes audible noise.
fn ensure_test_file() -> PathBuf {
    let path = std::env::temp_dir().join("quickstream_test_silence_10s.mp3");
    if !path.exists() {
        let _ = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=44100:cl=stereo",
                "-t",
                "10",
                path.to_str().unwrap(),
            ])
            .output();
    }
    path
}

/// Poll `cond` every 20 ms until it holds or `timeout` has passed. Returns whether it held.
fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// The mpv lifecycle test needs mpv itself: without it LocalPlayer falls back to ffplay, which
/// exits at once on machines without an audio device (CI runners).
fn mpv_available() -> bool {
    std::process::Command::new("mpv").arg("--version").output().is_ok()
}

#[test]
fn test_local_playback_lifecycle() {
    common::isolate();
    if !mpv_available() {
        eprintln!("SKIPPED: mpv not installed");
        return;
    }
    let test_file = ensure_test_file();
    assert!(test_file.exists(), "test file must exist");

    let mut player = LocalPlayer::new("mpv");
    let res = player.play(&test_file);
    assert!(res.is_ok(), "play failed: {:?}", res);

    // mpv reports a position once it has decoded some audio; on a loaded machine that can take
    // well over the 400 ms this test used to sleep (#89).
    let started = wait_until(Duration::from_secs(10), || player.position_seconds() > 0.0);
    let pos = player.position_seconds();
    println!("Reported position: {:.3}s", pos);
    assert!(started, "mpv never reported a position");
    assert!(player.is_playing(), "player should be playing");
    assert!(!player.is_paused(), "player should not be paused");
    assert!(!player.is_track_finished(), "track should not be finished yet");

    // Test pause
    assert!(player.pause().is_ok());
    assert!(player.is_paused());
    assert!(!player.is_playing());

    // Test resume
    assert!(player.resume().is_ok());
    assert!(!player.is_paused());
    assert!(player.is_playing());

    // Test seek forward
    assert!(player.seek_to(4.0).is_ok());
    wait_until(Duration::from_secs(5), || {
        (3.8..=4.5).contains(&player.position_seconds())
    });
    let pos_after_seek = player.position_seconds();
    assert!(
        (3.8..=4.5).contains(&pos_after_seek),
        "Position after seek_to(4.0) should be around 4.0s, got {:.3}s",
        pos_after_seek
    );

    // Test relative seek backward
    assert!(player.seek(-1.5).is_ok());
    wait_until(Duration::from_secs(5), || {
        (2.0..=3.3).contains(&player.position_seconds())
    });
    let pos_after_rel = player.position_seconds();
    assert!(
        (2.0..=3.3).contains(&pos_after_rel),
        "Position after seek(-1.5) should be around 2.5s, got {:.3}s",
        pos_after_rel
    );

    // Test seek while paused
    assert!(player.pause().is_ok());
    assert!(player.is_paused());
    assert!(player.seek_to(6.0).is_ok());
    assert!(
        player.position_seconds() >= 5.8,
        "Position when seeking while paused should be ~6.0s, got {:.3}s",
        player.position_seconds()
    );
    assert!(
        player.is_paused(),
        "Player should remain paused after seeking while paused"
    );
    assert!(player.resume().is_ok());
    assert!(player.is_playing());

    // Test volume
    assert!(player.set_volume(80).is_ok());
    assert_eq!(player.volume(), 80);

    // Stop
    assert!(player.stop().is_ok());
    assert!(!player.is_playing());
    assert!(!player.is_paused());
}

#[test]
fn test_ffplay_backend_lifecycle() {
    common::isolate();
    let test_file = ensure_test_file();
    assert!(test_file.exists(), "test file must exist");

    let mut player = LocalPlayer::new("ffplay");
    let res = player.play(&test_file);
    assert!(res.is_ok(), "ffplay play failed: {:?}", res);
    assert!(player.is_playing());

    thread::sleep(Duration::from_millis(200));
    assert!(player.position_seconds() > 0.0);

    // Test seek while playing on ffplay
    assert!(player.seek_to(3.5).is_ok());
    thread::sleep(Duration::from_millis(200));
    let ffplay_pos = player.position_seconds();
    assert!(
        (3.4..=4.5).contains(&ffplay_pos),
        "ffplay position after seek_to(3.5) should be >= 3.4, got {:.3}s",
        ffplay_pos
    );

    // Test seek while paused on ffplay
    assert!(player.pause().is_ok());
    assert!(player.is_paused());
    assert!(player.seek_to(5.0).is_ok());
    assert_eq!(
        player.position_seconds(),
        5.0,
        "ffplay position_seconds while paused should match seek target exactly"
    );

    // Test resume after seeking while paused
    assert!(player.resume().is_ok());
    assert!(player.is_playing());
    thread::sleep(Duration::from_millis(200));
    assert!(
        player.position_seconds() >= 5.0,
        "ffplay position after resume should advance from sought offset"
    );

    assert!(player.stop().is_ok());
    assert!(!player.is_playing());
}

#[test]
fn test_local_player_reports_the_backend_it_runs() {
    common::isolate();
    assert_eq!(LocalPlayer::new("ffplay").backend_name(), Some("ffplay"));

    // mpv configured but missing: the configured backend until a track plays, then ffplay.
    let mut player = LocalPlayer::with_mpv_binary("mpv", "/nonexistent/quickstream-test-mpv");
    assert_eq!(player.backend_name(), Some("mpv"));
    let test_file = ensure_test_file();
    assert!(test_file.exists(), "test file must exist");
    let res = player.play(&test_file);
    assert!(res.is_ok(), "ffplay fallback failed: {:?}", res);
    assert_eq!(player.backend_name(), Some("ffplay"), "mpv is missing, so ffplay plays");
    let _ = player.stop();
}
