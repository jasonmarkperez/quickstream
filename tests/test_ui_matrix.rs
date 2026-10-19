//! The render matrix (plan, PR 6): every tab and every modal, drawn from one realistic app (a
//! playing queue of 12 tagged tracks, a browsed folder, renderers with one in use, settings and
//! three playlists, one with a missing file) at every size the Telemetry layouts are designed
//! for, plus two tiny ones drawn only to show they don't panic. Each case is checked for the
//! chrome's guarantees; a failing check names the tab or modal and the size, and every failure
//! of a run is listed together.

mod common;

use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use common::fake_player::FakePlayer;
use common::tui;
use quickstream::discovery::DLNADevice;
use quickstream::metadata::AudioMetadata;
use quickstream::paths;
use quickstream::playlist::{Playlist, PlaylistEntry};
use quickstream::settings::{setting, SettingsManager};
use quickstream::ui::playlists::PlaylistModal;
use quickstream::ui::search::SearchModalState;
use quickstream::ui::{theme, AppUI};

/// Sizes every check runs at: 2b-2f (120x40), the 90-109 and 80-89 tiers, 3f/3g (80x24), the
/// single-pane tier, a short terminal and a small one.
const SIZES: [(u16, u16); 7] = [(120, 40), (100, 30), (85, 30), (80, 24), (70, 24), (80, 20), (40, 12)];

/// Sizes drawn only to show nothing panics: too small for any guarantee.
const TINY: [(u16, u16); 2] = [(1, 1), (20, 8)];

/// What a case draws: a tab, or a modal (or the Settings home editor, which takes typed text
/// inline) over the tab it is opened from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    NowPlaying,
    QueueLibrary,
    /// Tab 2 with the QUEUE pane focused.
    QueuePane,
    Devices,
    Settings,
    Playlists,
    /// Tab 5 with the tracks pane focused.
    PlaylistTracks,
    Help,
    Search,
    MixPicker,
    NamePrompt,
    NamePromptError,
    ConfirmDelete,
    AddPicker,
    HomeEditor,
}

const VIEWS: [View; 15] = [
    View::NowPlaying,
    View::QueueLibrary,
    View::QueuePane,
    View::Devices,
    View::Settings,
    View::Playlists,
    View::PlaylistTracks,
    View::Help,
    View::Search,
    View::MixPicker,
    View::NamePrompt,
    View::NamePromptError,
    View::ConfirmDelete,
    View::AddPicker,
    View::HomeEditor,
];

impl View {
    /// The tab drawn (under the modal, for a modal).
    fn tab(self) -> usize {
        match self {
            View::NowPlaying | View::MixPicker => 0,
            View::QueueLibrary | View::QueuePane | View::Help | View::Search | View::AddPicker => 1,
            View::Devices => 2,
            View::Settings | View::HomeEditor => 3,
            View::Playlists | View::PlaylistTracks | View::NamePrompt | View::NamePromptError | View::ConfirmDelete => {
                4
            }
        }
    }

    /// Whether a modal is drawn over the (dimmed) tab.
    fn is_modal(self) -> bool {
        !matches!(
            self,
            View::NowPlaying
                | View::QueueLibrary
                | View::QueuePane
                | View::Devices
                | View::Settings
                | View::Playlists
                | View::PlaylistTracks
                | View::HomeEditor
        )
    }

    /// A key the key row must show (`modal_key_hints` for a modal, the editor's while it takes
    /// text, else the tab's `? KEYS`), as keycap and label read on screen (each modal's Esc in
    /// its footer's words: help `? ESC  CLOSE`, the others `ESC  CANCEL`, as in 2g-2i, 3b-3e).
    fn key(self) -> &'static str {
        match self {
            View::Help => "ESC  CLOSE",
            View::Search | View::MixPicker | View::NamePrompt | View::NamePromptError | View::AddPicker => {
                "ESC  CANCEL"
            }
            View::ConfirmDelete => "Y  DELETE",
            View::HomeEditor => "ENTER  SAVE",
            _ => "?  KEYS",
        }
    }
}

const TITLES: [&str; 12] = [
    "Tidewater",
    "Glass Harbour",
    "Sodium Light",
    "Halcyon Drift",
    "Afterimage",
    "Northbound, Slowly",
    "Salt Lines",
    "Paper Moons",
    "Cold Sun",
    "Lanterns",
    "Undertow Hymn",
    "Low Tide Atlas",
];

const ARTISTS: [&str; 4] = ["Mira Lenz", "Oren Vasquez", "The Quiet Ward", "Ada Kowalczyk"];

fn file_name(i: usize) -> String {
    format!("{:02} {}.flac", i + 1, TITLES[i].replace(',', ""))
}

fn track(dir: &Path, i: usize) -> AudioMetadata {
    let lossy = i % 4 == 2;
    AudioMetadata {
        filepath: dir.join(file_name(i)),
        filename: file_name(i),
        title: TITLES[i].to_string(),
        artist: ARTISTS[i % ARTISTS.len()].to_string(),
        album: "Low Tide Atlas".to_string(),
        genre: "Ambient".to_string(),
        format_name: if lossy { "mp3" } else { "flac" }.to_string(),
        duration_seconds: 180.0 + 23.0 * i as f64,
        file_size: 48_200_000,
        bitrate_kbps: if lossy { 320 } else { 2304 },
        sample_rate_hz: if lossy { 44_100 } else { 96_000 },
        channels: 2,
    }
}

fn renderer(i: usize, name: &str) -> DLNADevice {
    let host = format!("192.168.1.{}", 20 + i);
    DLNADevice {
        friendly_name: name.to_string(),
        location: format!("http://{host}:49152/description.xml"),
        host: host.clone(),
        port: 49152,
        manufacturer: "Maker".to_string(),
        model_name: "Model".to_string(),
        av_transport_control_url: format!("http://{host}:49152/AVTransport/control"),
        rendering_control_url: format!("http://{host}:49152/RenderingControl/control"),
        udn: format!("uuid:matrix-renderer-{i}"),
    }
}

/// Write a playlist of the folder's tracks `range` straight into the playlists folder.
fn write_playlist(name: &str, dir: &Path, range: std::ops::Range<usize>) {
    let playlists = paths::get_playlists_dir();
    fs::create_dir_all(&playlists).unwrap();
    let playlist = Playlist {
        name: name.to_string(),
        entries: range
            .map(|i| {
                let t = track(dir, i);
                PlaylistEntry {
                    path: t.filepath,
                    duration_secs: t.duration_seconds as i64,
                    artist: t.artist,
                    title: t.title,
                }
            })
            .collect(),
    };
    fs::write(playlists.join(format!("{name}.m3u8")), playlist.to_m3u8()).unwrap();
}

fn press(app: &mut AppUI, code: KeyCode) {
    let _ = app.handle_input_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn type_text(app: &mut AppUI, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}

/// The app every case draws: browsing a folder of two subfolders and the 12 tracks, all queued
/// with tags and the fourth playing, three renderers with the second in use, and three
/// playlists (`Late Drives`, first and selected, lists a file that is missing).
fn matrix_app() -> (AppUI, PathBuf) {
    let dir = std::env::temp_dir().join(format!("quickstream_matrix_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    for sub in ["Low Tide Atlas", "Sea Glass EP"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    for i in 0..TITLES.len() {
        fs::write(dir.join(file_name(i)), b"").unwrap();
    }
    write_playlist("Late Drives", &dir, 4..9);
    write_playlist("Road Trip", &dir, 0..12);
    write_playlist("Sunday Slow", &dir, 9..12);

    let settings = SettingsManager::with_path(dir.join("config.json"));
    let mut app = AppUI::new(Some(dir.clone()), settings);
    // Every listed file exists except Salt Lines.
    let missing = dir.join(file_name(6));
    app.playlists.set_exists_checker(std::sync::Arc::new(move |p: &Path| {
        (p != missing).then(|| p.to_path_buf())
    }));
    let start = Instant::now();
    while app.browser_loading || !app.playlists.store.is_loaded() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "folder or playlists did not load"
        );
        app.poll_background_dir_scan();
        app.poll_playlists();
        std::thread::sleep(Duration::from_millis(5));
    }

    app.queue
        .add_tracks((0..TITLES.len()).map(|i| track(&dir, i)).collect());
    app.queue.set_current_index(3);
    app.player = Box::new(FakePlayer {
        volume: 65,
        backend: Some("mpv"),
        ..FakePlayer::playing_at(161.0, track(&dir, 3).duration_seconds)
    });
    let devices: Vec<DLNADevice> = ["Living Room", "Kitchen Speaker", "Study Amp"]
        .iter()
        .enumerate()
        .map(|(i, name)| renderer(i, name))
        .collect();
    app.device_manager.set_active_device(Some(devices[1].clone()));
    *app.device_manager.devices.lock().unwrap() = devices;
    app.devices_cursor = 2;
    app.settings_cursor = setting::SEEK_STEP_SECONDS;

    // Draw tab 5 until its shown playlist's files are checked, so the missing row reads so.
    app.active_tab = 4;
    let start = Instant::now();
    while !app.playlists.is_missing(&dir.join(file_name(6))) {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "playlist files were not checked"
        );
        tui::render(&mut app, 120, 40);
        app.poll_playlists();
        std::thread::sleep(Duration::from_millis(5));
    }
    app.active_tab = 1;
    app.queue_focus = false;
    // The library cursor on the first file (after Music Homes, `..` and the two subfolders).
    for _ in 0..4 {
        press(&mut app, KeyCode::Down);
    }
    (app, dir)
}

/// Open `view` the way a user would, from its tab.
fn open(app: &mut AppUI, view: View, dir: &Path) {
    app.active_tab = view.tab();
    app.queue_focus = false;
    app.playlists.tracks_focus = false;
    match view {
        View::QueuePane => app.queue_focus = true,
        View::PlaylistTracks => app.playlists.tracks_focus = true,
        View::Help => press(app, KeyCode::Char('?')),
        View::Search => {
            let tracks = (0..TITLES.len()).map(|i| track(dir, i)).collect();
            let mut search = SearchModalState::new(dir.to_path_buf(), tracks);
            search.update_query("la".to_string());
            assert!(search.filtered.len() > 1, "search has results");
            app.search_modal = Some(search);
        }
        View::MixPicker => press(app, KeyCode::Char('M')),
        View::NamePrompt => {
            press(app, KeyCode::Char('c'));
            type_text(app, "Night Bus");
        }
        View::NamePromptError => {
            press(app, KeyCode::Char('c'));
            type_text(app, "road trip");
            press(app, KeyCode::Enter);
        }
        View::ConfirmDelete => press(app, KeyCode::Char('D')),
        View::AddPicker => press(app, KeyCode::Char('L')),
        View::HomeEditor => {
            app.settings_cursor = setting::HOME_DIR;
            press(app, KeyCode::Enter);
            type_text(app, "~/Music/Library");
        }
        _ => {}
    }
    let opened = match view {
        View::Help => app.show_help_modal,
        View::Search => app.search_modal.is_some(),
        View::MixPicker => app.show_mix_picker_modal,
        View::NamePrompt | View::ConfirmDelete | View::AddPicker => app.playlists.modal.is_some(),
        View::NamePromptError => matches!(
            &app.playlists.modal,
            Some(PlaylistModal::NamePrompt { error: Some(e), .. }) if e.contains("already exists")
        ),
        View::HomeEditor => app.editing_home_dir.is_some(),
        _ => true,
    };
    assert!(opened, "{view:?} did not open (status {:?})", app.status_message);
}

fn close(app: &mut AppUI) {
    app.show_help_modal = false;
    app.show_mix_picker_modal = false;
    if let Some(mut search) = app.search_modal.take() {
        search.close();
    }
    app.playlists.modal = None;
    app.editing_home_dir = None;
    app.settings_cursor = setting::SEEK_STEP_SECONDS;
    app.status_message.clear();
}

/// The active tab's label at `width` (`tab_titles`: the full set from 110 columns, `QUEUE` for
/// tab 2 from 90, the short set under 90).
fn tab_label(tab: usize, width: u16) -> &'static str {
    match (tab, width) {
        (0, 90..) => "NOW PLAYING",
        (0, _) => "NOW",
        (1, 110..) => "QUEUE & LIBRARY",
        (1, _) => "QUEUE",
        (2, _) => "DEVICES",
        (3, _) => "SETTINGS",
        _ => "PLAYLISTS",
    }
}

/// The runs of reverse-video cells (INK background) on row `y`, as text.
fn chips(buffer: &Buffer, y: u16) -> Vec<String> {
    let ink = theme::palette().ink;
    let mut runs = Vec::new();
    let mut run: Option<String> = None;
    for x in 0..buffer.area.width {
        let cell = &buffer[(x, y)];
        if cell.bg == ink {
            run.get_or_insert_with(String::new).push_str(cell.symbol());
        } else if let Some(text) = run.take() {
            runs.push(text);
        }
    }
    runs.extend(run);
    runs
}

/// The modal's outer rectangle: from its top-left `┌` to the `┘` closing it.
fn modal_rect(buffer: &Buffer) -> Option<Rect> {
    let (x, y) = tui::find_text(buffer, "┌")?;
    let right = (x + 1..buffer.area.width).find(|&cx| buffer[(cx, y)].symbol() == "┐")?;
    let bottom = (y + 1..buffer.area.height).find(|&cy| buffer[(x, cy)].symbol() == "└")?;
    (buffer[(right, bottom)].symbol() == "┘").then(|| Rect::new(x, y, right - x + 1, bottom - y + 1))
}

/// Every guarantee `view` keeps at the buffer's size; the problems found, if any.
fn check(buffer: &Buffer, view: View) -> Vec<String> {
    let p = theme::palette();
    let (w, h) = (buffer.area.width, buffer.area.height);
    let mut problems = Vec::new();

    // The tab row (`ChromeLayout`): under the header from 24 rows, the first row below that.
    let tabs = if h < 24 { 0 } else { 3 };
    // The key row: the last row under 24 rows, else one above the bottom margin.
    let keys = if h < 24 { h - 1 } else { h - 2 };
    let rect = if view.is_modal() {
        let rect = modal_rect(buffer);
        if rect.is_none() {
            problems.push("no modal frame".to_string());
        }
        rect
    } else {
        None
    };
    let covers = |x: u16, y: u16| rect.is_some_and(|r| r.contains((x, y).into()));

    // Every cell on GROUND, or on INK (a reverse-video chip or cursor bar) or RULE (a keycap);
    // the delete confirmation's chip and `Y` keycap are on ERROR. Never the terminal's default.
    let mut backgrounds = vec![p.ground, p.ink, p.rule];
    if view == View::ConfirmDelete {
        backgrounds.push(p.error);
    }
    if let Some((x, y)) = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .find(|&(x, y)| !backgrounds.contains(&buffer[(x, y)].bg))
    {
        problems.push(format!("cell ({x}, {y}) on {:?}", buffer[(x, y)].bg));
    }

    // Nothing in the 2-column gutters: blank cells on GROUND. A cursor bar starts one column
    // before its text, so its blank leading cell may sit in the inner left gutter column (2c,
    // 3g draw it there). The help modal at 80-83 columns is the documented exception (it takes
    // 80 columns so every description is whole).
    let help_full_width = view == View::Help && (80..84).contains(&w);
    let blank = |x: u16, y: u16| {
        let cell = &buffer[(x, y)];
        let bar_pad = x == 1 && cell.bg == p.ink && buffer[(2, y)].bg == p.ink;
        cell.symbol() == " " && (cell.bg == p.ground || bar_pad)
    };
    if !help_full_width {
        for y in 0..h {
            if let Some(x) = [0, 1, w - 2, w - 1].into_iter().find(|&x| !blank(x, y)) {
                problems.push(format!("({x}, {y}) drawn in the gutter"));
                break;
            }
        }
    }

    // The active tab, whole, as the one chip on the tab row (where no modal dims or covers it).
    if !view.is_modal() {
        let tab = view.tab();
        let wanted = format!(" {} {} ", tab + 1, tab_label(tab, w));
        let found = chips(buffer, tabs);
        if found != [wanted.clone()] {
            problems.push(format!("tab row chips {found:?}, wanted [{wanted:?}]"));
        }
    }

    // The key row (unless the modal covers it): its key, and `? KEYS` only on a tab.
    if !covers(2, keys) {
        let row = tui::row(buffer, keys);
        if !row.contains(view.key()) {
            problems.push(format!("key row {row:?} without {:?}", view.key()));
        }
        if view.key() != "?  KEYS" && row.contains("?  KEYS") {
            problems.push(format!("key row {row:?} offers ? KEYS"));
        }
    }

    // Under a modal everything outside it is dimmed (RULE on GROUND, no modifiers), except the
    // key row, which shows the modal's keys.
    if let Some(r) = rect {
        if let Some((x, y)) = (0..h)
            .filter(|&y| y != keys)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .filter(|&(x, y)| !r.contains((x, y).into()))
            .find(|&(x, y)| {
                let cell = &buffer[(x, y)];
                cell.fg != p.rule || cell.bg != p.ground || !cell.modifier.is_empty()
            })
        {
            problems.push(format!("({x}, {y}) outside the modal not dimmed"));
        }
        if view == View::Help {
            problems.extend(orphaned_help_labels(buffer, r));
        }
    }
    problems
}

/// The help's section labels, as far as a label cut to its column (`NAVIGATION & TA…`) still
/// reads.
const HELP_SECTIONS: [&str; 5] = ["PLAYBACK", "NAVIGATI", "PLAYLIST", "LIBRARY ", "SEARCH &"];

/// Help section labels inside `modal` with no key under them: a column cut short by the height
/// must end on a key, not on the label of a section none of whose keys fit.
fn orphaned_help_labels(buffer: &Buffer, modal: Rect) -> Vec<String> {
    let rule = theme::palette().rule;
    let mut problems = Vec::new();
    for y in modal.top() + 1..modal.bottom() - 1 {
        let row: Vec<String> = (modal.left()..modal.right())
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect();
        for label in HELP_SECTIONS {
            let wanted: Vec<String> = label.chars().map(|c| c.to_string()).collect();
            let Some(at) =
                (0..row.len().saturating_sub(wanted.len())).find(|&i| row[i..i + wanted.len()] == wanted[..])
            else {
                continue;
            };
            let x = modal.left() + at as u16;
            // The first key: on the next row, or under the label's rule in the full layout, and
            // above the footer (the last row inside the border, whose `? ESC` is a keycap too).
            let below = |cy: u16| (x..(x + 10).min(modal.right() - 1)).any(|cx| buffer[(cx, cy)].bg == rule);
            let ruled = buffer[(x, y + 1)].symbol() == "─";
            let first_key = if ruled { y + 2 } else { y + 1 };
            if first_key >= modal.bottom() - 2 || !below(first_key) {
                problems.push(format!("help label {label:?} at ({x}, {y}) has no key under it"));
            }
        }
    }
    problems
}

/// Draw `app` at `w` x `h`, or the panic's message.
fn try_render(app: &mut AppUI, w: u16, h: u16) -> Result<Buffer, String> {
    panic::catch_unwind(AssertUnwindSafe(|| tui::render(app, w, h))).map_err(|e| {
        e.downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default()
    })
}

#[test]
fn every_tab_and_modal_keeps_the_chrome_at_every_size() {
    common::isolate();
    let (mut app, dir) = matrix_app();
    let mut failures = Vec::new();
    for view in VIEWS {
        open(&mut app, view, &dir);
        for (w, h) in SIZES.into_iter().chain(TINY) {
            let case = format!("{view:?} at {w}x{h}");
            match try_render(&mut app, w, h) {
                Err(message) => failures.push(format!("{case}: panicked: {message}")),
                Ok(_) if TINY.contains(&(w, h)) => {}
                Ok(buffer) => {
                    let problems = check(&buffer, view);
                    if !problems.is_empty() {
                        failures.push(format!(
                            "{case}:\n  {}\n{}",
                            problems.join("\n  "),
                            tui::rows(&buffer).join("\n")
                        ));
                    }
                }
            }
        }
        close(&mut app);
    }
    let _ = fs::remove_dir_all(&dir);
    assert!(
        failures.is_empty(),
        "{} failing cases:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
