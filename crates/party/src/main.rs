#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod cli;
mod client_window;
mod identity;
mod net;

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::thread::JoinHandle;

use audio::{AudioCapturer, CpalLoopbackCapturer};
use font8x8::UnicodeFonts;
use capture::{
    DeltaDetector, DisplayInfo, Hook, QuadTreeConfig, Rect,
    platform,
};
use clap::Parser;
use softbuffer::{Context, Surface};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, Modifiers, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey},
    platform::run_on_demand::EventLoopExtRunOnDemand,
    window::{Fullscreen, Window, WindowId, WindowLevel},
};

// ── Pixel constants (premultiplied ARGB) ─────────────────────────────────────

const SEL_OVERLAY:  u32 = 0x99_00_00_00; // semi-transparent dark tint outside selection
const SEL_INTERIOR: u32 = 0x00_00_00_00; // fully transparent inside selection
const SEL_BORDER:   u32 = 0xFF_00_E5_FF; // solid cyan (alpha=0xFF → opaque)
const SEL_BORDER_W: u32 = 2;
const CAP_BORDER:   u32 = 0x00_00_E5_FF; // solid cyan (alpha=0xFF → opaque)
const CAP_BORDER_W: u32 = 3;

// ── Chat panel constants ──────────────────────────────────────────────────────

const CHAR_W:      u32 = 9;  // 8px glyph + 1px letter spacing
const CHAR_H:      u32 = 11; // 8px glyph + 3px line gap
const CHAT_LINES:  usize = 28;
const CHAT_PAD:    u32 = 8;
const CHAT_HEIGHT: u32 = CHAT_PAD * 2 + (CHAT_LINES as u32 + 1) * CHAR_H + 4;
const CHAT_WIDTH:  u32 = 700;
const COLOR_BG:    u32 = 0x00_1A_1A_2E;
const COLOR_TEXT:  u32 = 0x00_E0_E0_E0;
const COLOR_INPUT: u32 = 0x00_88_FF_88;

// ── Application state ─────────────────────────────────────────────────────────

enum Phase {
    /// Showing the fullscreen region-selection overlay.
    Selecting {
        drag_start: Option<(f64, f64)>,
        cursor: Option<(f64, f64)>,
        modifiers: Modifiers,
    },
    /// Capture is running; overlay border is visible.
    Capturing {
        region: Rect,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
        hook: Option<Hook>,
    },
}

struct App {
    quit:        Arc<AtomicBool>,
    display:     DisplayInfo,
    phase:       Phase,
    broadcaster: Option<Arc<net::Broadcaster>>,
    // Chat state
    chat_rx:           Option<mpsc::Receiver<(String, String)>>,
    chat_history:      Vec<(String, String)>,
    chat_input:        String,
    host_display_name: String,
    tick:              u64,
    // Overlay window — drop order: surface → context → window.
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    context: Option<Context<Rc<Window>>>,
    window:  Option<Rc<Window>>,
    // Chat window (created while Capturing, destroyed on return to Selecting).
    chat_surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    chat_context: Option<Context<Rc<Window>>>,
    chat_window:  Option<Rc<Window>>,
    // Pending transition requested from within an event handler.
    next_phase: Option<PendingPhase>,
}

enum PendingPhase {
    Selecting,
    Capturing(Rect),
}

impl App {
    fn new(
        display:           DisplayInfo,
        broadcaster:       Option<Arc<net::Broadcaster>>,
        chat_rx:           Option<mpsc::Receiver<(String, String)>>,
        host_display_name: String,
    ) -> Self {
        Self {
            quit: Arc::new(AtomicBool::new(false)),
            display,
            phase: Phase::Selecting {
                drag_start: None,
                cursor: None,
                modifiers: Modifiers::default(),
            },
            broadcaster,
            chat_rx,
            chat_history: Vec::new(),
            chat_input:   String::new(),
            host_display_name,
            tick: 0,
            surface: None,
            context: None,
            window: None,
            chat_surface: None,
            chat_context: None,
            chat_window:  None,
            next_phase: None,
        }
    }

    // ── Window management ────────────────────────────────────────────────────

    fn destroy_window(&mut self) {
        self.surface = None;
        self.context = None;
        self.window = None;
    }

    fn destroy_chat_window(&mut self) {
        self.chat_surface = None;
        self.chat_context = None;
        self.chat_window  = None;
    }

    fn is_chat_window(&self, id: winit::window::WindowId) -> bool {
        self.chat_window.as_ref().map_or(false, |w| w.id() == id)
    }

    fn create_chat_window(&mut self, el: &ActiveEventLoop) {
        self.destroy_chat_window();
        let chat_x = (self.display.width as i32).saturating_sub(CHAT_WIDTH as i32);
        let attrs = Window::default_attributes()
            .with_title("Screen Party — chat")
            .with_decorations(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_position(winit::dpi::PhysicalPosition::new(chat_x, 0))
            .with_inner_size(winit::dpi::PhysicalSize::new(CHAT_WIDTH, CHAT_HEIGHT));
        let w = match el.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => { eprintln!("chat window: {e}"); return; }
        };
        let ctx = Context::new(w.clone()).expect("softbuffer ctx");
        let srf = Surface::new(&ctx, w.clone()).expect("softbuffer surface");
        self.chat_surface = Some(srf);
        self.chat_context = Some(ctx);
        self.chat_window  = Some(w);
    }

    fn render_chat(&mut self) {
        let (Some(win), Some(srf)) = (&self.chat_window, &mut self.chat_surface) else { return };
        let sz = win.inner_size();
        let (Some(nw), Some(nh)) =
            (NonZeroU32::new(sz.width), NonZeroU32::new(sz.height))
        else { return };
        if srf.resize(nw, nh).is_err() { return; }
        let Ok(mut buf) = srf.buffer_mut() else { return };
        let (w, _h) = (sz.width, sz.height);

        buf.fill(COLOR_BG);

        let start = self.chat_history.len().saturating_sub(CHAT_LINES);
        for (i, (sender, text)) in self.chat_history[start..].iter().enumerate() {
            let ly = CHAT_PAD + i as u32 * CHAR_H;
            let line = format!("{sender}: {text}");
            draw_str(&mut buf, w, CHAT_PAD, ly, &line, COLOR_TEXT);
        }

        let input_y = CHAT_PAD + CHAT_LINES as u32 * CHAR_H + 4;
        let cursor = if (self.tick / 30) % 2 == 0 { "_" } else { " " };
        let input_display = format!("> {}{}", self.chat_input, cursor);
        draw_str(&mut buf, w, CHAT_PAD, input_y, &input_display, COLOR_INPUT);

        let _ = buf.present();
    }

    fn create_selector_window(&mut self, el: &ActiveEventLoop) {
        self.destroy_window();
        let attrs = Window::default_attributes()
            .with_title("Screen Party — drag to select | Ctrl+Q to quit")
            .with_decorations(false)
            .with_transparent(true)
            .with_fullscreen(Some(Fullscreen::Borderless(None)));
        let w = match el.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => { eprintln!("selector window: {e}"); return; }
        };
        let ctx = Context::new(w.clone()).expect("softbuffer ctx");
        let srf = Surface::new(&ctx, w.clone()).expect("softbuffer surface");
        self.surface = Some(srf);
        self.context = Some(ctx);
        self.window = Some(w);
    }

    fn create_overlay_window(&mut self, el: &ActiveEventLoop, region: Rect) {
        self.destroy_window();
        // Do NOT use with_transparent(true): on Windows that sets
        // WS_EX_NOREDIRECTIONBITMAP which disables the DC that softbuffer
        // needs for BitBlt.  We add WS_EX_LAYERED manually below instead.
        let attrs = Window::default_attributes()
            .with_decorations(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_position(winit::dpi::PhysicalPosition::new(
                region.x as i32,
                region.y as i32,
            ))
            .with_inner_size(winit::dpi::PhysicalSize::new(
                region.width,
                region.height,
            ));
        let w = match el.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => { eprintln!("overlay window: {e}"); return; }
        };
        let _ = w.set_cursor_hittest(false);
        let ctx = Context::new(w.clone()).expect("softbuffer ctx");
        let srf = Surface::new(&ctx, w.clone()).expect("softbuffer surface");
        // Cut a hole in the window centre so the desktop shows through.
        set_frame_region(&w, region.width, region.height, CAP_BORDER_W);
        self.surface = Some(srf);
        self.context = Some(ctx);
        self.window = Some(w);
    }

    // ── Phase transitions ────────────────────────────────────────────────────

    fn enter_capturing(&mut self, el: &ActiveEventLoop, region: Rect) {
        self.create_overlay_window(el, region);
        self.create_chat_window(el);

        // Advertise real capture dimensions to connecting clients.
        if let Some(b) = &self.broadcaster {
            b.set_stream_dims(region.width, region.height, 30);
        }

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread  = stop.clone();
        let display      = self.display.clone();
        let config       = QuadTreeConfig::for_resolution(region.width, region.height);
        let broadcaster  = self.broadcaster.clone();

        let thread = std::thread::spawn(move || {
            let mut capturer = match platform::new_capturer(&display, region) {
                Ok(c) => c,
                Err(e) => { eprintln!("capture start: {e}"); return; }
            };
            let mut detector = DeltaDetector::new(config);
            let frame_budget = std::time::Duration::from_nanos(1_000_000_000 / 30);
            while !stop_thread.load(Ordering::Relaxed) {
                let frame_start = std::time::Instant::now();
                match capturer.next_frame() {
                    Ok(frame) => {
                        let frame = Arc::new(frame);
                        let dirty = detector.feed(frame.clone());
                        if !dirty.is_empty() {
                            if let Some(b) = &broadcaster {
                                b.broadcast(Arc::new(net::BroadcastMsg::VideoFrame {
                                    rects: dirty,
                                    frame,
                                }));
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("capture error: {e}");
                        stop_thread.store(true, Ordering::Relaxed);
                        break;
                    }
                }
                let elapsed = frame_start.elapsed();
                if elapsed < frame_budget {
                    std::thread::sleep(frame_budget - elapsed);
                }
            }
        });

        let hook = Hook::install(stop.clone(), self.quit.clone());

        self.phase = Phase::Capturing {
            region,
            stop,
            thread: Some(thread),
            hook: Some(hook),
        };
    }

    fn enter_selecting(&mut self, el: &ActiveEventLoop) {
        self.destroy_chat_window();
        self.chat_input.clear();

        // Capture stopped — reset dims so clients connecting mid-select get zeros.
        if let Some(b) = &self.broadcaster {
            b.set_stream_dims(0, 0, 0);
        }

        // Stop capture cleanly before recreating the selector.
        if let Phase::Capturing { stop, thread, hook, .. } =
            std::mem::replace(
                &mut self.phase,
                Phase::Selecting {
                    drag_start: None,
                    cursor: None,
                    modifiers: Modifiers::default(),
                },
            )
        {
            drop(hook); // uninstall keyboard hook first
            stop.store(true, Ordering::Relaxed);
            if let Some(h) = thread {
                let _ = h.join(); // capture thread exits in ≤ 33 ms
            }
        }
        self.create_selector_window(el);
    }

    // ── Rendering ────────────────────────────────────────────────────────────

    fn render(&mut self) {
        let (Some(win), Some(srf)) = (&self.window, &mut self.surface) else { return };
        let sz = win.inner_size();
        let (Some(nw), Some(nh)) =
            (NonZeroU32::new(sz.width), NonZeroU32::new(sz.height))
        else { return };
        if srf.resize(nw, nh).is_err() { return; }
        let Ok(mut buf) = srf.buffer_mut() else { return };
        let (w, h) = (sz.width, sz.height);

        match &self.phase {
            Phase::Selecting { drag_start, cursor, .. } => {
                buf.fill(SEL_OVERLAY);
                if let (Some(a), Some(b)) = (drag_start, cursor) {
                    let x1 = a.0.min(b.0) as u32;
                    let y1 = a.1.min(b.1) as u32;
                    let x2 = (a.0.max(b.0) as u32).min(w.saturating_sub(1));
                    let y2 = (a.1.max(b.1) as u32).min(h.saturating_sub(1));
                    for row in y1..=y2 {
                        let s = (row * w + x1) as usize;
                        let e = (row * w + x2 + 1).min(buf.len() as u32) as usize;
                        buf[s..e].fill(SEL_INTERIOR);
                    }
                    for col in x1..=x2 {
                        for d in 0..SEL_BORDER_W {
                            set_px(&mut buf, w, col, y1 + d, SEL_BORDER);
                            set_px(&mut buf, w, col, y2.saturating_sub(d), SEL_BORDER);
                        }
                    }
                    for row in y1..=y2 {
                        for d in 0..SEL_BORDER_W {
                            set_px(&mut buf, w, x1 + d, row, SEL_BORDER);
                            set_px(&mut buf, w, x2.saturating_sub(d), row, SEL_BORDER);
                        }
                    }
                }
            }
            Phase::Capturing { .. } => {
                // Fill the whole buffer with cyan. SetWindowRgn has already
                // punched out the interior, so only the border pixels are visible.
                buf.fill(CAP_BORDER);
            }
        }
        let _ = buf.present();
    }
}

// Punch a rectangular hole in the centre of `window`, leaving only a solid
// border of `border` pixels on each side.  No transparency API needed — the
// missing pixels simply expose whatever is behind the window.
fn set_frame_region(window: &Window, width: u32, height: u32, border: u32) {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::{
            Foundation::{BOOL, HWND},
            Graphics::Gdi::{CombineRgn, CreateRectRgn, DeleteObject, HGDIOBJ, RGN_DIFF, SetWindowRgn},
        };
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Ok(handle) = window.window_handle() else { return };
        let RawWindowHandle::Win32(h) = handle.as_raw() else { return };
        let hwnd = HWND(h.hwnd.get() as *mut _);
        let b = border as i32;
        let w = width as i32;
        let h = height as i32;
        unsafe {
            let outer = CreateRectRgn(0, 0, w, h);
            let inner = CreateRectRgn(b, b, w - b, h - b);
            let frame = CreateRectRgn(0, 0, 0, 0);
            CombineRgn(frame, outer, inner, RGN_DIFF);
            // System takes ownership of `frame`; we must not free it.
            SetWindowRgn(hwnd, frame, BOOL(1));
            let _ = DeleteObject(HGDIOBJ(outer.0));
            let _ = DeleteObject(HGDIOBJ(inner.0));
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = (window, width, height, border);
}

#[inline]
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

fn draw_str(buf: &mut [u32], stride: u32, x: u32, y: u32, s: &str, color: u32) {
    let mut cx = x;
    for ch in s.chars() {
        draw_char(buf, stride, cx, y, ch, color);
        cx = cx.saturating_add(CHAR_W);
    }
}

// ── ApplicationHandler ────────────────────────────────────────────────────────

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        // Called once at startup — create the initial selector window.
        self.create_selector_window(el);
    }

    fn exiting(&mut self, _el: &ActiveEventLoop) {
        self.surface = None;
        self.context = None;
        self.window  = None;
        self.destroy_chat_window();
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        self.tick += 1;

        // Apply any pending phase transition requested from window_event.
        if let Some(pending) = self.next_phase.take() {
            match pending {
                PendingPhase::Selecting => self.enter_selecting(el),
                PendingPhase::Capturing(r) => self.enter_capturing(el, r),
            }
        }

        if self.quit.load(Ordering::Relaxed) {
            if let Some(b) = &self.broadcaster {
                b.broadcast(Arc::new(net::BroadcastMsg::Disconnect));
            }
            el.exit();
            return;
        }

        // During capture, check if double-Escape (stop flag) was triggered.
        if let Phase::Capturing { stop, .. } = &self.phase {
            if stop.load(Ordering::Relaxed) {
                self.next_phase = Some(PendingPhase::Selecting);
            }
        }

        // Drain incoming chat messages from clients.
        if let Some(rx) = &self.chat_rx {
            while let Ok((sender, text)) = rx.try_recv() {
                self.chat_history.push((sender, text));
                if self.chat_history.len() > CHAT_LINES * 3 {
                    let excess = self.chat_history.len() - CHAT_LINES;
                    self.chat_history.drain(0..excess);
                }
            }
        }

        if let Some(w) = &self.window {
            w.request_redraw();
        }
        if let Some(w) = &self.chat_window {
            w.request_redraw();
        }
    }

    fn window_event(
        &mut self,
        _el: &ActiveEventLoop,
        id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::RedrawRequested => {
                if self.is_chat_window(id) {
                    self.render_chat();
                } else {
                    self.render();
                }
            }

            WindowEvent::ModifiersChanged(mods) => {
                if let Phase::Selecting { modifiers, .. } = &mut self.phase {
                    *modifiers = mods;
                }
            }

            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed =>
            {
                if self.is_chat_window(id) {
                    // Chat window: route typing to chat input.
                    match &event.logical_key {
                        Key::Named(NamedKey::Backspace) => {
                            self.chat_input.pop();
                        }
                        Key::Named(NamedKey::Enter) => {
                            let text = std::mem::take(&mut self.chat_input);
                            if !text.is_empty() {
                                let sender = self.host_display_name.clone();
                                self.chat_history.push((sender.clone(), text.clone()));
                                if self.chat_history.len() > CHAT_LINES * 3 {
                                    let excess = self.chat_history.len() - CHAT_LINES;
                                    self.chat_history.drain(0..excess);
                                }
                                if let Some(b) = &self.broadcaster {
                                    b.broadcast(Arc::new(net::BroadcastMsg::ChatMessage {
                                        sender,
                                        text,
                                    }));
                                }
                            }
                        }
                        Key::Named(NamedKey::Escape) => {
                            self.chat_input.clear();
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
                } else if let Phase::Selecting { modifiers, drag_start, cursor } =
                    &mut self.phase
                {
                    let ctrl = modifiers.state().contains(ModifiersState::CONTROL);
                    if ctrl && event.physical_key == PhysicalKey::Code(KeyCode::KeyQ) {
                        self.quit.store(true, Ordering::Relaxed);
                        return;
                    }
                    match event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            *drag_start = None;
                        }
                        Key::Named(NamedKey::Enter) => {
                            self.next_phase =
                                make_capture_phase(*drag_start, *cursor);
                        }
                        _ => {}
                    }
                }
            }

            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                if let Phase::Selecting { drag_start, cursor, .. } = &mut self.phase {
                    match state {
                        ElementState::Pressed => *drag_start = *cursor,
                        ElementState::Released => {
                            self.next_phase =
                                make_capture_phase(*drag_start, *cursor);
                        }
                    }
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                if let Phase::Selecting { cursor, .. } = &mut self.phase {
                    *cursor = Some((position.x, position.y));
                }
            }

            _ => {}
        }
    }
}

fn make_capture_phase(
    drag_start: Option<(f64, f64)>,
    cursor: Option<(f64, f64)>,
) -> Option<PendingPhase> {
    let (a, b) = (drag_start?, cursor?);
    let x1 = a.0.min(b.0) as u32;
    let y1 = a.1.min(b.1) as u32;
    let x2 = a.0.max(b.0) as u32;
    let y2 = a.1.max(b.1) as u32;
    let w = x2.saturating_sub(x1);
    let h = y2.saturating_sub(y1);
    if w < 8 || h < 8 { return None; }
    Some(PendingPhase::Capturing(Rect::new(x1, y1, w, h)))
}

// ── main ─────────────────────────────────────────────────────────────────────

fn main() {
    let cli = cli::Cli::parse();

    match cli.mode {
        cli::Mode::Host { port, generate_key, interactive } => {
            if generate_key {
                match identity::run_keygen_wizard() {
                    Ok(_) => {}
                    Err(e) => { eprintln!("keygen: {e}"); return; }
                }
            }
            run_host_gui(port, interactive);
        }

        cli::Mode::Client { host, port, interactive } => {
            client_window::run(&host, port, interactive);
        }
    }
}

fn run_host_gui(port: u16, interactive: bool) {
    // Load host fingerprint for the handshake (empty string if no identity generated yet).
    let host_fp = identity::load_fingerprint().unwrap_or_default();
    let host_display_name = if host_fp.is_empty() {
        "host".to_string()
    } else {
        host_fp.get(..8).unwrap_or(&host_fp).to_string()
    };

    // Probe audio config before opening the listener so STREAM_INFO carries
    // real values.  Audio capture starts immediately so the first client
    // connection gets audio without any warm-up delay.
    let audio_capturer = CpalLoopbackCapturer::new();
    let (sample_rate, channels) = match &audio_capturer {
        Ok(c)  => (c.config().sample_rate, c.config().channels),
        Err(e) => {
            eprintln!("[audio] loopback unavailable: {e} — host will stream video only");
            (48_000, 2)
        }
    };

    let broadcaster = net::Broadcaster::new(interactive, host_fp, sample_rate, channels);

    // Wire up the host chat channel before accepting any clients.
    let (chat_tx, chat_rx) = mpsc::channel::<(String, String)>();
    broadcaster.set_chat_sender(chat_tx);

    broadcaster.listen(port);

    // Spawn audio broadcast thread if capture opened successfully.
    if let Ok(mut capturer) = audio_capturer {
        let b = broadcaster.clone();
        std::thread::Builder::new()
            .name("audio-capture".into())
            .spawn(move || loop {
                match capturer.next_frame() {
                    Ok(frame) => {
                        let payload = Arc::new(net::proto::encode_audio_chunk(&frame.samples));
                        b.broadcast(Arc::new(net::BroadcastMsg::AudioChunk(payload)));
                    }
                    Err(e) => {
                        eprintln!("[audio] capture error: {e}");
                        break;
                    }
                }
            })
            .ok();
    }

    println!(
        "Screen Party — hosting on port {port}{}",
        if interactive { " (interactive key exchange required)" } else { "" },
    );
    println!("  Drag to select a region, release to start capture");
    println!("  Esc Esc  — stop capture and reselect");
    println!("  Ctrl+Q   — quit");

    let displays = match platform::list_displays() {
        Ok(d) if !d.is_empty() => d,
        Ok(_) => { eprintln!("no displays found"); return; }
        Err(e) => { eprintln!("display enum: {e}"); return; }
    };
    for d in &displays {
        println!(
            "  Display {}: {} ({}×{}){}",
            d.id, d.name, d.width, d.height,
            if d.primary { " [primary]" } else { "" }
        );
    }
    let display = displays.into_iter().find(|d| d.primary)
        .unwrap_or_else(|| panic!("no primary display"));

    let mut event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App::new(display, Some(broadcaster), Some(chat_rx), host_display_name);
    event_loop.run_app_on_demand(&mut app).ok();
    println!("Goodbye.");
}
