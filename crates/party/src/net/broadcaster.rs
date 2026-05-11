//! Host-side broadcaster: TCP listener + one-to-many fan-out.
//!
//! Each connecting client goes through:
//!   1. Handshake  — version check, interactive enforcement
//!   2. Speed test — host sends probe, client reports stats
//!   3. STREAM_INFO — codec/format metadata
//!   4. Split threads:
//!      • Send loop  — BroadcastMsgs → socket  (main client thread)
//!      • Read loop  — incoming CHAT_MSG → re-broadcast to all  (spawned thread)

use std::io::{self, BufReader, BufWriter};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc::{self, SyncSender},
    Arc, Mutex,
};
use std::time::Duration;

use capture::{Frame, Rect};

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
    // Clone the stream so the read thread and write thread each own a handle.
    let read_stream = stream.try_clone()?;
    let mut r = BufReader::new(read_stream);
    let mut w = BufWriter::new(&stream);

    stream.set_read_timeout(Some(Duration::from_secs(15)))?;

    // ── Handshake ─────────────────────────────────────────────────────────────
    proto::write_msg(
        &mut w,
        proto::msg::HANDSHAKE,
        &proto::encode_handshake(broadcaster.interactive, &broadcaster.host_fingerprint),
    )?;

    let (ty, payload) = proto::read_msg(&mut r)?;
    if ty == proto::msg::DISCONNECT { return Ok(()); }
    if ty != proto::msg::HANDSHAKE_ACK {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "expected HANDSHAKE_ACK"));
    }

    let ack = proto::decode_ack(&payload)?;

    if broadcaster.interactive && !ack.interactive_confirmed {
        proto::write_msg(&mut w, proto::msg::HANDSHAKE_REJECT, b"interactive confirmation required")?;
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "client skipped interactive confirmation",
        ));
    }

    // Use the first 8 chars of the fingerprint as the chat display name.
    let display_name = ack.fingerprint.get(..8).unwrap_or(&ack.fingerprint).to_string();
    println!(
        "[net] client {id} OK  name={}  interactive={}",
        display_name, ack.interactive_confirmed,
    );

    // ── Speed test ────────────────────────────────────────────────────────────
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    println!("[net] client {id} speed probe…");
    let stats = speed_test::host_send_probe(&mut r, &mut w)?;
    stats.log(&format!("client {id}"));
    if !stats.is_stable() {
        eprintln!("[net] WARNING: client {id} ({addr}) connection is unstable");
    }

    // ── Stream info ───────────────────────────────────────────────────────────
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

    // Read thread: handles incoming CHAT_MSG from this client and relays to all.
    let clients_for_read = broadcaster.clients.clone();
    let chat_notify      = broadcaster.chat_notify.clone();
    let read_inner = r.into_inner(); // recover the cloned TcpStream
    std::thread::Builder::new()
        .name(format!("client-{id}-read"))
        .spawn(move || client_read_loop(id, display_name, read_inner, clients_for_read, chat_notify))
        .ok();

    // Send loop: fan-out messages to this client's socket.
    for msg in rx {
        match msg.as_ref() {
            BroadcastMsg::VideoFrame { rects, frame } => {
                proto::write_msg(
                    &mut w,
                    proto::msg::VIDEO_FRAME,
                    &proto::encode_video_frame(rects, frame),
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
    stream:      TcpStream,
    clients:     Arc<Mutex<Vec<ClientHandle>>>,
    chat_notify: Arc<Mutex<Option<mpsc::Sender<(String, String)>>>>,
) {
    let mut r = BufReader::new(&stream);
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
