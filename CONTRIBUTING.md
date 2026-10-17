# Contributing to Quickstream

Thank you for your interest in contributing to Quickstream! This project aims to provide a blazing-fast, distraction-free terminal music player and DLNA / UPnP audio streamer written in native Rust.

---

## 1. Development Principles

- **Native Rust Performance**: Quickstream is built on Rust (2021 Edition). Crate dependencies are declared and managed strictly via `Cargo.toml`.
- **System Tooling**:
  - `ffmpeg`: Required for audio format transcoding, sample extraction, and fallback metadata extraction.
  - `mpv`: Primary local playback backend (JSON-RPC IPC socket).
  - `ffplay`: Fallback local audio playback if `mpv` is not installed.
- **Robustness**: Terminal UI applications must handle unexpected network disconnects, broken pipes, and terminal resizing cleanly without crashing or leaving orphaned processes or hanging sockets.
- **Zero-Touch Library Invariant**: Never write to, modify, rename, or touch user audio files or music library directories. All application state lives in standard XDG directories (`~/.config/quickstream/`, `~/.cache/quickstream/`, `~/.local/share/quickstream/`).

---

## 2. Setting Up the Environment

1. Clone or open the repository.
2. Ensure you have the Rust toolchain (1.75+ or newer):
   ```bash
   rustc --version
   cargo --version
   ```
3. Install `ffmpeg` and `mpv` if not already available:
   - **macOS** (Homebrew): `brew install ffmpeg mpv`
   - **Linux** (Debian/Ubuntu): `sudo apt install ffmpeg mpv`

---

## 3. Running Quickstream

Run Quickstream directly during development:
```bash
cargo run
```

To run with an audio file or directory:
```bash
cargo run -- /path/to/music
```

With web dashboard enabled:
```bash
cargo run -- --web
```

---

## 4. Running Tests & Quality Verification

Run the full integration test suite with single-threaded execution (required for deterministic local TCP port binding):
```bash
cargo test -- --test-threads=1
```

Run lints:
```bash
cargo clippy -- -D warnings
```

Build the optimized release binary:
```bash
cargo build --release
```

When contributing:
- Ensure all tests pass and `cargo clippy` has zero warnings before opening a PR or committing.
- Add integration tests in `tests/test_<feature>.rs` for any new functionality or bug fixes.
- Mock all network requests and external subprocesses in unit tests where practical.
- Use Conventional Commits (e.g. `feat:`, `fix:`, `docs:`, `test:`, `refactor:`).
