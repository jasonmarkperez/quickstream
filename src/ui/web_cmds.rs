//! The web dashboard's side of the UI thread: the commands the web server sends
//! (`handle_web_commands`), the folders it may serve, and the state it publishes
//! (`update_web_state`).

use std::path::PathBuf;

use super::AppUI;

impl AppUI {
    pub fn handle_web_commands(&mut self) {
        let commands: Vec<crate::web::WebCommand> = if let Some(ref rx) = self.web_cmd_rx {
            let mut list = Vec::new();
            while let Ok(cmd) = rx.try_recv() {
                list.push(cmd);
            }
            list
        } else {
            Vec::new()
        };

        for cmd in commands {
            match cmd {
                crate::web::WebCommand::TogglePause => {
                    if self.player.is_playing() {
                        let _ = self.player.pause();
                        self.status_message = "Paused playback (web)".to_string();
                    } else if self.player.is_paused() {
                        let _ = self.player.resume();
                        self.status_message = "Resumed playback (web)".to_string();
                    } else {
                        self.play_current_track();
                    }
                }
                crate::web::WebCommand::Play => {
                    if self.player.is_paused() {
                        let _ = self.player.resume();
                    } else if !self.player.is_playing() {
                        self.play_current_track();
                    }
                }
                crate::web::WebCommand::Pause => {
                    if self.player.is_playing() {
                        let _ = self.player.pause();
                    }
                }
                crate::web::WebCommand::Next => {
                    self.skip_next_track_debounced();
                }
                crate::web::WebCommand::Prev => {
                    self.skip_prev_track_debounced();
                }
                crate::web::WebCommand::SetVolume(vol) => {
                    let _ = self.player.set_volume(vol);
                    self.status_message = format!("Volume {}% (web)", vol);
                }
                crate::web::WebCommand::SeekTo(target) => {
                    let _ = self.player.seek_to(target);
                    self.update_web_state();
                }
                crate::web::WebCommand::SeekRelative(delta) => {
                    let _ = self.player.seek(delta);
                    self.update_web_state();
                }
                crate::web::WebCommand::PlayIndex(idx) => {
                    if idx < self.queue.len() {
                        self.supersede_playlist_queue();
                        self.queue.set_current_index(idx);
                        self.queue_cursor = idx;
                        self.play_current_track();
                        self.update_web_state();
                    }
                }
                crate::web::WebCommand::RemoveIndex(idx) => {
                    if self.remove_queue_track(idx).is_some() {
                        self.update_web_state();
                    }
                }
                crate::web::WebCommand::ClearQueue => {
                    self.supersede_playlist_queue();
                    let _ = self.player.stop();
                    self.queue.clear();
                    self.clamp_queue_cursor();
                    self.update_web_state();
                }
                crate::web::WebCommand::AddFile(path) => {
                    if path.exists() && crate::metadata::is_supported_audio_file(&path) {
                        let meta = self.quick_metadata(&path);
                        let title = meta.display_title().to_string();
                        self.enqueue_tracks(vec![meta]);
                        self.status_message = format!("Added to queue (web): {}", title);
                        self.update_web_state();
                    }
                }
                crate::web::WebCommand::AddFolder(path) => {
                    // Listing a folder on a network share can be slow: do it off the UI thread.
                    let tx = self.queue_add_tx.clone();
                    self.workers.spawn("add-folder", move || {
                        if let Ok(entries) = std::fs::read_dir(&path) {
                            let mut files: Vec<std::path::PathBuf> = entries
                                .filter_map(|e| e.ok())
                                .map(|e| e.path())
                                .filter(|p| crate::metadata::is_supported_audio_file(p))
                                .collect();
                            files.sort();
                            files.dedup(); // Some SMB shares return each entry several times (issue #35).
                            let tracks = files.iter().map(|p| crate::metadata::fallback_metadata(p)).collect();
                            let _ = tx.send(tracks);
                        }
                    });
                }
                crate::web::WebCommand::SelectDevice(id) => {
                    let devices = self.device_manager.get_devices();
                    if id == 0 {
                        self.switch_output_to_local("Switched to Local Audio (web)".to_string());
                    } else if let Some(dev) = devices.get(id.saturating_sub(1)) {
                        let status = format!("Switched to {} (web)", dev.friendly_name);
                        self.switch_output_to_upnp(dev.clone(), status);
                    }
                    self.update_web_state();
                }
                crate::web::WebCommand::RescanDevices => {
                    self.device_manager.start_background_scan();
                    self.status_message = "Scanning for DLNA MediaRenderers (web)...".to_string();
                    self.update_web_state();
                }
                crate::web::WebCommand::SetQueueMode(mode) => {
                    self.settings_mgr.settings.queue_mode = mode;
                    let _ = self.settings_mgr.save();
                    self.update_web_state();
                }
                crate::web::WebCommand::SetHome(path_opt) => {
                    self.settings_mgr.settings.home_dir = path_opt.clone();
                    let _ = self.settings_mgr.save();
                    if let Some(ref p) = path_opt {
                        self.status_message = format!("Set startup home directory (web): {}", p);
                    } else {
                        self.status_message = "Cleared startup home directory (web)".to_string();
                    }
                    self.update_web_state();
                }
                // The web thread resolved the folder inside the allowed ones (issue #102).
                crate::web::WebCommand::Autopilot(target) => self.start_autopilot(target),
                crate::web::WebCommand::AutopilotReshuffle(target) => self.reshuffle_autopilot(target),
                crate::web::WebCommand::ShuffleQueue => {
                    // The shuffle keeps the current track in front: for the continuous stream's
                    // wrap that is the track it plays next, which it may have named since this
                    // tick's sync.
                    self.sync_continuous_stream_index();
                    self.shuffle_queue(" (web)");
                }
                crate::web::WebCommand::SetMixProfile(profile_str) => {
                    let profile = match profile_str.to_lowercase().as_str() {
                        "bpm_flow" | "bpm" => crate::analysis::MixProfile::BpmFlow {
                            target_bpm: 120.0,
                            tolerance: 0.15,
                        },
                        "chill" | "ambient" => crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Chill),
                        "workout" | "energy" => {
                            crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Workout)
                        }
                        "focus" => crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Focus),
                        "groove" => crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Groove),
                        _ => crate::analysis::MixProfile::DiverseShuffle,
                    };
                    self.active_mix_profile = profile;
                    self.status_message = format!("⚡ Mix Profile set to: {}", self.active_mix_profile.name());
                    self.update_web_state();
                }
                crate::web::WebCommand::SeedRadio(path) => {
                    self.start_seed_radio(path);
                }
                crate::web::WebCommand::PathRejected(path) => {
                    self.status_message = format!(
                        "Ignored web request: {} is outside your home, bookmarks and launch folder",
                        path.display()
                    );
                }
                crate::web::WebCommand::QueuePlaylist {
                    name,
                    entries,
                    play_from,
                    reply,
                } => {
                    let result = self.queue_web_playlist(name, entries, play_from);
                    reply.send(result);
                }
                crate::web::WebCommand::CheckPlaylists => self.check_playlists_for_web(),
                crate::web::WebCommand::EditPlaylist(edit, reply) => {
                    let result = self.apply_web_playlist_edit(edit);
                    // Publish first, so the dashboard's reload after the answer sees the edit.
                    self.update_web_state();
                    reply.send(result);
                }
            }
        }
    }

    /// Folders the web dashboard may browse and act on: the startup home, the bookmarks and the
    /// folder the browser opened at launch. Folders browsed later in the TUI are not included,
    /// and neither are the filesystem root or the home folder itself (too broad).
    pub fn web_allowed_roots(&self) -> Vec<PathBuf> {
        static LOGGED_SKIP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let home = crate::web_access::home_dir();
        let mut roots: Vec<PathBuf> = Vec::new();
        let candidates = self
            .settings_mgr
            .settings
            .home_dir
            .iter()
            .chain(self.bookmarks.iter())
            .map(PathBuf::from)
            .chain(std::iter::once(self.launch_dir.clone()));
        for root in candidates {
            if roots.contains(&root) {
                continue;
            }
            if !crate::web_access::is_acceptable_root(&root, home.as_deref()) {
                if root.is_absolute() && !LOGGED_SKIP.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    crate::log_info!(
                        "web",
                        "Not letting the web dashboard reach {}: too broad (use a music folder, bookmark or startup home)",
                        root.display()
                    );
                }
                continue;
            }
            roots.push(root);
        }
        roots
    }

    pub fn update_web_state(&mut self) {
        // Published even while no server runs, so a server started later never begins with an
        // old value.
        if let Ok(mut state) = self.web_state.write() {
            state.remote_access = self.settings_mgr.settings.web_remote_access();
        }
        if self.web_server.is_none() {
            return;
        }
        if let Ok(mut state) = self.web_state.write() {
            state.is_playing = self.player.is_playing();
            state.is_paused = self.player.is_paused();
            state.position_seconds = self.player.position_seconds();
            state.duration_seconds = self.player.duration_seconds();
            state.volume = self.player.volume();
            state.output_device = self
                .device_manager
                .get_active_device()
                .map(|d| d.friendly_name)
                .unwrap_or_else(|| "Local Audio".to_string());
            state.queue_index = self.queue.current_index();
            state.queue_total = self.queue.len();
            state.queue_mode = self.settings_mgr.settings.queue_mode.clone();
            state.is_scanning = self.device_manager.is_scanning();
            state.home_dir = self.settings_mgr.settings.home_dir.clone();
            state.current_browser_dir = self.browser_dir.to_string_lossy().to_string();
            state.terminal_dir = Some(self.browser_dir.clone());
            state.allowed_roots = self.web_allowed_roots();
            state.bookmarks = self.bookmarks.clone();
            state.is_autopilot = self.autopilot_dir.is_some();
            state.autopilot_dir = self.autopilot_dir.as_ref().map(|d| d.to_string_lossy().to_string());
            state.active_mix_profile = self.active_mix_profile.name();

            state.queue = self
                .queue
                .tracks()
                .iter()
                .enumerate()
                .map(|(idx, track)| crate::web::WebQueueItem {
                    index: idx,
                    title: track.display_title().to_string(),
                    artist: track.artist.clone(),
                    album: track.album.clone(),
                    duration_seconds: track.duration_seconds,
                    filepath: track.filepath.to_string_lossy().to_string(),
                    is_current: idx == self.queue.current_index(),
                })
                .collect();
            // A track the continuous stream started with an unknown length takes it from the
            // queue as soon as it is published here, not only when the stream next writes.
            if let Some(ws) = &self.web_server {
                ws.stream_state().continuous.learn_published_duration(&mut state);
            }

            let active_dev = self.device_manager.get_active_device();
            let mut devices = Vec::new();
            devices.push(crate::web::WebDeviceItem {
                id: 0,
                name: "Local Audio (mpv/ffplay)".to_string(),
                host_port: "Local Device".to_string(),
                is_active: active_dev.is_none(),
            });
            for (idx, dev) in self.device_manager.get_devices().iter().enumerate() {
                let is_active = active_dev.as_ref().map(|d| d.udn == dev.udn).unwrap_or(false);
                devices.push(crate::web::WebDeviceItem {
                    id: idx + 1,
                    name: dev.display_label(),
                    host_port: format!("{}:{}", dev.host, dev.port),
                    is_active,
                });
            }
            state.devices = devices;

            if let Some(track) = self.queue.current_track() {
                state.title = track.title.clone();
                state.artist = track.artist.clone();
                state.album = track.album.clone();
                state.genre = track.genre.clone();
                state.format = track.format_name.clone();
                state.bitrate_kbps = track.bitrate_kbps;
                state.sample_rate_hz = track.sample_rate_hz;
                state.channels = track.channels;
                state.filepath = Some(track.filepath.clone());
                state.has_cover = true;
                // What this tick's poll of the features worker found for this track: no
                // database read on the UI thread, and never the previous track's features.
                let feat = self.track_features.features_for(&track.filepath);
                state.bpm = feat.and_then(|f| f.bpm);
                state.key = feat.and_then(|f| f.display_key());
            } else {
                state.title = "No Track Loaded".to_string();
                state.artist = String::new();
                state.album = String::new();
                state.genre = String::new();
                state.format = String::new();
                state.bitrate_kbps = 0;
                state.sample_rate_hz = 0;
                state.channels = 0;
                state.filepath = None;
                state.has_cover = false;
                state.bpm = None;
                state.key = None;
            }

            if self.queue.len() > 1 && self.queue.current_index() + 1 < self.queue.len() {
                state.next_track_title = self
                    .queue
                    .tracks()
                    .get(self.queue.current_index() + 1)
                    .map(|t| t.display_title().to_string());
            } else if !self.queue.is_empty() && self.settings_mgr.settings.queue_mode == "loop" {
                state.next_track_title = self.queue.tracks().first().map(|t| t.display_title().to_string());
            } else {
                state.next_track_title = None;
            }
        }
        self.publish_web_playlists();
    }
}
