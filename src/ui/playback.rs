//! Playback: debounced seeks and track skips, and playing the current, next or previous track.

use std::time::{Duration, Instant};

use super::{playlists, AppUI};

impl AppUI {
    pub fn accumulate_seek(&mut self, delta: f64) {
        if !self.player.is_playing() && !self.player.is_paused() {
            let sign = if delta > 0.0 { "+" } else { "" };
            self.status_message = format!("Seek {}{:.0}s (00:00:00)", sign, delta);
            return;
        }

        // If seek direction changed (+ to - or - to +), flush previous seek first
        if (self.pending_seek_delta > 0.0 && delta < 0.0) || (self.pending_seek_delta < 0.0 && delta > 0.0) {
            self.flush_pending_seek();
        }

        let dur = self.player.duration_seconds();
        let current_base = self
            .pending_seek_target
            .unwrap_or_else(|| self.player.position_seconds());
        let new_target = if dur > 0.0 {
            (current_base + delta).clamp(0.0, dur)
        } else {
            (current_base + delta).max(0.0)
        };
        self.pending_seek_delta += delta;
        self.pending_seek_target = Some(new_target);
        self.last_seek_key_time = Some(std::time::Instant::now());

        let sign = if self.pending_seek_delta > 0.0 { "+" } else { "" };
        self.status_message = format!(
            "Seek {}{:.0}s ({})",
            sign,
            self.pending_seek_delta,
            crate::player::upnp::format_seconds_to_upnp_time(new_target)
        );
        crate::log_info!(
            "ui",
            "Accumulated seek: target={:.2}s (delta={:.1}s)",
            new_target,
            self.pending_seek_delta
        );
    }

    pub fn flush_pending_seek(&mut self) {
        if let Some(target) = self.pending_seek_target.take() {
            let delta = self.pending_seek_delta;
            self.pending_seek_delta = 0.0;
            self.last_seek_key_time = None;

            let sign = if delta > 0.0 { "+" } else { "" };
            match self.player.seek_to(target) {
                Ok(_) => {
                    self.status_message = format!(
                        "Seek {}{:.0}s ({})",
                        sign,
                        delta,
                        crate::player::upnp::format_seconds_to_upnp_time(target)
                    );
                    crate::log_info!("ui", "Flushed seek: target={:.2}s (delta={:.1}s)", target, delta);
                }
                Err(e) => {
                    self.status_message = format!("Seek error: {}", e);
                    crate::log_warn!("ui", "Flushed seek to {:.2}s failed: {:?}", target, e);
                }
            }
            self.update_web_state();
        }
    }

    pub fn poll_pending_seek(&mut self) {
        if self.pending_seek_target.is_some() {
            if let Some(t) = self.last_seek_key_time {
                if t.elapsed() >= Duration::from_millis(350) {
                    self.flush_pending_seek();
                }
            }
        }
    }

    pub fn skip_next_track_debounced(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        if self.hold_skip_for_playlist_fill(playlists::PlaylistSkip::Next) {
            return;
        }

        let raw_mode = self.settings_mgr.settings.queue_mode.clone();
        let mode = if raw_mode == "repeat_one" { "loop" } else { &raw_mode };
        if let Some(track) = self.queue.next_track(mode) {
            let track_title = track.display_title().to_string();
            self.queue_cursor = self.queue.current_index();

            if !self.pending_track_skip && self.player.is_playing() {
                let _ = self.player.stop();
            }

            self.pending_track_skip = true;
            self.last_track_skip_time = Some(Instant::now());
            self.status_message = format!("Skipped to: {}", track_title);
            self.update_web_state();
        } else {
            self.status_message = "End of queue".to_string();
        }
    }

    pub fn skip_prev_track_debounced(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        if self.hold_skip_for_playlist_fill(playlists::PlaylistSkip::Previous) {
            return;
        }

        if let Some(track) = self.queue.prev_track() {
            let track_title = track.display_title().to_string();
            self.queue_cursor = self.queue.current_index();

            if !self.pending_track_skip && self.player.is_playing() {
                let _ = self.player.stop();
            }

            self.pending_track_skip = true;
            self.last_track_skip_time = Some(Instant::now());
            self.status_message = format!("Skipped to: {}", track_title);
            self.update_web_state();
        }
    }

    pub fn flush_pending_track_skip(&mut self) {
        if self.pending_track_skip {
            self.pending_track_skip = false;
            self.last_track_skip_time = None;
            self.play_current_track();
        }
    }

    pub fn poll_pending_track_skip(&mut self) {
        if self.pending_track_skip {
            if let Some(t) = self.last_track_skip_time {
                if t.elapsed() >= Duration::from_millis(300) {
                    self.flush_pending_track_skip();
                }
            }
        }
    }

    /// Play the queue's current track. Returns the playback error, if `player.play` failed (it
    /// is also put in the status as "Playback error: ...").
    pub fn play_current_track(&mut self) -> Option<String> {
        self.pending_track_skip = false;
        self.last_track_skip_time = None;
        if let Some(ref ws) = self.web_server {
            if ws
                .stream_state()
                .continuous
                .is_active
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                // Published first: the stream drops a request made while the queue it sees is empty.
                let continuous = ws.stream_state().continuous.clone();
                self.update_web_state();
                continuous.skip_to_index(self.queue.current_index());
                return None;
            }
        }
        let mut playback_error = None;
        if let Some(track) = self.queue.current_track() {
            let path = track.filepath.clone();
            if track.duration_seconds <= 0.0 || track.artist.is_empty() {
                self.start_queue_metadata_fetch();
            }
            if let Some(ref ws) = self.web_server {
                ws.close_active_streams();
                ws.register_stream_file(&path);
            }
            if let Ok(mut s) = self.web_state.write() {
                s.filepath = Some(path.clone());
            }
            self.player_track = Some(path.clone());
            match self.player.play(&path) {
                Ok(_) => {
                    self.status_message.clear();
                }
                Err(e) => {
                    let message = format!("Playback error: {}", e);
                    self.status_message = message.clone();
                    playback_error = Some(message);
                }
            }
            self.update_web_state();
        }
        playback_error
    }

    /// The player reported that the current track ended: go on to the next one.
    pub fn handle_track_finished(&mut self) {
        // While tab 5's Enter is still checking the rest of a playlist, the next track may not
        // be queued yet: wait for it rather than stop or replay (repeat-one replays anyway).
        if self.settings_mgr.settings.queue_mode != "repeat_one"
            && self.hold_skip_for_playlist_fill(playlists::PlaylistSkip::Ended)
        {
            return;
        }
        self.play_next_track();
    }

    pub fn play_next_track(&mut self) {
        self.pending_track_skip = false;
        self.last_track_skip_time = None;
        if let Some(ref ws) = self.web_server {
            if ws
                .stream_state()
                .continuous
                .is_active
                .load(std::sync::atomic::Ordering::SeqCst)
                && !self.queue.is_empty()
            {
                let next_idx = (self.queue.current_index() + 1) % self.queue.len();
                self.queue.set_current_index(next_idx);
                self.queue_cursor = next_idx;
                // Published first: the stream drops a request made while the queue it sees is empty.
                let continuous = ws.stream_state().continuous.clone();
                self.update_web_state();
                continuous.skip_to_index(next_idx);
                return;
            }
        }
        let mode = self.settings_mgr.settings.queue_mode.clone();
        if let Some(track) = self.queue.next_track(&mode) {
            let path = track.filepath.clone();
            if track.duration_seconds <= 0.0 || track.artist.is_empty() {
                self.start_queue_metadata_fetch();
            }
            if let Some(ref ws) = self.web_server {
                ws.close_active_streams();
                ws.register_stream_file(&path);
            }
            if let Ok(mut s) = self.web_state.write() {
                s.filepath = Some(path.clone());
            }
            self.player_track = Some(path.clone());
            match self.player.play(&path) {
                Ok(_) => {
                    self.status_message.clear();
                }
                Err(e) => {
                    self.status_message = format!("Playback error: {}", e);
                }
            }
            self.update_web_state();
        }
    }

    pub fn play_prev_track(&mut self) {
        self.pending_track_skip = false;
        self.last_track_skip_time = None;
        if let Some(ref ws) = self.web_server {
            if ws
                .stream_state()
                .continuous
                .is_active
                .load(std::sync::atomic::Ordering::SeqCst)
                && !self.queue.is_empty()
            {
                let prev_idx = if self.queue.current_index() > 0 {
                    self.queue.current_index() - 1
                } else {
                    self.queue.len().saturating_sub(1)
                };
                self.queue.set_current_index(prev_idx);
                self.queue_cursor = prev_idx;
                // Published first: the stream drops a request made while the queue it sees is empty.
                let continuous = ws.stream_state().continuous.clone();
                self.update_web_state();
                continuous.skip_to_index(prev_idx);
                return;
            }
        }
        if let Some(track) = self.queue.prev_track() {
            let path = track.filepath.clone();
            if track.duration_seconds <= 0.0 || track.artist.is_empty() {
                self.start_queue_metadata_fetch();
            }
            if let Some(ref ws) = self.web_server {
                ws.close_active_streams();
                ws.register_stream_file(&path);
            }
            if let Ok(mut s) = self.web_state.write() {
                s.filepath = Some(path.clone());
            }
            self.player_track = Some(path.clone());
            match self.player.play(&path) {
                Ok(_) => {
                    self.status_message.clear();
                }
                Err(e) => {
                    self.status_message = format!("Playback error: {}", e);
                }
            }
            self.update_web_state();
        }
    }
}
