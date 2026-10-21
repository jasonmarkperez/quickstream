//! `PlaybackQueue::upcoming`: what plays after the current track, as `next_track` (or the UPnP
//! continuous stream, which wraps whatever the mode) will actually play it. UP NEXT on tab 1 is
//! drawn from it.

mod common;

use std::path::PathBuf;

use quickstream::metadata::AudioMetadata;
use quickstream::queue::{PlaybackQueue, QueueEnd};

fn queue(n: usize, current: usize) -> PlaybackQueue {
    let mut q = PlaybackQueue::new();
    for i in 0..n {
        q.add_track(AudioMetadata {
            filepath: PathBuf::from(format!("/qs-test-nonexistent/{i}.flac")),
            title: format!("Track {i}"),
            ..Default::default()
        });
    }
    q.set_current_index(current);
    q
}

#[test]
fn test_upcoming_matches_next_track_in_every_mode() {
    common::isolate();
    for mode in ["stop", "loop", "repeat_one"] {
        for n in 1..=5 {
            for current in 0..n {
                let q = queue(n, current);
                let upcoming = q.upcoming(mode, false);
                let listed: Vec<usize> = upcoming.collect();
                assert_eq!(listed.len(), upcoming.len());
                // Playing on with next_track visits the same tracks in the same order.
                let mut walk = q.clone();
                for (step, want) in listed.iter().enumerate() {
                    let got = walk.next_track(mode).is_some().then(|| walk.current_index());
                    assert_eq!(got, Some(*want), "{mode} n={n} current={current} step {step}");
                }
                // Then what the end says happens.
                let after = walk.next_track(mode).is_some().then(|| walk.current_index());
                match upcoming.end() {
                    QueueEnd::Stops => assert_eq!(after, None, "{mode} n={n} current={current}"),
                    QueueEnd::RepeatOne => assert_eq!(after, Some(current), "{mode} n={n} current={current}"),
                    QueueEnd::Loops => assert_eq!(after, Some(current), "{mode} n={n} current={current}"),
                    QueueEnd::Reshuffles => panic!("only the continuous stream reshuffles"),
                }
            }
        }
    }
}

#[test]
fn test_upcoming_per_mode() {
    common::isolate();
    let q = queue(5, 2);
    let stop = q.upcoming("stop", false);
    assert_eq!(stop.collect::<Vec<_>>(), vec![3, 4]);
    assert_eq!(stop.end(), QueueEnd::Stops);

    // Loop wraps around to the first track; the current one comes round last.
    let lp = q.upcoming("loop", false);
    assert_eq!(lp.collect::<Vec<_>>(), vec![3, 4, 0, 1]);
    assert_eq!(lp.end(), QueueEnd::Loops);

    // Repeat-one replays the current track: nothing else is next.
    let one = q.upcoming("repeat_one", false);
    assert_eq!(one.len(), 0);
    assert_eq!(one.end(), QueueEnd::RepeatOne);

    // Loop over a single track replays it.
    let single = queue(1, 0).upcoming("loop", false);
    assert_eq!(single.len(), 0);
    assert_eq!(single.end(), QueueEnd::Loops);

    assert_eq!(PlaybackQueue::new().upcoming("loop", false).len(), 0);
}

/// The UPnP continuous stream plays on to the end of the queue whatever the mode, then
/// reshuffles and starts over.
#[test]
fn test_upcoming_with_the_continuous_stream() {
    common::isolate();
    for mode in ["stop", "loop", "repeat_one"] {
        let up = queue(5, 2).upcoming(mode, true);
        assert_eq!(up.collect::<Vec<_>>(), vec![3, 4], "{mode}");
        assert_eq!(up.end(), QueueEnd::Reshuffles, "{mode}");
        let last = queue(5, 4).upcoming(mode, true);
        assert_eq!(last.len(), 0, "{mode}");
        assert_eq!(last.end(), QueueEnd::Reshuffles, "{mode}");
    }
}

/// A single track under the continuous stream: the stream doesn't reshuffle a one-track queue,
/// it plays the track again, so the end is `Loops` (UP NEXT's "repeats this track"), never
/// `Reshuffles`, whatever the mode (#134).
#[test]
fn test_upcoming_single_track_under_the_continuous_stream_repeats() {
    common::isolate();
    for mode in ["stop", "loop", "repeat_one"] {
        let single = queue(1, 0).upcoming(mode, true);
        assert_eq!(single.len(), 0, "{mode}");
        assert_eq!(single.end(), QueueEnd::Loops, "{mode}");
    }
}

/// One rule says whether the continuous stream reshuffles a queue at its wrap
/// (`queue::reshuffles_on_wrap`, which the stream asks too): Up Next's end under the stream is
/// `Reshuffles` exactly when it holds, so the two can't disagree (#134).
#[test]
fn test_upcoming_under_the_stream_follows_reshuffles_on_wrap() {
    common::isolate();
    use quickstream::queue::{reshuffles_on_wrap, Upcoming};
    assert!(!reshuffles_on_wrap(0));
    assert!(!reshuffles_on_wrap(1));
    assert!(reshuffles_on_wrap(2));
    assert!(reshuffles_on_wrap(60));
    for mode in ["stop", "loop", "repeat_one"] {
        for len in 0usize..=4 {
            let last = len.saturating_sub(1);
            let end = Upcoming::after(last, len, mode, true).end();
            assert_eq!(
                end == QueueEnd::Reshuffles,
                reshuffles_on_wrap(len),
                "{mode}, {len} tracks: {end:?}"
            );
        }
    }
}
