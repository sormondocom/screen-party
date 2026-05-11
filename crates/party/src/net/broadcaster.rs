//! Host-side broadcaster: TCP listener + one-to-many fan-out.
//!
//! Each connecting client goes through:
//!   1. Handshake  — version check, X25519 key exchange, interactive enforcement
//!   2. Speed test — host sends probe, client reports stats  (encrypted)
//!   3. STREAM_INFO — codec/format metadata                  (encrypted)
//!   4. Split threads:
//!      • Send loop  — BroadcastMsgs → socket  (main client thread, encrypted)
//!      • Read loop  — incoming CHAT_MSG → re-broadcast to all  (spawned, encrypted)

use std::io::{self, BufReader, BufWriter};
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

// ── Public message type ───────────────────────────────────────────────────────

pub enum BroadcastMsg {
    VideoFrame { rects: Vec<Rect>, frame: Arc<Frame> },
    AudioChunk(Arc<Vec<u8>>),
    ChatMessage { sender: String, text: String },
    /// Sent to every client when the host is shutting down.
    Disconnect,
}

// ── Per-client bookkeeping ────────────────────────────────────────────────────

struct ClientHandle {
    id:   u64,
    addr: std::net::SocketAddr,
    tx:   SyncSender<Arc<BroadcastMsg>>,
}

// Frames buffered per client before we start dropping.
const QUEUE_DEPTH: usize = 30;

// ── Broadcaster ───────────────────────────────────────────────────────────────

pub struct Broadcaster {
    interactive:      bool,
    host_fingerprint: String,
    sample_rate:      u32,
    channels:         u16,
    /// Actual capture region dimensions and fps; (0,0,0) while no region is selected.
    stream_dims:      Mutex<(u32, u32, u8)>,
    clients:          Arc<Mutex<Vec<ClientHandle>>>,
    next_id:          AtomicU64,
    /// Forward incoming client chat messages to the host UI.
    chat_notify:      Arc<Mutex<Option<mpsc::Sender<(String, String)>>>>,
}

impl Broadcaster {
    pub fn new(
        interactive:      bool,
        host_fingerprint: String,
        sample_rate:      u32,
        channels:         u16,
    ) -> Arc<Self> {
        Arc::new(Self {
            interactive,
            host_fingerprint,
            sample_rate,
            channels,
            stream_dims: Mutex::new((0, 0, 0)),
            clients: Arc::new(Mutex::new(Vec::new())),
            next_id: AtomicU64::new(1),
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

    pub fn client_count(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    /// Update the capture region dimensions reported in STREAM_INFO.
    /// Call with (0, 0, 0) when capture stops.
    pub fn set_stream_dims(&self, width: u32, height: u32, fps: u8) {
        *self.stream_dims.lock().unwrap() = (width, height, fps);
    }

    /// Register the host UI's chat receiver.  Incoming client messages are
    /// forwarded to this sender so the host can display them.
    pub fn set_chat_sender(&self, tx: mpsc::Sender<(String, String)>) {
        *self.chat_notify.lock().unwrap() = Some(tx);
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
    // `read_stream` is the read half; `stream` is kept for writes and set_read_timeout.
    let read_stream = stream.try_clone()?;

    stream.set_read_timeout(Some(Duration::from_secs(15)))?;

    // ── Plaintext handshake ───────────────────────────────────────────────────
    // Use raw &stream / &read_stream (no BufReader) to avoid pre-fetching bytes
    // that belong to the first encrypted record.
    let kp = EphemeralKeypair::generate();

    proto::write_msg(
        &mut &stream,
        proto::msg::HANDSHAKE,
        &proto::encode_handshake(
            broadcaster.interactive,
            &broadcaster.host_fingerprint,
            &kp.public_bytes,
        ),
    )?;

    let (ty, payload) = proto::read_msg(&mut &read_stream)?;
    if ty == proto::msg::DISCONNECT { return Ok(()); }
    if ty != proto::msg::HANDSHAKE_ACK {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "expected HANDSHAKE_ACK"));
    }

    let ack = proto::decode_ack(&payload)?;

    // Derive session cipher from the completed key exchange.
    let keys = kp.complete(&ack.pubkey, Role::Host);

    // Switch to encrypted I/O for all subsequent messages.
    let mut r = BufReader::new(EncryptedReader::new(read_stream, keys.recv));
    let mut w = BufWriter::new(EncryptedWriter::new(&stream, keys.send));

    if broadcaster.interactive && !ack.interactive_confirmed {
        proto::write_msg(&mut w, proto::msg::HANDSHAKE_REJECT, b"interactive confirmation required")?;
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "client skipped interactive confirmation",
        ));
    }

    let display_name = proto::make_display_name(&ack.name, &ack.fingerprint);
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

    // ── Stream info (encrypted) ───────────────────────────────────────────────
    let (sw, sh, sfps) = *broadcaster.stream_dims.lock().unwrap();
    proto::write_msg(
        &mut w,
        proto::msg::STREAM_INFO,
        &proto::encode_stream_info(sw, sh, sfps, broadcaster.sample_rate, broadcaster.channels as u8),
    )?;

    // ── Register + split send / read threads ──────────────────────────────────
    let (tx, rx) = mpsc::sync_channel::<Arc<BroadcastMsg>>(QUEUE_DEPTH);
    broadcaster.clients.lock().unwrap().push(ClientHandle { id, addr, tx });
    stream.set_read_timeout(None)?;
    println!("[net] client {id} ({addr}) streaming");

    // Read thread: handles incoming CHAT_MSG from this client (encrypted).
    let clients_for_read = broadcaster.clients.clone();
    let chat_notify      = broadcaster.chat_notify.clone();
    let enc_reader = r.into_inner(); // BufReader → EncryptedReader<TcpStream>
    std::thread::Builder::new()
        .name(format!("client-{id}-read"))
        .spawn(move || client_read_loop(id, display_name, enc_reader, clients_for_read, chat_notify))
        .ok();

    // Send loop: fan-out encrypted messages to this client's socket.
    for msg in rx {
        match msg.as_ref() {
            BroadcastMsg::VideoFrame { rects, frame } => {
                proto::write_msg(
                    &mut w,
                    proto::msg::VIDEO_FRAME,
                    &proto::encode_video_frame(rects, frame)?,
                )?;
            }
            BroadcastMsg::AudioChunk(samples) => {
                proto::write_msg(&mut w, proto::msg::AUDIO_CHUNK, samples)?;
            }
            BroadcastMsg::ChatMessage { sender, text } => {
                proto::write_msg(
                    &mut w,
                    proto::msg::CHAT_MSG,
                    &proto::encode_chat_msg(sender, text),
                )?;
            }
            BroadcastMsg::Disconnect => {
                proto::write_msg(&mut w, proto::msg::DISCONNECT, b"")?;
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
    // If the read loop exits before the send loop evicts this client,
    // mark it disconnected by removing it now.
    clients.lock().unwrap().retain(|c| c.id != id);
}
