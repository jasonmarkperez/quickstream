# Contributing to Quickstream

Thank you for your interest in contributing to Quickstream! This project aims to provide a fast, distraction-free terminal music player and DLNA audio streamer.

---

## 1. Development Principles

- **Zero Third-Party Python Dependencies**: Quickstream relies exclusively on the Python standard library (3.10+). Do not introduce external packages from PyPI into dependencies.
- **System Tooling**: Quickstream utilizes standard external utilities:
  - `ffmpeg` for transcoding, metadata fallback, and waveform extraction.
  - `ffplay` for local audio output.
- **Robustness**: Terminal UI applications must handle unexpected disconnections, broken pipes, and terminal resizing cleanly without crashing or leaving orphaned processes.

---

## 2. Setting Up the Environment

1. Clone or open the repository.
2. Ensure you have Python 3.10 or newer:
   ```bash
   python3 --version
   ```
3. Install `ffmpeg` and `ffplay` if not already available:
   - **macOS** (Homebrew): `brew install ffmpeg`
   - **Linux** (Debian/Ubuntu): `sudo apt install ffmpeg`
4. Install Quickstream in editable development mode:
   ```bash
   pip install -e .
   ```

---

## 3. Running Quickstream

You can run Quickstream directly during development:
```bash
python3 -m quickstream
# or using the launcher script:
./bin/quickstream
```

To run with an audio file or directory:
```bash
python3 -m quickstream path/to/album/
```

---

## 4. Running Tests

Quickstream uses the built-in `unittest` framework:
```bash
python3 -m unittest discover tests
```

When contributing:
- Ensure all existing tests pass before opening a PR or committing.
- Add unit tests in `tests/test_<feature>.py` for any new functionality or bug fixes.
- Mock all network requests and external subprocesses in unit tests.

---

## 5. Coding Standards

- Follow PEP 8 style conventions.
- Provide type annotations and docstrings for public classes and functions.
- Keep curses terminal output resilient by wrapping draw operations with safe bounds checks.
- When committing, use Conventional Commits (e.g. `feat:`, `fix:`, `docs:`, `test:`, `refactor:`).
