use std::sync::Arc;

use crate::frame::{Frame, Rect};

/// Tuning knobs for the quadtree delta detector.
#[derive(Debug, Clone)]
pub struct QuadTreeConfig {
    /// Minimum tile dimension (pixels). Tiles smaller than this are never split.
    pub min_tile_px: u32,

    /// Fraction of pixels in a tile (0.0–1.0) that must differ before the tile
    /// is emitted as dirty without further subdivision.  Lower = more granular
    /// dirty rects; higher = coarser but faster.
    pub dirty_threshold: f32,

    /// Per-channel absolute difference that counts a pixel as "changed".
    /// A value of 0 means pixel-perfect; small values (e.g. 8) absorb
    /// compression artefacts and minor rendering noise.
    pub pixel_noise_floor: u8,
}

impl Default for QuadTreeConfig {
    fn default() -> Self {
        Self {
            min_tile_px: 32,
            dirty_threshold: 0.05,
            pixel_noise_floor: 8,
        }
    }
}

impl QuadTreeConfig {
    /// Suggest sensible defaults for a given capture resolution.
    pub fn for_resolution(width: u32, height: u32) -> Self {
        // Scale minimum tile so we get roughly 32×18 tiles on a 1080p frame.
        let min_tile_px = ((width.min(height)) / 32).max(8).next_power_of_two();
        Self {
            min_tile_px,
            ..Self::default()
        }
    }
}

/// Detects changed regions between successive frames using quadtree subdivision.
///
/// Feed frames in order; the detector returns a list of [`Rect`]s that cover
/// every pixel that changed.  The caller can use these rects to encode and
/// transmit only the parts of the screen that actually moved.
pub struct DeltaDetector {
    prev: Option<Arc<Frame>>,
    config: QuadTreeConfig,
}

impl DeltaDetector {
    pub fn new(config: QuadTreeConfig) -> Self {
        Self { prev: None, config }
    }

    /// Compare `frame` against the previous frame and return dirty regions.
    /// On the first call (no previous frame) the entire frame bounds are returned.
    /// Takes `Arc<Frame>` so callers can keep a reference for broadcasting without copying.
    pub fn feed(&mut self, frame: Arc<Frame>) -> Vec<Rect> {
        let dirty = match &self.prev {
            None => vec![Rect::new(0, 0, frame.width, frame.height)],
            Some(prev) => {
                let mut out = Vec::new();
                self.subdivide(
                    &frame,
                    prev,
                    0,
                    0,
                    frame.width,
                    frame.height,
                    &mut out,
                );
                out
            }
        };
        self.prev = Some(frame);
        dirty
    }

    /// Reset state — call this if the capture region changes.
    pub fn reset(&mut self) {
        self.prev = None;
    }

    fn subdivide(
        &self,
        curr: &Frame,
        prev: &Frame,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        out: &mut Vec<Rect>,
    ) {
        let ratio = self.change_ratio(curr, prev, x, y, w, h);

        if ratio == 0.0 {
            return;
        }

        let too_small = w <= self.config.min_tile_px || h <= self.config.min_tile_px;

        if too_small || ratio >= self.config.dirty_threshold {
            out.push(Rect::new(x, y, w, h));
            return;
        }

        // Split into four quadrants.  Use integer halving so tiles tile exactly.
        let hw = w / 2;
        let hh = h / 2;
        // NW
        self.subdivide(curr, prev, x,      y,      hw,    hh,    out);
        // NE
        self.subdivide(curr, prev, x + hw, y,      w - hw, hh,   out);
        // SW
        self.subdivide(curr, prev, x,      y + hh, hw,    h - hh, out);
        // SE
        self.subdivide(curr, prev, x + hw, y + hh, w - hw, h - hh, out);
    }

    fn change_ratio(&self, curr: &Frame, prev: &Frame, x: u32, y: u32, w: u32, h: u32) -> f32 {
        let noise = self.config.pixel_noise_floor as i16;
        let mut changed: u64 = 0;
        let total = (w as u64) * (h as u64);

        for py in y..y + h {
            let row_base_c = (py * curr.width + x) as usize * 4;
            let row_base_p = (py * prev.width + x) as usize * 4;

            for px in 0..w as usize {
                let ci = row_base_c + px * 4;
                let pi = row_base_p + px * 4;

                let dr = (curr.data[ci]     as i16 - prev.data[pi]     as i16).abs();
                let dg = (curr.data[ci + 1] as i16 - prev.data[pi + 1] as i16).abs();
                let db = (curr.data[ci + 2] as i16 - prev.data[pi + 2] as i16).abs();

                if dr > noise || dg > noise || db > noise {
                    changed += 1;
                }
            }
        }

        changed as f32 / total as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_frame(w: u32, h: u32, rgba: [u8; 4]) -> Frame {
        let data = rgba.iter().cycle().take((w * h * 4) as usize).copied().collect();
        Frame::new(w, h, data)
    }

    fn feed(det: &mut DeltaDetector, f: Frame) -> Vec<Rect> {
        det.feed(Arc::new(f))
    }

    #[test]
    fn identical_frames_produce_no_dirty_rects() {
        let mut det = DeltaDetector::new(QuadTreeConfig::default());
        feed(&mut det, solid_frame(256, 256, [0, 0, 0, 255]));
        let dirty = feed(&mut det, solid_frame(256, 256, [0, 0, 0, 255]));
        assert!(dirty.is_empty());
    }

    #[test]
    fn fully_changed_frame_produces_one_dirty_rect() {
        let mut det = DeltaDetector::new(QuadTreeConfig::default());
        feed(&mut det, solid_frame(256, 256, [0, 0, 0, 255]));
        let dirty = feed(&mut det, solid_frame(256, 256, [255, 255, 255, 255]));
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0], Rect::new(0, 0, 256, 256));
    }

    #[test]
    fn first_frame_is_fully_dirty() {
        let mut det = DeltaDetector::new(QuadTreeConfig::default());
        let dirty = feed(&mut det, solid_frame(128, 128, [10, 20, 30, 255]));
        assert_eq!(dirty, vec![Rect::new(0, 0, 128, 128)]);
    }

    #[test]
    fn partial_change_subdivides() {
        let config = QuadTreeConfig { min_tile_px: 64, dirty_threshold: 0.5, pixel_noise_floor: 0 };
        let mut det = DeltaDetector::new(config);

        feed(&mut det, solid_frame(256, 256, [0, 0, 0, 255]));

        let mut f2_data = vec![0u8; 256 * 256 * 4];
        for y in 0..128u32 {
            for x in 0..128u32 {
                let i = (y * 256 + x) as usize * 4;
                f2_data[i]     = 255;
                f2_data[i + 1] = 255;
                f2_data[i + 2] = 255;
                f2_data[i + 3] = 255;
            }
        }
        let dirty = feed(&mut det, Frame::new(256, 256, f2_data));

        assert!(dirty.iter().any(|r| r.contains(0, 0)));
        assert!(!dirty.iter().any(|r| r.contains(200, 200)));
    }
}
