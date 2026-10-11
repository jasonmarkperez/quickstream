use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use quickstream::player::local::LocalPlayer;
use quickstream::player::AudioPlayer;

#[test]
fn test_local_playback_lifecycle() {
    let test_file = PathBuf::from("/tmp/test_sine.mp3");
    assert!(test_file.exists(), "test file must exist");

    let mut player = LocalPlayer::new("mpv");
    let res = player.play(&test_file);
    assert!(res.is_ok(), "play failed: {:?}", res);

    // Give mpv a brief moment to decode audio and report position
    thread::sleep(Duration::from_millis(400));
    assert!(player.is_playing(), "player should be playing");
    assert!(!player.is_paused(), "player should not be paused");
    assert!(!player.is_track_finished(), "track should not be finished yet");

    let pos = player.position_seconds();
    println!("Reported position: {:.3}s", pos);
    assert!(pos > 0.0, "Position should be > 0.0s after 400ms of playback");

    // Test pause
    assert!(player.pause().is_ok());
    assert!(player.is_paused());
    assert!(!player.is_playing());

    // Test resume
    assert!(player.resume().is_ok());
    assert!(!player.is_paused());
    assert!(player.is_playing());

    // Test seek
    assert!(player.seek(2.0).is_ok());

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
    let test_file = PathBuf::from("/tmp/test_sine.mp3");
    assert!(test_file.exists(), "test file must exist");

    let mut player = LocalPlayer::new("ffplay");
    let res = player.play(&test_file);
    assert!(res.is_ok(), "ffplay play failed: {:?}", res);
    assert!(player.is_playing());

    thread::sleep(Duration::from_millis(200));
    assert!(player.position_seconds() > 0.0);

    assert!(player.stop().is_ok());
    assert!(!player.is_playing());
}
