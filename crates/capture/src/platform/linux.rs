//! Linux screen capture.
//!
//! Two paths:
//!   - X11: XShm extension via x11rb for low-latency shared-memory grabs.
//!   - Wayland: xdg-desktop-portal + PipeWire screencast (required for
//!     compositors that don't expose a direct capture API).
//!
//! We detect at runtime which path to use based on DISPLAY / WAYLAND_DISPLAY.
//!
//! TODO: implement — stub returns NotAvailable for now.

use crate::{
    capturer::{CaptureError, Capturer, DisplayInfo},
    frame::Rect,
};

pub(super) fn list_displays() -> Result<Vec<DisplayInfo>, CaptureError> {
    Err(CaptureError::NotAvailable(
        "Linux capture not yet implemented".into(),
    ))
}

pub(super) fn new_capturer(
    _display: &DisplayInfo,
    _region: Rect,
) -> Result<Box<dyn Capturer>, CaptureError> {
    Err(CaptureError::NotAvailable(
        "Linux capture not yet implemented".into(),
    ))
}
