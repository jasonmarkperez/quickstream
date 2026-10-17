//! Access control for the embedded web server.
//!
//! - Network (non-loopback) peers need the session token, sent as the `qs_token` cookie (set by
//!   opening the pairing link `/?token=<token>`) or an `Authorization: Bearer` header. `/stream/*`
//!   is exempt: DLNA renderers cannot authenticate, and it only serves registered files.
//! - Loopback peers need no token, but the `Host` header must name the loopback address on every
//!   route (including `/stream/*`), which blocks DNS rebinding.
//! - State-changing requests need `Content-Type: application/json` (a cross-site form or
//!   `text/plain` fetch cannot send it without a CORS preflight, which is never granted) and, when
//!   an `Origin` header is present, it must be this server's own origin.
//! - Paths named by web requests must resolve inside the allowed folders (startup home,
//!   bookmarks, launch folder).

use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Name of the session cookie that carries the token.
pub const TOKEN_COOKIE: &str = "qs_token";

/// Request headers relevant to authorization, collected while the request is parsed.
#[derive(Debug, Default, Clone)]
pub struct AuthHeaders {
    pub host: Option<String>,
    pub origin: Option<String>,
    pub content_type: Option<String>,
    pub cookie: Option<String>,
    pub authorization: Option<String>,
}

impl AuthHeaders {
    /// Record one raw header line (`Name: value`) if it matters for authorization.
    pub fn observe(&mut self, line: &str) {
        let Some((name, value)) = line.split_once(':') else {
            return;
        };
        let value = value.trim().to_string();
        let slot = match name.trim().to_ascii_lowercase().as_str() {
            "host" => &mut self.host,
            "origin" => &mut self.origin,
            "content-type" => &mut self.content_type,
            "authorization" => &mut self.authorization,
            "cookie" => {
                match self.cookie {
                    Some(ref mut c) => {
                        c.push_str("; ");
                        c.push_str(&value);
                    }
                    None => self.cookie = Some(value),
                }
                return;
            }
            _ => return,
        };
        if slot.is_some() {
            // A repeated Host/Origin/Content-Type/Authorization header is ambiguous: make it
            // match nothing.
            *slot = Some(String::from("\u{0}duplicate"));
        } else {
            *slot = Some(value);
        }
    }

    fn cookie_token(&self) -> Option<&str> {
        self.cookie.as_deref()?.split(';').find_map(|pair| {
            let (k, v) = pair.trim().split_once('=')?;
            (k.trim() == TOKEN_COOKIE).then(|| v.trim())
        })
    }

    fn bearer_token(&self) -> Option<&str> {
        let auth = self.authorization.as_deref()?.trim();
        let (scheme, value) = auth.split_once(' ')?;
        scheme.eq_ignore_ascii_case("bearer").then(|| value.trim())
    }
}

/// Outcome of [`authorize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthDecision {
    /// Route the request normally.
    Allow,
    /// Valid pairing link: set the session cookie and redirect to the dashboard.
    Pair,
    /// Network peer without a valid token (401).
    Unauthorized,
    /// Refused regardless of credentials (403), with a short reason.
    Forbidden(&'static str),
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    }
}

fn path_only(path: &str) -> &str {
    path.split(['?', '#']).next().unwrap_or(path)
}

fn query_param<'a>(path: &'a str, key: &str) -> Option<&'a str> {
    let query = path.split_once('?')?.1;
    let query = query.split('#').next().unwrap_or(query);
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (k == key).then_some(v)
    })
}

/// Compare two secrets without an early exit on the first differing byte.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn token_matches(candidate: Option<&str>, token: &str) -> bool {
    !token.is_empty() && candidate.is_some_and(|c| constant_time_eq(c, token))
}

fn is_loopback_host(host: &str, port: u16) -> bool {
    let host = host.to_ascii_lowercase();
    ["127.0.0.1", "localhost", "[::1]"].iter().any(|name| {
        host == format!("{}:{}", name, port) || (port == 80 && host == *name)
    })
}

/// Decide whether a request may proceed. `server_port` is the port the request arrived on.
pub fn authorize(
    peer: IpAddr,
    server_port: u16,
    method: &str,
    path: &str,
    headers: &AuthHeaders,
    token: &str,
) -> AuthDecision {
    let route = path_only(path);
    let read_only = method == "GET" || method == "HEAD";

    if is_loopback(peer) {
        // Applies to every route, /stream/* included: a foreign Host means DNS rebinding.
        if !headers.host.as_deref().is_some_and(|h| is_loopback_host(h, server_port)) {
            return AuthDecision::Forbidden("unexpected Host header");
        }
    } else if read_only && route.starts_with("/stream/") {
        // DLNA renderers cannot authenticate.
        return AuthDecision::Allow;
    } else {
        let is_dashboard = route == "/" || route == "/index.html";
        if read_only && is_dashboard {
            if let Some(candidate) = query_param(path, "token") {
                return if token_matches(Some(candidate), token) {
                    AuthDecision::Pair
                } else {
                    AuthDecision::Unauthorized
                };
            }
        }
        let authenticated =
            token_matches(headers.cookie_token(), token) || token_matches(headers.bearer_token(), token);
        if !authenticated {
            return AuthDecision::Unauthorized;
        }
    }

    if !read_only {
        let is_json = headers.content_type.as_deref().is_some_and(|ct| {
            ct.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/json")
        });
        if !is_json {
            return AuthDecision::Forbidden("Content-Type must be application/json");
        }
        if let Some(ref origin) = headers.origin {
            let same_origin = headers
                .host
                .as_deref()
                .is_some_and(|host| origin.eq_ignore_ascii_case(&format!("http://{}", host)));
            if !same_origin {
                return AuthDecision::Forbidden("cross-origin request");
            }
        }
    }

    // A loopback peer that opens the pairing link still gets the cookie, so the same link works
    // everywhere.
    if read_only && (route == "/" || route == "/index.html") {
        if let Some(candidate) = query_param(path, "token") {
            if token_matches(Some(candidate), token) {
                return AuthDecision::Pair;
            }
        }
    }

    AuthDecision::Allow
}

const UNAUTHORIZED_HTML: &str = "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Quickstream</title>\
<style>body{font-family:-apple-system,system-ui,sans-serif;background:#0b0b10;color:#e8e8ef;\
max-width:32rem;margin:15vh auto;padding:0 1.25rem;line-height:1.5}a{color:#7aa2ff}</style></head>\
<body><h1>Quickstream</h1><p>This device is not paired with Quickstream yet.</p>\
<p>Open the pairing link shown in Quickstream under <b>[4] Settings &rarr; Now Playing Web Server</b> \
(it ends in <code>/?token=&hellip;</code>). After that, this page opens normally.</p>\
<p><a href=\"/\">Retry</a></p></body></html>";

/// Write the response for a non-`Allow` decision.
pub fn write_decision_response(out: &mut impl Write, decision: &AuthDecision, path: &str, token: &str) -> std::io::Result<()> {
    let is_api = path_only(path).starts_with("/api/");
    let (status, content_type, body, extra): (&str, &str, String, String) = match decision {
        AuthDecision::Allow => return Ok(()),
        AuthDecision::Pair => (
            "303 See Other",
            "text/plain; charset=utf-8",
            String::new(),
            format!(
                "Location: /\r\nSet-Cookie: {}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=31536000\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n",
                TOKEN_COOKIE, token
            ),
        ),
        AuthDecision::Unauthorized if is_api => (
            "401 Unauthorized",
            "application/json",
            r#"{"ok":false,"error":"unauthorized"}"#.to_string(),
            "Cache-Control: no-store\r\n".to_string(),
        ),
        AuthDecision::Unauthorized => (
            "401 Unauthorized",
            "text/html; charset=utf-8",
            UNAUTHORIZED_HTML.to_string(),
            "Cache-Control: no-store\r\nX-Frame-Options: DENY\r\n".to_string(),
        ),
        AuthDecision::Forbidden(reason) if is_api => (
            "403 Forbidden",
            "application/json",
            serde_json::json!({ "ok": false, "error": reason }).to_string(),
            "Cache-Control: no-store\r\n".to_string(),
        ),
        AuthDecision::Forbidden(reason) => (
            "403 Forbidden",
            "text/plain; charset=utf-8",
            format!("Forbidden: {}", reason),
            "Cache-Control: no-store\r\n".to_string(),
        ),
    };
    let header = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        content_type,
        extra,
        body.len()
    );
    out.write_all(header.as_bytes())?;
    out.write_all(body.as_bytes())?;
    out.flush()
}

/// Authorize a request on `stream` with the process-wide token. Writes the refusal (or pairing
/// redirect) and returns `false` when routing must stop.
pub fn check_request(
    stream: &mut std::net::TcpStream,
    method: &str,
    path: &str,
    headers: &AuthHeaders,
) -> std::io::Result<bool> {
    let peer = stream.peer_addr()?.ip();
    let port = stream.local_addr()?.port();
    // Loopback peers only need the token to open the pairing link; loading it for every local
    // request would create the token file on first use. An empty token never matches.
    let token = if is_loopback(peer) && query_param(path, "token").is_none() {
        ""
    } else {
        web_token()
    };
    let decision = authorize(peer, port, method, path, headers, token);
    if decision == AuthDecision::Allow {
        return Ok(true);
    }
    write_decision_response(stream, &decision, path, token)?;
    Ok(false)
}

/// Write a JSON 403 for a request naming a path outside the allowed folders.
pub fn write_path_forbidden(out: &mut impl Write) -> std::io::Result<()> {
    write_decision_response(out, &AuthDecision::Forbidden("path is outside the allowed folders"), "/api/", "")
}

// ---------------------------------------------------------------------------------------------
// Token
// ---------------------------------------------------------------------------------------------

fn is_valid_token(t: &str) -> bool {
    t.len() == 32 && t.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A new random 128-bit token as 32 lowercase hex characters.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 16];
    let from_os = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .is_ok();
    if !from_os {
        // RandomState is keyed from the OS random source.
        use std::hash::{BuildHasher, Hasher};
        for (i, chunk) in bytes.chunks_mut(8).enumerate() {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_usize(i);
            h.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
            );
            chunk.copy_from_slice(&h.finish().to_le_bytes());
        }
    }
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn write_private_file(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(path);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(contents.as_bytes())?;
    f.write_all(b"\n")
}

/// Load the token stored at `path`, creating it (mode 0600) when missing or malformed.
pub fn load_or_create_token(path: &Path) -> std::io::Result<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let existing = existing.trim();
        if is_valid_token(existing) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
            return Ok(existing.to_string());
        }
    }
    let token = generate_token();
    write_private_file(path, &token)?;
    Ok(token)
}

/// The session token for this process, loaded from (or created at) `<config dir>/web_token`.
/// If the file cannot be written, a token valid for this run only is used.
pub fn web_token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        let path = crate::paths::get_web_token_path();
        load_or_create_token(&path).unwrap_or_else(|e| {
            crate::log_info!("web", "Could not persist web token at {}: {}", path.display(), e);
            generate_token()
        })
    })
}

/// The link that pairs a browser with this Quickstream instance.
pub fn pairing_url(host: &str, port: u16, token: &str) -> String {
    format!("http://{}:{}/?token={}", host, port, token)
}

// ---------------------------------------------------------------------------------------------
// Allowed folders
// ---------------------------------------------------------------------------------------------

/// Whether `root` may be an allowed folder for the web dashboard. The filesystem root, the home
/// folder `home` and folders containing it are too broad and never accepted, nor are relative
/// paths.
///
/// The check is lexical (no filesystem access, so the UI thread may call it);
/// [`resolve_allowed_path`] repeats it on the canonicalized roots.
pub fn is_acceptable_root(root: &Path, home: Option<&Path>) -> bool {
    if !root.is_absolute() {
        return false;
    }
    let root = normalize_lexically(root);
    if root.parent().is_none() {
        return false;
    }
    match home {
        Some(home) => !normalize_lexically(home).starts_with(&root),
        None => true,
    }
}

/// Resolve `.` and `..` components without touching the filesystem.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The current user's home folder.
pub fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

/// Canonicalize `path` and return it if it lies inside one of `roots` (also canonicalized).
/// Relative paths, missing paths and symlinks leading out of the roots are refused, and so are
/// roots that resolve to the filesystem root or the home folder (see [`is_acceptable_root`]).
pub fn resolve_allowed_path(path: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let canonical = std::fs::canonicalize(path).ok()?;
    let home = home_dir().map(|h| std::fs::canonicalize(&h).unwrap_or(h));
    roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .filter(|root| is_acceptable_root(root, home.as_deref()))
        .any(|root| canonical.starts_with(&root))
        .then_some(canonical)
}
