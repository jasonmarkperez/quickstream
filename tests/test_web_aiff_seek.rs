//! Seeking in an AIFF or AIFC over `/stream/<name>?pos=` (issue #114): a new `FORM` header with
//! the file's own `COMM` chunk (its frame count set to what is sent), an `SSND` chunk with offset
//! and block size 0, and the sound data alone in whole frames from the frame-aligned offset.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use quickstream::web::{NowPlayingState, WebServer, WebServerHandle};

/// A sample rate as the 80-bit IEEE extended float `COMM` stores (whole rates only).
fn extended(rate: u32) -> [u8; 10] {
    let e = 31 - rate.leading_zeros();
    let mut b = [0u8; 10];
    b[0..2].copy_from_slice(&(16383 + e as u16).to_be_bytes());
    b[2..10].copy_from_slice(&(u64::from(rate) << (63 - e)).to_be_bytes());
    b
}

/// A plain AIFF `COMM` body (18 bytes).
fn comm(channels: u16, frames: u32, bits: u16, rate: u32) -> Vec<u8> {
    let mut c = Vec::new();
    c.extend_from_slice(&channels.to_be_bytes());
    c.extend_from_slice(&frames.to_be_bytes());
    c.extend_from_slice(&bits.to_be_bytes());
    c.extend_from_slice(&extended(rate));
    c
}

/// An AIFC `COMM` body: the AIFF fields, the compression type and its Pascal-string name.
fn comm_aifc(channels: u16, frames: u32, bits: u16, rate: u32, kind: &[u8; 4], name: &str) -> Vec<u8> {
    let mut c = comm(channels, frames, bits, rate);
    c.extend_from_slice(kind);
    c.push(name.len() as u8);
    c.extend_from_slice(name.as_bytes());
    // The Pascal string (count byte and text) is padded to an even length.
    if (name.len() + 1) % 2 == 1 {
        c.push(0);
    }
    c
}

fn chunk(id: &[u8; 4], size_field: u32, body: &[u8]) -> Vec<u8> {
    let mut c = Vec::new();
    c.extend_from_slice(id);
    c.extend_from_slice(&size_field.to_be_bytes());
    c.extend_from_slice(body);
    if body.len() % 2 == 1 {
        c.push(0);
    }
    c
}

/// An `SSND` chunk: `offset` filler bytes before `audio`.
fn ssnd(offset: u32, audio: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&offset.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend(std::iter::repeat_n(0xEEu8, offset as usize));
    body.extend_from_slice(audio);
    chunk(b"SSND", body.len() as u32, &body)
}

/// `FORM` + `form_type` around `chunks`, with the FORM size of what follows it.
fn form(form_type: &[u8; 4], chunks: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = chunks.concat();
    let mut f = Vec::new();
    f.extend_from_slice(b"FORM");
    f.extend_from_slice(&(4 + body.len() as u32).to_be_bytes());
    f.extend_from_slice(form_type);
    f.extend_from_slice(&body);
    f
}

fn fver() -> Vec<u8> {
    chunk(b"FVER", 4, &0xA280_5140u32.to_be_bytes())
}

/// Audio bytes whose value depends on their position, so a wrong offset shows.
fn audio(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 % 251) as u8).collect()
}

/// A web server on 127.0.0.1 with `bytes` registered as `name`.
fn serve(tag: &str, name: &str, bytes: &[u8]) -> (WebServerHandle, PathBuf) {
    let dir = common::isolate().join(format!("aiff_seek_{}", tag));
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

/// What an AIFF reader finds in a response body.
#[derive(Debug)]
struct ParsedAiff {
    form_type: [u8; 4],
    comm: Vec<u8>,
    has_fver: bool,
    /// The sound data, after the SSND offset field's filler.
    audio: Vec<u8>,
}

/// Parse `body` as an AIFF the way a strict reader does: the FORM size is the body, every chunk
/// fits inside it, odd chunks are padded, and the last chunk ends where the body does.
fn parse_aiff(body: &[u8]) -> ParsedAiff {
    assert!(
        body.len() >= 12,
        "body too short for a FORM header: {} bytes",
        body.len()
    );
    assert_eq!(&body[0..4], b"FORM");
    let form_size = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
    assert_eq!(form_size, body.len() - 8, "FORM size is the body after it");
    let mut form_type = [0u8; 4];
    form_type.copy_from_slice(&body[8..12]);
    let mut pos = 12usize;
    let (mut comm, mut audio, mut has_fver) = (None, None, false);
    while pos < body.len() {
        assert!(pos + 8 <= body.len(), "a partial chunk header at {}", pos);
        let id = &body[pos..pos + 4];
        let size = u32::from_be_bytes(body[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let end = pos + 8 + size;
        assert!(
            end + size % 2 <= body.len(),
            "chunk {:?} runs past the body",
            String::from_utf8_lossy(id)
        );
        let chunk = &body[pos + 8..end];
        match id {
            b"COMM" => comm = Some(chunk.to_vec()),
            b"FVER" => has_fver = true,
            b"SSND" => {
                let offset = u32::from_be_bytes(chunk[0..4].try_into().unwrap());
                let block = u32::from_be_bytes(chunk[4..8].try_into().unwrap());
                assert_eq!((offset, block), (0, 0), "SSND offset and block size");
                audio = Some(chunk[8..].to_vec());
            }
            _ => {}
        }
        pos = end + size % 2;
    }
    assert_eq!(pos, body.len(), "the last chunk ends with the body");
    ParsedAiff {
        form_type,
        comm: comm.expect("a COMM chunk"),
        has_fver,
        audio: audio.expect("an SSND chunk"),
    }
}

/// `?pos=<pos>` of a file with `comm` and sound data `sound` at `rate` and `frame` bytes a frame:
/// a valid AIFF with the file's own COMM fields, the frame count of what is sent, and the audio
/// from `pos` rounded down to a whole frame. Returns the body.
fn assert_seeked(port: u16, name: &str, pos: &str, comm: &[u8], sound: &[u8], rate: u32, frame: usize) -> Vec<u8> {
    let (status, headers, body) = get(port, &format!("/stream/{}?pos={}", name, pos), None);
    assert_eq!(status, 200, "{}?pos={}: {}", name, pos, headers);
    assert_eq!(
        header(&headers, "Content-Length"),
        Some(body.len().to_string()),
        "{}?pos={}: Content-Length is the body sent",
        name,
        pos
    );
    let aiff = parse_aiff(&body);
    let wanted = (pos.parse::<f64>().unwrap() * f64::from(rate)) as usize * frame;
    let whole = sound.len() / frame * frame;
    let offset = wanted.min(whole);
    assert_eq!(aiff.audio.len() % frame, 0, "{}?pos={}: whole frames only", name, pos);
    assert_eq!(aiff.audio.len(), whole - offset, "{}?pos={}: audio length", name, pos);
    assert!(
        aiff.audio == sound[offset..whole],
        "{}?pos={}: audio from byte {}",
        name,
        pos,
        offset
    );

    let mut expected = comm.to_vec();
    expected[2..6].copy_from_slice(&(((whole - offset) / frame) as u32).to_be_bytes());
    assert_eq!(
        aiff.comm, expected,
        "{}?pos={}: the file's COMM, frames of what is sent",
        name, pos
    );
    let aifc = name.ends_with(".aifc");
    assert_eq!(&aiff.form_type, if aifc { b"AIFC" } else { b"AIFF" });
    assert_eq!(aiff.has_fver, aifc, "{}?pos={}: an AIFC has its FVER chunk", name, pos);
    body
}

#[test]
fn test_aiff_seek_sends_valid_header_and_aligned_audio() {
    common::isolate();
    // 16-bit stereo at 8 kHz, 2 s, with a trailing chunk the seek must not send.
    let sound = audio(8000 * 4 * 2);
    let c = comm(2, (sound.len() / 4) as u32, 16, 8000);
    let file = form(
        b"AIFF",
        &[chunk(b"COMM", 18, &c), ssnd(0, &sound), chunk(b"ANNO", 7, b"trailer")],
    );
    for name in ["pcm16.aiff", "pcm16.aif"] {
        let (handle, dir) = serve(name, name, &file);
        let port = handle.port;

        let full = assert_seeked(port, name, "1.0001", &c, &sound, 8000, 4);
        assert_seeked(port, name, "0.33", &c, &sound, 8000, 4);
        assert_seeked(port, name, "0.0001", &c, &sound, 8000, 4);
        // Past the end: the header and no audio.
        assert_seeked(port, name, "60", &c, &sound, 8000, 4);

        // Ranges of the seeked stream index the same representation.
        let path = format!("/stream/{}?pos=1.0001", name);
        let (status, headers, body) = get(port, &path, Some("bytes=20-119"));
        assert_eq!(status, 206, "{}: {}", name, headers);
        assert_eq!(
            header(&headers, "Content-Range"),
            Some(format!("bytes 20-119/{}", full.len()))
        );
        assert_eq!(body, &full[20..120]);
        let (status, _, body) = get(port, &path, Some("bytes=-10"));
        assert_eq!(status, 206);
        assert_eq!(
            body,
            &full[full.len() - 10..],
            "{}: the last bytes are audio, not ANNO",
            name
        );

        handle.stop();
        let _ = fs::remove_dir_all(&dir);
    }
}

#[test]
fn test_aiff_seek_odd_frames_pad_the_ssnd_chunk() {
    common::isolate();
    // 24-bit mono at 44.1 kHz: 3-byte frames, so an odd frame count is an odd SSND chunk, which
    // gets its pad byte (counted in the FORM size, not in the SSND size). The SSND offset field
    // skips 4 filler bytes, and the file ends in a partial frame, which is never sent.
    let mut sound = audio(44_100 * 3);
    sound.push(0x42);
    let c = comm(1, 44_100, 24, 44_100);
    let file = form(b"AIFF", &[chunk(b"COMM", 18, &c), ssnd(4, &sound)]);
    let (handle, dir) = serve("odd", "mono24.aiff", &file);

    // 0.5 s is frame 22050: 22050 frames (even) are left. 0.50003 s is frame 22051: 22049 (odd).
    let even = assert_seeked(handle.port, "mono24.aiff", "0.5", &c, &sound, 44_100, 3);
    let odd = assert_seeked(handle.port, "mono24.aiff", "0.50003", &c, &sound, 44_100, 3);
    assert_eq!(even.len() % 2, 0);
    assert_eq!(odd.len() % 2, 0, "the pad byte keeps the chunk even");
    assert_eq!(odd.last(), Some(&0), "the pad byte is sent last");
    // A range that ends in the pad byte.
    let (status, headers, body) = get(handle.port, "/stream/mono24.aiff?pos=0.50003", Some("bytes=-3"));
    assert_eq!(status, 206, "{}", headers);
    assert_eq!(body, &odd[odd.len() - 3..]);

    handle.stop();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_aifc_seek_keeps_compression_fields() {
    common::isolate();
    // Little-endian 16-bit stereo (`sowt`) whose writer left out the compression name's pad byte,
    // so the COMM chunk is odd and gets the chunk pad instead; the SSND offset field skips 8 filler
    // bytes.
    let sound = audio(8000 * 4 * 2);
    let mut c = comm_aifc(2, (sound.len() / 4) as u32, 16, 8000, b"sowt", "Not compressed");
    c.pop();
    assert_eq!(c.len() % 2, 1, "an odd COMM chunk");
    let file = form(b"AIFC", &[fver(), chunk(b"COMM", c.len() as u32, &c), ssnd(8, &sound)]);
    let (handle, dir) = serve("aifc", "sowt.aifc", &file);
    assert_seeked(handle.port, "sowt.aifc", "1.0001", &c, &sound, 8000, 4);
    assert_seeked(handle.port, "sowt.aifc", "0.25", &c, &sound, 8000, 4);
    handle.stop();
    let _ = fs::remove_dir_all(&dir);

    // 32-bit float mono, `fl32`.
    let sound = audio(8000 * 4);
    let c = comm_aifc(1, 8000, 32, 8000, b"fl32", "32-bit floating point");
    let file = form(b"AIFC", &[fver(), chunk(b"COMM", c.len() as u32, &c), ssnd(0, &sound)]);
    let (handle, dir) = serve("fl32", "float.aifc", &file);
    assert_seeked(handle.port, "float.aifc", "0.6", &c, &sound, 8000, 4);
    handle.stop();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_aiff_seek_unseekable_serves_whole_file() {
    common::isolate();
    let sound = audio(8000 * 4);
    let good = comm(2, 8000, 16, 8000);
    let mut zero_rate = good.clone();
    zero_rate[8..18].copy_from_slice(&[0; 10]);
    let mut zero_channels = good.clone();
    zero_channels[0..2].copy_from_slice(&0u16.to_be_bytes());
    let ima4 = comm_aifc(2, 8000, 16, 8000, b"ima4", "IMA 4:1");
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("no_comm.aiff", form(b"AIFF", &[ssnd(0, &sound)])),
        ("no_ssnd.aiff", form(b"AIFF", &[chunk(b"COMM", 18, &good)])),
        (
            "short_comm.aiff",
            form(b"AIFF", &[chunk(b"COMM", 14, &good[..14]), ssnd(0, &sound)]),
        ),
        (
            "zero_rate.aiff",
            form(b"AIFF", &[chunk(b"COMM", 18, &zero_rate), ssnd(0, &sound)]),
        ),
        (
            "zero_channels.aiff",
            form(b"AIFF", &[chunk(b"COMM", 18, &zero_channels), ssnd(0, &sound)]),
        ),
        // A compressed AIFC has no fixed frame size to seek by.
        (
            "ima4.aifc",
            form(
                b"AIFC",
                &[fver(), chunk(b"COMM", ima4.len() as u32, &ima4), ssnd(0, &sound)],
            ),
        ),
        // An SSND offset past the chunk's end.
        ("bad_offset.aiff", {
            let mut f = form(b"AIFF", &[chunk(b"COMM", 18, &good)]);
            f.extend_from_slice(b"SSND\x00\x00\x00\x10");
            f.extend_from_slice(&0x100u32.to_be_bytes());
            f.extend_from_slice(&[0; 12]);
            f
        }),
        ("not_aiff.aiff", sound.clone()),
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
