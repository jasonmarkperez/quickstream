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
2. Ensure you have the Rust toolchain (1.89 or newer, see `rust-version` in `Cargo.toml`):
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

Tests that bind all network interfaces are `#[ignore]`d so local runs do not trigger macOS firewall prompts; CI runs them on Linux. To run them yourself (`test_discover_devices_real` scans your LAN and asserts nothing, so it is skipped here):
```bash
cargo test -- --test-threads=1 --ignored --skip test_discover_devices_real
```

Check formatting and lints (including test code):
```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

CI (`.github/workflows/ci.yml`) runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test -- --test-threads=1` on Ubuntu and macOS for every push and pull request to `main`, plus the ignored tests on Ubuntu. The one-time bulk `cargo fmt` commit is listed in `.git-blame-ignore-revs`; run `git config blame.ignoreRevsFile .git-blame-ignore-revs` once so `git blame` skips it.

Build the optimized release binary:
```bash
cargo build --release
```

When contributing:
- Ensure all tests pass, `cargo fmt --check` is clean and `cargo clippy --all-targets` has zero warnings before opening a PR or committing.
- Add integration tests in `tests/test_<feature>.rs` for any new functionality or bug fixes. Each test file declares `mod common;` and each test starts with `common::isolate();` so it never touches your real `~/.config`, `~/.cache` or `~/.local/share` quickstream dirs (`tests/test_isolation.rs` enforces this).
- Mock all network requests and external subprocesses in unit tests where practical.
- Use Conventional Commits (e.g. `feat:`, `fix:`, `docs:`, `test:`, `refactor:`).
