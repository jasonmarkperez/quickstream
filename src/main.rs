//! Quickstream: A lightweight terminal audio player and DLNA network streamer in Rust.

pub mod discovery;
pub mod library;
pub mod logger;
pub mod metadata;
pub mod paths;
pub mod player;
pub mod queue;
pub mod settings;
pub mod ui;
pub mod web;

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

use crate::library::LibraryManager;
use crate::metadata::{extract_metadata, is_supported_audio_file};
use crate::settings::SettingsManager;
use crate::ui::AppUI;

#[derive(Parser, Debug)]
#[command(name = "quickstream")]
#[command(version = "0.2.0")]
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
}

fn main() -> anyhow::Result<()> {
    // Initialize logging and signal handler
    let log_path = crate::logger::init();
    crate::log_info!("main", "Starting Quickstream v0.2.0 (PID {}, log: {})", std::process::id(), log_path.display());

    let _ = ctrlc::set_handler(move || {
        let _ = crossterm::terminal::disable_raw_mode();
        let mut stdout = std::io::stdout();
        let _ = crossterm::execute!(stdout, crossterm::terminal::LeaveAlternateScreen, crossterm::cursor::Show);
        std::process::exit(0);
    });

    let cli = Cli::parse();

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
    let start_dir = cli.paths.first().filter(|p| p.is_dir()).cloned();
    let mut app = AppUI::new(start_dir, settings_mgr);

    // Pre-queue any explicit files passed on CLI
    for path in &cli.paths {
        if path.is_file() && is_supported_audio_file(path) {
            if let Ok(meta) = extract_metadata(path) {
                app.queue.add_track(meta);
            }
        }
    }

    app.run()
}
