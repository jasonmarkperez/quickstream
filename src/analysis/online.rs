//! Optional online metadata enrichment via Last.fm and MusicBrainz.
//! Never halts or blocks offline playback; all queries are cached into local SQLite.

use serde_json::Value;
use std::time::Duration;

/// Lightweight client for online music metadata and similarity graphs.
pub struct OnlineMetadataClient;

impl OnlineMetadataClient {
    /// Fetch community style and genre tags for an artist or track from Last.fm.
    pub fn fetch_artist_tags(artist: &str) -> Vec<String> {
        let clean_artist = artist.trim();
        if clean_artist.is_empty() {
            return Vec::new();
        }

        let encoded_artist = crate::player::upnp::url_encode(clean_artist);
        // Public Last.fm API endpoint for top tags
        let url = format!(
            "https://ws.audioscrobbler.com/2.0/?method=artist.gettoptags&artist={}&api_key=b25b959554ed76058ac220b7b2e0a026&format=json",
            encoded_artist
        );

        let resp = match ureq::get(&url)
            .timeout(Duration::from_secs(3))
            .call()
        {
            Ok(r) => r,
            Err(_) => return Vec::new(), // Gracefully return empty if offline or blocked
        };

        let json: Value = match resp.into_json() {
            Ok(j) => j,
            Err(_) => return Vec::new(),
        };

        let mut tags = Vec::new();
        if let Some(tag_array) = json.pointer("/toptags/tag").and_then(|t| t.as_array()) {
            for item in tag_array.iter().take(8) {
                if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                    let clean = name.to_lowercase().trim().to_string();
                    if !clean.is_empty() && clean.len() > 2 {
                        tags.push(clean);
                    }
                }
            }
        }

        tags
    }

    /// Fetch similar artists from Last.fm similarity graph.
    pub fn fetch_similar_artists(artist: &str) -> Vec<(String, f64)> {
        let clean_artist = artist.trim();
        if clean_artist.is_empty() {
            return Vec::new();
        }

        let encoded_artist = crate::player::upnp::url_encode(clean_artist);
        let url = format!(
            "https://ws.audioscrobbler.com/2.0/?method=artist.getsimilar&artist={}&limit=10&api_key=b25b959554ed76058ac220b7b2e0a026&format=json",
            encoded_artist
        );

        let resp = match ureq::get(&url)
            .timeout(Duration::from_secs(3))
            .call()
        {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        let json: Value = match resp.into_json() {
            Ok(j) => j,
            Err(_) => return Vec::new(),
        };

        let mut similar = Vec::new();
        if let Some(artists) = json.pointer("/similarartists/artist").and_then(|a| a.as_array()) {
            for item in artists {
                if let (Some(name), Some(match_score)) = (
                    item.get("name").and_then(|n| n.as_str()),
                    item.get("match").and_then(|m| m.as_str()).and_then(|s| s.parse::<f64>().ok()),
                ) {
                    similar.push((name.to_string(), match_score));
                }
            }
        }

        similar
    }
}
