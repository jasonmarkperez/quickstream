//! `PlaybackQueue::revision()`: every change to the queue's tracks or current track goes through a
//! method that bumps the revision, so caches built from the queue (the IN QUEUE path set, totals)
//! can tell when to rebuild. Reads and calls that change nothing leave it alone.

mod common;

use std::path::PathBuf;

use quickstream::metadata::AudioMetadata;
use quickstream::queue::PlaybackQueue;

fn track(n: usize) -> AudioMetadata {
    AudioMetadata {
        filepath: PathBuf::from(format!("/music/song{n}.mp3")),
        filename: format!("song{n}.mp3"),
        title: format!("Song {n}"),
        duration_seconds: 100.0,
        ..AudioMetadata::default()
    }
}

fn queue_of(n: usize) -> PlaybackQueue {
    let mut q = PlaybackQueue::new();
    q.add_tracks((1..=n).map(track).collect());
    q
}

/// Run `change` on `q` and assert it bumped the revision.
fn assert_bumps(q: &mut PlaybackQueue, what: &str, change: impl FnOnce(&mut PlaybackQueue)) {
    let before = q.revision();
    change(q);
    assert!(q.revision() > before, "{what} should bump the revision");
}

/// Run `call` on `q` and assert the revision stayed the same.
fn assert_keeps(q: &mut PlaybackQueue, what: &str, call: impl FnOnce(&mut PlaybackQueue)) {
    let before = q.revision();
    call(q);
    assert_eq!(q.revision(), before, "{what} should not bump the revision");
}

#[test]
fn adding_tracks_bumps_revision() {
    common::isolate();
    let mut q = PlaybackQueue::new();
    assert_bumps(&mut q, "add_track", |q| q.add_track(track(1)));
    assert_bumps(&mut q, "add_tracks", |q| q.add_tracks(vec![track(2), track(3)]));
    assert_bumps(&mut q, "insert_tracks", |q| q.insert_tracks(1, vec![track(4)]));
    assert_eq!(q.len(), 4);
}

#[test]
fn removing_and_clearing_bump_revision() {
    common::isolate();
    let mut q = queue_of(4);
    assert_bumps(&mut q, "remove", |q| {
        q.remove(1);
    });
    assert_bumps(&mut q, "remove_track", |q| {
        q.remove_track(0);
    });
    assert_bumps(&mut q, "clear", |q| q.clear());
    assert!(q.is_empty());
}

#[test]
fn reordering_and_shuffling_bump_revision() {
    common::isolate();
    let mut q = queue_of(6);
    assert_bumps(&mut q, "reorder_track", |q| {
        assert!(q.reorder_track(0, 3));
    });
    assert_bumps(&mut q, "shuffle", |q| {
        q.shuffle();
    });
    q.set_current_index(1);
    assert_bumps(&mut q, "shuffle_from_next", |q| q.shuffle_from_next());
    assert_bumps(&mut q, "shuffle_in", |q| {
        q.shuffle_in(vec![track(7), track(8)]);
    });
}

#[test]
fn current_track_changes_bump_revision() {
    common::isolate();
    let mut q = queue_of(3);
    assert_bumps(&mut q, "set_current_index", |q| q.set_current_index(2));
    assert_eq!(q.current_index(), 2);
    assert_bumps(&mut q, "next_track (loop wraps)", |q| {
        q.next_track("loop");
    });
    assert_eq!(q.current_index(), 0);
    assert_bumps(&mut q, "prev_track", |q| {
        q.prev_track();
    });
    assert_eq!(q.current_index(), 2);
}

#[test]
fn truncate_after_keeps_up_to_index_and_bumps_revision() {
    common::isolate();
    let mut q = queue_of(5);
    q.set_current_index(1);
    assert_bumps(&mut q, "truncate_after", |q| q.truncate_after(1));
    let titles: Vec<&str> = q.tracks().iter().map(|t| t.title.as_str()).collect();
    assert_eq!(titles, ["Song 1", "Song 2"]);
    assert_eq!(q.current_index(), 1);
    assert_keeps(&mut q, "truncate_after past the end", |q| q.truncate_after(5));
    assert_eq!(q.len(), 2);
}

#[test]
fn metadata_updates_bump_revision_only_when_a_track_matches() {
    common::isolate();
    let mut q = queue_of(3);
    let mut tagged = track(2);
    tagged.title = "Tagged".to_string();
    assert_bumps(&mut q, "update_metadata on a queued path", |q| {
        assert!(q.update_metadata(&tagged));
    });
    assert_eq!(q.tracks()[1].title, "Tagged");
    assert_keeps(&mut q, "update_metadata on a path not in the queue", |q| {
        assert!(!q.update_metadata(&track(9)));
    });
}

#[test]
fn mutable_track_access_bumps_revision() {
    common::isolate();
    let mut q = queue_of(2);
    assert_bumps(&mut q, "track_mut", |q| q.track_mut(1).unwrap().duration_seconds = 0.0);
    assert_eq!(q.tracks()[1].duration_seconds, 0.0);
    assert_bumps(&mut q, "current_track_mut", |q| {
        q.current_track_mut().unwrap().title = "Edited".to_string();
    });
    assert_eq!(q.tracks()[0].title, "Edited");
}

#[test]
fn reads_do_not_bump_revision() {
    common::isolate();
    let mut q = queue_of(3);
    q.set_current_index(1);
    assert_keeps(&mut q, "reads", |q| {
        let _ = q.len();
        let _ = q.is_empty();
        let _ = q.tracks();
        let _ = q.current_index();
        let _ = q.current_track();
        let _ = q.total_duration_seconds();
        let _ = q.revision();
    });
}

#[test]
fn calls_that_change_nothing_do_not_bump_revision() {
    common::isolate();
    let mut q = queue_of(3);
    q.set_current_index(1);
    assert_keeps(&mut q, "set_current_index to the same index", |q| {
        q.set_current_index(1)
    });
    assert_keeps(&mut q, "add_tracks with no tracks", |q| q.add_tracks(Vec::new()));
    assert_keeps(&mut q, "insert_tracks with no tracks", |q| {
        q.insert_tracks(0, Vec::new())
    });
    assert_keeps(&mut q, "remove out of range", |q| {
        assert!(q.remove(7).is_none());
    });
    assert_keeps(&mut q, "reorder_track out of range", |q| {
        assert!(!q.reorder_track(0, 9));
    });
    assert_keeps(&mut q, "next_track in repeat_one", |q| {
        q.next_track("repeat_one");
    });
    q.set_current_index(2);
    assert_keeps(&mut q, "next_track at the end in stop mode", |q| {
        assert!(q.next_track("stop").is_none());
    });
    assert_keeps(&mut q, "shuffle_from_next on the last track", |q| q.shuffle_from_next());

    let mut empty = PlaybackQueue::new();
    assert_keeps(&mut empty, "clear on an empty queue", |q| q.clear());
    assert_keeps(&mut empty, "shuffle on an empty queue", |q| {
        q.shuffle();
    });
    assert_keeps(&mut empty, "shuffle_in of no tracks", |q| {
        q.shuffle_in(Vec::new());
    });
    assert_keeps(&mut empty, "next_track on an empty queue", |q| {
        assert!(q.next_track("loop").is_none());
    });
}

/// Revisions come from one process-wide counter, so two queues (a replacement, a clone that then
/// changes) never share a revision while their tracks differ (PR #68 review).
#[test]
fn different_queues_never_share_a_revision() {
    common::isolate();
    let mut a = PlaybackQueue::new();
    let mut b = PlaybackQueue::new();
    assert_ne!(a.revision(), b.revision(), "two new queues");
    a.add_track(track(1));
    b.add_track(track(2));
    assert_ne!(a.revision(), b.revision(), "same number of changes, different tracks");

    let mut c = a.clone();
    assert_ne!(c.revision(), a.revision(), "a clone gets its own revision");
    c.add_track(track(3));
    a.add_track(track(4));
    assert_ne!(
        c.revision(),
        a.revision(),
        "a clone and its original after one change each"
    );

    let d = PlaybackQueue::default();
    assert_ne!(d.revision(), PlaybackQueue::default().revision(), "default queues");
}
