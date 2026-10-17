//! Access control for the embedded web server: session token for network peers, Host check
//! for loopback peers, CSRF checks on state-changing requests, and folder restrictions.

use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use quickstream::web::{NowPlayingState, WebBrowserResponse, WebCommand, WebServer};
use quickstream::web_access::{
    authorize, is_acceptable_root, load_or_create_token, resolve_allowed_path, AuthDecision, AuthHeaders,
};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const PORT: u16 = 8080;

/// Point the config dir at a temp folder before anything loads the web token, so tests never
/// create `web_token` in the developer's real ~/.config/quickstream.
fn isolate_config() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("qs_web_access_cfg_{}", std::process::id()));
        std::env::set_var("XDG_CONFIG_HOME", dir);
    });
}

fn lan_peer() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50))
}

fn loopback_peer() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

fn headers(lines: &[&str]) -> AuthHeaders {
    let mut h = AuthHeaders::default();
    for line in lines {
        h.observe(line);
    }
    h
}

fn lan_host() -> String {
    format!("Host: 192.168.1.10:{}", PORT)
}

// ---------------------------------------------------------------------------------------------
// Pure authorization decisions (network peers cannot be simulated over real sockets in tests)
// ---------------------------------------------------------------------------------------------

#[test]
fn test_remote_dashboard_without_token_is_unauthorized() {
    let h = headers(&[&lan_host()]);
    assert_eq!(authorize(lan_peer(), PORT, "GET", "/", &h, TOKEN), AuthDecision::Unauthorized);
    assert_eq!(authorize(lan_peer(), PORT, "GET", "/index.html", &h, TOKEN), AuthDecision::Unauthorized);
}

#[test]
fn test_remote_api_without_token_is_unauthorized() {
    let h = headers(&[&lan_host()]);
    for path in ["/api/status", "/api/browser?path=/", "/api/cover", "/favicon.ico"] {
        assert_eq!(authorize(lan_peer(), PORT, "GET", path, &h, TOKEN), AuthDecision::Unauthorized, "{}", path);
    }
    let post = headers(&[&lan_host(), "Content-Type: application/json"]);
    assert_eq!(authorize(lan_peer(), PORT, "POST", "/api/action", &post, TOKEN), AuthDecision::Unauthorized);
}

#[test]
fn test_remote_pairing_link_with_valid_token_pairs() {
    let h = headers(&[&lan_host()]);
    let path = format!("/?token={}", TOKEN);
    assert_eq!(authorize(lan_peer(), PORT, "GET", &path, &h, TOKEN), AuthDecision::Pair);
}

#[test]
fn test_remote_pairing_link_with_wrong_token_is_unauthorized() {
    let h = headers(&[&lan_host()]);
    let wrong = "/?token=ffffffffffffffffffffffffffffffff";
    assert_eq!(authorize(lan_peer(), PORT, "GET", wrong, &h, TOKEN), AuthDecision::Unauthorized);
    assert_eq!(authorize(lan_peer(), PORT, "GET", "/?token=", &h, TOKEN), AuthDecision::Unauthorized);
    // A prefix of the real token must not match.
    let prefix = format!("/?token={}", &TOKEN[..16]);
    assert_eq!(authorize(lan_peer(), PORT, "GET", &prefix, &h, TOKEN), AuthDecision::Unauthorized);
}

#[test]
fn test_remote_token_in_query_of_api_does_not_authorize() {
    // The query token only pairs the dashboard; API calls need the cookie or bearer header.
    let h = headers(&[&lan_host()]);
    let path = format!("/api/status?token={}", TOKEN);
    assert_eq!(authorize(lan_peer(), PORT, "GET", &path, &h, TOKEN), AuthDecision::Unauthorized);
}

#[test]
fn test_remote_cookie_token_is_accepted() {
    let cookie = format!("Cookie: theme=dark; qs_token_{}={}; other=1", PORT, TOKEN);
    let h = headers(&[&lan_host(), &cookie]);
    assert_eq!(authorize(lan_peer(), PORT, "GET", "/", &h, TOKEN), AuthDecision::Allow);
    assert_eq!(authorize(lan_peer(), PORT, "GET", "/api/status", &h, TOKEN), AuthDecision::Allow);
}

#[test]
fn test_session_cookie_is_scoped_to_the_server_port() {
    // Cookies are shared by every port on a host, so the cookie name carries the port.
    let unscoped = format!("Cookie: qs_token={}", TOKEN);
    let other_port = format!("Cookie: qs_token_9999={}", TOKEN);
    for cookie in [unscoped, other_port] {
        let h = headers(&[&lan_host(), &cookie]);
        assert_eq!(authorize(lan_peer(), PORT, "GET", "/api/status", &h, TOKEN), AuthDecision::Unauthorized, "{}", cookie);
    }
    assert_eq!(quickstream::web_access::token_cookie_name(8080), "qs_token_8080");
}

#[test]
fn test_remote_bearer_token_is_accepted() {
    let auth = format!("Authorization: Bearer {}", TOKEN);
    let h = headers(&[&lan_host(), &auth]);
    assert_eq!(authorize(lan_peer(), PORT, "GET", "/api/status", &h, TOKEN), AuthDecision::Allow);
}

#[test]
fn test_remote_wrong_cookie_or_bearer_is_unauthorized() {
    let h = headers(&[&lan_host(), &format!("Cookie: qs_token_{}=ffffffffffffffffffffffffffffffff", PORT)]);
    assert_eq!(authorize(lan_peer(), PORT, "GET", "/api/status", &h, TOKEN), AuthDecision::Unauthorized);
    let h = headers(&[&lan_host(), "Authorization: Bearer nope"]);
    assert_eq!(authorize(lan_peer(), PORT, "GET", "/api/status", &h, TOKEN), AuthDecision::Unauthorized);
    // A cookie whose name merely ends in the session cookie name is not the session cookie.
    let h = headers(&[&lan_host(), &format!("Cookie: xqs_token_{}={}", PORT, TOKEN)]);
    assert_eq!(authorize(lan_peer(), PORT, "GET", "/api/status", &h, TOKEN), AuthDecision::Unauthorized);
}

#[test]
fn test_remote_stream_requests_need_no_token() {
    // DLNA renderers cannot authenticate; /stream only serves registered files.
    let h = headers(&[&lan_host()]);
    assert_eq!(authorize(lan_peer(), PORT, "GET", "/stream/song.flac", &h, TOKEN), AuthDecision::Allow);
    assert_eq!(authorize(lan_peer(), PORT, "HEAD", "/stream/song.flac", &h, TOKEN), AuthDecision::Allow);
    assert_eq!(authorize(lan_peer(), PORT, "GET", "/stream/continuous.wav", &h, TOKEN), AuthDecision::Allow);
    // Only /stream/... paths are exempt.
    assert_eq!(authorize(lan_peer(), PORT, "GET", "/streamx", &h, TOKEN), AuthDecision::Unauthorized);
}

#[test]
fn test_remote_post_requires_json_content_type() {
    let cookie = format!("Cookie: qs_token_{}={}", PORT, TOKEN);
    let h = headers(&[&lan_host(), &cookie, "Content-Type: text/plain"]);
    assert!(matches!(authorize(lan_peer(), PORT, "POST", "/api/action", &h, TOKEN), AuthDecision::Forbidden(_)));
    let h = headers(&[&lan_host(), &cookie]);
    assert!(matches!(authorize(lan_peer(), PORT, "POST", "/api/action", &h, TOKEN), AuthDecision::Forbidden(_)));
    let h = headers(&[&lan_host(), &cookie, "Content-Type: application/json; charset=utf-8"]);
    assert_eq!(authorize(lan_peer(), PORT, "POST", "/api/action", &h, TOKEN), AuthDecision::Allow);
}

#[test]
fn test_remote_post_with_foreign_origin_is_forbidden() {
    let cookie = format!("Cookie: qs_token_{}={}", PORT, TOKEN);
    let h = headers(&[&lan_host(), &cookie, "Content-Type: application/json", "Origin: http://evil.example"]);
    assert!(matches!(authorize(lan_peer(), PORT, "POST", "/api/action", &h, TOKEN), AuthDecision::Forbidden(_)));
    let h = headers(&[&lan_host(), &cookie, "Content-Type: application/json", "Origin: null"]);
    assert!(matches!(authorize(lan_peer(), PORT, "POST", "/api/action", &h, TOKEN), AuthDecision::Forbidden(_)));
    let origin = format!("Origin: http://192.168.1.10:{}", PORT);
    let h = headers(&[&lan_host(), &cookie, "Content-Type: application/json", &origin]);
    assert_eq!(authorize(lan_peer(), PORT, "POST", "/api/action", &h, TOKEN), AuthDecision::Allow);
}

#[test]
fn test_loopback_needs_no_token_but_requires_loopback_host() {
    for host in ["127.0.0.1", "localhost", "LOCALHOST", "[::1]"] {
        let h = headers(&[&format!("Host: {}:{}", host, PORT)]);
        assert_eq!(authorize(loopback_peer(), PORT, "GET", "/api/status", &h, TOKEN), AuthDecision::Allow, "{}", host);
    }
    // DNS rebinding: an attacker's name resolving to 127.0.0.1.
    let h = headers(&[&format!("Host: rebind.evil.example:{}", PORT)]);
    assert!(matches!(authorize(loopback_peer(), PORT, "GET", "/api/status", &h, TOKEN), AuthDecision::Forbidden(_)));
    assert!(matches!(authorize(loopback_peer(), PORT, "GET", "/", &h, TOKEN), AuthDecision::Forbidden(_)));
    // Wrong port and missing Host are rejected too.
    let h = headers(&["Host: 127.0.0.1:9999"]);
    assert!(matches!(authorize(loopback_peer(), PORT, "GET", "/api/status", &h, TOKEN), AuthDecision::Forbidden(_)));
    let h = headers(&[]);
    assert!(matches!(authorize(loopback_peer(), PORT, "GET", "/api/status", &h, TOKEN), AuthDecision::Forbidden(_)));
}

#[test]
fn test_loopback_post_requires_json_and_same_origin() {
    let host = format!("Host: 127.0.0.1:{}", PORT);
    let h = headers(&[&host, "Content-Type: text/plain"]);
    assert!(matches!(authorize(loopback_peer(), PORT, "POST", "/api/action", &h, TOKEN), AuthDecision::Forbidden(_)));
    let h = headers(&[&host, "Content-Type: application/json", "Origin: http://evil.example"]);
    assert!(matches!(authorize(loopback_peer(), PORT, "POST", "/api/action", &h, TOKEN), AuthDecision::Forbidden(_)));
    let origin = format!("Origin: http://127.0.0.1:{}", PORT);
    let h = headers(&[&host, "Content-Type: application/json", &origin]);
    assert_eq!(authorize(loopback_peer(), PORT, "POST", "/api/action", &h, TOKEN), AuthDecision::Allow);
    let h = headers(&[&host, "Content-Type: application/json"]);
    assert_eq!(authorize(loopback_peer(), PORT, "POST", "/api/action", &h, TOKEN), AuthDecision::Allow);
}

#[test]
fn test_loopback_stream_requires_loopback_host() {
    // DNS rebinding: a page on a foreign name that resolves to 127.0.0.1 must not read streams.
    for path in ["/stream/a.mp3", "/stream/continuous.wav"] {
        for method in ["GET", "HEAD"] {
            let h = headers(&[&format!("Host: evil.example:{}", PORT)]);
            assert!(
                matches!(authorize(loopback_peer(), PORT, method, path, &h, TOKEN), AuthDecision::Forbidden(_)),
                "{} {}",
                method,
                path
            );
            let h = headers(&[]);
            assert!(matches!(authorize(loopback_peer(), PORT, method, path, &h, TOKEN), AuthDecision::Forbidden(_)));
        }
    }
    // A renderer on this computer addressing the server by its loopback address still works.
    for host in ["127.0.0.1", "localhost", "[::1]"] {
        let h = headers(&[&format!("Host: {}:{}", host, PORT)]);
        assert_eq!(authorize(loopback_peer(), PORT, "GET", "/stream/a.mp3", &h, TOKEN), AuthDecision::Allow, "{}", host);
    }
}

#[test]
fn test_loopback_server_rejects_foreign_host_on_stream_routes() {
    let dir = temp_dir("streamhost");
    let song = dir.join("song.mp3");
    fs::write(&song, vec![0u8; 2048]).unwrap();
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (handle, _rx) = start(state);
    handle.register_stream_file(&song);
    let port = handle.port;
    let (status, _, body) = raw_request(
        port,
        &format!("GET /stream/song.mp3 HTTP/1.1\r\nHost: evil.example:{}\r\nConnection: close\r\n\r\n", port),
    );
    assert_eq!(status, 403);
    assert!(body.len() < 100, "no audio may be served: {} bytes", body.len());
    let (status, _, _) = get(port, "/stream/song.mp3");
    assert_eq!(status, 200);
    handle.stop();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_ipv4_mapped_loopback_peer_counts_as_loopback() {
    let mapped: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
    let h = headers(&[&format!("Host: localhost:{}", PORT)]);
    assert_eq!(authorize(mapped, PORT, "GET", "/api/status", &h, TOKEN), AuthDecision::Allow);
}

// ---------------------------------------------------------------------------------------------
// Token persistence
// ---------------------------------------------------------------------------------------------

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qs_access_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    // Canonical so path comparisons hold on macOS (/var -> /private/var).
    fs::canonicalize(&dir).unwrap()
}

#[test]
fn test_token_is_created_persisted_and_private() {
    let dir = temp_dir("token");
    let path = dir.join("nested").join("web_token");
    let token = load_or_create_token(&path).expect("create token");
    assert_eq!(token.len(), 32);
    assert!(token.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_eq!(fs::read_to_string(&path).unwrap().trim(), token);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    // Survives restarts: the same token is loaded again.
    assert_eq!(load_or_create_token(&path).unwrap(), token);

    // A fresh token differs.
    let other = load_or_create_token(&dir.join("other_token")).unwrap();
    assert_ne!(other, token);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_invalid_token_file_is_replaced() {
    let dir = temp_dir("badtoken");
    let path = dir.join("web_token");
    fs::write(&path, "short").unwrap();
    let token = load_or_create_token(&path).unwrap();
    assert_eq!(token.len(), 32);
    assert_ne!(token, "short");
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// Allowed folders
// ---------------------------------------------------------------------------------------------

#[test]
fn test_resolve_allowed_path_accepts_only_paths_inside_roots() {
    let dir = temp_dir("roots");
    let music = dir.join("Music");
    let outside = dir.join("Private");
    fs::create_dir_all(music.join("Album")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.mp3"), b"x").unwrap();
    let roots = vec![music.clone()];

    assert_eq!(resolve_allowed_path(&music, &roots), Some(music.clone()));
    assert_eq!(resolve_allowed_path(&music.join("Album"), &roots), Some(music.join("Album")));
    assert_eq!(resolve_allowed_path(&outside, &roots), None);
    assert_eq!(resolve_allowed_path(Path::new("/"), &roots), None);
    // Traversal out of a root.
    assert_eq!(resolve_allowed_path(&music.join("..").join("Private"), &roots), None);
    // Relative paths are never accepted.
    assert_eq!(resolve_allowed_path(Path::new("Music"), &roots), None);
    // A sibling whose name shares the root's prefix is outside.
    fs::create_dir_all(dir.join("MusicX")).unwrap();
    assert_eq!(resolve_allowed_path(&dir.join("MusicX"), &roots), None);
    // No roots: nothing is allowed.
    assert_eq!(resolve_allowed_path(&music, &[]), None);

    #[cfg(unix)]
    {
        // A symlink inside a root that points outside it is rejected.
        std::os::unix::fs::symlink(&outside, music.join("escape")).unwrap();
        assert_eq!(resolve_allowed_path(&music.join("escape"), &roots), None);
        assert_eq!(resolve_allowed_path(&music.join("escape").join("secret.mp3"), &roots), None);
    }
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// Integration over 127.0.0.1
// ---------------------------------------------------------------------------------------------

fn parse_http_response(raw: &[u8]) -> (u16, String, Vec<u8>) {
    let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(raw.len());
    let header_str = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let body = if header_end + 4 <= raw.len() { raw[header_end + 4..].to_vec() } else { Vec::new() };
    let status_code = header_str
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    (status_code, header_str, body)
}

fn raw_request(port: u16, req: &str) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    stream.write_all(req.as_bytes()).unwrap();
    let mut resp = Vec::new();
    let _ = stream.read_to_end(&mut resp);
    parse_http_response(&resp)
}

fn get(port: u16, path: &str) -> (u16, String, Vec<u8>) {
    raw_request(port, &format!("GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", path, port))
}

fn post_with(port: u16, extra_headers: &str, body: &str) -> (u16, String, Vec<u8>) {
    raw_request(
        port,
        &format!(
            "POST /api/action HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            port,
            extra_headers,
            body.len(),
            body
        ),
    )
}

fn post_json(port: u16, body: &str) -> (u16, String, Vec<u8>) {
    post_with(port, "Content-Type: application/json\r\n", body)
}

fn start(state: Arc<RwLock<NowPlayingState>>) -> (quickstream::web::WebServerHandle, mpsc::Receiver<WebCommand>) {
    isolate_config();
    let (tx, rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    (handle, rx)
}

#[test]
fn test_loopback_server_rejects_foreign_host_header() {
    let (handle, _rx) = start(Arc::new(RwLock::new(NowPlayingState::default())));
    let port = handle.port;
    let (status, _, body) = raw_request(
        port,
        &format!("GET /api/status HTTP/1.1\r\nHost: rebind.evil.example:{}\r\nConnection: close\r\n\r\n", port),
    );
    assert_eq!(status, 403);
    assert!(!String::from_utf8_lossy(&body).contains("is_playing"));
    let (status, _, _) = get(port, "/api/status");
    assert_eq!(status, 200);
    handle.stop();
}

#[test]
fn test_loopback_server_rejects_cross_site_style_posts() {
    let (handle, rx) = start(Arc::new(RwLock::new(NowPlayingState::default())));
    let port = handle.port;

    // text/plain needs no CORS preflight, so it must be refused.
    let (status, _, _) = post_with(port, "Content-Type: text/plain\r\n", r#"{"action":"next"}"#);
    assert_eq!(status, 403);
    // No Content-Type at all.
    let (status, _, _) = post_with(port, "", r#"{"action":"next"}"#);
    assert_eq!(status, 403);
    // Foreign Origin.
    let (status, _, _) = post_with(
        port,
        "Content-Type: application/json\r\nOrigin: http://evil.example\r\n",
        r#"{"action":"next"}"#,
    );
    assert_eq!(status, 403);
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "rejected requests must not dispatch commands");

    // Same-origin dashboard request goes through.
    let origin = format!("Content-Type: application/json\r\nOrigin: http://127.0.0.1:{}\r\n", port);
    let (status, _, _) = post_with(port, &origin, r#"{"action":"next"}"#);
    assert_eq!(status, 200);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), WebCommand::Next);
    handle.stop();
}

#[test]
fn test_responses_do_not_allow_cross_origin_reads() {
    let dir = temp_dir("cors");
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    state.write().unwrap().allowed_roots = vec![dir.clone()];
    let (handle, _rx) = start(state);
    let port = handle.port;
    for path in ["/", "/api/status", "/api/cover", "/api/browser"] {
        let (status, headers, _) = get(port, path);
        assert_eq!(status, 200, "{}", path);
        assert!(!headers.to_lowercase().contains("access-control-allow-origin"), "{}: {}", path, headers);
    }
    let (_, headers, _) = post_json(port, r#"{"action":"next"}"#);
    assert!(!headers.to_lowercase().contains("access-control-allow-origin"));
    handle.stop();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_dashboard_forbids_framing() {
    let (handle, _rx) = start(Arc::new(RwLock::new(NowPlayingState::default())));
    let (status, headers, _) = get(handle.port, "/");
    assert_eq!(status, 200);
    assert!(headers.to_lowercase().contains("x-frame-options: deny"), "{}", headers);
    handle.stop();
}

#[test]
fn test_pairing_link_sets_session_cookie_and_redirects() {
    isolate_config();
    let token = quickstream::web_access::web_token();
    let (handle, _rx) = start(Arc::new(RwLock::new(NowPlayingState::default())));
    let (status, headers, _) = get(handle.port, &format!("/?token={}", token));
    assert_eq!(status, 303);
    assert!(headers.contains("Location: /\r\n") || headers.ends_with("Location: /"), "{}", headers);
    let cookie_line = headers
        .lines()
        .find(|l| l.to_lowercase().starts_with("set-cookie:"))
        .expect("Set-Cookie header");
    assert!(cookie_line.contains(&format!("qs_token_{}={}", handle.port, token)), "{}", cookie_line);
    assert!(cookie_line.contains("HttpOnly"));
    assert!(cookie_line.contains("SameSite=Strict"));
    assert!(cookie_line.contains("Path=/"));
    handle.stop();
}

#[test]
fn test_browser_api_rejects_paths_outside_allowed_roots() {
    let dir = temp_dir("browse");
    let music = dir.join("Music");
    fs::create_dir_all(music.join("Rock")).unwrap();
    fs::write(music.join("song.mp3"), b"x").unwrap();
    let private = dir.join("Private");
    fs::create_dir_all(&private).unwrap();
    fs::write(private.join("secret.mp3"), b"x").unwrap();

    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    state.write().unwrap().allowed_roots = vec![music.clone()];
    let (handle, _rx) = start(state.clone());
    let port = handle.port;
    let enc = |p: &Path| quickstream::player::upnp::url_encode(&p.to_string_lossy());

    // Outside the roots: refused, nothing listed.
    for p in [private.clone(), PathBuf::from("/"), music.join("..")] {
        let (status, _, body) = get(port, &format!("/api/browser?path={}", enc(&p)));
        assert_eq!(status, 403, "{}", p.display());
        assert!(!String::from_utf8_lossy(&body).contains("secret.mp3"));
    }

    // Inside the roots: listed. The root has no navigable parent.
    let (status, _, body) = get(port, &format!("/api/browser?path={}", enc(&music)));
    assert_eq!(status, 200);
    let resp: WebBrowserResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(resp.current_path, music.to_string_lossy());
    assert_eq!(resp.parent_path, None);
    assert!(resp.entries.iter().any(|e| e.name == "song.mp3"));

    let (status, _, body) = get(port, &format!("/api/browser?path={}", enc(&music.join("Rock"))));
    assert_eq!(status, 200);
    let resp: WebBrowserResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(resp.parent_path, Some(music.to_string_lossy().to_string()));

    // The TUI browsing outside the roots is neither listed by default nor revealed.
    state.write().unwrap().current_browser_dir = private.to_string_lossy().to_string();
    let (status, _, body) = get(port, "/api/browser");
    assert_eq!(status, 200);
    let resp: WebBrowserResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(resp.current_path, music.to_string_lossy());
    assert_eq!(resp.terminal_dir, None);
    let (_, _, status_body) = get(port, "/api/status");
    assert!(!String::from_utf8_lossy(&status_body).contains("Private"));

    handle.stop();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_browser_api_without_roots_never_lists_process_directory() {
    let (handle, _rx) = start(Arc::new(RwLock::new(NowPlayingState::default())));
    let (status, _, _) = get(handle.port, "/api/browser");
    assert_eq!(status, 403);
    handle.stop();
}

#[test]
fn test_path_commands_outside_roots_are_rejected() {
    let dir = temp_dir("cmds");
    let music = dir.join("Music");
    fs::create_dir_all(music.join("Album")).unwrap();
    fs::write(music.join("Album").join("a.mp3"), b"x").unwrap();
    let private = dir.join("Private");
    fs::create_dir_all(&private).unwrap();
    fs::write(private.join("b.mp3"), b"x").unwrap();

    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    state.write().unwrap().allowed_roots = vec![music.clone()];
    let (handle, rx) = start(state);
    let port = handle.port;
    let json_path = |p: &Path| serde_json::to_string(&p.to_string_lossy()).unwrap();

    let outside_file = private.join("b.mp3");
    for action in ["add_file", "add_track", "add_folder", "set_home", "autopilot", "autopilot_reshuffle", "seed_radio"] {
        let target = if action.contains("file") || action.contains("track") || action == "seed_radio" {
            outside_file.clone()
        } else {
            private.clone()
        };
        let body = format!(r#"{{"action":"{}","path":{}}}"#, action, json_path(&target));
        let (status, _, _) = post_json(port, &body);
        assert_eq!(status, 403, "{}", action);
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(WebCommand::PathRejected(p)) => assert_eq!(p, target, "{}", action),
            other => panic!("{}: expected PathRejected, got {:?}", action, other),
        }
    }

    // Inside the roots the commands carry the resolved path.
    let inside_file = music.join("Album").join("a.mp3");
    let (status, _, _) = post_json(port, &format!(r#"{{"action":"add_file","path":{}}}"#, json_path(&inside_file)));
    assert_eq!(status, 200);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), WebCommand::AddFile(inside_file.clone()));
    let (status, _, _) = post_json(port, &format!(r#"{{"action":"add_folder","path":{}}}"#, json_path(&music.join("Album"))));
    assert_eq!(status, 200);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), WebCommand::AddFolder(music.join("Album")));
    let (status, _, _) = post_json(port, &format!(r#"{{"action":"set_home","path":{}}}"#, json_path(&music)));
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::SetHome(Some(music.to_string_lossy().to_string()))
    );
    let (status, _, _) = post_json(port, &format!(r#"{{"action":"seed_radio","path":{}}}"#, json_path(&inside_file)));
    assert_eq!(status, 200);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), WebCommand::SeedRadio(inside_file));
    // Path-less variants still work.
    let (status, _, _) = post_json(port, r#"{"action":"autopilot"}"#);
    assert_eq!(status, 200);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), WebCommand::Autopilot(None));
    let (status, _, _) = post_json(port, r#"{"action":"set_home"}"#);
    assert_eq!(status, 200);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), WebCommand::SetHome(None));

    handle.stop();
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// TUI integration
// ---------------------------------------------------------------------------------------------

fn web_app(start_dir: PathBuf, home: Option<String>) -> quickstream::ui::AppUI {
    let mut settings_mgr = quickstream::settings::SettingsManager::new();
    settings_mgr.settings.web_server_enabled = true;
    settings_mgr.settings.web_server_lan = false;
    settings_mgr.settings.web_server_port = 0;
    settings_mgr.settings.home_dir = home;
    quickstream::ui::AppUI::new(Some(start_dir), settings_mgr)
}

#[test]
fn test_app_publishes_allowed_roots_to_web_state() {
    let dir = temp_dir("approots");
    let launch = dir.join("Launch");
    let home = dir.join("Home");
    let bookmark = dir.join("Bookmark");
    let elsewhere = dir.join("Elsewhere");
    for d in [&launch, &home, &bookmark, &elsewhere] {
        fs::create_dir_all(d).unwrap();
    }
    let mut app = web_app(launch.clone(), Some(home.to_string_lossy().to_string()));
    app.bookmarks = vec![bookmark.to_string_lossy().to_string()];
    // Browsing elsewhere in the TUI does not widen what the web may reach.
    app.browser_dir = elsewhere.clone();
    app.update_web_state();

    let roots = app.web_state.read().unwrap().allowed_roots.clone();
    assert!(roots.contains(&launch), "{:?}", roots);
    assert!(roots.contains(&home), "{:?}", roots);
    assert!(roots.contains(&bookmark), "{:?}", roots);
    assert!(!roots.contains(&elsewhere), "{:?}", roots);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_filesystem_root_and_home_are_never_allowed_roots() {
    let dir = temp_dir("broadroots");
    let home = dir.join("home").join("me");
    let music = home.join("Music");
    fs::create_dir_all(&music).unwrap();

    assert!(!is_acceptable_root(Path::new("/"), Some(&home)));
    assert!(!is_acceptable_root(Path::new("/"), None));
    assert!(!is_acceptable_root(&home, Some(&home)));
    assert!(!is_acceptable_root(&home.join("Music").join(".."), Some(&home)));
    // Folders containing the home folder are broader still.
    assert!(!is_acceptable_root(&dir.join("home"), Some(&home)));
    assert!(!is_acceptable_root(Path::new("Music"), Some(&home)));
    assert!(is_acceptable_root(&music, Some(&home)));
    assert!(is_acceptable_root(&music, None));

    #[cfg(unix)]
    {
        // A root that is a symlink to / grants nothing.
        let link = dir.join("everything");
        std::os::unix::fs::symlink("/", &link).unwrap();
        assert_eq!(resolve_allowed_path(Path::new("/etc"), &[link.clone()]), None);
        assert_eq!(resolve_allowed_path(&music, &[link]), None);
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_app_never_publishes_root_or_home_as_allowed_roots() {
    let dir = temp_dir("approots_broad");
    let music = dir.join("Music");
    fs::create_dir_all(&music).unwrap();
    let real_home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()).expect("home dir");

    let mut app = web_app(music.clone(), Some(real_home.to_string_lossy().to_string()));
    // Launched from / with the home folder bookmarked.
    app.launch_dir = PathBuf::from("/");
    app.bookmarks = vec!["/".to_string(), real_home.to_string_lossy().to_string(), music.to_string_lossy().to_string()];
    app.update_web_state();

    let roots = app.web_state.read().unwrap().allowed_roots.clone();
    assert_eq!(roots, vec![music.clone()]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_pathless_web_autopilot_only_uses_terminal_folder_inside_roots() {
    let dir = temp_dir("pathless_ap");
    let music = dir.join("Music");
    let elsewhere = dir.join("Elsewhere");
    fs::create_dir_all(&music).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    let mut app = web_app(music.clone(), None);
    let (tx, rx) = mpsc::channel();
    app.web_cmd_rx = Some(rx);

    // The terminal is browsing outside the allowed folders: path-less requests are refused.
    for cmd in [WebCommand::Autopilot(None), WebCommand::AutopilotReshuffle(None)] {
        app.browser_dir = elsewhere.clone();
        app.status_message.clear();
        tx.send(cmd.clone()).unwrap();
        app.handle_web_commands();
        assert_eq!(app.autopilot_dir, None, "{:?}", cmd);
        assert!(app.status_message.contains("Elsewhere"), "{:?}: {}", cmd, app.status_message);
    }

    // Inside the allowed folders the terminal folder is used.
    app.browser_dir = music.clone();
    tx.send(WebCommand::Autopilot(None)).unwrap();
    app.handle_web_commands();
    assert_eq!(app.autopilot_dir, Some(music.clone()));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_status_api_hides_paths_outside_allowed_roots() {
    let dir = temp_dir("status_paths");
    let music = dir.join("Music");
    let private = dir.join("Private");
    fs::create_dir_all(&music).unwrap();
    fs::create_dir_all(&private).unwrap();
    let inside = music.join("in.mp3");
    let outside = private.join("out.mp3");
    let item = |index: usize, p: &Path| quickstream::web::WebQueueItem {
        index,
        title: format!("T{}", index),
        artist: String::new(),
        album: String::new(),
        duration_seconds: 1.0,
        filepath: p.to_string_lossy().to_string(),
        is_current: index == 0,
    };

    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    {
        let mut s = state.write().unwrap();
        s.allowed_roots = vec![music.clone()];
        s.queue = vec![item(0, &outside), item(1, &inside)];
        s.filepath = Some(outside.clone());
        s.autopilot_dir = Some(private.to_string_lossy().to_string());
    }
    let (handle, _rx) = start(state.clone());
    let port = handle.port;

    let (status, _, body) = get(port, "/api/status");
    assert_eq!(status, 200);
    assert!(!String::from_utf8_lossy(&body).contains("Private"), "{}", String::from_utf8_lossy(&body));
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["queue"][0]["filepath"], "");
    assert_eq!(json["queue"][1]["filepath"], inside.to_string_lossy().as_ref());
    assert_eq!(json["filepath"], "out.mp3");
    assert!(json["autopilot_dir"].is_null());
    // The shared state itself is untouched (the continuous stream decodes from it).
    assert_eq!(state.read().unwrap().queue[0].filepath, outside.to_string_lossy());

    // Paths inside the roots are reported in full.
    {
        let mut s = state.write().unwrap();
        s.filepath = Some(inside.clone());
        s.autopilot_dir = Some(music.to_string_lossy().to_string());
    }
    let (_, _, body) = get(port, "/api/status");
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["filepath"], inside.to_string_lossy().as_ref());
    assert_eq!(json["autopilot_dir"], music.to_string_lossy().as_ref());

    handle.stop();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_rejected_web_path_shows_status_message() {
    let dir = temp_dir("rejectmsg");
    let mut app = web_app(dir.clone(), None);
    let (tx, rx) = mpsc::channel();
    app.web_cmd_rx = Some(rx);
    tx.send(WebCommand::PathRejected(PathBuf::from("/etc"))).unwrap();
    app.handle_web_commands();
    assert!(app.status_message.contains("/etc"), "{}", app.status_message);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_settings_show_pairing_link_when_lan_access_enabled() {
    let mut mgr = quickstream::settings::SettingsManager::new();
    mgr.settings.web_server_enabled = true;
    mgr.settings.web_server_port = 8080;
    mgr.settings.web_server_lan = false;
    let local = mgr.get_display_value(6);
    assert!(!local.contains("token"), "{}", local);

    isolate_config();
    mgr.settings.web_server_lan = true;
    let lan = mgr.get_display_value(6);
    let token = quickstream::web_access::web_token();
    assert!(lan.contains(&format!(":8080/?token={}", token)), "{}", lan);
    assert!(lan.contains("http://"), "{}", lan);
}
