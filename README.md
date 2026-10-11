# Quickstream

A lightweight, terminal-based audio player and DLNA / UPnP network streamer built entirely in Python.

Stream **MP3**, **FLAC**, **WAV**, and **AIFF** files directly to your network stereo, AVR, or smart speaker without clunky desktop apps, or play them locally through your machine.

---

## Features

- **Zero External Python Dependencies**: Built 100% on standard library Python 3.10+ (requires system `ffmpeg`/`ffplay`).
- **All-in-One Interactive Terminal Interface**:
  - **Tab 1: Queue & Library**: Split-pane interface featuring an in-place **File Browser** on the left with persistent directory bookmarks ("Music Homes") and metadata tag toggle (`[v]`), and the **Playback Queue** on the right.
  - **Tab 2: Network Devices**: Discovers DLNA/UPnP stereos on your local network alongside standard **Local Audio**.
  - **Tab 3: Settings & Preferences**: Configure file display mode, default audio device, seek interval, volume step size, queue loop mode, and transcoding.
  - **Persistent Bottom Playback Bar**: Always-visible playback status, track metadata, volume, and progress bar across all tabs without obscuring the active view.
  - **Interactive Help Modal**: Press `[?]` anytime to bring up an overlay cheat-sheet with full keyboard shortcuts.
- **Dual Playback Engines**:
  - **Local Playback**: Seamless headless local playback via `ffplay`.
  - **DLNA / UPnP Streaming**: Built-in HTTP server supporting `206 Partial Content` and DLNA headers, paired with UPnP AVTransport / RenderingControl SOAP remote controls.
- **On-the-Fly Transcoding**: Press `[t]` to instantly transcode audio into standard 16-bit PCM WAV via `ffmpeg` for network stereos that do not natively support high-res or exotic formats.
- **High-Resolution Waveform Oscilloscope**: Braille 2D matrix visualizer rendering audio waveforms with multi-level zoom.

---

## Requirements

- **Python**: 3.10 or higher.
- **FFmpeg & ffplay**:
  - macOS: `brew install ffmpeg`
  - Linux: `sudo apt install ffmpeg`

---

## Installation & Running

### Install locally
```bash
git clone https://github.com/quickstream/quickstream.git
cd quickstream
pip install -e .
```

### Launch Quickstream
```bash
# Launch interactive TUI
quickstream

# Or run directly without installation
./bin/quickstream
```

### Command-line Options
```bash
# Pre-load files or folders into the queue
quickstream path/to/music/
quickstream track1.flac track2.mp3

# Quick CLI scan for DLNA renderers on your network
quickstream --scan

# Inspect audio file metadata and exit
quickstream song.flac --info
```

---

## Keyboard Controls

Press `[?]` at any time inside the app to view the interactive help modal.

### Global Playback Controls
| Key | Action |
| --- | --- |
| `[Space]` | Play / Pause toggle |
| `[s]` | Stop playback |
| `[←]` / `[→]` | Previous / Next track in queue |
| `[Shift+←]` / `[Shift+→]` | Seek backward / forward 10 seconds |
| `[+]` / `[-]` | Volume Up / Down (or `[↑]` / `[↓]` in Devices) |
| `[m]` | Toggle Mute / Unmute |
| `[R]` | Restart track from beginning |
| `[t]` | Toggle PCM WAV transcoding |

### Navigation & Panes
| Key | Action |
| --- | --- |
| `[Tab]` | Switch between **Queue**, **Devices**, and **Settings** tabs |
| `[1]` / `[2]` / `[3]` | Jump directly to tab 1 (Queue), tab 2 (Devices), or tab 3 (Settings) |
| `[h]` / `[l]` | Switch focus between **Browser** and **Queue** panes |
| `[↑]` / `[↓]` | Navigate items in list / settings (with circular wraparound) |
| `[v]` | Toggle between **Metadata (Tags)** and **Filename** view |
| `[?]` | Toggle Help modal overlay |
| `[q]` / `[Esc]` | Clean exit |

### Queue & File Browser Actions
| Key | Action |
| --- | --- |
| `[Enter]` | Enter directory / Add highlighted file to queue / Play selected track |
| `[v]` | Toggle file list display mode (Metadata vs Filename) |
| `[a]` | Add all audio files in current browser directory to queue |
| `[b]` | Bookmark or unbookmark current folder (or remove highlighted bookmark in Music Homes) |
| `[d]` / `[x]` | Remove highlighted track from queue, or remove selected bookmark in Music Homes |
| `[c]` | Clear entire queue |

---

## Architecture Overview

For details on project architecture, constraints, and AI agent guidelines, refer to [`AGENTS.md`](AGENTS.md).

---

## Testing

Run the full automated test suite:
```bash
python3 -m unittest discover tests
```

---

## License

This project is licensed under the [MIT License](LICENSE).
