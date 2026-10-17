//! Quickstream: A lightweight terminal audio player and DLNA network streamer in Rust.
//!
//! The binary is a thin CLI over the `quickstream` library crate (`src/lib.rs`), so every module
//! and static (logger, shutdown flags) exists exactly once.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

use quickstream::discovery;
use quickstream::library::LibraryManager;
use quickstream::metadata::{extract_metadata, is_supported_audio_file};
use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;

#[derive(Parser, Debug)]
#[command(name = "quickstream")]
#[command(version)]
#[command(about = "A lightweight terminal music player and DLNA network streamer in Rust", long_about = None)]
struct Cli {
    /// Audio files or directories to load
    paths: Vec<PathBuf>,

    /// Inspect audio metadata tags and exit
    #[arg(long)]
    info: Option<PathBuf>,

    /// Launch directly into Skim interactive fuzzy finder mode
    #[arg(long, short)]
    search: bool,

    /// Scan local network for DLNA / UPnP MediaRenderers
    #[arg(long)]
    scan: bool,

    /// Launch local Now Playing web server (HTTP metadata, cover art & remote control)
    #[arg(long)]
    web: bool,

    /// Port for Now Playing web server (default: 8080)
    #[arg(long)]
    web_port: Option<u16>,

    /// Set persistent startup home directory and exit (use "default" or "none" to clear)
    #[arg(long)]
    set_home: Option<PathBuf>,

    /// Autopilot mode: recursively discover all audio tracks, build random queue, and start playing immediately
    #[arg(long, short = 'a', num_args = 0..=1)]
    autopilot: Option<Option<PathBuf>>,
}

fn main() -> anyhow::Result<()> {
    // Initialize logging and signal handler
    let log_path = quickstream::logger::init();
    quickstream::log_info!(
        "main",
        "Starting Quickstream v{} (PID {}, log: {})",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        log_path.display()
    );

    let _ = ctrlc::set_handler(move || {
        if quickstream::ui::is_tui_active() {
            // Let the event loop return so Drop cleanup runs (kills mpv, removes its socket, stops renderers).
            quickstream::ui::request_shutdown();
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while quickstream::ui::is_tui_active() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if !quickstream::ui::is_tui_active() {
                return;
            }
        } else if quickstream::ui::is_shutting_down() {
            // Already exiting (e.g. a second Ctrl-C after the terminal left raw mode): let Drop
            // cleanup finish instead of exiting with mpv still running.
            return;
        }
        let _ = crossterm::terminal::disable_raw_mode();
        let mut stdout = std::io::stdout();
        let _ = crossterm::execute!(stdout, crossterm::terminal::LeaveAlternateScreen, crossterm::cursor::Show);
        std::process::exit(0);
    });

    let cli = Cli::parse();

    // Set startup home flag
    if let Some(home_path) = cli.set_home {
        let trimmed = home_path.to_string_lossy().trim().to_string();
        if trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("none")
            || trimmed.eq_ignore_ascii_case("default")
            || trimmed.eq_ignore_ascii_case("clear")
        {
            let mut settings_mgr = SettingsManager::new();
            settings_mgr.settings.home_dir = None;
            if settings_mgr.save() {
                println!("Startup home directory cleared (defaulting to launch directory).");
                return Ok(());
            } else {
                eprintln!("Error: Failed to save settings to '{}'", settings_mgr.path.display());
                std::process::exit(1);
            }
        }

        let abs_path = if home_path.is_absolute() {
            home_path
        } else {
            std::env::current_dir()?.join(home_path)
        };
        let canonical = match abs_path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("Error: Cannot resolve path '{}': {}", abs_path.display(), e);
                std::process::exit(1);
            }
        };
        if !canonical.is_dir() {
            eprintln!("Error: '{}' is not a directory", canonical.display());
            std::process::exit(1);
        }
        let mut settings_mgr = SettingsManager::new();
        let path_str = canonical.to_string_lossy().to_string();
        settings_mgr.settings.home_dir = Some(path_str.clone());
        if settings_mgr.save() {
            println!("Startup home directory set to: {}", path_str);
            return Ok(());
        } else {
            eprintln!("Error: Failed to save settings to '{}'", settings_mgr.path.display());
            std::process::exit(1);
        }
    }

    // Inspect metadata flag
    if let Some(info_path) = cli.info {
        if !info_path.exists() {
            eprintln!("Error: File '{}' does not exist", info_path.display());
            std::process::exit(1);
        }
        match extract_metadata(&info_path) {
            Ok(meta) => {
                println!("--- Audio Metadata ({}) ---", meta.format_name);
                println!("File:        {}", meta.filepath.display());
                println!("Title:       {}", meta.title);
                println!("Artist:      {}", meta.artist);
                println!("Album:       {}", meta.album);
                println!("Genre:       {}", meta.genre);
                println!("Duration:    {}", meta.formatted_duration());
                println!("Bitrate:     {} kbps", meta.bitrate_kbps);
                println!("Sample Rate: {} Hz", meta.sample_rate_hz);
                println!("Channels:    {}", meta.channels);
                println!("Size:        {:.2} MB", meta.file_size as f64 / 1_048_576.0);
                return Ok(());
            }
            Err(e) => {
                eprintln!("Error extracting metadata: {}", e);
                std::process::exit(1);
            }
        }
    }

    // Direct Skim search mode
    if cli.search {
        println!("Loading library index for Skim search...");
        let start_dir = cli.paths.first().cloned().unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let mut tracks = LibraryManager::load_cache();
        if tracks.is_empty() {
            println!("Indexing '{}'...", start_dir.display());
            tracks = LibraryManager::scan_directory(&start_dir);
            LibraryManager::save_cache(&tracks);
        }

        let selected = LibraryManager::search_with_skim(&tracks);
        if selected.is_empty() {
            println!("No tracks selected.");
            return Ok(());
        }

        println!("Selected {} tracks:", selected.len());
        for t in &selected {
            println!("  - {}", t.format_display_label(true));
        }
        return Ok(());
    }

    // Network scan
    if cli.scan {
        println!("Scanning network for DLNA MediaRenderers (SSDP)...");
        let devices = discovery::discover_devices(Duration::from_millis(3000));
        println!("Discovered audio outputs:");
        println!("  [0] Local Audio (mpv/ffplay)");
        for (i, dev) in devices.iter().enumerate() {
            println!("  [{}] {} ({}:{})", i + 1, dev.display_label(), dev.host, dev.port);
            println!("      Control: {}", dev.av_transport_control_url);
        }
        println!("\nScan complete ({} device(s) found).", devices.len() + 1);
        return Ok(());
    }

    // Launch full interactive TUI
    let mut settings_mgr = SettingsManager::new();
    if cli.web {
        settings_mgr.settings.web_server_enabled = true;
    }
    if let Some(port) = cli.web_port {
        settings_mgr.settings.web_server_port = port;
        settings_mgr.settings.web_server_enabled = true;
    }
    let explicit_dir = cli.paths.first().filter(|p| !is_supported_audio_file(p)).cloned();
    let start_dir = explicit_dir.or_else(|| {
        settings_mgr
            .settings
            .home_dir
            .as_ref()
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
    });
    let mut app = AppUI::new(start_dir, settings_mgr);

    // Check for Autopilot mode
    if let Some(autopilot_target) = cli.autopilot {
        let target_dir = autopilot_target
            .or_else(|| cli.paths.first().filter(|p| !is_supported_audio_file(p)).cloned())
            .or_else(|| {
                app.settings_mgr
                    .settings
                    .home_dir
                    .as_ref()
                    .map(PathBuf::from)
                    .filter(|p| p.is_dir())
            })
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        app.start_autopilot(target_dir);
    } else {
        // Pre-queue any explicit files passed on CLI
        for path in &cli.paths {
            if is_supported_audio_file(path) {
                if let Ok(meta) = extract_metadata(path) {
                    app.queue.add_track(meta);
                }
            }
        }
    }

    app.run()
}
