//! Client-side rendering window: video display, chat overlay, disconnect menu.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::mpsc;

use font8x8::UnicodeFonts;
use softbuffer::{Context, Surface};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{Key, NamedKey},
    platform::run_on_demand::EventLoopExtRunOnDemand,
    window::{Window, WindowId},
};

use audio::CpalPlayer;

use crate::identity;
use crate::net::client::{run_network, ClientEvent, ClientSend};

// ── Layout constants ──────────────────────────────────────────────────────────

const CHAR_W:      u32 = 16; // 8px glyph × 2 scale
const CHAR_H:      u32 = 16;
const CHAT_LINES:  usize = 6;
const CHAT_PAD:    u32 = 8;
// panel height: top_pad + id header + history rows + gap + input row + bottom_pad
const CHAT_HEIGHT: u32 = CHAT_PAD * 2 + (CHAT_LINES as u32 + 2) * CHAR_H + 4;

const COLOR_BG:     u32 = 0x00_1A_1A_2E;
const COLOR_TEXT:   u32 = 0x00_E0_E0_E0;
const COLOR_INPUT:  u32 = 0x00_88_FF_88;
const COLOR_MENU:   u32 = 0x00_11_11_44;
const COLOR_MENU_T: u32 = 0x00_EE_EE_FF;

// ── ClientApp ─────────────────────────────────────────────────────────────────

struct ClientApp {
    event_rx:     mpsc::Receiver<ClientEvent>,
    send_tx:      mpsc::Sender<ClientSend>,
    // Stream state
    stream_w:     u32,
    stream_h:     u32,
    video_buf:    Vec<u32>, // 0x00RRGGBB, stream_w * stream_h pixels
    audio_out:    Option<CpalPlayer>,
    // Identity
    client_fp:    String,
    host_fp:      String,
    host_trusted: Option<bool>, // None = not yet received, Some(true) = known, Some(false) = new
    // Chat state
    chat_history: Vec<(String, String)>, // (sender, text)
    chat_input:   String,
    // UI state
    menu_open:    bool,
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
            stream_w:     0,
            stream_h:     0,
            video_buf:    Vec::new(),
            audio_out:    None,
            client_fp,
            host_fp:      String::new(),
            host_trusted: None,
            chat_history: Vec::new(),
            chat_input:   String::new(),
            menu_open:    false,
            tick:         0,
            disconnected: false,
            surface:      None,
            context:      None,
            window:       None,
        }
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.event_rx.try_recv() {
            match ev {
                ClientEvent::StreamInfo { width, height, sample_rate, channels, buffer_ms, .. } => {
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
                    // Open audio output pre-buffered to absorb the measured jitter.
                    let prebuf = (sample_rate as u64 * channels as u64 * buffer_ms / 1000) as usize;
                    match CpalPlayer::new(sample_rate, channels, prebuf) {
                        Ok(player) => self.audio_out = Some(player),
                        Err(e) => eprintln!("[audio] output unavailable: {e}"),
                    }
                }

                ClientEvent::VideoFrame(frame) => {
                    // Reallocate if dims changed (first frame, or host reselected region).
                    if self.stream_w != frame.width || self.stream_h != frame.height {
                        self.stream_w = frame.width;
                        self.stream_h = frame.height;
                        self.video_buf = vec![0u32; (frame.width * frame.height) as usize];
                    }
                    for rect in &frame.rects {
                        for (i, row_px) in
                            rect.pixels.chunks(rect.w as usize * 4).enumerate()
                        {
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
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
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
        let panel_y = h.saturating_sub(CHAT_HEIGHT);
        for row in panel_y..h {
            let base = (row * w) as usize;
            buf[base..base + w as usize].fill(COLOR_BG);
        }

        // Identity header: "You: fp8  Host: fp8 [known/new]"
        let header_y = panel_y + CHAT_PAD;
        let you_fp = self.client_fp.get(..8).unwrap_or(&self.client_fp);
        let you_str = format!("You: {you_fp}");
        draw_str(&mut buf, w, CHAT_PAD, header_y, &you_str, COLOR_INPUT);

        let host_str = if self.host_fp.is_empty() {
            "  Host: ...".to_string()
        } else {
            let fp = self.host_fp.get(..8).unwrap_or(&self.host_fp);
            match self.host_trusted {
                Some(true)  => format!("  Host: {fp} [known]"),
                Some(false) => format!("  Host: {fp} [new]"),
                None        => format!("  Host: {fp}"),
            }
        };
        let host_x = CHAT_PAD + you_str.len() as u32 * CHAR_W;
        draw_str(&mut buf, w, host_x, header_y, &host_str, COLOR_TEXT);

        // History (shifted down one row for the header).
        let start = self.chat_history.len().saturating_sub(CHAT_LINES);
        for (i, (sender, text)) in self.chat_history[start..].iter().enumerate() {
            let ly = panel_y + CHAT_PAD + CHAR_H + i as u32 * CHAR_H;
            let line = format!("{sender}: {text}");
            draw_str(&mut buf, w, CHAT_PAD, ly, &line, COLOR_TEXT);
        }

        // Input line with blinking cursor.
        let input_y = panel_y + CHAT_PAD + CHAR_H + CHAT_LINES as u32 * CHAR_H + 4;
        let cursor = if (self.tick / 30) % 2 == 0 { "_" } else { " " };
        let input_display = format!("> {}{}", self.chat_input, cursor);
        draw_str(&mut buf, w, CHAT_PAD, input_y, &input_display, COLOR_INPUT);

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

        if self.disconnected {
            el.exit();
            return;
        }

        if let Some(w) = &self.window {
            w.request_redraw();
        }
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

            _ => {}
        }
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn run(host: &str, port: u16, interactive: bool, name: Option<String>) {
    let client_fp = identity::load_fingerprint().unwrap_or_default();
    let (event_tx, event_rx) = mpsc::channel::<ClientEvent>();
    let (send_tx, send_rx)   = mpsc::channel::<ClientSend>();

    let host_owned = host.to_owned();
    let fp_owned   = client_fp.clone();
    std::thread::Builder::new()
        .name("net-client".into())
        .spawn(move || {
            if let Err(e) =
                run_network(host_owned, port, interactive, fp_owned, name, event_tx, send_rx)
            {
                eprintln!("[net] {e}");
            }
        })
        .expect("net-client thread");

    let mut event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
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
                    // Render at 2× scale (each pixel → 2×2 block).
                    let px = x + col_i * 2;
                    let py = y + row_i as u32 * 2;
                    set_px(buf, stride, px,     py,     color);
                    set_px(buf, stride, px + 1, py,     color);
                    set_px(buf, stride, px,     py + 1, color);
                    set_px(buf, stride, px + 1, py + 1, color);
                }
            }
        }
    }
}

fn draw_str(buf: &mut [u32], stride: u32, x: u32, y: u32, s: &str, color: u32) {
    let mut cx = x;
    for ch in s.chars() {
        draw_char(buf, stride, cx, y, ch, color);
        cx = cx.saturating_add(CHAR_W);
    }
}
