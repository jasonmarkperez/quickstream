//! The web server listens on loopback unless LAN access is needed (DLNA renderer active or
//! the "Web Remote on LAN" setting is on).

use quickstream::discovery::DLNADevice;
use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;
use quickstream::web::WebBind;

fn closed_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn fake_renderer() -> DLNADevice {
    let port = closed_port();
    DLNADevice {
        udn: "uuid:bind-test".to_string(),
        friendly_name: "Bind Test Renderer".to_string(),
        model_name: "Test".to_string(),
        manufacturer: "Test".to_string(),
        host: "127.0.0.1".to_string(),
        port,
        location: format!("http://127.0.0.1:{}/desc.xml", port),
        av_transport_control_url: format!("http://127.0.0.1:{}/ctrl/AVTransport", port),
        rendering_control_url: format!("http://127.0.0.1:{}/ctrl/RenderingControl", port),
    }
}

fn app(web_enabled: bool, lan: bool) -> AppUI {
    let mut settings_mgr = SettingsManager::new();
    settings_mgr.settings.web_server_enabled = web_enabled;
    settings_mgr.settings.web_server_lan = lan;
    settings_mgr.settings.web_server_port = 0;
    AppUI::new(None, settings_mgr)
}

#[test]
fn test_web_remote_binds_loopback_by_default() {
    let app = app(true, false);
    assert_eq!(app.web_server.as_ref().expect("server running").bind, WebBind::Loopback);
}

#[test]
fn test_web_remote_binds_lan_when_setting_enabled() {
    let app = app(true, true);
    assert_eq!(app.web_server.as_ref().expect("server running").bind, WebBind::Lan);
}

#[test]
fn test_dlna_renderer_switches_server_to_lan_and_back() {
    let mut app = app(true, false);
    app.switch_to_upnp_player(fake_renderer());
    assert_eq!(app.web_server.as_ref().unwrap().bind, WebBind::Lan);

    app.switch_to_local_player();
    assert_eq!(app.web_server.as_ref().unwrap().bind, WebBind::Loopback);
}
