//! macOS screen capture via ScreenCaptureKit (macOS 12.3+).
//!
//! SCShareableContent enumerates displays and windows.  SCStream delivers
//! frames as CVPixelBuffers which we convert to RGBA and crop to the
//! user-selected region.
//!
//! TODO: implement — stub returns NotAvailable for now.

use crate::{
    capturer::{CaptureError, Capturer, DisplayInfo},
    frame::Rect,
};

pub(super) fn list_displays() -> Result<Vec<DisplayInfo>, CaptureError> {
    Err(CaptureError::NotAvailable(
        "ScreenCaptureKit capture not yet implemented".into(),
    ))
}

pub(super) fn new_capturer(
    _display: &DisplayInfo,
    _region: Rect,
) -> Result<Box<dyn Capturer>, CaptureError> {
    Err(CaptureError::NotAvailable(
        "ScreenCaptureKit capture not yet implemented".into(),
    ))
}
