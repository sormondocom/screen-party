//! Host-side broadcaster: TCP listener + one-to-many fan-out.
//!
//! Each connecting client goes through:
//!   1. Handshake  — version check, X25519 key exchange, interactive enforcement
//!   2. Speed test — host sends probe, client reports stats  (encrypted)
//!   3. Approval   — if `--approve` is set, host must type /approve <fp8> in chat
//!   4. STREAM_INFO — codec/format metadata                  (encrypted)
//!   5. Split threads:
//!      • Send loop  — BroadcastMsgs → socket  (main client thread, encrypted)
//!      • Read loop  — incoming CHAT_MSG → re-broadcast to all  (spawned, encrypted)

use std::io::{self, BufReader, BufWriter, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc::{self, SyncSender},
    Arc, Mutex,
};
use std::time::Duration;

use capture::{Frame, Rect};

use super::cipher::{EncryptedReader, EncryptedWriter, EphemeralKeypair, Role};
use super::{proto, speed_test};

// ── Public message types ──────────────────────────────────────────────────────

pub enum BroadcastMsg {
    VideoFrame { rects: Vec<Rect>, frame: Arc<Frame> },
    AudioChunk(Arc<Vec<u8>>),
    ChatMessage { sender: String, text: String },
    /// Sent to every client (or one client for a kick) to close the session.
    Disconnect,
}

pub enum ApprovalDecision {
    Approved,
    Denied(String),
}

// ── Internal types ────────────────────────────────────────────────────────────

struct ClientHandle {
    id:          u64,
    name:        String,
    fingerprint: String,
    tx:          SyncSender<Arc<BroadcastMsg>>,
}

struct PendingEntry {
    id:          u64,
    name:        String,
    fingerprint: String,
    decision_tx: mpsc::SyncSender<ApprovalDecision>,
}

// Messages buffered per client before dropping. Sized for ~200 ms of video
// at 30 fps with headroom for interleaved audio chunks. A deeper queue hides
// a slow send thread at the cost of silent latency buildup.
const QUEUE_DEPTH: usize = 6;

// ── Broadcaster ───────────────────────────────────────────────────────────────

pub struct Broadcaster {
    host_fingerprint: String,
    sample_rate:      u32,
    channels:         u16,
    /// Actual capture region dimensions and fps; (0,0,0) while no region is selected.
    stream_dims:  Mutex<(u32, u32, u8)>,
    clients:      Arc<Mutex<Vec<ClientHandle>>>,
    pending:      Arc<Mutex<Vec<PendingEntry>>>,
    next_id:      AtomicU64,
    /// Forward incoming chat + system events to the host UI.
    chat_notify:  Arc<Mutex<Option<mpsc::Sender<(String, String)>>>>,
}

impl Broadcaster {
    pub fn new(
        host_fingerprint: String,
        sample_rate:      u32,
        channels:         u16,
    ) -> Arc<Self> {
        Arc::new(Self {
            host_fingerprint,
            sample_rate,
            channels,
            stream_dims: Mutex::new((0, 0, 0)),
            clients:     Arc::new(Mutex::new(Vec::new())),
            pending:     Arc::new(Mutex::new(Vec::new())),
            next_id:     AtomicU64::new(1),
            chat_notify: Arc::new(Mutex::new(None)),
        })
    }

    pub fn listen(self: &Arc<Self>, port: u16) {
        let b = self.clone();
        std::thread::Builder::new()
            .name("net-listener".into())
            .spawn(move || Broadcaster::accept_loop(b, port))
            .expect("listener thread");
    }

    pub fn broadcast(&self, msg: Arc<BroadcastMsg>) {
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|c| match c.tx.try_send(msg.clone()) {
            Ok(_) => true,
            Err(mpsc::TrySendError::Full(_)) => {
                eprintln!("[net] client {} slow, frame dropped", c.id);
                true
            }
            Err(mpsc::TrySendError::Disconnected(_)) => false,
        });
    }

    pub fn set_stream_dims(&self, width: u32, height: u32, fps: u8) {
        *self.stream_dims.lock().unwrap() = (width, height, fps);
    }

    pub fn set_chat_sender(&self, tx: mpsc::Sender<(String, String)>) {
        *self.chat_notify.lock().unwrap() = Some(tx);
    }

    /// Approve the pending client whose fingerprint starts with `fp_prefix`.
    /// Returns `true` if a match was found.
    pub fn approve_pending(&self, fp_prefix: &str) -> bool {
        let mut pending = self.pending.lock().unwrap();
        if let Some(pos) = pending.iter().position(|p| p.fingerprint.starts_with(fp_prefix)) {
            let entry = pending.remove(pos);
            let _ = entry.decision_tx.send(ApprovalDecision::Approved);
            true
        } else {
            false
        }
    }

    /// Deny the pending client whose fingerprint starts with `fp_prefix`.
    /// Returns `true` if a match was found.
    pub fn deny_pending(&self, fp_prefix: &str, reason: &str) -> bool {
        let mut pending = self.pending.lock().unwrap();
        if let Some(pos) = pending.iter().position(|p| p.fingerprint.starts_with(fp_prefix)) {
            let entry = pending.remove(pos);
            let _ = entry.decision_tx.send(ApprovalDecision::Denied(reason.to_string()));
            true
        } else {
            false
        }
    }

    /// Disconnect a currently-streaming viewer by fingerprint prefix.
    /// Returns `true` if a match was found.
    pub fn kick_client(&self, fp_prefix: &str) -> bool {
        let mut clients = self.clients.lock().unwrap();
        if let Some(pos) = clients.iter().position(|c| c.fingerprint.starts_with(fp_prefix)) {
            let _ = clients[pos].tx.try_send(Arc::new(BroadcastMsg::Disconnect));
            clients.remove(pos);
            true
        } else {
            false
        }
    }

    /// Display names of all currently-streaming viewers.
    pub fn list_viewers(&self) -> Vec<String> {
        self.clients.lock().unwrap().iter()
            .map(|c| fmt_name(&c.name, &c.fingerprint))
            .collect()
    }

    /// Display names of all clients currently waiting for approval.
    pub fn list_pending(&self) -> Vec<String> {
        self.pending.lock().unwrap().iter()
            .map(|p| fmt_name(&p.name, &p.fingerprint))
            .collect()
    }

    fn accept_loop(broadcaster: Arc<Self>, port: u16) {
        let listener = match TcpListener::bind(("0.0.0.0", port)) {
            Ok(l) => { println!("[net] listening on 0.0.0.0:{port}"); l }
            Err(e) => { eprintln!("[net] bind failed: {e}"); return; }
        };
        for stream in listener.incoming() {
            match stream {
                Ok(s) => Broadcaster::spawn_client(broadcaster.clone(), s),
                Err(e) => eprintln!("[net] accept: {e}"),
            }
        }
    }

    fn spawn_client(broadcaster: Arc<Self>, stream: TcpStream) {
        let addr = match stream.peer_addr() {
            Ok(a) => a,
            Err(_) => return,
        };
        let _ = stream.set_nodelay(true);
        let id = broadcaster.next_id.fetch_add(1, Ordering::Relaxed);

        std::thread::Builder::new()
            .name(format!("client-{id}"))
            .spawn(move || {
                println!("[net] client {id} connecting from {addr}");
                if let Err(e) = run_client(id, addr, stream, &broadcaster) {
                    eprintln!("[net] client {id} ({addr}): {e}");
                }
                // Clean up any stale pending entry if the client disconnected mid-approval.
                broadcaster.pending.lock().unwrap().retain(|p| p.id != id);
                broadcaster.clients.lock().unwrap().retain(|c| c.id != id);
                println!("[net] client {id} ({addr}) disconnected");
            })
            .ok();
    }
}

// ── Per-client handler ────────────────────────────────────────────────────────

fn run_client(
    id:          u64,
    addr:        std::net::SocketAddr,
    stream:      TcpStream,
    broadcaster: &Arc<Broadcaster>,
) -> io::Result<()> {
    sys_notify(&broadcaster.chat_notify, format!("[CONN] {addr} is connecting…"));

    // `read_stream` is the read half; `stream` is kept for writes and set_read_timeout.
    let read_stream = stream.try_clone()?;

    stream.set_read_timeout(Some(Duration::from_secs(15)))?;

    // ── Plaintext handshake ───────────────────────────────────────────────────
    let kp = EphemeralKeypair::generate();

    proto::write_msg(
        &mut &stream,
        proto::msg::HANDSHAKE,
        &proto::encode_handshake(
            true, // interactive confirmation always required
            &broadcaster.host_fingerprint,
            &kp.public_bytes,
        ),
    )?;

    let (ty, payload) = proto::read_msg(&mut &read_stream)?;
    if ty == proto::msg::DISCONNECT { return Ok(()); }
    if ty != proto::msg::HANDSHAKE_ACK {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "expected HANDSHAKE_ACK"));
    }

    let ack  = proto::decode_ack(&payload)?;
    let keys = kp.complete(&ack.pubkey, Role::Host);

    // Switch to encrypted I/O.
    let mut r = BufReader::new(EncryptedReader::new(read_stream, keys.recv));
    let mut w = BufWriter::new(EncryptedWriter::new(&stream, keys.send));

    if !ack.interactive_confirmed {
        proto::write_msg(&mut w, proto::msg::HANDSHAKE_REJECT, b"fingerprint confirmation required")?;
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "client did not confirm host fingerprint",
        ));
    }

    let client_fp    = ack.fingerprint;
    let client_name  = ack.name;
    let display_name = proto::make_display_name(&client_name, &client_fp);
    let fp8          = client_fp.get(..8).unwrap_or(&client_fp).to_string();

    println!(
        "[net] client {id} OK  name={}  interactive={}",
        display_name, ack.interactive_confirmed,
    );

    // ── Speed test (encrypted) ────────────────────────────────────────────────
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    println!("[net] client {id} speed probe…");
    let stats = speed_test::host_send_probe(&mut r, &mut w)?;
    stats.log(&format!("client {id}"));
    if !stats.is_stable() {
        eprintln!("[net] WARNING: client {id} ({addr}) connection is unstable");
    }

    // ── Approval gate ─────────────────────────────────────────────────────────
    sys_notify(&broadcaster.chat_notify, format!("[JOIN] {display_name}  fp: {client_fp}"));
    sys_notify(&broadcaster.chat_notify, format!("  → /approve {fp8}  or  /deny {fp8}"));

    let (dtx, drx) = mpsc::sync_channel::<ApprovalDecision>(1);
    broadcaster.pending.lock().unwrap().push(PendingEntry {
        id,
        name:        client_name.clone(),
        fingerprint: client_fp.clone(),
        decision_tx: dtx,
    });

    match drx.recv() {
        Ok(ApprovalDecision::Approved) => {}
        Ok(ApprovalDecision::Denied(reason)) => {
            sys_notify(
                &broadcaster.chat_notify,
                format!("[DENIED] {display_name}: {reason}"),
            );
            proto::write_msg(&mut w, proto::msg::HANDSHAKE_REJECT, reason.as_bytes())?;
            return Ok(());
        }
        Err(_) => {
            // Broadcaster dropped (host quit) while client was pending.
            proto::write_msg(&mut w, proto::msg::DISCONNECT, b"")?;
            return Ok(());
        }
    }

    // ── Stream info (encrypted) ───────────────────────────────────────────────
    let (sw, sh, sfps) = *broadcaster.stream_dims.lock().unwrap();
    proto::write_msg(
        &mut w,
        proto::msg::STREAM_INFO,
        &proto::encode_stream_info(sw, sh, sfps, broadcaster.sample_rate, broadcaster.channels as u8),
    )?;

    // ── Register + split send / read threads ──────────────────────────────────
    let (tx, rx) = mpsc::sync_channel::<Arc<BroadcastMsg>>(QUEUE_DEPTH);
    broadcaster.clients.lock().unwrap().push(ClientHandle {
        id,
        name:        client_name,
        fingerprint: client_fp,
        tx,
    });
    stream.set_read_timeout(None)?;
    println!("[net] client {id} ({addr}) streaming");
    sys_notify(&broadcaster.chat_notify, format!("[JOINED] {display_name} is watching"));

    // Read thread: handles incoming CHAT_MSG (encrypted).
    let clients_for_read = broadcaster.clients.clone();
    let chat_notify      = broadcaster.chat_notify.clone();
    let display_for_read = display_name.clone();
    let enc_reader       = r.into_inner();
    std::thread::Builder::new()
        .name(format!("client-{id}-read"))
        .spawn(move || {
            client_read_loop(id, display_for_read, enc_reader, clients_for_read, chat_notify)
        })
        .ok();

    // Send loop: fan-out encrypted messages to this client (blocks until client leaves).
    let result = send_loop(&mut w, rx);
    sys_notify(&broadcaster.chat_notify, format!("[LEFT] {display_name} disconnected"));
    result
}

fn send_loop(w: &mut impl Write, rx: mpsc::Receiver<Arc<BroadcastMsg>>) -> io::Result<()> {
    for msg in rx {
        match msg.as_ref() {
            BroadcastMsg::VideoFrame { rects, frame } => {
                proto::write_msg(
                    w,
                    proto::msg::VIDEO_FRAME,
                    &proto::encode_video_frame(rects, frame)?,
                )?;
            }
            BroadcastMsg::AudioChunk(samples) => {
                proto::write_msg(w, proto::msg::AUDIO_CHUNK, samples)?;
            }
            BroadcastMsg::ChatMessage { sender, text } => {
                proto::write_msg(
                    w,
                    proto::msg::CHAT_MSG,
                    &proto::encode_chat_msg(sender, text),
                )?;
            }
            BroadcastMsg::Disconnect => {
                proto::write_msg(w, proto::msg::DISCONNECT, b"")?;
                return Ok(());
            }
        }
    }
    Ok(())
}

fn client_read_loop(
    id:          u64,
    _name:       String,
    enc_reader:  EncryptedReader<TcpStream>,
    clients:     Arc<Mutex<Vec<ClientHandle>>>,
    chat_notify: Arc<Mutex<Option<mpsc::Sender<(String, String)>>>>,
) {
    let mut r = BufReader::new(enc_reader);
    loop {
        let (ty, payload) = match proto::read_msg(&mut r) {
            Ok(m) => m,
            Err(_) => break,
        };
        match ty {
            proto::msg::CHAT_MSG => {
                if let Ok(chat) = proto::decode_chat_msg(&payload) {
                    // Forward to host UI.
                    if let Ok(lock) = chat_notify.lock() {
                        if let Some(tx) = lock.as_ref() {
                            let _ = tx.send((chat.sender.clone(), chat.text.clone()));
                        }
                    }
                    // Relay to all connected clients.
                    let msg = Arc::new(BroadcastMsg::ChatMessage {
                        sender: chat.sender,
                        text:   chat.text,
                    });
                    let mut lock = clients.lock().unwrap();
                    lock.retain(|c| match c.tx.try_send(msg.clone()) {
                        Ok(_) | Err(mpsc::TrySendError::Full(_)) => true,
                        Err(mpsc::TrySendError::Disconnected(_)) => false,
                    });
                }
            }
            proto::msg::DISCONNECT => break,
            _ => {}
        }
    }
    clients.lock().unwrap().retain(|c| c.id != id);
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Send a system event (empty sender) to the host UI chat channel.
fn sys_notify(chat_notify: &Arc<Mutex<Option<mpsc::Sender<(String, String)>>>>, msg: String) {
    if let Ok(lock) = chat_notify.lock() {
        if let Some(tx) = lock.as_ref() {
            let _ = tx.send((String::new(), msg));
        }
    }
}

/// Format a display name with fp8 suffix for the viewer list.
fn fmt_name(name: &str, fingerprint: &str) -> String {
    let fp8 = fingerprint.get(..8).unwrap_or(fingerprint);
    if name.is_empty() { fp8.to_string() } else { format!("{name} [{fp8}]") }
}
