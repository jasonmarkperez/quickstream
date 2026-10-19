//! Settings model behind tab `[4]` (Telemetry mockup 2e): the PLAYBACK / LIBRARY / WEB groups and
//! their display order, the choices each setting cycles through (`options`, shared with
//! `cycle_setting`), the compact values, bookmarks taken from memory instead of bookmarks.json,
//! j/k following the display order, and a LAN address that is never looked up while drawing.

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::KeyCode;
use quickstream::settings::{
    display_position, music_dir, step_in_display_order, SettingGroup, SettingValue, SettingsManager,
    SETTING_DEFINITIONS, SETTING_DISPLAY_ORDER,
};
use quickstream::ui::AppUI;

/// A settings manager saving to its own file in the isolated config dir.
fn manager(name: &str) -> SettingsManager {
    let root = common::isolate();
    SettingsManager::with_path(root.join(format!("settings_model_{}.json", name)))
}

fn test_app(name: &str) -> AppUI {
    let mut mgr = manager(name);
    mgr.settings.web_server_enabled = false;
    AppUI::new(Some(std::env::temp_dir()), mgr)
}

fn index_of(key: &str) -> usize {
    SETTING_DEFINITIONS
        .iter()
        .position(|d| d.key == key)
        .unwrap_or_else(|| panic!("no setting {}", key))
}

fn wait_for(cond: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    cond()
}

// ---------- groups and display order ----------

#[test]
fn display_order_lists_every_setting_once() {
    common::isolate();
    let mut sorted = SETTING_DISPLAY_ORDER.to_vec();
    sorted.sort_unstable();
    assert_eq!(sorted, (0..SETTING_DEFINITIONS.len()).collect::<Vec<_>>());
    for (pos, &index) in SETTING_DISPLAY_ORDER.iter().enumerate() {
        assert_eq!(display_position(index), Some(pos));
    }
    assert_eq!(display_position(SETTING_DEFINITIONS.len()), None);
}

#[test]
fn display_order_groups_settings_as_mockup_2e() {
    common::isolate();
    let keys_of = |group: SettingGroup| -> Vec<&str> {
        SETTING_DISPLAY_ORDER
            .iter()
            .filter(|&&i| SETTING_DEFINITIONS[i].group == group)
            .map(|&i| SETTING_DEFINITIONS[i].key)
            .collect()
    };
    assert_eq!(
        keys_of(SettingGroup::Playback),
        [
            "player_backend",
            "seek_step_seconds",
            "volume_step",
            "queue_mode",
            "gapless_autopilot",
            "force_transcode"
        ]
    );
    assert_eq!(
        keys_of(SettingGroup::Library),
        ["metadata_view", "home_dir", "default_mix_profile", "online_enrichment"]
    );
    assert_eq!(keys_of(SettingGroup::Web), ["web_server_enabled", "web_server_lan"]);

    // Each group is one run in the display order, in PLAYBACK, LIBRARY, WEB order.
    let groups: Vec<SettingGroup> = SETTING_DISPLAY_ORDER
        .iter()
        .map(|&i| SETTING_DEFINITIONS[i].group)
        .collect();
    let mut runs = groups.clone();
    runs.dedup();
    assert_eq!(runs, [SettingGroup::Playback, SettingGroup::Library, SettingGroup::Web]);
    assert_eq!(
        runs.iter().map(|g| g.label()).collect::<Vec<_>>(),
        ["PLAYBACK", "LIBRARY", "WEB"]
    );
}

#[test]
fn step_in_display_order_wraps_both_ways() {
    common::isolate();
    let n = SETTING_DISPLAY_ORDER.len();
    for pos in 0..n {
        let index = SETTING_DISPLAY_ORDER[pos];
        assert_eq!(step_in_display_order(index, true), SETTING_DISPLAY_ORDER[(pos + 1) % n]);
        assert_eq!(
            step_in_display_order(index, false),
            SETTING_DISPLAY_ORDER[(pos + n - 1) % n]
        );
    }
}

// ---------- options and cycle_setting ----------

#[test]
fn options_match_cycle_setting_for_every_index() {
    common::isolate();
    let bookmarks = vec!["/qs-bookmark/one".to_string(), "/qs-bookmark/two".to_string()];
    for (index, definition) in SETTING_DEFINITIONS.iter().enumerate() {
        for forward in [true, false] {
            let mut mgr = manager(&format!("cycle_{}_{}", index, forward));
            let options = mgr.options(index, &bookmarks, music_dir());
            let n = options.choices.len();
            assert!(n >= 2, "setting {} has {} choices", index, n);
            let start = options.current.expect("a default setting is one of its choices");
            assert_eq!(Some(&options.choices[start].value), mgr.value(index).as_ref());
            assert_eq!(mgr.short_value(index), options.choices[start].label);

            // Cycling visits the choices in order and comes back round to the start.
            for step in 1..=n {
                mgr.cycle_setting(index, forward, &bookmarks);
                let expected = if forward {
                    (start + step) % n
                } else {
                    (start + n - step) % n
                };
                let label = definition.key;
                assert_eq!(
                    mgr.value(index).as_ref(),
                    Some(&options.choices[expected].value),
                    "{} step {} forward={}",
                    label,
                    step,
                    forward
                );
                let now = mgr.options(index, &bookmarks, music_dir());
                assert_eq!(now.current, Some(expected), "{} step {}", label, step);
                assert_eq!(mgr.short_value(index), options.choices[expected].label);
            }
        }
    }
}

#[test]
fn short_values_read_as_mockup_2e() {
    common::isolate();
    let mut mgr = manager("short");
    let s = |mgr: &SettingsManager, key: &str| mgr.short_value(index_of(key));
    assert_eq!(s(&mgr, "player_backend"), "MPV");
    assert_eq!(s(&mgr, "seek_step_seconds"), "10 S");
    assert_eq!(s(&mgr, "volume_step"), "5 %");
    assert_eq!(s(&mgr, "queue_mode"), "CONTINUOUS (LOOP)");
    assert_eq!(s(&mgr, "gapless_autopilot"), "ON");
    assert_eq!(s(&mgr, "force_transcode"), "OFF");
    assert_eq!(s(&mgr, "metadata_view"), "TAGS");
    assert_eq!(s(&mgr, "online_enrichment"), "OFF");
    assert_eq!(s(&mgr, "web_server_enabled"), "OFF");
    assert_eq!(s(&mgr, "web_server_lan"), "THIS COMPUTER");
    assert_eq!(s(&mgr, "default_mix_profile"), "DIVERSE SHUFFLE");

    mgr.settings.web_server_enabled = true;
    // The port the server bound (no server runs here).
    mgr.set_bound_web_port(Some(8080));
    assert_eq!(s(&mgr, "web_server_enabled"), "ON  :8080");
    mgr.settings.queue_mode = "repeat_one".to_string();
    assert_eq!(s(&mgr, "queue_mode"), "REPEAT TRACK");
    mgr.settings.default_mix_profile = "bpm_flow".to_string();
    assert_eq!(s(&mgr, "default_mix_profile"), "BPM FLOW");

    // Paths keep their own case, with ~ for home.
    let home = directories::BaseDirs::new().unwrap().home_dir().to_path_buf();
    mgr.settings.home_dir = Some(home.join("Music").to_string_lossy().into_owned());
    assert_eq!(s(&mgr, "home_dir"), "~/Music");
    mgr.settings.home_dir = None;
    assert_eq!(s(&mgr, "home_dir"), "LAUNCH DIRECTORY");

    let queue = mgr.options(index_of("queue_mode"), &[], None);
    let labels: Vec<&str> = queue.choices.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, ["CONTINUOUS (LOOP)", "STOP AT END", "REPEAT TRACK"]);
}

#[test]
fn display_values_keep_their_wording() {
    common::isolate();
    let mut mgr = manager("wording");
    assert_eq!(mgr.get_display_value(index_of("queue_mode")), "Continuous (Loop Queue)");
    assert_eq!(mgr.get_display_value(index_of("seek_step_seconds")), "10 seconds");
    assert_eq!(
        mgr.get_display_value(index_of("default_mix_profile")),
        "Diverse Library Shuffle"
    );
    mgr.settings.web_server_enabled = true;
    mgr.set_bound_web_port(Some(8123));
    assert_eq!(
        mgr.get_display_value(index_of("web_server_enabled")),
        "Enabled (http://127.0.0.1:8123)"
    );
}

#[test]
fn values_outside_the_choices_cycle_from_where_they_did_before() {
    common::isolate();
    let mut mgr = manager("hand_edited");
    // A hand-edited config.json: no choice is current, cycling starts from the old fallback.
    mgr.settings.seek_step_seconds = 7.0;
    let seek = index_of("seek_step_seconds");
    assert_eq!(mgr.options(seek, &[], None).current, None);
    mgr.cycle_setting(seek, true, &[]);
    assert_eq!(mgr.settings.seek_step_seconds, 15.0);

    mgr.settings.player_backend = "vlc".to_string();
    mgr.cycle_setting(index_of("player_backend"), true, &[]);
    assert_eq!(mgr.settings.player_backend, "mpv");

    mgr.settings.volume_step = 7;
    mgr.cycle_setting(index_of("volume_step"), false, &[]);
    assert_eq!(mgr.settings.volume_step, 2);

    mgr.settings.queue_mode = "shuffle".to_string();
    mgr.cycle_setting(index_of("queue_mode"), true, &[]);
    assert_eq!(mgr.settings.queue_mode, "stop");

    mgr.settings.default_mix_profile = "focus".to_string();
    mgr.cycle_setting(index_of("default_mix_profile"), true, &[]);
    assert_eq!(mgr.settings.default_mix_profile, "bpm_flow");
}

#[test]
fn home_dir_options_list_launch_music_home_bookmarks_then_custom() {
    common::isolate();
    let mut mgr = manager("home_options");
    let custom = "/qs-custom/home".to_string();
    mgr.settings.home_dir = Some(custom.clone());
    let music = PathBuf::from("/qs-music");
    let bookmarks = vec!["/qs-bm/a".to_string(), "/qs-music".to_string()];
    let home = directories::BaseDirs::new()
        .unwrap()
        .home_dir()
        .to_string_lossy()
        .into_owned();
    let options = mgr.options(index_of("home_dir"), &bookmarks, Some(music.as_path()));
    let values: Vec<SettingValue> = options.choices.iter().map(|c| c.value.clone()).collect();
    assert_eq!(
        values,
        [
            SettingValue::Path(None),
            SettingValue::Path(Some("/qs-music".to_string())),
            SettingValue::Path(Some(home)),
            SettingValue::Path(Some("/qs-bm/a".to_string())),
            SettingValue::Path(Some(custom)),
        ],
        "duplicates are listed once"
    );
    assert_eq!(options.current, Some(4));
}

#[test]
fn home_dir_options_take_bookmarks_from_the_argument_not_the_file() {
    common::isolate();
    let file = quickstream::paths::get_bookmarks_path();
    std::fs::write(&file, r#"["/qs-from-file/bookmark"]"#).unwrap();

    let mut mgr = manager("bookmarks_arg");
    let index = index_of("home_dir");
    let bookmarks = vec!["/qs-from-memory/bookmark".to_string()];
    let options = mgr.options(index, &bookmarks, None);
    let has = |path: &str| {
        options
            .choices
            .iter()
            .any(|c| c.value == SettingValue::Path(Some(path.to_string())))
    };
    assert!(has("/qs-from-memory/bookmark"));
    assert!(!has("/qs-from-file/bookmark"), "bookmarks.json must not be read");

    let mut seen = Vec::new();
    for _ in 0..options.choices.len() {
        mgr.cycle_setting(index, true, &bookmarks);
        seen.push(mgr.settings.home_dir.clone());
    }
    assert!(seen.contains(&Some("/qs-from-memory/bookmark".to_string())));
    assert!(!seen.contains(&Some("/qs-from-file/bookmark".to_string())));
    std::fs::write(&file, "[]").unwrap();
}

#[test]
fn settings_tab_cycles_home_dir_through_the_bookmarks_in_memory() {
    common::isolate();
    let mut app = test_app("ui_bookmarks");
    // Written after startup, so only a read during the key press could pick it up.
    let file = quickstream::paths::get_bookmarks_path();
    std::fs::write(&file, r#"["/qs-ui-from-file"]"#).unwrap();
    app.bookmarks = vec!["/qs-ui-from-memory".to_string()];
    app.active_tab = 3;
    app.settings_cursor = index_of("home_dir");

    let mut seen = Vec::new();
    for _ in 0..8 {
        app.handle_tab_key(KeyCode::Right);
        seen.push(app.settings_mgr.settings.home_dir.clone());
    }
    assert!(seen.contains(&Some("/qs-ui-from-memory".to_string())), "{:?}", seen);
    assert!(!seen.contains(&Some("/qs-ui-from-file".to_string())), "{:?}", seen);
    std::fs::write(&file, "[]").unwrap();
}

// ---------- navigation ----------

#[test]
fn settings_navigation_follows_the_display_order_and_wraps() {
    common::isolate();
    let mut app = test_app("nav");
    app.active_tab = 3;
    let n = SETTING_DISPLAY_ORDER.len();
    app.settings_cursor = SETTING_DISPLAY_ORDER[0];

    for key in [KeyCode::Char('j'), KeyCode::Down] {
        let mut visited = Vec::new();
        for _ in 0..n {
            app.handle_tab_key(key);
            visited.push(app.settings_cursor);
        }
        let mut expected: Vec<usize> = SETTING_DISPLAY_ORDER[1..].to_vec();
        expected.push(SETTING_DISPLAY_ORDER[0]);
        assert_eq!(visited, expected, "{:?}", key);
    }

    for key in [KeyCode::Char('k'), KeyCode::Up] {
        let mut visited = Vec::new();
        for _ in 0..n {
            app.handle_tab_key(key);
            visited.push(app.settings_cursor);
        }
        let expected: Vec<usize> = SETTING_DISPLAY_ORDER.iter().rev().copied().collect();
        assert_eq!(
            visited.first(),
            Some(&SETTING_DISPLAY_ORDER[n - 1]),
            "{:?} wraps up",
            key
        );
        assert_eq!(visited.last(), Some(&SETTING_DISPLAY_ORDER[0]), "{:?}", key);
        assert_eq!(visited, expected, "{:?}", key);
    }
}

#[test]
fn settings_first_and_last_keys_jump_in_display_order() {
    common::isolate();
    let mut app = test_app("nav_ends");
    app.active_tab = 3;
    let first = SETTING_DISPLAY_ORDER[0];
    let last = *SETTING_DISPLAY_ORDER.last().unwrap();
    for key in [KeyCode::Char('G'), KeyCode::End, KeyCode::PageDown] {
        app.settings_cursor = first;
        app.handle_tab_key(key);
        assert_eq!(app.settings_cursor, last, "{:?}", key);
    }
    for key in [KeyCode::Char('g'), KeyCode::Home, KeyCode::PageUp] {
        app.settings_cursor = last;
        app.handle_tab_key(key);
        assert_eq!(app.settings_cursor, first, "{:?}", key);
    }
}

// ---------- LAN address ----------

/// A resolver that counts its calls, records the thread each ran on, and blocks until released.
struct GatedResolver {
    calls: Arc<AtomicUsize>,
    threads: Arc<Mutex<Vec<std::thread::ThreadId>>>,
    release: mpsc::Sender<()>,
}

fn gated_resolver(mgr: &mut SettingsManager, ip: &'static str) -> GatedResolver {
    let calls = Arc::new(AtomicUsize::new(0));
    let threads = Arc::new(Mutex::new(Vec::new()));
    let (release, gate) = mpsc::channel::<()>();
    let gate = Mutex::new(gate);
    let (c, t) = (calls.clone(), threads.clone());
    mgr.set_lan_ip_resolver(move || {
        c.fetch_add(1, Ordering::SeqCst);
        t.lock().unwrap().push(std::thread::current().id());
        let _ = gate.lock().unwrap().recv_timeout(Duration::from_secs(5));
        ip.to_string()
    });
    GatedResolver {
        calls,
        threads,
        release,
    }
}

#[test]
fn lan_address_is_resolved_off_the_calling_thread_and_cached() {
    common::isolate();
    let mut mgr = manager("lan");
    let resolver = gated_resolver(&mut mgr, "192.0.2.44");
    mgr.settings.web_server_enabled = true;
    mgr.settings.web_server_lan = true;
    mgr.set_bound_web_port(Some(8080));
    let index = index_of("web_server_enabled");

    // While the address is unknown the value leaves it out instead of waiting for it.
    let start = Instant::now();
    for _ in 0..50 {
        assert_eq!(mgr.get_display_value(index), "Enabled");
    }
    assert!(start.elapsed() < Duration::from_millis(500), "{:?}", start.elapsed());
    assert!(wait_for(|| resolver.calls.load(Ordering::SeqCst) == 1));
    assert!(!resolver.threads.lock().unwrap().contains(&std::thread::current().id()));

    resolver.release.send(()).unwrap();
    let token = quickstream::web_access::web_token();
    let expected = format!("Enabled (http://192.0.2.44:8080/?token={})", token);
    assert!(
        wait_for(|| mgr.get_display_value(index) == expected),
        "{}",
        mgr.get_display_value(index)
    );
    for _ in 0..50 {
        mgr.get_display_value(index);
    }
    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        1,
        "cached, not looked up per call"
    );
}

#[test]
fn web_url_is_the_address_the_display_value_shows() {
    common::isolate();
    let mut mgr = manager("web_url");
    let resolver = gated_resolver(&mut mgr, "192.0.2.46");
    let web = index_of("web_server_enabled");
    mgr.settings.web_server_enabled = false;
    assert_eq!(mgr.web_url(), None);

    mgr.settings.web_server_enabled = true;
    // The server bound 8099; the configured port is another.
    mgr.settings.web_server_port = 8080;
    mgr.set_bound_web_port(Some(8099));
    assert_eq!(mgr.web_url().as_deref(), Some("http://127.0.0.1:8099"));
    assert_eq!(mgr.get_display_value(web), "Enabled (http://127.0.0.1:8099)");

    // Network access: the pairing link once the address is known, nothing before.
    mgr.settings.web_server_lan = true;
    assert_eq!(mgr.web_url(), None);
    assert_eq!(mgr.get_display_value(web), "Enabled");
    resolver.release.send(()).unwrap();
    assert!(wait_for(|| mgr.web_url().is_some()));
    let url = mgr.web_url().unwrap();
    assert_eq!(
        url,
        quickstream::web_access::pairing_url("192.0.2.46", 8099, quickstream::web_access::web_token())
    );
    assert_eq!(mgr.get_display_value(web), format!("Enabled ({url})"));
}

#[test]
fn settings_tab_render_never_resolves_the_lan_address() {
    common::isolate();
    let mut app = test_app("lan_render");
    let resolver = gated_resolver(&mut app.settings_mgr, "192.0.2.45");
    app.settings_mgr.settings.web_server_enabled = true;
    app.settings_mgr.settings.web_server_lan = true;
    // As if the server were running (none is started here).
    app.settings_mgr.set_bound_web_port(Some(8080));
    app.active_tab = 3;
    // The option panel shows the pairing link while the web server row is selected.
    app.settings_cursor = index_of("web_server_enabled");

    let start = Instant::now();
    let text = common::tui::render_text(&mut app, 120, 40);
    for _ in 0..20 {
        common::tui::render(&mut app, 120, 40);
    }
    assert!(start.elapsed() < Duration::from_secs(1), "{:?}", start.elapsed());
    let row = text
        .lines()
        .find(|l| l.contains("NOW PLAYING WEB SERVER"))
        .expect("web server row");
    assert!(!row.contains("http://"), "{}", row);
    assert!(wait_for(|| resolver.calls.load(Ordering::SeqCst) == 1));
    assert!(!resolver.threads.lock().unwrap().contains(&std::thread::current().id()));

    resolver.release.send(()).unwrap();
    assert!(wait_for(|| app
        .settings_mgr
        .get_display_value(index_of("web_server_enabled"))
        .contains("192.0.2.45")));
    let text = common::tui::render_text(&mut app, 120, 40);
    assert!(text.contains("192.0.2.45"), "{}", text);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn web_values_name_the_bound_port_not_the_configured_one() {
    common::isolate();
    let mut mgr = manager("bound_port");
    let web = index_of("web_server_enabled");
    mgr.settings.web_server_enabled = true;
    mgr.settings.web_server_port = 8080;

    // No server running: no port or address is made up from the configured one.
    assert_eq!(mgr.short_value(web), "ON");
    assert_eq!(mgr.web_url(), None);
    assert_eq!(mgr.get_display_value(web), "Enabled");

    // 8080 was taken, so the server bound 8090: every value names 8090.
    mgr.set_bound_web_port(Some(8090));
    assert_eq!(mgr.short_value(web), "ON  :8090");
    assert_eq!(mgr.web_url().as_deref(), Some("http://127.0.0.1:8090"));
    assert_eq!(mgr.get_display_value(web), "Enabled (http://127.0.0.1:8090)");
    let labels: Vec<String> = mgr
        .options(web, &[], None)
        .choices
        .into_iter()
        .map(|c| c.label)
        .collect();
    assert_eq!(labels, ["OFF", "ON  :8090"]);

    // Stopped again.
    mgr.set_bound_web_port(None);
    assert_eq!(mgr.short_value(web), "ON");
    assert_eq!(mgr.web_url(), None);
}
