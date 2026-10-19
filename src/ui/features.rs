//! The current track's analysis features (BPM, key, energy, …), read from the analysis database
//! on a worker thread so the UI thread never waits on SQLite (non-blocking UI invariant).
//!
//! The UI tells the worker which track is current ([`TrackFeaturesWorker::follow`]) and picks up
//! results without waiting ([`TrackFeaturesWorker::poll`]). A track that has no features yet is
//! read again every [`RETRY_INTERVAL`], since the analysis worker may finish it later; once it
//! has some they are read again every [`REFRESH_INTERVAL`] while the track stays current, so a
//! re-analysis (the file was retagged or replaced) shows without a track change. Only a changed
//! result is sent. A result that arrives after the track changed is dropped.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

use super::workers::WorkerRegistry;
use crate::analysis::{AnalysisDatabase, TrackFeatures};

/// How long the worker waits before reading a track's features again while it has none.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// How long the worker waits before reading the current track's features again once it has them.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Where features come from: the analysis database in the app, a fake in tests. Called on the
/// worker thread only.
pub type FeatureSource = Arc<dyn Fn(&Path) -> Option<TrackFeatures> + Send + Sync>;

/// A source that reads `db`.
pub fn database_source(db: AnalysisDatabase) -> FeatureSource {
    Arc::new(move |path: &Path| db.get_features(path))
}

/// The UI side of the features worker: the track it follows and the latest features found for it.
pub struct TrackFeaturesWorker {
    /// Tracks to read, newest last (`None`: nothing is playing). Dropped to stop the worker.
    requests: Option<Sender<Option<PathBuf>>>,
    /// Features found, with the track they were read for.
    results: Receiver<(PathBuf, TrackFeatures)>,
    /// The track the UI asked for last.
    track: Option<PathBuf>,
    /// Features of `track`, once found.
    features: Option<TrackFeatures>,
}

impl TrackFeaturesWorker {
    /// Start the worker on `workers`, retrying every [`RETRY_INTERVAL`] and refreshing every
    /// [`REFRESH_INTERVAL`].
    pub fn spawn(workers: &mut WorkerRegistry, source: FeatureSource) -> Self {
        Self::spawn_with_retry(workers, source, RETRY_INTERVAL)
    }

    /// Start the worker on `workers`, retrying a track without features every `retry`.
    pub fn spawn_with_retry(workers: &mut WorkerRegistry, source: FeatureSource, retry: Duration) -> Self {
        Self::spawn_with_intervals(workers, source, retry, REFRESH_INTERVAL)
    }

    /// Start the worker on `workers`, retrying a track without features every `retry` and
    /// reading a track's features again every `refresh` once it has them (test seam).
    pub fn spawn_with_intervals(
        workers: &mut WorkerRegistry,
        source: FeatureSource,
        retry: Duration,
        refresh: Duration,
    ) -> Self {
        let (requests, inbox) = mpsc::channel();
        let (outbox, results) = mpsc::channel();
        workers.spawn("track-features", move || run(&inbox, &outbox, &*source, retry, refresh));
        Self {
            requests: Some(requests),
            results,
            track: None,
            features: None,
        }
    }

    /// Make `track` the one to read features for. Cheap when it is already the current one, so
    /// the UI can call it every tick. Never blocks.
    pub fn follow(&mut self, track: Option<&Path>) {
        if self.track.as_deref() == track {
            return;
        }
        self.track = track.map(Path::to_path_buf);
        self.features = None;
        if let Some(requests) = &self.requests {
            if requests.send(self.track.clone()).is_err() {
                self.requests = None;
            }
        }
    }

    /// Take the results the worker has sent, keeping only those for the current track. Never
    /// blocks.
    pub fn poll(&mut self) {
        while let Ok((track, features)) = self.results.try_recv() {
            if self.track.as_ref() == Some(&track) {
                self.features = Some(features);
            }
        }
    }

    /// Features of the current track, if they have been found.
    pub fn features(&self) -> Option<&TrackFeatures> {
        self.features.as_ref()
    }

    /// Features of `track` if it is the track followed and they have been found. The renderer
    /// asks this way, so a frame drawn after the current track changed, before the next
    /// [`follow`](Self::follow), never shows the previous track's features.
    pub fn features_for(&self, track: &Path) -> Option<&TrackFeatures> {
        if self.track.as_deref() == Some(track) {
            self.features.as_ref()
        } else {
            None
        }
    }

    /// Tell the worker thread to exit (it finishes a read in progress first). Further calls to
    /// [`follow`](Self::follow) only record the track.
    pub fn stop(&mut self) {
        self.requests = None;
    }
}

impl Drop for TrackFeaturesWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The worker thread: read the newest requested track, again every `retry` until it has
/// features, then every `refresh` while it stays current, sending each changed result. Exits
/// once the UI side stops or is dropped.
fn run(
    inbox: &Receiver<Option<PathBuf>>,
    outbox: &Sender<(PathBuf, TrackFeatures)>,
    source: &(dyn Fn(&Path) -> Option<TrackFeatures> + Send + Sync),
    retry: Duration,
    refresh: Duration,
) {
    // The track followed, and the features last sent for it.
    let mut current: Option<PathBuf> = None;
    let mut sent: Option<TrackFeatures> = None;
    loop {
        let next = match (&current, &sent) {
            (None, _) => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
            (Some(_), None) => inbox.recv_timeout(retry),
            (Some(_), Some(_)) => inbox.recv_timeout(refresh),
        };
        match next {
            // Skip tracks that were current only briefly; read the newest.
            Ok(track) => {
                current = inbox.try_iter().last().unwrap_or(track);
                sent = None;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let Some(track) = &current else { continue };
        if let Some(features) = source(track) {
            if sent.as_ref() != Some(&features) {
                if outbox.send((track.clone(), features.clone())).is_err() {
                    return;
                }
                sent = Some(features);
            }
        }
    }
}
