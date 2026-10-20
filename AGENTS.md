# Quickstream Agent Developer Guide (`AGENTS.md`)

This guide is intended for AI coding agents and automated development tools working on the **Quickstream** codebase. It documents architectural decisions, core invariants, technology constraints, module structure, and operational patterns.

---

## 1. Project Philosophy & Core Invariants

1. **Native Rust 2021 Architecture**:
   - Quickstream is a high-performance terminal music player and DLNA / UPnP network streamer written in **Rust (2021 Edition)**.
   - Crate dependencies are managed strictly via [`Cargo.toml`](file:///Users/jp/Development/quickstream/Cargo.toml) (e.g. `ratatui`, `crossterm`, `lofty`, `rusqlite`, `walkdir`, `clap`, `serde`).
   - **Do not introduce Python runtime dependencies or pip packages**: The early Python prototype (`quickstream/`, `quickstream.py`, `pyproject.toml`, `tests/*.py`) has been removed from the tree; it remains available in git history only. All active development, bug fixes, and features live strictly in `src/`.

2. **System Dependencies**:
   - `ffmpeg`: Required for audio format transcoding, sample extraction, and fallback metadata extraction.
   - `mpv`: Primary local playback backend via JSON-RPC IPC socket with dedicated event listener.
   - `ffplay`: Fallback local audio playback if `mpv` is not installed.

3. **Zero-Touch Library Invariant**:
   - Quickstream **MUST NEVER** write to, modify, delete, or rename files or directories in the user's music library or network shares (e.g. `/Volumes/Music`).
   - All persistent application data strictly complies with the XDG Base Directory specification:
     - Configuration: `~/.config/quickstream/config.json` (includes `last_playlist`, the playlist the add picker preselects)
     - Bookmarks: `~/.config/quickstream/bookmarks.json`
     - Legacy files (`~/.quickstream_config.json`, `~/.quickstream_bookmarks.json`, macOS `~/Library/Application Support/quickstream/`) are copied into the default `~/.config/quickstream` once per process (`paths::config_file` / `paths::bookmarks_file`, a `ConfigFile` with `read_path` and `write_path`), never into a folder `XDG_CONFIG_HOME` names. When the copy fails the app reads the legacy file and saves to the new path; a legacy file is never written. `SettingsManager`, `AppUI` (its `bookmarks_file`) and the web dashboard (bookmarks published in `NowPlayingState::bookmarks`, never read from the file per request) use that one answer.
     - Playlists: `~/.config/quickstream/playlists/<name>.m3u8` (`paths::get_playlists_dir()`; extended M3U8, UTF-8, LF, absolute track paths written unchanged, `#EXTINF` plus an `#EXTART` artist line per entry, one file per playlist). On parse, a path line keeps its trailing whitespace only when its entry has an `#EXTART` line (Quickstream's own files); entries without one (other players, hand edits) are trimmed. Track paths are normalized lexically with `playlist::normalize_path` (no `.` or `..`, no disk access, symlinks not followed) when a file is parsed and before `PlaylistStore::add`'s duplicate check. Every write, rename and delete goes through `PlaylistStore` and `playlist::playlist_file_path`, which refuses any path whose parent is not the playlists folder. A new playlist's file is created with `persist::create_new_file` (temp file, then a hard link, which never replaces), and a rename never replaces another file. The writer reports a failed create or rename back to the store over an mpsc channel (`persist::Reporter`); `AppUI::poll_playlists` then removes the playlist that couldn't be created (`Couldn't create "Mix": a file with that name already exists`, and the name is reserved; if the cursor was on it, it goes back to the playlist selected before the create) or restores the name a failed rename left the file under (`Couldn't rename "Old" to "New": …`). Until then the writer drops saves to that path, so it never overwrites the other file or leaves `New.m3u8` next to `Old.m3u8`. The writer also acknowledges every create and rename that worked (`persist::PersistReport::Done`), so the store knows which are still pending. Until one is settled, creating another playlist under a name it starts from or goes to, or renaming another playlist onto one, is refused (`Still renaming "Old" to "New", try again` / `Still creating "Mix", try again`, `StoreError::StillRenaming` / `StillCreating`), in the terminal and from the web dashboard alike, so two playlists never claim one file. A playlist whose own rename is still pending can't be renamed again, not even straight back, and gets the same `Still renaming` refusal: if that rename failed, the undo would restore a name the file had already left (a rename back and then on to a third name used to leave the playlist named `A` while its file was `C.m3u8`). It may be renamed while its own create is pending, and a deleted one frees its names. Missing-file checks never prune a playlist, and a file that fails to load is skipped and never rewritten.
     - Track Analysis Database: `~/.cache/quickstream/track_analysis.db` (SQLite)
     - Cache Indexes: `~/.cache/quickstream/music_index.jsonl`
     - Logs: `~/.local/share/quickstream/quickstream.log`

4. **Non-Blocking UI Invariant**:
   - Disk I/O, network SSDP discovery, audio decoding/analysis, metadata extraction, and library scanning **MUST** run asynchronously on background worker threads, worker pools, or Tokio tasks.
   - Communication with the main UI loop is performed using `std::sync::mpsc` channels and lock-free/atomic states. The Ratatui UI event loop must never block or drop below 60 FPS.
   - Tag reads from the browser metadata fetch, the queue metadata pool and playlist folder adds all go through `SharedTagReads` (`src/ui/tag_reads.rs`), which keeps one set of paths being read so no two workers read the same file at once. The browser and queue workers skip a path that is being read; a folder add waits for that read. Every read publishes its tags (on-disk index, `MetadataCache`, `metadata_tx`) before releasing its path, and a failed or panicking read still releases it. `AppUI::set_tag_reader` is the test seam.

5. **Dual Playback Architecture**:
   - Quickstream abstracts playback through the unified `AudioPlayer` trait (`src/player/mod.rs`):
     - `play(path, offset)` / `pause()` / `resume()` / `stop()` / `seek(seconds)` / `set_volume(level)` / `get_position()` / `get_state()` / `backend_name()` (the local backend actually playing, `mpv` or `ffplay` after a fallback; `None` for DLNA).
   - **Local Playback**: Implemented by `LocalPlayer` (`src/player/local.rs`) utilizing an `mpv` JSON-RPC IPC Unix domain socket with a dedicated listener thread, falling back to `ffplay` subprocess. Each `loadfile` carries a `request_id`; `MpvLoadTracker` takes the new playlist entry's id from mpv's answer (from the first `start-file` after it on mpv 0.33–0.37, whose answer has none) and only counts an `end-file` for that entry, so the late end of a replaced load is never the new load's; an mpv that names no entries (before 0.33) has its events taken for the newest load from that `start-file` on. An `end-file` with reason `error` counts as the track finishing only for a load started past 0:00 that mpv loaded and reports as `no audio or video data played` (a start at or past the file's real end; tag lengths can be estimates); every other error (`loading failed`, `unrecognized file format`, …) is a failure reported through `take_error` (`couldn't play <file> (<file_error>)`, shown as `Playback error: …`; `AsyncPlayer` passes it on). `PositionGate` holds back mpv's `time-pos` while it may predate the newest seek, load or stop, so a position mpv sent before handling one never puts the old time back or starts a relative seek from it (issue #92). `seek_to` shows its target at once and sends the seek with a request id (`SEEK_REQUEST_BASE` and up, never a load's); `time-pos` then waits for mpv's success answer to that seek, then a `seek` event, then `playback-restart` (mpv 0.41, playing or paused: answer, `seek`, `time-pos` at the target, `playback-restart`, and paused the landing position about 50 ms later; two seeks sent together: both answers, then `seek` and `time-pos` for each, one `playback-restart`), since a restart before the `seek` event can be an earlier seek's or the load's. Only the newest seek counts; an error answer or an `end-file` for the playing entry (as `MpvLoadTracker` counts it, never a replaced load's) ends the wait, and a seek that couldn't be sent puts the position back. After a `loadfile` (`LocalPlayer::begin_load`, tracker and gate under one lock) it waits until mpv has loaded the new entry (`file-loaded`), ended it or refused the load; after `stop`, until the playing entry's `end-file`. A new load or stop replaces the wait, and 2 s without confirmation ends any (`SEEK_CONFIRM_TIMEOUT`, `LocalPlayer::set_seek_confirm_timeout` is the test seam). Lock order `load`, `position_gate`, `position`; the listener reads each message and its `time-pos` under one guard.
   - **Output switch** (`AppUI::switch_to_local_player` / `switch_to_upnp_player`, the web dashboard's `SelectDevice` too, issue #48): the new output resumes where the old one was, decided by `player::resume_position(position, duration)` from numbers read before the old player stops (`AppUI::take_hand_off`; its length while it plays or pauses the current track, else the track's tag length). A pending debounced seek's target is the position; a pending debounced skip plays its target from 0:00 on the new output (never on the old one), as applying it would. Inside the last second (`RESUME_END_MARGIN_SECONDS`) or past the end, or when the old player had already reported the track finished before the event loop handled it, it is `Resume::NextTrack`, handled by `handle_track_finished` as a natural end (stop-at-end, repeat-one; a paused track's successor loads paused, also when a playlist fill holds the skip); the status that leaves (`Playback error: …`, `Waiting for the rest of …`) stays, else `Active output: …`. An unknown length (0) keeps the position. When the player no longer holds the queue's current track (`AppUI::player_track`, the file it was last told to play: the track removed after it ended, or the queue cleared and refilled while it played on), the current track starts from 0:00. `AppUI::set_output_player_factory` is the test seam for the player a switch moves to.
   - **DLNA / UPnP Streaming**: Implemented by `UPnPPlayer` (`src/player/upnp.rs`) sending SOAP XML payloads (`AVTransport` / `RenderingControl`) to discovered network renderers, while `WebServer` (`src/web/`: listener in `server.rs`, streaming in `stream.rs`) serves audio over HTTP with Byte-Range support (`206 Partial Content`) and continuous streaming (`/stream/continuous-<nonce>.wav`, armed per UPnP continuous session via `StreamServerState::arm_continuous_session()`; other continuous paths return 404). A track the continuous stream starts with an unknown length (placeholder metadata) takes it from the published web queue while it plays, once its tags have been read: `AppUI::update_web_state` offers it as it publishes the queue (`ContinuousStreamController::learn_published_duration`, matched on the decoded track's queue index and path, which the stream sets together with its length; lock order web state, then `current_duration`), so it never waits on the stream's socket writes (a renderer reading at real time can block them for seconds); the write loop also checks after its writes (`learn_duration`); `UPnPPlayer::seek_to` clamps continuous-stream seeks to that length once known, and to 0 and up until then.
     - Skips and seeks (`ContinuousStreamController::skip_to_index` / `seek_to`) set one pending `StreamRequest` (a queue index and a start position), then kill the decoder. `skip_to_index` also sets `current_track_index` at once (lock order: the web state, then the request, then `current_track_index`), so the UI, which follows that index every tick (`AppUI::sync_continuous_stream_index`), stays on the skip while the stream is blocked writing, and a second Next goes on from it. A seek is for the track skipped to when a skip is pending, else for the one `current_track_index` names; the stream moves that index only when the next track starts (not when a track ends), so a seek that arrives as a track ends restarts that track at the offset and is never applied to the next one. The stream takes the request in one step as it picks its next track, so none is lost or applied to the wrong track; a request made while the queue it sees is empty is dropped (and the index reset to 0), so the next queue starts at its top, from its start (the UI publishes the queue before it sends a skip). A decoder output that ends while a request is pending is not the end of the track, so a kill during ffmpeg's startup never moves on to the next track or sends `ShuffleQueue` (sent only when the last track really ends, and only after a wrap that streamed audio). The decoder is registered before the loop's first request check, so a request that arrived just before it was spawned still stops it. A renderer's reconnect supersedes the old connection of the session (`active_conn_id`): the old one checks that under the lock it takes requests with, so it never takes the new one's request, and a decoder is registered with the connection that started it, so one connection never overwrites, leaves running or kills the other's (a superseded connection's decoder is stopped when the new one registers its own). Ending or arming a session (`disarm_session`, so `close_active_streams`, and `arm_session`) drops a pending request, so a skip or seek the old session never acted on doesn't move the next one off the index the UI left it on. `pending_request()` shows what is pending (`quickstream::web::StreamRequest`); `set_decoder` (the ffmpeg to run, `ffmpeg` on `PATH` by default) is the test seam.
     - With nothing to decode the stream sends silence at real time (176,400 B/s plus a 100 ms lead, measured from an `Instant`, `SilencePacer`): while the queue is empty, when a look through the whole queue (paths copied out of the web state, never statted under its lock) finds no file, and once a whole queue's worth of tracks in a row gave no audio (missing files passed over, decoder runs that ended without output: files ffmpeg can't decode or a decoder that can't start; after that one file is tried per recheck). A run that gave any audio starts that count again, however it was stopped (a skip, a seek, a read error), as do a request and a changed queue. While tracks give no audio, a file the stream tries without being asked for (the rest of the walk, every recheck) is not shown as playing (`current_track_index`, the web state's title and queue index) until it gives audio, so now playing doesn't step through the queue over silence. The missing files are logged once per queue (a queue that replaces it is logged too), a decoder that can't start once until audio plays again, and the queue is looked through again every second, or as soon as it changes (another length at once, other files of the same length at the next recheck) or a request arrives. A failed silence write ends the stream, so a renderer that left is noticed.

6. **SQLite Schema Migration Standard**:
   - The track feature database (`src/analysis/db.rs`) uses SQLite `PRAGMA user_version` for incremental migrations.
   - When modifying tables or adding columns, append a step to the `MIGRATIONS` array and bump `CURRENT_SCHEMA_VERSION` (a compile-time assert keeps them equal). `MIGRATIONS[i]` upgrades `user_version` `i` → `i + 1`; each step runs in its own transaction together with its `user_version` bump:
     ```rust
     const MIGRATIONS: &[&str] = &[
         /* v1 */ "CREATE TABLE IF NOT EXISTS track_features (...); ...",
         /* v2 */ "ALTER TABLE track_features ADD COLUMN ...;",
     ];
     ```
   - Never edit or remove an existing step, and never change `journal_mode` inside a step. Connection pragmas (`journal_mode=WAL`, `synchronous=NORMAL`, `busy_timeout`) are applied on every open.
   - A database whose `user_version` is newer than the binary is refused (`SchemaTooNewError`) without being modified.
   - Never drop existing user analysis tables or risk data loss.

---

## 2. Codebase Structure & Module Map

```
quickstream/
├── Cargo.toml            # Package manifest and Rust crate dependencies
├── src/
│   ├── main.rs           # CLI argument parsing (clap), startup flags, signal traps, app bootstrap (theme::init from COLORTERM / TERM / NO_COLOR)
│   ├── lib.rs            # Library entrypoint and module exports
│   │
│   ├── ui/               # Terminal User Interface (Ratatui + Crossterm), see section 3
│   │   ├── mod.rs        # AppUI state and `new`, terminal setup/restore and the panic hook, shutdown flags, settings context and status, track features polling; AppUI's methods live in the modules below, by area (issue #26)
│   │   ├── event_loop.rs # `run` / `event_loop`: poll workers, draw, wait up to 100 ms for a key; shutdown_workers, poll_shutdown_request
│   │   ├── render.rs     # AppUI::render (chrome, tab body, modal over the dimmed screen), modal_open
│   │   ├── keys.rs       # Key routing: handle_input_key (modals, global hotkeys), handle_tab_key_with_modifiers (tab and pane keys), open_fuzzy_search
│   │   ├── browser.rs    # File browser: entry labels, refresh_browser, directory scan and cache, browser metadata fetch, navigate_browser, spotlight, browser paging
│   │   ├── queue_ops.rs  # Queue edits (enqueue / insert / remove, cursor), queue metadata pool, poll_background_metadata, queue paging
│   │   ├── playback.rs   # Debounced seeks and track skips, play_current_track / play_next_track / play_prev_track, handle_track_finished
│   │   ├── autopilot.rs  # Autopilot and Seed Radio: poll_background_autopilot, start / reshuffle / stop, mix picker choice, reset_playback_session
│   │   ├── output.rs     # Output switching (switch_to_local_player / switch_to_upnp_player, HandOff of the playing track), web server binding (required_web_bind, ensure_web_server_started, sync_web_server)
│   │   ├── web_cmds.rs   # Web dashboard: handle_web_commands, web_allowed_roots, update_web_state
│   │   ├── playlists.rs  # Tab 5 Playlists: PlaylistsUi, PLAYLISTS / tracks panels, name prompt (3b/3c) / delete confirm (3d) / add picker (3e, scrolls), L key, play & queue
│   │   ├── queue_tab.rs  # Tab 2 LIBRARY / QUEUE panels: rows mapped at draw time, IN QUEUE set + totals + column layout from the whole list, cached per revision; split_panes for tabs 2 and 5
│   │   ├── tag_reads.rs  # SharedTagReads: tag reads shared by browser/queue/folder-add workers, in-flight path set
│   │   ├── workers.rs    # WorkerRegistry: the UI's background threads, joined on shutdown (stragglers detached after a deadline)
│   │   ├── features.rs   # TrackFeaturesWorker: current track's analysis features (BPM/key/energy) read on a worker, retried every 5 s, refreshed every 30 s
│   │   ├── devices_tab.rs # Tab [3] Devices (2d): OUTPUT DEVICES table (NAME/TYPE/ADDRESS/ACTIVE), FOUND · SSDP footnote, SCANNING spinner, DEVICE detail panel from 110 columns
│   │   ├── settings_tab.rs # Tab [4] Settings (2e): grouped SETTINGS list (PLAYBACK / LIBRARY / WEB), ◂ VALUE ▸ cursor, config-file footnote, option panel (●/○, description, web address) from 110 columns, inline startup home editor, read-only TRACK ANALYSIS row (AnalysisWorker::progress)
│   │   ├── now_playing.rs # Tab [1] Now Playing (2b/3f): NOW PLAYING + SIGNAL panels, big numerals, bar, UP NEXT (from PlaybackQueue::upcoming); Up Next summary cached per revision, queue mode and continuous-stream state
│   │   ├── search.rs     # SearchModalState, streaming fuzzy search (fuzzy-matcher / SkimMatcherV2), the words each row ranked on in its title / artist, row_marks (characters to underline) and long_lengths, cached per ranked list
│   │   ├── search_view.rs # Search modal (2h): query row + counts, SCANNING note, RANKED MATCHES (visible rows only), TRACK panel from 90 columns; headerless matches on short screens
│   │   ├── mix_picker.rs # Mix picker (2i): rows from MixProfile::picker_profiles (title, description), ●/○, DIM 1–5; scrolls to the cursor on short screens
│   │   ├── help.rs       # Help modal (2g): key sections as data, full 96×32 layout or compact (≤ 22 rows, title in the border), inside the gutters (80 columns wide at 80–83)
│   │   ├── theme.rs      # Telemetry colour tokens: ColorDepth (COLORTERM / TERM / NO_COLOR: RGB, 256-colour greys, 16 named ANSI colours or Mono), Palette (TRUECOLOR / INDEXED / BASIC / MONO, styles as methods), theme::init (once, at startup), style constructors (`bold()` is the one bold INK style)
│   │   ├── kit.rs        # Key row, volume meter, progress bar, panel header (panel/PanelSpec; panel_spaced / keeps_spacer with a PanelFit per panel, panel_text_width, message_rows for short terminals), visible_window, list_row columns (underlined characters: Col::underline / underline_chars), width-aware fit / clip / visible_tail / wrap_clipped, duration (MM:SS / H:MM:SS), grouped (`4,812`), big_digits (3-row numerals), spinner (clock-driven `⣾` frames), ClockCache, modal pieces: clear, dim_buffer, centered, modal_area (centred inside the 2-column gutters, MODAL_MARGIN), modal_frame/ModalSpec (subtitle after the chip), modal_title, modal_rule (`────── ▪`)
│   │   ├── chrome.rs     # Screen chrome: ChromeLayout rows, header, tab row (tab_titles), strip (player_state_glyph, playback_times), key row (tab_key_hints, modal_key_hints, status message)
│   │   └── screen.rs     # WidthTier (<80 / 80–89 / 90–109 / ≥110) and Screen (tier, short) per frame
│   │
│   ├── player/           # Audio playback abstraction layer
│   │   ├── mod.rs        # AudioPlayer trait and PlaybackState enum
│   │   ├── local.rs      # LocalPlayer: mpv JSON-RPC IPC socket client & ffplay subprocess fallback
│   │   └── upnp.rs       # UPnPPlayer: SOAP XML generation, AVTransport / RenderingControl client
│   │
│   ├── analysis/         # Intelligent Autopilot & Acoustic Analysis
│   │   ├── mod.rs        # Analysis module exports & MixProfile definitions
│   │   ├── bpm.rs        # Peak-picking autocorrelation BPM detection
│   │   ├── harmonic.rs   # Musical key parsing and Camelot wheel compatibility scoring
│   │   ├── mixer.rs      # DjMixer::sequence_mix (harmonic flow, BPM gradient, artist/album spacing); MixProfile picker data (picker_profiles, picker_index, title, description)
│   │   ├── db.rs         # SQLite persistent feature store (~/.cache/quickstream/track_analysis.db)
│   │   ├── online.rs     # Lightweight metadata/tag enrichment via ureq HTTP client
│   │   └── worker.rs     # AnalysisWorker: background analysis into the feature store (tags first, ffmpeg when no BPM tag); AnalysisQueue (queued tracks before autopilot candidates, at most MAX_PENDING_CANDIDATES = 200 of those, each path once a session); progress() counters (atomics); a panicking analysis counts as failed and the thread goes on; spawn_with_analyser is the test seam
│   │
│   ├── library.rs        # Recursive walkdir scanner, progressive autopilot streamer, JSONL cache, MetadataCache (with revision and per-folder folder_revision)
│   ├── metadata.rs       # Lofty audio metadata extraction, cover art decoding, path heuristics
│   ├── playlist.rs       # Pure M3U8 playlist model: parse/write, name validation, safe file paths, queue plan
│   ├── playlist_store.rs # In-memory PlaylistStore (sorted), write-through to disk via PersistHandle
│   ├── persist.rs        # Background atomic writer: ordered save / create / delete / rename (PersistHandle), failures reported back
│   ├── queue.rs          # PlaybackQueue (private fields, revision() bumped by every mutation, drawn from one process-wide counter so new/cloned queues never repeat one), upcoming(mode, wrap) (what plays next, with QueueEnd), FastRng (SplitMix64 deterministic PRNG), shuffle & loop
│   ├── web/              # Embedded web server: every pub item re-exported as crate::web::X
│   │   ├── mod.rs        # Module docs and the pub re-exports
│   │   ├── state.rs      # NowPlayingState and the shared types (WebQueueItem, WebPlaylists, …), WebCommand, PlaylistEdit, WebReply
│   │   ├── server.rs     # WebServer / WebServerHandle / WebBind / ServerLimits: listener, connection limits (ConnectionPermit), request parsing (read_request_head / read_request_body, RequestHead)
│   │   ├── api.rs        # handle_client: route dispatch, REST API (/api/status, /api/browser, /api/playlists, /api/action incl. playlists), cover art, the dashboard page
│   │   ├── stream.rs     # Byte-range file streaming (parse_byte_range, serve_audio_stream), continuous WAV stream (serve_continuous_stream, ContinuousStreamController), StreamServerState
│   │   └── assets/
│   │       └── index.html # Dashboard HTML/CSS/JS, served as NOW_PLAYING_HTML via include_str!
│   ├── web_access.rs     # Web access control: pairing token, Host/Origin/Content-Type checks, LAN peers only `/stream/*` while remote access (or the web server) is off, allowed folders
│   ├── discovery.rs      # SSDP UDP multicast discovery (`SSDP_MULTICAST_ADDR`, 239.255.255.250:1900), XML device parsing
│   ├── settings.rs       # User preferences model, SETTING_DEFINITIONS (with SettingGroup), named indexes (`setting::HOME_DIR`, …) and SETTING_DISPLAY_ORDER, SettingsContext (bookmarks, music folder, home, web port from the caller), options / cycle_setting / short_value, web_url, cached LAN address (prefetch), lookup_music_dir, config.json persistence
│   ├── paths.rs          # XDG Base Directory resolution and backward-compatible migration: config_file() / bookmarks_file() resolve a ConfigFile (read_path, write_path) once per process (later lookups never touch the disk or retry); legacy files are copied only into the default ~/.config/quickstream (compared with symlinks resolved; skipped and logged when XDG_CONFIG_HOME points elsewhere) with persist::create_new_file (never replaces, no half-written file); after a failed copy the legacy file is read (never written, no .bak beside it) and saves go to the new path; display_path (`~` for home, original case)
│   └── logger.rs         # Non-blocking rotating file logger (~/.local/share/quickstream/quickstream.log)
│
└── tests/                # Integration test suite (Rust)
    ├── test_autopilot.rs     # Autopilot progressive streaming, candidate gathering, DJ mix profiles; a profile mix sends its unqueued, unanalysed candidates once (`AutopilotEvent::Candidates`), a row with a stale mtime counting as unanalysed; `analysis_candidates` keeps at most `MAX_PENDING_CANDIDATES`, matched first
    ├── test_analysis_worker.rs # AnalysisWorker: tags / acoustic analysis, resumable, never writes the library, ffmpeg failures and timeouts (#19), prompt shutdown; AnalysisQueue order (queued tracks first, a waiting candidate that reaches the queue moves ahead), candidate bound, no path twice; a track queued during a candidate burst analysed next; one run's candidates past the bound keep its first; a panicking analysis counted failed, the worker goes on; candidates from the UI reach the worker, not the queue
    ├── test_browser.rs       # File browser navigation, bookmarks, cache, and metadata toggles
    ├── test_devices_tab.rs   # Tab 3 Devices (2d): table columns, active ●/ACTIVE, any IPv4 host:port whole at every width, IPv6 `[host]:port` keeping its port, MODEL left out when it repeats the name, cursor + j/k wrap, FOUND · SSDP footnote, SCANNING note, detail panel at 120 / none at 100, no inactive volume, 120×40/100×30/80×24, narrow widths keep the cursor row, under 24 rows the DEVICE panel's fields take part in the spacer decision
    ├── test_discovery.rs     # SSDP discovery packet parsing and device extraction
    ├── test_fake_exec.rs     # common::fake_exec::write_script: exact text, mode 755, replaces; Linux: a written script starts while other threads spawn processes (no ETXTBSY)
    ├── test_local_player.rs  # mpv / ffplay local audio backend lifecycle
    ├── test_local_player_seek.rs # Fake mpv: a time-pos sent before mpv handled a seek never shows (relative seeks start from the target), consecutive seeks keep the newest target, a restart before the `seek` event or another entry's end-file keeps the wait, positions taken again after an error answer, the playing entry's end-file, a new load or the time bound; a seek that couldn't be sent keeps the position; the old track's positions never show after a load (until the time bound when unanswered) or a stop
    ├── test_local_player_resume.rs # play_at start positions (mpv `start` option, ffplay `-ss`, slow sources); an mpv start past the file's end counts as finished, a missing or corrupt file started past 0:00 is an error (not finished), a replaced load's late end-file is ignored (fake mpv); `MpvLoadTracker` fed each mpv generation's messages (entry id in the answer, only in the events, none at all)
    ├── test_output_switch.rs # resume_position; switching output (FakePlayer outputs, no mpv or renderer) mid-track resumes, in the last second / past the end moves on (loop, repeat-one, stop at end, paused, tag length when the player has none, an end the old player reported but the event loop hadn't handled; a stopped player stays stopped), pending skip / seek (keys and web), current track removed or queue refilled, a failed next track's error kept
    ├── test_metadata.rs      # Lofty audio header parsing, duration, path heuristics; placeholders leave audio properties unknown
    ├── test_now_playing.rs   # Tab [1] Now Playing: numerals, bar, SIGNAL rows/omission, 120×40/100×30/80×24, UP NEXT per mode (repeat-one, loop wrap, stop end, continuous stream), SIGNAL vs UP NEXT at 120×30, same length as the strip, placeholder metadata, pending skip, autopilot, no DB on render
    ├── test_big_digits.rs    # kit::big_digits glyphs (2a style sheet), colon, widths
    ├── test_track_features.rs # Features worker: current track only, stale results dropped, retry, never blocks, shutdown; web BPM/key
    ├── test_paths_migration.rs # Legacy config migration: none into XDG_CONFIG_HOME (logged), into ~/.config/quickstream when unset or named through a symlink, a failed copy never returns a legacy write path, loads the legacy settings / bookmarks and the first save carries them to the new path (legacy bytes unchanged), a later lookup never copies again, a corrupt legacy file gets no .bak beside it (HOME a temp dir)
    ├── test_playlists.rs     # M3U8 parse/write round trips, name rules, safe paths, queue planning
    ├── test_playlist_store.rs # Store create/add/remove/move/rename/delete, case-only rename, load skips, file_path_of during a pending rename
    ├── test_playlists_ui.rs  # Tab 5 tab switching, two-pane navigation, rendering, modal hook, help
    ├── test_playlists_edit.rs # Name prompt, create/rename/delete, K/J move, d/x remove
    ├── test_playlists_add.rs  # L key, add picker, folder adds, last_playlist
    ├── test_playlists_play.rs # Missing-file checks, Enter replace+play, a append, all-missing; an output switch while the rest is checked keeps a paused track paused
    ├── test_search.rs        # Multi-threaded fuzzy search, scoped scan, and modal navigation
    ├── test_ui_search_mix.rs # Search modal (2h) at 120×40 / 100×30 / 80×24: counts, SCANNING, underlined matches (only in the field a word ranked on), no rescans or match work on redraw, cursor, TRACK panel only from 90 columns, fields without data left out; mix picker (2i) rows from MixProfile data, ● on the current profile (none, and a `NOW:` note, for Seed Radio / Focus), keys unchanged and following the profile list; short screens (40×12, 80×12, 70×10): search draws its matches (no headers) with the cursor in view, the mix picker scrolls to the cursor, both footers keep `ESC CANCEL` whole
    ├── test_settings_model.rs # Settings groups and display order, named indexes match their keys, options vs cycle_setting, short values, bookmarks from memory, j/k order + wrap, LAN address off the render path and looked up early (startup, web rows), web_url, values name the context's web port (none without a server) and the app's running server, home from the context, music folder from its worker (left out until found), the pairing link added to the status once known (not over a newer status; the LAN-binding end-to-end test is `#[ignore]`)
    ├── test_settings_tab.rs  # Tab 4 Settings (2e): grouped order + headings, cursor bar + ◂ ▸, config path footnote, option panel ●/○ + descriptions at 120 / none at 100, startup home choices from memory, web address case, bound web port (not the configured one) in the value, option panel and footnote, inline home editor (q is text), web address at every width (compact rows: one, two, start cut), whole on short terminals (option panel from 110 columns, two compact rows under it), long startup home cut from its start, key row vs handlers, 120×40/100×30/80×24, scrolling, narrow widths, TRACK ANALYSIS row (a real worker with a fake ffmpeg: counts at 120×40 / 100×30 / 80×24 / 80×20, zeros left out, `IDLE`, cut from its end, MID, never the cursor)
    ├── test_ui_chrome.rs     # Theme palettes (colour depth from COLORTERM / TERM / NO_COLOR: 16-colour terms Basic, 8-colour and monochrome terms Mono; BASIC tokens apart on 8 backgrounds, the screen under a modal in DIM; MONO all `Reset` with reverse / bold / faint, styles over faint cells not faint, the error chip underlined), key row trimming (fit_top_hints / whole_key_row: only whole hints), meters, text fitting, width tiers; rendered chrome; each modal's footer keys in the key row in the same words
    ├── test_ui_theme_init.rs # theme::init picks the palette once per process (own binary, so the other UI tests keep the TRUECOLOR default)
    ├── test_ui_theme_mono.rs # Mono (NO_COLOR, 8-colour and monochrome terminals; own binary): every tab and modal drawn without colour, cursor bar / tab and modal chips / keycaps / search's block cursor in reverse video, the screen under a modal faint, the key row under a modal and the spotlight footnote's keycap and query not faint, crossterm output under NO_COLOR never resets attributes mid-frame
    ├── test_ui_playlist_modals.rs # Name prompt (3b; 3c error replaces the keys, in ERROR; long errors wrap), delete confirm (3d: ERROR border, chip, Y keycap; long names keep the count), add picker (3e; 40 playlists scroll with the cursor visible) at 120×40 / 80×24 / 40×12 and tiny sizes
    ├── test_ui_modals.rs     # Modal frame (dim_buffer, centered, modal_frame: DIM/ERROR border, title chip, subtitle, padding, compact, tiny sizes), the screen dimmed under every modal except the key row (its keys and status stay readable), every modal inside the 2-column gutters at 120×40 … 40×12 (kit::modal_area; the help takes 80 columns at 80–83), help at 120×40 (2g) / 80×24 (compact) / small sizes, every help key and description (whole from 80 columns)
    ├── test_ui_panels.rs     # Panel header rows/colours, compact footnote, visible_window vs ratatui List, list_row columns, cursor, ▸ in its own column, keeps_spacer (every shown panel, hidden panes ignored), message_rows
    ├── test_queue_upcoming.rs # PlaybackQueue::upcoming matches next_track per mode; continuous stream wraps and reshuffles
    ├── test_queue_revision.rs # PlaybackQueue::revision bumps on every mutation, not on reads or no-ops, never shared by two queues; truncate_after
    ├── test_ui_short.rs      # Under 24 rows and tiny sizes (80×23 … 20×8), every tab and modal: GROUND, gutters, whole active chip, key row last with whole keys, strip, cursor row on screen, spacer rows given up only when needed, plain-digit time when the numerals don't fit (decided from the track's length, so it holds across the hour mark), only whole keys on the key row at every width from 1 to 25 on every tab and pane (`N QUEUE` whenever it fits) and on every modal's footer from 1 to 60
    ├── test_ui_matrix.rs     # Render matrix: every tab (both panes of tabs 2 and 5) and every modal (help, search with results, mix picker, name prompt and its error, delete confirmation, add picker, Settings home editor) of one realistic app at 120×40, 100×30, 85×30, 80×24, 70×24, 80×20, 40×12 (1×1 and 20×8 for no panic): GROUND, gutters (a cursor bar's leading cell may sit in the inner left one; help at 80–83 columns takes 80), whole active chip, the key row's own keys and `? KEYS` on tabs only, dimmed outside a modal except the key row, no help label without a key under it; failures name the case and size. Edge states (Now Playing empty, playlists empty / loading / failed, an empty playlist's tracks, Devices scanning / nothing found with no renderers, Settings on a web row with the address under the list) at short sizes 120×22 … 20×8: no message or address is cut while the blank row under the rule is kept, and each shows whole where it fits without it
    ├── test_ui_lists.rs      # Tab 2 and tab 5 panels: columns, IN QUEUE / ▪ / ▶, cursor vs ▸ (never hiding ▶ / ▪), columns stable while scrolling, width rules, spotlight (real subfolders only), CJK, durations (H:MM:SS), folder duration (own-folder recomputes only), MISSING FILE, playlist file footnote
    ├── test_shared_tag_reads.rs # No concurrent duplicate tag reads; skipped/waiting requests still get tags
    ├── test_upnp.rs          # UPnP SOAP XML generation, AVTransport state machine, continuous stream
    ├── test_continuous_stream.rs # Continuous stream: armed session path and peer, gapless queue and shuffle on wrap, length learned mid-track, seek clamp; with a fake ffmpeg (no output until killed): seek / skip during decoder startup, seek on the last track (no ShuffleQueue), skip at a track boundary; silence at real time on an empty queue, on an all-missing one (logged once, renderer leaving noticed, a file that comes back plays) and on one ffmpeg can't decode (bounded decoder runs, no ShuffleQueue, a file added to the queue plays once it gives audio, rechecks not shown as playing); no ffmpeg (failure logged once); a missing queue replaced by another (each logged once); audio resets the silent count when skipped out of; two Nexts while the stream is blocked writing land two tracks on; a seek just after a track ends restarts that track, not the next; a request made while the queue is empty is dropped; a reconnect stops the old connection's decoder; a request pending when the session ends is not carried into the next
    ├── test_web.rs           # Embedded web server, byte ranges, REST API, and command dispatch
    └── test_web_playlists.rs # Web playlists: snapshot, GET /api/playlists, play/append, edits, path checks
```

TUI tests render through the shared helpers in `tests/common/tui.rs` (`render`, `rows`, `find_row`, `find_text`, `cell_style`, `text_styles`): find chrome rows by their text, not by row number. `tests/common/fake_player.rs` (`FakePlayer`) stands in for the player when a test needs a given position, play state or backend; its `calls` log records `play` / `play_at` / `pause`. `tests/common/fake_exec.rs` (`write_script`, Unix only) writes the fake `mpv` and `ffmpeg` scripts tests run (see section 5).

The background writer's create, delete and rename tests (used by `PlaylistStore`), including the failure reports, are in the existing `tests/test_persistence_worker.rs`, which the map above does not list.

---

## 3. Terminal UI Architecture & Ratatui Guidelines

The TUI is the Telemetry design: style sheet 2a, screens 2b–2i at 120×40, states, 80×24 screens and width rules in 3a–3h. `AppUI::render` (`src/ui/render.rs`) draws the chrome (`chrome.rs`), the active tab's body and the open modal; each has its own module (section 2). Tabs are named here by their keys `[1]`–`[5]`; in code `active_tab` is 0–4.

### 3.1 Ground rules
- **No I/O on the render path or the UI thread.** Frames and key handlers read memory only:
  - tags from the queue and `MetadataCache`, features from `TrackFeaturesWorker`, the analysis counters from `AnalysisWorker::progress` (atomics), devices from `DeviceManager`, Music Homes from `AppUI::bookmarks`
  - the clock through `kit::ClockCache` (read at most once a second), the LAN address from its cache
  - work over a whole list is cached per revision (`PlaybackQueue::revision`, `PlaylistStore::revision`, `MetadataCache::folder_revision`), and only the visible rows are laid out (`kit::visible_window`), so a frame never walks a 5,000-entry folder.
- **Never make data up.** A field without data is left out: no bit depth (sample rate in kHz only), no folder child counts, no inactive renderer's volume, a length only when known.
- **Styles come from `ui::theme` only.** No `Color::` outside `theme.rs`, no `Modifier::REVERSED` (the cursor's colours are explicit; the one exception is `theme.rs` in the Mono depth, which has no colours to name, 3.2), no ratatui `Clear` (it leaves the terminal's default background; use `kit::clear`), no borders on panes (only modal frames have one).
- **Case.** Labels, titles, artists and values are UPPERCASE through `kit::upper` (full Unicode mapping: `ß` → `SS`, so text can get longer). Kept in their own case: paths (`paths::display_path`, `~` for home), file names in Filenames mode, typed text, the web address and pairing link, status messages, and the sentences of panel messages and modal bodies.
- **Width-aware text.** Pad and cut by display width with `kit::fit` / `kit::clip` / `kit::visible_tail` / `kit::wrap_clipped` (CJK takes 2 columns; a wide character that would straddle a cut is dropped and the gap padded), so columns stay aligned. Paths are cut from their start with `…` (`visible_tail`) so the last folder shows; other text from its end.
- **Durations.** Every length and position (list rows, totals, the strip, Now Playing) is `kit::duration`: `MM:SS` under an hour, `H:MM:SS` from an hour (`1:15:00`).

### 3.2 Theme (`src/ui/theme.rs`)

| Token | Used for | RGB | 256-colour | Basic (16) | Mono |
|---|---|---|---|---|---|
| GROUND | background of every cell | `#141413` | 234 | Black | terminal's own |
| INK | primary text, focus, cursor bar | `#E8E8E4` | 254 | White (bright) | terminal's own |
| MID | secondary text, unfocused labels | `#9A9A95` | 247 | Gray | terminal's own |
| DIM | indices, notes, footnotes | `#5F5F5B` | 240 | DarkGray | faint |
| RULE | rules, keycap background, the screen under a modal | `#2E2E2C` | 236 | Blue (the screen under a modal: DarkGray) | faint |
| ERROR | errors and the delete confirmation only | `#ED7473` | 210 | Red | bold |

- **Colour depth:** `ColorDepth::from_env(COLORTERM, TERM, no_color)`, in this order:
  - `Mono` when `NO_COLOR` is set and not empty (`ColorDepth::no_color`, as crossterm reads it; crossterm then writes no colours).
  - `TrueColor` (RGB) for `COLORTERM` `truecolor` or `24bit` (any case; `ColorDepth::from_colorterm`), whatever `TERM` says.
  - crossterm 0.28 writes even a named colour as a 256-colour index (`ESC[38;5;Nm` / `ESC[48;5;Nm`), never `30`–`37` / `90`–`97`, so the named colours only help a terminal that reads those indexes:
    - `Basic` (the 16 named ANSI colours) for `linux` and its other `linux-*` entries, `screen.linux` and any `*-16color` (never a `*256color` entry).
    - `Mono` for terminals that show 8 colours or none: `*-8color`, `xterm-color`, the `ansi`, `vt52` / `vt100` / `vt102` / `vt220` / `vt320`, `cons25` and `dumb` families, and the monochrome `linux-m*`. They can't show the indexes (with 8 colours DIM, index 8, can fall back to black on the black ground).
  - Otherwise `Indexed` (the nearest 256-colour greys), plain `xterm`, `screen` and `tmux` included.
  - `main.rs` calls `theme::init(ColorDepth::detect())` (reads the three variables) once at startup (a `OnceLock`: only the first call counts).
  - Until then, and in tests, `theme::palette()` is `Palette::TRUECOLOR`. `tests/test_ui_theme_init.rs` tests `init` in its own binary, `tests/test_ui_theme_mono.rs` the Mono screen in its own.
- **Basic:** the backgrounds (GROUND, RULE, INK under the cursor, ERROR under the delete chip) are normal ANSI colours, so they stay apart where bright backgrounds turn normal (the Linux console). RULE is blue: a bright-black RULE dropped to black there and keycaps vanished. The screen under a modal is DarkGray (DIM, `Palette::under_modal`), not the blue RULE, so it stays behind the modal's own DIM text.
- **Mono:** every token is `Color::Reset`, so a frame never changes colour from cell to cell (under `NO_COLOR` crossterm's colour command is an empty `ESC[;m`, which also resets bold and reverse video). Attributes carry what colour would: the cursor, chips and keycaps in reverse video, ERROR bold, the error chip (delete confirmation, its `Y` keycap) bold reverse video and underlined, DIM / RULE / the screen under a modal faint. They compose as colours do: every style that isn't faint clears faint (so the key row drawn over the screen under a modal, or a footnote's keycap and query over DIM, isn't faint), and the faint ones clear reverse video and bold.
- **Styles** (the free functions are `palette()`'s `Palette` methods): `base()` INK on GROUND (every cell), `ink()` `mid()` `dim()` `rule()` `error()`, `bold()` (the one bold INK style), `label(focused)` (bold INK, else MID), `cursor()` (GROUND on INK, bold: the cursor bar, the active tab chip, a block cursor; Mono: reverse video, bold, never faint), `chip(error)` (a modal's title chip, GROUND on ERROR when `error`; Mono: reverse video, bold, and underlined when `error`), `keycap()` (INK on RULE; Mono: reverse video), `dimmed()` (the screen under a modal: RULE on GROUND; Basic: DIM on GROUND; Mono: faint).

### 3.3 Screen layout (`src/ui/chrome.rs`, `src/ui/screen.rs`)
- `render` fills every cell with `theme::base()`, takes `Screen::new(area)` once per frame (`tier`, `short`), then places the parts with `ChromeLayout`.
- 2-column gutters on both sides (none on screens 4 columns wide or less). Nothing is drawn in them, except a cursor bar's leading cell (inner left column) and the help at 80–83 columns.
- **Rows, 24 and taller:** margin, header, blank, tab row, gap (2 rows from 30 rows, else 1), tab body, blank, rule (`─` in RULE) and strip, blank, key row, margin. Now Playing has no rule or strip.
- **Rows, under 24 (`Screen::short`):** tab row (with the header's right-hand part at its end), body, strip (from 3 rows; not on Now Playing), key row on the last row. No margins or gaps.
- **Width tiers** (`WidthTier::from_width`, mockup 3h):

| Tier | Columns | What changes |
|---|---|---|
| `Full` | ≥ 110 | Side panels: SIGNAL `[1]`, DEVICE `[3]`, option panel `[4]`. Tab label `QUEUE & LIBRARY`. Now Playing title under the numerals. |
| `Medium` | 90–109 | Side panels go; their facts join a footnote or the tech line. Tab label `QUEUE`. |
| `Compact` | 80–89 | Short tab labels (`1 NOW`). Artist (and library format) columns drop. Footnotes move to the label row (`PanelSpec::compact`). Two-pane gap 2. Strip without the artist. At most 5 keys. `VOL <n>` instead of the volume meter. |
| `Narrow` | < 80 | As Compact, and tabs `[2]` and `[5]` show only the focused pane (`h` / `l` switch it). |

- **Short terminals (under 24 rows):**
  - Every tab treats `short` like Compact: footnotes go to the label row (tab `[5]`'s file footnote drops).
  - Spacer rows go first (3h), but only when needed: a panel gives up the blank row under its rule when what it draws would not fit with it (`kit::panel_spaced`, `kit::keeps_spacer`). Each caller passes a `kit::PanelFit` per panel with every content row it draws: the list, a wrapped message (`kit::message_rows` at `kit::panel_text_width`: tab `[5]`'s loading, failed and empty messages), Devices' column names and its `SEARCHING …` / `NO RENDERERS FOUND` line with its blank row, the DEVICE panel's fields (and the `ENTER SWITCH OUTPUT HERE` line with its blank row for an output not in use), the web address under the Settings list.
  - Panels side by side decide together, so their rows stay level (the spotlight keeps its own footnote row). Devices also drops the blank under its column names; the table and the DEVICE panel decide together, so neither cuts its last rows (the active output's `STATUS` and `VOLUME`) while the other keeps its blank row. Settings drops the blanks between groups first.
  - Now Playing drops UP NEXT, then its spacers, then the elapsed row. Its empty state drops both blank rows (under the rule, and between `NO TRACK LOADED` and the hint) when the three rows don't fit.
  - `tests/test_ui_short.rs` (80×23 … 20×8) and `tests/test_ui_matrix.rs` guard all of this.

#### Header
- Left: `QUICKSTREAM` (bold INK) and ` ▪ <version>` (DIM).
- Right, 4 columns apart:
  - the output (MID): `LOCAL/<BACKEND>` or `DLNA/<RENDERER>`. The backend is the one actually playing (`AudioPlayer::backend_name`, so `LOCAL/FFPLAY` after a fallback from mpv), and the `player_backend` setting until the player has reported (`AppUI::local_backend`).
  - `WEB :<port>` (MID), only while the web server runs, with the port it bound.
  - the clock `HH:MM` (INK, `kit::ClockCache`).
- Short of room, `WEB` goes first, then the output, then the clock.

#### Tab row (`chrome::tab_titles`)
- `1 NOW PLAYING` `2 QUEUE & LIBRARY` `3 DEVICES` `4 SETTINGS` `5 PLAYLISTS` from 110 columns; `2 QUEUE` at 90–109; `1 NOW` `2 QUEUE` `3 DEVICES` … under 90, or wherever the tier's set doesn't fit.
- Narrower still (#59): only the numbers plus the active tab's short label. Under about 37 columns the numbers farthest from the active tab go too (the later of two equally far), and an active label too wide on its own is cut with `…`. The active tab is always shown and never clipped.
- The active tab is a chip (` 2 QUEUE `) in `theme::cursor()`; the others are MID, labels 4 columns apart.

#### Mini player strip (`AppUI::render_strip`, tabs `[2]`–`[5]`)
- Left: the state glyph and the title (bold INK); from 90 columns the artist (MID).
- Right: elapsed time, `kit::progress_bar` (`▕`, `╱` in INK for the elapsed share, `╱` in RULE for the rest, `▏`), the length, `VOL <n>`.
  - The bar is 2/5 of the width less 4 (42 cells at 120), shrinking to leave the title room; under 6 cells it becomes `02:41 / 05:52`.
  - What doesn't fit beside the title goes: the volume first, then the whole right side.
  - `QUEUED` (DIM) leads the right side while a debounced skip is pending.
  - While autopilot runs, `⚡ <PROFILE>` (`MixProfile::short_name`, MID) comes first, only when the title and artist still fit whole beside it.
- No track: `■ NO TRACK LOADED` (DIM) and `VOL <n>`.
- Shared with Now Playing, so the two never disagree:
  - `AppUI::player_state_glyph`: `▶` playing, `‖` paused, `■` stopped, `»` while a debounced skip is pending.
  - `AppUI::playback_times`: the position is a pending seek's target, else the player's. The length is the player's when it knows it and is playing or paused (a tag length can be an estimate), else the track's own. During a pending skip the target track shows from `00:00` with only its own length (none while that is unknown), never the old track's, which the player still holds.

#### Key row (`AppUI::render_key_row`)
- **Notation:** a keycap (` SPC `) in `theme::keycap()`, then ` LABEL` in MID; hints 4 columns apart (`kit::KeyHint { keys, label, priority }`, `kit::key_row`).
  - Keys are UPPERCASE, Shift is `⇧` (`⇧J ⇧K`, `C ⇧R ⇧D`), pairs are space-separated (`J K`, `H L`, `[ ]`, `← →`).
  - Only keys the handler in charge acts on are offered.
- **Trimming:**
  - The lowest priority goes first (the rightmost of equals); the rest keep their order. `kit::key_row` never drops the hints with the highest priority.
  - At 80–89 columns at most 5 keys (`kit::top_hints`).
  - Too narrow even for the top hints (Now Playing's `N QUEUE` and `? KEYS` need 21 columns), `kit::fit_top_hints` lets the rightmost of them give way, then the next priority takes their place: only whole keys show (a keycap never without its whole label), and none when not even one fits. `N QUEUE` stays whenever it fits (13 columns and up, with the gutters). Every modal footer and the Settings detail keys do the same (`kit::whole_key_row`, `key_row` of `fit_top_hints`).
- **Status message** (`AppUI::status_message`) at the right end:
  - ERROR when it contains `error`, `failed` or `couldn't` (any case), else MID.
  - Hidden when empty or when it repeats the screen (`Playing:…`, `Playing`, `Paused`, `Stopped`, `Welcome to Quickstream…`).
  - A long status takes the least important keys' room, then is cut with `…`.
- **Volume (Now Playing only):** `kit::volume_meter` at the end (`VOL ▮▮▮▮▮▮▮▯▯▯▯ 65`: `VOL` DIM, 11 cells filled INK / empty DIM, the number bold) from 90 columns, `VOL <n>` below. It shows whole or not at all, and shares the row with a status only when the status gets at least 12 columns (`MIN_STATUS_COLS`); otherwise the meter gives way.
- **Per tab and pane** (`AppUI::tab_key_hints`). `? KEYS` is always among the top keys, except where `?` is typed text (spotlight, startup home editor):

| Where | Keys |
|---|---|
| `[1]` | `SPC PLAY` (`SPC PAUSE` while playing), `←→ TRACK`, `[ ] SEEK <seek_step_seconds>S`, `+− VOL`, `N QUEUE`, `? KEYS`. `N QUEUE` stays at every width and Esc is never offered (#57). |
| `[2]` LIBRARY | `H L PANE`, `ENTER OPEN / ADD`, `A ADD ALL` (not in Music Homes), `/ SPOTLIGHT`, `F SEARCH`, `V TAGS/FILES`, `? KEYS` |
| `[2]` QUEUE | `H L PANE`, `ENTER PLAY`, `D REMOVE`, `C CLEAR`, `F SEARCH`, `V TAGS/FILES`, `? KEYS` |
| `[2]` spotlight | `↑↓ MATCH`, `ENTER OPEN / ADD`, `ESC CLOSE` |
| `[3]` | `J K SELECT`, `ENTER USE OUTPUT`, `R RESCAN`, `? KEYS` |
| `[4]` | `J K SELECT`, `← → CHANGE`, `ENTER CHANGE`, `? KEYS`; on Startup Home Directory `ENTER EDIT` and `C CLEAR` |
| `[4]` home editor | `ENTER SAVE`, `ESC CANCEL` |
| `[5]` PLAYLISTS | `H L PANE`, `ENTER OPEN`, `A QUEUE`, `C ⇧R ⇧D NEW / RENAME / DELETE`, `? KEYS` |
| `[5]` tracks | `H L PANE`, `ENTER PLAY`, `⇧J ⇧K MOVE TRACK`, `D REMOVE`, `A QUEUE ALL`, `? KEYS` |

- **While a modal is open** the row shows that modal's keys instead (`AppUI::modal_key_hints`, see 3.6), never `? KEYS`.

### 3.4 Panels and list rows (`src/ui/kit.rs`)
- **Panel:** `kit::panel(f, area, PanelSpec { label, right, focused, footnote, compact })` draws the header and returns the content area. No borders. Rows:
  1. label row: the label in UPPERCASE, the right note (UPPERCASE, DIM) against the right edge. The label is cut to keep the note while 8 of its columns stay; otherwise the note drops.
  2. rule: `─` then ` ▪`.
  3. footnote (optional): DIM, drawn as given (paths keep their case; span styles apply over DIM), cut with `…`.
  4. a blank row, then the content.
  - Focused: label bold INK, rule and `▪` INK. Unfocused: label MID, rule RULE, `▪` DIM.
  - `compact` (80–89 columns or short): the footnote takes the note's place on the label row and has no row of its own.
  - The content area stops 2 columns short of the right edge (`PANEL_RIGHT_PAD`, under the ` ▪`).
  - `kit::panel_spaced(.., spacer)`, `kit::panel_header_rows` and `kit::keeps_spacer(short, &[kit::PanelFit])` handle the blank row on short terminals (3.3); panels side by side pass one `PanelFit` each and decide together.
- **Layout:**
  - Side panels sit 4 columns from the main one: SIGNAL 34 columns; DEVICE and the option panel 40.
  - Two-pane tabs use `queue_tab::split_panes(area, tier, right_focus, share)`: LIBRARY takes 1/2 and PLAYLISTS 23/56 of the width the gap leaves (46 + 4 + 66 at 120). The gap is 4 from 90 columns, 2 at 80–89. Under 80 only the focused pane shows, full width, and a hidden pane pages by the shown one's rows.
- **Rows:** `kit::list_row(f, area, &[Col], RowMark)`.
  - Columns: `Col::fixed(text, width, style)`, `Col::flex` (shares what is left), `Col::auto` (its text's width). `.underline(range)` / `.underline_chars(indices)` underline characters (none of those cut by `…`).
  - Every column but the last is cut to leave a space before the next.
- **Cursor:**
  - `RowMark::Cursor` in the focused pane: the bar in `theme::cursor()`, one column wider than the row on each side (so its leading cell may sit in the inner left gutter column).
  - `RowMark::Marker` in the unfocused pane: `▸` in the row's first column. Callers keep that column as a 2-column marker column with no text of its own, so the marker never hides `▶` / `▪` (both tab `[2]` panels, PLAYLISTS).
  - Tab `[5]`'s unfocused tracks pane marks nothing.
- **Scrolling:** `kit::visible_window(&mut ListState, len, height)` returns the rows to draw and scrolls like a ratatui `List` (the offset moves only as far as needed and is written back). The render records each list's height; PageUp/PageDown move by it.
- **Column widths** (artist, format, length) come from the whole list, cached per revision, so they don't change while scrolling.
- **Other pieces:**
  - `kit::big_digits`: 3-row heavy box-drawing numerals; digits 3 columns, the colon 1, glyphs 1 column apart.
  - `kit::spinner(elapsed)`: `⣾` frames every 100 ms, turned by the clock (no thread or counter).
  - `progress_bar`, `volume_meter`, `duration`, and the modal pieces in 3.6.

### 3.5 Tabs

#### `[1]` Now Playing (`src/ui/now_playing.rs`, mockups 2b at 120×40, 3f at 80×24)
- **Data:** monochrome (no colour pills), drawn with `kit::panel` / `kit::list_row`; only data in memory (the queue's tags and the features worker's result).
  - Placeholder metadata (`metadata::fallback_metadata`: the scoped scan's and autopilot's untagged candidates; tags not read yet or unreadable) leaves length, bitrate, sample rate and channels at 0 (unknown), so the tab shows only the format and size and takes the length from the player.
  - `metadata::extract_metadata` (through `metadata::audio_properties`) likewise leaves a channel count, bitrate or sample rate lofty couldn't read at 0: the search TRACK panel leaves the field out, `/api/status` reports `channels: 0`. Only a missing length is estimated, from the bitrate and file size.
  - Times and the state glyph come from `AppUI::playback_times` / `AppUI::player_state_glyph`, as on the strip (3.3).
- **NOW PLAYING panel:**
  - Footnote `TRACK 04/12  ·  QUEUE LOOP  ·  ▶ PLAYING`. The state follows the player (`▶ PLAYING`, `‖ PAUSED`, `■ STOPPED`, `» QUEUED` while a debounced skip is pending); the mode comes from `queue_mode` (`queue_tab::queue_mode_note`: `LOOP`, `STOP AT END`, `REPEAT ONE`).
  - While autopilot runs, the label row's note names the mix profile (`AUTOPILOT · BPM FLOW`, from `MixProfile::short_name`).
  - Compact (under 90 columns, or under 24 rows): the footnote moves to the label row as `04/12  ·  LOOP  ·  ▶ PLAYING  ·  ⚡ BPM FLOW`, giving up the mode, then the position, then the profile when it doesn't fit. The state always stays, as its glyph alone (`▶`, `‖`, `■`, `»`) when even `▶ PLAYING` doesn't fit.
  - The elapsed time (or a pending seek's target) in `kit::big_digits` (bold INK).
  - From 110 columns: `/ 05:52` (regular DIM) after the numerals' bottom row; under them the title (bold) and `ARTIST  /  ALBUM  /  YEAR` (MID; the year from the features); the bar row ends with the percentage.
  - Under 110 columns: the title, artist and `ALBUM  /  YEAR` sit beside the numerals (3f), and the bar row ends with the total. Too narrow for the numerals plus 10 columns of title, the time is written in plain bold digits with the title and artist under it, never numerals cut at the edge. This (and where the title sits) is decided from the widest time the track shows, its length when known (else the elapsed time), so the layout holds for the whole track, across the hour mark (`59:59` → `1:00:00`) and while seeking.
  - The progress bar spans the whole width to the panels' text edge. Under it `ELAPSED 02:41   REMAIN 03:11   <path>` (DIM; the path in its own case with `~`, its start cut with `…`).
  - Unknown length: no total, percentage or REMAIN.
- **SIGNAL panel** (from 110 columns, 34 columns at the right; footnote `SOURCE <output>`):
  - Rows `FORMAT`, `RATE` (`96 KHZ`), `BITRATE`, `CHANNELS`, `GENRE` (tags, else features), `BPM`, `KEY` (Camelot, else the tagged key), `ENERGY`, `SIZE`; a row appears only when it has data.
  - SIGNAL gets only the rows left once the hero, bar, elapsed row and UP NEXT are placed, so UP NEXT never depends on whether features have arrived. Rows that don't fit are cut least useful first (`SIZE`, `CHANNELS`, `ENERGY`, `GENRE`, `BITRATE`, `KEY`, `BPM`, `RATE`; `FORMAT` last); the rest keep their order.
  - Under 110 columns it is replaced by a DIM tech line under the bar (`FLAC  96 KHZ  2304 KBPS  STEREO  112 BPM  8A`, last fields dropped to fit).
- **UP NEXT panel:** what actually plays next, from `PlaybackQueue::upcoming(queue_mode, wrap)`. `wrap` is set while the UPnP continuous stream plays (`AppUI::continuous_stream_active`): the stream plays on to the end of the queue whatever the mode, then reshuffles and starts over.
  - Label note `<n> REMAINING  ·  <total>`: the tracks listed; the total only when every listed length is known, and not when compact.
  - Rows: index (DIM), title, artist (Tags mode, from 80 columns), format column `FLAC 96` / `MP3 320` (from 110 columns: the bitrate for lossy formats, the sample rate otherwise; an M4A/MP4 above 500 kbps counts as lossless, likely ALAC, since the codec isn't in the metadata), length. Filenames mode shows the file names in their own case.
  - Stop mode: the tracks to the end of the queue, then `END OF QUEUE  ·  PLAYBACK STOPS` when there is a row for it.
  - Loop mode: every other track, running past the end to the first, with a DIM `END OF QUEUE  ·  LOOPS TO 01` row where it wraps.
  - Repeat-one: nothing listed, `REPEAT ONE  ·  THIS TRACK REPEATS`.
  - Continuous stream: the tracks to the end of the queue, then `END OF QUEUE  ·  RESHUFFLES AND STARTS OVER`.
  - Nothing listed: `END OF QUEUE  ·  PLAYBACK STOPS AFTER THIS TRACK` (stop), `END OF QUEUE  ·  REPEATS THIS TRACK` (loop over a single track), `END OF QUEUE  ·  RESHUFFLES AND STARTS OVER` (continuous stream).
  - The count, total and column widths are cached per `PlaybackQueue::revision`, queue mode and continuous-stream state (`AppUI::up_next_summary_builds` for tests).
- **Rows when short:** under 24 rows, or when the hero leaves no room for UP NEXT with up to 3 rows, UP NEXT is dropped first; then, under 24 rows, the spacer rows (the blank under the rule, the one before the bar and, from 110 columns, the one between the numerals and the title, SIGNAL's blank going with them); the elapsed row next.
- **Empty state:** `NO TRACK LOADED` and `Press Space to play active queue, or navigate to [2] Queue & Library to browse your collection.`; no SIGNAL or UP NEXT.
- **Features** come from `features::TrackFeaturesWorker`:
  - `AppUI::poll_track_features` is called once per tick by the event loop, before `update_web_state`; `AppUI::set_track_feature_source` is the test seam.
  - The worker reads the analysis database when the track changes, again every 5 s (`RETRY_INTERVAL`) while there are none yet, then every 30 s (`REFRESH_INTERVAL`) while the track stays current, so a re-analysis shows without a track change. Only a changed result is sent (`TrackFeaturesWorker::spawn_with_intervals` is the test seam), and results for a track no longer current are dropped.
  - The render reads them with `TrackFeaturesWorker::features_for(current path)`, so a frame drawn after a track change, before the next poll, never shows the previous track's features.
  - Nothing on the UI thread reads the database. `update_web_state` doesn't poll the worker again: it takes `bpm`/`key` from what that tick's poll found for the current track (`features_for`).
- **Keys:** `r` starts Seed Radio from the now-playing track (its only tab key); the rest are global (3.7).

#### `[2]` Queue & Library (`src/ui/queue_tab.rs`, mockups 2c, 3a, 3g)
- Two panels, LIBRARY and QUEUE (1/2 of the width each, 3.4). `h` / `l` move the focus; only the visible rows are drawn; rows are mapped from `browser_entries` and the queue at draw time (the stored labels stay as they are). Every row of both panels starts with the 2-column marker column.
- **LIBRARY panel** (the file browser: the directory tree and the bookmarked Music Homes in `~/.config/quickstream/bookmarks.json`):
  - Rows: `★ MUSIC HOMES`, `.. PARENT` (`.. FILE SYSTEM` in Music Homes), `DIR  NAME/` (bold, no child counts), file rows `01  TITLE  ARTIST  FORMAT  04:02`. Tags mode: title and artist from `MetadataCache` (the file name until its tags are read). Filenames mode: the file name in its original case. Artist only in Tags mode from 90 columns; format from 90 columns; a length only when known.
  - A queued file shows `IN QUEUE` in the format column; the playing one shows `▪` in place of its number.
  - Label note `TAGS` / `FILES` (`v` toggles).
  - Footnote: the folder (`~` for home, original case, start cut with `…`; `MUSIC HOMES` in Music Homes), `<cursor> / <entries>` and, once every file's length is in memory, the folder's total length. The total is invalidated by `MetadataCache::folder_revision` of the browsed folder, so tags read for other folders never recompute it; it is never read from disk.
  - While a folder is read: a `··· READING DIRECTORY · <n> ITEMS` row.
  - **Spotlight** (`/`, mockup 3a): label note `SPOTLIGHT`; the footnote row holds the ` / ` keycap, the query as typed with a block cursor and `<k> OF <n> SUBFOLDERS` / `NO MATCHING SUBFOLDERS` (ERROR) / `TYPE TO JUMP (<n> SUBFOLDERS)`. Only real subfolders (in Music Homes, the homes) are matched and counted, never the `..` or `★ MUSIC HOMES` rows (`is_subfolder_entry`). Matched characters are underlined and, while something matches, the other rows are DIM.
- **QUEUE panel:** rows `▶ 01  TITLE  ARTIST  04:02` (`▶` and bold on the current track; artist in Tags mode from 90 columns; Filenames mode shows file names). Label note the queue mode (`LOOP`, `STOP AT END`, `REPEAT ONE`); footnote `<n> TRACKS  ·  <total>` (the total only when every length is known).
- **Caches:** the queued-path set behind `IN QUEUE` and the queue's totals and columns are rebuilt per `PlaybackQueue::revision()`; the library's total and columns with its folder summary.
- **Width rules:** from 90 columns the panels are 4 apart. At 80–89 the artist and format columns drop, the panels are 2 apart, and each footnote moves to its label row (`7/14`, the queue's track count), except the spotlight query, which keeps its row. Under 80 only the focused pane shows.
- **Keys:**
  - `j` / `k` / arrows wrap. PageUp / PageDown (also `Ctrl+U` / `Ctrl+B`, `⇧K`, `Shift+↑` and `Ctrl+D` / `Ctrl+F`, `⇧J`, `Shift+↓`) move by the rows the list shows, stop at the first / last row and wrap only when pressed there. `g` / `Home`, `G` / `End` jump.
  - Library: `Enter` opens a folder or adds a file to the queue; `Backspace` goes to the parent (from Music Homes back to the file system); `a` adds the folder's files (not in Music Homes); `b` bookmarks or un-bookmarks the browsed folder; `d` / `x` / `Del` removes the selected Music Home; `r` starts Seed Radio from a track row, or rescans the folder on any other row; `⇧H` sets (or clears) the startup home; `~` jumps to the startup home.
  - Queue: `Enter` plays the selected track; `d` / `x` / `Del` removes it; `c` clears the queue; `r` starts Seed Radio from it.
  - Spotlight: typing jumps to the first match; `↓` / `Tab` next match, `↑` / `⇧Tab` previous; `Backspace` edits; `Enter` closes it and opens or adds the selected row; `Esc` closes it.

#### `[3]` Devices (`src/ui/devices_tab.rs`, mockup 2d)
- Only what `DeviceManager` holds in memory is drawn (device list, scanning flag, active device); nothing is fetched for the render.
- **OUTPUT DEVICES panel:**
  - Footnote `<n> FOUND  ·  SSDP 239.255.255.250:1900` (`n` = discovered renderers; Local Audio isn't counted). Label note `SCANNING ⣾` while discovery runs (`kit::spinner` from `AppUI::ui_epoch`).
  - DIM column names `NAME  TYPE  ADDRESS`, then `LOCAL AUDIO` (type the local backend actually playing, e.g. `MPV`; address `THIS MACHINE`) and one row per renderer in discovery order (`DLNA`, `host:port`; an IPv6 host in brackets, `[fe80::1]:1400`).
  - Columns at 120: marker 4, NAME 30, TYPE 8 (2 less than 2d, so the address has room), ADDRESS 22 (any IPv4 `host:port` shows whole, up to `255.255.255.255:65535`), `ACTIVE` 6; a row without `ACTIVE` gives its 6 columns to the address. An address still too wide (a long IPv6 host) is cut in the middle of the host, so the brackets and port always show (`[fe80::1c2b…:5e6f:7081]:1400`).
  - The active output reads `●`, its name bold and `ACTIVE` at the end; the others `○` (MID).
  - The cursor bar is on `devices_cursor`; only the visible rows are drawn (`kit::visible_window` on `devices_state`). Rows are double-spaced while every row fits that way, else single.
  - Under the rows, after a blank row and only when every output still has a row: `⣾ SEARCHING FOR DLNA / UPNP MEDIARENDERERS` (DIM) while scanning, `NO RENDERERS FOUND` once a scan found none.
  - Names keep at least 12 columns: narrower, `ACTIVE`, then TYPE, then ADDRESS drop.
- **DEVICE panel** (from 110 columns, 40 columns at the right):
  - Footnote `DLNA MEDIARENDERER` or `LOCAL PLAYBACK`.
  - Rows `NAME`, `MAKER` / `MODEL` (only when discovery found them; a model that only repeats the name is left out, as in the footnote and `DLNADevice::display_label`), `HOST`, `PORT`, `UDN` (original case, cut in the middle), `CONTROL` (the AVTransport control path); for Local Audio `ENGINE`.
  - `STATUS` (`ACTIVE` bold, else `NOT IN USE`); `VOLUME` only for the active output (the player's volume, as on the strip; an inactive renderer's volume is never shown); for an output not in use ` ENTER  SWITCH OUTPUT HERE`.
- **Width rules:** under 110 columns no DEVICE panel, and the selected renderer's maker and model join the footnote (`3 FOUND  ·  SSDP …  ·  PRIMARE I15`). At 80–89 columns or under 24 rows the footnote goes to the label row as `<n> FOUND` (`  ·  SCANNING ⣾` while scanning).
- **Keys:** `j` / `k` / arrows wrap; PageUp / PageDown (and `⇧K` / `⇧J`, `Ctrl+U` / `Ctrl+D`, `Shift+↑` / `Shift+↓`) by 10, stopping at the ends, then wrapping; `g` / `G` (`Home` / `End`); `Enter` uses the selected output; `r` rescans.

#### `[4]` Settings (`src/ui/settings_tab.rs`, mockup 2e)
- Only what is in memory is drawn: the settings, their `settings::SettingsContext` (`AppUI::settings_context`: the Music Homes in `AppUI::bookmarks`; the music folder once its startup worker has found it; the home folder the library shows as `~`, from `queue_tab`, one source for the whole tab and the startup home editor's `~`; the port of the running web server) and the cached LAN address.
- **SETTINGS panel:**
  - Footnote: the config file (`SettingsManager::path`, `~` for home, original case, DIM, start cut with `…`).
  - Each `SETTING_DEFINITIONS` entry has a `SettingGroup`. The list shows the DIM headings `PLAYBACK`, `LIBRARY`, `WEB`, each followed by its settings in `settings::SETTING_DISPLAY_ORDER` (engine, seek, volume step, queue mode, gapless, transcoding | file list display, startup home, mix algorithm, tag enrichment | web server, remote access), with a blank row between groups.
  - Code names a setting by `settings::setting::<KEY>` (e.g. `setting::HOME_DIR`, `setting::WEB` for both web rows), worked out at compile time from its `SETTING_DEFINITIONS` key; never by its number.
  - A row is the label (UPPERCASE, MID, 32 columns) and `short_value(index)` (INK): the compact UPPERCASE value (`MPV`, `10 S`, `CONTINUOUS (LOOP)`, `ON  :8090`, paths in their own case with `~`), which also labels the choices. `get_display_value` keeps the long wording (status messages).
  - The selected row is the cursor bar with the value as `◂ VALUE ▸`. A value too wide is cut with `…`: a startup home path from its start, so its last folder shows (`◂ …/Herbert von Karajan ▸`, selected or not, as in the editor and footnote); every other value (`LAUNCH DIRECTORY` too) from its end.
  - When the list doesn't fit, the blank rows go first, then it scrolls to keep the cursor on screen (the first row of a group brings its heading along, the LIBRARY group's last setting the TRACK ANALYSIS row under it; once it fits again it shows whole).
  - **Track analysis** (issue #30): the LIBRARY group ends with a read-only `TRACK ANALYSIS` row, label and value in MID (values that change are INK), which the cursor never lands on (it isn't a setting, so `display_position` and the `<pos> / <n>` count leave it out). Its value is what the background `AnalysisWorker` has done this session, read from its atomics on every frame (`AnalysisWorker::progress`): `37 PENDING  ·  412 ANALYSED  ·  1 FAILED  ·  3 SKIPPED` (skipped: already analysed, file unchanged; pending counts the track being analysed), counts with thousands grouped (`kit::grouped`), those at zero left out, `IDLE` when all are; cut from its end, so pending stays longest. It is here rather than on the strip or key row: the strip belongs to playback, the status message is overwritten by every action, and Settings is where the library's state lives, at every width and on short terminals (it scrolls like the other rows).
- **Option panel** (from 110 columns, 40 columns at the right: 72 + 4 + 40 at 120):
  - Label the selected setting; footnote its position `<pos> / <n>` (`settings::display_position`, e.g. `04 / 12` on Queue Playback Mode).
  - Every choice from `SettingsManager::options(index, &context)`, `●` (bold INK) on the one it holds and `○` (MID) on the rest, windowed around the current choice when there are too many.
  - A blank row, then the setting's `description` (MID; short, from what the setting does; Audio Transcoding and Online Tag Enrichment say `Not used yet`, since no code reads them).
  - On the web server / remote access rows, the web address in its own case (DIM, `SettingsManager::web_url`). It is always whole: on a short panel the choices around the current one give it their rows and it comes before the description, which is cut instead; a panel too short even for the address leaves it to the rows under the list.
  - The description and address are a ratatui `Paragraph` with `Wrap` (at spaces; a URL broken at the edge); `Paragraph::line_count` (ratatui feature `unstable-rendered-line-info`) places the keys under them: the keys that change the setting (`tab_key_hints` without `J K` / `?`).
- **Width rules:**
  - Under 110 columns there is no option panel, and the position joins the footnote (`<config path>  ·  04 / 12`); on a web row the web address takes the config path's place there.
  - At 80–89 columns, or under 24 rows, the footnote shrinks to the position on the label row. Then, without an option panel (or when it can't hold it whole), the web address (while a web row is selected) takes the list's last row in DIM, or its last two rows when wider than the list, leaving the list at least the cursor's row. Its start is cut with `…` only when the rows it gets are too few; the list scrolls above it.
  - So the address, including the pairing link, shows at every width and height, and is never cut at its end.
- **Keys:**
  - `j` / `k` / `↑` / `↓` move `settings_cursor` (a setting index, starting on the first listed) along the display order with wraparound (`settings::step_in_display_order`). `g` / `G` / `Home` / `End` / PageUp / PageDown / `⇧K` / `⇧J` jump to its ends.
  - `←` / `→` / `h` / `l` / `Space` cycle the value: `cycle_setting(index, forward, &context)` moves along `options`, so the two never disagree; a hand-edited value outside the list cycles on from the default. `Enter` cycles forward.
  - On Startup Home Directory, `Enter` (or `e`, `/`) opens the inline editor and `c` / `Del` clear it. The startup home cycles through the launch directory, the music folder, home, the bookmarks the UI holds in memory (bookmarks.json is never read on a key press or a render) and a custom path. The music folder comes from `settings::lookup_music_dir` (on Linux it reads `user-dirs.dirs`), which `AppUI::new` runs on a worker; until it answers, the music folder is left out of the choices and of cycling, as if there were none (`AppUI::set_music_dir_lookup` is the test seam).
- **Startup home editor:** inline on its row in the cursor bar: the typed text in its own case with a block cursor (`█`), its end shown with `…` when long. Every character is text (including `q` and `?`). `Backspace` deletes, `Enter` saves (empty, `default` or `none` clears; `~` expands), `Esc` cancels. The option panel shows what the field takes instead of the choices.
- **Web port:** the web server's value (`ON  :<port>`), its choice in the option panel, the web address and the long value name the port the server actually bound (as `WEB :<port>` in the header), which differs from `web_server_port` when another program held that one (the server moves on to the next candidate). It is read from the running server (`AppUI::web_port`, `web_server.port`) into `SettingsContext::web_port` and `SettingsManager::web_url(web_port)`; the settings model keeps no copy. While no server runs the value reads `ON` and there is no address.
- **Web address:** `SettingsManager::web_url` is `http://127.0.0.1:<port>`, or with network access the pairing link (LAN address and session token).
  - The LAN address is looked up on a short-lived thread (which also loads the web token) and cached, re-looked up every 30 s while shown (`SettingsManager::set_lan_ip_resolver` is the test seam). Until it is known there is no link and the long value reads `Enabled`, so drawing never waits on it.
  - The lookup starts early so the link is usually ready: at startup when network access is on, and whenever a web row is shown with network access on, even while the server is off (`SettingsManager::prefetch_lan_address`).
  - Turning the server on puts `Now Playing Web Server: Enabled (<address>)` in the status line (`AppUI::show_setting_status`). If the pairing link isn't known yet, the status reads `Enabled` and `AppUI::poll_settings_status` (once per tick) adds the link when the address arrives, only while the status still reads what it set and the server still runs.
- **Remote access off:** an active remote DLNA renderer binds the server to the LAN whatever `web_server_lan` says, so `web_access::authorize` takes `remote_access` (`Settings::web_remote_access`: `web_server_lan` and `web_server_enabled` both on, since a renderer also keeps the server running while the web server setting is off): while it is false a network peer gets only the tokenless read-only `/stream/*`; every other route is 403 `remote access is off` (`web_access::REMOTE_ACCESS_OFF`), even with a valid token (a phone paired earlier keeps its cookie): JSON on `/api/*` (the dashboard's status poll then shows `REMOTE ACCESS OFF`, not `OFFLINE`), elsewhere an HTML page naming the two settings. Loopback is unaffected. The web thread reads it per request from `NowPlayingState::remote_access` (memory only), which the UI sets in `AppUI::new`, before each server start, in `update_web_state` (also while no server runs) and right after a web setting changes.

#### `[5]` Playlists (`src/ui/playlists.rs`, mockup 2f, state in `AppUI::playlists: PlaylistsUi`)
- Two panels like tab `[2]`: PLAYLISTS (23/56 of the width) and the tracks of the selected playlist; only the visible rows are drawn.
- **Loading:** a startup worker loads the playlists. Until it finishes, the panel shows `Loading playlists…` and edits are refused with `Playlists are still loading`. If the folder exists but can't be listed, it shows `Couldn't load playlists. Restart Quickstream to try again.` (ERROR) and edits stay refused for the session (`Couldn't load playlists`), so no create can overwrite a file the store never read.
- **PLAYLISTS panel:** label note the playlist count; one row per playlist (`NAME` and its track count, DIM, against the right edge), sorted case-insensitively. While the tracks pane has focus, the open playlist keeps a bold `▸`. Empty: `No playlists yet. Press c to create one, or L on any track.`
- **Tracks panel:**
  - Label the playlist's name (`TRACKS` when none is selected), note `<n> TRACKS  ·  <total>`: the total of the known lengths (unknown ones left out; no total when none is known).
  - Footnote the playlist's file (`~` for home, original case, DIM). While a rename of it is unsettled, the old name the file still has (`PlaylistStore::file_path_of`).
  - Rows `01  TITLE  ARTIST  04:02` (a length only when known). Missing files read `01  TITLE  MISSING FILE  --:--`, with `MISSING FILE` in ERROR and the title DIM. Only the focused tracks pane marks its cursor.
  - The note's total and the column layout (from the whole playlist, so it doesn't change while scrolling) are cached per `PlaylistStore::revision()`, cursor and missing-file checks; the PLAYLISTS count column likewise comes from every playlist.
  - Empty: `Empty. Press L on a track in tab 2 to add it.`
- **Missing-file checks:** existence is checked on a worker and cached per path for the session. Rows not yet checked render normally.
- **Width rules:** as tab `[2]`: from 90 columns the panels are 4 apart; at 80–89 the artist column and the file footnote drop and the panels are 2 apart (`MISSING FILE` still shows); under 80 only the focused pane shows (a hidden pane pages by the shown one's rows).
- **Keys:**
  - `h` / `l` toggle the focus (`l` and `Enter` open the selected playlist's tracks). `j` / `k` / `↑` / `↓` wrap. In either pane `g` / `G` (`Home` / `End`) jump to the first / last row and PageUp / PageDown move by the rows the pane shows, stopping at the first / last row without wrapping.
  - PLAYLISTS pane: `c` create, `R` rename (prompt prefilled with the current name), `D` delete (after the confirmation), `a` append the whole playlist to the queue.
  - Tracks pane: `Enter` play from the selected track, `a` append the whole playlist, `K` / `J` move the selected track up / down (no wraparound; on tab `[2]` these keys page instead), `d` / `x` / `Del` remove it.
  - Edits save automatically through the background writer (`PersistHandle`). There is no save key.
- **`Enter` (play from the selected track):** replaces the queue with the playlist.
  - It stops any autopilot scan and continuous stream first (`AppUI::reset_playback_session`) and skips missing files.
  - The worker checks the selected entry first, then the ones after it, then the ones before it. The first that exists (the track playback starts on) plays as soon as it is found; the rest of the playlist is put around it when their checks finish (status `Queued <k> tracks from <name>, <m> missing`).
  - Tracks queued meanwhile stay after the playlist; if the queue is replaced or cleared meanwhile, the rest is dropped.
  - The started track is matched by its entry index (a file listed twice is queued once per entry) and, in the queue, as the current track.
  - If it ends, or Next/Previous is pressed, before the rest is checked, the skip waits for the rest (`Waiting for the rest of <name>…`, `Next track plays once <name> is checked…`, `AppUI::hold_skip_for_playlist_fill`; `AppUI::handle_track_finished` is the event loop's track-end hook) instead of stopping or replaying.
  - If every file is missing, the queue is left untouched and the status reads `None of <name>'s <n> tracks found. Is /Volumes/Music mounted?`, naming the top two folders above the first entry's file (no question when the file is fewer than two folders deep, e.g. `/Volumes/ghost.mp3`).

#### Web dashboard playlists (issue #50)
The dashboard's `Playlists` tab uses the same store and code as tab `[5]`. On the web side the snapshot types, `WebCommand`, `PlaylistEdit` and `WebReply` are in `src/web/state.rs`, the routes and `/api/action` handlers in `src/web/api.rs`, and the page itself in `src/web/assets/index.html`.
- **Read:** `GET /api/playlists` serves the `WebPlaylists` snapshot the UI thread publishes in `NowPlayingState::playlists` (an `Arc`, `#[serde(skip)]` from `/api/status`).
  - It is republished whenever `PlaylistStore::revision()`, the missing-file cache, the load state or `last_playlist` changes (`AppUI::publish_web_playlists`, called from `update_web_state`); a missing-file-cache change alone republishes at most every 500 ms (`WEB_EXISTS_REPUBLISH_INTERVAL`). The playlists folder is never read per request.
  - `/api/status` carries `playlists_version` so the page knows when to fetch again.
  - The snapshot has `state` (`loading`/`failed`/`ready`), the TUI's `message` when not ready, `revision`, `last_playlist` and each playlist's entries (title, artist, duration, path, `missing`: `true`/`false`/`null` until checked). Responses clear the path and set `outside` for entries outside the allowed web folders, as `/api/status` does.
  - Serving it sends `WebCommand::CheckPlaylists`, which checks every playlist's unchecked files on one worker (paths checked or in flight are skipped, so repeated fetches start no new checks).
- **Play / append:** `/api/action` `playlist_play` (`name`, optional `index`, which requires `revision`) and `playlist_append` (`name`).
  - The web thread checks every entry of the snapshot's playlist: one whose file resolves outside the allowed folders, or a missing one whose path lies outside them, refuses the whole request with 403 and `WebCommand::PathRejected`, like `add_file`. Entries are checked with `web_access::AllowedRoots`, which canonicalizes the allowed folders once per request (and once per play on the worker), with the same answers as `resolve_allowed_path`.
  - Otherwise it sends `WebCommand::QueuePlaylist` with the checked entries and a `WebReply`, waits up to 5 s, and returns the UI's answer: 200 with `Checking <n> tracks in <name>…`, or 409 with its refusal (`Still checking <name>…`).
  - The UI queues them with tab `[5]`'s worker code (`queue_playlist_entries`: same `plan_queue`, reset of autopilot scan and continuous stream on replace, same status messages, which name the playlist instead of saying `(web)`). For web requests the worker also requires each checked file to resolve inside the allowed folders.
  - Refusals (JSON `{"ok":false,"error":…}`): not loaded (409, `Playlists are still loading` / `Couldn't load playlists`), unknown name (404), empty playlist (409), bad index (400), `index` without `revision` (400, `Missing revision: reload the playlists and try again`), stale `revision` (409, `Playlists changed; try again`).
- **Edit:** `playlist_create` (`name`), `playlist_rename` (`name`, `new_name`), `playlist_delete`, `playlist_add` (`name`, `path`), `playlist_remove` (`name`, `index`, `revision`), `playlist_move` (`name`, `index`, `direction` `up`/`down`, `revision`).
  - `playlist_add`'s path is checked like `add_file` on the canonical path (403 + `PathRejected` outside the allowed folders, 400 if not an audio file) and stored as `playlist::normalize_path` of the requested path, the form a terminal add stores; `Already in <name>` if the playlist holds either form.
  - `revision` is required for remove and move (400 without it).
  - The web thread sends `WebCommand::EditPlaylist(PlaylistEdit, WebReply)` and waits up to 5 s for the UI's answer. `AppUI::apply_web_playlist_edit` applies it through `PlaylistStore` only (same `validate_name` rules, duplicate and reserved-name checks and messages as the name prompt), shows it in the status line, republishes the snapshot and answers: 200 `{"ok":true,"message":…}`, 409 with the store's message, 503 if the UI didn't answer.
  - Tab `[5]` keeps its playlist selected (following a web rename).
- All of these share the `web_access` checks (pairing token, Host, Origin, JSON Content-Type). The dashboard builds every row from text and `dataset` only (see `tests/support/dashboard_dom_harness.js`).

### 3.6 Modals
- **One at a time:** help (`?`), search (`f`), mix picker (`M`), and the playlist modals (name prompt, delete confirmation, add picker). `AppUI::modal_open` says whether one is open.
- **Dimming:** `AppUI::render` first dims the whole screen under the modal (`kit::dim_buffer` with `theme::dimmed()`: fg RULE, bg GROUND, no modifiers; DIM in Basic; faint in Mono), except the key row, which it draws after dimming. The modal's keys and the status message (e.g. search's `Tab` confirmation `Added to queue: …`, or an ERROR status) stay readable; a modal tall enough to reach the key row still covers it.
- **Frame:** `kit::modal_frame(f, area, ModalSpec { title, subtitle, note, error, compact })` clears the area with `kit::clear` and returns the content area.
  - Thin border in DIM (ERROR when `error`); reverse-video title chip (` KEYS `, `kit::modal_title`; GROUND on ERROR when `error`); the subtitle 2 columns after the chip, in MID and its own case (start cut with `…`, dropped before the note); the note against the right edge in DIM.
  - Full: a padding row, the title row, a blank row, content 3 columns in, a padding row at the bottom.
  - Compact: chip and note in the top border, content 1 column in, no padding rows.
  - Section rules inside a modal: `kit::modal_rule` (`────── ▪` in INK).
- **Placement:** `kit::centered(area, w, h)` centres and clamps to the screen (never larger, never panics at tiny sizes). Every modal is placed with `kit::modal_area(screen, w, h)`, which also narrows it to leave the screen's 2-column gutters (`kit::MODAL_MARGIN`), so no modal draws outside them, except the help on 80–83 column screens.
- **Footer = key row:** each modal's footer keys and the key row come from one list (`help::footer_hints`, `search_view::footer_hints`, `mix_picker::footer_hints`, `playlists::name_prompt_hints` / `confirm_delete_hints` / `add_picker_hints`), so every key a footer shows is in the key row in the same words; the key row adds only keys the footer leaves out:

| Modal | Key row |
|---|---|
| Help | `? ESC CLOSE` |
| Search | `↑↓ SELECT`, `ENTER ADD & CLOSE`, `TAB ADD TO QUEUE`, `⇧TAB ADD ALL (<n>)`, `ESC CANCEL` |
| Mix picker | `J K SELECT`, `ENTER START MIX`, `1-5 PROFILE` (`1-<n>`, n from `MixProfile::picker_profiles`), `ESC CANCEL` |
| Name prompt | `ENTER SAVE`, `ESC CANCEL` |
| Delete confirmation | `Y DELETE`, `ANY KEY CANCEL` (equal priority, so the key row never drops either) |
| Add picker | `ENTER ADD`, `J K SELECT`, `ESC CANCEL` |

#### Help (`?`, mockup 2g, `src/ui/help.rs`)
- Chip ` KEYS `, note `QUICKSTREAM <version>`. Two columns of sections: PLAYBACK & AUDIO, NAVIGATION & TABS, PLAYLISTS (TAB 5) | LIBRARY & QUEUE, SEARCH & DISCOVERY.
- Each key a keycap in a 10-column key field, then its description in MID (at most 27 characters). Keys in Shift notation (`⇧J ⇧K`, `C ⇧R ⇧D`, `G ⇧G`, `⇧P`, `⇧M`, `⇧L`, `⇧H`), `PgUp/PgDn` kept; only keys the handlers act on. The seek keys read `Seek -<n>s / +<n>s` from the settings' `seek_step_seconds` (`Seek -10s / +10s` by default). Footer `? ESC CLOSE`.
- Full layout (96×32, section rules `────── ▪`) when the screen has at least 32 rows and 88 columns; otherwise compact (22 rows, fits 80×24: title in the border, no rules or blank rows).
- Inside the gutters, except at 80–83 columns, where the compact layout takes 80 columns (gutters included), because that is what keeps every description whole at 80×24. Under 80 columns it keeps the gutters and the longest descriptions are cut with `…`, every key still listed.
- Shorter than the layout (under 22 rows), each column shows the keys that fit from the top. A section's label shows only with its first key under it, so a column never ends on a label.
- Closed by `Esc`, `q` or `?`.

#### Fuzzy search (`f`, mockup 2h, `src/ui/search_view.rs`)
- Streaming fuzzy finder over the browsed folder (the highlighted home in Music Homes): cached tracks first, then the scan's.
- Size: up to 100×32, 2 columns / 2 rows inside the screen, never under 3 rows (the compact frame and the query row; shrunk to the screen only when it has fewer). Full frame from 28 rows, else compact (title in the border, no rule under the query). When the panel headers would leave fewer than 3 list rows (e.g. 40×12), the matches go right under the query row, with no blank row, no `RANKED MATCHES` header and no TRACK panel.
- Chip ` SEARCH `, subtitle the folder searched (`~` for home), note `SCANNING ⣾` (MID, `kit::spinner` from the scan's start) while the scan streams.
- Query row: `QUERY` (DIM), the query as typed (bold, start cut with `…`) and a block cursor; at the right `<n> / <total> MATCHED` (count bold INK, rest DIM, thousands grouped `4,812`; `n` is the ranked list, capped).
- Then a rule (full only) and `RANKED MATCHES`: `TITLE  ARTIST  05:52` rows (artist and length MID). The length column is as wide as the longest length in the whole list (`SearchModalState::long_lengths`, worked out when the list is ranked, never per frame); no length when unknown. Only the visible rows (`kit::visible_window` on the modal's `ListState`), the cursor bar on the selection.
- The characters the query matched are underlined (`SearchModalState::row_marks`): each word only in the field the ranking matched it in, the title or the artist, with the ranking's boosts; a word ranked on the album, file name or a folder marks nothing. Worked out the first time a row is drawn and kept until the list changes, so a redraw does no match work (`view_work` for tests). No matches: `No tracks match your query.` (DIM).
- From 90 columns a `TRACK` panel beside it (52 : 36 of the width) lists only the selected track's fields with data: `TITLE` (bold), `ARTIST`, `ALBUM`, `DURATION`, `FORMAT`, `RATE` (`96 KHZ`), `BITRATE`, `CHANNELS`, `SIZE`, then `PATH` and the path in its own case with `~`, wrapped after `/`. Short of rows, `SIZE`, `CHANNELS`, `BITRATE`, `RATE` go first, then the start of the path.
- Footer keycaps `ENTER ADD & CLOSE`, `TAB ADD TO QUEUE`, `⇧TAB ADD ALL (<n>)`, `ESC CANCEL`; when narrow `⇧TAB`, `TAB`, then `ENTER` go, and `ESC CANCEL` always shows whole.
- Keys: typing (letters, `q` included) edits the query, `Backspace` deletes; `↑` / `↓` (`Ctrl+P` / `Ctrl+N`) wrap; PageUp / PageDown (`Ctrl+U` / `Ctrl+B`, `Ctrl+D` / `Ctrl+F`) by 15; `Home` / `End`; `Enter` adds the selection and closes; `Tab` adds it and stays open; `⇧Tab` (or `Ctrl+A`) adds all matches and closes; `Esc` closes.

#### Mix profile picker (`M`, mockup 2i, `src/ui/mix_picker.rs`)
- 84×25. Compact (17 rows) under 25 rows: no padding or blank rows between profiles. Under 17 rows also no subtitle, rule or blank rows, and the profiles scroll to keep the cursor's in view (`kit::visible_window` over a `ListState` reset on open).
- Chip ` AUTOPILOT MIX `; `Sequences the queue with DJ spacing and transitions.` (MID) over a `────── ▪` rule.
- One row per `MixProfile::picker_profiles()` (Diverse Shuffle, BPM Flow at 120, Chill & Ambient, Workout & High Energy, Late Night Groove): DIM index `1`–`5`; `●` (bold) on the profile in use, with a DIM `ACTIVE` at the right, only when the picker lists it (`MixProfile::picker_row`); `○` (MID) on the others.
- A profile in use that the picker doesn't list (Seed Radio from `r`, Focus from the web mood) marks no row; the title row names it instead (`NOW: SEED RADIO`, `NOW: FOCUS`), and the cursor starts on the closest row (`MixProfile::picker_index`: Diverse Shuffle, Late Night Groove).
- The title in UPPERCASE (`MixProfile::title`: `name()` without `Mood:`), and under it `MixProfile::description()` (MID on the cursor row, else DIM). A mood's description ends with its BPM range from `MoodType::bpm_range`, so no BPM number is written twice. Cursor bar on `mix_picker_cursor`.
- Footer `J K SELECT`, `ENTER START MIX`, `ESC CANCEL`; when narrow `J K`, then `ENTER` go, and `ESC CANCEL` always shows whole. No `r` hint.
- Keys: `j` / `k` / arrows wrap; `Enter` starts the cursor's profile; `1`–`5` start that profile (digits from 1 to `picker_profiles().len()`; other digits do nothing); `Esc` / `q` / `M` close.

#### Add-to-playlist picker (`L`, mockup 3e, `src/ui/playlists.rs`)
- 52 columns wide (2 columns inside the screen when narrower), as tall as its rows up to the screen less 2 rows. Full frame while that leaves at least 3 list rows, else compact (title in the border, no blank rows).
- Chip ` ADD TO PLAYLIST `. Header `ADDING` (DIM, 8 columns) and what is being added in bold UPPERCASE (the track title, or `<folder> (folder)`; `PendingAdd::label`), with a single track's artist after it in MID (`  ·  MIRA LENZ`, only when tagged), over a `────── ▪` rule.
- Rows: the playlists as tab `[5]` lists them (UPPERCASE, track count in DIM at the right edge), then `+ NEW PLAYLIST…` (MID; `NEW_PLAYLIST_ROW`). The cursor bar starts on `last_playlist` (or the first row if it no longer exists).
- Only the rows that fit are drawn (`kit::visible_window` over `PlaylistsUi`'s picker `ListState`, reset on open), so the cursor row always shows however many playlists there are.
- Footer `ENTER ADD`, `J K SELECT`, `ESC CANCEL`. `j` / `k` / arrows wrap, `Enter` adds (or, on `+ NEW PLAYLIST…`, opens the name prompt and adds to the new playlist), `Esc` / `q` cancel.
- What `L` adds (on tab `[2]`, the focused pane's selection; on every other tab the now-playing track, `Nothing is playing` when idle):
  - a browser track row: that track; the queue pane: the selected queue track;
  - a browser folder row: the audio files directly inside it, in name order, not recursive, listed on a worker (`Couldn't read <folder>` on error, `No audio files in <folder>` when there are none; after `+ New playlist…` these read `Created "<name>"; couldn't read <folder>` / `Created "<name>"; no audio files in <folder>`);
  - a `★ Music Homes` or `..` row: status `Select a track or folder`.
  - Entries are built from metadata already in memory (queue row, now playing, `MetadataCache`, else the file stem); the share is never read on the UI thread.
- Results: `Added "<title>" to <name> (<n> tracks)`, `Already in <name>`, `Added <k> to <name>, skipped <m> already in it`.
- A folder add follows its playlist by id (`PlaylistStore::id_of`), so a rename made while its tags are read moves the add with it. If the playlist was deleted meanwhile it reports `<name> was deleted; nothing added`; if `+ New playlist…` created it for the add and the create then failed, `<name> couldn't be created; nothing added`.

#### Playlist name prompt (`c`, `R`, `+ New playlist…`; mockups 3b / 3c)
- 64 columns wide (2 columns inside the screen when narrower), 10 rows (compact, 6 rows, on screens under 10 rows).
- Chip ` NEW PLAYLIST `, or ` RENAME ` with the playlist's name (UPPERCASE, as listed) as the subtitle.
- `NAME` (DIM, 7 columns), the name as typed in bold with a `█` cursor, an INK rule under the field, a blank row, then `ENTER SAVE` `ESC CANCEL`. A name wider than the field scrolls: the field shows `…` and the end of the name, so the cursor stays visible.
- An error replaces those keys, in ERROR, and turns the field's rule ERROR, so the prompt keeps its size. An error wider than the row wraps onto the blank row above (`kit::wrap_clipped`); one that needs more than those two rows ends the second with `…`.
- Keys: every character is text, including `q` and digits. `Backspace` deletes, `Enter` confirms, `Esc` cancels.
- An invalid or duplicate name keeps the prompt open and shows the error:
  - duplicates ignore case (full Unicode case folding) and Unicode normalization, as APFS does: e.g. `Name can't contain /` or `"Road Trip" already exists`;
  - a name taken by a file the store skipped at load (or found in the way of a create): `A playlist file named "Latin" exists but couldn't be loaded` (`NameError::Reserved`);
  - a name another playlist's unfinished create or rename starts from or goes to: `Still renaming "Old" to "New", try again` (or `Still creating "Mix", try again`) until the background writer has finished with it;
  - renaming a playlist whose own previous rename is unfinished: `Still renaming "Old" to "New", try again` too, whatever the new name.

#### Playlist delete confirmation (`D`, mockup 3d)
- The one modal in ERROR: border, chip ` DELETE PLAYLIST ` and the ` Y ` keycap (GROUND on ERROR). Same size as the name prompt.
- `Delete "<name>" (<n> tracks)?` in bold (the name cut with `…` so the count stays), `The .m3u8 file is removed. Audio files are not touched.` (MID), then `Y DELETE` `ANY KEY CANCEL` (the latter dropped first when narrow, then `Y DELETE` when it doesn't fit whole either, `kit::fit_top_hints`).
- `y` deletes the file. Any other key cancels (status `Kept "<name>"`).

### 3.7 Key Navigation Invariants
1. **List wraparound:** `Up` / `Down` (`k` / `j`) wrap in every interactive list (Queue, file browser, Devices, Settings in display order, search results, mix profiles, Playlists, playlist tracks, add-to-playlist picker): `Up` at the first row goes to the last, `Down` at the last to the first.
   - Page keys on tabs `[2]` and `[3]` stop at the first / last row and wrap only when pressed there; on `[4]` they jump to the ends.
   - On tab `[5]`, `g` / `G` / PageUp / PageDown never wrap, and neither does moving a playlist track with `⇧K` / `⇧J`.
2. **Precedence** (`AppUI::run` in `src/ui/event_loop.rs`, then `AppUI::handle_input_key` in `src/ui/keys.rs`):
   - Resizing needs no handler: each frame is drawn at the terminal's current size, and `run` redraws at least every 100 ms and after every key.
   - Modals and typed text take every key first, in this order: help, search, spotlight (in `run`), then the mix picker, the startup home editor and the playlist modals (in `handle_input_key`). `Ctrl+C` quits from any of them. Dismissal: help `Esc` / `q` / `?`; search `Esc` (letters are query text); mix picker `Esc` / `q` / `M`; add picker `Esc` / `q`; the name prompt and the startup home editor treat `q` as text and close only with `Esc` / `Enter`; the delete confirmation closes on any key.
   - In `handle_input_key`, a key other than a seek key first applies a pending debounced seek, and a key other than `←` / `→` (`s` and `c` aside) applies a pending debounced skip.
   - Global hotkeys come next, so tab handlers cannot reuse them: `q` / `Esc` / `Ctrl+C` (quit; while a directory scan is running, `Esc` cancels the scan instead), `?`, `Tab` / `Shift+Tab`, `1`–`5`, `n` / `N`, `Space`, `s`, `Left` / `Right`, the seek keys, `+` / `=` / `-`, `v`, `/`, `f`, `z` / `Z`, `p`, `P`, `M`, `L`. Some act only on certain tabs (`v`, `/`, `f`). There is no global `A`, `R`, `m` or `t` binding; `R` is the tab `[5]` rename key.
   - Tab and pane keys last (`handle_tab_key_with_modifiers`; tab `[5]`: `handle_playlists_tab_key`).
3. **Global key mappings:**
   - `1`–`5` / `Tab` / `Shift+Tab`: switch tab (cycling covers all 5 tabs). `n`: toggle Now Playing ↔ Queue & Library.
   - `Space`: play / pause (on `[4]` it cycles the selected value). `s`: stop.
   - `Left` / `Right`: previous / next track, debounced (on `[4]` they cycle the selected value).
   - `Shift` / `Ctrl` / `Alt` + `Left` / `Right`, `[` / `]`, `<` / `>`: seek backward / forward by `seek_step_seconds` (default 10 s), debounced.
   - `+` (or `=`) / `-`: volume up / down by `volume_step`.
   - `v` (`[1]`, `[2]`): toggle Tags / Filenames. `/`: spotlight in the library, search on `[1]` and in the queue pane, the startup home editor on `[4]`. `f` (`[1]`, `[2]`): fuzzy search.
   - `p`: start Autopilot on the current directory (or the highlighted Music Home). `P`: reshuffle the Autopilot seed from the next track on. `M`: open the mix profile picker.
   - `z` / `Z`: shuffle the queue.
   - `r` (tab key): Seed Radio from the now-playing track on `[1]`; on `[2]` from the selected browser or queue track (on any other library row it rescans the directory); rescans renderers on `[3]`.
   - `L`: add to a playlist (opens the add-to-playlist picker; what it adds is listed there).

---

## 4. Autopilot & DJ Sequencing Rules

1. **Fair Multi-Artist Candidate Sampling**:
   - In large libraries, the scanner shuffles top-level subdirectories and samples evenly across artist folders rather than greedily consuming all tracks from the first alphabetical folder.
   - Even when an artist directory has 500 tracks, it contributes only a small quota to the candidate pool, guaranteeing diverse representation across 50–100 distinct artists.
2. **Targeted Match Gathering & Supplementation**:
   - When filtering by mood, tempo, or seed similarity, the scanner continues searching until at least 80 matching candidates are found, 500 folders are scanned, or 20 seconds have passed (`ProfileScanLimits`), whichever comes first. At least one folder is always scanned.
   - The first matching track is sent to the queue immediately so playback starts while gathering continues; the rest of the mix is sequenced to follow it (`DjMixer::sequence_mix_after`).
   - If a library has fewer matching tracks than the queue limit (60) but contains other songs (`total_candidates > 10`), Quickstream places all matches at the top of the mix and supplements remaining slots with diverse candidates from other artists so the queue is never truncated.
3. **DJ Sequencing (`DjMixer::sequence_mix`)**:
   - **Artist Separation** (`ARTIST_COOLDOWN = 8`): after a real artist plays, the next 8 tracks must be by other artists (a repeat is at least 9 queue positions later). Hard rule: a cooldown-violating candidate is only chosen when no candidate satisfies it, and then the one whose artist played longest ago.
   - **Generic Artists**: "Various Artists", "Unknown Artist", and untagged tracks are exempt from the artist cooldown.
   - **Album Separation** (`ALBUM_COOLDOWN = 12`): after an album plays, the next 12 tracks must be from other albums; hard in the same way, ranked after the artist rule. Untagged albums are exempt.
   - **Position-based distances**: cooldowns count queue positions, so untagged/generic tracks still count as tracks between repeats; the already-queued track passed to `DjMixer::sequence_mix_after` is position 0.
   - **Camelot Harmonic Flow**: Key transitions scored by harmonic compatibility (exact match, relative major/minor, ±1 energy boost).
   - **BPM Gradient**: Tracks sequenced with smooth tempo progression.
4. **Analysing the Candidate Pool** (issue #30):
   - A profile mix (BPM Flow, moods, Seed Radio) gathers up to ~200 candidates for a 60-track mix. After its tracks, before `Finished`, it sends `AutopilotEvent::Candidates` once: the pool's paths it didn't queue, less those whose analysis row is current (mtime and size, as the worker checks them: `worker::row_is_current`), judged while gathering from the row matching already reads and the file's stamp (`worker::stamp_of`) from the stat that reads its tags, so no file or row is read again. `library::analysis_candidates` builds the list: matched candidates first, then the others, at most `MAX_PENDING_CANDIDATES` (a pool can hold 120 matched and 200 others), so the next mix has more tracks with a BPM, key and energy. Diverse Shuffle has no pool beyond what it queues and sends none.
   - `AppUI::poll_background_autopilot` hands them to `AnalysisWorker::enqueue_candidates`; they are never queued. Queued tracks (`AnalysisWorker::enqueue`) are always analysed first, so a 200-track burst delays a queued track by at most the one candidate in flight; a waiting candidate that reaches the queue moves ahead. At most `MAX_PENDING_CANDIDATES` (200) candidates wait (a later run's push out the oldest, which may be offered again; one run past the bound keeps its first, `AnalysisQueue::push_candidates`); a path is analysed at most once a session. A panic while a track is analysed is caught: the track counts as failed and the thread goes on, so the pending count never sticks. Analysis stays on the worker's threads and writes only the analysis database.

---

## 5. Development & Testing Workflow

### Running Tests
Execute the full test suite using Cargo with single-threaded test execution (required for deterministic local TCP port binding in web server tests):
```bash
cargo test -- --test-threads=1
```

Tests that bind all network interfaces (real SSDP discovery, LAN web server binding) are marked `#[ignore]` so local runs do not trigger macOS firewall prompts. CI runs them on Linux (`.github/workflows/ci.yml`); to run them yourself:
```bash
cargo test -- --test-threads=1 --ignored --skip test_discover_devices_real
```
(`test_discover_devices_real` scans your LAN for real renderers and asserts nothing; drop the `--skip` to include it.)

To run a specific test:
```bash
cargo test --test test_autopilot test_autopilot_mix_generates_full_queue_when_library_has_sparse_matches
```

### Code Quality & Lints
Always verify formatting and zero compiler and clippy warnings (including test code) before committing. CI (`.github/workflows/ci.yml`, Ubuntu + macOS) runs the same checks plus the test suite on every push and pull request to `main`:
```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```
Formatting follows `rustfmt.toml`; the one-time bulk reformat commit is listed in `.git-blame-ignore-revs` (`git config blame.ignoreRevsFile .git-blame-ignore-revs`).

### Release Build
```bash
cargo build --release
```

### Important Rules for Future Agents
- **No Temporary Root Scripts**: Do not create ad-hoc test scripts (e.g. `test_something.py`, `scratch.rs`, `patch.py`) in the repository root. Always place unit and integration tests inside `tests/` following the `test_*.rs` naming convention.
- **Test Isolation**: Every integration test file declares `mod common;` and every `#[test]` starts with `common::isolate();` (`tests/common/mod.rs`), which points `XDG_CONFIG_HOME` / `XDG_CACHE_HOME` / `XDG_DATA_HOME` at a per-process temp dir so tests never read or write the real `~/.config`, `~/.cache` or `~/.local/share` quickstream dirs. `tests/test_isolation.rs` fails if a test skips it.
- **Fake Executables**: A test that writes a script and then runs it (a fake `mpv`, `ffplay` or `ffmpeg`) writes it with `common::fake_exec::write_script`, never through the test process's own file descriptor (`fs::write` + `set_permissions`). On Linux exec fails with ETXTBSY while any process holds the file open for writing, and a child another test thread forks meanwhile inherits that descriptor until its own exec; the helper writes through a short-lived `/bin/sh` instead (`tests/test_fake_exec.rs` races it on Linux).
- **Graceful Resource Cleanup**: All networking threads (`DLNAScanner`), streaming HTTP sockets, Tokio runtimes, and subprocesses (`mpv`, `ffplay`) must terminate cleanly upon shutdown or `ctrlc` signal.
- **File Modifications**: Use targeted file editing tools (`replace_file_content` or `write_to_file`) rather than shell sed/cat scripts.
