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
     - Playlists: `~/.config/quickstream/playlists/<name>.m3u8` (`paths::get_playlists_dir()`; extended M3U8, UTF-8, LF, absolute track paths written unchanged, `#EXTINF` plus an `#EXTART` artist line per entry, one file per playlist). Track paths are normalized lexically with `playlist::normalize_path` (no `.` or `..`, no disk access, symlinks not followed) when a file is parsed and before `PlaylistStore::add`'s duplicate check. Every write, rename and delete goes through `PlaylistStore` and `playlist::playlist_file_path`, which refuses any path whose parent is not the playlists folder. A new playlist's file is created with `persist::create_new_file` (temp file, then a hard link, which never replaces), and a rename never replaces another file. The writer reports a failed create or rename back to the store over an mpsc channel (`persist::Reporter`); `AppUI::poll_playlists` then removes the playlist that couldn't be created (`Couldn't create "Mix": a file with that name already exists`, and the name is reserved) or restores the name a failed rename left the file under (`Couldn't rename "Old" to "New": …`). Until then the writer drops saves to that path, so it never overwrites the other file or leaves `New.m3u8` next to `Old.m3u8`. Missing-file checks never prune a playlist, and a file that fails to load is skipped and never rewritten.
     - Track Analysis Database: `~/.cache/quickstream/track_analysis.db` (SQLite)
     - Cache Indexes: `~/.cache/quickstream/music_index.jsonl`
     - Logs: `~/.local/share/quickstream/quickstream.log`

4. **Non-Blocking UI Invariant**:
   - Disk I/O, network SSDP discovery, audio decoding/analysis, metadata extraction, and library scanning **MUST** run asynchronously on background worker threads, worker pools, or Tokio tasks.
   - Communication with the main UI loop is performed using `std::sync::mpsc` channels and lock-free/atomic states. The Ratatui UI event loop must never block or drop below 60 FPS.
   - Tag reads from the browser metadata fetch, the queue metadata pool and playlist folder adds all go through `SharedTagReads` (`src/ui/tag_reads.rs`), which keeps one set of paths being read so no two workers read the same file at once. The browser and queue workers skip a path that is being read; a folder add waits for that read. Every read publishes its tags (on-disk index, `MetadataCache`, `metadata_tx`) before releasing its path, and a failed or panicking read still releases it. `AppUI::set_tag_reader` is the test seam.

5. **Dual Playback Architecture**:
   - Quickstream abstracts playback through the unified `AudioPlayer` trait (`src/player/mod.rs`):
     - `play(path, offset)` / `pause()` / `resume()` / `stop()` / `seek(seconds)` / `set_volume(level)` / `get_position()` / `get_state()`.
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
│   │   ├── playlists.rs  # Tab 5 Playlists: PlaylistsUi, name prompt / delete confirm / add picker, L key, play & queue
│   │   ├── tag_reads.rs  # SharedTagReads: tag reads shared by browser/queue/folder-add workers, in-flight path set
│   │   └── search.rs     # SearchModalState, streaming fuzzy search (fuzzy-matcher / SkimMatcherV2)
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
│   ├── library.rs        # Recursive walkdir scanner, progressive autopilot streamer, JSONL cache
│   ├── metadata.rs       # Lofty audio metadata extraction, cover art decoding, path heuristics
│   ├── playlist.rs       # Pure M3U8 playlist model: parse/write, name validation, safe file paths, queue plan
│   ├── playlist_store.rs # In-memory PlaylistStore (sorted), write-through to disk via PersistHandle
│   ├── persist.rs        # Background atomic writer: ordered save / create / delete / rename (PersistHandle), failures reported back
│   ├── queue.rs          # PlaybackQueue, FastRng (SplitMix64 deterministic PRNG), shuffle & loop
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
    ├── test_playlists.rs     # M3U8 parse/write round trips, name rules, safe paths, queue planning
    ├── test_playlist_store.rs # Store create/add/remove/move/rename/delete, case-only rename, load skips
    ├── test_playlists_ui.rs  # Tab 5 tab switching, two-pane navigation, rendering, modal hook, help
    ├── test_playlists_edit.rs # Name prompt, create/rename/delete, K/J move, d/x remove
    ├── test_playlists_add.rs  # L key, add picker, folder adds, last_playlist
    ├── test_playlists_play.rs # Missing-file checks, Enter replace+play, a append, all-missing
    ├── test_search.rs        # Multi-threaded fuzzy search, scoped scan, and modal navigation
    ├── test_shared_tag_reads.rs # No concurrent duplicate tag reads; skipped/waiting requests still get tags
    ├── test_upnp.rs          # UPnP SOAP XML generation, AVTransport state machine, continuous stream
    ├── test_web.rs           # Embedded web server, byte ranges, REST API, and command dispatch
    └── test_web_playlists.rs # Web playlists: snapshot, GET /api/playlists, play/append, edits, path checks
```

The background writer's create, delete and rename tests (used by `PlaylistStore`), including the failure reports, are in the existing `tests/test_persistence_worker.rs`, which the map above does not list.

---

## 3. Terminal UI Architecture & Ratatui Guidelines

### 5-Tab Layout Structure
- **Header**: Tab bar (`[1] Now Playing` | `[2] Queue & Library` | `[3] Devices` | `[4] Settings` | `[5] Playlists`), 79 columns. Narrower terminals get short labels (`[1] Now` | `[2] Library` | `[3] Devices` | `[4] Settings` | `[5] Playlists`, 58 columns), then only the numbers plus the active tab's short label, so the active tab stays visible (`tab_titles` in `src/ui/mod.rs`).
- **Main View Area**:
  - **Tab 0 (`[1] Now Playing`)**: Hero view with large typography (Title, Artist, Album), technical codec/format badges, timeline, and upcoming queue preview.
  - **Tab 1 (`[2] Queue & Library`)**: Split-pane layout.
    - **Left Pane (`browser`)**: Bounded `FileBrowser` showing directory tree, bookmarked Music Homes (`~/.config/quickstream/bookmarks.json`), and metadata tag view toggle (`[v]`).
    - **Right Pane (`queue`)**: `PlaybackQueue` showing queued songs, duration, status badges, and track index. Focus toggled with `h` and `l`.
  - **Tab 2 (`[3] Devices`)**: Discovered DLNA MediaRenderers and `Local Audio` with active output indicator (`●`).
  - **Tab 3 (`[4] Settings`)**: User preferences editor (metadata display, audio backend, seek interval, volume step, queue mode, PCM transcode, web server toggle, startup home directory).
  - **Tab 4 (`[5] Playlists`)**: Split-pane layout (`src/ui/playlists.rs`, state in `AppUI::playlists: PlaylistsUi`). A startup worker loads the playlists. Until the load finishes, the tab shows `Loading playlists…` and edits are refused with `Playlists are still loading`. If the folder exists but can't be listed, the tab shows `Couldn't load playlists` and edits stay refused for the session, so no create can overwrite a file the store never read. Focus toggles with `h` and `l`. `j`/`k`/`Up`/`Down` move with wraparound. In either pane `g`/`G` (or `Home`/`End`) jump to the first/last row and `PageUp`/`PageDown` move by the rows the pane shows, stopping at the first/last row without wrapping.
    - **Left Pane (playlists)**: one row per playlist (name and track count), sorted case-insensitively. `c` create, `R` rename (prompt prefilled with the current name), `D` delete after `Delete "<name>" (<n> tracks)? y/n`, `Enter` focus the tracks pane, `a` append the whole playlist to the queue. Empty state: `No playlists yet. Press c to create one, or L on any track.`
    - **Right Pane (tracks)**: title `<NAME> (<n> tracks, <total length>)`, with unknown durations left out of the total. Rows read `<n>. <artist> - <title>  <m:ss>`. Missing files show `✗ missing: <artist> - <title>` and `----`. Existence is checked on a worker and cached per path for the session. Rows not yet checked render normally. `Enter` replaces the queue with the playlist and plays from the selected track: it stops any autopilot scan and continuous stream first (`AppUI::reset_playback_session`) and skips missing files. The worker checks the selected entry first, then the ones after it, then the ones before it; the first that exists (the track playback starts on) plays as soon as it is found, and the rest of the playlist is put around it when their checks finish (status `Queued <k> tracks from <name>, <m> missing`). Tracks queued meanwhile stay after the playlist; if the queue is replaced or cleared meanwhile, the rest is dropped. If every file is missing, it leaves the queue untouched and the status reads `None of <name>'s <n> tracks found. Is /Volumes/Music mounted?`, naming the top two folders above the first entry's file (no question when the file is fewer than two folders deep, e.g. `/Volumes/ghost.mp3`). `a` appends the whole playlist. `K`/`J` move the selected track up/down (no wraparound; on tab `[2]` these keys page instead). `d`/`x`/`Del` remove it. Empty state: `Empty. Press L on a track in tab 2 to add it.`
    - Edits save automatically through the background writer (`PersistHandle`). There is no save key.
- **Web Dashboard Playlists (issue #50)**: the dashboard's `Playlists` tab uses the same store and code as tab 5.
  - Read: `GET /api/playlists` serves the `WebPlaylists` snapshot the UI thread publishes in `NowPlayingState::playlists` (an `Arc`, `#[serde(skip)]` from `/api/status`) whenever `PlaylistStore::revision()`, the missing-file cache, the load state or `last_playlist` changes (`AppUI::publish_web_playlists`, called from `update_web_state`). The playlists folder is never read per request. `/api/status` carries `playlists_version` so the page knows when to fetch again. The snapshot has `state` (`loading`/`failed`/`ready`), the TUI's `message` when not ready, `revision`, `last_playlist` and each playlist's entries (title, artist, duration, path, `missing`: `true`/`false`/`null` until checked). Responses clear the path and set `outside` for entries outside the allowed web folders, as `/api/status` does. Serving it sends `WebCommand::CheckPlaylists`, which checks every playlist's unchecked files on one worker.
  - Play / append: `/api/action` `playlist_play` (`name`, optional `index`, `revision`) and `playlist_append` (`name`). The web thread checks every entry of the snapshot's playlist: one whose file resolves outside the allowed folders, or a missing one whose path lies outside them, refuses the whole request with 403 and `WebCommand::PathRejected`, like `add_file`. Otherwise it sends `WebCommand::QueuePlaylist` with the checked entries and the UI queues them with tab 5's worker code (`queue_playlist_entries`: same `plan_queue`, reset of autopilot scan and continuous stream on replace, same status messages, which name the playlist instead of saying `(web)`). For web requests the worker also requires each checked file to resolve inside the allowed folders. Refusals (JSON `{"ok":false,"error":…}`): not loaded (409, `Playlists are still loading` / `Couldn't load playlists`), unknown name (404), empty playlist (409), bad index (400), stale `revision` (409, `Playlists changed; try again`).
  - Edit: `playlist_create` (`name`), `playlist_rename` (`name`, `new_name`), `playlist_delete`, `playlist_add` (`name`, `path`: checked like `add_file`, 403 + `PathRejected` outside the allowed folders, 400 if not an audio file), `playlist_remove` (`name`, `index`, `revision`), `playlist_move` (`name`, `index`, `direction` `up`/`down`, `revision`). The web thread sends `WebCommand::EditPlaylist(PlaylistEdit, WebReply)` and waits up to 5 s for the UI's answer; `AppUI::apply_web_playlist_edit` applies it through `PlaylistStore` only (same `validate_name` rules, duplicate and reserved-name checks and messages as the name prompt), shows it in the status line, republishes the snapshot and answers. 200 `{"ok":true,"message":…}`, 409 with the store's message, 503 if the UI didn't answer. Tab 5 keeps its playlist selected (following a web rename).
  - All of these share the `web_access` checks (pairing token, Host, Origin, JSON Content-Type). The dashboard builds every row from text and `dataset` only (see `tests/support/dashboard_dom_harness.js`).
- **Persistent Bottom Playback Bar**:
  - Rendered across all tabs.
  - Displays: State badge (`▶`, `⏸`, `⏹`), track title, artist, format badge (`[FLAC] [24b/96k]`), volume/mute, interactive progress bar, elapsed/total time, and active mix profile badge.
- **Overlay Modals**:
  - **Help Modal (`?`)**: Full keyboard shortcut cheat-sheet.
  - **Fuzzy Search Modal (`f`)**: Streaming fuzzy finder with real-time preview card.
  - **Mix Profile Modal (`M`)**: Interactive autopilot mix profile selector (`[1]` Diverse Shuffle, `[2]` BPM Flow, `[3]` Chill & Ambient, `[4]` Workout, `[5]` Late Night Groove).
  - **Add-to-Playlist Picker (`L`)**: the header names what is being added (the track title, or `<folder> (folder)`). The rows list the playlists, with `last_playlist` highlighted (or the first row if it no longer exists), then `+ New playlist…`. `j`/`k`/arrows wrap, `Enter` adds, `Esc`/`q` cancel. Results: `Added "<title>" to <name> (<n> tracks)`, `Already in <name>`, `Added <k> to <name>, skipped <m> already in it`. A folder add follows its playlist by id (`PlaylistStore::id_of`), so a rename made while its tags are read moves the add with it; if the playlist was deleted meanwhile it reports `<name> was deleted; nothing added`.
  - **Playlist Name Prompt** (`c`, `R`, `+ New playlist…`): every character is text, including `q` and digits. `Backspace` deletes, `Enter` confirms, `Esc` cancels. An invalid or duplicate name (ignoring case, with full Unicode case folding, and Unicode normalization, as APFS does) keeps the prompt open and shows the error, e.g. `Name can't contain /` or `"Road Trip" already exists`. A name wider than the field scrolls: the field shows `…` and the end of the name, so the cursor stays visible. A name taken by a file the store skipped at load (or found in the way of a create) shows `A playlist file named "Latin" exists but couldn't be loaded` (`NameError::Reserved`).
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
