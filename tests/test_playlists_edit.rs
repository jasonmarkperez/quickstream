//! Tab 5 editing: name prompt (create / rename), delete confirmation, moving and removing tracks.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use quickstream::paths;
use quickstream::playlist::{parse_m3u8, PlaylistEntry};
use quickstream::playlist_store::PlaylistStore;
use quickstream::settings::SettingsManager;
use quickstream::ui::playlists::{NamePurpose, PlaylistModal};
use quickstream::ui::AppUI;

/// A fresh app on tab 5 with a loaded, empty playlists store. Tests in this binary share one
/// isolated config dir, so the playlists folder is removed first.
fn app_on_playlists_tab(tag: &str) -> (AppUI, PathBuf) {
    let temp_dir = std::env::temp_dir().join(format!("quickstream_pl_edit_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    let _ = fs::remove_dir_all(paths::get_playlists_dir());

    let settings_mgr = SettingsManager::with_path(temp_dir.join("config.json"));
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);
    for _ in 0..200 {
        app.poll_playlists();
        if app.playlists.store.is_loaded() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(app.playlists.store.is_loaded(), "playlists did not load within 2s");
    app.active_tab = 4;
    (app, temp_dir)
}

/// Send one key; returns true when the app asked to quit.
fn press(app: &mut AppUI, code: KeyCode, modifiers: KeyModifiers) -> bool {
    app.handle_input_key(KeyEvent::new(code, modifiers)).unwrap()
}

/// Type `text`, sending upper-case letters with Shift as terminals do.
fn type_text(app: &mut AppUI, text: &str) {
    for c in text.chars() {
        let modifiers = if c.is_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        press(app, KeyCode::Char(c), modifiers);
    }
}

/// Create playlist `name` holding `tracks` directly through the store.
fn seed(app: &mut AppUI, name: &str, tracks: &[PathBuf]) {
    let stored = app.playlists.store.create(name).unwrap();
    let entries = tracks.iter().map(|p| PlaylistEntry::from_path(p.as_path())).collect();
    app.playlists.store.add(&stored, entries).unwrap();
}

fn playlist_names(app: &AppUI) -> Vec<String> {
    app.playlists.store.playlists().iter().map(|p| p.name.clone()).collect()
}

/// File names in the playlists folder, sorted.
fn playlist_files() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(paths::get_playlists_dir())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn render_view(app: &mut AppUI, width: u16, height: u16) -> String {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|f| app.render(f)).unwrap();
    terminal.backend().to_string()
}

#[test]
fn test_create_prompt_types_q_and_digits_and_selects_new_playlist() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("create");
    seed(&mut app, "Alpha", &[]);
    seed(&mut app, "Zulu", &[]);
    app.playlists.list_cursor = 0;

    press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    assert!(
        matches!(
            app.playlists.modal,
            Some(PlaylistModal::NamePrompt {
                purpose: NamePurpose::Create,
                ..
            })
        ),
        "c should open the name prompt"
    );

    // In the prompt, q, digits and M are typed as text. They do not quit, switch tabs or open the mix picker.
    type_text(&mut app, "Mq 2");
    assert_eq!(app.active_tab, 4);
    assert!(!app.show_mix_picker_modal);
    match &app.playlists.modal {
        Some(PlaylistModal::NamePrompt { input, error, .. }) => {
            assert_eq!(input, "Mq 2");
            assert!(error.is_none());
        }
        _ => panic!("the prompt closed while typing"),
    }

    assert!(!press(&mut app, KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        app.playlists.modal.is_none(),
        "Enter with a valid name closes the prompt"
    );
    assert_eq!(playlist_names(&app), vec!["Alpha", "Mq 2", "Zulu"]);
    assert_eq!(
        app.playlists.list_cursor, 1,
        "the cursor follows the new playlist to its sorted row"
    );
    assert_eq!(app.status_message, "Created \"Mq 2\"");

    assert!(app.persistence.flush());
    assert_eq!(playlist_files(), vec!["Alpha.m3u8", "Mq 2.m3u8", "Zulu.m3u8"]);

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_name_prompt_shows_errors_and_stays_open() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("errors");
    seed(&mut app, "Road Trip", &[]);

    press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    assert!(
        matches!(app.playlists.modal, Some(PlaylistModal::NamePrompt { .. })),
        "c should open the name prompt"
    );
    type_text(&mut app, "road trip");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    match &app.playlists.modal {
        Some(PlaylistModal::NamePrompt {
            input,
            error: Some(error),
            ..
        }) => {
            assert_eq!(input, "road trip", "the typed name is kept so it can be fixed");
            assert!(error.ends_with("already exists"), "unexpected error: {}", error);
        }
        _ => panic!("a duplicate name must keep the prompt open with an error"),
    }
    let view = render_view(&mut app, 100, 30);
    assert!(
        view.contains("already exists"),
        "the error is drawn in the prompt:\n{}",
        view
    );

    // Editing clears the error; a name starting with "." is rejected with its own message.
    for _ in 0.."road trip".len() {
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    }
    match &app.playlists.modal {
        Some(PlaylistModal::NamePrompt { input, error, .. }) => {
            assert!(input.is_empty());
            assert!(error.is_none());
        }
        _ => panic!("Backspace closed the prompt"),
    }
    type_text(&mut app, ".hidden");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    match &app.playlists.modal {
        Some(PlaylistModal::NamePrompt { error: Some(error), .. }) => {
            assert_eq!(error, "Name can't start with .")
        }
        _ => panic!("an invalid name must keep the prompt open"),
    }

    // Esc cancels without creating anything.
    assert!(!press(&mut app, KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.playlists.modal.is_none());
    assert_eq!(playlist_names(&app), vec!["Road Trip"]);
    assert_eq!(app.status_message, "Cancelled");

    // Ctrl+C still quits from inside the prompt.
    press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    assert!(press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL));

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_rename_prefills_current_name_and_cursor_follows() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("rename");
    seed(&mut app, "Alpha", &[temp_dir.join("a.flac")]);
    seed(&mut app, "Beta", &[]);
    seed(&mut app, "Charlie", &[]);
    app.playlists.list_cursor = 0;

    press(&mut app, KeyCode::Char('R'), KeyModifiers::SHIFT);
    match &app.playlists.modal {
        Some(PlaylistModal::NamePrompt {
            purpose: NamePurpose::Rename { old },
            input,
            error,
        }) => {
            assert_eq!(old, "Alpha");
            assert_eq!(input, "Alpha", "the prompt starts with the current name");
            assert!(error.is_none());
        }
        _ => panic!("R should open the rename prompt"),
    }

    for _ in 0.."Alpha".len() {
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    }
    type_text(&mut app, "Zed");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert!(app.playlists.modal.is_none());
    assert_eq!(playlist_names(&app), vec!["Beta", "Charlie", "Zed"]);
    assert_eq!(app.playlists.list_cursor, 2, "the cursor follows the renamed playlist");
    assert_eq!(app.playlists.store.get("Zed").unwrap().entries.len(), 1);
    assert_eq!(app.status_message, "Renamed \"Alpha\" to \"Zed\"");

    assert!(app.persistence.flush());
    assert_eq!(playlist_files(), vec!["Beta.m3u8", "Charlie.m3u8", "Zed.m3u8"]);

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_delete_asks_first_and_clamps_cursor() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("delete");
    seed(&mut app, "Alpha", &[]);
    seed(
        &mut app,
        "Road Trip",
        &[temp_dir.join("a.flac"), temp_dir.join("b.flac")],
    );
    seed(&mut app, "Zulu", &[]);
    app.playlists.list_cursor = 1;

    press(&mut app, KeyCode::Char('D'), KeyModifiers::SHIFT);
    match &app.playlists.modal {
        Some(PlaylistModal::ConfirmDelete { name, count }) => {
            assert_eq!(name, "Road Trip");
            assert_eq!(*count, 2);
        }
        _ => panic!("D should ask before deleting"),
    }
    let view = render_view(&mut app, 100, 30);
    assert!(
        view.contains("Delete \"Road Trip\" (2 tracks)? y/n"),
        "confirmation text:\n{}",
        view
    );

    press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    assert!(app.playlists.modal.is_none());
    assert_eq!(playlist_names(&app), vec!["Alpha", "Zulu"]);
    assert_eq!(
        app.playlists.list_cursor, 1,
        "the cursor stays on the same row (now Zulu)"
    );
    assert_eq!(app.status_message, "Deleted \"Road Trip\"");
    assert!(app.persistence.flush());
    assert_eq!(playlist_files(), vec!["Alpha.m3u8", "Zulu.m3u8"]);

    // Deleting the last row moves the cursor up to the new last row (Review Focus 5).
    press(&mut app, KeyCode::Char('D'), KeyModifiers::SHIFT);
    press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    assert_eq!(playlist_names(&app), vec!["Alpha"]);
    assert_eq!(app.playlists.list_cursor, 0);

    // A one-track playlist reads "1 track", not "1 tracks".
    seed(&mut app, "Solo", &[temp_dir.join("s.flac")]);
    app.playlists.list_cursor = 1;
    press(&mut app, KeyCode::Char('D'), KeyModifiers::SHIFT);
    let view = render_view(&mut app, 100, 30);
    assert!(
        view.contains("Delete \"Solo\" (1 track)? y/n"),
        "confirmation text:\n{}",
        view
    );
    press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    assert_eq!(playlist_names(&app), vec!["Alpha"]);
    assert_eq!(app.playlists.list_cursor, 0);

    // Deleting the only playlist leaves an empty list with the cursor at 0.
    press(&mut app, KeyCode::Char('D'), KeyModifiers::SHIFT);
    press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    assert!(playlist_names(&app).is_empty());
    assert_eq!(app.playlists.list_cursor, 0);

    // With nothing selected, D does nothing.
    press(&mut app, KeyCode::Char('D'), KeyModifiers::SHIFT);
    assert!(app.playlists.modal.is_none());
    assert!(app.persistence.flush());
    assert!(playlist_files().is_empty());

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_delete_is_cancelled_by_any_key_other_than_y() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("keep");
    seed(&mut app, "Keep Me", &[temp_dir.join("a.flac")]);
    app.playlists.list_cursor = 0;

    for (code, modifiers) in [
        (KeyCode::Char('n'), KeyModifiers::NONE),
        (KeyCode::Esc, KeyModifiers::NONE),
        (KeyCode::Char('q'), KeyModifiers::NONE),
        (KeyCode::Char('Y'), KeyModifiers::SHIFT),
    ] {
        press(&mut app, KeyCode::Char('D'), KeyModifiers::SHIFT);
        assert!(
            matches!(app.playlists.modal, Some(PlaylistModal::ConfirmDelete { .. })),
            "D should ask before deleting"
        );
        assert!(!press(&mut app, code, modifiers), "{:?} must not quit", code);
        assert!(app.playlists.modal.is_none(), "{:?} closes the confirmation", code);
        assert_eq!(playlist_names(&app), vec!["Keep Me"], "{:?} must not delete", code);
    }
    assert_eq!(app.status_message, "Kept \"Keep Me\"");
    assert!(app.persistence.flush());
    assert_eq!(playlist_files(), vec!["Keep Me.m3u8"]);

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_create_is_refused_while_playlists_load() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("loading");
    // A store that has not received the startup load yet.
    app.playlists.store = PlaylistStore::new(paths::get_playlists_dir(), app.persistence.handle());

    press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    assert!(app.playlists.modal.is_none(), "no prompt while loading");
    assert_eq!(app.status_message, "Playlists are still loading");

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

fn track_paths(app: &AppUI, name: &str) -> Vec<PathBuf> {
    app.playlists
        .store
        .get(name)
        .unwrap()
        .entries
        .iter()
        .map(|e| e.path.clone())
        .collect()
}

#[test]
fn test_shift_j_k_move_track_and_cursor_follows() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("move");
    let (a, b, c) = (
        temp_dir.join("a.flac"),
        temp_dir.join("b.flac"),
        temp_dir.join("c.flac"),
    );
    seed(&mut app, "Mix", &[a.clone(), b.clone(), c.clone()]);
    app.playlists.list_cursor = 0;
    app.playlists.tracks_focus = true;
    app.playlists.track_cursor = 0;

    press(&mut app, KeyCode::Char('J'), KeyModifiers::SHIFT);
    assert_eq!(
        track_paths(&app, "Mix"),
        vec![b.clone(), a.clone(), c.clone()],
        "J should move the track down"
    );
    assert_eq!(app.playlists.track_cursor, 1, "the cursor follows the moved track");

    press(&mut app, KeyCode::Char('J'), KeyModifiers::SHIFT);
    assert_eq!(track_paths(&app, "Mix"), vec![b.clone(), c.clone(), a.clone()]);
    assert_eq!(app.playlists.track_cursor, 2);

    // Moves do not wrap around.
    press(&mut app, KeyCode::Char('J'), KeyModifiers::SHIFT);
    assert_eq!(track_paths(&app, "Mix"), vec![b.clone(), c.clone(), a.clone()]);
    assert_eq!(app.playlists.track_cursor, 2);

    press(&mut app, KeyCode::Char('K'), KeyModifiers::SHIFT);
    assert_eq!(
        track_paths(&app, "Mix"),
        vec![b.clone(), a.clone(), c.clone()],
        "K should move the track up"
    );
    assert_eq!(app.playlists.track_cursor, 1);

    // The file on disk has the same order as memory.
    assert!(app.persistence.flush());
    let dir = paths::get_playlists_dir();
    let text = fs::read_to_string(dir.join("Mix.m3u8")).unwrap();
    let on_disk: Vec<PathBuf> = parse_m3u8("Mix", &text, &dir)
        .entries
        .into_iter()
        .map(|e| e.path)
        .collect();
    assert_eq!(on_disk, vec![b.clone(), a.clone(), c.clone()]);

    // In the playlists pane, J does not reorder tracks.
    app.playlists.tracks_focus = false;
    press(&mut app, KeyCode::Char('J'), KeyModifiers::SHIFT);
    assert_eq!(track_paths(&app, "Mix"), vec![b, a, c]);

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_d_x_delete_remove_tracks_and_clamp_cursor() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("remove");
    let (a, b, c) = (
        temp_dir.join("a.flac"),
        temp_dir.join("b.flac"),
        temp_dir.join("c.flac"),
    );
    seed(&mut app, "Mix", &[a.clone(), b.clone(), c]);
    app.playlists.list_cursor = 0;
    app.playlists.tracks_focus = true;
    app.playlists.track_cursor = 2;

    press(&mut app, KeyCode::Char('d'), KeyModifiers::NONE);
    assert_eq!(
        track_paths(&app, "Mix"),
        vec![a.clone(), b],
        "d should remove the selected track"
    );
    assert_eq!(app.playlists.track_cursor, 1, "the cursor moves up to the new last row");
    assert_eq!(app.status_message, "Removed \"c\" from Mix");

    press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
    assert_eq!(track_paths(&app, "Mix"), vec![a]);
    assert_eq!(app.playlists.track_cursor, 0);

    press(&mut app, KeyCode::Delete, KeyModifiers::NONE);
    assert!(track_paths(&app, "Mix").is_empty());
    assert_eq!(app.playlists.track_cursor, 0);

    // Removing from an empty playlist is a no-op.
    press(&mut app, KeyCode::Char('d'), KeyModifiers::NONE);
    assert!(track_paths(&app, "Mix").is_empty());

    // Removing every track keeps the (now empty) playlist file.
    assert!(app.persistence.flush());
    let dir = paths::get_playlists_dir();
    let text = fs::read_to_string(dir.join("Mix.m3u8")).unwrap();
    assert!(parse_m3u8("Mix", &text, &dir).entries.is_empty());
    assert_eq!(playlist_names(&app), vec!["Mix"]);

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_help_modal_lists_playlist_editing_keys() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("help");
    press(&mut app, KeyCode::Char('?'), KeyModifiers::NONE);
    assert!(app.show_help_modal);

    let view = render_view(&mut app, 120, 60);
    for text in ["c / R / D", "New / Rename / Delete", "Page (2) / Move track (5)"] {
        assert!(view.contains(text), "help should list {:?}:\n{}", text, view);
    }
    // Small terminals still render without panicking.
    render_view(&mut app, 70, 20);

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

/// Open the rename prompt on the selected playlist and submit `new` as its name.
fn rename_selected(app: &mut AppUI, old: &str, new: &str) {
    press(app, KeyCode::Char('R'), KeyModifiers::SHIFT);
    for _ in 0..old.chars().count() {
        press(app, KeyCode::Backspace, KeyModifiers::NONE);
    }
    type_text(app, new);
    press(app, KeyCode::Enter, KeyModifiers::NONE);
}

fn saved_last_playlist(temp_dir: &std::path::Path) -> Option<String> {
    SettingsManager::with_path(temp_dir.join("config.json"))
        .settings
        .last_playlist
}

#[test]
fn test_rename_moves_last_playlist_to_the_new_name() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("rename_last");
    seed(&mut app, "Alpha", &[]);
    seed(&mut app, "Beta", &[]);
    app.settings_mgr.settings.last_playlist = Some("alpha".to_string());
    app.playlists.list_cursor = 0;

    rename_selected(&mut app, "Alpha", "Zed");
    assert_eq!(app.settings_mgr.settings.last_playlist.as_deref(), Some("Zed"));
    assert!(app.persistence.flush());
    assert_eq!(saved_last_playlist(&temp_dir).as_deref(), Some("Zed"));

    // Renaming some other playlist leaves it alone.
    let beta = app.playlists.store.index_of("Beta").unwrap();
    app.playlists.list_cursor = beta;
    rename_selected(&mut app, "Beta", "Gamma");
    assert_eq!(app.settings_mgr.settings.last_playlist.as_deref(), Some("Zed"));

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_delete_clears_last_playlist() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("delete_last");
    seed(&mut app, "Alpha", &[]);
    seed(&mut app, "Beta", &[]);
    app.settings_mgr.settings.last_playlist = Some("Alpha".to_string());
    app.settings_mgr.save();

    // Deleting another playlist keeps it.
    app.playlists.list_cursor = 1;
    press(&mut app, KeyCode::Char('D'), KeyModifiers::SHIFT);
    press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    assert_eq!(app.settings_mgr.settings.last_playlist.as_deref(), Some("Alpha"));

    app.playlists.list_cursor = 0;
    press(&mut app, KeyCode::Char('D'), KeyModifiers::SHIFT);
    press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    assert_eq!(app.settings_mgr.settings.last_playlist, None);
    assert!(app.persistence.flush());
    assert_eq!(saved_last_playlist(&temp_dir), None);

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_rename_to_the_same_name_says_name_unchanged() {
    common::isolate();
    let (mut app, temp_dir) = app_on_playlists_tab("rename_same");
    seed(&mut app, "Alpha", &[]);
    app.playlists.list_cursor = 0;

    // Enter on the prefilled name submits it unchanged.
    press(&mut app, KeyCode::Char('R'), KeyModifiers::SHIFT);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert!(app.playlists.modal.is_none());
    assert_eq!(app.status_message, "Name unchanged");
    assert_eq!(playlist_names(&app), vec!["Alpha"]);

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_unlistable_playlists_folder_refuses_edits_and_keeps_files() {
    common::isolate();
    // Issue #56: a folder that exists but can't be listed must not load as empty, or creating
    // a playlist with an existing file's name would overwrite that file.
    let (mut app, temp_dir) = app_on_playlists_tab("unlistable");
    let dir = paths::get_playlists_dir();
    fs::create_dir_all(&dir).unwrap();
    let original = "#EXTM3U\n#PLAYLIST:bom\n/music/a.flac\n";
    fs::write(dir.join("bom.m3u8"), original).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o300)).unwrap();
    // Root lists any folder, so the test proves nothing there.
    let listable = fs::read_dir(&dir).is_ok();

    app.playlists.store = PlaylistStore::new(dir.clone(), app.persistence.handle());
    app.start_playlist_load();
    for _ in 0..200 {
        app.poll_playlists();
        if app.playlists.load_failed() || app.playlists.store.is_loaded() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let loaded = app.playlists.store.is_loaded();
    let failed = app.playlists.load_failed();
    press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    let prompt_opened = app.playlists.modal.is_some();
    let status = app.status_message.clone();
    let view = render_view(&mut app, 100, 30);
    let flushed = app.persistence.flush();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    if listable {
        drop(app);
        let _ = fs::remove_dir_all(&temp_dir);
        return;
    }

    assert!(flushed);
    assert!(!loaded, "store must stay not loaded");
    assert!(failed, "load must be marked failed");
    assert!(!prompt_opened, "no create prompt after a failed load");
    assert_eq!(status, "Couldn't load playlists");
    assert!(view.contains("Couldn't load playlists"), "{}", view);
    assert_eq!(fs::read_to_string(dir.join("bom.m3u8")).unwrap(), original);

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

/// Write a file the store never read into the playlists folder (issues #62, #52).
fn foreign_playlist_file(file: &str, text: &str) -> PathBuf {
    let dir = paths::get_playlists_dir();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(file);
    fs::write(&path, text).unwrap();
    path
}

/// Let the writer finish and the UI apply what it reported, as the next ticks would.
fn settle(app: &mut AppUI) {
    assert!(app.persistence.flush());
    app.poll_playlists();
}

#[test]
fn test_create_onto_an_unseen_file_shows_an_error_and_removes_the_playlist() {
    common::isolate();
    // Issue #62: a file the name check can't see must survive a create, and the UI must say so.
    let (mut app, temp_dir) = app_on_playlists_tab("create_collision");
    seed(&mut app, "Alpha", &[]);
    let theirs = "#EXTM3U\n#PLAYLIST:Mix\n/user/own.flac\n";
    let path = foreign_playlist_file("Mix.m3u8", theirs);

    press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    type_text(&mut app, "Mix");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.status_message, "Created \"Mix\"");
    app.settings_mgr.settings.last_playlist = Some("Mix".to_string());
    settle(&mut app);

    assert_eq!(
        app.status_message,
        "Couldn't create \"Mix\": a file with that name already exists"
    );
    assert_eq!(playlist_names(&app), vec!["Alpha"]);
    assert_eq!(app.playlists.list_cursor, 0);
    assert_eq!(app.settings_mgr.settings.last_playlist, None);
    assert_eq!(fs::read_to_string(&path).unwrap(), theirs);
    let view = render_view(&mut app, 100, 30);
    assert!(!view.contains("Mix  ("), "{}", view);

    // Trying again is refused in the prompt instead.
    press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    type_text(&mut app, "Mix");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        matches!(&app.playlists.modal, Some(PlaylistModal::NamePrompt { error: Some(e), .. }) if e == "A playlist file named \"Mix\" exists but couldn't be loaded"),
        "{:?}",
        app.playlists.modal
    );
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    settle(&mut app);
    assert_eq!(fs::read_to_string(&path).unwrap(), theirs);

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_failed_rename_shows_an_error_and_restores_the_name() {
    common::isolate();
    // Issue #52: a rename the writer couldn't do must not leave the new name in memory, or
    // later edits would write New.m3u8 next to Old.m3u8.
    let (mut app, temp_dir) = app_on_playlists_tab("rename_failure");
    let track = temp_dir.join("a.flac");
    seed(&mut app, "Beta", &[]);
    seed(&mut app, "Old", &[track]);
    settle(&mut app);
    let path = foreign_playlist_file("Able.m3u8", "theirs");
    app.settings_mgr.settings.last_playlist = Some("Old".to_string());
    app.playlists.list_cursor = app.playlists.store.index_of("Old").unwrap();

    rename_selected(&mut app, "Old", "Able");
    assert_eq!(playlist_names(&app), vec!["Able", "Beta"]);
    settle(&mut app);

    assert_eq!(
        app.status_message,
        "Couldn't rename \"Old\" to \"Able\": a file with that name already exists"
    );
    assert_eq!(playlist_names(&app), vec!["Beta", "Old"]);
    assert_eq!(
        app.playlists.selected_name().as_deref(),
        Some("Old"),
        "cursor stays on the playlist"
    );
    assert_eq!(app.settings_mgr.settings.last_playlist.as_deref(), Some("Old"));
    assert_eq!(fs::read_to_string(&path).unwrap(), "theirs");

    // Later edits go to the file that exists.
    app.playlists.tracks_focus = true;
    press(&mut app, KeyCode::Char('d'), KeyModifiers::NONE);
    settle(&mut app);
    assert_eq!(playlist_files(), vec!["Able.m3u8", "Beta.m3u8", "Old.m3u8"]);
    let old = fs::read_to_string(paths::get_playlists_dir().join("Old.m3u8")).unwrap();
    assert!(
        parse_m3u8("Old", &old, &paths::get_playlists_dir()).entries.is_empty(),
        "{}",
        old
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "theirs");

    drop(app);
    let _ = fs::remove_dir_all(&temp_dir);
}
