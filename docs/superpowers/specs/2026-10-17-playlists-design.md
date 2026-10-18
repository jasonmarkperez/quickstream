# Playlists design (issue #37)

Date: 2026-10-17. Status: approved in conversation, pending written-spec review.

## Goal

Let the user hand-curate playlists in the terminal UI: build a list track by track, separate from the queue, then play it or add it to the queue. Playlists are stored as standard extended M3U8 files so they can be opened by other players.

### Answers to the questions in issue #37

| Question | Answer |
|---|---|
| Where are they stored? | `~/.config/quickstream/playlists/<name>.m3u8`, one file per playlist. Never in the music library. |
| Are they exportable? | Yes. The stored file is the export; copy it anywhere. |
| Standard format? | Extended M3U8 (UTF-8), read by VLC, mpv, foobar2000, MPD, Navidrome, Jellyfin, Plex and Sonos. |
| How do they reference files? | Absolute paths (`/Volumes/Music/...`), the same as bookmarks, `music_index.jsonl` and `track_features`. |

### Decisions made with the user

1. Purpose: hand-curated lists, separate from the queue.
2. Tracks are added from the library browser, the now-playing track and the queue pane. Not from fuzzy search.
3. Adding opens a picker with the last-used playlist highlighted and a `+ New playlist…` row.
4. Viewing and editing happen in a new tab `[5] Playlists`.
5. Storage: M3U8 files with absolute paths.
6. TUI only in v1. Web dashboard support is a follow-up issue (#50, now done: see Section 5).
7. Edits save automatically; there is no save key.
8. Adding a folder adds the audio files directly inside it, in name order, not recursive.
9. Adding a track that is already in the playlist is skipped with a status message.
10. Missing files are shown as `✗ missing` and skipped when queueing. The playlist file is never pruned because of missing files.
11. Playing a track from tab 5 replaces the queue with the playlist and plays from that track, after stopping any autopilot scan.
12. Architecture: an in-memory store loaded at startup, with each edit written through to disk by the background writer (approach A). Rejected: re-reading the folder on demand (B) and storing playlists in the SQLite analysis DB (C, which lives in `~/.cache` and is not a standard format).

### Out of scope for v1

- Web dashboard playlists (list, play, edit). Filed as #50 and since added (Section 5).
- Opening or importing `.m3u`/`.pls`/`.xspf` files that live on the share.
- XSPF or PLS output.
- Rewriting path prefixes when the share mounts at a different path (for example `/Volumes/Music-1`) or for other servers. This affects bookmarks and caches too and should be solved once for all of them.
- Adding from the fuzzy search modal.
- Watching the playlists folder for edits made outside the app.

## Section 1: Architecture and data model

### `src/playlist.rs` (new, pure: no disk I/O, no UI)

```rust
pub struct PlaylistEntry {
    pub path: PathBuf,
    pub duration_secs: i64, // -1 when unknown
    pub artist: String,
    pub title: String,
}

pub struct Playlist {
    pub name: String,
    pub entries: Vec<PlaylistEntry>,
}
```

- `Playlist::to_m3u8(&self) -> String` writes UTF-8 with LF line endings and no BOM:
  ```
  #EXTM3U
  #PLAYLIST:Road Trip
  #EXTINF:284,Radiohead - Airbag
  #EXTART:Radiohead
  /Volumes/Music/Radiohead/OK Computer/01 Airbag.flac
  ```
  When the artist is empty the `#EXTINF` text is the title alone and `#EXTART:` is empty. Every entry gets an `#EXTART` line so the label round-trips even when the artist or title contains ` - ` (issue #52: an untagged `01 - Intro` used to reload as artist `01`). Artist and title are written trimmed and with control characters replaced by spaces. The path line is written unchanged, spaces included.
- `parse_m3u8(name: &str, text: &str, base_dir: &Path) -> Playlist` accepts files edited by hand or copied in from elsewhere:
  - strips a leading UTF-8 BOM and accepts CRLF;
  - ignores blank lines and `#` lines other than `#EXTINF` and `#EXTART` (including `#EXTM3U` and `#PLAYLIST`);
  - strips only the line ending and leading whitespace from a path line, so a file name ending in spaces round-trips (issue #52);
  - takes duration, artist and title from the `#EXTINF` and `#EXTART` lines before an entry (in either order, tags matched case-insensitively). With an empty `#EXTART` the whole `#EXTINF` text is the title. With a non-empty `#EXTART` whose value starts the text as `<artist> - `, the artist is the `#EXTART` value and the title the rest. Otherwise (no `#EXTART`, as in files from other players, or an `#EXTART` that other players use for the album artist and that does not start the text) the text splits at the first ` - ` as `<artist> - <title>`, and text without ` - ` is the title;
  - resolves relative paths against `base_dir` (the playlist file's folder), then normalizes every path with `normalize_path` (issue #58);
  - an entry without `#EXTINF` gets duration `-1`, empty artist and the file stem as title.
  The playlist's name is the file stem, not `#PLAYLIST`. The path line, not the `#EXTINF` text, is the source of truth for the file, so a label that does not round-trip only affects display.
- `normalize_path(path: &Path) -> PathBuf` (issue #58) normalizes a path lexically: drops `.` components and lets each `..` cancel the component before it. It never reads the disk and never follows symlinks (`link/..` is the folder holding `link`). A `..` right after the root is dropped, as the OS does (`/../x` → `/x`); on a relative path a leading `..` with nothing to cancel is kept. A hand-written `../../music/x.mp3` entry is therefore loaded as an absolute path without `..`, and the first edit never writes `<playlists>/../..` back.
- `validate_name(name: &str) -> Result<String, NameError>` trims whitespace, then rejects: empty, longer than 100 characters, containing `/`, `\`, NUL or other control characters, or starting with `.`. Returns the trimmed name.
- Name comparison matches the default macOS file system (APFS), which treats these names as one file: it ignores case using full Unicode case folding (`Straße` = `Strasse`) and ignores Unicode normalization (NFC `Zoë` = NFD `Zoë`). The `names_equal` helper compares `name_key`, the canonical caseless key NFD(casefold(NFD(name))) (issue #55).

### Files

- `paths::get_playlists_dir()` returns `get_config_dir().join("playlists")`, so it honours `XDG_CONFIG_HOME` and tests stay isolated through `tests/common::isolate()`. The folder is created on first write.
- The file for playlist `name` is `<playlists dir>/<name>.m3u8`.
- `playlist_file_path(dir, name)` builds the path and checks that its parent equals the playlists dir. Every write and delete goes through it, so no playlist operation can touch the library, `config.json`, `bookmarks.json` or `web_token`.

### `PlaylistStore` (new, held by `AppUI`)

- Holds `Vec<Playlist>`, sorted by name (case-insensitive).
- At startup a worker (`self.workers.spawn`) reads every `*.m3u8` in the playlists folder and sends the parsed playlists to the UI over an mpsc channel. Until the result arrives the tab shows `Loading playlists…`. A file that cannot be read or is not valid UTF-8 is logged and skipped. It is never rewritten or renamed, and its name is reserved: creating or renaming a playlist onto it is refused with `A playlist file named "Latin" exists but couldn't be loaded` (`NameError::Reserved`, issue #59), since no playlist of that name is listed. If the folder exists but cannot be listed (permissions, an entry that cannot be read, or a symlink whose target is missing), the load fails: the store stays not loaded, the tab shows `Couldn't load playlists`, and every edit is refused for the session, so no create can overwrite a file the store never read (issue #56).
- Operations, each changing the in-memory copy first and then handing the full file contents to the background writer (`PersistHandle::save`, atomic temp-file-then-rename):
  - `create(name)` – fails on an invalid name or a case-insensitive duplicate. The file is written with `PersistHandle::create` (`create_new_file`: temp file, fsync, then a hard link to the final name, which fails instead of replacing; a disk without hard links falls back to `OpenOptions::create_new`). The name checks can't model every file system (HFS+, SMB or exFAT name matching, another running instance), so a file the store never saw may already be there: the writer leaves it untouched and reports the collision, and the store removes the playlist, reserves the name and the status reads `Couldn't create "Mix": a file with that name already exists` (issue #62). A cursor still on the removed playlist goes back to the playlist selected before the create, not the row it had sorted into. While another playlist's create or rename that the writer hasn't settled starts from or goes to the name, the create is refused with `Still creating "Mix", try again` or `Still renaming "Old" to "New", try again` (see the pending rule below);
  - `add(name, entries) -> AddResult { added, skipped_duplicates }` – a duplicate is an entry whose path is already in the playlist. Each path is first made absolute (`std::path::absolute`) and normalized with `normalize_path`, so the same file reached through `..` (`quickstream ./music/../music`) is a duplicate and no `..` is stored (issue #58);
  - `remove(name, index)`;
  - `move_entry(name, index, up|down)` – no wraparound for moves;
  - `rename(old, new)` – validates the new name. The duplicate check ignores the playlist being renamed, so a case-only rename (`road trip` → `Road Trip`) is allowed. The file is moved with the operating system's rename (`fs::rename(old_path, new_path)`), then the new file is saved again so its `#PLAYLIST` line shows the new name. Rename never uses "write new file, then delete old file": on a case-insensitive disk (the macOS default) `road trip.m3u8` and `Road Trip.m3u8` are the same file, so deleting the old name would delete the playlist just written. The rename never replaces a different file at the new name (a file that is the old one itself, a case-only rename, is fine). If it fails for any reason other than the old file being gone, the writer reports it, the store gives the playlist its old name back, re-sorts and saves it to the old file, and the status reads `Couldn't rename "Old" to "New": <reason>` (issue #52). A new name that another playlist's unsettled create or rename starts from or goes to is refused like in `create`;
  - `delete(name)` – deletes the file.
- `src/persist.rs`: the background writer gains `PersistHandle::delete(path)` and `PersistHandle::rename(from, to)`. Saves, deletes and renames are applied in the order they were sent, and a pending save for a path is written before a later delete or rename of that path, so "save then delete", "delete then save" and "save, rename, save" all end in the expected state. It also gains `PersistHandle::create(path, bytes, reporter)` and `rename_reporting(from, to, reporter)`: a failed create or rename is sent as a `PersistFailure` carrying the reporter's tag (the store's id for the playlist, which survives renames) on an mpsc channel the UI polls each tick. Until the store has undone the change, the writer remembers the path: saves to it are dropped (in later batches too, and after shutdown, when operations run on the calling thread), a rename from it moves nothing (except a rename straight back onto the file that really holds the content, which lands there, so saves of that name reach the file again), a delete of it removes the real file after a failed rename and nothing after a failed create. A rename that moves a file updates the remembered paths that pointed at it.
- Pending creates and renames (PR #65 review). Every operation queued with a reporter answers exactly once on the channel: `PersistReport::Failed(PersistFailure)` or `PersistReport::Done { tag }` (it worked, its source was already gone, or its source was a path whose own failure is already reported). The store tags each create and rename with its own operation number and keeps it pending until it is acknowledged or its failure has been undone. While it is pending, creating a different playlist under a name it starts from or goes to, or renaming a different playlist onto one (compared like `names_equal`), is refused with `Still renaming "Old" to "New", try again` or `Still creating "Mix", try again`. Without this rule, a rename `A → B` that fails, followed before the failure is heard by renaming `C` onto `A` or creating `A`, leaves two playlists claiming `A.m3u8`: the undo used to drop one of them from memory, drop the restored playlist's saves and make deleting `A` remove `C.m3u8`. The pending playlist's own later renames are not refused (the writer follows the chain, and a rename straight back lands on its real file), and neither are names of a playlist deleted since (its delete removed the file left at the old name, or left a file that was never ours alone). The web dashboard's create and rename go through the same store calls, so two quick `/api/action` requests get the same refusal.
- `Settings` gains `last_playlist: Option<String>` (serde default `None`) in `config.json`. It is set after every successful add and preselected by the picker. If it names a playlist that no longer exists, the picker highlights the first row.

## Section 2: Terminal UI

### Tab `[5] Playlists`

- Key `5` opens it. Tab/BackTab cycling changes from `% 4` to `% 5` (`src/ui/mod.rs` around line 2075) and the tab bar gets a fifth label.
- Split pane like tab 2: playlists on the left, tracks of the selected playlist on the right. `h`/`l` switch focus. All lists wrap around (AGENTS.md section 3 invariant).
- In either pane `g`/`G` (or `Home`/`End`) jump to the first/last row, and `PageUp`/`PageDown` move by the number of rows the pane showed at the last render, stopping at the first/last row without wrapping (issue #52). None of these keys is a global hotkey.
- Left pane: each row shows the name and track count.
  - `c` create (opens the name prompt)
  - `R` rename (name prompt prefilled with the current name)
  - `D` delete, after a `Delete "Road Trip" (24 tracks)? y/n` prompt; any key other than `y` cancels
  - `Enter` move focus to the tracks pane
  - `a` append the whole playlist to the queue
- Right pane: rows show `<n>. <artist> - <title>  <m:ss>`; missing files show `✗ missing: <artist> - <title>` and `----`.
  - `Enter` replace the queue with the playlist and play from the selected track
  - `a` append the whole playlist to the queue
  - `K`/`J` move the selected track up/down
  - `d`/`x`/`Del` remove the selected track from the playlist
- The tracks pane title is `<NAME> (<n> tracks, <total length>)`. Entries with unknown duration are left out of the total.
- Empty states: no playlists → `No playlists yet. Press c to create one, or L on any track.`; empty playlist → `Empty. Press L on a track in tab 2 to add it.`

### Add key `L`

- Matched as `(KeyCode::Char('L'), _)`, like the existing `H`, `M` and `P` arms.
- On tab 2 it adds the selection in the focused pane:
  - browser, track row: that track;
  - browser, folder row: the audio files directly inside the folder, sorted by name, not recursive;
  - browser, `★ Music Homes` or `..` row: status `Select a track or folder`;
  - queue pane: the selected queue track.
- On every other tab it adds the now-playing track. With nothing playing: status `Nothing is playing`.
- It opens the **add picker** modal: header shows what is being added (track title, or `<folder> (folder)`); rows are the playlists, the `last_playlist` highlighted, then `+ New playlist…`. `j`/`k`/arrows move with wraparound, `Enter` adds, `Esc`/`q` cancels. `+ New playlist…` opens the name prompt; on confirm it creates the playlist and adds. A folder is listed after the playlist is created (so a name error still shows in the open prompt); if it then adds nothing the status says so and names the new playlist: `Created "Road Trip"; no audio files in <folder>` or `Created "Road Trip"; couldn't read <folder>` (issue #59).
- Result messages: `Added "Kerala" to Road Trip (25 tracks)`, `Already in Road Trip`, `Added 9 to Road Trip, skipped 3 already in it`.

### Name prompt modal

- Same pattern as `editing_home_dir`: captured before the global hotkeys, every `Char` is text (including `q` and digits), `Backspace` deletes, `Enter` confirms, `Esc` cancels.
- A name wider than the field (about 55 columns in a full-width prompt) scrolls horizontally: the field shows `…` followed by as much of the end of the name as fits, measured in display columns, then the cursor (issue #52).
- On an invalid or duplicate name the prompt stays open and shows the error, for example `Name can't contain /` or `"Road Trip" already exists`. A name reserved by a file the store skipped shows `A playlist file named "Latin" exists but couldn't be loaded`.

### Docs

- The `?` help modal lists the tab 5 keys and `L`.
- AGENTS.md section 3 is updated: the 5-tab layout, tab 5 keys, `L`, and the playlists files in section 1.3. The existing entries listing `A`, `R`, `m` and `t` as global keys are corrected, because none of them is bound in the code.

## Section 3: Data flow and error handling

### Adding

- A track's entry is built from metadata the app already holds, in this order: the queue row or now-playing `AudioMetadata`, then `MetadataCache`. On a miss the title is the file stem, artist empty, duration `-1`. Nothing reads the share on the UI thread.
- A folder add runs on a worker: read the folder listing, keep files where `is_supported_audio_file` is true, sort by name, send the paths back over a channel. The UI then builds entries as above and calls `PlaylistStore::add`. If the folder can't be read: status `Couldn't read <folder>`. If it holds no audio files: `No audio files in <folder>`.
- The folder add worker reads tags for files with no real cache entry through the shared tag reads (`src/ui/tag_reads.rs`): a file the browser or queue metadata workers are reading right now is not read again; the folder add waits for that read and uses its tags from the cache (issue #52).
- The folder add names its target by the store's stable id (`PlaylistStore::id_of` / `name_of`), not by name. A rename while the tags are read (including a rename the writer undid) moves the add with the playlist. If the playlist was deleted meanwhile, nothing is added and the status reads `<name> was deleted; nothing added` (the name it had when the add started); a new playlist that took the old name gets nothing.

### Missing-file check

- When a playlist is shown in the tracks pane, a worker checks `Path::exists()` for each entry not yet checked this session and sends the results back. Results are cached per path for the session, so returning to a playlist does not re-check.
- Rows whose result is "missing" show `✗ missing`. Rows not yet checked show normally.
- Missing-file results never change the playlist or its file.

### Playing and queueing

1. The UI sends the playlist's entries (and, for `Enter`, the selected index) to a worker.
2. The worker checks which files exist and builds placeholder `AudioMetadata` from the entry data plus a `MetadataCache` hit, without the `stat` that `fallback_metadata` does. It sends the result to the UI over the existing `queue_add_tx` channel with a source tag.
3. `poll_pending_queue_adds` calls `enqueue_tracks`, which registers each file for DLNA streaming (`register_stream_file`) and starts the background tag fetch. Its status message uses the source tag (`Queued 22 tracks from Road Trip, 2 missing`) instead of the hard-coded `(web)`.
4. **Replace** (`Enter`) does not wait for every check (issue #52). The worker checks the selected entry first, then the entries after it, then the ones before it. The first entry in that order that can be queued is the track playback starts on (the rule below), so the worker sends it as soon as it is found, and the UI resets the session as described here, queues that one track and plays it, with the status `Playing from Road Trip; checking the rest…`. When every check has finished, the tracks before it go to the top of the queue and the tracks after it right behind it, so anything queued meanwhile follows the playlist, and the status becomes the usual summary (keeping a playback error from the start). If the queue was replaced or cleared in between, the rest is dropped. The reset (`AppUI::reset_playback_session`, whose parts autopilot shares) stops any autopilot scan (`autopilot_scan_stop`, `autopilot_rx = None`, `is_autopilot_scanning = false`), closes any continuous DLNA stream, stops playback and clears the queue. Once the rest is in, `current_index` and `queue_cursor` stay on the started track, which is the selected track's position among the tracks that exist. If the selected track itself is missing, playback starts on the next existing track after it, or the first existing track if none follows.
5. If every entry is missing: nothing is queued, the queue is left as it was, and the status is `None of Road Trip's 24 tracks found. Is /Volumes/Music mounted?` (the folder shown is the top two folders above the first entry's file). When that file is fewer than two folders deep (`/ghost.mp3`, `/Volumes/ghost.mp3`) there is no share to name and the question is left out: `None of Road Trip's 1 track found.` (issue #59).

### Other cases

- An add or edit while the store is still loading is refused with `Playlists are still loading`.
- A file edited by hand while the app runs is not reloaded. The next in-app edit of that playlist overwrites it. Restarting the app picks up hand edits.
- Two running app instances: the last write wins, as with bookmarks.
- A save or delete error from the background writer is logged. The in-memory copy stays as edited. A failed create or rename is reported back and undone in memory (see Section 1).

## Section 4: Testing

Tests are written before the code they cover. New file `tests/test_playlists.rs`; every test starts with `common::isolate()`.

1. **M3U8 format**
   - write then parse returns the same playlist;
   - parse handles a BOM, CRLF, unknown `#` lines, blank lines, relative paths, `..` and `.` entries (normalized, never written back), `#EXTINF:-1`, entries without `#EXTINF`, `#EXTART` (empty, before `#EXTINF`, album-artist style), labels containing ` - ` and paths with leading or trailing spaces;
   - an artist containing ` - ` keeps the path correct.
2. **Names**: each rejection rule; trimming; case-insensitive duplicates on create and rename; case-only rename allowed.
3. **Store**
   - create, add, skip duplicates, remove, move, rename (new file exists, old file gone), delete;
   - case-only rename (`road trip` → `Road Trip`): afterwards the playlists folder holds exactly one file, its name is `Road Trip.m3u8` (exact case, checked through `read_dir`), and it contains every track;
   - writes and deletes outside the playlists folder are refused;
   - an unreadable or non-UTF-8 file is skipped at load and its bytes are unchanged afterwards.
4. **Background writer** (`tests/test_persist.rs` or the existing persist tests): `delete` removes the file; `rename` moves it; save-then-delete, delete-then-save and save-rename-save on one path end in the expected state.
5. **UI** (following the existing `AppUI` tests)
   - tab cycling covers 5 tabs and `5` opens the tab;
   - `L` on a browser track → picker → `Enter` adds it; `last_playlist` is preselected next time;
   - the name prompt accepts `q` as text and shows an error for a duplicate;
   - replace stops a running autopilot scan and starts at the right index when some entries are missing;
   - all-missing leaves the queue unchanged;
   - the help modal lists `L` and the tab 5 keys.

Before the PR: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test -- --test-threads=1`.

## Section 5: Web dashboard (issue #50)

The dashboard gets a `Playlists` tab that lists, plays and edits playlists with the same store and code as tab 5.

### Reading

- The UI thread publishes a `WebPlaylists` snapshot in the shared web state (`NowPlayingState::playlists`, an `Arc` left out of `/api/status`). It is rebuilt only when something it shows changed: `PlaylistStore::revision()` (bumped by every load, create, add, remove, move, rename, delete and undone write), the missing-file cache, the load state, or `last_playlist`. `/api/status` carries `playlists_version`, so the page fetches again only after a change.
- `GET /api/playlists` serves the snapshot. The playlists folder is never read per request. Entries outside the allowed web folders lose their path and are flagged `outside`, as `/api/status` does for queue entries.
- Serving it asks the UI (`WebCommand::CheckPlaylists`) to check every playlist's unchecked files on one worker, using the session cache tab 5 uses. Results show as `missing: true/false`; unchecked entries are `null`. A check never changes a playlist.

### Playing and appending

- `playlist_play` (`name`, optional `index`, `revision`) and `playlist_append` (`name`) go through `/api/action`.
- The web thread checks every entry of the snapshot's playlist against the allowed folders, even though the playlist file itself is trusted: an entry whose file resolves outside them (including a symlink leading out), or a missing entry whose path lies outside them, refuses the whole request with 403 and `WebCommand::PathRejected`, like `add_file`. Missing entries inside the folders pass and are skipped by the queue as in tab 5. Refusing the whole request (rather than dropping the outside entries) keeps the rule identical to `add_file`; the dashboard marks such entries so the reason is visible.
- The UI gets `WebCommand::QueuePlaylist` with the checked entries (not the store's current list, so it never queues an entry the web thread didn't check) and runs tab 5's code: the worker checks, `plan_queue`, the session reset on replace, and the same status messages. For web requests the worker also requires each checked file to resolve inside the allowed folders, so a symlink changed after the web thread's check can't reach another file.
- The status names the playlist (`Queued 22 tracks from Road Trip, 2 missing`) rather than saying `(web)`: the message already says where the tracks came from, and it is the same text tab 5 shows.

### Editing

- `playlist_create`, `playlist_rename` (`new_name`), `playlist_delete`, `playlist_add` (`path`), `playlist_remove` (`index`) and `playlist_move` (`index`, `direction`). The web thread sends `WebCommand::EditPlaylist(edit, reply)` and waits (up to 5 seconds) for the UI's answer, so name errors reach the browser with the TUI's exact text.
- The UI applies edits through `PlaylistStore` only, so every write stays behind `playlist_file_path` and the ordered writer. Names go through `validate_name` and the store's duplicate (case-insensitive, normalization-insensitive) and reserved-name checks. Edits are refused while the store is loading (`Playlists are still loading`) or after a failed load (`Couldn't load playlists`).
- A file to add is checked like `add_file` (403 and `PathRejected` outside the allowed folders; 400 if it isn't an audio file). Its entry comes from the queue row for that file, else the metadata cache, else the file name; the file is never read.
- Index-based requests (play from a track, remove, move) carry the snapshot's `revision`; if the playlists changed since, they are refused with `Playlists changed; try again` and the page reloads.
- Tab 5 stays on the playlist it showed, following a web rename.

## Files touched

- New: `src/playlist.rs`, `tests/test_playlists.rs`.
- Changed: `src/lib.rs` (module export), `src/paths.rs` (`get_playlists_dir`), `src/persist.rs` (`delete`, `rename`), `src/settings.rs` (`last_playlist`), `src/ui/mod.rs` (tab 5, modals, `L`, workers, status source tag), `AGENTS.md`.
- If `src/ui/mod.rs` grows by more than about 600 lines, put the tab 5 rendering and key handling in `src/ui/playlists.rs` (related to issue #26).
