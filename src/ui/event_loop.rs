//! The event loop: `run` sets the terminal up and `event_loop` polls the workers, draws and
//! waits for a key each tick; background workers are stopped on shutdown.

use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
use ratatui::prelude::*;

use super::{AppUI, TerminalGuard, SHUTDOWN_REQUESTED, SHUTTING_DOWN, TUI_ACTIVE};

impl AppUI {
    /// Signal every background worker to stop (directory scan, tag readers, queue metadata pool,
    /// search scanner, autopilot scanner), then join them, waiting at most `timeout` in total.
    /// Workers still running at the deadline are detached. Returns how many were left running.
    /// Called when the app is dropped; safe to call more than once.
    pub fn shutdown_workers(&mut self, timeout: Duration) -> usize {
        let stops = [
            self.dir_scan_stop.take(),
            self.metadata_worker_stop.take(),
            self.autopilot_scan_stop.take(),
            Some(self.queue_metadata_stop.clone()),
        ];
        for stop in stops.into_iter().flatten() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.playlists.stop_checks();
        self.track_features.stop();
        // Dropping the inbox wakes queue metadata workers blocked waiting for work.
        self.queue_metadata_tx = None;
        if let Some(mut modal) = self.search_modal.take() {
            modal.close();
        }
        self.autopilot_rx = None;
        self.is_autopilot_scanning = false;

        let start = Instant::now();
        let left = self.workers.join_all(timeout);
        crate::log_info!(
            "ui",
            "Background workers stopped in {:.2?} ({} detached)",
            start.elapsed(),
            left
        );
        left
    }

    /// Stop playback and return true if a shutdown was requested (e.g. by SIGINT/SIGTERM/SIGHUP).
    pub fn poll_shutdown_request(&mut self) -> bool {
        if !SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        crate::log_info!("ui", "Shutdown requested by signal, stopping playback");
        let _ = self.player.stop();
        true
    }

    /// While the continuous stream plays, follow its track index (`current_track_index`) in the
    /// queue. Called once per tick by the event loop.
    pub fn sync_continuous_stream_index(&mut self) {
        if let Some(ref ws) = self.web_server {
            if ws
                .stream_state()
                .continuous
                .is_active
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                let cur_idx = *ws.stream_state().continuous.current_track_index.lock().unwrap();
                if cur_idx != self.queue.current_index() && cur_idx < self.queue.len() {
                    self.queue.set_current_index(cur_idx);
                    self.queue_cursor = cur_idx;
                }
            }
        }
    }

    pub fn run(&mut self) -> anyhow::Result<()> {
        self.device_manager.start_background_scan();
        enable_raw_mode()?;
        // From here on the terminal is restored on every exit path, including `?` and panics.
        let guard = TerminalGuard::claim();
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        TUI_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
        let res = self.event_loop(&mut terminal);
        SHUTTING_DOWN.store(true, std::sync::atomic::Ordering::SeqCst);
        // A bookmark or setting changed right before quitting must reach disk.
        self.persistence.flush();
        drop(guard);

        res
    }

    fn event_loop<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> anyhow::Result<()> {
        loop {
            if self.poll_shutdown_request() {
                return Ok(());
            }

            if let Some(err) = self.player.take_error() {
                self.status_message = format!("Playback error: {}", err);
            }

            // Check if active track finished and auto-advance queue
            if self.player.is_track_finished() {
                self.player.reset_finished();
                self.handle_track_finished();
            }

            self.sync_continuous_stream_index();

            // Process remote web commands
            self.handle_web_commands();

            self.poll_track_features();

            // Synchronize web server live state
            self.update_web_state();

            // Poll background directory scan
            self.poll_background_dir_scan();

            // Poll background metadata extraction
            self.poll_background_metadata();
            self.poll_pending_queue_adds();
            self.poll_playlists();
            self.poll_settings_status();

            // Poll progressive background autopilot discovery
            self.poll_background_autopilot();

            if let Some(ref mut modal) = self.search_modal {
                modal.poll_background_scan();
            }

            // Flush debounced seek if debounce timer expired
            self.poll_pending_seek();

            // Flush debounced track skip if debounce timer expired
            self.poll_pending_track_skip();

            terminal.draw(|f| self.render(f))?;

            let poll_dur = Duration::from_millis(100);

            if event::poll(poll_dur)? {
                if let Event::Key(key) = event::read()? {
                    if self.show_help_modal {
                        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
                            self.player.stop()?;
                            return Ok(());
                        }
                        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?')) {
                            self.show_help_modal = false;
                        }
                        continue;
                    }

                    if let Some(ref mut modal) = self.search_modal {
                        match (key.code, key.modifiers) {
                            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                                if let Some(mut m) = self.search_modal.take() {
                                    m.close();
                                }
                                self.player.stop()?;
                                return Ok(());
                            }
                            (KeyCode::Esc, _) => {
                                if let Some(mut m) = self.search_modal.take() {
                                    m.close();
                                }
                            }
                            (KeyCode::Enter, KeyModifiers::NONE) => {
                                let track_opt = modal.selected_track().cloned();
                                if let Some(mut m) = self.search_modal.take() {
                                    m.close();
                                }
                                if let Some(track) = track_opt {
                                    let title = track.display_title().to_string();
                                    self.register_stream_file(&track.filepath);
                                    self.queue.add_track(track);
                                    self.status_message = format!("Added to queue: {}", title);
                                }
                            }
                            (KeyCode::Tab, KeyModifiers::NONE) => {
                                if let Some(track) = modal.selected_track().cloned() {
                                    self.register_stream_file(&track.filepath);
                                    self.queue.add_track(track.clone());
                                    self.status_message = format!("Added to queue: {}", track.display_title());
                                }
                            }
                            (KeyCode::BackTab, _) | (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                                let tracks = modal.all_filtered_tracks();
                                let count = tracks.len();
                                if let Some(mut m) = self.search_modal.take() {
                                    m.close();
                                }
                                for t in tracks {
                                    self.register_stream_file(&t.filepath);
                                    self.queue.add_track(t);
                                }
                                self.status_message = format!("Added {} tracks to queue", count);
                            }
                            (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                                modal.move_selection_up();
                            }
                            (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                                modal.move_selection_down();
                            }
                            (KeyCode::PageUp, _)
                            | (KeyCode::Char('u'), KeyModifiers::CONTROL)
                            | (KeyCode::Char('b'), KeyModifiers::CONTROL) => {
                                modal.page_up(15);
                            }
                            (KeyCode::PageDown, _)
                            | (KeyCode::Char('d'), KeyModifiers::CONTROL)
                            | (KeyCode::Char('f'), KeyModifiers::CONTROL) => {
                                modal.page_down(15);
                            }
                            (KeyCode::Home, _) => {
                                modal.home();
                            }
                            (KeyCode::End, _) => {
                                modal.end();
                            }
                            (KeyCode::Backspace, _) => {
                                modal.backspace();
                            }
                            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                                modal.append_char(c);
                            }
                            _ => {}
                        }
                        continue;
                    }

                    if self.browser_spotlight.is_some() {
                        match (key.code, key.modifiers) {
                            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                                self.close_spotlight();
                                self.player.stop()?;
                                return Ok(());
                            }
                            (KeyCode::Esc, _) => {
                                self.close_spotlight();
                            }
                            (KeyCode::Enter, KeyModifiers::NONE) => {
                                self.close_spotlight();
                                if self.browser_cursor < self.browser_entries.len() {
                                    let (_, path, is_dir) = self.browser_entries[self.browser_cursor].clone();
                                    if is_dir {
                                        self.navigate_browser(path);
                                    } else {
                                        let meta = self.quick_metadata(&path);
                                        let title = meta.display_title().to_string();
                                        self.enqueue_tracks(vec![meta]);
                                        self.status_message = format!("Added to queue: {}", title);
                                    }
                                }
                            }
                            (KeyCode::Down, _) | (KeyCode::Tab, KeyModifiers::NONE) => {
                                self.spotlight_next_match();
                            }
                            (KeyCode::Up, _) | (KeyCode::BackTab, _) => {
                                self.spotlight_prev_match();
                            }
                            (KeyCode::Backspace, _) => {
                                if let Some(ref mut q) = self.browser_spotlight {
                                    q.pop();
                                }
                                self.jump_spotlight_to_first_match();
                            }
                            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                                if let Some(ref mut q) = self.browser_spotlight {
                                    q.push(c);
                                }
                                self.jump_spotlight_to_first_match();
                            }
                            _ => {}
                        }
                        continue;
                    }

                    if self.handle_input_key(key)? {
                        return Ok(());
                    }
                }
            }
        }
    }
}
