use crate::{
    capturer::{CaptureError, Capturer, DisplayInfo},
    frame::Rect,
};

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod linux;

/// Enumerate all displays available for capture on the current platform.
pub fn list_displays() -> Result<Vec<DisplayInfo>, CaptureError> {
    #[cfg(target_os = "windows")]
    return windows::list_displays();

    #[cfg(target_os = "macos")]
    return macos::list_displays();

    #[cfg(target_os = "linux")]
    return linux::list_displays();

    #[allow(unreachable_code)]
    Err(CaptureError::NotAvailable("unsupported platform".into()))
}

/// Create a capturer for the given display, initially capturing `region`.
///
/// `region` is in display-local pixel coordinates.  Pass the full display
/// bounds if you want the whole screen; pass a sub-rect to restrict the
/// capture to a drawn selection box.
pub fn new_capturer(
    display: &DisplayInfo,
    region: Rect,
) -> Result<Box<dyn Capturer>, CaptureError> {
    #[cfg(target_os = "windows")]
    return windows::new_capturer(display, region);

    #[cfg(target_os = "macos")]
    return macos::new_capturer(display, region);

    #[cfg(target_os = "linux")]
    return linux::new_capturer(display, region);

    #[allow(unreachable_code)]
    Err(CaptureError::NotAvailable("unsupported platform".into()))
}
