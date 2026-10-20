//! Keys that check a folder before acting (issue #103): the mix picker, Seed Radio (`r`, and the
//! web `seed_radio` command) and `~` hand the check to a `folder-check` worker and act on its
//! answer in `poll_folder_checks`, so a slow or hung share never stalls the key handler.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use quickstream::analysis::MixProfile;
use quickstream::settings::SettingsManager;
use quickstream::ui::{AppUI, PathKind};
use quickstream::web::WebCommand;

/// Stands in for a slow share: looking `gated` up waits until the test opens the gate (or the
/// gate is dropped); other paths are looked up at once. `wait_entered` returns once a check of
/// `gated` has started.
struct GatedCheck {
    open: Arc<(Mutex<bool>, Condvar)>,
    entered: mpsc::Receiver<PathBuf>,
}

impl GatedCheck {
    fn install(app: &mut AppUI, gated: &Path) -> Self {
        let open = Arc::new((Mutex::new(false), Condvar::new()));
        let (entered_tx, entered) = mpsc::channel();
        let entered_tx = Mutex::new(entered_tx);
        let (gated, gate) = (gated.to_path_buf(), open.clone());
        app.set_path_check(Arc::new(move |path: &Path| {
            if path == gated {
                let _ = entered_tx.lock().unwrap().send(path.to_path_buf());
                let (lock, cvar) = &*gate;
                drop(cvar.wait_while(lock.lock().unwrap(), |open| !*open).unwrap());
            }
            PathKind::of(path)
        }));
        GatedCheck { open, entered }
    }

    /// Wait (at most 5 s, a deadlock guard) until a check of the gated path has started.
    fn wait_entered(&self) -> PathBuf {
        self.entered
            .recv_timeout(Duration::from_secs(5))
            .expect("the gated path was never checked")
    }

    fn open(&self) {
        let (lock, cvar) = &*self.open;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }
}

impl Drop for GatedCheck {
    fn drop(&mut self) {
        self.open();
    }
}

/// Poll until `poll_folder_checks` has taken in `answers` answers in all (at most 5 s, a
/// deadlock guard).
fn wait_for_answers(app: &mut AppUI, answers: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.folder_check_answers() < answers && Instant::now() < deadline {
        app.poll_folder_checks();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(app.folder_check_answers(), answers, "the folder check never answered");
}

fn press(app: &mut AppUI, code: KeyCode) {
    app.handle_input_key(KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
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

#[test]
fn test_tilde_jumps_home_only_once_the_check_answers() {
    common::isolate();
    let (mut app, root) = setup("tilde", Some("Home"));
    let home = root.join("Home");
    fs::create_dir_all(&home).unwrap();
    let gate = GatedCheck::install(&mut app, &home);
    let before = app.status_message.clone();

    press(&mut app, KeyCode::Char('~'));
    // The key only started the check: nothing waited for it.
    assert_eq!(gate.wait_entered(), home);
    app.poll_folder_checks();
    assert_eq!(app.browser_dir, root.join("Music"));
    assert_eq!(app.opening_folder(), None);
    assert_eq!(app.status_message, before);
    assert!(app.folder_check_pending());

    gate.open();
    wait_for_answers(&mut app, 1);
    assert_eq!(app.status_message, format!("Jumped to home: {}", home.display()));
    common::wait_for_dir_scan(&mut app);
    assert_eq!(app.browser_dir, home);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_tilde_reports_a_missing_home_after_the_poll() {
    common::isolate();
    let (mut app, root) = setup("tilde_missing", Some("Gone"));
    let before = app.status_message.clone();

    press(&mut app, KeyCode::Char('~'));
    assert_eq!(app.status_message, before, "nothing is reported before the poll");
    wait_for_answers(&mut app, 1);
    assert_eq!(
        app.status_message,
        format!("Home directory not found: {}", root.join("Gone").display())
    );
    assert_eq!(app.browser_dir, root.join("Music"));
    assert_eq!(app.opening_folder(), None);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_tilde_answer_is_dropped_once_the_browser_moved_on() {
    common::isolate();
    let (mut app, root) = setup("tilde_moved", Some("Home"));
    let home = root.join("Home");
    fs::create_dir_all(&home).unwrap();
    let gate = GatedCheck::install(&mut app, &home);

    press(&mut app, KeyCode::Char('~'));
    gate.wait_entered();
    // Backspace to the parent while the check runs.
    press(&mut app, KeyCode::Backspace);
    common::wait_for_dir_scan(&mut app);
    assert_eq!(app.browser_dir, root);
    let status = app.status_message.clone();

    gate.open();
    wait_for_answers(&mut app, 1);
    assert!(!app.folder_check_pending());
    assert_eq!(app.browser_dir, root, "the late answer does not move the browser");
    assert_eq!(app.opening_folder(), None);
    assert_eq!(app.status_message, status);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_mix_picker_starts_autopilot_only_once_the_check_answers() {
    common::isolate();
    let (mut app, root) = setup("mix", None);
    let music = root.join("Music");
    let gate = GatedCheck::install(&mut app, &music);

    press(&mut app, KeyCode::Char('M'));
    press(&mut app, KeyCode::Enter);
    assert!(!app.show_mix_picker_modal);
    let profile = MixProfile::picker_profiles()[0].clone();
    assert_eq!(app.active_mix_profile, profile);
    assert_eq!(
        app.status_message,
        format!("⚡ Mix Profile: '{}' - Starting Autopilot...", profile.name())
    );
    assert_eq!(gate.wait_entered(), music);
    app.poll_folder_checks();
    assert_eq!(app.autopilot_dir, None, "Autopilot waits for the check");
    assert!(!app.is_autopilot_scanning);

    gate.open();
    wait_for_answers(&mut app, 1);
    assert_eq!(app.autopilot_dir, Some(music));
    assert!(app
        .status_message
        .starts_with("⚡ Autopilot: Discovering tracks from 'Music'"));
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
    assert_eq!(
        app.autopilot_dir,
        Some(other.clone()),
        "nothing started before the poll"
    );
    assert!(!app.is_autopilot_scanning);
    wait_for_answers(&mut app, 1);
    assert_eq!(app.autopilot_dir, Some(other));
    assert!(app.is_autopilot_scanning);
    assert!(app
        .status_message
        .starts_with("⚡ Autopilot: Discovering tracks from 'Other'"));
    app.reset_playback_session();
    let _ = fs::remove_dir_all(&root);
}

fn queue_seed(app: &mut AppUI, seed: &Path) {
    app.queue.add_track(quickstream::metadata::AudioMetadata {
        title: "Seed".to_string(),
        filepath: seed.to_path_buf(),
        ..Default::default()
    });
    app.queue.set_current_index(0);
}

#[test]
fn test_seed_radio_starts_only_once_the_check_answers() {
    common::isolate();
    let (mut app, root) = setup("seed", None);
    let music = root.join("Music");
    let seed = root.join("Other").join("seed.wav");
    queue_seed(&mut app, &seed);
    let gate = GatedCheck::install(&mut app, &music);

    app.active_tab = 0;
    press(&mut app, KeyCode::Char('r'));
    assert_eq!(app.active_mix_profile, MixProfile::SeedRadio(seed.clone()));
    assert_eq!(app.status_message, "📻 Seed Radio: Initiating station from 'seed'...");
    gate.wait_entered();
    app.poll_folder_checks();
    assert_eq!(app.autopilot_dir, None, "Seed Radio waits for the check");
    assert_eq!(app.queue.len(), 1, "the queue is left alone until it starts");

    gate.open();
    wait_for_answers(&mut app, 1);
    assert_eq!(app.autopilot_dir, Some(music.clone()));
    app.reset_playback_session();

    // The web dashboard's `seed_radio` goes the same way.
    let (tx, rx) = mpsc::channel();
    app.web_cmd_rx = Some(rx);
    let gate = GatedCheck::install(&mut app, &music);
    tx.send(WebCommand::SeedRadio(seed.clone())).unwrap();
    app.handle_web_commands();
    gate.wait_entered();
    app.poll_folder_checks();
    assert_eq!(app.autopilot_dir, None);
    gate.open();
    wait_for_answers(&mut app, 2);
    assert_eq!(app.autopilot_dir, Some(music));
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
    assert_eq!(app.autopilot_dir, None, "nothing started before the poll");
    wait_for_answers(&mut app, 1);
    assert_eq!(app.autopilot_dir, Some(root.join("Other")));
    app.reset_playback_session();
    let _ = fs::remove_dir_all(&root);
}

/// Another Autopilot start, or the end of the session, before the answer drops it.
#[test]
fn test_pending_autopilot_check_is_superseded() {
    common::isolate();
    let (mut app, root) = setup("mix_superseded", None);
    let music = root.join("Music");
    let other = root.join("Other");

    let gate = GatedCheck::install(&mut app, &music);
    press(&mut app, KeyCode::Char('M'));
    press(&mut app, KeyCode::Enter);
    gate.wait_entered();
    app.start_autopilot(other.clone());
    assert!(!app.folder_check_pending());
    gate.open();
    wait_for_answers(&mut app, 1);
    assert_eq!(
        app.autopilot_dir,
        Some(other),
        "the late answer does not restart the mix"
    );
    app.reset_playback_session();

    let gate = GatedCheck::install(&mut app, &music);
    press(&mut app, KeyCode::Char('M'));
    press(&mut app, KeyCode::Enter);
    gate.wait_entered();
    app.reset_playback_session();
    gate.open();
    wait_for_answers(&mut app, 2);
    assert_eq!(app.autopilot_dir, None, "a reset session stays reset");
    assert!(!app.is_autopilot_scanning);
    let _ = fs::remove_dir_all(&root);
}
