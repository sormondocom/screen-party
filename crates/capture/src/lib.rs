pub mod capturer;
pub mod frame;
pub mod hotkey;
pub mod platform;
pub mod quadtree;

pub use capturer::{CaptureError, Capturer, DisplayInfo};
pub use frame::{Frame, Rect};
pub use hotkey::Hook;
pub use quadtree::{DeltaDetector, QuadTreeConfig};
