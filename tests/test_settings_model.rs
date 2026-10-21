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
    display_position, step_in_display_order, SettingGroup, SettingValue, SettingsContext, SettingsManager,
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

/// A context holding only `bookmarks`: no music folder, home or running web server.
fn with_bookmarks(bookmarks: &[String]) -> SettingsContext<'_> {
    SettingsContext {
        bookmarks,
        ..SettingsContext::default()
    }
}

/// A context in which the web server runs on `port` (no server is started).
fn with_port(port: u16) -> SettingsContext<'static> {
    SettingsContext {
        web_port: Some(port),
        ..SettingsContext::default()
    }
}

/// Start the app's web server on loopback the way the Settings tab does (cycling its row on),
/// on a port the OS picks, and return the port it bound. Loopback only: no firewall prompt.
fn start_loopback_web(app: &mut AppUI) -> u16 {
    app.settings_mgr.settings.web_server_lan = false;
    app.settings_mgr.settings.web_server_enabled = false;
    app.settings_mgr.settings.web_server_port = 0;
    app.settings_cursor = index_of("web_server_enabled");
    app.cycle_and_apply_setting(true);
    app.web_port().expect("web server started")
}

fn index_of(key: &str) -> usize {
    SETTING_DEFINITIONS
        .iter()
        .position(|d| d.key == key)
        .unwrap_or_else(|| panic!("no setting {}", key))
}

fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
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
fn named_setting_indexes_match_their_keys() {
    common::isolate();
    use quickstream::settings::setting;
    let named = [
        (setting::METADATA_VIEW, "metadata_view"),
        (setting::PLAYER_BACKEND, "player_backend"),
        (setting::SEEK_STEP_SECONDS, "seek_step_seconds"),
        (setting::VOLUME_STEP, "volume_step"),
        (setting::QUEUE_MODE, "queue_mode"),
        (setting::FORCE_TRANSCODE, "force_transcode"),
        (setting::WEB_SERVER_ENABLED, "web_server_enabled"),
        (setting::HOME_DIR, "home_dir"),
        (setting::GAPLESS_AUTOPILOT, "gapless_autopilot"),
        (setting::DEFAULT_MIX_PROFILE, "default_mix_profile"),
        (setting::ONLINE_ENRICHMENT, "online_enrichment"),
        (setting::WEB_SERVER_LAN, "web_server_lan"),
        (setting::LIBRARY_INDEX, "library_index"),
    ];
    for (index, key) in named {
        assert_eq!(SETTING_DEFINITIONS[index].key, key);
        assert_eq!(index, index_of(key));
    }
    // Every setting has a name.
    assert_eq!(named.len(), SETTING_DEFINITIONS.len());
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
        [
            "metadata_view",
            "home_dir",
            "default_mix_profile",
            "online_enrichment",
            "library_index"
        ]
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

/// The Library Index (#41): a LIBRARY setting listed after tag enrichment, on by default (also
/// for a config.json saved before it existed), cycling ON and OFF.
#[test]
fn library_index_is_on_by_default_after_tag_enrichment() {
    common::isolate();
    let index = quickstream::settings::setting::LIBRARY_INDEX;
    let def = &SETTING_DEFINITIONS[index];
    assert_eq!((def.label, def.group), ("Library Index", SettingGroup::Library));
    assert!(def.description.contains("Never changes your files"));
    let pos = display_position(index).unwrap();
    assert_eq!(
        SETTING_DISPLAY_ORDER[pos - 1],
        quickstream::settings::setting::ONLINE_ENRICHMENT
    );
    assert_eq!(SETTING_DISPLAY_ORDER.len(), 13);

    assert!(quickstream::settings::Settings::default().library_index);
    let legacy: quickstream::settings::Settings = serde_json::from_str(
        r#"{"metadata_view":true,"default_device":"local-audio","seek_step_seconds":10.0,"volume_step":5,"queue_mode":"loop","force_transcode":false,"player_backend":"mpv"}"#,
    )
    .unwrap();
    assert!(
        legacy.library_index,
        "a config saved before the setting existed indexes"
    );

    let mut mgr = manager("library_index");
    let context = SettingsContext::default();
    let options = mgr.options(index, &context);
    let labels: Vec<&str> = options.choices.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, ["OFF", "ON"]);
    assert_eq!(options.current, Some(1));
    assert_eq!(mgr.short_value(index, &context), "ON");
    mgr.cycle_setting(index, true, &context);
    assert!(!mgr.settings.library_index);
    assert_eq!(mgr.short_value(index, &context), "OFF");
    assert_eq!(mgr.get_display_value(index, &context), "Off");
    mgr.cycle_setting(index, true, &context);
    assert!(mgr.settings.library_index);
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
            let options = mgr.options(index, &with_bookmarks(&bookmarks));
            let n = options.choices.len();
            assert!(n >= 2, "setting {} has {} choices", index, n);
            let start = options.current.expect("a default setting is one of its choices");
            assert_eq!(Some(&options.choices[start].value), mgr.value(index).as_ref());
            assert_eq!(
                mgr.short_value(index, &with_bookmarks(&bookmarks)),
                options.choices[start].label
            );

            // Cycling visits the choices in order and comes back round to the start.
            for step in 1..=n {
                mgr.cycle_setting(index, forward, &with_bookmarks(&bookmarks));
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
                let now = mgr.options(index, &with_bookmarks(&bookmarks));
                assert_eq!(now.current, Some(expected), "{} step {}", label, step);
                assert_eq!(
                    mgr.short_value(index, &with_bookmarks(&bookmarks)),
                    options.choices[expected].label
                );
            }
        }
    }
}

#[test]
fn short_values_read_as_mockup_2e() {
    common::isolate();
    let mut mgr = manager("short");
    let home = PathBuf::from("/qs-home");
    let context = SettingsContext {
        home: Some(home.as_path()),
        ..SettingsContext::default()
    };
    let s = |mgr: &SettingsManager, key: &str| mgr.short_value(index_of(key), &context);
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
    assert_eq!(
        mgr.short_value(index_of("web_server_enabled"), &with_port(8080)),
        "ON  :8080"
    );
    mgr.settings.queue_mode = "repeat_one".to_string();
    assert_eq!(s(&mgr, "queue_mode"), "REPEAT TRACK");
    mgr.settings.default_mix_profile = "bpm_flow".to_string();
    assert_eq!(s(&mgr, "default_mix_profile"), "BPM FLOW");

    // Paths keep their own case, with ~ for the context's home.
    mgr.settings.home_dir = Some(home.join("Music").to_string_lossy().into_owned());
    assert_eq!(s(&mgr, "home_dir"), "~/Music");
    mgr.settings.home_dir = None;
    assert_eq!(s(&mgr, "home_dir"), "LAUNCH DIRECTORY");

    let queue = mgr.options(index_of("queue_mode"), &SettingsContext::default());
    let labels: Vec<&str> = queue.choices.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, ["CONTINUOUS (LOOP)", "STOP AT END", "REPEAT TRACK"]);
}

#[test]
fn display_values_keep_their_wording() {
    common::isolate();
    let mut mgr = manager("wording");
    let none = SettingsContext::default();
    assert_eq!(
        mgr.get_display_value(index_of("queue_mode"), &none),
        "Continuous (Loop Queue)"
    );
    assert_eq!(
        mgr.get_display_value(index_of("seek_step_seconds"), &none),
        "10 seconds"
    );
    assert_eq!(
        mgr.get_display_value(index_of("default_mix_profile"), &none),
        "Diverse Library Shuffle"
    );
    mgr.settings.web_server_enabled = true;
    assert_eq!(
        mgr.get_display_value(index_of("web_server_enabled"), &with_port(8123)),
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
    let none = SettingsContext::default();
    assert_eq!(mgr.options(seek, &none).current, None);
    mgr.cycle_setting(seek, true, &none);
    assert_eq!(mgr.settings.seek_step_seconds, 15.0);

    mgr.settings.player_backend = "vlc".to_string();
    mgr.cycle_setting(index_of("player_backend"), true, &none);
    assert_eq!(mgr.settings.player_backend, "mpv");

    mgr.settings.volume_step = 7;
    mgr.cycle_setting(index_of("volume_step"), false, &none);
    assert_eq!(mgr.settings.volume_step, 2);

    mgr.settings.queue_mode = "shuffle".to_string();
    mgr.cycle_setting(index_of("queue_mode"), true, &none);
    assert_eq!(mgr.settings.queue_mode, "stop");

    mgr.settings.default_mix_profile = "focus".to_string();
    mgr.cycle_setting(index_of("default_mix_profile"), true, &none);
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
    let home = PathBuf::from("/qs-home-folder");
    let context = SettingsContext {
        bookmarks: &bookmarks,
        music_dir: Some(music.as_path()),
        home: Some(home.as_path()),
        web_port: None,
    };
    let home = home.to_string_lossy().into_owned();
    let options = mgr.options(index_of("home_dir"), &context);
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
    let options = mgr.options(index, &with_bookmarks(&bookmarks));
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
        mgr.cycle_setting(index, true, &with_bookmarks(&bookmarks));
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
    let running = with_port(8080);
    let index = index_of("web_server_enabled");

    // While the address is unknown the value leaves it out instead of waiting for it.
    let start = Instant::now();
    for _ in 0..50 {
        assert_eq!(mgr.get_display_value(index, &running), "Enabled");
    }
    assert!(start.elapsed() < Duration::from_millis(500), "{:?}", start.elapsed());
    assert!(wait_for(|| resolver.calls.load(Ordering::SeqCst) == 1));
    assert!(!resolver.threads.lock().unwrap().contains(&std::thread::current().id()));

    resolver.release.send(()).unwrap();
    let token = quickstream::web_access::web_token();
    let expected = format!("Enabled (http://192.0.2.44:8080/?token={})", token);
    assert!(
        wait_for(|| mgr.get_display_value(index, &running) == expected),
        "{}",
        mgr.get_display_value(index, &running)
    );
    for _ in 0..50 {
        mgr.get_display_value(index, &running);
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
    assert_eq!(mgr.web_url(Some(8099)), None);

    mgr.settings.web_server_enabled = true;
    // The server bound 8099; the configured port is another.
    mgr.settings.web_server_port = 8080;
    let running = with_port(8099);
    assert_eq!(mgr.web_url(Some(8099)).as_deref(), Some("http://127.0.0.1:8099"));
    assert_eq!(mgr.get_display_value(web, &running), "Enabled (http://127.0.0.1:8099)");

    // Network access: the pairing link once the address is known, nothing before.
    mgr.settings.web_server_lan = true;
    assert_eq!(mgr.web_url(Some(8099)), None);
    assert_eq!(mgr.get_display_value(web, &running), "Enabled");
    resolver.release.send(()).unwrap();
    assert!(wait_for(|| mgr.web_url(Some(8099)).is_some()));
    let url = mgr.web_url(Some(8099)).unwrap();
    assert_eq!(
        url,
        quickstream::web_access::pairing_url("192.0.2.46", 8099, quickstream::web_access::web_token())
    );
    assert_eq!(mgr.get_display_value(web, &running), format!("Enabled ({url})"));
}

#[test]
fn settings_tab_render_never_resolves_the_lan_address() {
    common::isolate();
    let mut app = test_app("lan_render");
    // A loopback server runs; network access is then turned on in the settings only, so the
    // pairing link is what the tab shows (and nothing binds the LAN).
    start_loopback_web(&mut app);
    app.settings_mgr.settings.web_server_lan = true;
    let resolver = gated_resolver(&mut app.settings_mgr, "192.0.2.45");
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
        .get_display_value(index_of("web_server_enabled"), &app.settings_context())
        .contains("192.0.2.45")));
    let text = common::tui::render_text(&mut app, 120, 40);
    assert!(text.contains("192.0.2.45"), "{}", text);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn web_values_name_the_port_in_the_context_not_the_configured_one() {
    common::isolate();
    let mut mgr = manager("bound_port");
    let web = index_of("web_server_enabled");
    mgr.settings.web_server_enabled = true;
    mgr.settings.web_server_port = 8080;

    // No server running: no port or address is made up from the configured one.
    let stopped = SettingsContext::default();
    assert_eq!(mgr.short_value(web, &stopped), "ON");
    assert_eq!(mgr.web_url(None), None);
    assert_eq!(mgr.get_display_value(web, &stopped), "Enabled");

    // 8080 was taken, so the server bound 8090: every value names 8090.
    let running = with_port(8090);
    assert_eq!(mgr.short_value(web, &running), "ON  :8090");
    assert_eq!(mgr.web_url(Some(8090)).as_deref(), Some("http://127.0.0.1:8090"));
    assert_eq!(mgr.get_display_value(web, &running), "Enabled (http://127.0.0.1:8090)");
    let labels: Vec<String> = mgr
        .options(web, &running)
        .choices
        .into_iter()
        .map(|c| c.label)
        .collect();
    assert_eq!(labels, ["OFF", "ON  :8090"]);
}

#[test]
fn app_context_names_the_running_servers_port() {
    common::isolate();
    let mut app = test_app("context_port");
    assert_eq!(app.web_port(), None);
    assert_eq!(app.settings_context().web_port, None);
    let port = start_loopback_web(&mut app);
    assert_eq!(app.web_server.as_ref().map(|s| s.port), Some(port));
    assert_eq!(app.settings_context().web_port, Some(port));
    // Stopped: no port.
    app.cycle_and_apply_setting(true);
    assert!(app.web_server.is_none());
    assert_eq!(app.settings_context().web_port, None);
}

#[test]
fn values_and_choices_take_the_home_folder_from_the_context() {
    common::isolate();
    let mut mgr = manager("context_home");
    let index = index_of("home_dir");
    let home = PathBuf::from("/qs-ctx-home");
    let context = SettingsContext {
        home: Some(home.as_path()),
        ..SettingsContext::default()
    };
    mgr.settings.home_dir = Some("/qs-ctx-home/Music/Jazz".to_string());
    assert_eq!(mgr.short_value(index, &context), "~/Music/Jazz");
    assert_eq!(mgr.get_display_value(index, &context), "~/Music/Jazz");
    let values: Vec<SettingValue> = mgr
        .options(index, &context)
        .choices
        .into_iter()
        .map(|c| c.value)
        .collect();
    assert!(
        values.contains(&SettingValue::Path(Some("/qs-ctx-home".to_string()))),
        "{values:?}"
    );
    // No home in the context: none listed, and paths read in full.
    let none = SettingsContext::default();
    assert_eq!(mgr.short_value(index, &none), "/qs-ctx-home/Music/Jazz");
    assert_eq!(
        mgr.options(index, &none).choices.len(),
        2,
        "launch directory and the custom path"
    );

    // The app's context uses the home folder the library shows as `~`.
    let app = test_app("context_home_app");
    let ctx = app.settings_context();
    assert_eq!(ctx.home.map(PathBuf::from), quickstream::web_access::home_dir());
}

#[test]
fn startup_home_lists_the_music_folder_once_its_worker_has_found_it() {
    common::isolate();
    let mut app = test_app("music_dir");
    let (release, gate) = mpsc::channel::<()>();
    let lookup_thread = Arc::new(Mutex::new(None));
    let seen = lookup_thread.clone();
    app.set_music_dir_lookup(move || {
        *seen.lock().unwrap() = Some(std::thread::current().id());
        let _ = gate.recv_timeout(Duration::from_secs(5));
        Some(PathBuf::from("/qs-music-folder"))
    });
    let index = index_of("home_dir");
    let music = SettingValue::Path(Some("/qs-music-folder".to_string()));
    let listed = |app: &AppUI| {
        app.settings_mgr
            .options(index, &app.settings_context())
            .choices
            .iter()
            .any(|c| c.value == music)
    };

    // Still looking: no music folder, and cycling skips it as if there were none.
    assert!(wait_for(|| lookup_thread.lock().unwrap().is_some()));
    assert_ne!(*lookup_thread.lock().unwrap(), Some(std::thread::current().id()));
    assert_eq!(app.settings_context().music_dir, None);
    assert!(!listed(&app));
    app.settings_cursor = index;
    let n = app.settings_mgr.options(index, &app.settings_context()).choices.len();
    for _ in 0..n {
        app.cycle_and_apply_setting(true);
        assert_ne!(app.settings_mgr.settings.home_dir.as_deref(), Some("/qs-music-folder"));
    }

    // Found: listed right after the launch directory, and cycling reaches it.
    release.send(()).unwrap();
    assert!(wait_for(|| app.settings_context().music_dir.is_some()));
    assert!(listed(&app));
    app.settings_mgr.settings.home_dir = None;
    app.cycle_and_apply_setting(true);
    assert_eq!(app.settings_mgr.settings.home_dir.as_deref(), Some("/qs-music-folder"));
}

// ---------- the pairing link in the status line ----------

#[test]
fn status_gets_the_pairing_link_once_the_lan_address_is_known() {
    common::isolate();
    let mut app = test_app("status_link");
    let port = start_loopback_web(&mut app);
    // Network access on (in the settings only, so nothing binds the LAN here).
    app.settings_mgr.settings.web_server_lan = true;
    let resolver = gated_resolver(&mut app.settings_mgr, "192.0.2.47");
    let web = index_of("web_server_enabled");

    // As right after turning the server on: the address isn't known yet.
    app.show_setting_status(web);
    assert_eq!(app.status_message, "Now Playing Web Server: Enabled");
    app.poll_settings_status();
    assert_eq!(app.status_message, "Now Playing Web Server: Enabled");

    resolver.release.send(()).unwrap();
    let link = quickstream::web_access::pairing_url("192.0.2.47", port, quickstream::web_access::web_token());
    let expected = format!("Now Playing Web Server: Enabled ({link})");
    assert!(
        wait_for(|| {
            app.poll_settings_status();
            app.status_message == expected
        }),
        "{}",
        app.status_message
    );
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_newer_status_is_not_replaced_by_the_pairing_link() {
    common::isolate();
    let mut app = test_app("status_link_newer");
    let port = start_loopback_web(&mut app);
    app.settings_mgr.settings.web_server_lan = true;
    let resolver = gated_resolver(&mut app.settings_mgr, "192.0.2.48");
    app.show_setting_status(index_of("web_server_enabled"));
    assert_eq!(app.status_message, "Now Playing Web Server: Enabled");
    app.status_message = "Volume 40%".to_string();

    resolver.release.send(()).unwrap();
    assert!(wait_for(|| app.settings_mgr.web_url(Some(port)).is_some()));
    for _ in 0..3 {
        app.poll_settings_status();
    }
    assert_eq!(app.status_message, "Volume 40%");
}

#[test]
fn lan_address_lookup_starts_before_the_link_is_needed() {
    common::isolate();
    // At startup, with network access on.
    let mut mgr = manager("lan_prefetch_startup");
    mgr.settings.web_server_enabled = false;
    mgr.settings.web_server_lan = true;
    let resolver = gated_resolver(&mut mgr, "192.0.2.49");
    let app = AppUI::new(Some(std::env::temp_dir()), mgr);
    assert!(wait_for(|| resolver.calls.load(Ordering::SeqCst) == 1));
    resolver.release.send(()).unwrap();
    drop(app);

    // When a web row is shown, even with the server still off.
    let mut app = test_app("lan_prefetch_row");
    app.settings_mgr.settings.web_server_lan = true;
    let resolver = gated_resolver(&mut app.settings_mgr, "192.0.2.50");
    app.active_tab = 3;
    app.settings_cursor = index_of("web_server_enabled");
    common::tui::render(&mut app, 80, 24);
    assert!(wait_for(|| resolver.calls.load(Ordering::SeqCst) == 1));
    assert!(!resolver.threads.lock().unwrap().contains(&std::thread::current().id()));
    resolver.release.send(()).unwrap();
}

/// The whole flow: turning the web server on with network access binds the LAN, so this runs
/// on CI only (`--ignored`), like the other tests that bind every interface.
#[test]
#[ignore]
fn turning_the_server_on_with_network_access_puts_the_link_in_the_status() {
    common::isolate();
    let mut app = test_app("status_link_lan");
    app.settings_mgr.settings.web_server_lan = true;
    app.settings_mgr.settings.web_server_port = 0;
    let resolver = gated_resolver(&mut app.settings_mgr, "192.0.2.51");
    app.settings_cursor = index_of("web_server_enabled");
    app.cycle_and_apply_setting(true);
    let port = app.web_port().expect("web server started");
    assert_eq!(app.status_message, "Now Playing Web Server: Enabled");
    resolver.release.send(()).unwrap();
    let link = quickstream::web_access::pairing_url("192.0.2.51", port, quickstream::web_access::web_token());
    assert!(
        wait_for(|| {
            app.poll_settings_status();
            app.status_message.ends_with(&format!("({link})"))
        }),
        "{}",
        app.status_message
    );
}
