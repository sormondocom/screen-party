use crate::frame::{Frame, Rect};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("capture backend not available on this platform: {0}")]
    NotAvailable(String),
    #[error("capture region is invalid or out of bounds")]
    InvalidRegion,
    #[error("capture backend error: {0}")]
    Backend(String),
}

/// Metadata about a display that can be captured.
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    /// Position of the display's top-left corner in the virtual desktop.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// True if this is the primary display.
    pub primary: bool,
}

/// A live screen capture session for a selected region.
///
/// Create one via [`platform::new_capturer`] and call [`Capturer::next_frame`]
/// in a loop.  The capture region is an arbitrary rectangle within a display;
/// set it at construction time or call [`Capturer::set_region`] to update it
/// (which will trigger a full-dirty frame on the next [`DeltaDetector::feed`]).
pub trait Capturer: Send {
    /// Grab the next frame.  Blocks until a new frame is available.
    fn next_frame(&mut self) -> Result<Frame, CaptureError>;

    /// The region currently being captured, in display-local coordinates.
    fn region(&self) -> Rect;

    /// Change the capture region.  Returns an error if the new region falls
    /// outside the display bounds.
    fn set_region(&mut self, region: Rect) -> Result<(), CaptureError>;

    /// Resolution of the full display this capturer is attached to.
    fn display_info(&self) -> &DisplayInfo;
}
