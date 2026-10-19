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
   - **Local Playback**: Implemented by `LocalPlayer` (`src/player/local.rs`) utilizing an `mpv` JSON-RPC IPC Unix domain socket with a dedicated listener thread, falling back to `ffplay` subprocess.
   - **DLNA / UPnP Streaming**: Implemented by `UPnPPlayer` (`src/player/upnp.rs`) sending SOAP XML payloads (`AVTransport` / `RenderingControl`) to discovered network renderers, while `WebServer` (`src/web.rs`) serves audio over HTTP with Byte-Range support (`206 Partial Content`) and continuous streaming (`/stream/continuous-<nonce>.wav`, armed per UPnP continuous session via `StreamServerState::arm_continuous_session()`; other continuous paths return 404).

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
│   ├── main.rs           # CLI argument parsing (clap), startup flags, signal traps, app bootstrap
│   ├── lib.rs            # Library entrypoint and module exports
│   │
│   ├── ui/               # Terminal User Interface (Ratatui + Crossterm)
│   │   ├── mod.rs        # AppUI state machine, 60 FPS render passes, tab switching, hotkey router
│   │   ├── playlists.rs  # Tab 5 Playlists: PlaylistsUi, PLAYLISTS / tracks panels, name prompt / delete confirm / add picker, L key, play & queue
│   │   ├── queue_tab.rs  # Tab 2 LIBRARY / QUEUE panels: rows mapped at draw time, IN QUEUE set + totals + column layout from the whole list, cached per revision
│   │   ├── tag_reads.rs  # SharedTagReads: tag reads shared by browser/queue/folder-add workers, in-flight path set
│   │   ├── features.rs   # TrackFeaturesWorker: current track's analysis features (BPM/key/energy) read on a worker, retried every 5 s
│   │   ├── search.rs     # SearchModalState, streaming fuzzy search (fuzzy-matcher / SkimMatcherV2)
│   │   ├── theme.rs      # Telemetry colour tokens: ColorDepth (COLORTERM), Palette, style constructors
│   │   ├── kit.rs        # Key row, volume meter, progress bar, panel header (panel/PanelSpec), visible_window, list_row columns, width-aware fit / visible_tail, duration (MM:SS / H:MM:SS), big_digits (3-row numerals), ClockCache
│   │   ├── chrome.rs     # Screen chrome: ChromeLayout rows, header, tab row (tab_titles), strip, key row (tab_key_hints)
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
│   │   ├── mixer.rs      # DjMixer::sequence_mix (harmonic flow, BPM gradient, artist/album spacing)
│   │   ├── db.rs         # SQLite persistent feature store (~/.cache/quickstream/track_analysis.db)
│   │   └── online.rs     # Lightweight metadata/tag enrichment via ureq HTTP client
│   │
│   ├── library.rs        # Recursive walkdir scanner, progressive autopilot streamer, JSONL cache, MetadataCache (with revision and per-folder folder_revision)
│   ├── metadata.rs       # Lofty audio metadata extraction, cover art decoding, path heuristics
│   ├── playlist.rs       # Pure M3U8 playlist model: parse/write, name validation, safe file paths, queue plan
│   ├── playlist_store.rs # In-memory PlaylistStore (sorted), write-through to disk via PersistHandle
│   ├── persist.rs        # Background atomic writer: ordered save / create / delete / rename (PersistHandle), failures reported back
│   ├── queue.rs          # PlaybackQueue (private fields, revision() bumped by every mutation, drawn from one process-wide counter so new/cloned queues never repeat one), FastRng (SplitMix64 deterministic PRNG), shuffle & loop
│   ├── web.rs            # Embedded HTTP byte-range server, continuous WAV streaming, REST API (incl. playlists), Web UI
│   ├── web_access.rs     # Web access control: pairing token, Host/Origin/Content-Type checks, allowed folders
│   ├── discovery.rs      # SSDP UDP multicast discovery (239.255.255.250:1900), XML device parsing
│   ├── settings.rs       # User preferences model, cycle definitions, config.json persistence
│   ├── paths.rs          # XDG Base Directory resolution and backward-compatible migration
│   └── logger.rs         # Non-blocking rotating file logger (~/.local/share/quickstream/quickstream.log)
│
└── tests/                # Integration test suite (Rust)
    ├── test_autopilot.rs     # Autopilot progressive streaming, candidate gathering, DJ mix profiles
    ├── test_browser.rs       # File browser navigation, bookmarks, cache, and metadata toggles
    ├── test_discovery.rs     # SSDP discovery packet parsing and device extraction
    ├── test_local_player.rs  # mpv / ffplay local audio backend lifecycle
    ├── test_metadata.rs      # Lofty audio header parsing, duration, and path heuristics
    ├── test_now_playing.rs   # Tab 0 Now Playing hero view layout and rendering
    ├── test_big_digits.rs    # kit::big_digits glyphs (2a style sheet), colon, widths
    ├── test_track_features.rs # Features worker: current track only, stale results dropped, retry, never blocks, shutdown; web BPM/key
    ├── test_playlists.rs     # M3U8 parse/write round trips, name rules, safe paths, queue planning
    ├── test_playlist_store.rs # Store create/add/remove/move/rename/delete, case-only rename, load skips, file_path_of during a pending rename
    ├── test_playlists_ui.rs  # Tab 5 tab switching, two-pane navigation, rendering, modal hook, help
    ├── test_playlists_edit.rs # Name prompt, create/rename/delete, K/J move, d/x remove
    ├── test_playlists_add.rs  # L key, add picker, folder adds, last_playlist
    ├── test_playlists_play.rs # Missing-file checks, Enter replace+play, a append, all-missing
    ├── test_search.rs        # Multi-threaded fuzzy search, scoped scan, and modal navigation
    ├── test_ui_chrome.rs     # Theme palettes, key row trimming, meters, text fitting, width tiers; rendered chrome
    ├── test_ui_panels.rs     # Panel header rows/colours, compact footnote, visible_window vs ratatui List, list_row columns, cursor, ▸ in its own column
    ├── test_queue_revision.rs # PlaybackQueue::revision bumps on every mutation, not on reads or no-ops, never shared by two queues; truncate_after
    ├── test_ui_lists.rs      # Tab 2 and tab 5 panels: columns, IN QUEUE / ▪ / ▶, cursor vs ▸ (never hiding ▶ / ▪), columns stable while scrolling, width rules, spotlight (real subfolders only), CJK, durations (H:MM:SS), folder duration (own-folder recomputes only), MISSING FILE, playlist file footnote
    ├── test_shared_tag_reads.rs # No concurrent duplicate tag reads; skipped/waiting requests still get tags
    ├── test_upnp.rs          # UPnP SOAP XML generation, AVTransport state machine, continuous stream
    ├── test_web.rs           # Embedded web server, byte ranges, REST API, and command dispatch
    └── test_web_playlists.rs # Web playlists: snapshot, GET /api/playlists, play/append, edits, path checks
```

TUI tests render through the shared helpers in `tests/common/tui.rs` (`render`, `rows`, `find_row`, `find_text`, `cell_style`, `text_styles`): find chrome rows by their text, not by row number. `tests/common/fake_player.rs` (`FakePlayer`) stands in for the player when a test needs a given position, play state or backend.

The background writer's create, delete and rename tests (used by `PlaylistStore`), including the failure reports, are in the existing `tests/test_persistence_worker.rs`, which the map above does not list.

---

## 3. Terminal UI Architecture & Ratatui Guidelines

### 5-Tab Layout Structure
- **Screen chrome** (`src/ui/chrome.rs`, Telemetry style 2a, colours from `ui::theme`; the tab bodies keep their own widgets and foreground colours until they are restyled, with selection rows in `theme::cursor()`): `render` first fills every cell with `theme::base()` (INK on GROUND), then draws, with 2-column gutters on both sides: top margin, **header**, blank row, **tab row**, a gap (2 rows from 30 rows tall, else 1), the tab body, blank row, **rule** and **strip** (not on Now Playing), blank row, **key row**, bottom margin. Under 24 rows: tab row (with the header's right-hand part folded in), body, strip, key row (`ChromeLayout`).
  - **Header**: `QUICKSTREAM ▪ <version>` on the left; on the right the output (`LOCAL/<BACKEND>` or `DLNA/<RENDERER>`; the backend is the one actually playing, from `AudioPlayer::backend_name`, so `LOCAL/FFPLAY` once mpv is missing or failed, and the `player_backend` setting until the player has reported), `WEB :<port>` (only while the web server runs) and the clock (`HH:MM`, `kit::ClockCache`), 4 columns apart. When narrow, `WEB` goes first, then the output.
  - **Tab row**: `1 NOW PLAYING` `2 QUEUE & LIBRARY` `3 DEVICES` `4 SETTINGS` `5 PLAYLISTS` from 110 columns; `2 QUEUE` at 90–109; `1 NOW` … under 90; when even those don't fit, only the numbers plus the active tab's short label (#59); under about 37 columns the numbers farthest from the active tab go too, and an active label too wide on its own is cut with `…`, so the active tab is always shown and never clipped (`tab_titles`). The active tab is a chip in `theme::cursor()` (GROUND on INK, bold); the others are MID.
  - **Key row**: keycaps (` SPC ` in `theme::keycap()`) and MID labels, 4 columns apart, per tab and focused pane (`AppUI::tab_key_hints`; only keys the handlers act on, e.g. tab 5 shows `C ⇧R ⇧D` in the list pane and `⇧J ⇧K MOVE TRACK` / `D REMOVE` in the tracks pane). `? KEYS` is always kept on a tab (except while spotlight or the startup home editor takes typed text, where `?` is text). While a modal is open the row shows that modal's keys instead of the tab's, and no `? KEYS` (`AppUI::modal_key_hints`): help `ESC CLOSE`; search `↑↓ SELECT` `ENTER ADD & CLOSE` `TAB ADD` `⇧TAB ADD ALL` `ESC CLOSE`; mix picker `J K SELECT` `ENTER START` `1-5 PROFILE` `ESC CLOSE`; name prompt `ENTER SAVE` `ESC CANCEL`; delete confirmation `Y DELETE` `ANY KEY CANCEL`; add picker `J K SELECT` `ENTER ADD` `ESC CANCEL`. Now Playing: `SPC PLAY/PAUSE`, `←→ TRACK`, `[ ] SEEK <seek_step_seconds>S`, `+− VOL`, `N QUEUE`, `? KEYS`, then the volume meter; `N QUEUE` stays at every width and Esc is never offered (#57). At 80–89 columns at most 5 keys. The status message sits at the right end (ERROR colour when it contains error/failed/couldn't, else MID; messages that repeat the screen, such as `Welcome to Quickstream…` or `Playing:`, are not shown). A long status drops the least important keys first, then is cut with `…`.
- **Main View Area**:
  - **Tab 0 (`[1] Now Playing`)**: Hero view with large typography (Title, Artist, Album), technical codec/format badges, timeline, and upcoming queue preview. While autopilot runs, its badge names the mix profile (`[⚡ AUTOPILOT · BPM FLOW]`, from `MixProfile::short_name`). While a debounced skip is pending (`pending_track_skip`), the state pill reads `» QUEUED` and the timeline shows the target track from `00:00` of its own duration (`00:00` while that is unknown; never the old track's, which the player still holds), as the strip does on the other tabs. The current track's analysis features (BPM, key, energy) come from `features::TrackFeaturesWorker` (`AppUI::poll_track_features` each tick, read with `AppUI::track_features()`; `AppUI::set_track_feature_source` is the test seam): it reads the analysis database on a worker when the track changes, again every 5 s (`RETRY_INTERVAL`) while there are none yet, and drops results for a track that is no longer current. Nothing on the UI thread reads the database; `update_web_state` takes `bpm`/`key` from the worker's latest result. `kit::big_digits(text)` / `big_digits_width` draw times (`02:41`, `1:15:00`) in the 2a heavy box-drawing numerals (digits 3 columns, `:` 1 column, glyphs 1 column apart); the layout does not use them yet.
  - **Tab 1 (`[2] Queue & Library`)**: two Telemetry panels (`src/ui/queue_tab.rs`, mockup 2c) drawn with `kit::panel` / `kit::list_row`, no borders; only the visible rows are drawn (`kit::visible_window`), and PageUp/PageDown move by the rows each list shows. Focus toggled with `h` and `l`: the focused pane has the cursor bar (`theme::cursor()`), the other a MID `▸` in a 2-column marker column every row of both panels starts with, so it never hides the playing track's `▶` (queue) or `▪` (library) when the cursor is on it.
    - **Left Pane (`LIBRARY`)**: the file browser, showing the directory tree and bookmarked Music Homes (`~/.config/quickstream/bookmarks.json`). Rows are mapped from `browser_entries` at draw time (stored labels unchanged): `★ MUSIC HOMES`, `.. PARENT` (`.. FILE SYSTEM` in Music Homes), `DIR NAME/` (no child counts), file rows `01 TITLE ARTIST FORMAT 04:02` (title/artist from `MetadataCache` in Tags mode, the file name in its original case in Filenames mode; artist only in Tags mode from 90 columns; a length only when known). A file in the queue shows `IN QUEUE` in the format column and the playing one `▪` in place of its number. Label note `TAGS`/`FILES` (`v` toggles); footnote: the folder (`~` for home, original case), `<cursor> / <entries>` and, once every file's length is in memory, the folder's total length (`MetadataCache::folder_revision` of the browsed folder invalidates it, so tags read for other folders never recompute it; never read from disk). While a folder is read: `··· READING DIRECTORY · <n> ITEMS`. Spotlight (`/`, mockup 3a): label note `SPOTLIGHT`, footnote row ` / ` keycap, the query as typed with a block cursor, `<k> OF <n> SUBFOLDERS` / `NO MATCHING SUBFOLDERS` (ERROR) / `TYPE TO JUMP (<n> SUBFOLDERS)`; only real subfolders (in Music Homes, the homes) are matched and counted, never the `..` or `★ MUSIC HOMES` rows (`is_subfolder_entry`); matched characters are underlined and, while something matches, the other rows are DIM.
    - **Right Pane (`QUEUE`)**: `PlaybackQueue` rows `▶ 01 TITLE ARTIST 04:02` (`▶` and bold on the current track; artist from 90 columns), label note the queue mode (`LOOP`, `STOP AT END`, `REPEAT ONE`), footnote `<n> TRACKS · <total>` (total only when every length is known). The queued-path set and the totals are cached per `PlaybackQueue::revision()`. In both panels the columns (artist, format, length widths) are decided from the whole list, not the rows on screen, so they don't change while scrolling; the library's are cached with its total, the queue's per revision.
    - **Width rules**: from 90 columns the panels are 4 columns apart; at 80–89 the artist and format columns drop, the panels are 2 apart and each footnote moves to its label row (`7/14`, the queue's track count), except the spotlight query, which keeps its row; under 80 only the focused pane shows and `h`/`l` switch it.
  - **Tab 2 (`[3] Devices`)**: Discovered DLNA MediaRenderers and `Local Audio` with active output indicator (`●`).
  - **Tab 3 (`[4] Settings`)**: User preferences editor (metadata display, audio backend, seek interval, volume step, queue mode, PCM transcode, web server toggle, startup home directory).
  - **Tab 4 (`[5] Playlists`)**: two Telemetry panels (`src/ui/playlists.rs`, mockup 2f, state in `AppUI::playlists: PlaylistsUi`) drawn with `kit::panel` / `kit::list_row` like tab `[2]`, no borders, only the visible rows drawn; the PLAYLISTS panel takes 23/56 of the width (46 + 4 + 66 columns at 120). A startup worker loads the playlists. Until the load finishes, the tab shows `Loading playlists…` and edits are refused with `Playlists are still loading`. If the folder exists but can't be listed, the tab shows `Couldn't load playlists` (ERROR) and edits stay refused for the session, so no create can overwrite a file the store never read. Focus toggles with `h` and `l`. `j`/`k`/`Up`/`Down` move with wraparound. In either pane `g`/`G` (or `Home`/`End`) jump to the first/last row and `PageUp`/`PageDown` move by the rows the pane shows, stopping at the first/last row without wrapping.
    - **Left Pane (`PLAYLISTS`)**: label note the playlist count; one row per playlist (`NAME` and its track count, DIM, against the right edge), sorted case-insensitively. While the tracks pane has focus the open playlist keeps a bold `▸`. `c` create, `R` rename (prompt prefilled with the current name), `D` delete after `Delete "<name>" (<n> tracks)? y/n`, `Enter` focus the tracks pane, `a` append the whole playlist to the queue. Empty state: `No playlists yet. Press c to create one, or L on any track.`
    - **Right Pane (tracks)**: label the playlist's name, note `<n> TRACKS · <total>` with unknown durations left out of the total (no total when none is known; cached per `PlaylistStore::revision()`, cursor and missing-file checks, together with the column layout, which comes from the whole playlist so it doesn't change while scrolling; the PLAYLISTS count column likewise from every playlist), footnote the playlist's file (`~` for home, original case, DIM; while a rename of it is unsettled, the old name the file still has, from `PlaylistStore::file_path_of`). Rows read `01 TITLE ARTIST 04:02` (a length only when known); missing files read `01 TITLE MISSING FILE --:--` with `MISSING FILE` in ERROR and the title DIM. Only the focused tracks pane marks its cursor. Existence is checked on a worker and cached per path for the session. Rows not yet checked render normally. `Enter` replaces the queue with the playlist and plays from the selected track: it stops any autopilot scan and continuous stream first (`AppUI::reset_playback_session`) and skips missing files. The worker checks the selected entry first, then the ones after it, then the ones before it; the first that exists (the track playback starts on) plays as soon as it is found, and the rest of the playlist is put around it when their checks finish (status `Queued <k> tracks from <name>, <m> missing`). Tracks queued meanwhile stay after the playlist; if the queue is replaced or cleared meanwhile, the rest is dropped. The started track is matched by its entry index (a file listed twice is queued once per entry) and, in the queue, as the current track. If it ends, or Next/Previous is pressed, before the rest is checked, the skip waits for the rest (`Waiting for the rest of <name>…`, `Next track plays once <name> is checked…`, `AppUI::hold_skip_for_playlist_fill`; `AppUI::handle_track_finished` is the event loop's track-end hook) instead of stopping or replaying. If every file is missing, it leaves the queue untouched and the status reads `None of <name>'s <n> tracks found. Is /Volumes/Music mounted?`, naming the top two folders above the first entry's file (no question when the file is fewer than two folders deep, e.g. `/Volumes/ghost.mp3`). `a` appends the whole playlist. `K`/`J` move the selected track up/down (no wraparound; on tab `[2]` these keys page instead). `d`/`x`/`Del` remove it. Empty state: `Empty. Press L on a track in tab 2 to add it.`
    - **Width rules**: as tab `[2]`: from 90 columns the panels are 4 columns apart; at 80–89 the artist column and the file footnote drop and the panels are 2 apart; under 80 only the focused pane shows and `h`/`l` switch it (a hidden pane pages by the shown one's rows).
    - Edits save automatically through the background writer (`PersistHandle`). There is no save key.
- **Web Dashboard Playlists (issue #50)**: the dashboard's `Playlists` tab uses the same store and code as tab 5.
  - Read: `GET /api/playlists` serves the `WebPlaylists` snapshot the UI thread publishes in `NowPlayingState::playlists` (an `Arc`, `#[serde(skip)]` from `/api/status`) whenever `PlaylistStore::revision()`, the missing-file cache, the load state or `last_playlist` changes (`AppUI::publish_web_playlists`, called from `update_web_state`); a missing-file-cache change alone republishes at most every 500 ms (`WEB_EXISTS_REPUBLISH_INTERVAL`). The playlists folder is never read per request. `/api/status` carries `playlists_version` so the page knows when to fetch again. The snapshot has `state` (`loading`/`failed`/`ready`), the TUI's `message` when not ready, `revision`, `last_playlist` and each playlist's entries (title, artist, duration, path, `missing`: `true`/`false`/`null` until checked). Responses clear the path and set `outside` for entries outside the allowed web folders, as `/api/status` does. Serving it sends `WebCommand::CheckPlaylists`, which checks every playlist's unchecked files on one worker (paths checked or in flight are skipped, so repeated fetches start no new checks).
  - Play / append: `/api/action` `playlist_play` (`name`, optional `index`, which requires `revision`) and `playlist_append` (`name`). The web thread checks every entry of the snapshot's playlist: one whose file resolves outside the allowed folders, or a missing one whose path lies outside them, refuses the whole request with 403 and `WebCommand::PathRejected`, like `add_file`. Entries are checked with `web_access::AllowedRoots`, which canonicalizes the allowed folders once per request (and once per play on the worker), with the same answers as `resolve_allowed_path`. Otherwise it sends `WebCommand::QueuePlaylist` with the checked entries and a `WebReply`, waits up to 5 s, and returns the UI's answer: 200 with `Checking <n> tracks in <name>…`, or 409 with its refusal (`Still checking <name>…`). The UI queues them with tab 5's worker code (`queue_playlist_entries`: same `plan_queue`, reset of autopilot scan and continuous stream on replace, same status messages, which name the playlist instead of saying `(web)`). For web requests the worker also requires each checked file to resolve inside the allowed folders. Refusals (JSON `{"ok":false,"error":…}`): not loaded (409, `Playlists are still loading` / `Couldn't load playlists`), unknown name (404), empty playlist (409), bad index (400), `index` without `revision` (400, `Missing revision: reload the playlists and try again`), stale `revision` (409, `Playlists changed; try again`).
  - Edit: `playlist_create` (`name`), `playlist_rename` (`name`, `new_name`), `playlist_delete`, `playlist_add` (`name`, `path`: checked like `add_file` on the canonical path, 403 + `PathRejected` outside the allowed folders, 400 if not an audio file; stored as `playlist::normalize_path` of the requested path, the form a terminal add stores, and `Already in <name>` if the playlist holds either form), `playlist_remove` (`name`, `index`, `revision`), `playlist_move` (`name`, `index`, `direction` `up`/`down`, `revision`); `revision` is required for remove and move (400 without it). The web thread sends `WebCommand::EditPlaylist(PlaylistEdit, WebReply)` and waits up to 5 s for the UI's answer; `AppUI::apply_web_playlist_edit` applies it through `PlaylistStore` only (same `validate_name` rules, duplicate and reserved-name checks and messages as the name prompt), shows it in the status line, republishes the snapshot and answers. 200 `{"ok":true,"message":…}`, 409 with the store's message, 503 if the UI didn't answer. Tab 5 keeps its playlist selected (following a web rename).
  - All of these share the `web_access` checks (pairing token, Host, Origin, JSON Content-Type). The dashboard builds every row from text and `dataset` only (see `tests/support/dashboard_dom_harness.js`).
- **Mini player strip** (tabs `[2]`–`[5]`, under a RULE-coloured rule): state glyph (`▶` playing, `‖` paused, `■` stopped, `»` while a debounced skip is pending, with `QUEUED`) and the title in bold INK, the artist in MID from 90 columns; on the right elapsed time, progress bar (`▕╱╱╱▏`, elapsed INK, rest RULE), duration and `VOL <n>`. While autopilot runs, the mix profile (`⚡ BPM FLOW`, `MixProfile::short_name` uppercased, MID) comes first on the right; it has the lowest priority and shows only when the title and artist still fit whole beside it. With no track: `■ NO TRACK LOADED`. Every length and position in the TUI (list rows, totals, the strip, Now Playing) reads `MM:SS` under an hour and `H:MM:SS` from an hour (`1:15:00`), from `kit::duration`. Now Playing has no strip; its volume is the meter at the end of the key row.
- **Overlay Modals** (drawn on GROUND: each clears its area with `kit::clear`, never ratatui's `Clear`, which leaves the terminal's default background; borders and titles keep their colours until the modals are restyled):
  - **Help Modal (`?`)**: Full keyboard shortcut cheat-sheet.
  - **Fuzzy Search Modal (`f`)**: Streaming fuzzy finder with real-time preview card.
  - **Mix Profile Modal (`M`)**: Interactive autopilot mix profile selector (`[1]` Diverse Shuffle, `[2]` BPM Flow, `[3]` Chill & Ambient, `[4]` Workout, `[5]` Late Night Groove).
  - **Add-to-Playlist Picker (`L`)**: the header names what is being added (the track title, or `<folder> (folder)`). The rows list the playlists, with `last_playlist` highlighted (or the first row if it no longer exists), then `+ New playlist…`. `j`/`k`/arrows wrap, `Enter` adds, `Esc`/`q` cancel. Results: `Added "<title>" to <name> (<n> tracks)`, `Already in <name>`, `Added <k> to <name>, skipped <m> already in it`. A folder add follows its playlist by id (`PlaylistStore::id_of`), so a rename made while its tags are read moves the add with it; if the playlist was deleted meanwhile it reports `<name> was deleted; nothing added`, and if "+ New playlist…" created it for the add and the create then failed, `<name> couldn't be created; nothing added`.
  - **Playlist Name Prompt** (`c`, `R`, `+ New playlist…`): every character is text, including `q` and digits. `Backspace` deletes, `Enter` confirms, `Esc` cancels. An invalid or duplicate name (ignoring case, with full Unicode case folding, and Unicode normalization, as APFS does) keeps the prompt open and shows the error, e.g. `Name can't contain /` or `"Road Trip" already exists`. A name wider than the field scrolls: the field shows `…` and the end of the name, so the cursor stays visible. A name taken by a file the store skipped at load (or found in the way of a create) shows `A playlist file named "Latin" exists but couldn't be loaded` (`NameError::Reserved`). A name another playlist's unfinished create or rename starts from or goes to shows `Still renaming "Old" to "New", try again` (or `Still creating "Mix", try again`) until the background writer has finished with it. Renaming a playlist whose own previous rename is unfinished shows `Still renaming "Old" to "New", try again` too, whatever the new name.
  - **Playlist Delete Confirmation (`D`)**: `y` deletes the file. Any other key cancels.
  - Modals intercept keyboard input when active, before the global hotkeys. Most are dismissed with `Esc` or `q`. The playlist name prompt and the Startup Home Directory editor treat `q` as text and close only with `Esc`/`Enter`. `Ctrl+C` quits from any modal.

### Key Navigation Invariants
1. **List Wraparound**:
   All interactive lists (Queue, FileBrowser, Devices, Search Results, Mix Profiles, Playlists, Playlist Tracks, Add-to-Playlist Picker) wrap around: navigating `Up` at index `0` moves to the last item, and navigating `Down` at the end loops back to `0`. Moving a playlist track with `K`/`J` does not wrap, and neither do `PageUp`/`PageDown` on tab `[5]`.
2. **Keybinding Precedence**:
   - Terminal resize events handled first.
   - Active overlay modal input handled next (see Overlay Modals for dismissal keys).
   - Global hotkeys in `AppUI::handle_input_key` come next, so tab handlers cannot reuse them: `q`/`Esc`/`Ctrl+C` (quit; while a directory scan is running, `Esc` cancels the scan instead), `?`, `Tab`/`Shift+Tab`, `1`–`5`, `n`, `Space`, `s`, `Left`/`Right`, the seek keys, `+`/`=`/`-`, `v`, `/`, `f`, `z`/`Z`, `p`, `P`, `M`, `L`. Some act only on certain tabs (`v`, `/`, `f`). There is no global `A`, `R`, `m` or `t` binding. `R` is the tab 5 rename key.
   - Tab-specific and pane-specific keys handle item manipulation.
3. **Key Mappings**:
   - `1`–`5` / `Tab` / `Shift+Tab`: Switch tab (cycling covers all 5 tabs). `n`: Toggle Now Playing ↔ Queue & Library.
   - `Space`: Play / Pause (on `[4] Settings` it cycles the selected value). `s`: Stop.
   - `Left` / `Right`: Global Previous / Next track (on `[4] Settings` they cycle the selected value).
   - `Shift`/`Ctrl`/`Alt` + `Left` / `Right`, `[` / `]`, `<` / `>`: Global Seek backward / forward by `seek_step_seconds` (default 10s).
   - `+` (or `=`) / `-`: Volume up / down by `volume_step`.
   - `p`: Start Autopilot on the current directory (or the highlighted Music Home).
   - `P`: Reshuffle Autopilot seed from next track forward.
   - `M`: Open Mix Profile modal.
   - `z` / `Z`: Shuffle the queue.
   - `r` (tab-specific): Seed Radio from the now-playing track on `[1]`; from the selected browser track or queue track on `[2]` (on a folder row it rescans the directory); rescans renderers on `[3]`.
   - `L`: Add to playlist (opens the Add-to-Playlist Picker). On `[2]` it adds the focused pane's selection. Browser track row: that track. Browser folder row: the audio files directly inside the folder, in name order, not recursive, listed on a worker (`Couldn't read <folder>` on error, `No audio files in <folder>` when there are none; after `+ New playlist…` these read `Created "<name>"; couldn't read <folder>` / `Created "<name>"; no audio files in <folder>`). `★ Music Homes` or `..` row: status `Select a track or folder`. Queue pane: the selected queue track. On every other tab it adds the now-playing track (`Nothing is playing` when idle). Entries are built from metadata already in memory (queue row, now playing, `MetadataCache`, else the file stem). The share is never read on the UI thread.

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
- **Graceful Resource Cleanup**: All networking threads (`DLNAScanner`), streaming HTTP sockets, Tokio runtimes, and subprocesses (`mpv`, `ffplay`) must terminate cleanly upon shutdown or `ctrlc` signal.
- **File Modifications**: Use targeted file editing tools (`replace_file_content` or `write_to_file`) rather than shell sed/cat scripts.
