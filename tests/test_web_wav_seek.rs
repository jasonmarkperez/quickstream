//! Seeking in a WAV over `/stream/<name>?pos=` (issue #108): the new RIFF header keeps the file's
//! own `fmt ` chunk, sizes match the bytes sent, only the `data` chunk is sent, and a corrupt
//! header falls back to the whole file.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use quickstream::web::{wav_header_sizes, NowPlayingState, WebServer, WebServerHandle};

const RATE: u32 = 8000;

/// A plain PCM `fmt ` chunk body (16 bytes).
fn pcm_fmt(channels: u16, bits: u16) -> Vec<u8> {
    let align = channels * bits.div_ceil(8);
    let mut f = Vec::new();
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&channels.to_le_bytes());
    f.extend_from_slice(&RATE.to_le_bytes());
    f.extend_from_slice(&(RATE * align as u32).to_le_bytes());
    f.extend_from_slice(&align.to_le_bytes());
    f.extend_from_slice(&bits.to_le_bytes());
    f
}

/// A WAVE_FORMAT_EXTENSIBLE `fmt ` chunk body (40 bytes), as ffmpeg writes for 24-bit stereo.
fn extensible_fmt_24() -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&0xFFFEu16.to_le_bytes());
    f.extend_from_slice(&2u16.to_le_bytes());
    f.extend_from_slice(&RATE.to_le_bytes());
    f.extend_from_slice(&(RATE * 6).to_le_bytes());
    f.extend_from_slice(&6u16.to_le_bytes());
    f.extend_from_slice(&24u16.to_le_bytes());
    f.extend_from_slice(&22u16.to_le_bytes()); // cbSize
    f.extend_from_slice(&24u16.to_le_bytes()); // valid bits
    f.extend_from_slice(&3u32.to_le_bytes()); // channel mask: front left | front right
                                              // KSDATAFORMAT_SUBTYPE_PCM
    f.extend_from_slice(&[
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
    ]);
    assert_eq!(f.len(), 40);
    f
}

fn chunk(id: &[u8; 4], size_field: u32, body: &[u8]) -> Vec<u8> {
    let mut c = Vec::new();
    c.extend_from_slice(id);
    c.extend_from_slice(&size_field.to_le_bytes());
    c.extend_from_slice(body);
    if body.len() % 2 == 1 {
        c.push(0);
    }
    c
}

/// `RIFF` + `WAVE` around `chunks`, with the RIFF size of what follows it.
fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = chunks.concat();
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(4 + body.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(&body);
    w
}

/// Audio bytes whose value depends on their position, so a wrong offset shows.
fn pcm(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 % 251) as u8).collect()
}

fn list_chunk() -> Vec<u8> {
    chunk(b"LIST", 21, b"INFOINAM\x09\x00\x00\x00Trailer!\x00")
}

/// A web server on 127.0.0.1 with `bytes` registered as `name`.
fn serve(tag: &str, name: &str, bytes: &[u8]) -> (WebServerHandle, PathBuf) {
    let dir = common::isolate().join(format!("wav_seek_{}", tag));
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join(name);
    fs::write(&file, bytes).unwrap();
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    handle.register_stream_file(&file);
    (handle, dir)
}

fn get(port: u16, path: &str, range: Option<&str>) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let range = range.map(|r| format!("Range: {}\r\n", r)).unwrap_or_default();
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{}Connection: close\r\n\r\n",
        path, port, range
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut resp = Vec::new();
    let _ = stream.read_to_end(&mut resp);
    let end = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no response to {} (connection closed after {} bytes)", path, resp.len()));
    let headers = String::from_utf8_lossy(&resp[..end]).to_string();
    let status = headers
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, headers, resp[end + 4..].to_vec())
}

fn header(headers: &str, name: &str) -> Option<String> {
    headers.lines().skip(1).find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
    })
}

/// What a WAV reader finds in a response body.
#[derive(Debug)]
struct ParsedWav {
    riff_size: u32,
    fmt: Vec<u8>,
    data_size: u32,
    /// Where the audio starts in the body.
    data_offset: usize,
}

/// Parse `body` as a WAV the way a strict reader does: chunk by chunk, odd chunks padded.
fn parse_wav(body: &[u8]) -> ParsedWav {
    assert!(
        body.len() >= 12,
        "body too short for a RIFF header: {} bytes",
        body.len()
    );
    assert_eq!(&body[0..4], b"RIFF");
    assert_eq!(&body[8..12], b"WAVE");
    let riff_size = u32::from_le_bytes(body[4..8].try_into().unwrap());
    let mut pos = 12usize;
    let mut fmt = None;
    loop {
        assert!(pos + 8 <= body.len(), "no data chunk in the header");
        let id = &body[pos..pos + 4];
        let size = u32::from_le_bytes(body[pos + 4..pos + 8].try_into().unwrap());
        if id == b"data" {
            return ParsedWav {
                riff_size,
                fmt: fmt.expect("fmt chunk before data"),
                data_size: size,
                data_offset: pos + 8,
            };
        }
        let end = pos + 8 + size as usize;
        assert!(
            end <= body.len(),
            "chunk {:?} runs past the body",
            String::from_utf8_lossy(id)
        );
        if id == b"fmt " {
            fmt = Some(body[pos + 8..end].to_vec());
        }
        pos = end + (size as usize % 2);
    }
}

/// `?pos=<pos>` of a file whose audio is `audio`: a valid WAV with the file's own `fmt ` chunk,
/// sizes that match the body, and the audio from `pos` rounded down to a whole block.
fn assert_seeked(port: u16, name: &str, pos: &str, fmt: &[u8], audio: &[u8]) -> Vec<u8> {
    let (status, headers, body) = get(port, &format!("/stream/{}?pos={}", name, pos), None);
    assert_eq!(status, 200, "{}?pos={}: {}", name, pos, headers);
    assert_eq!(
        header(&headers, "Content-Length").as_deref(),
        Some(body.len().to_string().as_str()),
        "{}?pos={}: Content-Length is the body sent",
        name,
        pos
    );
    let wav = parse_wav(&body);
    assert_eq!(wav.fmt, fmt, "{}?pos={}: the file's own fmt chunk", name, pos);
    assert_eq!(
        wav.riff_size as usize,
        body.len() - 8,
        "{}?pos={}: RIFF size",
        name,
        pos
    );
    assert_eq!(
        wav.data_size as usize,
        body.len() - wav.data_offset,
        "{}?pos={}: data size",
        name,
        pos
    );

    // PCM and extensible: the byte rate is the sample rate times the block, whatever the file says.
    let rate = u32::from_le_bytes(fmt[4..8].try_into().unwrap()) as usize;
    let align = u16::from_le_bytes(fmt[12..14].try_into().unwrap()) as usize;
    let wanted = (pos.parse::<f64>().unwrap() * (rate * align) as f64) as usize;
    let whole = audio.len() / align * align;
    let offset = (wanted / align * align).min(whole);
    assert_eq!(offset % align, 0);
    let sent = &body[wav.data_offset..];
    assert_eq!(sent.len() % align, 0, "{}?pos={}: whole frames only", name, pos);
    assert_eq!(sent.len(), whole - offset, "{}?pos={}: audio length", name, pos);
    assert!(
        sent == &audio[offset..whole],
        "{}?pos={}: audio from byte {}",
        name,
        pos,
        offset
    );
    body
}

#[test]
fn test_wav_seek_keeps_extensible_fmt_chunk() {
    common::isolate();
    let fmt = extensible_fmt_24();
    let audio = pcm(RATE as usize * 6 * 2);
    let file = riff(&[chunk(b"fmt ", 40, &fmt), chunk(b"data", audio.len() as u32, &audio)]);
    let (handle, dir) = serve("extensible", "ext24.wav", &file);

    // 1.0001 s is 48004.8 bytes in: the audio starts at 48000, a whole 6-byte frame.
    let body = assert_seeked(handle.port, "ext24.wav", "1.0001", &fmt, &audio);
    assert_eq!(parse_wav(&body).fmt.len(), 40, "cbSize, valid bits, mask and GUID kept");
    assert_seeked(handle.port, "ext24.wav", "0.5", &fmt, &audio);

    handle.stop();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_wav_seek_stops_at_data_chunk_end() {
    common::isolate();
    // A trailing LIST chunk, an odd-sized chunk before fmt, and an odd-sized fmt chunk.
    let mut fmt17 = pcm_fmt(2, 16);
    fmt17.push(0x5A);
    let cases: Vec<(&str, Vec<u8>, Vec<u8>)> = vec![
        ("pcm16.wav", pcm_fmt(2, 16), pcm(RATE as usize * 4 * 2)),
        ("pcm24.wav", pcm_fmt(2, 24), pcm(RATE as usize * 6 * 2)),
        ("oddfmt.wav", fmt17, pcm(RATE as usize * 4 * 2)),
    ];
    for (name, fmt, audio) in cases {
        let file = riff(&[
            chunk(b"JUNK", 3, b"abc"),
            chunk(b"fmt ", fmt.len() as u32, &fmt),
            chunk(b"data", audio.len() as u32, &audio),
            list_chunk(),
        ]);
        let (handle, dir) = serve(name, name, &file);
        let port = handle.port;

        let full = assert_seeked(port, name, "1.0001", &fmt, &audio);
        assert_seeked(port, name, "0.33", &fmt, &audio);
        assert_seeked(port, name, "0.0001", &fmt, &audio);
        // Past the end: the header and no audio.
        assert_seeked(port, name, "60", &fmt, &audio);

        // Ranges of the seeked stream index the same representation.
        let path = format!("/stream/{}?pos=1.0001", name);
        let (status, headers, body) = get(port, &path, Some("bytes=50-149"));
        assert_eq!(status, 206, "{}: {}", name, headers);
        assert_eq!(
            header(&headers, "Content-Range"),
            Some(format!("bytes 50-149/{}", full.len()))
        );
        assert_eq!(header(&headers, "Content-Length").as_deref(), Some("100"));
        assert_eq!(body, &full[50..150]);
        let (status, headers, body) = get(port, &path, Some("bytes=-10"));
        assert_eq!(status, 206, "{}: {}", name, headers);
        assert_eq!(
            body,
            &full[full.len() - 10..],
            "{}: the last bytes are audio, not the LIST chunk",
            name
        );
        let (status, _, body) = get(port, &path, Some("bytes=10-"));
        assert_eq!(status, 206);
        assert_eq!(body, &full[10..]);

        handle.stop();
        let _ = fs::remove_dir_all(&dir);
    }
}

#[test]
fn test_wav_seek_unknown_data_size_runs_to_end_of_file() {
    common::isolate();
    let fmt = pcm_fmt(2, 16);
    // 2 s of audio, then the same with a trailing partial frame, which is never sent.
    let whole = RATE as usize * 4 * 2;
    for (name, size_field, audio_len) in [
        ("unknown_ff.wav", u32::MAX, whole),
        ("unknown_zero.wav", 0, whole),
        ("unknown_ff_partial.wav", u32::MAX, whole + 1),
        ("unknown_zero_partial.wav", 0, whole + 2),
    ] {
        let audio = pcm(audio_len);
        let file = riff(&[chunk(b"fmt ", 16, &fmt), chunk(b"data", size_field, &audio)]);
        let (handle, dir) = serve(name, name, &file);

        for pos in ["0.0001", "1.0001", "60"] {
            let (status, headers, body) = get(handle.port, &format!("/stream/{}?pos={}", name, pos), None);
            assert_eq!(status, 200, "{}?pos={}: {}", name, pos, headers);
            assert_eq!(header(&headers, "Content-Length"), Some(body.len().to_string()));
            let wav = parse_wav(&body);
            assert_eq!(wav.fmt, fmt);
            assert_eq!(wav.riff_size, u32::MAX, "{}?pos={}: length still unknown", name, pos);
            assert_eq!(wav.data_size, u32::MAX, "{}?pos={}: length still unknown", name, pos);
            let wanted = (pos.parse::<f64>().unwrap() * (RATE * 4) as f64) as usize;
            let offset = (wanted / 4 * 4).min(whole);
            assert_eq!(
                (body.len() - wav.data_offset) % 4,
                0,
                "{}?pos={}: whole frames only",
                name,
                pos
            );
            assert!(
                body[wav.data_offset..] == audio[offset..whole],
                "{}?pos={}: audio from {}",
                name,
                pos,
                offset
            );
        }

        handle.stop();
        let _ = fs::remove_dir_all(&dir);
    }
}

#[test]
fn test_wav_seek_truncated_data_chunk() {
    common::isolate();
    let fmt = pcm_fmt(2, 16);
    // The data chunk says 2 s, but the file ends after 30001 bytes of it (not a whole frame).
    let audio = pcm(30001);
    let mut file = riff(&[chunk(b"fmt ", 16, &fmt)]);
    file.extend_from_slice(b"data");
    file.extend_from_slice(&(RATE * 4 * 2).to_le_bytes());
    file.extend_from_slice(&audio);
    let (handle, dir) = serve("truncated", "truncated.wav", &file);

    // Whole frames only: the stray last byte is left out of the audio and its size.
    let body = assert_seeked(handle.port, "truncated.wav", "0.5", &fmt, &audio);
    assert_eq!(parse_wav(&body).data_size, 30000 - 16000);
    assert_seeked(handle.port, "truncated.wav", "0.0001", &fmt, &audio);
    // Past what the file holds: from its last whole frame, so no audio.
    let body = assert_seeked(handle.port, "truncated.wav", "1.5", &fmt, &audio);
    assert_eq!(parse_wav(&body).data_size, 0);

    handle.stop();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_wav_seek_data_size_not_whole_frames() {
    common::isolate();
    let fmt = pcm_fmt(2, 16);
    // A data chunk of 30001 bytes (odd, so padded) with a LIST chunk after it.
    let audio = pcm(30001);
    let file = riff(&[
        chunk(b"fmt ", 16, &fmt),
        chunk(b"data", audio.len() as u32, &audio),
        list_chunk(),
    ]);
    let (handle, dir) = serve("partial_frame", "partial.wav", &file);

    let body = assert_seeked(handle.port, "partial.wav", "0.5", &fmt, &audio);
    assert_eq!(parse_wav(&body).data_size, 30000 - 16000);
    assert_seeked(handle.port, "partial.wav", "0.9", &fmt, &audio);

    handle.stop();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_wav_seek_byte_rate_from_sample_rate_and_block() {
    common::isolate();
    // 16-bit stereo at 44.1 kHz whose nAvgBytesPerSec says 88200 (it is 176400): PCM and
    // WAVE_FORMAT_EXTENSIBLE seek by the sample rate times the block align.
    let audio = pcm(44_100 * 4 * 2);
    let mut pcm16 = Vec::new();
    pcm16.extend_from_slice(&1u16.to_le_bytes());
    pcm16.extend_from_slice(&2u16.to_le_bytes());
    pcm16.extend_from_slice(&44_100u32.to_le_bytes());
    pcm16.extend_from_slice(&88_200u32.to_le_bytes());
    pcm16.extend_from_slice(&4u16.to_le_bytes());
    pcm16.extend_from_slice(&16u16.to_le_bytes());
    let mut ext = extensible_fmt_24();
    ext[4..8].copy_from_slice(&44_100u32.to_le_bytes());
    ext[8..12].copy_from_slice(&88_200u32.to_le_bytes());
    ext[12..14].copy_from_slice(&4u16.to_le_bytes());
    ext[14..16].copy_from_slice(&16u16.to_le_bytes());
    ext[18..20].copy_from_slice(&16u16.to_le_bytes());
    for (name, fmt) in [("rate_pcm.wav", pcm16), ("rate_ext.wav", ext)] {
        let file = riff(&[
            chunk(b"fmt ", fmt.len() as u32, &fmt),
            chunk(b"data", audio.len() as u32, &audio),
        ]);
        let (handle, dir) = serve(name, name, &file);

        let body = assert_seeked(handle.port, name, "1", &fmt, &audio);
        let wav = parse_wav(&body);
        assert!(
            body[wav.data_offset..] == audio[176_400..],
            "{}?pos=1: the audio starts at byte 176400",
            name
        );

        handle.stop();
        let _ = fs::remove_dir_all(&dir);
    }
}

#[test]
fn test_wav_seek_header_sizes() {
    common::isolate();
    // "WAVE", the fmt chunk's header and body (padded to even), the data chunk's header, the audio.
    assert_eq!(wav_header_sizes(16, 1000, false), (4 + 8 + 16 + 8 + 1000, 1000));
    assert_eq!(wav_header_sizes(17, 1000, false), (4 + 8 + 18 + 8 + 1000, 1000));
    assert_eq!(wav_header_sizes(16, 1000, true), (u32::MAX, u32::MAX));
    // Audio whose RIFF size only just fits.
    let fits = u64::from(u32::MAX) - 37;
    assert_eq!(wav_header_sizes(16, fits, false), (u32::MAX - 1, fits as u32));
    // The data size fits but the RIFF size doesn't: both unknown, never a mix.
    assert_eq!(wav_header_sizes(16, fits + 2, false), (u32::MAX, u32::MAX));
    // A RIFF size of exactly u32::MAX would read as unknown next to a known data size.
    assert_eq!(wav_header_sizes(16, fits + 1, false), (u32::MAX, u32::MAX));
    assert_eq!(
        wav_header_sizes(40, u64::from(u32::MAX) - 50, false),
        (u32::MAX, u32::MAX)
    );
    assert_eq!(
        wav_header_sizes(16, u64::from(u32::MAX) + 1, false),
        (u32::MAX, u32::MAX)
    );
}

#[test]
fn test_wav_seek_corrupt_header_serves_whole_file() {
    common::isolate();
    let audio = pcm(RATE as usize * 4);
    let good = pcm_fmt(2, 16);
    // A format other than PCM / extensible (IEEE float) seeks by nAvgBytesPerSec, here 0.
    let mut zero_rate = good.clone();
    zero_rate[0..2].copy_from_slice(&3u16.to_le_bytes());
    zero_rate[8..12].copy_from_slice(&0u32.to_le_bytes());
    let mut zero_sample_rate = good.clone();
    zero_sample_rate[4..8].copy_from_slice(&0u32.to_le_bytes());
    let mut zero_align = good.clone();
    zero_align[12..14].copy_from_slice(&0u16.to_le_bytes());

    // A huge fmt size (no allocation of it), one that runs past the end, a short one, zero byte
    // rate / sample rate / block align, no data chunk, data before fmt with an unknown length, a truncated
    // RIFF header and a file that isn't RIFF at all.
    let mut huge_fmt = riff(&[]);
    huge_fmt.extend_from_slice(b"fmt ");
    huge_fmt.extend_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
    huge_fmt.extend_from_slice(&good);
    huge_fmt.extend_from_slice(&chunk(b"data", audio.len() as u32, &audio));
    let mut past_end = riff(&[chunk(b"data", audio.len() as u32, &audio)]);
    past_end.extend_from_slice(b"fmt ");
    past_end.extend_from_slice(&40u32.to_le_bytes());
    past_end.extend_from_slice(&good);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("huge_fmt.wav", huge_fmt),
        ("fmt_past_end.wav", past_end),
        (
            "short_fmt.wav",
            riff(&[
                chunk(b"fmt ", 14, &good[..14]),
                chunk(b"data", audio.len() as u32, &audio),
            ]),
        ),
        (
            "zero_rate.wav",
            riff(&[
                chunk(b"fmt ", 16, &zero_rate),
                chunk(b"data", audio.len() as u32, &audio),
            ]),
        ),
        (
            "zero_sample_rate.wav",
            riff(&[
                chunk(b"fmt ", 16, &zero_sample_rate),
                chunk(b"data", audio.len() as u32, &audio),
            ]),
        ),
        (
            "zero_align.wav",
            riff(&[
                chunk(b"fmt ", 16, &zero_align),
                chunk(b"data", audio.len() as u32, &audio),
            ]),
        ),
        ("no_data.wav", riff(&[chunk(b"fmt ", 16, &good), list_chunk()])),
        (
            "data_first.wav",
            riff(&[chunk(b"data", u32::MAX, &audio), chunk(b"fmt ", 16, &good)]),
        ),
        ("truncated_riff.wav", b"RIFF\x24\x00".to_vec()),
        ("not_riff.wav", audio.clone()),
    ];
    for (name, file) in cases {
        let (handle, dir) = serve(name, name, &file);
        for pos in ["1.0", "0.0001"] {
            let (status, headers, body) = get(handle.port, &format!("/stream/{}?pos={}", name, pos), None);
            assert_eq!(status, 200, "{}?pos={}: {}", name, pos, headers);
            assert_eq!(header(&headers, "Content-Length"), Some(file.len().to_string()));
            assert!(body == file, "{}?pos={}: the whole file, unchanged", name, pos);
        }
        handle.stop();
        let _ = fs::remove_dir_all(&dir);
    }
}
