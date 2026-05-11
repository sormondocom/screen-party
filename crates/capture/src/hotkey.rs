//! Global keyboard hook — fires regardless of which window has focus.
//! Currently Windows only (`WH_KEYBOARD_LL`); stubs for other platforms.

#[cfg(target_os = "windows")]
mod win {
    use std::cell::Cell;
    use std::sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    };
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows::Win32::{
        Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM},
        UI::{
            Input::KeyboardAndMouse::GetKeyState,
            WindowsAndMessaging::{
                CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx,
                HHOOK, KBDLLHOOKSTRUCT, WH_KEYBOARD_LL,
            },
        },
    };

    const WM_KEYDOWN: usize = 0x0100;
    const VK_ESCAPE: u32 = 0x1B;
    const VK_Q: u32 = 0x51;
    const VK_CTRL: i32 = 0x11;
    const DOUBLE_TAP_MS: u64 = 400;

    static LAST_ESC_MS: AtomicU64 = AtomicU64::new(0);

    thread_local! {
        static STOP_PTR: Cell<*const AtomicBool> = Cell::new(std::ptr::null());
        static QUIT_PTR: Cell<*const AtomicBool> = Cell::new(std::ptr::null());
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    unsafe extern "system" fn hook_proc(
        code: i32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if code >= 0 && wparam.0 == WM_KEYDOWN {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            match kb.vkCode {
                vk if vk == VK_ESCAPE => {
                    let now = now_ms();
                    let prev = LAST_ESC_MS.swap(now, Ordering::Relaxed);
                    if prev > 0 && now.saturating_sub(prev) < DOUBLE_TAP_MS {
                        LAST_ESC_MS.store(0, Ordering::Relaxed);
                        STOP_PTR.with(|c| {
                            let p = c.get();
                            if !p.is_null() {
                                (*p).store(true, Ordering::Relaxed);
                            }
                        });
                    }
                }
                vk if vk == VK_Q => {
                    if GetKeyState(VK_CTRL) as u16 & 0x8000 != 0 {
                        QUIT_PTR.with(|c| {
                            let p = c.get();
                            if !p.is_null() {
                                (*p).store(true, Ordering::Relaxed);
                            }
                        });
                        STOP_PTR.with(|c| {
                            let p = c.get();
                            if !p.is_null() {
                                (*p).store(true, Ordering::Relaxed);
                            }
                        });
                    }
                }
                _ => {}
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }

    /// RAII keyboard hook. Drop to uninstall.
    /// Must be held on the thread running the Win32 message pump.
    pub struct Hook {
        handle: HHOOK,
        // Keep Arcs alive so the raw pointers remain valid.
        _stop: Arc<AtomicBool>,
        _quit: Arc<AtomicBool>,
    }

    impl Hook {
        pub fn install(stop: Arc<AtomicBool>, quit: Arc<AtomicBool>) -> Self {
            LAST_ESC_MS.store(0, Ordering::Relaxed);
            STOP_PTR.with(|c| c.set(Arc::as_ptr(&stop)));
            QUIT_PTR.with(|c| c.set(Arc::as_ptr(&quit)));
            let handle = unsafe {
                SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), HINSTANCE::default(), 0)
                    .expect("SetWindowsHookExW")
            };
            Self { handle, _stop: stop, _quit: quit }
        }
    }

    impl Drop for Hook {
        fn drop(&mut self) {
            unsafe { let _ = UnhookWindowsHookEx(self.handle); }
            STOP_PTR.with(|c| c.set(std::ptr::null()));
            QUIT_PTR.with(|c| c.set(std::ptr::null()));
        }
    }
}

#[cfg(target_os = "windows")]
pub use win::Hook;

/// On non-Windows platforms: stub that does nothing.
/// Global hotkey support per-platform is a future task.
#[cfg(not(target_os = "windows"))]
pub struct Hook;

#[cfg(not(target_os = "windows"))]
impl Hook {
    pub fn install(
        _stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        _quit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self
    }
}
