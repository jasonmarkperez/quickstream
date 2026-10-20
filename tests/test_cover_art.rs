//! Cover art served by `/api/cover`: only real image types go out as `Content-Type`, whatever
//! MIME string the audio file's tag carries.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use quickstream::metadata::{cover_image_mime, extract_cover_art};
use quickstream::web::{NowPlayingState, WebServer};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x02\x00\x00\x00";
const JPEG: &[u8] = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00";
const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff";
const WEBP: &[u8] = b"RIFF\x1a\x00\x00\x00WEBPVP8 \x0e\x00\x00\x00";
const HTML: &[u8] = b"<html><script>alert(document.cookie)</script></html>";
const SVG: &[u8] = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>";

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qs_cover_art_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// One second of silent mono 16-bit PCM.
fn write_wav(path: &Path) {
    let samples = 8000u32;
    let data_len = samples * 2;
    let mut b: Vec<u8> = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVE");
    b.extend_from_slice(b"fmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&8000u32.to_le_bytes());
    b.extend_from_slice(&16000u32.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    b.resize(b.len() + data_len as usize, 0);
    fs::write(path, b).unwrap();
}

/// A WAV whose ID3v2 tag embeds `data` as the front cover with the given MIME string.
fn write_wav_with_picture(path: &Path, mime: &str, data: &[u8]) {
    use lofty::config::WriteOptions;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::prelude::*;
    use lofty::tag::{Tag, TagType};

    write_wav(path);
    let mut tag = Tag::new(TagType::Id3v2);
    tag.set_title("Cover Test".to_string());
    let picture = Picture::unchecked(data.to_vec())
        .pic_type(PictureType::CoverFront)
        .mime_type(MimeType::Unknown(mime.to_string()))
        .build();
    tag.push_picture(picture);
    tag.save_to_path(path, WriteOptions::default()).unwrap();
}

// ---------------------------------------------------------------------------------------------
// MIME selection
// ---------------------------------------------------------------------------------------------

#[test]
fn test_cover_mime_accepts_allowlisted_tag_types() {
    common::isolate();
    assert_eq!(cover_image_mime(Some("image/jpeg"), JPEG), Some("image/jpeg"));
    assert_eq!(cover_image_mime(Some("image/png"), PNG), Some("image/png"));
    assert_eq!(cover_image_mime(Some("image/webp"), WEBP), Some("image/webp"));
    assert_eq!(cover_image_mime(Some("image/gif"), GIF), Some("image/gif"));
    assert_eq!(cover_image_mime(Some(" IMAGE/JPG "), JPEG), Some("image/jpeg"));
}

#[test]
fn test_cover_mime_rejects_hostile_tag_types_for_non_image_data() {
    common::isolate();
    assert_eq!(cover_image_mime(Some("text/html"), HTML), None);
    assert_eq!(cover_image_mime(Some("image/png\r\nSet-Cookie: x=1"), HTML), None);
    assert_eq!(cover_image_mime(Some("image/svg+xml"), SVG), None);
    assert_eq!(cover_image_mime(Some(""), HTML), None);
    assert_eq!(cover_image_mime(None, HTML), None);
    assert_eq!(cover_image_mime(Some("application/octet-stream"), b""), None);
}

#[test]
fn test_cover_mime_uses_magic_bytes_when_tag_type_is_missing_or_unknown() {
    common::isolate();
    assert_eq!(cover_image_mime(None, JPEG), Some("image/jpeg"));
    assert_eq!(cover_image_mime(Some(""), PNG), Some("image/png"));
    assert_eq!(cover_image_mime(Some("text/html"), PNG), Some("image/png"));
    assert_eq!(
        cover_image_mime(Some("image/png\r\nSet-Cookie: x=1"), PNG),
        Some("image/png")
    );
    assert_eq!(
        cover_image_mime(Some("application/octet-stream"), GIF),
        Some("image/gif")
    );
    assert_eq!(cover_image_mime(Some("image/x-unknown"), WEBP), Some("image/webp"));
}

#[test]
fn test_cover_mime_trusts_the_image_bytes_over_the_tag() {
    common::isolate();
    // A PNG labelled as JPEG goes out as PNG.
    assert_eq!(cover_image_mime(Some("image/jpeg"), PNG), Some("image/png"));
}

// ---------------------------------------------------------------------------------------------
// extract_cover_art with real tagged files
// ---------------------------------------------------------------------------------------------

#[test]
fn test_embedded_picture_with_hostile_mime_is_not_served() {
    common::isolate();
    let dir = temp_dir("hostile_embedded");
    for (i, mime) in ["text/html", "image/png\r\nSet-Cookie: x=1", "", "image/svg+xml"]
        .iter()
        .enumerate()
    {
        let track = dir.join(format!("track{}.wav", i));
        write_wav_with_picture(&track, mime, HTML);
        assert_eq!(extract_cover_art(&track), None, "mime {:?}", mime);
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_embedded_picture_with_hostile_mime_falls_back_to_folder_art() {
    common::isolate();
    let dir = temp_dir("folder_fallback");
    let track = dir.join("track.wav");
    write_wav_with_picture(&track, "text/html", HTML);
    fs::write(dir.join("cover.png"), PNG).unwrap();
    let (bytes, mime) = extract_cover_art(&track).expect("folder art");
    assert_eq!(mime, "image/png");
    assert_eq!(bytes, PNG);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_embedded_image_with_hostile_mime_is_served_with_detected_type() {
    common::isolate();
    let dir = temp_dir("detected");
    let track = dir.join("track.wav");
    write_wav_with_picture(&track, "image/png\r\nSet-Cookie: x=1", PNG);
    let (bytes, mime) = extract_cover_art(&track).expect("embedded art");
    assert_eq!(mime, "image/png");
    assert_eq!(bytes, PNG);
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// /api/cover over 127.0.0.1
// ---------------------------------------------------------------------------------------------

fn get(port: u16, path: &str) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        path, port
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(raw.len());
    let headers = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let body = if header_end + 4 <= raw.len() {
        raw[header_end + 4..].to_vec()
    } else {
        Vec::new()
    };
    let status = headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, headers, body)
}

fn content_type(headers: &str) -> String {
    headers
        .lines()
        .find_map(|l| l.strip_prefix("Content-Type: "))
        .unwrap_or_default()
        .to_string()
}

fn start(filepath: Option<PathBuf>) -> quickstream::web::WebServerHandle {
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    state.write().unwrap().filepath = filepath;
    let (tx, _rx) = mpsc::channel();
    WebServer::start(0, state, tx).expect("start web server")
}

#[test]
fn test_api_cover_never_sends_tag_controlled_content_type() {
    common::isolate();
    let dir = temp_dir("api_hostile");
    for (i, mime) in ["text/html", "image/png\r\nSet-Cookie: x=1", ""].iter().enumerate() {
        let track = dir.join(format!("track{}.wav", i));
        write_wav_with_picture(&track, mime, HTML);
        let handle = start(Some(track));
        let (status, headers, body) = get(handle.port, "/api/cover");
        handle.stop();
        assert_eq!(status, 200, "mime {:?}", mime);
        assert_eq!(content_type(&headers), "image/svg+xml", "mime {:?}: {}", mime, headers);
        assert!(!headers.to_lowercase().contains("set-cookie"), "{}", headers);
        assert!(!String::from_utf8_lossy(&body).contains("alert(document.cookie)"));
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_api_cover_serves_embedded_image_with_nosniff() {
    common::isolate();
    let dir = temp_dir("api_image");
    let track = dir.join("track.wav");
    write_wav_with_picture(&track, "image/png\r\nSet-Cookie: x=1", PNG);
    let handle = start(Some(track));
    let (status, headers, body) = get(handle.port, "/api/cover");
    handle.stop();
    assert_eq!(status, 200);
    assert_eq!(content_type(&headers), "image/png", "{}", headers);
    assert!(!headers.to_lowercase().contains("set-cookie"), "{}", headers);
    assert!(
        headers.to_lowercase().contains("x-content-type-options: nosniff"),
        "{}",
        headers
    );
    assert_eq!(body, PNG);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_api_cover_placeholder_sends_nosniff() {
    common::isolate();
    let handle = start(None);
    let (status, headers, _) = get(handle.port, "/api/cover");
    handle.stop();
    assert_eq!(status, 200);
    assert_eq!(content_type(&headers), "image/svg+xml");
    assert!(
        headers.to_lowercase().contains("x-content-type-options: nosniff"),
        "{}",
        headers
    );
}

#[test]
fn test_dashboard_and_json_responses_send_nosniff() {
    common::isolate();
    let dir = temp_dir("json_nosniff");
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    // /api/browser answers 403 unless there is an allowed folder to list.
    state.write().unwrap().allowed_roots = vec![dir.clone()];
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    for path in ["/", "/api/status", "/api/browser"] {
        let (status, headers, _) = get(handle.port, path);
        assert_eq!(status, 200, "{}", path);
        assert!(
            headers.to_lowercase().contains("x-content-type-options: nosniff"),
            "{}: {}",
            path,
            headers
        );
    }
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", handle.port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let body = r#"{"action":"rescan_devices"}"#;
    let req = format!(
        "POST /api/action HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        handle.port,
        body.len(),
        body
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    handle.stop();
    let text = String::from_utf8_lossy(&raw).to_lowercase();
    assert!(text.starts_with("http/1.1 200"), "{}", text);
    assert!(text.contains("x-content-type-options: nosniff"), "{}", text);
    let _ = fs::remove_dir_all(&dir);
}
