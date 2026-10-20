//! Output switching (local player or a DLNA renderer, handing playback over) and binding the web
//! server to the interfaces the output needs.

use super::{spawn_local_player, AppUI, OutputPlayerFactory};
use crate::player::worker::AsyncPlayer;
use crate::player::{resume_position, AudioPlayer, Resume};

/// What the new output picks up when playback moves to it (issue #48), worked out from the old
/// player before it stops.
enum HandOff {
    /// Nothing was playing or paused: the new output starts idle.
    Idle,
    /// Start the queue's current track at `position`, paused when `paused`.
    Resume { position: f64, paused: bool },
    /// The current track is over: move on as at its natural end, paused when `paused`.
    NextTrack { paused: bool },
}

impl AppUI {
    /// Drop a replaced player on a worker thread so its shutdown (killing mpv, sending Stop to a
    /// renderer) never blocks the UI. Shutdown waits for it, so mpv is gone before the process exits.
    pub(super) fn retire_player(&mut self, player: Box<dyn AudioPlayer>) {
        self.workers.spawn("retire-player", move || drop(player));
    }

    /// Build the player every later output switch moves to with `factory`, instead of starting
    /// mpv or a renderer client (tests use a stand-in player). Public only because integration
    /// tests in `tests/` drive `AppUI` directly.
    #[doc(hidden)]
    pub fn set_output_player_factory(&mut self, factory: OutputPlayerFactory) {
        self.output_player_factory = Some(factory);
    }

    /// Interfaces the web server must listen on right now: the LAN while a DLNA renderer is
    /// active or the user enabled LAN access, otherwise loopback only. Unless Web Remote Access
    /// and the web server are both on (`Settings::web_remote_access`), network peers get only
    /// `/stream/*` (`NowPlayingState::remote_access`).
    pub fn required_web_bind(&self) -> crate::web::WebBind {
        let device = self.device_manager.get_active_device();
        self.web_bind_for_renderer(device.as_ref())
    }

    /// Interfaces needed while `renderer` (if any) streams from this server. A renderer on this
    /// computer (loopback address) reaches the server over loopback.
    fn web_bind_for_renderer(&self, renderer: Option<&crate::discovery::DLNADevice>) -> crate::web::WebBind {
        let remote_renderer = renderer.is_some_and(|d| {
            d.host != "localhost" && !d.host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
        });
        if self.settings_mgr.settings.web_server_lan || remote_renderer {
            crate::web::WebBind::Lan
        } else {
            crate::web::WebBind::Loopback
        }
    }

    pub fn ensure_web_server_started(&mut self) -> u16 {
        let bind = self.required_web_bind();
        self.ensure_web_server_bound(bind)
            .unwrap_or(self.settings_mgr.settings.web_server_port)
    }

    /// Make sure the web server is running on `bind`, restarting it if it is bound differently.
    fn ensure_web_server_bound(&mut self, bind: crate::web::WebBind) -> anyhow::Result<u16> {
        if let Some(ref handle) = self.web_server {
            if handle.bind == bind {
                return Ok(handle.port);
            }
        }
        // Reuse the running server's port so open web remotes reconnect to the same address.
        let port = match self.web_server.take() {
            Some(old) => {
                let port = old.port;
                old.stop();
                port
            }
            None => self.settings_mgr.settings.web_server_port,
        };
        // Before the first request: the setting may have changed since the last publish.
        if let Ok(mut state) = self.web_state.write() {
            state.remote_access = self.settings_mgr.settings.web_remote_access();
        }
        let (tx, rx) = std::sync::mpsc::channel();
        match crate::web::WebServer::start_with_bind(bind, port, self.web_state.clone(), tx) {
            Ok(handle) => {
                let actual_port = handle.port;
                for track in self.queue.tracks() {
                    handle.register_stream_file(&track.filepath);
                }
                self.web_server = Some(handle);
                self.web_cmd_rx = Some(rx);
                Ok(actual_port)
            }
            Err(e) => {
                crate::log_info!("ui", "Failed to start streaming server: {}", e);
                self.web_cmd_rx = None;
                Err(e)
            }
        }
    }

    pub fn register_stream_file(&self, path: &std::path::Path) {
        if let Some(ref ws) = self.web_server {
            ws.register_stream_file(path);
        }
    }

    /// Whether the player holds the queue's current track. Not while a debounced skip is
    /// pending (the queue moved, the player not yet), nor once the queue moved on without it
    /// (the track it played removed, or the queue cleared and refilled while it played on).
    /// Trusted while the app hasn't told it to play anything (`player_track` is `None`).
    fn player_holds_current_track(&self) -> bool {
        if self.pending_track_skip {
            return false;
        }
        match &self.player_track {
            None => true,
            Some(path) => self.queue.current_track().is_some_and(|t| t.filepath == *path),
        }
    }

    /// Length of the current track for [`resume_position`], as [`Self::playback_times`] takes
    /// it: the player's while it is playing or paused and holds the current track (a tag length
    /// can be an estimate), else the track's own (0 when neither is known). Read before the old
    /// player stops; the new one's `play_at` returns before it has loaded the file.
    fn resume_duration(&self) -> f64 {
        let loaded = self.player.is_playing() || self.player.is_paused();
        let reported = self.player.duration_seconds();
        if loaded && self.player_holds_current_track() && reported.is_finite() && reported > 0.0 {
            return reported;
        }
        self.queue.current_track().map(|t| t.duration_seconds).unwrap_or(0.0)
    }

    /// What the new output picks up, worked out from the old player before it stops (issue
    /// #48). A pending debounced skip or seek is applied here, as any other key applies it, but
    /// on the new output only:
    /// - a pending skip: its target (already the current track) plays from its start;
    /// - a pending seek: its target is the position, so a seek back out of the last second
    ///   resumes there and a seek into it moves on.
    ///
    /// A track end the old player reported but the event loop has not handled would be lost with
    /// the retired player and leave the queue stuck, so the track is over then too. When the
    /// player no longer holds the current track, its position and end say nothing about it: the
    /// current track starts from 0:00.
    fn take_hand_off(&mut self) -> HandOff {
        let holds = self.player_holds_current_track();
        let skip_pending = std::mem::take(&mut self.pending_track_skip);
        self.last_track_skip_time = None;
        let seek_target = self.pending_seek_target.take();
        self.pending_seek_delta = 0.0;
        self.last_seek_key_time = None;
        if self.queue.current_track().is_none() {
            return HandOff::Idle;
        }
        if skip_pending {
            return HandOff::Resume {
                position: 0.0,
                paused: false,
            };
        }
        let paused = self.player.is_paused();
        if self.player.is_track_finished() {
            return if holds {
                HandOff::NextTrack { paused }
            } else {
                HandOff::Resume { position: 0.0, paused }
            };
        }
        if !self.player.is_playing() && !paused {
            return HandOff::Idle;
        }
        if !holds {
            return HandOff::Resume { position: 0.0, paused };
        }
        let position = seek_target.unwrap_or_else(|| self.player.position_seconds());
        match resume_position(position, self.resume_duration()) {
            Resume::At(position) => HandOff::Resume { position, paused },
            Resume::NextTrack => HandOff::NextTrack { paused },
        }
    }

    /// The player the switch moves to: the test seam's, else `build`'s.
    fn output_player(&mut self, build: impl FnOnce() -> Box<dyn AudioPlayer>) -> Box<dyn AudioPlayer> {
        match self.output_player_factory.as_mut() {
            Some(factory) => factory(),
            None => build(),
        }
    }

    /// Put `new_player` in place of the old output (retired on a worker) and pick up `hand_off`
    /// on it, then show `status`. A track found over moves on as at its natural end
    /// (`handle_track_finished`: stop-at-end and repeat-one as usual, a held skip while a
    /// playlist is still being checked); a paused track's successor loads paused. When that
    /// left a status of its own (`Playback error: …`, `Waiting for the rest of …`), it stays.
    fn hand_off_to(&mut self, new_player: Box<dyn AudioPlayer>, hand_off: HandOff, status: String) {
        let old = std::mem::replace(&mut self.player, new_player);
        self.retire_player(old);
        match hand_off {
            HandOff::Idle => self.status_message = status,
            HandOff::Resume { position, paused } => {
                if let Some(path) = self.queue.current_track().map(|t| t.filepath.clone()) {
                    if let Some(ref ws) = self.web_server {
                        ws.register_stream_file(&path);
                    }
                    if let Ok(mut s) = self.web_state.write() {
                        s.filepath = Some(path.clone());
                    }
                    self.player_track = Some(path.clone());
                    if self.player.play_at(&path, position).is_ok() && paused {
                        let _ = self.player.pause();
                    }
                }
                self.status_message = status;
            }
            HandOff::NextTrack { paused } => {
                self.status_message.clear();
                self.handle_track_finished();
                if paused {
                    if self.player.is_playing() {
                        let _ = self.player.pause();
                    } else {
                        self.pause_after_held_skip();
                    }
                }
                if self.status_message.is_empty() {
                    self.status_message = status;
                }
            }
        }
    }

    pub fn switch_to_local_player(&mut self) {
        self.switch_output_to_local("Active output: Local Audio".to_string());
    }

    /// Move playback to Local Audio, showing `status` once there.
    pub(super) fn switch_output_to_local(&mut self, status: String) {
        let hand_off = self.take_hand_off();
        let vol = self.player.volume();

        self.player.detach_stream_state();
        let _ = self.player.stop();
        if let Some(ref ws) = self.web_server {
            ws.close_active_streams();
        }
        self.device_manager.set_active_device(None);

        let backend = self.settings_mgr.settings.player_backend.clone();
        let mut new_player = self.output_player(|| spawn_local_player(&backend, vol));
        let _ = new_player.set_volume(vol);

        self.hand_off_to(new_player, hand_off, status);
        self.sync_web_server();
        self.update_web_state();
    }

    pub fn switch_to_upnp_player(&mut self, dev: crate::discovery::DLNADevice) {
        let status = format!("Active output: {}", dev.friendly_name);
        self.switch_output_to_upnp(dev, status);
    }

    /// Move playback to the renderer `dev`, showing `status` once there.
    pub(super) fn switch_output_to_upnp(&mut self, dev: crate::discovery::DLNADevice, status: String) {
        // Re-selecting the active renderer is a no-op: a new player would queue Play while the
        // old player's delayed Stop could still reach the same device.
        if self
            .device_manager
            .get_active_device()
            .is_some_and(|d| d.udn == dev.udn)
        {
            self.status_message = status;
            return;
        }
        let bind = self.web_bind_for_renderer(Some(&dev));
        let port = self
            .ensure_web_server_bound(bind)
            .unwrap_or(self.settings_mgr.settings.web_server_port);
        let hand_off = self.take_hand_off();

        self.player.detach_stream_state();
        let _ = self.player.stop();
        if let Some(ref ws) = self.web_server {
            ws.close_active_streams();
            for track in self.queue.tracks() {
                ws.register_stream_file(&track.filepath);
            }
        }
        self.device_manager.set_active_device(Some(dev.clone()));

        let stream_state = self.web_server.as_ref().map(|ws| ws.stream_state());
        let worker_dev = dev.clone();
        // Built on the worker thread: UPnPPlayer::new queries the renderer's volume over the network.
        // Do NOT overwrite the remote hardware volume with local software volume!
        // The UPnP device preserves its own hardware volume setting.
        let upnp_player = self.output_player(|| {
            Box::new(AsyncPlayer::spawn(
                move || {
                    Box::new(crate::player::upnp::UPnPPlayer::new(worker_dev, port).with_stream_state(stream_state))
                },
                // The renderer's volume is unknown until UPnPPlayer::new has queried it. Report 0 rather
                // than the local volume so a volume key in that window cannot jump the speaker.
                0,
            ))
        });

        self.hand_off_to(upnp_player, hand_off, status);
        self.update_web_state();
    }

    pub fn sync_web_server(&mut self) {
        let needs_server =
            self.settings_mgr.settings.web_server_enabled || self.device_manager.get_active_device().is_some();

        if needs_server {
            let bind = self.required_web_bind();
            let was_running = self.web_server.is_some();
            let already_bound = self.web_server.as_ref().is_some_and(|h| h.bind == bind);
            if !already_bound {
                match self.ensure_web_server_bound(bind) {
                    // A rebind (e.g. after switching output) keeps the caller's status message.
                    Ok(port) if !was_running => {
                        self.status_message = format!("Started server on port {}", port);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        self.status_message = format!("Failed to start server: {}", e);
                        // Only a failed cold start turns the setting off; a failed rebind may be transient.
                        if !was_running && self.settings_mgr.settings.web_server_enabled {
                            self.settings_mgr.settings.web_server_enabled = false;
                            self.settings_mgr.save();
                        }
                    }
                }
            }
        } else if let Some(handle) = self.web_server.take() {
            handle.stop();
            self.web_cmd_rx = None;
            self.status_message = "Stopped web server".to_string();
        }
    }
}
