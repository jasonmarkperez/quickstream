//! A seek's target is never overwritten by a position mpv reported before it handled the seek
//! (issue #92).
//!
//! Each test runs a stand-in mpv that, on a seek, first reports the old position (as an event
//! already on its way when the seek was sent would), then answers the seek in the way the test
//! names: confirmed (`seek`, `playback-restart`, then the new position), refused, never answered,
//! or ending the file.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use quickstream::player::local::LocalPlayer;
use quickstream::player::AudioPlayer;

/// Where the fake mpv starts, and so the position it reports as stale on a seek.
const START: f64 = 5.0;
/// mpv's first report after a load is half a second past the start.
const PLAYING_AT: f64 = START + 0.5;

fn has(binary: &str) -> bool {
    std::process::Command::new(binary).arg("-V").output().is_ok()
}

/// Fresh scratch dir unique to this process and test.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join(format!("qs-seek-{}", std::process::id()))
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A stand-in mpv. A load answers with its playlist entry and reports the start (from the
/// `start` property), then half a second past it. A seek first reports the position the fake was
/// at (stale), waits 300 ms, then follows `mode`:
/// - `confirm`: the answer, `seek`, the target, `playback-restart`, then the target + 1 s (as mpv
///   0.41 does, playing or paused);
/// - `fail`: an error answer (as mpv gives for a seek with no file loaded), then the old
///   position + 2 s;
/// - `silent`: no answer or event, then 300 ms later the old position + 2.5 s;
/// - `end`: the answer, `seek`, `end-file` (eof), then 100 ms later 9.0 s.
fn fake_mpv(dir: &Path, mode: &str) -> PathBuf {
    let script = dir.join("fake-mpv");
    let body = r#"#!/usr/bin/env python3
# (no extra shebang arguments: Linux passes "python3 -I" to env as one word)
import json, socket, sys, time
MODE = "@MODE@"
sock = next(a.split("=", 1)[1] for a in sys.argv if a.startswith("--input-ipc-server="))
srv = socket.socket(socket.AF_UNIX)
srv.bind(sock)
srv.listen(4)

def send(conn, msg):
    conn.sendall((json.dumps(msg) + "\n").encode())

def time_pos(conn, value):
    send(conn, {"event": "property-change", "id": 1, "name": "time-pos", "data": value})

while True:
    conn, _ = srv.accept()
    buf = b""
    start = 0.0
    entry = 0
    pos = 0.0
    while True:
        data = conn.recv(65536)
        if not data:
            break
        buf += data
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            if not line.strip():
                continue
            req = json.loads(line)
            cmd = req.get("command", [])
            rid = req.get("request_id", 0)
            name = cmd[0] if cmd else ""
            if name == "quit":
                sys.exit(0)
            if name == "set_property" and cmd[1] == "start":
                start = 0.0 if cmd[2] == "none" else float(cmd[2])
            if name == "loadfile":
                entry += 1
                send(conn, {"request_id": rid, "error": "success", "data": {"playlist_entry_id": entry}})
                send(conn, {"event": "start-file", "playlist_entry_id": entry})
                send(conn, {"event": "file-loaded"})
                send(conn, {"event": "property-change", "id": 4, "name": "duration", "data": 60.0})
                time_pos(conn, start)
                send(conn, {"event": "playback-restart"})
                pos = start + 0.5
                time_pos(conn, pos)
                continue
            if name == "seek":
                target = float(cmd[1])
                time.sleep(0.05)
                time_pos(conn, pos)
                time.sleep(0.3)
                if MODE == "confirm":
                    send(conn, {"request_id": rid, "error": "success", "data": None})
                    send(conn, {"event": "seek"})
                    time_pos(conn, target)
                    send(conn, {"event": "playback-restart"})
                    pos = target + 1.0
                    time_pos(conn, pos)
                elif MODE == "fail":
                    send(conn, {"request_id": rid, "error": "error running command"})
                    pos += 2.0
                    time_pos(conn, pos)
                elif MODE == "silent":
                    time.sleep(0.3)
                    pos += 2.5
                    time_pos(conn, pos)
                elif MODE == "end":
                    send(conn, {"request_id": rid, "error": "success", "data": None})
                    send(conn, {"event": "seek"})
                    send(conn, {"event": "end-file", "reason": "eof", "playlist_entry_id": entry})
                    time.sleep(0.1)
                    pos = 9.0
                    time_pos(conn, pos)
                continue
            send(conn, {"request_id": rid, "error": "success"})
"#
    .replace("@MODE@", mode);
    common::fake_exec::write_script(&script, &body);
    script
}

/// A player on a fake mpv in `mode`, playing a (fake) track from [`START`], once the fake has
/// reported [`PLAYING_AT`].
fn playing(dir: &Path, mode: &str) -> LocalPlayer {
    let fake = fake_mpv(dir, mode);
    let track = dir.join("track.mp3");
    std::fs::write(&track, b"not audio").unwrap();
    let mut player = LocalPlayer::with_mpv_binary("mpv", &fake);
    player.play_at(&track, START).expect("play_at");
    let positions = watch(&player, Duration::from_secs(5), |p| p == PLAYING_AT);
    assert_eq!(
        positions.last(),
        Some(&PLAYING_AT),
        "fake mpv never reported: {:?}",
        positions
    );
    player
}

/// Every position `player` reads (each change once) until `done` holds for one or `limit` runs
/// out.
fn watch(player: &LocalPlayer, limit: Duration, done: impl Fn(f64) -> bool) -> Vec<f64> {
    let deadline = Instant::now() + limit;
    let mut seen: Vec<f64> = Vec::new();
    loop {
        let pos = player.position_seconds();
        if seen.last() != Some(&pos) {
            seen.push(pos);
        }
        if done(pos) || Instant::now() > deadline {
            return seen;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// `seek_to` only writes the command: it never waits for mpv (the fake takes 350 ms to answer).
fn seek_to(player: &mut LocalPlayer, target: f64) {
    let started = Instant::now();
    player.seek_to(target).expect("seek_to");
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "seek_to waited {:?} for mpv",
        started.elapsed()
    );
}

#[test]
fn test_mpv_position_before_seek_confirmed_is_ignored() {
    common::isolate();
    if !has("python3") {
        eprintln!("SKIPPED: python3 not installed");
        return;
    }
    let dir = scratch_dir("confirm");
    let mut player = playing(&dir, "confirm");

    seek_to(&mut player, 30.0);
    let positions = watch(&player, Duration::from_secs(3), |p| p == 31.0);
    // The stale 5.5 s mpv sent before handling the seek never shows; mpv's reports after its
    // playback-restart do.
    assert_eq!(positions, vec![30.0, 31.0], "positions after seek_to(30)");

    // A relative seek starts from the target, not from the stale position.
    player.seek(-10.0).expect("seek");
    let positions = watch(&player, Duration::from_secs(3), |p| p == 22.0);
    assert_eq!(positions, vec![21.0, 22.0], "positions after seek(-10) from 31 s");

    drop(player);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_mpv_consecutive_seeks_keep_the_newest_target() {
    common::isolate();
    if !has("python3") {
        eprintln!("SKIPPED: python3 not installed");
        return;
    }
    let dir = scratch_dir("consecutive");
    let mut player = playing(&dir, "confirm");

    // The second seek is sent before mpv answers the first, so the first's answer, restart and
    // new position (31 s) all arrive after it, then the second's stale report (31 s again).
    seek_to(&mut player, 30.0);
    seek_to(&mut player, 40.0);
    let positions = watch(&player, Duration::from_secs(3), |p| p == 41.0);
    assert_eq!(positions, vec![40.0, 41.0], "positions after seek_to(30), seek_to(40)");

    drop(player);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_mpv_refused_seek_accepts_positions_again() {
    common::isolate();
    if !has("python3") {
        eprintln!("SKIPPED: python3 not installed");
        return;
    }
    let dir = scratch_dir("fail");
    let mut player = playing(&dir, "fail");

    // mpv answers with an error and no restart follows: its next report (7.5 s) is where it
    // really is, well before the time bound.
    seek_to(&mut player, 30.0);
    let positions = watch(&player, Duration::from_millis(1500), |p| p == 7.5);
    assert_eq!(positions, vec![30.0, 7.5], "positions after a refused seek");

    drop(player);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_mpv_unanswered_seek_accepts_positions_after_the_time_bound() {
    common::isolate();
    if !has("python3") {
        eprintln!("SKIPPED: python3 not installed");
        return;
    }
    let dir = scratch_dir("silent");
    let mut player = playing(&dir, "silent");
    player.set_seek_confirm_timeout(Duration::from_millis(300));

    // mpv never answers: the stale report at 50 ms is ignored, the one at 650 ms (8.0 s), past
    // the bound, is taken.
    seek_to(&mut player, 30.0);
    let positions = watch(&player, Duration::from_secs(3), |p| p == 8.0);
    assert_eq!(positions, vec![30.0, 8.0], "positions after an unanswered seek");

    drop(player);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_mpv_load_after_unanswered_seek_accepts_positions() {
    common::isolate();
    if !has("python3") {
        eprintln!("SKIPPED: python3 not installed");
        return;
    }
    let dir = scratch_dir("reload");
    let mut player = playing(&dir, "silent");

    seek_to(&mut player, 30.0);
    let positions = watch(&player, Duration::from_millis(400), |_| false);
    assert_eq!(positions, vec![30.0], "the stale report before the load");

    // A new load replaces the unanswered seek: its positions show at once, long before the time
    // bound (the fake reports 8.0 s first, then the new load's 12 s and 12.5 s).
    player.stop().expect("stop");
    player.play_at(&dir.join("track.mp3"), 12.0).expect("play_at");
    let positions = watch(&player, Duration::from_secs(1), |p| p == 12.5);
    assert_eq!(
        positions.last(),
        Some(&12.5),
        "positions after the new load: {:?}",
        positions
    );

    drop(player);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_mpv_file_ending_after_seek_accepts_positions() {
    common::isolate();
    if !has("python3") {
        eprintln!("SKIPPED: python3 not installed");
        return;
    }
    let dir = scratch_dir("end");
    let mut player = playing(&dir, "end");

    // mpv takes the seek, then the file ends instead of restarting: what it reports next counts.
    seek_to(&mut player, 30.0);
    let positions = watch(&player, Duration::from_millis(1500), |p| p == 9.0);
    assert_eq!(positions, vec![30.0, 9.0], "positions after a seek the file ended on");
    assert!(player.is_track_finished(), "the end-file still finishes the track");

    drop(player);
    let _ = std::fs::remove_dir_all(&dir);
}
