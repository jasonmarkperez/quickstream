//! Access control for the embedded web server: session token for network peers, Host check
//! for loopback peers, CSRF checks on state-changing requests, and folder restrictions.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use quickstream::web::{NowPlayingState, WebBrowserResponse, WebCommand, WebServer};
use quickstream::web_access::{
    authorize, is_acceptable_root, load_or_create_token, resolve_allowed_path, AllowedRoots, AuthDecision, AuthHeaders,
};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const PORT: u16 = 8080;
/// The Web Remote Access setting (`web_server_lan`), passed to `authorize`.
const REMOTE_ON: bool = true;
const REMOTE_OFF: bool = false;

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
    common::isolate();
    let h = headers(&[&lan_host()]);
    assert_eq!(
        authorize(lan_peer(), PORT, "GET", "/", &h, TOKEN, REMOTE_ON),
        AuthDecision::Unauthorized
    );
    assert_eq!(
        authorize(lan_peer(), PORT, "GET", "/index.html", &h, TOKEN, REMOTE_ON),
        AuthDecision::Unauthorized
    );
}

#[test]
fn test_remote_api_without_token_is_unauthorized() {
    common::isolate();
    let h = headers(&[&lan_host()]);
    for path in [
        "/api/status",
        "/api/browser?path=/",
        "/api/cover",
        "/api/playlists",
        "/favicon.ico",
    ] {
        assert_eq!(
            authorize(lan_peer(), PORT, "GET", path, &h, TOKEN, REMOTE_ON),
            AuthDecision::Unauthorized,
            "{}",
            path
        );
    }
    let post = headers(&[&lan_host(), "Content-Type: application/json"]);
    assert_eq!(
        authorize(lan_peer(), PORT, "POST", "/api/action", &post, TOKEN, REMOTE_ON),
        AuthDecision::Unauthorized
    );
}

#[test]
fn test_remote_pairing_link_with_valid_token_pairs() {
    common::isolate();
    let h = headers(&[&lan_host()]);
    let path = format!("/?token={}", TOKEN);
    assert_eq!(
        authorize(lan_peer(), PORT, "GET", &path, &h, TOKEN, REMOTE_ON),
        AuthDecision::Pair
    );
}

#[test]
fn test_remote_pairing_link_with_wrong_token_is_unauthorized() {
    common::isolate();
    let h = headers(&[&lan_host()]);
    let wrong = "/?token=ffffffffffffffffffffffffffffffff";
    assert_eq!(
        authorize(lan_peer(), PORT, "GET", wrong, &h, TOKEN, REMOTE_ON),
        AuthDecision::Unauthorized
    );
    assert_eq!(
        authorize(lan_peer(), PORT, "GET", "/?token=", &h, TOKEN, REMOTE_ON),
        AuthDecision::Unauthorized
    );
    // A prefix of the real token must not match.
    let prefix = format!("/?token={}", &TOKEN[..16]);
    assert_eq!(
        authorize(lan_peer(), PORT, "GET", &prefix, &h, TOKEN, REMOTE_ON),
        AuthDecision::Unauthorized
    );
}

#[test]
fn test_remote_token_in_query_of_api_does_not_authorize() {
    common::isolate();
    // The query token only pairs the dashboard; API calls need the cookie or bearer header.
    let h = headers(&[&lan_host()]);
    let path = format!("/api/status?token={}", TOKEN);
    assert_eq!(
        authorize(lan_peer(), PORT, "GET", &path, &h, TOKEN, REMOTE_ON),
        AuthDecision::Unauthorized
    );
}

#[test]
fn test_remote_cookie_token_is_accepted() {
    common::isolate();
    let cookie = format!("Cookie: theme=dark; qs_token_{}={}; other=1", PORT, TOKEN);
    let h = headers(&[&lan_host(), &cookie]);
    assert_eq!(
        authorize(lan_peer(), PORT, "GET", "/", &h, TOKEN, REMOTE_ON),
        AuthDecision::Allow
    );
    assert_eq!(
        authorize(lan_peer(), PORT, "GET", "/api/status", &h, TOKEN, REMOTE_ON),
        AuthDecision::Allow
    );
}

#[test]
fn test_session_cookie_is_scoped_to_the_server_port() {
    common::isolate();
    // Cookies are shared by every port on a host, so the cookie name carries the port.
    let unscoped = format!("Cookie: qs_token={}", TOKEN);
    let other_port = format!("Cookie: qs_token_9999={}", TOKEN);
    for cookie in [unscoped, other_port] {
        let h = headers(&[&lan_host(), &cookie]);
        assert_eq!(
            authorize(lan_peer(), PORT, "GET", "/api/status", &h, TOKEN, REMOTE_ON),
            AuthDecision::Unauthorized,
            "{}",
            cookie
        );
    }
    assert_eq!(quickstream::web_access::token_cookie_name(8080), "qs_token_8080");
}

#[test]
fn test_remote_bearer_token_is_accepted() {
    common::isolate();
    let auth = format!("Authorization: Bearer {}", TOKEN);
    let h = headers(&[&lan_host(), &auth]);
    assert_eq!(
        authorize(lan_peer(), PORT, "GET", "/api/status", &h, TOKEN, REMOTE_ON),
        AuthDecision::Allow
    );
}

#[test]
fn test_remote_wrong_cookie_or_bearer_is_unauthorized() {
    common::isolate();
    let h = headers(&[
        &lan_host(),
        &format!("Cookie: qs_token_{}=ffffffffffffffffffffffffffffffff", PORT),
    ]);
    assert_eq!(
        authorize(lan_peer(), PORT, "GET", "/api/status", &h, TOKEN, REMOTE_ON),
        AuthDecision::Unauthorized
    );
    let h = headers(&[&lan_host(), "Authorization: Bearer nope"]);
    assert_eq!(
        authorize(lan_peer(), PORT, "GET", "/api/status", &h, TOKEN, REMOTE_ON),
        AuthDecision::Unauthorized
    );
    // A cookie whose name merely ends in the session cookie name is not the session cookie.
    let h = headers(&[&lan_host(), &format!("Cookie: xqs_token_{}={}", PORT, TOKEN)]);
    assert_eq!(
        authorize(lan_peer(), PORT, "GET", "/api/status", &h, TOKEN, REMOTE_ON),
        AuthDecision::Unauthorized
    );
}

#[test]
fn test_remote_stream_requests_need_no_token() {
    common::isolate();
    // DLNA renderers cannot authenticate; /stream only serves registered files.
    let h = headers(&[&lan_host()]);
    assert_eq!(
        authorize(lan_peer(), PORT, "GET", "/stream/song.flac", &h, TOKEN, REMOTE_ON),
        AuthDecision::Allow
    );
    assert_eq!(
        authorize(lan_peer(), PORT, "HEAD", "/stream/song.flac", &h, TOKEN, REMOTE_ON),
        AuthDecision::Allow
    );
    assert_eq!(
        authorize(lan_peer(), PORT, "GET", "/stream/continuous.wav", &h, TOKEN, REMOTE_ON),
        AuthDecision::Allow
    );
    // Only /stream/... paths are exempt.
    assert_eq!(
        authorize(lan_peer(), PORT, "GET", "/streamx", &h, TOKEN, REMOTE_ON),
        AuthDecision::Unauthorized
    );
}

/// Requests a paired phone makes: the dashboard, its API, the pairing link and an action.
fn paired_requests() -> Vec<(&'static str, String, AuthHeaders)> {
    let cookie = format!("Cookie: qs_token_{}={}", PORT, TOKEN);
    let bearer = format!("Authorization: Bearer {}", TOKEN);
    let get = headers(&[&lan_host(), &cookie]);
    let post = headers(&[&lan_host(), &cookie, "Content-Type: application/json"]);
    vec![
        ("GET", "/".to_string(), get.clone()),
        ("GET", "/index.html".to_string(), get.clone()),
        ("GET", "/api/status".to_string(), get.clone()),
        ("GET", "/api/playlists".to_string(), get.clone()),
        ("GET", "/api/cover".to_string(), get.clone()),
        ("GET", "/favicon.ico".to_string(), get),
        ("GET", "/api/status".to_string(), headers(&[&lan_host(), &bearer])),
        ("GET", format!("/?token={}", TOKEN), headers(&[&lan_host()])),
        ("POST", "/api/action".to_string(), post.clone()),
        ("POST", "/stream/song.flac".to_string(), post),
    ]
}

#[test]
fn test_paired_remote_peer_is_refused_while_remote_access_is_off() {
    common::isolate();
    // A renderer can make the server listen on the LAN with remote access off (issue #49): a
    // phone paired earlier keeps its token, but must not control Quickstream.
    for (method, path, h) in paired_requests() {
        assert_eq!(
            authorize(lan_peer(), PORT, method, &path, &h, TOKEN, REMOTE_OFF),
            AuthDecision::Forbidden("remote access is off"),
            "{} {}",
            method,
            path
        );
    }
    // Unpaired peers are refused the same way.
    let h = headers(&[&lan_host()]);
    assert_eq!(
        authorize(lan_peer(), PORT, "GET", "/api/status", &h, TOKEN, REMOTE_OFF),
        AuthDecision::Forbidden("remote access is off")
    );
}

#[test]
fn test_paired_remote_peer_is_allowed_while_remote_access_is_on() {
    common::isolate();
    for (method, path, h) in paired_requests() {
        let expected = if path.contains("?token=") {
            AuthDecision::Pair
        } else {
            AuthDecision::Allow
        };
        assert_eq!(
            authorize(lan_peer(), PORT, method, &path, &h, TOKEN, REMOTE_ON),
            expected,
            "{} {}",
            method,
            path
        );
    }
}

#[test]
fn test_remote_stream_requests_are_served_while_remote_access_is_off() {
    common::isolate();
    // The active renderer still needs its tokenless, read-only stream.
    let h = headers(&[&lan_host()]);
    for (method, path) in [
        ("GET", "/stream/song.flac"),
        ("HEAD", "/stream/song.flac"),
        ("GET", "/stream/continuous-0123.wav"),
    ] {
        assert_eq!(
            authorize(lan_peer(), PORT, method, path, &h, TOKEN, REMOTE_OFF),
            AuthDecision::Allow,
            "{} {}",
            method,
            path
        );
    }
    assert_eq!(
        authorize(lan_peer(), PORT, "GET", "/streamx", &h, TOKEN, REMOTE_OFF),
        AuthDecision::Forbidden("remote access is off")
    );
}

#[test]
fn test_loopback_is_unaffected_by_remote_access() {
    common::isolate();
    let local = format!("Host: 127.0.0.1:{}", PORT);
    let cases: Vec<(&str, String, AuthHeaders)> = vec![
        ("GET", "/".to_string(), headers(&[&local])),
        ("GET", "/api/status".to_string(), headers(&[&local])),
        ("GET", "/stream/song.flac".to_string(), headers(&[&local])),
        ("GET", format!("/?token={}", TOKEN), headers(&[&local])),
        (
            "POST",
            "/api/action".to_string(),
            headers(&[&local, "Content-Type: application/json"]),
        ),
        ("POST", "/api/action".to_string(), headers(&[&local])),
        (
            "GET",
            "/api/status".to_string(),
            headers(&["Host: rebind.evil.example"]),
        ),
    ];
    for (method, path, h) in cases {
        assert_eq!(
            authorize(loopback_peer(), PORT, method, &path, &h, TOKEN, REMOTE_OFF),
            authorize(loopback_peer(), PORT, method, &path, &h, TOKEN, REMOTE_ON),
            "{} {}",
            method,
            path
        );
    }
    assert_eq!(
        authorize(
            loopback_peer(),
            PORT,
            "GET",
            "/api/status",
            &headers(&[&local]),
            TOKEN,
            REMOTE_OFF
        ),
        AuthDecision::Allow
    );
    assert_eq!(
        authorize(
            loopback_peer(),
            PORT,
            "POST",
            "/api/action",
            &headers(&[&local, "Content-Type: application/json"]),
            TOKEN,
            REMOTE_OFF
        ),
        AuthDecision::Allow
    );
}

#[test]
fn test_remote_post_requires_json_content_type() {
    common::isolate();
    let cookie = format!("Cookie: qs_token_{}={}", PORT, TOKEN);
    let h = headers(&[&lan_host(), &cookie, "Content-Type: text/plain"]);
    assert!(matches!(
        authorize(lan_peer(), PORT, "POST", "/api/action", &h, TOKEN, REMOTE_ON),
        AuthDecision::Forbidden(_)
    ));
    let h = headers(&[&lan_host(), &cookie]);
    assert!(matches!(
        authorize(lan_peer(), PORT, "POST", "/api/action", &h, TOKEN, REMOTE_ON),
        AuthDecision::Forbidden(_)
    ));
    let h = headers(&[&lan_host(), &cookie, "Content-Type: application/json; charset=utf-8"]);
    assert_eq!(
        authorize(lan_peer(), PORT, "POST", "/api/action", &h, TOKEN, REMOTE_ON),
        AuthDecision::Allow
    );
}

#[test]
fn test_remote_post_with_foreign_origin_is_forbidden() {
    common::isolate();
    let cookie = format!("Cookie: qs_token_{}={}", PORT, TOKEN);
    let h = headers(&[
        &lan_host(),
        &cookie,
        "Content-Type: application/json",
        "Origin: http://evil.example",
    ]);
    assert!(matches!(
        authorize(lan_peer(), PORT, "POST", "/api/action", &h, TOKEN, REMOTE_ON),
        AuthDecision::Forbidden(_)
    ));
    let h = headers(&[&lan_host(), &cookie, "Content-Type: application/json", "Origin: null"]);
    assert!(matches!(
        authorize(lan_peer(), PORT, "POST", "/api/action", &h, TOKEN, REMOTE_ON),
        AuthDecision::Forbidden(_)
    ));
    let origin = format!("Origin: http://192.168.1.10:{}", PORT);
    let h = headers(&[&lan_host(), &cookie, "Content-Type: application/json", &origin]);
    assert_eq!(
        authorize(lan_peer(), PORT, "POST", "/api/action", &h, TOKEN, REMOTE_ON),
        AuthDecision::Allow
    );
}

#[test]
fn test_loopback_needs_no_token_but_requires_loopback_host() {
    common::isolate();
    for host in ["127.0.0.1", "localhost", "LOCALHOST", "[::1]"] {
        let h = headers(&[&format!("Host: {}:{}", host, PORT)]);
        assert_eq!(
            authorize(loopback_peer(), PORT, "GET", "/api/status", &h, TOKEN, REMOTE_ON),
            AuthDecision::Allow,
            "{}",
            host
        );
    }
    // DNS rebinding: an attacker's name resolving to 127.0.0.1.
    let h = headers(&[&format!("Host: rebind.evil.example:{}", PORT)]);
    assert!(matches!(
        authorize(loopback_peer(), PORT, "GET", "/api/status", &h, TOKEN, REMOTE_ON),
        AuthDecision::Forbidden(_)
    ));
    assert!(matches!(
        authorize(loopback_peer(), PORT, "GET", "/", &h, TOKEN, REMOTE_ON),
        AuthDecision::Forbidden(_)
    ));
    // Wrong port and missing Host are rejected too.
    let h = headers(&["Host: 127.0.0.1:9999"]);
    assert!(matches!(
        authorize(loopback_peer(), PORT, "GET", "/api/status", &h, TOKEN, REMOTE_ON),
        AuthDecision::Forbidden(_)
    ));
    let h = headers(&[]);
    assert!(matches!(
        authorize(loopback_peer(), PORT, "GET", "/api/status", &h, TOKEN, REMOTE_ON),
        AuthDecision::Forbidden(_)
    ));
}

#[test]
fn test_loopback_post_requires_json_and_same_origin() {
    common::isolate();
    let host = format!("Host: 127.0.0.1:{}", PORT);
    let h = headers(&[&host, "Content-Type: text/plain"]);
    assert!(matches!(
        authorize(loopback_peer(), PORT, "POST", "/api/action", &h, TOKEN, REMOTE_ON),
        AuthDecision::Forbidden(_)
    ));
    let h = headers(&[&host, "Content-Type: application/json", "Origin: http://evil.example"]);
    assert!(matches!(
        authorize(loopback_peer(), PORT, "POST", "/api/action", &h, TOKEN, REMOTE_ON),
        AuthDecision::Forbidden(_)
    ));
    let origin = format!("Origin: http://127.0.0.1:{}", PORT);
    let h = headers(&[&host, "Content-Type: application/json", &origin]);
    assert_eq!(
        authorize(loopback_peer(), PORT, "POST", "/api/action", &h, TOKEN, REMOTE_ON),
        AuthDecision::Allow
    );
    let h = headers(&[&host, "Content-Type: application/json"]);
    assert_eq!(
        authorize(loopback_peer(), PORT, "POST", "/api/action", &h, TOKEN, REMOTE_ON),
        AuthDecision::Allow
    );
}

#[test]
fn test_loopback_stream_requires_loopback_host() {
    common::isolate();
    // DNS rebinding: a page on a foreign name that resolves to 127.0.0.1 must not read streams.
    for path in ["/stream/a.mp3", "/stream/continuous.wav"] {
        for method in ["GET", "HEAD"] {
            let h = headers(&[&format!("Host: evil.example:{}", PORT)]);
            assert!(
                matches!(
                    authorize(loopback_peer(), PORT, method, path, &h, TOKEN, REMOTE_ON),
                    AuthDecision::Forbidden(_)
                ),
                "{} {}",
                method,
                path
            );
            let h = headers(&[]);
            assert!(matches!(
                authorize(loopback_peer(), PORT, method, path, &h, TOKEN, REMOTE_ON),
                AuthDecision::Forbidden(_)
            ));
        }
    }
    // A renderer on this computer addressing the server by its loopback address still works.
    for host in ["127.0.0.1", "localhost", "[::1]"] {
        let h = headers(&[&format!("Host: {}:{}", host, PORT)]);
        assert_eq!(
            authorize(loopback_peer(), PORT, "GET", "/stream/a.mp3", &h, TOKEN, REMOTE_ON),
            AuthDecision::Allow,
            "{}",
            host
        );
    }
}

#[test]
fn test_loopback_server_rejects_foreign_host_on_stream_routes() {
    common::isolate();
    let dir = temp_dir("streamhost");
    let song = dir.join("song.mp3");
    fs::write(&song, vec![0u8; 2048]).unwrap();
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (handle, _rx) = start(state);
    handle.register_stream_file(&song);
    let port = handle.port;
    let (status, _, body) = raw_request(
        port,
        &format!(
            "GET /stream/song.mp3 HTTP/1.1\r\nHost: evil.example:{}\r\nConnection: close\r\n\r\n",
            port
        ),
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
    common::isolate();
    let mapped: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
    let h = headers(&[&format!("Host: localhost:{}", PORT)]);
    assert_eq!(
        authorize(mapped, PORT, "GET", "/api/status", &h, TOKEN, REMOTE_ON),
        AuthDecision::Allow
    );
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
    common::isolate();
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
    common::isolate();
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
    common::isolate();
    let dir = temp_dir("roots");
    let music = dir.join("Music");
    let outside = dir.join("Private");
    fs::create_dir_all(music.join("Album")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.mp3"), b"x").unwrap();
    let roots = vec![music.clone()];

    assert_eq!(resolve_allowed_path(&music, &roots), Some(music.clone()));
    assert_eq!(
        resolve_allowed_path(&music.join("Album"), &roots),
        Some(music.join("Album"))
    );
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
        assert_eq!(
            resolve_allowed_path(&music.join("escape").join("secret.mp3"), &roots),
            None
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn test_allowed_roots_resolved_once_give_the_same_answers_as_resolve_allowed_path() {
    common::isolate();
    // PR #65 review: checking a playlist's entries canonicalized every root again for every
    // entry. `AllowedRoots` canonicalizes them once; every answer must stay the same, including
    // for roots and paths reached through symlinks.
    let dir = temp_dir("roots_once");
    let real = dir.join("Real");
    let private = dir.join("Private");
    fs::create_dir_all(real.join("Album")).unwrap();
    fs::create_dir_all(&private).unwrap();
    fs::write(real.join("Album").join("a.mp3"), b"x").unwrap();
    fs::write(private.join("secret.mp3"), b"x").unwrap();
    // A root that is itself a symlink to the real music folder.
    let linked_root = dir.join("Music");
    std::os::unix::fs::symlink(&real, &linked_root).unwrap();
    // Inside the real folder: a link to a folder in it, and a link leading out.
    std::os::unix::fs::symlink(real.join("Album"), real.join("Shortcut")).unwrap();
    std::os::unix::fs::symlink(&private, real.join("escape")).unwrap();
    // A root that points at / and one that doesn't exist grant nothing.
    let everything = dir.join("everything");
    std::os::unix::fs::symlink("/", &everything).unwrap();

    let root_sets = [
        vec![linked_root.clone()],
        vec![real.clone()],
        vec![everything.clone(), linked_root.clone()],
        vec![dir.join("ghost"), real.join("Album")],
        vec![],
    ];
    let paths = [
        linked_root.clone(),
        linked_root.join("Album").join("a.mp3"),
        real.join("Album").join("a.mp3"),
        real.join("Shortcut").join("a.mp3"),
        linked_root.join("Shortcut").join("a.mp3"),
        real.join("escape").join("secret.mp3"),
        linked_root.join("escape"),
        private.join("secret.mp3"),
        linked_root.join("..").join("Private").join("secret.mp3"),
        linked_root.join("Album").join("ghost.mp3"),
        PathBuf::from("/etc"),
        PathBuf::from("Music"),
    ];
    for roots in &root_sets {
        let allowed = AllowedRoots::new(roots);
        for path in &paths {
            assert_eq!(
                allowed.resolve(path),
                resolve_allowed_path(path, roots),
                "roots {:?}, path {}",
                roots,
                path.display()
            );
        }
    }
    // Spot checks, so the comparison above can't pass with both sides wrong.
    let allowed = AllowedRoots::new(std::slice::from_ref(&linked_root));
    let canonical_real = fs::canonicalize(&real).unwrap();
    assert_eq!(
        allowed.resolve(&linked_root.join("Shortcut").join("a.mp3")),
        Some(canonical_real.join("Album").join("a.mp3"))
    );
    assert_eq!(allowed.resolve(&real.join("escape").join("secret.mp3")), None);
    assert_eq!(AllowedRoots::new(&[everything]).resolve(Path::new("/etc")), None);
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// Integration over 127.0.0.1
// ---------------------------------------------------------------------------------------------

fn parse_http_response(raw: &[u8]) -> (u16, String, Vec<u8>) {
    let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(raw.len());
    let header_str = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let body = if header_end + 4 <= raw.len() {
        raw[header_end + 4..].to_vec()
    } else {
        Vec::new()
    };
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
    raw_request(
        port,
        &format!(
            "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            path, port
        ),
    )
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
    let (tx, rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    (handle, rx)
}

#[test]
fn test_loopback_server_rejects_foreign_host_header() {
    common::isolate();
    let (handle, _rx) = start(Arc::new(RwLock::new(NowPlayingState::default())));
    let port = handle.port;
    let (status, _, body) = raw_request(
        port,
        &format!(
            "GET /api/status HTTP/1.1\r\nHost: rebind.evil.example:{}\r\nConnection: close\r\n\r\n",
            port
        ),
    );
    assert_eq!(status, 403);
    assert!(!String::from_utf8_lossy(&body).contains("is_playing"));
    let (status, _, _) = get(port, "/api/status");
    assert_eq!(status, 200);
    handle.stop();
}

#[test]
fn test_loopback_server_rejects_cross_site_style_posts() {
    common::isolate();
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
    assert!(
        rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "rejected requests must not dispatch commands"
    );

    // Same-origin dashboard request goes through.
    let origin = format!(
        "Content-Type: application/json\r\nOrigin: http://127.0.0.1:{}\r\n",
        port
    );
    let (status, _, _) = post_with(port, &origin, r#"{"action":"next"}"#);
    assert_eq!(status, 200);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), WebCommand::Next);
    handle.stop();
}

/// Issue #50: the playlist actions share `/api/action`, so they get the same Content-Type and
/// Origin checks as every other action.
#[test]
fn test_loopback_server_rejects_cross_site_style_playlist_posts() {
    common::isolate();
    let (handle, rx) = start(Arc::new(RwLock::new(NowPlayingState::default())));
    let port = handle.port;
    for body in [
        r#"{"action":"playlist_play","name":"Road Trip"}"#,
        r#"{"action":"playlist_append","name":"Road Trip"}"#,
        r#"{"action":"playlist_create","name":"Road Trip"}"#,
        r#"{"action":"playlist_delete","name":"Road Trip"}"#,
    ] {
        for headers in [
            "Content-Type: text/plain\r\n",
            "",
            "Content-Type: application/json\r\nOrigin: http://evil.example\r\n",
        ] {
            let (status, _, _) = post_with(port, headers, body);
            assert_eq!(status, 403, "{} with {:?}", body, headers);
        }
    }
    assert!(
        rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "rejected requests must not dispatch commands"
    );
    handle.stop();
}

#[test]
fn test_responses_do_not_allow_cross_origin_reads() {
    common::isolate();
    let dir = temp_dir("cors");
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    state.write().unwrap().allowed_roots = vec![dir.clone()];
    let (handle, _rx) = start(state);
    let port = handle.port;
    for path in ["/", "/api/status", "/api/cover", "/api/browser"] {
        let (status, headers, _) = get(port, path);
        assert_eq!(status, 200, "{}", path);
        assert!(
            !headers.to_lowercase().contains("access-control-allow-origin"),
            "{}: {}",
            path,
            headers
        );
    }
    let (_, headers, _) = post_json(port, r#"{"action":"next"}"#);
    assert!(!headers.to_lowercase().contains("access-control-allow-origin"));
    handle.stop();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_dashboard_forbids_framing() {
    common::isolate();
    let (handle, _rx) = start(Arc::new(RwLock::new(NowPlayingState::default())));
    let (status, headers, _) = get(handle.port, "/");
    assert_eq!(status, 200);
    assert!(headers.to_lowercase().contains("x-frame-options: deny"), "{}", headers);
    handle.stop();
}

#[test]
fn test_loopback_dashboard_ignores_query_strings() {
    common::isolate();
    let (handle, _rx) = start(Arc::new(RwLock::new(NowPlayingState::default())));
    let port = handle.port;
    for path in ["/?token=bad", "/?foo", "/index.html?x=1", "/#top"] {
        let (status, _, body) = get(port, path);
        assert_eq!(status, 200, "{}", path);
        assert!(String::from_utf8_lossy(&body).contains("<html"), "{}", path);
    }
    handle.stop();
}

#[test]
fn test_pairing_link_sets_session_cookie_and_redirects() {
    common::isolate();
    let token = quickstream::web_access::web_token();
    let (handle, _rx) = start(Arc::new(RwLock::new(NowPlayingState::default())));
    let (status, headers, _) = get(handle.port, &format!("/?token={}", token));
    assert_eq!(status, 303);
    assert!(
        headers.contains("Location: /\r\n") || headers.ends_with("Location: /"),
        "{}",
        headers
    );
    let cookie_line = headers
        .lines()
        .find(|l| l.to_lowercase().starts_with("set-cookie:"))
        .expect("Set-Cookie header");
    assert!(
        cookie_line.contains(&format!("qs_token_{}={}", handle.port, token)),
        "{}",
        cookie_line
    );
    assert!(cookie_line.contains("HttpOnly"));
    assert!(cookie_line.contains("SameSite=Strict"));
    assert!(cookie_line.contains("Path=/"));
    handle.stop();
}

#[test]
fn test_browser_api_rejects_paths_outside_allowed_roots() {
    common::isolate();
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
fn test_browser_api_lists_the_bookmarks_the_ui_published_not_the_file() {
    common::isolate();
    let dir = temp_dir("browse_bookmarks");
    let music = dir.join("Music");
    fs::create_dir_all(&music).unwrap();
    // The web thread never reads bookmarks.json (#60): no path lookup or file read per request.
    let file = quickstream::paths::get_bookmarks_path();
    fs::write(&file, r#"["/qs-web-from-file"]"#).unwrap();

    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    {
        let mut s = state.write().unwrap();
        s.allowed_roots = vec![music.clone()];
        s.bookmarks = vec![music.to_string_lossy().to_string()];
    }
    let (handle, _rx) = start(state.clone());
    let (status, _, body) = get(handle.port, "/api/browser");
    assert_eq!(status, 200);
    let resp: WebBrowserResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(resp.bookmarks, vec![music.to_string_lossy().to_string()]);
    let (_, _, status_body) = get(handle.port, "/api/status");
    assert!(
        !String::from_utf8_lossy(&status_body).contains("\"bookmarks\""),
        "bookmarks are not part of /api/status"
    );

    handle.stop();
    fs::write(&file, "[]").unwrap();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_browser_api_without_roots_never_lists_process_directory() {
    common::isolate();
    let (handle, _rx) = start(Arc::new(RwLock::new(NowPlayingState::default())));
    let (status, _, _) = get(handle.port, "/api/browser");
    assert_eq!(status, 403);
    handle.stop();
}

#[test]
fn test_path_commands_outside_roots_are_rejected() {
    common::isolate();
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
    for action in [
        "add_file",
        "add_track",
        "add_folder",
        "set_home",
        "autopilot",
        "autopilot_reshuffle",
        "seed_radio",
    ] {
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
    let (status, _, _) = post_json(
        port,
        &format!(r#"{{"action":"add_file","path":{}}}"#, json_path(&inside_file)),
    );
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::AddFile(inside_file.clone())
    );
    let (status, _, _) = post_json(
        port,
        &format!(
            r#"{{"action":"add_folder","path":{}}}"#,
            json_path(&music.join("Album"))
        ),
    );
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::AddFolder(music.join("Album"))
    );
    let (status, _, _) = post_json(
        port,
        &format!(r#"{{"action":"set_home","path":{}}}"#, json_path(&music)),
    );
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::SetHome(Some(music.to_string_lossy().to_string()))
    );
    let (status, _, _) = post_json(
        port,
        &format!(r#"{{"action":"seed_radio","path":{}}}"#, json_path(&inside_file)),
    );
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::SeedRadio(inside_file)
    );
    // Path-less variants still work.
    let (status, _, _) = post_json(port, r#"{"action":"autopilot"}"#);
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::Autopilot(None)
    );
    let (status, _, _) = post_json(port, r#"{"action":"set_home"}"#);
    assert_eq!(status, 200);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        WebCommand::SetHome(None)
    );

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
    common::isolate();
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
    // The bookmarks themselves too, for GET /api/browser (#60).
    let published = app.web_state.read().unwrap().bookmarks.clone();
    assert_eq!(published, app.bookmarks);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_filesystem_root_and_home_are_never_allowed_roots() {
    common::isolate();
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
        assert_eq!(
            resolve_allowed_path(Path::new("/etc"), std::slice::from_ref(&link)),
            None
        );
        assert_eq!(resolve_allowed_path(&music, &[link]), None);
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_app_publishes_remote_access_setting_to_web_state() {
    common::isolate();
    let dir = temp_dir("remote_access_state");
    fs::create_dir_all(&dir).unwrap();
    let mut app = web_app(dir.clone(), None);
    assert!(!app.web_state.read().unwrap().remote_access);

    // AppUI::new publishes it before any server starts: Web Remote Access on means nothing
    // while the web server is off (a renderer would start the server on the LAN).
    let mut settings_mgr = quickstream::settings::SettingsManager::new();
    settings_mgr.settings.web_server_enabled = false;
    settings_mgr.settings.web_server_lan = true;
    let idle = quickstream::ui::AppUI::new(Some(dir.clone()), settings_mgr);
    assert!(idle.web_server.is_none());
    assert!(!idle.web_state.read().unwrap().remote_access);

    // Toggled while the server runs: the web thread's gate follows on the next publish.
    app.settings_mgr.settings.web_server_lan = true;
    app.update_web_state();
    assert!(app.web_state.read().unwrap().remote_access);

    // Web server turned off while a renderer keeps it running: no remote control either.
    app.settings_mgr.settings.web_server_enabled = false;
    app.update_web_state();
    assert!(!app.web_state.read().unwrap().remote_access);

    app.settings_mgr.settings.web_server_enabled = true;
    app.settings_mgr.settings.web_server_lan = false;
    app.update_web_state();
    assert!(!app.web_state.read().unwrap().remote_access);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_app_never_publishes_root_or_home_as_allowed_roots() {
    common::isolate();
    let dir = temp_dir("approots_broad");
    let music = dir.join("Music");
    fs::create_dir_all(&music).unwrap();
    let real_home = directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .expect("home dir");

    let mut app = web_app(music.clone(), Some(real_home.to_string_lossy().to_string()));
    // Launched from / with the home folder bookmarked.
    app.launch_dir = PathBuf::from("/");
    app.bookmarks = vec![
        "/".to_string(),
        real_home.to_string_lossy().to_string(),
        music.to_string_lossy().to_string(),
    ];
    app.update_web_state();

    let roots = app.web_state.read().unwrap().allowed_roots.clone();
    assert_eq!(roots, vec![music.clone()]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_pathless_web_autopilot_only_uses_terminal_folder_inside_roots() {
    common::isolate();
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
        assert!(
            app.status_message.contains("Elsewhere"),
            "{:?}: {}",
            cmd,
            app.status_message
        );
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
    common::isolate();
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
    assert!(
        !String::from_utf8_lossy(&body).contains("Private"),
        "{}",
        String::from_utf8_lossy(&body)
    );
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
    common::isolate();
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
    common::isolate();
    let mut mgr = quickstream::settings::SettingsManager::new();
    mgr.settings.web_server_enabled = true;
    let running = quickstream::settings::SettingsContext {
        web_port: Some(8080),
        ..Default::default()
    };
    mgr.settings.web_server_lan = false;
    let local = mgr.get_display_value(quickstream::settings::setting::WEB_SERVER_ENABLED, &running);
    assert!(!local.contains("token"), "{}", local);

    mgr.settings.web_server_lan = true;
    // The LAN address is looked up on a worker; the link shows once it is known.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut lan = mgr.get_display_value(quickstream::settings::setting::WEB_SERVER_ENABLED, &running);
    while !lan.contains("http://") && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
        lan = mgr.get_display_value(quickstream::settings::setting::WEB_SERVER_ENABLED, &running);
    }
    let token = quickstream::web_access::web_token();
    assert!(lan.contains(&format!(":8080/?token={}", token)), "{}", lan);
    assert!(lan.contains("http://"), "{}", lan);
}
