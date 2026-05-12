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
const DRAG_BAR_H:  u32 = 14;
const CHAT_HEIGHT: u32 = DRAG_BAR_H + CHAT_PAD * 2 + (CHAT_LINES as u32 + 2) * CHAR_H + 4;
const CHAT_WIDTH:  u32 = 700;
const RESIZE_ZONE: f64 = 6.0;
const COLOR_BG:       u32 = 0x00_1A_1A_2E;
const COLOR_TEXT:     u32 = 0x00_E0_E0_E0;
const COLOR_INPUT:    u32 = 0x00_88_FF_88;
const COLOR_SYSTEM:   u32 = 0x00_FF_AA_00; // amber — join/leave/admin notifications
const COLOR_DRAG_BAR: u32 = 0x00_25_25_3E;

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
    chat_cursor:       Option<(f64, f64)>,
    host_display_name: String,
    host_fingerprint:  String,
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
        host_fingerprint:  String,
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
            chat_cursor:  None,
            host_display_name,
            host_fingerprint,
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

    // ── Admin commands ───────────────────────────────────────────────────────

    fn push_sys(&mut self, text: String) {
        self.chat_history.push((String::new(), text));
        if self.chat_history.len() > CHAT_LINES * 3 {
            let excess = self.chat_history.len() - CHAT_LINES;
            self.chat_history.drain(0..excess);
        }
    }

    fn handle_admin_command(&mut self, cmd: String) {
        let parts: Vec<&str> = cmd.splitn(3, ' ').collect();
        let Some(b) = self.broadcaster.clone() else {
            self.push_sys("No broadcaster active".to_string());
            return;
        };
        match parts[0] {
            "/approve" => match parts.get(1).copied().filter(|s| !s.is_empty()) {
                Some(fp) => {
                    if b.approve_pending(fp) {
                        self.push_sys(format!("Approved {fp}"));
                    } else {
                        self.push_sys(format!("No pending client matching '{fp}'"));
                    }
                }
                None => self.push_sys("Usage: /approve <fp>".to_string()),
            },
            "/deny" => match parts.get(1).copied().filter(|s| !s.is_empty()) {
                Some(fp) => {
                    let reason = parts.get(2).copied().unwrap_or("denied by host");
                    if b.deny_pending(fp, reason) {
                        self.push_sys(format!("Denied {fp}"));
                    } else {
                        self.push_sys(format!("No pending client matching '{fp}'"));
                    }
                }
                None => self.push_sys("Usage: /deny <fp> [reason]".to_string()),
            },
            "/kick" => match parts.get(1).copied().filter(|s| !s.is_empty()) {
                Some(fp) => {
                    if b.kick_client(fp) {
                        self.push_sys(format!("Kicked {fp}"));
                    } else {
                        self.push_sys(format!("No viewer matching '{fp}'"));
                    }
                }
                None => self.push_sys("Usage: /kick <fp>".to_string()),
            },
            "/viewers" => {
                let viewers = b.list_viewers();
                let pending = b.list_pending();
                if viewers.is_empty() && pending.is_empty() {
                    self.push_sys("No viewers connected".to_string());
                } else {
                    if !viewers.is_empty() {
                        self.push_sys(format!(
                            "Watching ({}): {}",
                            viewers.len(),
                            viewers.join(", "),
                        ));
                    }
                    if !pending.is_empty() {
                        self.push_sys(format!(
                            "Pending ({}): {}",
                            pending.len(),
                            pending.join(", "),
                        ));
                    }
                }
            }
            "/help" => {
                self.push_sys(
                    "/approve <fp>  /deny <fp> [msg]  /kick <fp>  /viewers".to_string(),
                );
            }
            _ => self.push_sys(format!("Unknown command '{}' — type /help", parts[0])),
        }
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
        let (w, h) = (sz.width, sz.height);

        buf.fill(COLOR_BG);

        // Drag bar.
        let bar_px = ((DRAG_BAR_H * w) as usize).min(buf.len());
        buf[..bar_px].fill(COLOR_DRAG_BAR);
        let dot_y = DRAG_BAR_H / 2 - 1;
        let cx = w / 2;
        for i in 0..3u32 {
            let dx = cx.saturating_sub(7) + i * 7;
            for dy in 0..2 {
                set_px(&mut buf, w, dx,     dot_y + dy, COLOR_TEXT);
                set_px(&mut buf, w, dx + 1, dot_y + dy, COLOR_TEXT);
            }
        }

        let chars_per_line = (w.saturating_sub(CHAT_PAD * 2) / CHAR_W).max(1) as usize;

        // Pre-wrap the input so we know how many lines it occupies before
        // computing the history region.
        let cursor_char = if (self.tick / 30) % 2 == 0 { "_" } else { " " };
        let input_display = format!("> {}{}", self.chat_input, cursor_char);
        let input_wrapped = wrap_line(&input_display, chars_per_line);
        let input_line_count = input_wrapped.len() as u32;
        let input_top_y = h.saturating_sub(CHAT_PAD + CHAR_H * input_line_count);

        // Identity header below drag bar.
        let header_y = DRAG_BAR_H + CHAT_PAD;
        if header_y < input_top_y {
            let id_str = if self.host_fingerprint.is_empty() {
                "Your ID: (no identity — run with --generate-key)".to_string()
            } else {
                format!("Your ID: {}", &self.host_fingerprint)
            };
            for (i, line) in wrap_line(&id_str, chars_per_line).iter().enumerate() {
                let ly = header_y + i as u32 * CHAR_H;
                if ly + CHAR_H > input_top_y { break; }
                draw_str(&mut buf, w, CHAT_PAD, ly, line, COLOR_INPUT);
            }
        }

        // History: wrap every line to the current column width, then show as
        // many of the most-recent wrapped lines as fit above the input.
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

        // Input lines pinned to bottom, growing upward as they wrap.
        for (i, line) in input_wrapped.iter().enumerate() {
            let ly = input_top_y + i as u32 * CHAR_H;
            if ly + CHAR_H > h { break; }
            draw_str(&mut buf, w, CHAT_PAD, ly, line, COLOR_INPUT);
        }

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
                                match net::proto::encode_video_frame(&dirty, &frame) {
                                    Ok(payload) => b.broadcast(Arc::new(net::BroadcastMsg::VideoFrame(Arc::new(payload)))),
                                    Err(e) => eprintln!("encode error: {e}"),
                                }
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
        if let Phase::Capturing { stop, thread, hook } =
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
                            if text.starts_with('/') {
                                self.handle_admin_command(text);
                            } else if !text.is_empty() {
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
                if self.is_chat_window(id) {
                    if state == ElementState::Pressed {
                        let maybe_sz = self.chat_window.as_ref().map(|w| w.inner_size());
                        if let (Some(sz), Some((cx, cy))) = (maybe_sz, self.chat_cursor) {
                            let (fw, fh) = (sz.width as f64, sz.height as f64);
                            let rz = RESIZE_ZONE;
                            let (left, right, top, bot) = (
                                cx < rz, cx > fw - rz, cy < rz, cy > fh - rz,
                            );
                            use winit::window::ResizeDirection as RD;
                            let resize_dir = match (left, right, top, bot) {
                                (true,  false, true,  false) => Some(RD::NorthWest),
                                (false, true,  true,  false) => Some(RD::NorthEast),
                                (true,  false, false, true)  => Some(RD::SouthWest),
                                (false, true,  false, true)  => Some(RD::SouthEast),
                                (true,  false, false, false) => Some(RD::West),
                                (false, true,  false, false) => Some(RD::East),
                                (false, false, true,  false) => Some(RD::North),
                                (false, false, false, true)  => Some(RD::South),
                                _ => None,
                            };
                            if let Some(dir) = resize_dir {
                                if let Some(win) = &self.chat_window {
                                    let _ = win.drag_resize_window(dir);
                                }
                            } else if cy < DRAG_BAR_H as f64 {
                                if let Some(win) = &self.chat_window {
                                    let _ = win.drag_window();
                                }
                            }
                        }
                    }
                } else if let Phase::Selecting { drag_start, cursor, .. } = &mut self.phase {
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
                if self.is_chat_window(id) {
                    self.chat_cursor = Some((position.x, position.y));
                } else if let Phase::Selecting { cursor, .. } = &mut self.phase {
                    *cursor = Some((position.x, position.y));
                }
            }

            _ => {}
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
    // Raise Windows multimedia timer resolution to 1 ms so that
    // thread::sleep calls in the capture loop are accurate instead of
    // rounding up to the default 15.6 ms system tick.
    #[cfg(target_os = "windows")]
    unsafe { windows::Win32::Media::timeBeginPeriod(1); }

    let cli = cli::Cli::parse();

    match cli.mode {
        cli::Mode::Host { port, generate_key } => {
            if generate_key {
                match identity::run_keygen_wizard() {
                    Ok(_) => {}
                    Err(e) => { eprintln!("keygen: {e}"); return; }
                }
            }
            run_host_gui(port);
        }

        cli::Mode::Client { host, port, name } => {
            client_window::run(&host, port, name);
        }
    }
}

fn run_host_gui(port: u16) {
    // Load host fingerprint for the handshake (empty string if no identity generated yet).
    let host_fp = identity::load_fingerprint().unwrap_or_default();
    let host_display_name = if host_fp.is_empty() {
        "host".to_string()
    } else {
        host_fp.get(..8).unwrap_or(&host_fp).to_string()
    };
    let host_fingerprint = host_fp.clone();

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

    let broadcaster = net::Broadcaster::new(host_fp, sample_rate, channels);

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

    println!("Screen Party — hosting on port {port}");
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

    let mut app = App::new(display, Some(broadcaster), Some(chat_rx), host_display_name, host_fingerprint);
    event_loop.run_app_on_demand(&mut app).ok();
    println!("Goodbye.");
}
