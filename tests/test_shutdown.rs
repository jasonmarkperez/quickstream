use quickstream::settings::SettingsManager;
use quickstream::ui::{self, AppUI};

#[test]
fn test_shutdown_request_is_observed_by_event_loop_tick() {
    let mut settings_mgr = SettingsManager::new();
    settings_mgr.settings.web_server_enabled = false;
    let mut app = AppUI::new(None, settings_mgr);

    assert!(!app.poll_shutdown_request());

    ui::request_shutdown();
    assert!(app.poll_shutdown_request());

    ui::clear_shutdown_request();
    assert!(!app.poll_shutdown_request());
}
