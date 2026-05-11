/// RGBA row-major pixel buffer.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Raw RGBA bytes, length == width * height * 4.
    pub data: Vec<u8>,
}

impl Frame {
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Self {
        debug_assert_eq!(data.len(), (width * height * 4) as usize);
        Self { width, height, data }
    }

    #[inline]
    pub fn pixel_offset(&self, x: u32, y: u32) -> usize {
        ((y * self.width + x) * 4) as usize
    }
}

/// A rectangle in screen-space pixels, used both for capture regions and dirty
/// regions reported by the delta detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    pub fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    pub fn contains(&self, x: u32, y: u32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}
