//! Keys that act on a folder that may sit on a slow share (issue #103). The mix picker and Seed
//! Radio (`r`, and the web `seed_radio` command) start their Autopilot scan with the key and leave
//! the check of the browsed folder (else their fallback) to the scan's worker; `~` opens the
//! startup home like any other folder (`navigate_browser`). Nothing a key does waits for the disk.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use common::tui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use quickstream::analysis::MixProfile;
use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;
use quickstream::web::WebCommand;

/// Whether `path` is there, as the app's own check finds it.
fn path_check(path: &Path) -> bool {
    quickstream::ui::folder_exists(path)
}

/// Stands in for a slow share: a check (or listing) of `gated` waits until the test opens the
/// gate (or the gate is dropped); other paths are looked up at once. Each check of `gated` that
/// starts is reported to `entered`.
struct Gate {
    open: Arc<(Mutex<bool>, Condvar)>,
    entered: mpsc::Receiver<PathBuf>,
}

impl Gate {
    fn new() -> (Self, impl Fn(&Path) + Send + Sync + 'static) {
        let open = Arc::new((Mutex::new(false), Condvar::new()));
        let (entered_tx, entered) = mpsc::channel();
        let entered_tx = Mutex::new(entered_tx);
        let gate = open.clone();
        let wait = move |path: &Path| {
            let _ = entered_tx.lock().unwrap().send(path.to_path_buf());
            let (lock, cvar) = &*gate;
            drop(cvar.wait_while(lock.lock().unwrap(), |open| !*open).unwrap());
        };
        (Gate { open, entered }, wait)
    }

    /// Gate the app's folder check of `gated` (`set_path_check`).
    fn check(app: &mut AppUI, gated: &Path) -> Self {
        let (gate, wait) = Self::new();
        let gated = gated.to_path_buf();
        app.set_path_check(Arc::new(move |path: &Path| {
            if path == gated {
                wait(path);
            }
            path_check(path)
        }));
        gate
    }

    /// Gate the browser's listing of `gated` (a folder being opened on a slow share).
    fn listing(app: &mut AppUI, gated: &Path) -> Self {
        let (gate, wait) = Self::new();
        let gated = gated.to_path_buf();
        app.set_dir_reader(Arc::new(move |dir: &Path| {
            if dir == gated {
                wait(dir);
            }
            Ok(Box::new(fs::read_dir(dir)?) as quickstream::ui::DirEntries)
        }));
        gate
    }

    /// Wait (at most 5 s, a deadlock guard) until a check of the gated path has started.
    fn wait_entered(&self) -> PathBuf {
        self.entered
            .recv_timeout(Duration::from_secs(5))
            .expect("the gated path was never checked")
    }

    /// How many more checks of the gated path start within `wait`.
    fn entered_within(&self, wait: Duration) -> usize {
        let deadline = Instant::now() + wait;
        let mut count = 0;
        while self
            .entered
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .is_ok()
        {
            count += 1;
        }
        count
    }

    fn open(&self) {
        let (lock, cvar) = &*self.open;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        self.open();
    }
}

/// Take in what the background workers have sent, as one event-loop tick does.
fn pump(app: &mut AppUI) {
    app.poll_background_autopilot();
    app.poll_background_dir_scan();
}

/// Pump for `wait`, checking `holds` after every tick.
fn pump_for(app: &mut AppUI, wait: Duration, holds: impl Fn(&AppUI) -> bool, what: &str) {
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        pump(app);
        assert!(holds(app), "{}", what);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn press(app: &mut AppUI, code: KeyCode) -> bool {
    app.handle_input_key(KeyEvent::new(code, KeyModifiers::NONE)).unwrap()
}

/// A temp root (canonical, so paths compare on macOS) with `Music/` and `Other/`, and an app
/// browsing `Music/` on tab 2's library pane, with `home` as its startup home.
fn setup(name: &str, home: Option<&str>) -> (AppUI, PathBuf) {
    let root = std::env::temp_dir().join(format!("qs_folder_checks_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("Music")).unwrap();
    fs::create_dir_all(root.join("Other")).unwrap();
    let root = fs::canonicalize(&root).unwrap();
    let mut settings_mgr = SettingsManager::with_path(root.join("config.json"));
    settings_mgr.settings.metadata_view = false;
    settings_mgr.settings.home_dir = home.map(|h| root.join(h).to_string_lossy().to_string());
    let mut app = AppUI::new(Some(root.join("Music")), settings_mgr);
    common::wait_for_dir_scan(&mut app);
    app.player = Box::new(common::fake_player::FakePlayer::default());
    app.active_tab = 1;
    (app, root)
}

/// A short WAV file the Autopilot scan picks up.
fn write_wav(path: &Path) {
    let data_len = 1000u32;
    let mut bytes = Vec::with_capacity(44 + data_len as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&44100u32.to_le_bytes());
    bytes.extend_from_slice(&(44100u32 * 4).to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    bytes.resize(44 + data_len as usize, 0);
    fs::write(path, bytes).unwrap();
}

fn queue_seed(app: &mut AppUI, seed: &Path) {
    app.queue.add_track(quickstream::metadata::AudioMetadata {
        title: "Seed".to_string(),
        filepath: seed.to_path_buf(),
        ..Default::default()
    });
    app.queue.set_current_index(0);
}

// ---------------------------------------------------------------------------------------------
// `~`
// ---------------------------------------------------------------------------------------------

/// `~` opens the home at once, like any folder: the shown one stays until the home's listing is
/// in (or its deadline passes, then READING DIRECTORY), and the status says where it goes.
#[test]
fn test_tilde_opens_the_home_at_once() {
    common::isolate();
    let (mut app, root) = setup("tilde", Some("Home"));
    let home = root.join("Home");
    fs::create_dir_all(&home).unwrap();
    let listing = Gate::listing(&mut app, &home);
    app.set_folder_open_deadline(Duration::ZERO);

    assert!(!press(&mut app, KeyCode::Char('~')));
    assert_eq!(app.status_message, format!("Jumped to home: {}", home.display()));
    assert_eq!(app.opening_folder(), Some(home.as_path()));
    assert_eq!(listing.wait_entered(), home);

    // Past the deadline the home shows, still being read.
    pump(&mut app);
    assert_eq!(app.browser_dir, home);
    assert!(app.browser_loading);
    let screen = tui::render_text(&mut app, 120, 40);
    assert!(screen.contains("READING DIRECTORY"), "{}", screen);

    listing.open();
    common::wait_for_dir_scan(&mut app);
    assert_eq!(app.browser_dir, home);
    assert_eq!(app.status_message, format!("Jumped to home: {}", home.display()));
    let _ = fs::remove_dir_all(&root);
}

/// A home that isn't there (or isn't a folder) is reported as before, and the browser stays in
/// the folder it shows.
#[test]
fn test_tilde_reports_a_missing_home() {
    common::isolate();
    for (name, home) in [("tilde_missing", "Gone"), ("tilde_file", "file.txt")] {
        let (mut app, root) = setup(name, Some(home));
        fs::write(root.join("file.txt"), b"not a folder").unwrap();

        assert!(!press(&mut app, KeyCode::Char('~')));
        common::wait_for_dir_scan(&mut app);
        assert_eq!(
            app.status_message,
            format!("Home directory not found: {}", root.join(home).display()),
            "{}",
            home
        );
        assert_eq!(app.browser_dir, root.join("Music"), "{}", home);
        assert_eq!(app.opening_folder(), None, "{}", home);
        let _ = fs::remove_dir_all(&root);
    }
}

/// `~` to a missing home while the shown folder is itself still being read (so the home was
/// shown at once): the browser goes back to that folder and reads it again.
#[test]
fn test_tilde_to_a_missing_home_from_a_folder_still_being_read_goes_back() {
    common::isolate();
    let (mut app, root) = setup("tilde_back", Some("Gone"));
    let sub = root.join("Music").join("Sub");
    fs::create_dir_all(&sub).unwrap();
    write_wav(&sub.join("track.wav"));
    app.set_folder_open_deadline(Duration::ZERO);
    let listing = Gate::listing(&mut app, &sub);

    app.navigate_browser(sub.clone());
    listing.wait_entered();
    pump(&mut app);
    assert_eq!(app.browser_dir, sub);
    assert!(app.browser_loading);

    press(&mut app, KeyCode::Char('~'));
    let missing = format!("Home directory not found: {}", root.join("Gone").display());
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.status_message != missing && Instant::now() < deadline {
        pump(&mut app);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(app.status_message, missing);
    assert_eq!(
        app.browser_dir, sub,
        "the browser goes back to the folder it was reading"
    );
    assert!(app.browser_loading, "and reads it again");

    listing.open();
    common::wait_for_dir_scan(&mut app);
    assert_eq!(app.browser_dir, sub);
    assert!(
        app.browser_entries.iter().any(|(_, p, _)| p == &sub.join("track.wav")),
        "{:?}",
        app.browser_entries
    );
    let _ = fs::remove_dir_all(&root);
}

/// A home that is there but can't be listed reads like any folder that can't be listed.
#[test]
fn test_tilde_reports_a_home_that_cannot_be_read() {
    common::isolate();
    let (mut app, root) = setup("tilde_unreadable", Some("Home"));
    let home = root.join("Home");
    fs::create_dir_all(&home).unwrap();
    let refused = home.clone();
    app.set_dir_reader(Arc::new(move |dir: &Path| {
        if dir == refused {
            return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        }
        Ok(Box::new(fs::read_dir(dir)?) as quickstream::ui::DirEntries)
    }));

    press(&mut app, KeyCode::Char('~'));
    common::wait_for_dir_scan(&mut app);
    assert!(
        app.status_message
            .starts_with(&format!("Error reading directory '{}': ", home.display())),
        "{}",
        app.status_message
    );
    let _ = fs::remove_dir_all(&root);
}

/// Esc while a slow home opens stops opening it and stays in the shown folder; it doesn't quit.
#[test]
fn test_esc_while_tilde_opens_a_slow_home_does_not_quit() {
    common::isolate();
    let (mut app, root) = setup("tilde_esc", Some("Home"));
    let home = root.join("Home");
    fs::create_dir_all(&home).unwrap();
    app.set_folder_open_deadline(Duration::from_secs(600));
    let _check = Gate::check(&mut app, &home);
    let listing = Gate::listing(&mut app, &home);

    press(&mut app, KeyCode::Char('~'));
    assert!(!press(&mut app, KeyCode::Esc), "Esc stops the open instead of quitting");
    assert_eq!(app.status_message, "Stopped opening 'Home'");
    assert_eq!(app.browser_dir, root.join("Music"));
    assert_eq!(app.opening_folder(), None);

    listing.open();
    pump_for(
        &mut app,
        Duration::from_millis(100),
        |app| app.opening_folder().is_none() && app.browser_dir.ends_with("Music"),
        "the stopped open never shows",
    );
    let _ = fs::remove_dir_all(&root);
}

/// Pressing `~` again while the home is still being opened keeps that open: no new worker per
/// press, however long the share takes.
#[test]
fn test_repeated_tilde_presses_on_a_slow_home_read_it_once() {
    common::isolate();
    let (mut app, root) = setup("tilde_repeat", Some("Home"));
    let home = root.join("Home");
    fs::create_dir_all(&home).unwrap();
    app.set_folder_open_deadline(Duration::from_secs(600));
    let check = Gate::check(&mut app, &home);
    let listing = Gate::listing(&mut app, &home);

    press(&mut app, KeyCode::Char('~'));
    listing.wait_entered();
    let workers = app.live_worker_count();
    for _ in 0..5 {
        press(&mut app, KeyCode::Char('~'));
        pump(&mut app);
    }
    assert_eq!(listing.entered_within(Duration::from_millis(200)), 0);
    assert_eq!(check.entered_within(Duration::ZERO), 0, "no separate folder check");
    assert!(app.live_worker_count() <= workers);
    assert_eq!(app.opening_folder(), Some(home.as_path()));

    listing.open();
    common::wait_for_dir_scan(&mut app);
    assert_eq!(app.browser_dir, home);
    let _ = fs::remove_dir_all(&root);
}

/// The browser sent elsewhere while the home opens stays there (the last key wins).
#[test]
fn test_tilde_listing_is_dropped_once_the_browser_moved_on() {
    common::isolate();
    let (mut app, root) = setup("tilde_moved", Some("Home"));
    let home = root.join("Home");
    fs::create_dir_all(&home).unwrap();
    let listing = Gate::listing(&mut app, &home);

    press(&mut app, KeyCode::Char('~'));
    // Backspace to the parent while the home is read.
    press(&mut app, KeyCode::Backspace);
    common::wait_for_dir_scan(&mut app);
    assert_eq!(app.browser_dir, root);
    let status = app.status_message.clone();

    listing.open();
    pump_for(
        &mut app,
        Duration::from_millis(100),
        |app| app.opening_folder().is_none(),
        "the home's late listing opens nothing",
    );
    assert_eq!(app.browser_dir, root);
    assert_eq!(app.status_message, status);
    let _ = fs::remove_dir_all(&root);
}

/// `~` pressed while another folder is still being opened wins over it.
#[test]
fn test_tilde_wins_over_a_folder_already_being_opened() {
    common::isolate();
    let (mut app, root) = setup("tilde_open", Some("Home"));
    let home = root.join("Home");
    let sub = root.join("Music").join("Sub");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&sub).unwrap();
    app.set_folder_open_deadline(Duration::from_secs(600));
    let read_sub = Gate::listing(&mut app, &sub);

    app.navigate_browser(sub.clone());
    read_sub.wait_entered();
    assert_eq!(app.opening_folder(), Some(sub.as_path()));
    press(&mut app, KeyCode::Char('~'));
    read_sub.open();
    common::wait_for_dir_scan(&mut app);
    assert_eq!(app.browser_dir, home, "the last key pressed wins");
    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// Mix picker and Seed Radio
// ---------------------------------------------------------------------------------------------

#[test]
fn test_mix_picker_starts_autopilot_at_the_key_while_its_worker_checks_the_folder() {
    common::isolate();
    let (mut app, root) = setup("mix", None);
    let music = root.join("Music");
    let check = Gate::check(&mut app, &music);

    press(&mut app, KeyCode::Char('M'));
    press(&mut app, KeyCode::Enter);
    assert!(!app.show_mix_picker_modal);
    assert_eq!(app.active_mix_profile, MixProfile::picker_profiles()[0]);
    // The scan started with the key; only its worker waits for the folder.
    assert_eq!(app.autopilot_dir, Some(music.clone()));
    assert!(app.is_autopilot_scanning);
    assert!(
        app.status_message
            .starts_with("⚡ Autopilot: Discovering tracks from 'Music'"),
        "{}",
        app.status_message
    );
    assert_eq!(check.wait_entered(), music);

    check.open();
    app.wait_for_autopilot_scan(Duration::from_secs(5));
    assert!(!app.is_autopilot_scanning);
    assert_eq!(app.autopilot_dir, Some(music.clone()));
    assert_eq!(
        app.status_message,
        format!("No audio files found in '{}'", music.display())
    );
    app.reset_playback_session();
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_mix_picker_falls_back_when_the_browsed_folder_is_gone() {
    common::isolate();
    let (mut app, root) = setup("mix_gone", None);
    let other = root.join("Other");
    app.autopilot_dir = Some(other.clone());
    fs::remove_dir_all(root.join("Music")).unwrap();

    press(&mut app, KeyCode::Char('M'));
    press(&mut app, KeyCode::Char('2'));
    assert_eq!(app.active_mix_profile, MixProfile::picker_profiles()[1]);
    assert!(app.is_autopilot_scanning);
    app.wait_for_autopilot_scan(Duration::from_secs(5));
    assert_eq!(app.autopilot_dir, Some(other.clone()));
    assert_eq!(
        app.status_message,
        format!("No audio files found in '{}'", other.display())
    );
    app.reset_playback_session();
    let _ = fs::remove_dir_all(&root);
}

/// The mix picker pressed again while the first start still checks a folder that turns out to
/// be gone falls back where the first start does (the last mix's folder), not to the folder
/// still being checked.
#[test]
fn test_mix_picker_pressed_again_during_a_check_keeps_the_fallback() {
    common::isolate();
    let (mut app, root) = setup("mix_again", None);
    let music = root.join("Music");
    let other = root.join("Other");
    app.autopilot_dir = Some(other.clone());
    fs::remove_dir_all(&music).unwrap();
    let check = Gate::check(&mut app, &music);

    press(&mut app, KeyCode::Char('M'));
    press(&mut app, KeyCode::Enter);
    check.wait_entered();
    press(&mut app, KeyCode::Char('M'));
    press(&mut app, KeyCode::Enter);
    check.open();
    app.wait_for_autopilot_scan(Duration::from_secs(5));
    assert_eq!(app.autopilot_dir, Some(other.clone()));
    assert_eq!(
        app.status_message,
        format!("No audio files found in '{}'", other.display())
    );
    app.reset_playback_session();
    let _ = fs::remove_dir_all(&root);
}

/// Esc on a start still checking its folder puts back the folder the last mix named, so the
/// next start falls back there as before.
#[test]
fn test_esc_during_a_check_keeps_the_fallback_for_the_next_start() {
    common::isolate();
    let (mut app, root) = setup("mix_esc_again", None);
    let music = root.join("Music");
    let other = root.join("Other");
    app.autopilot_dir = Some(other.clone());
    fs::remove_dir_all(&music).unwrap();
    let check = Gate::check(&mut app, &music);

    press(&mut app, KeyCode::Char('M'));
    press(&mut app, KeyCode::Enter);
    check.wait_entered();
    assert!(!press(&mut app, KeyCode::Esc));
    assert_eq!(app.status_message, "Stopped starting Autopilot in 'Music'");
    assert_eq!(app.autopilot_dir, Some(other.clone()));

    check.open();
    press(&mut app, KeyCode::Char('M'));
    press(&mut app, KeyCode::Enter);
    app.wait_for_autopilot_scan(Duration::from_secs(5));
    assert_eq!(app.autopilot_dir, Some(other.clone()));
    assert_eq!(
        app.status_message,
        format!("No audio files found in '{}'", other.display())
    );
    app.reset_playback_session();
    let _ = fs::remove_dir_all(&root);
}

/// Choosing a mix stops the Autopilot scan already running at once, as before the check moved
/// off the UI thread: none of the old scan's tracks are queued while the new one's folder is
/// checked.
#[test]
fn test_mix_picker_stops_the_running_scan_at_the_key() {
    common::isolate();
    let (mut app, root) = setup("mix_stops", None);
    let music = root.join("Music");
    let other = root.join("Other");
    for i in 0..3 {
        write_wav(&other.join(format!("old{}.wav", i)));
    }
    app.start_autopilot(other.clone());
    // The old scan sends its tracks; nothing has taken them in yet.
    std::thread::sleep(Duration::from_millis(200));
    let check = Gate::check(&mut app, &music);

    press(&mut app, KeyCode::Char('M'));
    press(&mut app, KeyCode::Enter);
    check.wait_entered();
    assert_eq!(app.autopilot_dir, Some(music.clone()));
    pump_for(
        &mut app,
        Duration::from_millis(500),
        |app| app.queue.is_empty(),
        "the stopped scan queued a track",
    );
    app.reset_playback_session();
    let _ = fs::remove_dir_all(&root);
}

/// Esc while an Autopilot start still checks its folder stops it; it doesn't quit.
#[test]
fn test_esc_while_autopilot_checks_its_folder_stops_it() {
    common::isolate();
    let (mut app, root) = setup("mix_esc", None);
    let music = root.join("Music");
    let check = Gate::check(&mut app, &music);

    press(&mut app, KeyCode::Char('M'));
    press(&mut app, KeyCode::Enter);
    check.wait_entered();
    assert!(
        !press(&mut app, KeyCode::Esc),
        "Esc stops the start instead of quitting"
    );
    assert!(!app.is_autopilot_scanning);
    assert_eq!(app.status_message, "Stopped starting Autopilot in 'Music'");

    check.open();
    pump_for(
        &mut app,
        Duration::from_millis(100),
        |app| !app.is_autopilot_scanning,
        "the stopped start never scans",
    );
    // With nothing left to stop, Esc quits as before.
    assert!(press(&mut app, KeyCode::Esc));
    let _ = fs::remove_dir_all(&root);
}

/// Pressing the mix picker again and again while a hung share holds the folder check starts no
/// second check: later starts wait for the one running (and a superseded start's worker leaves).
#[test]
fn test_repeated_mix_picker_presses_on_a_hung_folder_check_it_once() {
    common::isolate();
    let (mut app, root) = setup("mix_repeat", None);
    let music = root.join("Music");
    let check = Gate::check(&mut app, &music);

    press(&mut app, KeyCode::Char('M'));
    press(&mut app, KeyCode::Enter);
    check.wait_entered();
    let workers = app.live_worker_count();
    for _ in 0..5 {
        press(&mut app, KeyCode::Char('M'));
        press(&mut app, KeyCode::Enter);
    }
    assert_eq!(check.entered_within(Duration::from_millis(200)), 0);
    // The stuck check's worker, and the latest start's waiting for it.
    let deadline = Instant::now() + Duration::from_secs(2);
    while app.live_worker_count() > workers + 1 && Instant::now() < deadline {
        pump(&mut app);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        app.live_worker_count() <= workers + 1,
        "superseded starts left their workers running: {} > {}",
        app.live_worker_count(),
        workers + 1
    );

    check.open();
    app.wait_for_autopilot_scan(Duration::from_secs(5));
    assert_eq!(app.autopilot_dir, Some(music));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_seed_radio_starts_at_the_key_while_its_worker_checks_the_folder() {
    common::isolate();
    let (mut app, root) = setup("seed", None);
    let music = root.join("Music");
    let seed = root.join("Other").join("seed.wav");
    queue_seed(&mut app, &seed);
    let check = Gate::check(&mut app, &music);

    app.active_tab = 0;
    press(&mut app, KeyCode::Char('r'));
    assert_eq!(app.active_mix_profile, MixProfile::SeedRadio(seed.clone()));
    assert_eq!(app.autopilot_dir, Some(music.clone()));
    assert!(app.is_autopilot_scanning);
    assert_eq!(app.queue.len(), 1, "an appending mix keeps the queue");
    check.wait_entered();
    check.open();
    app.wait_for_autopilot_scan(Duration::from_secs(5));
    assert_eq!(app.autopilot_dir, Some(music.clone()));
    app.reset_playback_session();

    // The web dashboard's `seed_radio` starts at once too, in the folder the web thread resolved
    // (issue #126), which the UI checks no further.
    queue_seed(&mut app, &seed);
    let (tx, rx) = mpsc::channel();
    app.web_cmd_rx = Some(rx);
    tx.send(WebCommand::SeedRadio {
        seed: seed.clone(),
        dir: music.clone(),
    })
    .unwrap();
    app.handle_web_commands();
    assert_eq!(app.active_mix_profile, MixProfile::SeedRadio(seed.clone()));
    assert_eq!(app.autopilot_dir, Some(music.clone()));
    assert!(app.is_autopilot_scanning);
    app.wait_for_autopilot_scan(Duration::from_secs(5));
    assert_eq!(app.autopilot_dir, Some(music));
    app.reset_playback_session();
    let _ = fs::remove_dir_all(&root);
}

/// Starting Seed Radio stops the scan already running at once, like the mix picker.
#[test]
fn test_seed_radio_stops_the_running_scan_at_the_key() {
    common::isolate();
    let (mut app, root) = setup("seed_stops", None);
    let music = root.join("Music");
    let other = root.join("Other");
    for i in 0..3 {
        write_wav(&other.join(format!("old{}.wav", i)));
    }
    let seed = other.join("old0.wav");
    queue_seed(&mut app, &seed);
    app.start_autopilot(other.clone());
    std::thread::sleep(Duration::from_millis(200));
    let check = Gate::check(&mut app, &music);

    app.active_tab = 0;
    press(&mut app, KeyCode::Char('r'));
    check.wait_entered();
    pump_for(
        &mut app,
        Duration::from_millis(500),
        |app| app.queue.len() == 1,
        "the stopped scan appended a track",
    );
    app.reset_playback_session();
    let _ = fs::remove_dir_all(&root);
}

/// A web profile change while Seed Radio's folder is checked doesn't restart or replace the
/// scan the key started (with the key's profile); it stands for later mixes.
#[test]
fn test_seed_radio_scan_survives_a_web_profile_change_during_its_check() {
    common::isolate();
    let (mut app, root) = setup("seed_profile", None);
    let music = root.join("Music");
    let seed = root.join("Other").join("seed.wav");
    queue_seed(&mut app, &seed);
    let (tx, rx) = mpsc::channel();
    app.web_cmd_rx = Some(rx);
    let check = Gate::check(&mut app, &music);

    app.active_tab = 0;
    press(&mut app, KeyCode::Char('r'));
    check.wait_entered();
    tx.send(WebCommand::SetMixProfile("focus".to_string())).unwrap();
    app.handle_web_commands();
    let focus = MixProfile::MoodVibe(quickstream::analysis::MoodType::Focus);
    assert_eq!(app.active_mix_profile, focus);
    assert!(app.is_autopilot_scanning);

    check.open();
    app.wait_for_autopilot_scan(Duration::from_secs(5));
    assert_eq!(app.autopilot_dir, Some(music));
    assert_eq!(app.active_mix_profile, focus);
    app.reset_playback_session();
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_seed_radio_falls_back_to_the_seed_folder_when_the_browsed_folder_is_gone() {
    common::isolate();
    let (mut app, root) = setup("seed_gone", None);
    let seed = root.join("Other").join("seed.wav");
    queue_seed(&mut app, &seed);
    fs::remove_dir_all(root.join("Music")).unwrap();

    app.active_tab = 0;
    press(&mut app, KeyCode::Char('r'));
    app.wait_for_autopilot_scan(Duration::from_secs(5));
    assert_eq!(app.autopilot_dir, Some(root.join("Other")));
    app.reset_playback_session();
    let _ = fs::remove_dir_all(&root);
}

/// Another Autopilot start, or the end of the session, while a start checks its folder: the
/// check's late answer starts nothing.
#[test]
fn test_autopilot_start_checking_its_folder_is_superseded() {
    common::isolate();
    let (mut app, root) = setup("mix_superseded", None);
    let music = root.join("Music");
    let other = root.join("Other");

    let check = Gate::check(&mut app, &music);
    press(&mut app, KeyCode::Char('M'));
    press(&mut app, KeyCode::Enter);
    check.wait_entered();
    app.start_autopilot(other.clone());
    check.open();
    app.wait_for_autopilot_scan(Duration::from_secs(5));
    pump(&mut app);
    assert_eq!(
        app.autopilot_dir,
        Some(other),
        "the late answer does not restart the mix"
    );
    app.reset_playback_session();

    let check = Gate::check(&mut app, &music);
    press(&mut app, KeyCode::Char('M'));
    press(&mut app, KeyCode::Enter);
    check.wait_entered();
    app.reset_playback_session();
    check.open();
    pump_for(
        &mut app,
        Duration::from_millis(100),
        |app| app.autopilot_dir.is_none() && !app.is_autopilot_scanning,
        "a reset session stays reset",
    );
    let _ = fs::remove_dir_all(&root);
}
