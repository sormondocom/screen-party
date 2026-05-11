//! Client-side network driver: handshake, key exchange, speed probe, streaming receive loop.

use std::io::{self, BufReader, BufWriter};
use std::net::TcpStream;
use std::sync::mpsc;
use std::time::Duration;

use super::cipher::{EncryptedReader, EncryptedWriter, EphemeralKeypair, Role};
use super::{proto, speed_test};
use crate::identity;

// ── Channel types ─────────────────────────────────────────────────────────────

pub enum ClientEvent {
    StreamInfo {
        width:       u32,
        height:      u32,
        fps:         u8,
        sample_rate: u32,
        channels:    u16,
        buffer_ms:   u64,
    },
    VideoFrame(proto::DecodedFrame),
    AudioChunk(Vec<f32>),
    ChatMessage { sender: String, text: String },
    Disconnected { reason: String },
}

pub enum ClientSend {
    ChatMessage(String),
    Disconnect,
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run the full client network loop in the calling thread.
///
/// Performs handshake + key exchange + speed probe, then:
///  • Spawns a write thread draining `send_rx` → socket (encrypted).
///  • Reads video / chat / disconnect messages via encrypted I/O.
pub fn run_network(
    host:               String,
    port:               u16,
    interactive:        bool,
    client_fingerprint: String,
    name:               Option<String>,
    event_tx:           mpsc::Sender<ClientEvent>,
    send_rx:            mpsc::Receiver<ClientSend>,
) -> io::Result<()> {
    println!("[net] connecting to {host}:{port}…");

    // `ctrl` is kept for set_read_timeout calls; rstrm/wstrm own the socket I/O.
    let ctrl  = TcpStream::connect((host.as_str(), port))?;
    let _ = ctrl.set_nodelay(true);
    let rstrm = ctrl.try_clone()?;
    let wstrm = ctrl.try_clone()?;

    ctrl.set_read_timeout(Some(Duration::from_secs(15)))?;

    // Sanitize the name: printable chars only, max 32 characters.
    let clean_name: String = name
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(32)
        .collect();

    let kp = EphemeralKeypair::generate();

    // ── Plaintext handshake ───────────────────────────────────────────────────
    // Use raw references (no BufReader) so we don't pre-fetch bytes that
    // belong to the first encrypted record.
    let (ty, payload) = proto::read_msg(&mut &rstrm)?;
    if ty != proto::msg::HANDSHAKE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "expected HANDSHAKE"));
    }
    let hs = proto::decode_handshake(&payload)?;

    if hs.version != proto::VERSION {
        proto::write_msg(&mut &wstrm, proto::msg::DISCONNECT, b"version mismatch")?;
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("version mismatch: host={} us={}", hs.version, proto::VERSION),
        ));
    }

    let interactive_confirmed = if hs.interactive_required {
        if !interactive {
            eprintln!("Host requires --interactive. Re-run with --interactive.");
            proto::write_msg(&mut &wstrm, proto::msg::DISCONNECT, b"interactive required")?;
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "host requires interactive confirmation",
            ));
        }
        match identity::interactive_key_confirm(&host, &hs.fingerprint) {
            Ok(true) => true,
            Ok(false) => {
                proto::write_msg(&mut &wstrm, proto::msg::DISCONNECT, b"user declined")?;
                println!("Connection declined.");
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    } else {
        false
    };

    proto::write_msg(
        &mut &wstrm,
        proto::msg::HANDSHAKE_ACK,
        &proto::encode_ack(interactive_confirmed, &client_fingerprint, &kp.public_bytes, &clean_name),
    )?;

    // ── Derive session cipher ─────────────────────────────────────────────────
    let keys = kp.complete(&hs.pubkey, Role::Client);

    // ── Switch to encrypted I/O ───────────────────────────────────────────────
    ctrl.set_read_timeout(Some(Duration::from_secs(60)))?;
    println!("[net] running speed test…");

    let mut r = BufReader::new(EncryptedReader::new(rstrm, keys.recv));
    let mut w = BufWriter::new(EncryptedWriter::new(wstrm, keys.send));

    // ── Speed probe (encrypted) ───────────────────────────────────────────────
    let stats = speed_test::client_receive_probe(&mut r, &mut w)?;
    stats.log("downstream");
    if !stats.is_stable() {
        eprintln!("WARNING: connection is unstable — video may stutter");
    }

    // ── Stream info (encrypted) ───────────────────────────────────────────────
    let (ty, payload) = proto::read_msg(&mut r)?;
    if ty == proto::msg::HANDSHAKE_REJECT {
        let reason = String::from_utf8_lossy(&payload).into_owned();
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("rejected by host: {reason}"),
        ));
    }
    if ty != proto::msg::STREAM_INFO {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "expected STREAM_INFO"));
    }
    let info = proto::decode_stream_info(&payload)?;
    println!(
        "[net] stream: {}×{} @ {}fps, audio {}Hz {}ch  buffer={}ms",
        info.width, info.height, info.fps,
        info.sample_rate, info.channels,
        stats.recommended_buffer_ms(),
    );
    let _ = event_tx.send(ClientEvent::StreamInfo {
        width:       info.width,
        height:      info.height,
        fps:         info.fps,
        sample_rate: info.sample_rate,
        channels:    info.channels as u16,
        buffer_ms:   stats.recommended_buffer_ms(),
    });

    // ── Split write thread ────────────────────────────────────────────────────
    ctrl.set_read_timeout(None)?;
    let display_name = proto::make_display_name(&clean_name, &client_fingerprint);

    std::thread::Builder::new()
        .name("net-write".into())
        .spawn(move || {
            for msg in send_rx {
                let res = match msg {
                    ClientSend::ChatMessage(text) => proto::write_msg(
                        &mut w,
                        proto::msg::CHAT_MSG,
                        &proto::encode_chat_msg(&display_name, &text),
                    ),
                    ClientSend::Disconnect => {
                        let _ = proto::write_msg(&mut w, proto::msg::DISCONNECT, b"");
                        break;
                    }
                };
                if res.is_err() {
                    break;
                }
            }
        })
        .ok();

    // ── Read loop (encrypted) ─────────────────────────────────────────────────
    loop {
        let (ty, payload) = match proto::read_msg(&mut r) {
            Ok(m) => m,
            Err(e) => {
                let _ = event_tx.send(ClientEvent::Disconnected { reason: e.to_string() });
                return Err(e);
            }
        };
        match ty {
            proto::msg::VIDEO_FRAME => {
                if let Ok(frame) = proto::decode_video_frame(&payload) {
                    let _ = event_tx.send(ClientEvent::VideoFrame(frame));
                }
            }
            proto::msg::AUDIO_CHUNK => {
                let samples = proto::decode_audio_chunk(&payload);
                let _ = event_tx.send(ClientEvent::AudioChunk(samples));
            }
            proto::msg::CHAT_MSG => {
                if let Ok(chat) = proto::decode_chat_msg(&payload) {
                    let _ = event_tx.send(ClientEvent::ChatMessage {
                        sender: chat.sender,
                        text:   chat.text,
                    });
                }
            }
            proto::msg::DISCONNECT => {
                let _ = event_tx.send(ClientEvent::Disconnected {
                    reason: "host closed the session".into(),
                });
                return Ok(());
            }
            _ => {}
        }
    }
}
