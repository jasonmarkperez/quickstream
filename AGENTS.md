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
     - Configuration: `~/.config/quickstream/config.json`
     - Bookmarks: `~/.config/quickstream/bookmarks.json`
     - Track Analysis Database: `~/.cache/quickstream/track_analysis.db` (SQLite)
     - Cache Indexes: `~/.cache/quickstream/music_index.jsonl`
     - Logs: `~/.local/share/quickstream/quickstream.log`

4. **Non-Blocking UI Invariant**:
   - Disk I/O, network SSDP discovery, audio decoding/analysis, metadata extraction, and library scanning **MUST** run asynchronously on background worker threads, worker pools, or Tokio tasks.
   - Communication with the main UI loop is performed using `std::sync::mpsc` channels and lock-free/atomic states. The Ratatui UI event loop must never block or drop below 60 FPS.

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
│   ├── queue.rs          # PlaybackQueue, FastRng (SplitMix64 deterministic PRNG), shuffle & loop
│   ├── web.rs            # Embedded HTTP byte-range server, continuous WAV streaming, REST API, Web UI
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
    ├── test_search.rs        # Multi-threaded fuzzy search, scoped scan, and modal navigation
    ├── test_upnp.rs          # UPnP SOAP XML generation, AVTransport state machine, continuous stream
    └── test_web.rs           # Embedded web server, byte ranges, REST API, and command dispatch
```

---

## 3. Terminal UI Architecture & Ratatui Guidelines

### 4-Tab Layout Structure
- **Header**: Tab bar (`[1] Now Playing` | `[2] Queue & Library` | `[3] Devices` | `[4] Settings`).
- **Main View Area**:
  - **Tab 0 (`[1] Now Playing`)**: Hero view with large typography (Title, Artist, Album), technical codec/format badges, timeline, and upcoming queue preview.
  - **Tab 1 (`[2] Queue & Library`)**: Split-pane layout.
    - **Left Pane (`browser`)**: Bounded `FileBrowser` showing directory tree, bookmarked Music Homes (`~/.config/quickstream/bookmarks.json`), and metadata tag view toggle (`[v]`).
    - **Right Pane (`queue`)**: `PlaybackQueue` showing queued songs, duration, status badges, and track index. Focus toggled with `h` and `l`.
  - **Tab 2 (`[3] Devices`)**: Discovered DLNA MediaRenderers and `Local Audio` with active output indicator (`●`).
  - **Tab 3 (`[4] Settings`)**: User preferences editor (metadata display, audio backend, seek interval, volume step, queue mode, PCM transcode, web server toggle, startup home directory).
- **Persistent Bottom Playback Bar**:
  - Rendered across all tabs.
  - Displays: State badge (`▶`, `⏸`, `⏹`), track title, artist, format badge (`[FLAC] [24b/96k]`), volume/mute, interactive progress bar, elapsed/total time, and active mix profile badge.
- **Overlay Modals**:
  - **Help Modal (`?`)**: Full keyboard shortcut cheat-sheet.
  - **Fuzzy Search Modal (`f`)**: Streaming fuzzy finder with real-time preview card.
  - **Mix Profile Modal (`M`)**: Interactive autopilot mix profile selector (`[1]` Diverse Shuffle, `[2]` BPM Flow, `[3]` Chill & Ambient, `[4]` Workout, `[5]` Late Night Groove).
  - Modals intercept keyboard input when active; dismissed with `Esc` or `q`.

### Key Navigation Invariants
1. **List Wraparound**:
   All interactive lists (Queue, FileBrowser, Devices, Search Results, Mix Profiles) wrap around: navigating `Up` at index `0` moves to the last item, and navigating `Down` at the end loops back to `0`.
2. **Keybinding Precedence**:
   - Terminal resize events handled first.
   - Active overlay modal input handled next (dismissal on `Esc`/`q`).
   - Global playback hotkeys (`Space`, `s`, `R`, `m`, `t`, `+`, `-`, arrows, Shift+arrows) work across all tabs.
   - Tab-specific and pane-specific keys handle item manipulation.
3. **Key Mappings**:
   - `Left` / `Right`: Global Previous / Next track.
   - `Shift+Left` / `Shift+Right`: Global Seek backward / forward by configured interval (default 10s).
   - `A`: Start / Restart Autopilot mode on current directory.
   - `M`: Open Mix Profile modal.
   - `P`: Reshuffle Autopilot seed from next track forward.
   - `r`: Start Seed Radio station from selected track.

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
