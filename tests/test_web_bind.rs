//! The web server listens on loopback unless LAN access is needed (DLNA renderer active or
//! the "Web Remote Access" setting is set to the network).

mod common;

use quickstream::discovery::DLNADevice;
use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;
use quickstream::web::WebBind;

fn closed_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn fake_renderer() -> DLNADevice {
    renderer_at("127.0.0.1", closed_port())
}

/// A renderer elsewhere on the network (TEST-NET-1, never routed).
fn network_renderer() -> DLNADevice {
    renderer_at("192.0.2.10", 1400)
}

fn renderer_at(host: &str, port: u16) -> DLNADevice {
    DLNADevice {
        udn: "uuid:bind-test".to_string(),
        friendly_name: "Bind Test Renderer".to_string(),
        model_name: "Test".to_string(),
        manufacturer: "Test".to_string(),
        host: host.to_string(),
        port,
        location: format!("http://{}:{}/desc.xml", host, port),
        av_transport_control_url: format!("http://{}:{}/ctrl/AVTransport", host, port),
        rendering_control_url: format!("http://{}:{}/ctrl/RenderingControl", host, port),
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
    common::isolate();
    let app = app(true, false);
    assert_eq!(app.web_server.as_ref().expect("server running").bind, WebBind::Loopback);
}

#[test]
#[ignore = "binds all network interfaces (macOS firewall prompt); run with --include-ignored"]
fn test_web_remote_binds_lan_when_setting_enabled() {
    common::isolate();
    let app = app(true, true);
    assert_eq!(app.web_server.as_ref().expect("server running").bind, WebBind::Lan);
}

#[test]
#[ignore = "binds all network interfaces (macOS firewall prompt); run with --include-ignored"]
fn test_dlna_renderer_switches_server_to_lan_and_back() {
    common::isolate();
    let mut app = app(true, false);
    app.switch_to_upnp_player(network_renderer());
    assert_eq!(app.web_server.as_ref().unwrap().bind, WebBind::Lan);

    app.switch_to_local_player();
    assert_eq!(app.web_server.as_ref().unwrap().bind, WebBind::Loopback);
}

#[test]
#[ignore = "binds all network interfaces (macOS firewall prompt); run with --include-ignored"]
fn test_lan_request_never_falls_back_to_loopback_on_busy_port() {
    common::isolate();
    use quickstream::web::{NowPlayingState, WebServer};
    use std::sync::{mpsc, Arc, RwLock};

    // Another app holds *:port. Binding 127.0.0.1:port would still succeed on macOS, which
    // would send renderers' LAN requests to that other app.
    let other_app = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let busy_port = other_app.local_addr().unwrap().port();

    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start_with_bind(WebBind::Lan, busy_port, state, tx).expect("start");
    assert_eq!(handle.bind, WebBind::Lan);
    assert_ne!(handle.port, busy_port);
    handle.stop();
}

#[test]
fn test_switching_back_to_local_keeps_output_status_message() {
    common::isolate();
    let mut app = app(true, false);
    app.switch_to_upnp_player(fake_renderer());
    app.switch_to_local_player();
    assert_eq!(app.status_message, "Active output: Local Audio");
}

#[test]
fn test_creating_app_does_not_start_network_discovery() {
    common::isolate();
    let app = app(false, false);
    // Discovery runs on a background thread for ~2.5s once started.
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        !app.device_manager.is_scanning(),
        "discovery must start from run(), not AppUI::new"
    );
}

#[test]
fn test_renderer_on_this_computer_keeps_server_on_loopback() {
    common::isolate();
    // A renderer at 127.0.0.1 reaches the stream over loopback; no network exposure needed.
    let mut app = app(true, false);
    app.switch_to_upnp_player(fake_renderer());
    assert_eq!(app.web_server.as_ref().unwrap().bind, WebBind::Loopback);
}
