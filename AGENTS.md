# Quickstream Agent Developer Guide (`AGENTS.md`)

This guide is intended for AI coding agents and automated development tools working on the **Quickstream** codebase. It documents architectural decisions, core invariants, technology constraints, and operational patterns.

---

## 1. Project Philosophy & Core Invariants

1. **Zero External Python Dependencies**:
   - The entire codebase **MUST** run on standard Python 3.10+ without any third-party pip packages (no `requests`, `numpy`, `click`, `rich`, `textual`, etc.).
   - Standard library modules used: `curses`, `socket`, `http.server`, `urllib`, `xml.etree.ElementTree`, `subprocess`, `threading`, `dataclasses`, `pathlib`, `typing`, `array`.
   - **Do not add external Python dependencies** to `pyproject.toml` unless explicitly instructed by the user.

2. **System Dependencies**:
   - `ffmpeg`: Required for audio format transcoding, Braille waveform sample extraction, and fallback metadata extraction.
   - `ffplay`: Required for headless local audio playback (`Local Audio` device).

3. **Dual Playback Architecture**:
   - Quickstream abstracts playback through a unified client interface providing `play()`, `pause()`, `stop()`, `seek()`, `get_position_info()`, `get_transport_info()`, `set_volume()`, and `set_mute()`.
   - **Local Playback**: Implemented by `LocalPlayerClient` (`quickstream/local_player.py`) using `ffplay -nodisp -autoexit` via `subprocess.Popen`.
   - **DLNA / UPnP Streaming**: Implemented by `UPnPClient` (`quickstream/upnp.py`) sending SOAP XML payloads to network renderers, while `DLNAStreamServer` (`quickstream/server.py`) serves the audio file over HTTP with Byte-Range support (`206 Partial Content`).

---

## 2. Codebase Structure & Module Map

```
quickstream/
├── __init__.py           # Package version definition
├── __main__.py           # python -m quickstream entrypoint
├── cli.py                # Argument parsing, discovery scan CLI, server bootstrap, signal traps
├── tui.py                # Curses TUI, split-pane queue, persistent bottom bar, help modal
├── queue.py              # PlaybackQueue data structure (tracks, indices, auto-advance, reorder)
├── browser.py            # FileBrowser sub-pane, filesystem traversal, persistent bookmarks
├── discovery.py          # SSDP UDP multicast (239.255.255.250:1900), XML parsing, DeviceManager
├── upnp.py               # SOAP client for AVTransport and RenderingControl services
├── local_player.py       # ffplay subprocess wrapper implementing the UPnP client interface
├── server.py             # Custom HTTP byte-range server with DLNA profile headers
├── transcoder.py         # On-the-fly ffmpeg pipeline to 16-bit PCM WAV
├── metadata.py           # Native ID3v2, FLAC, RIFF WAV, AIFF metadata parser
├── waveform.py           # Braille-based 2D high-resolution audio waveform generator
└── settings.py           # User preferences and persistence (~/.quickstream_config.json)

tests/
├── test_discovery.py         # SSDP packet parsing and device extraction tests
├── test_local_player.py      # LocalPlayerClient subprocess and lifecycle tests
├── test_metadata.py          # Audio header parsing and duration calculation tests
├── test_playback_controls.py # TUI playback orchestration, hotkeys, seeking, and server tests
├── test_queue.py             # Queue operations, wrapping, and indexing tests
├── test_server.py            # HTTP byte ranges, 206 Partial Content, DLNA headers
├── test_settings.py          # Configuration loading, saving, cycling, and defaults
├── test_upnp.py              # SOAP XML generation and response parsing tests
└── test_waveform.py          # Waveform sample extraction and Braille plotting tests
```

---

## 3. TUI & Curses Guidelines

### Layout Structure
- **Header**: Tab bar (`[1] Queue` | `[2] Devices` | `[3] Settings`).
- **Main View Area (`0` to `max_y - 5`)**:
  - **Tab 0 (Queue)**: Split-pane layout.
    - **Left Pane (`browser`)**: Bounded `FileBrowser` showing directory tree, music homes/bookmarks (`~/.quickstream_bookmarks.json`), and metadata view toggle (`[v]`).
    - **Right Pane (`queue`)**: `PlaybackQueue` showing queued songs, duration, and track status.
    - Focus toggled between panes with `h` and `l`.
  - **Tab 1 (Devices)**: Discovered DLNA MediaRenderers and `Local Audio`.
  - **Tab 2 (Settings)**: Configurable application settings (metadata view, default output, seek interval, volume step, queue mode, transcode).
- **Persistent Bottom Playback Bar (`max_y - 5` to `max_y`)**:
  - Always rendered across all tabs.
  - Displays: State badge (`▶`, `⏸`, `⏹`), track title, artist, format, volume/mute, Braille/ASCII progress bar, and elapsed/total time.
- **Help Modal (`?`)**:
  - Centered popup modal overlaying the main view.
  - Intercepts keyboard input when active; dismissed with `Esc`, `q`, or `?`.

### Curses Invariants
1. **Always Use `_safe_addstr()`**:
   Terminal sizes vary. Direct `stdscr.addstr()` raises `curses.error` when writing to the bottom-right character or beyond screen boundaries. Always route writes through `self._safe_addstr(stdscr, y, x, text, attr)` which enforces bounds and catches `curses.error`.
2. **List Navigation Wraparound**:
   All lists (Queue, FileBrowser, Devices) wrap around: navigating `Up` at index `0` moves to the last item, and navigating `Down` at the end loops back to `0`.
3. **Keybinding Precedence**:
   - `KEY_RESIZE` handled first.
   - `?` Help Modal toggle handled next.
   - If Help Modal is open: only handle `Esc`, `q`, `?`.
   - Global hotkeys (`Space`, `s`, `R`, `m`, `t`, `+`, `-`, arrows, Shift+arrows) work across all tabs.
   - Active pane keys (browser vs queue vs devices) handle specific item manipulation.
4. **Key Mappings**:
   - `curses.KEY_LEFT` / `curses.KEY_RIGHT`: Global Previous / Next track.
   - `curses.KEY_SLEFT` / `curses.KEY_SRIGHT`: Global Seek backward / forward 10s.

---

## 4. Development & Testing Workflow

### Running Tests
Execute the full test suite using Python's standard `unittest`:
```bash
python3 -m unittest discover tests
```

### Important Rules for Future Agents
- **No Temporary Root Scripts**: Do not create ad-hoc test scripts (e.g. `test_something.py`, `scratch.py`, `patch.py`) in the repository root. Always place unit tests inside the `tests/` directory following the `test_*.py` naming convention.
- **Graceful Resource Cleanup**: All networking threads (`DLNAScanner`), streaming sockets, and subprocesses must terminate cleanly. When editing `server.py`, `local_player.py`, or `tui.py`, verify that `cleanup()` and signal handlers close active client sockets to prevent hanging processes or `BrokenPipeError` noise.
- **File Modifications**: Use targeted file editing tools (`replace_file_content` or `write_to_file`) rather than shell sed/cat scripts.
