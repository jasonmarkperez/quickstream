//! Optional online metadata enrichment via Last.fm.
//!
//! Off unless the user turns on `online_enrichment` and supplies a Last.fm API key through
//! the `QUICKSTREAM_LASTFM_API_KEY` environment variable; without both, no request is ever
//! made. Requests carry a descriptive User-Agent, are spaced at least `min_interval` apart,
//! and successful answers are cached in memory for the life of the client.
//!
//! Calls block (throttling and network), so run them on a background thread, never on the
//! UI thread.

use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Environment variable holding the Last.fm API key.
pub const API_KEY_ENV: &str = "QUICKSTREAM_LASTFM_API_KEY";

/// Last.fm web service endpoint.
pub const LASTFM_BASE_URL: &str = "https://ws.audioscrobbler.com/2.0/";

/// Minimum gap between two requests from one client.
pub const DEFAULT_MIN_INTERVAL: Duration = Duration::from_millis(250);

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

/// Largest response body read; anything longer is treated as a failed lookup.
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

/// Entries kept per cache before it is cleared.
const MAX_CACHE_ENTRIES: usize = 4096;

/// User-Agent sent with every request, as Last.fm asks clients to identify themselves.
pub fn user_agent() -> String {
    format!(
        "quickstream/{} (terminal music player; https://github.com/jasonmarkperez/quickstream)",
        env!("CARGO_PKG_VERSION")
    )
}

/// How the online client may behave.
#[derive(Debug, Clone, PartialEq)]
pub struct OnlineConfig {
    /// The user opted in to online enrichment.
    pub enabled: bool,
    /// Last.fm API key; `None` (or blank) disables the client.
    pub api_key: Option<String>,
    /// Web service endpoint (overridden in tests).
    pub base_url: String,
    /// Minimum gap between requests.
    pub min_interval: Duration,
    /// Per-request timeout.
    pub timeout: Duration,
}

impl OnlineConfig {
    /// Opt-in from `settings.online_enrichment`, API key from `QUICKSTREAM_LASTFM_API_KEY`.
    pub fn from_settings(settings: &crate::settings::Settings) -> Self {
        let api_key = std::env::var(API_KEY_ENV)
            .ok()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty());
        Self {
            enabled: settings.online_enrichment,
            api_key,
            base_url: LASTFM_BASE_URL.to_string(),
            min_interval: DEFAULT_MIN_INTERVAL,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// Throttled, caching client for Last.fm artist tags and similar artists.
pub struct OnlineMetadataClient {
    config: OnlineConfig,
    agent: ureq::Agent,
    /// When the previous request started; held across a request to serialize them.
    last_request: Mutex<Option<Instant>>,
    tag_cache: Mutex<HashMap<String, Vec<String>>>,
    similar_cache: Mutex<HashMap<String, Vec<(String, f64)>>>,
}

impl OnlineMetadataClient {
    pub fn new(config: OnlineConfig) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(config.timeout)
            .user_agent(&user_agent())
            .build();
        Self {
            config,
            agent,
            last_request: Mutex::new(None),
            tag_cache: Mutex::new(HashMap::new()),
            similar_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn from_settings(settings: &crate::settings::Settings) -> Self {
        Self::new(OnlineConfig::from_settings(settings))
    }

    /// True when the user opted in and an API key is configured.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled && self.api_key().is_some()
    }

    fn api_key(&self) -> Option<&str> {
        self.config.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty())
    }

    /// Community genre/style tags for an artist (lowercase, at most 8). Empty when disabled,
    /// offline, or the lookup fails.
    pub fn fetch_artist_tags(&self, artist: &str) -> Vec<String> {
        let Some(key) = cache_key(artist) else {
            return Vec::new();
        };
        if !self.is_enabled() {
            return Vec::new();
        }
        if let Some(hit) = self.tag_cache.lock().unwrap().get(&key) {
            return hit.clone();
        }
        let Some(json) = self.request("artist.gettoptags", artist.trim(), "") else {
            return Vec::new();
        };
        let tags = parse_top_tags(&json);
        insert_bounded(&self.tag_cache, key, tags.clone());
        tags
    }

    /// Similar artists with Last.fm match scores (0.0–1.0, at most 10). Empty when disabled,
    /// offline, or the lookup fails.
    pub fn fetch_similar_artists(&self, artist: &str) -> Vec<(String, f64)> {
        let Some(key) = cache_key(artist) else {
            return Vec::new();
        };
        if !self.is_enabled() {
            return Vec::new();
        }
        if let Some(hit) = self.similar_cache.lock().unwrap().get(&key) {
            return hit.clone();
        }
        let Some(json) = self.request("artist.getsimilar", artist.trim(), "&limit=10") else {
            return Vec::new();
        };
        let similar = parse_similar_artists(&json);
        insert_bounded(&self.similar_cache, key, similar.clone());
        similar
    }

    /// One throttled GET. `None` on any network, HTTP, size, parse or Last.fm API error, so
    /// failures are never cached.
    fn request(&self, method: &str, artist: &str, extra: &str) -> Option<Value> {
        let api_key = self.api_key()?;
        let url = format!(
            "{}?method={}&artist={}{}&api_key={}&format=json",
            self.config.base_url,
            method,
            crate::player::upnp::url_encode(artist),
            extra,
            crate::player::upnp::url_encode(api_key)
        );

        let mut last = self.last_request.lock().unwrap();
        if let Some(prev) = *last {
            let since = prev.elapsed();
            if since < self.config.min_interval {
                std::thread::sleep(self.config.min_interval - since);
            }
        }
        *last = Some(Instant::now());
        let resp = self.agent.get(&url).call();
        drop(last);

        let resp = resp.ok()?;
        let mut body = Vec::new();
        resp.into_reader()
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut body)
            .ok()?;
        if body.len() as u64 > MAX_RESPONSE_BYTES {
            return None;
        }
        let json: Value = serde_json::from_slice(&body).ok()?;
        if json.get("error").is_some() {
            return None;
        }
        Some(json)
    }
}

/// Cache key for an artist name; `None` for a blank name.
fn cache_key(artist: &str) -> Option<String> {
    let trimmed = artist.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_lowercase())
}

fn insert_bounded<V>(cache: &Mutex<HashMap<String, V>>, key: String, value: V) {
    let mut cache = cache.lock().unwrap();
    if cache.len() >= MAX_CACHE_ENTRIES {
        cache.clear();
    }
    cache.insert(key, value);
}

/// Tags from an `artist.gettoptags` response: lowercase, longer than 2 characters, at most 8.
pub fn parse_top_tags(json: &Value) -> Vec<String> {
    let mut tags = Vec::new();
    if let Some(tag_array) = json.pointer("/toptags/tag").and_then(|t| t.as_array()) {
        for item in tag_array.iter().take(8) {
            if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                let clean = name.trim().to_lowercase();
                if clean.len() > 2 {
                    tags.push(clean);
                }
            }
        }
    }
    tags
}

/// (name, match score) pairs from an `artist.getsimilar` response; entries without a numeric
/// score are skipped.
pub fn parse_similar_artists(json: &Value) -> Vec<(String, f64)> {
    let mut similar = Vec::new();
    if let Some(artists) = json.pointer("/similarartists/artist").and_then(|a| a.as_array()) {
        for item in artists {
            let name = item.get("name").and_then(|n| n.as_str());
            let score = item.get("match").and_then(|m| match m {
                Value::String(s) => s.parse::<f64>().ok(),
                Value::Number(n) => n.as_f64(),
                _ => None,
            });
            if let (Some(name), Some(score)) = (name, score) {
                similar.push((name.to_string(), score));
            }
        }
    }
    similar
}
