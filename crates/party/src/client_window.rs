//! Client-side rendering window: video display, chat overlay, disconnect menu.

use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use font8x8::UnicodeFonts;
use softbuffer::{Context, Surface};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{Key, NamedKey},
    platform::run_on_demand::EventLoopExtRunOnDemand,
    window::{Window, WindowId},
};

use audio::CpalPlayer;

use crate::identity;
use crate::net::client::{run_network, ClientEvent, ClientSend};
use crate::net::proto::DecodedFrame;

// ── Layout constants ──────────────────────────────────────────────────────────

const CHAR_W:      u32 = 9;  // 8px glyph + 1px letter spacing
const CHAR_H:      u32 = 11; // 8px glyph + 3px line gap
const CHAT_LINES:  usize = 28;
const CHAT_PAD:    u32 = 8;
const CHAT_HEIGHT: u32 = CHAT_PAD * 2 + (CHAT_LINES as u32 + 2) * CHAR_H + 4;

// Default video queue capacity before StreamInfo arrives. Overridden once the
// host's cache_secs is known; see ClientApp::video_queue_cap.
const VIDEO_QUEUE_DEFAULT: usize = 120;

const COLOR_BG:       u32 = 0x00_1A_1A_2E;
const COLOR_TEXT:     u32 = 0x00_E0_E0_E0;
const COLOR_INPUT:    u32 = 0x00_88_FF_88;
const COLOR_SYSTEM:   u32 = 0x00_FF_AA_00;
const COLOR_MENU:     u32 = 0x00_11_11_44;
const COLOR_MENU_T:   u32 = 0x00_EE_EE_FF;
const COLOR_EDGE:     u32 = 0x00_25_25_3E;

const EDGE_W:         u32 = 3;   // px wide/tall resize handles
const EDGE_HIT:       f64 = 8.0; // px either side that triggers a drag
const CHAT_MIN_W:     u32 = 200;
const CHAT_MIN_H:     u32 = 50;

// ── ClientApp ─────────────────────────────────────────────────────────────────

struct ClientApp {
    event_rx:     mpsc::Receiver<ClientEvent>,
    send_tx:      mpsc::Sender<ClientSend>,
    // Stream state
    stream_w:     u32,
    stream_h:     u32,
    video_buf:    Vec<u32>, // 0x00RRGGBB, stream_w * stream_h pixels
    audio_out:    Option<CpalPlayer>,
    // Video playback buffer
    video_queue:      VecDeque<DecodedFrame>,
    video_queue_cap:  usize,  // sized from host cache_secs on StreamInfo
    playback_fps:     u8,
    next_frame_due:   Instant,
    prebuffering:     bool,
    prebuffer_target: usize, // frames to accumulate before first display (from speed-test)
    // Identity
    client_fp:    String,
    host_fp:      String,
    host_trusted: Option<bool>, // None = not yet received, Some(true) = known, Some(false) = new
    // Chat state
    chat_history: Vec<(String, String)>, // (sender, text)
    chat_input:   String,
    chat_w:       u32,   // panel pixel width;  0 = full window width
    chat_h:       u32,   // panel pixel height; 0 = CHAT_HEIGHT default
    // UI state
    cursor_pos:     Option<(f64, f64)>,
    drag_chat_edge: bool, // dragging right edge (width)
    drag_chat_top:  bool, // dragging top edge (height)
    menu_open:     bool,
    tick:         u64,
    disconnected: bool,
    // Winit / softbuffer handles
    surface:      Option<Surface<Rc<Window>, Rc<Window>>>,
    context:      Option<Context<Rc<Window>>>,
    window:       Option<Rc<Window>>,
}

impl ClientApp {
    fn new(
        event_rx:  mpsc::Receiver<ClientEvent>,
        send_tx:   mpsc::Sender<ClientSend>,
        client_fp: String,
    ) -> Self {
        Self {
            event_rx,
            send_tx,
            stream_w:        0,
            stream_h:        0,
            video_buf:       Vec::new(),
            audio_out:       None,
            video_queue:      VecDeque::new(),
            video_queue_cap:  VIDEO_QUEUE_DEFAULT,
            playback_fps:     30,
            next_frame_due:   Instant::now(),
            prebuffering:     true,
            prebuffer_target: 2,
            client_fp,
            host_fp:         String::new(),
            host_trusted:    None,
            chat_history:    Vec::new(),
            chat_input:      String::new(),
            chat_w:          0,
            chat_h:          0,
            cursor_pos:      None,
            drag_chat_edge:  false,
            drag_chat_top:   false,
            menu_open:       false,
            tick:            0,
            disconnected:    false,
            surface:         None,
            context:         None,
            window:          None,
        }
    }

    // Apply a decoded frame's dirty rects to video_buf.
    fn blit_frame(&mut self, frame: &DecodedFrame) {
        if self.stream_w != frame.width || self.stream_h != frame.height {
            self.stream_w = frame.width;
            self.stream_h = frame.height;
            self.video_buf = vec![0u32; (frame.width * frame.height) as usize];
            if let Some(w) = &self.window {
                let _ = w.request_inner_size(
                    winit::dpi::PhysicalSize::new(frame.width, frame.height),
                );
            }
        }
        for rect in &frame.rects {
            for (i, row_px) in rect.pixels.chunks(rect.w as usize * 4).enumerate() {
                let ry = rect.y as usize + i;
                if ry >= self.stream_h as usize { break; }
                let base = ry * self.stream_w as usize + rect.x as usize;
                for (j, px) in row_px.chunks(4).enumerate() {
                    let idx = base + j;
                    if idx < self.video_buf.len() {
                        self.video_buf[idx] = rgba_to_xrgb(px[0], px[1], px[2]);
                    }
                }
            }
        }
    }

    // Pop one frame from the playback queue if it is due, blit it, and request
    // a redraw.  Called every about_to_wait tick.
    //
    // Behaviour:
    //   • Prebuffering: holds display until VIDEO_PREBUFFER_FRAMES have
    //     accumulated, absorbing initial network jitter before first paint.
    //   • Normal: one frame per 1/fps seconds; clock advances on each blit.
    //   • Underrun (queue empty): last frame stays on screen; clock pauses
    //     so playback resumes from where it froze, not a burst catch-up.
    //   • Overrun (> VIDEO_MAX_QUEUE): drain_events already drops oldest.
    fn advance_video(&mut self) {
        if self.prebuffering {
            if self.video_queue.len() < self.prebuffer_target {
                return;
            }
            self.prebuffering = false;
            self.next_frame_due = Instant::now();
        }

        let now = Instant::now();
        if now < self.next_frame_due {
            return;
        }

        if let Some(frame) = self.video_queue.pop_front() {
            let frame_dur = Duration::from_nanos(
                1_000_000_000 / self.playback_fps.max(1) as u64,
            );
            self.blit_frame(&frame);
            // += keeps the deadline grid fixed regardless of when about_to_wait fires,
            // preventing cumulative drift. The snap-forward guard prevents burst
            // catch-up after an underrun: if we're still more than one frame behind
            // after advancing, jump the clock to now so only one frame is shown early.
            self.next_frame_due += frame_dur;
            if self.next_frame_due + frame_dur < now {
                self.next_frame_due = now;
            }
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
        // Queue empty → clock stays frozen at next_frame_due so the next
        // arriving frame shows immediately without a burst of catch-up frames.
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.event_rx.try_recv() {
            match ev {
                ClientEvent::StreamInfo { width, height, fps, sample_rate, channels, buffer_ms, cache_secs } => {
                    // Only resize the video buffer if dims are non-zero.
                    // Clients that connected before the host selected a region
                    // will get zeros here and rely on VideoFrame lazy init instead.
                    if width > 0 && height > 0 {
                        self.stream_w = width;
                        self.stream_h = height;
                        self.video_buf = vec![0u32; (width * height) as usize];
                        if let Some(w) = &self.window {
                            let _ = w.request_inner_size(
                                winit::dpi::PhysicalSize::new(width, height),
                            );
                        }
                    }
                    // Reset the playback buffer for the new stream.
                    self.playback_fps = fps.max(1);
                    self.video_queue.clear();
                    self.prebuffering = true;
                    // Size the video queue to hold the full pre-seeded cache tail plus
                    // a live-playback cushion. Cap at 10× the FPS to stay reasonable.
                    self.video_queue_cap = ((cache_secs * self.playback_fps as f32).ceil() as usize)
                        .max(VIDEO_QUEUE_DEFAULT);
                    // Size the prebuffer to absorb the measured link jitter:
                    //   frames = ceil(buffer_ms * fps / 1000), clamped to [1, queue_cap/2].
                    // On LAN (buffer_ms ≈ 3 ms, fps=30) → 1 frame (~33 ms).
                    // On internet (buffer_ms ≈ 500 ms) → 15 frames (~500 ms).
                    self.prebuffer_target = ((buffer_ms * self.playback_fps as u64 + 999) / 1000)
                        .clamp(1, (self.video_queue_cap / 2) as u64) as usize;
                    // Open audio output pre-buffered to absorb the measured jitter.
                    let prebuf = (sample_rate as u64 * channels as u64 * buffer_ms / 1000) as usize;
                    match CpalPlayer::new(sample_rate, channels, prebuf) {
                        Ok(player) => self.audio_out = Some(player),
                        Err(e) => eprintln!("[audio] output unavailable: {e}"),
                    }
                }

                ClientEvent::VideoFrame(frame) => {
                    // Drop the oldest frame if the queue is at its hard ceiling,
                    // keeping the viewer within video_queue_cap frames of live.
                    if self.video_queue.len() >= self.video_queue_cap {
                        self.video_queue.pop_front();
                    }
                    self.video_queue.push_back(frame);
                    // Blitting and redraw are handled by advance_video in about_to_wait.
                }

                ClientEvent::AudioChunk(samples) => {
                    if let Some(player) = &self.audio_out {
                        player.push(samples);
                    }
                }

                ClientEvent::ChatMessage { sender, text } => {
                    self.chat_history.push((sender, text));
                    // Keep history bounded; trim oldest past capacity.
                    if self.chat_history.len() > CHAT_LINES * 3 {
                        let excess = self.chat_history.len() - CHAT_LINES;
                        self.chat_history.drain(0..excess);
                    }
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }

                ClientEvent::PeerInfo { host_fingerprint, trusted } => {
                    self.host_fp      = host_fingerprint;
                    self.host_trusted = Some(trusted);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }

                ClientEvent::Disconnected { reason } => {
                    println!("[net] disconnected: {reason}");
                    self.disconnected = true;
                }
            }
        }
    }

    fn render(&mut self) {
        let (Some(win), Some(srf)) = (&self.window, &mut self.surface) else { return };
        let sz = win.inner_size();
        let (Some(nw), Some(nh)) =
            (NonZeroU32::new(sz.width), NonZeroU32::new(sz.height))
        else { return };
        if srf.resize(nw, nh).is_err() { return; }
        let Ok(mut buf) = srf.buffer_mut() else { return };
        let (w, h) = (sz.width, sz.height);

        // ── Video background (nearest-neighbour scale-to-fill) ────────────────
        if self.stream_w == 0 || self.stream_h == 0 {
            buf.fill(0x00_11_11_11);
            draw_str(&mut buf, w, 20, 20, "Connecting...", COLOR_TEXT);
        } else {
            // Precompute source column indices once per row to halve divisions.
            let src_cols: Vec<usize> = (0..w as usize)
                .map(|c| (c * self.stream_w as usize / w as usize)
                    .min(self.stream_w as usize - 1))
                .collect();
            for dst_row in 0..(h as usize) {
                let src_row = (dst_row * self.stream_h as usize / h as usize)
                    .min(self.stream_h as usize - 1);
                let src_base = src_row * self.stream_w as usize;
                let dst_base = dst_row * w as usize;
                for (dst_col, &src_col) in src_cols.iter().enumerate() {
                    buf[dst_base + dst_col] = self.video_buf[src_base + src_col];
                }
            }
        }

        // ── Chat panel ────────────────────────────────────────────────────────
        let chat_h  = if self.chat_h == 0 { CHAT_HEIGHT } else { self.chat_h.min(h) };
        let panel_y = h.saturating_sub(chat_h);
        let chat_w  = if self.chat_w == 0 { w } else { self.chat_w.min(w) };

        for row in panel_y..h {
            let base = (row * w) as usize;
            let end  = (base + chat_w as usize).min(buf.len());
            buf[base..end].fill(COLOR_BG);
        }

        // Top-edge resize handle.
        for col in 0..chat_w {
            for dy in 0..EDGE_W {
                set_px(&mut buf, w, col, panel_y + dy, COLOR_EDGE);
            }
        }

        // Right-edge resize handle.
        if chat_w >= EDGE_W {
            let hx = chat_w - EDGE_W;
            for row in panel_y..h {
                for dx in 0..EDGE_W {
                    set_px(&mut buf, w, hx + dx, row, COLOR_EDGE);
                }
            }
        }

        let chars_per_line = (chat_w.saturating_sub(CHAT_PAD * 2 + EDGE_W) / CHAR_W).max(1) as usize;

        // Pre-wrap input so history knows how much space to leave.
        let cursor_char = if (self.tick / 30) % 2 == 0 { "_" } else { " " };
        let input_display = format!("> {}{}", self.chat_input, cursor_char);
        let input_wrapped = wrap_line(&input_display, chars_per_line);
        let input_line_count = input_wrapped.len() as u32;
        let input_top_y = h.saturating_sub(CHAT_PAD + CHAR_H * input_line_count);

        // Identity header: "You: fp8  Host: fp8 [known/new]"
        let header_y = panel_y + CHAT_PAD;
        let you_fp = self.client_fp.get(..8).unwrap_or(&self.client_fp);
        let host_part = if self.host_fp.is_empty() {
            "Host: ...".to_string()
        } else {
            let fp = self.host_fp.get(..8).unwrap_or(&self.host_fp);
            match self.host_trusted {
                Some(true)  => format!("Host: {fp} [known]"),
                Some(false) => format!("Host: {fp} [new]"),
                None        => format!("Host: {fp}"),
            }
        };
        let id_str = format!("You: {you_fp}  {host_part}");
        if header_y < input_top_y {
            for (i, line) in wrap_line(&id_str, chars_per_line).iter().enumerate() {
                let ly = header_y + i as u32 * CHAR_H;
                if ly + CHAR_H > input_top_y { break; }
                draw_str(&mut buf, w, CHAT_PAD, ly, line, COLOR_INPUT);
            }
        }

        // History: wrap every line, show most-recent that fit above the input.
        let history_top = header_y + CHAR_H;
        let history_bot = input_top_y.saturating_sub(4);
        if history_bot > history_top {
            let avail = ((history_bot - history_top) / CHAR_H) as usize;
            let mut wrapped: Vec<(String, u32)> = Vec::new();
            for (sender, text) in &self.chat_history {
                let (line_str, color) = if sender.is_empty() {
                    (text.clone(), COLOR_SYSTEM)
                } else {
                    (format!("{sender}: {text}"), COLOR_TEXT)
                };
                for wl in wrap_line(&line_str, chars_per_line) {
                    wrapped.push((wl, color));
                }
            }
            let skip = wrapped.len().saturating_sub(avail);
            for (i, (line, color)) in wrapped[skip..].iter().enumerate() {
                let ly = history_top + i as u32 * CHAR_H;
                if ly + CHAR_H > history_bot { break; }
                draw_str(&mut buf, w, CHAT_PAD, ly, line, *color);
            }
        }

        // Input pinned to bottom, growing upward as it wraps.
        for (i, line) in input_wrapped.iter().enumerate() {
            let ly = input_top_y + i as u32 * CHAR_H;
            if ly + CHAR_H > h { break; }
            draw_str(&mut buf, w, CHAT_PAD, ly, line, COLOR_INPUT);
        }

        // ── Menu overlay (Escape) ─────────────────────────────────────────────
        if self.menu_open {
            let mw: u32 = 224;
            let mh: u32 = 80;
            let mx = w.saturating_sub(mw) / 2;
            let my = h.saturating_sub(CHAT_HEIGHT + mh) / 2;
            for row in my..my + mh {
                if row >= h { break; }
                let base = (row * w + mx) as usize;
                let len = mw.min(w.saturating_sub(mx)) as usize;
                if base + len <= buf.len() {
                    buf[base..base + len].fill(COLOR_MENU);
                }
            }
            draw_str(&mut buf, w, mx + 8, my + 12,             "[D] Disconnect", COLOR_MENU_T);
            draw_str(&mut buf, w, mx + 8, my + 12 + CHAR_H + 4, "[C] Cancel",    COLOR_MENU_T);
        }

        let _ = buf.present();
    }
}

impl ApplicationHandler for ClientApp {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("Screen Party — viewer")
            .with_inner_size(winit::dpi::PhysicalSize::new(1280u32, 720u32));
        let w = match el.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => { eprintln!("client window: {e}"); return; }
        };
        let ctx = Context::new(w.clone()).expect("softbuffer ctx");
        let srf = Surface::new(&ctx, w.clone()).expect("softbuffer surface");
        self.surface = Some(srf);
        self.context = Some(ctx);
        self.window  = Some(w);
    }

    fn exiting(&mut self, _el: &ActiveEventLoop) {
        self.surface = None;
        self.context = None;
        self.window  = None;
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        self.tick += 1;
        self.drain_events();
        self.advance_video();

        if self.disconnected {
            el.exit();
            return;
        }

        if let Some(w) = &self.window {
            w.request_redraw();
        }

        // Sleep until the next frame deadline so we don't spin the CPU.
        // During prebuffering, check every 5 ms for enough frames to arrive.
        let wake_at = if self.prebuffering {
            std::time::Instant::now() + Duration::from_millis(5)
        } else {
            self.next_frame_due
        };
        el.set_control_flow(ControlFlow::WaitUntil(wake_at));
    }

    fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::RedrawRequested => self.render(),

            WindowEvent::CloseRequested => {
                let _ = self.send_tx.send(ClientSend::Disconnect);
                self.disconnected = true;
            }

            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed =>
            {
                if self.menu_open {
                    match &event.logical_key {
                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("d") => {
                            let _ = self.send_tx.send(ClientSend::Disconnect);
                            self.disconnected = true;
                        }
                        Key::Character(s) if s.as_str().eq_ignore_ascii_case("c") => {
                            self.menu_open = false;
                        }
                        Key::Named(NamedKey::Escape) => {
                            self.menu_open = false;
                        }
                        _ => {}
                    }
                } else {
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            self.menu_open = true;
                        }
                        Key::Named(NamedKey::Backspace) => {
                            self.chat_input.pop();
                        }
                        Key::Named(NamedKey::Enter) => {
                            let text = std::mem::take(&mut self.chat_input);
                            if !text.is_empty() {
                                let _ = self.send_tx.send(ClientSend::ChatMessage(text));
                            }
                        }
                        Key::Named(NamedKey::Space) => {
                            self.chat_input.push(' ');
                        }
                        Key::Character(s) => {
                            for ch in s.chars() {
                                if !ch.is_control() {
                                    self.chat_input.push(ch);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = Some((position.x, position.y));
                if self.drag_chat_edge || self.drag_chat_top {
                    if let Some(win) = &self.window {
                        let sz = win.inner_size();
                        if self.drag_chat_edge {
                            self.chat_w = (position.x as u32).clamp(CHAT_MIN_W, sz.width);
                        }
                        if self.drag_chat_top {
                            self.chat_h = sz.height.saturating_sub(position.y as u32).clamp(CHAT_MIN_H, sz.height);
                        }
                        win.request_redraw();
                    }
                }
            }

            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                match state {
                    ElementState::Pressed if !self.menu_open => {
                        if let (Some((cx, cy)), Some(win)) = (self.cursor_pos, &self.window) {
                            let sz = win.inner_size();
                            let ch = if self.chat_h == 0 { CHAT_HEIGHT } else { self.chat_h.min(sz.height) };
                            let cw = if self.chat_w == 0 { sz.width  } else { self.chat_w.min(sz.width) };
                            let panel_y = sz.height.saturating_sub(ch) as f64;
                            if cy >= panel_y {
                                if (cx - cw as f64).abs() < EDGE_HIT {
                                    self.drag_chat_edge = true;
                                } else if (cy - panel_y).abs() < EDGE_HIT {
                                    self.drag_chat_top = true;
                                }
                            }
                        }
                    }
                    ElementState::Released => {
                        self.drag_chat_edge = false;
                        self.drag_chat_top  = false;
                    }
                    _ => {}
                }
            }

            _ => {}
        }
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn run(host: &str, port: u16, name: Option<String>) {
    let client_fp = identity::ensure_identity();
    let (event_tx, event_rx) = mpsc::channel::<ClientEvent>();
    let (send_tx, send_rx)   = mpsc::channel::<ClientSend>();

    let host_owned = host.to_owned();
    let fp_owned   = client_fp.clone();
    std::thread::Builder::new()
        .name("net-client".into())
        .spawn(move || {
            if let Err(e) =
                run_network(host_owned, port, fp_owned, name, event_tx, send_rx)
            {
                eprintln!("[net] {e}");
            }
        })
        .expect("net-client thread");

    let mut event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::WaitUntil(std::time::Instant::now()));
    let mut app = ClientApp::new(event_rx, send_tx, client_fp);
    event_loop.run_app_on_demand(&mut app).ok();
}

// ── Pixel helpers ─────────────────────────────────────────────────────────────

fn rgba_to_xrgb(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

fn set_px(buf: &mut [u32], stride: u32, x: u32, y: u32, color: u32) {
    if let Some(p) = buf.get_mut((y * stride + x) as usize) {
        *p = color;
    }
}

fn draw_char(buf: &mut [u32], stride: u32, x: u32, y: u32, ch: char, color: u32) {
    if let Some(glyph) = font8x8::BASIC_FONTS.get(ch) {
        for (row_i, &row_bits) in glyph.iter().enumerate() {
            for col_i in 0..8u32 {
                if (row_bits >> col_i) & 1 != 0 {
                    set_px(buf, stride, x + col_i, y + row_i as u32, color);
                }
            }
        }
    }
}

fn wrap_line(s: &str, max_chars: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    if max_chars == 0 || chars.len() <= max_chars {
        return vec![chars.iter().collect()];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + max_chars).min(chars.len());
        let break_at = if end < chars.len() {
            chars[start..end].iter().rposition(|&c| c == ' ')
                .map(|i| start + i + 1)
                .unwrap_or(end)
        } else {
            end
        };
        out.push(chars[start..break_at].iter().collect::<String>().trim_end().to_string());
        start = break_at;
    }
    if out.is_empty() { out.push(String::new()); }
    out
}

fn draw_str(buf: &mut [u32], stride: u32, x: u32, y: u32, s: &str, color: u32) {
    let mut cx = x;
    for ch in s.chars() {
        draw_char(buf, stride, cx, y, ch, color);
        cx = cx.saturating_add(CHAR_W);
    }
}
