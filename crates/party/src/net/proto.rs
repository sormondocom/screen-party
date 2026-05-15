//! Wire protocol: framing and control message bodies.
//!
//! Every message on the wire is:
//!   [u32 payload_len LE][u8 msg_type][payload bytes]
//!
//! Video frame payload:
//!   [u32 rect_count] { [u32 x][u32 y][u32 w][u32 h][u32 px_len][RGBA bytes] } ...

use std::io::{self, Read, Write};

use capture::{Frame, Rect};

pub const VERSION: u8 = 3;

/// Total bytes sent per speed probe pass.
pub const PROBE_BYTES: usize = 2 * 1024 * 1024; // 2 MB
/// Payload bytes per probe chunk (excluding the 8-byte header).
pub const PROBE_CHUNK_DATA: usize = 32 * 1024; // 32 KB

pub mod msg {
    pub const HANDSHAKE:        u8 = 0x01;
    pub const HANDSHAKE_ACK:    u8 = 0x02;
    pub const HANDSHAKE_REJECT: u8 = 0x03;
    pub const SPEED_PROBE:      u8 = 0x10;
    pub const SPEED_REPORT:     u8 = 0x11;
    pub const STREAM_INFO:      u8 = 0x20;
    pub const VIDEO_FRAME:      u8 = 0x21;
    pub const AUDIO_CHUNK:      u8 = 0x22;
    pub const CHAT_MSG:         u8 = 0x30;
    pub const DISCONNECT:       u8 = 0xFF;
}

// ── Framing ───────────────────────────────────────────────────────────────────

/// Write `[u32 len LE][u8 type][payload]` and flush.
pub fn write_msg(w: &mut impl Write, ty: u8, payload: &[u8]) -> io::Result<()> {
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(&[ty])?;
    w.write_all(payload)?;
    w.flush()
}

/// Read one framed message → `(msg_type, payload)`.
pub fn read_msg(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 5];
    r.read_exact(&mut hdr)?;
    let len = u32::from_le_bytes(hdr[..4].try_into().unwrap()) as usize;
    if len > 16 * 1024 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    let ty = hdr[4];
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((ty, payload))
}

// ── Handshake (host → client) ─────────────────────────────────────────────────

/// Encode the HANDSHAKE message.
/// Layout: [version u8][interactive u8][fp_len u16][fp bytes][pubkey 32 bytes]
pub fn encode_handshake(interactive_required: bool, fingerprint: &str, pubkey: &[u8; 32]) -> Vec<u8> {
    let fp = fingerprint.as_bytes();
    let mut b = Vec::with_capacity(4 + fp.len() + 32);
    b.push(VERSION);
    b.push(interactive_required as u8);
    b.extend_from_slice(&(fp.len() as u16).to_le_bytes());
    b.extend_from_slice(fp);
    b.extend_from_slice(pubkey);
    b
}

pub struct Handshake {
    pub version:     u8,
    pub fingerprint: String,
    pub pubkey:      [u8; 32],
}

pub fn decode_handshake(p: &[u8]) -> io::Result<Handshake> {
    if p.len() < 4 { return Err(bad("handshake too short")); }
    let fp_len = u16::from_le_bytes([p[2], p[3]]) as usize;
    if p.len() < 4 + fp_len + 32 { return Err(bad("handshake truncated")); }
    let mut pubkey = [0u8; 32];
    pubkey.copy_from_slice(&p[4 + fp_len..4 + fp_len + 32]);
    Ok(Handshake {
        version:     p[0],
        fingerprint: String::from_utf8(p[4..4 + fp_len].to_vec())
                         .map_err(|_| bad("bad fingerprint utf8"))?,
        pubkey,
    })
}

// ── Handshake ACK (client → host) ────────────────────────────────────────────

/// Encode the HANDSHAKE_ACK message.
/// Layout: [interactive u8][fp_len u16][fp bytes][pubkey 32 bytes][name_len u16][name bytes]
pub fn encode_ack(interactive_confirmed: bool, fingerprint: &str, pubkey: &[u8; 32], name: &str) -> Vec<u8> {
    let fp = fingerprint.as_bytes();
    let nm = name.as_bytes();
    let mut b = Vec::with_capacity(3 + fp.len() + 32 + 2 + nm.len());
    b.push(interactive_confirmed as u8);
    b.extend_from_slice(&(fp.len() as u16).to_le_bytes());
    b.extend_from_slice(fp);
    b.extend_from_slice(pubkey);
    b.extend_from_slice(&(nm.len() as u16).to_le_bytes());
    b.extend_from_slice(nm);
    b
}

pub struct Ack {
    pub interactive_confirmed: bool,
    pub fingerprint:           String,
    pub pubkey:                [u8; 32],
    pub name:                  String,
}

pub fn decode_ack(p: &[u8]) -> io::Result<Ack> {
    if p.len() < 3 { return Err(bad("ack too short")); }
    let fp_len = u16::from_le_bytes([p[1], p[2]]) as usize;
    if p.len() < 3 + fp_len + 32 + 2 { return Err(bad("ack truncated")); }
    let mut pubkey = [0u8; 32];
    pubkey.copy_from_slice(&p[3 + fp_len..3 + fp_len + 32]);
    let name_off = 3 + fp_len + 32;
    let name_len = u16::from_le_bytes([p[name_off], p[name_off + 1]]) as usize;
    if p.len() < name_off + 2 + name_len { return Err(bad("ack name truncated")); }
    Ok(Ack {
        interactive_confirmed: p[0] != 0,
        fingerprint:           String::from_utf8(p[3..3 + fp_len].to_vec())
                                   .map_err(|_| bad("bad ack fingerprint utf8"))?,
        pubkey,
        name:                  String::from_utf8(p[name_off + 2..name_off + 2 + name_len].to_vec())
                                   .map_err(|_| bad("bad ack name utf8"))?,
    })
}

/// Build the chat display name shown to all participants.
/// Format: `"Name [fp6]"` when both are present, `"Name"` or `"fp6"` otherwise.
pub fn make_display_name(name: &str, fingerprint: &str) -> String {
    let fp6 = fingerprint.get(..6).unwrap_or(fingerprint);
    match (name.is_empty(), fp6.is_empty()) {
        (false, false) => format!("{name} [{fp6}]"),
        (false, true)  => name.to_string(),
        (true,  false) => fp6.to_string(),
        (true,  true)  => "unknown".to_string(),
    }
}

// ── Speed report (client → host) ─────────────────────────────────────────────

/// `[u64 total_bytes][u64 elapsed_us][u64 jitter_us as f64 bits]`
pub fn encode_speed_report(total_bytes: u64, elapsed_us: u64, jitter_us: f64) -> Vec<u8> {
    let mut b = Vec::with_capacity(24);
    b.extend_from_slice(&total_bytes.to_le_bytes());
    b.extend_from_slice(&elapsed_us.to_le_bytes());
    b.extend_from_slice(&jitter_us.to_bits().to_le_bytes());
    b
}

pub struct SpeedReport {
    pub total_bytes: u64,
    pub elapsed_us: u64,
    pub jitter_us: f64,
}

pub fn decode_speed_report(p: &[u8]) -> io::Result<SpeedReport> {
    if p.len() < 24 { return Err(bad("speed report too short")); }
    Ok(SpeedReport {
        total_bytes: u64::from_le_bytes(p[0..8].try_into().unwrap()),
        elapsed_us:  u64::from_le_bytes(p[8..16].try_into().unwrap()),
        jitter_us:   f64::from_bits(u64::from_le_bytes(p[16..24].try_into().unwrap())),
    })
}

// ── Stream info (host → client) ───────────────────────────────────────────────

/// `[u32 width][u32 height][u8 fps][u32 sample_rate][u8 channels][f32 cache_secs]`
pub fn encode_stream_info(
    width: u32, height: u32, fps: u8,
    sample_rate: u32, channels: u8,
    cache_secs: f32,
) -> Vec<u8> {
    let mut b = Vec::with_capacity(18);
    b.extend_from_slice(&width.to_le_bytes());
    b.extend_from_slice(&height.to_le_bytes());
    b.push(fps);
    b.extend_from_slice(&sample_rate.to_le_bytes());
    b.push(channels);
    b.extend_from_slice(&cache_secs.to_le_bytes());
    b
}

pub struct StreamInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u8,
    pub sample_rate: u32,
    pub channels: u8,
    /// Host's ring-buffer depth in seconds; informs client video-queue sizing.
    pub cache_secs: f32,
}

pub fn decode_stream_info(p: &[u8]) -> io::Result<StreamInfo> {
    if p.len() < 18 { return Err(bad("stream info too short")); }
    Ok(StreamInfo {
        width:       u32::from_le_bytes(p[0..4].try_into().unwrap()),
        height:      u32::from_le_bytes(p[4..8].try_into().unwrap()),
        fps:         p[8],
        sample_rate: u32::from_le_bytes(p[9..13].try_into().unwrap()),
        channels:    p[13],
        cache_secs:  f32::from_le_bytes(p[14..18].try_into().unwrap()),
    })
}

// ── Video frame ───────────────────────────────────────────────────────────────

/// Encode dirty rects + their pixels from `frame` into a compressed VIDEO_FRAME payload.
///
/// Wire layout: `[u64 pts_us][zstd( [u32 w][u32 h][u32 rect_count]{ rect_header + pixels }… )]`
/// The pts_us prefix is uncompressed so it can be read without decompressing.
pub fn encode_video_frame(pts_us: u64, rects: &[Rect], frame: &Frame) -> io::Result<Vec<u8>> {
    let mut raw = Vec::new();
    raw.extend_from_slice(&frame.width.to_le_bytes());
    raw.extend_from_slice(&frame.height.to_le_bytes());
    raw.extend_from_slice(&(rects.len() as u32).to_le_bytes());
    for r in rects {
        raw.extend_from_slice(&r.x.to_le_bytes());
        raw.extend_from_slice(&r.y.to_le_bytes());
        raw.extend_from_slice(&r.width.to_le_bytes());
        raw.extend_from_slice(&r.height.to_le_bytes());
        let px_bytes = (r.width * r.height * 4) as usize;
        raw.extend_from_slice(&(px_bytes as u32).to_le_bytes());
        for row in 0..r.height {
            let off = ((r.y + row) * frame.width + r.x) as usize * 4;
            raw.extend_from_slice(&frame.data[off..off + r.width as usize * 4]);
        }
    }
    let compressed = zstd::encode_all(std::io::Cursor::new(raw), 1)?;
    let mut out = Vec::with_capacity(8 + compressed.len());
    out.extend_from_slice(&pts_us.to_le_bytes());
    out.extend_from_slice(&compressed);
    Ok(out)
}

// ── Chat message (bidirectional) ──────────────────────────────────────────────

/// `[u16 sender_len][sender bytes][u16 text_len][text bytes]`
pub fn encode_chat_msg(sender: &str, text: &str) -> Vec<u8> {
    let s = sender.as_bytes();
    let t = text.as_bytes();
    let mut b = Vec::with_capacity(4 + s.len() + t.len());
    b.extend_from_slice(&(s.len() as u16).to_le_bytes());
    b.extend_from_slice(s);
    b.extend_from_slice(&(t.len() as u16).to_le_bytes());
    b.extend_from_slice(t);
    b
}

pub struct ChatMsg {
    pub sender: String,
    pub text:   String,
}

pub fn decode_chat_msg(p: &[u8]) -> io::Result<ChatMsg> {
    if p.len() < 2 { return Err(bad("chat msg too short")); }
    let s_len = u16::from_le_bytes([p[0], p[1]]) as usize;
    if p.len() < 2 + s_len + 2 { return Err(bad("chat sender truncated")); }
    let sender = String::from_utf8(p[2..2 + s_len].to_vec()).map_err(|_| bad("bad sender utf8"))?;
    let t0 = 2 + s_len;
    let t_len = u16::from_le_bytes([p[t0], p[t0 + 1]]) as usize;
    if p.len() < t0 + 2 + t_len { return Err(bad("chat text truncated")); }
    let text = String::from_utf8(p[t0 + 2..t0 + 2 + t_len].to_vec()).map_err(|_| bad("bad text utf8"))?;
    Ok(ChatMsg { sender, text })
}

// ── Video frame decode (client side) ─────────────────────────────────────────

pub struct DecodedRect {
    pub x: u32, pub y: u32,
    pub w: u32,
    pub pixels: Vec<u8>, // RGBA, row-major
}

pub struct DecodedFrame {
    /// Host-side capture timestamp in microseconds (same clock as audio chunks).
    pub pts_us: u64,
    pub width:  u32,
    pub height: u32,
    pub rects:  Vec<DecodedRect>,
}

pub fn decode_video_frame(payload: &[u8]) -> io::Result<DecodedFrame> {
    if payload.len() < 8 { return Err(bad("frame payload too short")); }
    let pts_us  = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let payload = zstd::decode_all(&payload[8..])?;
    let payload = payload.as_slice();
    if payload.len() < 12 { return Err(bad("frame payload too short")); }
    let width      = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let height     = u32::from_le_bytes(payload[4..8].try_into().unwrap());
    let rect_count = u32::from_le_bytes(payload[8..12].try_into().unwrap()) as usize;
    let mut rects = Vec::with_capacity(rect_count);
    let mut off = 12;
    for _ in 0..rect_count {
        if payload.len() < off + 20 { return Err(bad("rect header truncated")); }
        let x      = u32::from_le_bytes(payload[off..off + 4].try_into().unwrap());
        let y      = u32::from_le_bytes(payload[off + 4..off + 8].try_into().unwrap());
        let w      = u32::from_le_bytes(payload[off + 8..off + 12].try_into().unwrap());
        let _h     = u32::from_le_bytes(payload[off + 12..off + 16].try_into().unwrap());
        let px_len = u32::from_le_bytes(payload[off + 16..off + 20].try_into().unwrap()) as usize;
        off += 20;
        if payload.len() < off + px_len { return Err(bad("pixel data truncated")); }
        let pixels = payload[off..off + px_len].to_vec();
        off += px_len;
        rects.push(DecodedRect { x, y, w, pixels });
    }
    Ok(DecodedFrame { pts_us, width, height, rects })
}

// ── Audio chunk (bidirectional) ───────────────────────────────────────────────

/// Encode interleaved f32 samples with a capture timestamp.
/// Wire layout: `[u64 pts_us][f32 f32 …]` (all little-endian).
pub fn encode_audio_chunk(pts_us: u64, samples: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + samples.len() * 4);
    b.extend_from_slice(&pts_us.to_le_bytes());
    for &s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

/// Decode an audio payload; returns `(pts_us, samples)`.
pub fn decode_audio_chunk(p: &[u8]) -> (u64, Vec<f32>) {
    if p.len() < 8 { return (0, vec![]); }
    let pts_us = u64::from_le_bytes(p[0..8].try_into().unwrap());
    let samples = p[8..].chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    (pts_us, samples)
}

fn bad(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
