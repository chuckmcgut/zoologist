//! Motion detection on the luma (Y) plane (plan Step 3.1).
//!
//! A running-average background is kept at a small analysis size. Pixels that differ from it by
//! more than a threshold are "changed"; connected groups of changed pixels become motion boxes.
//! Only frames with motion are sent to the object detector, which is what keeps CPU use low.

use zoologist_core::config::MotionConfig;
use zoologist_core::yuv::downscale_y;
use zoologist_core::{BBox, Frame};

/// Frames after (re)start during which no motion is reported while the background settles.
const WARM_UP_FRAMES: u32 = 10;
/// Background values are stored multiplied by this, so slow learning rates still move them.
const FIXED: i32 = 16;

/// Finds moving areas in a camera's frames. One instance per camera.
pub struct MotionDetector {
    cfg: MotionConfig,
    /// Camera masks: polygons in normalised coordinates, rasterised on (re)initialisation.
    mask_polygons: Vec<Vec<f32>>,
    frame_w: u32,
    frame_h: u32,
    /// Analysis size (downscaled Y plane).
    w: usize,
    h: usize,
    /// Background × [`FIXED`]. Empty until the first frame.
    bg: Vec<i32>,
    /// `true` for pixels inside a motion mask (ignored).
    masked: Vec<bool>,
    frames_seen: u32,
    last_changed_fraction: f32,
    // Scratch buffers reused between frames.
    blurred: Vec<u8>,
    changed: Vec<bool>,
    dilated: Vec<bool>,
    labels: Vec<u32>,
    parent: Vec<u32>,
}

impl MotionDetector {
    /// Creates a detector for frames of `frame_w × frame_h`. `masks` are the camera's
    /// `motion_mask` polygons (flat `[x1, y1, x2, y2, …]` lists, normalised).
    pub fn new(cfg: &MotionConfig, frame_w: u32, frame_h: u32, masks: &[Vec<f32>]) -> Self {
        let mut detector = MotionDetector {
            cfg: cfg.clone(),
            mask_polygons: masks.to_vec(),
            frame_w: 0,
            frame_h: 0,
            w: 0,
            h: 0,
            bg: Vec::new(),
            masked: Vec::new(),
            frames_seen: 0,
            last_changed_fraction: 0.0,
            blurred: Vec::new(),
            changed: Vec::new(),
            dilated: Vec::new(),
            labels: Vec::new(),
            parent: Vec::new(),
        };
        detector.resize(frame_w, frame_h);
        detector
    }

    /// Fraction of analysed pixels that changed in the last processed frame (for statistics).
    pub fn last_changed_fraction(&self) -> f32 {
        self.last_changed_fraction
    }

    /// Returns the motion boxes (normalised) for this frame; empty means no motion.
    pub fn process(&mut self, frame: &Frame) -> Vec<BBox> {
        if frame.width != self.frame_w || frame.height != self.frame_h {
            self.resize(frame.width, frame.height);
        }
        let Ok((gray, w, h)) = downscale_y(
            frame.y_plane(),
            frame.width,
            frame.height,
            self.cfg.analysis_width,
        ) else {
            return Vec::new();
        };
        debug_assert_eq!((w as usize, h as usize), (self.w, self.h));
        box_blur_3x3(&gray, self.w, self.h, &mut self.blurred);

        self.frames_seen = self.frames_seen.saturating_add(1);
        if self.bg.is_empty() {
            self.reset_background();
            return Vec::new();
        }

        // 1. Changed pixels (outside masks).
        let threshold = self.cfg.threshold as i32 * FIXED;
        let mut changed_count = 0usize;
        for i in 0..self.blurred.len() {
            let diff = (self.blurred[i] as i32 * FIXED - self.bg[i]).abs();
            let changed = !self.masked[i] && diff > threshold;
            self.changed[i] = changed;
            changed_count += changed as usize;
        }
        let pixels = self.blurred.len();
        self.last_changed_fraction = changed_count as f32 / pixels as f32;

        // 2. Whole-scene change (IR switch, lightning, camera exposure jump): start over.
        if self.last_changed_fraction > self.cfg.lightning_fraction {
            self.reset_background();
            self.frames_seen = WARM_UP_FRAMES;
            return Vec::new();
        }

        // 3. Grow changed areas slightly so a moving object is one component, then group them.
        dilate_3x3(&self.changed, self.w, self.h, &mut self.dilated);
        let boxes = if self.frames_seen <= WARM_UP_FRAMES {
            Vec::new()
        } else {
            let min_pixels = (self.cfg.contour_area * pixels as f32).max(1.0) as u32;
            self.components(min_pixels)
        };

        // 4. Learn the background, slowly where there is motion so stopped objects fade in.
        let alpha = ((self.cfg.frame_alpha * 256.0).round() as i32).clamp(1, 256);
        let alpha_motion = (alpha / 4).max(1);
        for i in 0..pixels {
            let a = if self.dilated[i] { alpha_motion } else { alpha };
            let target = self.blurred[i] as i32 * FIXED;
            self.bg[i] += ((target - self.bg[i]) * a) >> 8;
        }
        boxes
    }

    /// (Re)initialises every buffer for a new frame size.
    fn resize(&mut self, frame_w: u32, frame_h: u32) {
        self.frame_w = frame_w;
        self.frame_h = frame_h;
        let out_w = self.cfg.analysis_width.min(frame_w).max(1);
        let out_h = ((frame_h as u64 * out_w as u64 + frame_w as u64 / 2) / frame_w.max(1) as u64)
            .max(1) as u32;
        self.w = out_w as usize;
        self.h = out_h as usize;
        let n = self.w * self.h;
        self.bg.clear();
        self.masked = rasterise_masks(&self.mask_polygons, self.w, self.h);
        self.changed = vec![false; n];
        self.dilated = vec![false; n];
        self.labels = vec![0; n];
        self.frames_seen = 0;
    }

    fn reset_background(&mut self) {
        self.bg.clear();
        self.bg
            .extend(self.blurred.iter().map(|&p| p as i32 * FIXED));
    }

    /// Connected components (8-connectivity) of `self.dilated` with at least `min_pixels`
    /// pixels, as normalised boxes. Two-pass labelling with union-find.
    fn components(&mut self, min_pixels: u32) -> Vec<BBox> {
        let (w, h) = (self.w, self.h);
        self.parent.clear();
        self.parent.push(0); // label 0 = background
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                if !self.dilated[i] {
                    self.labels[i] = 0;
                    continue;
                }
                // Already-visited neighbours: W, NW, N, NE.
                let mut neighbours = [0u32; 4];
                if x > 0 {
                    neighbours[0] = self.labels[i - 1];
                }
                if y > 0 {
                    let up = i - w;
                    if x > 0 {
                        neighbours[1] = self.labels[up - 1];
                    }
                    neighbours[2] = self.labels[up];
                    if x + 1 < w {
                        neighbours[3] = self.labels[up + 1];
                    }
                }
                let smallest = neighbours.iter().copied().filter(|&l| l != 0).min();
                let label = match smallest {
                    Some(l) => {
                        for &n in neighbours.iter().filter(|&&n| n != 0) {
                            union(&mut self.parent, l, n);
                        }
                        l
                    }
                    None => {
                        let new = self.parent.len() as u32;
                        self.parent.push(new);
                        new
                    }
                };
                self.labels[i] = label;
            }
        }

        // Second pass: accumulate pixel counts and bounds per root label.
        struct Blob {
            count: u32,
            x1: usize,
            y1: usize,
            x2: usize,
            y2: usize,
        }
        let mut blobs: Vec<Option<Blob>> = (0..self.parent.len()).map(|_| None).collect();
        for y in 0..h {
            for x in 0..w {
                let label = self.labels[y * w + x];
                if label == 0 {
                    continue;
                }
                let root = find(&mut self.parent, label) as usize;
                let blob = blobs[root].get_or_insert(Blob {
                    count: 0,
                    x1: x,
                    y1: y,
                    x2: x,
                    y2: y,
                });
                blob.count += 1;
                blob.x1 = blob.x1.min(x);
                blob.y1 = blob.y1.min(y);
                blob.x2 = blob.x2.max(x);
                blob.y2 = blob.y2.max(y);
            }
        }
        let (fw, fh) = (w as f32, h as f32);
        let mut boxes: Vec<(u32, BBox)> = blobs
            .into_iter()
            .flatten()
            .filter(|b| b.count >= min_pixels)
            .map(|b| {
                let bbox = BBox::new(
                    b.x1 as f32 / fw,
                    b.y1 as f32 / fh,
                    (b.x2 + 1) as f32 / fw,
                    (b.y2 + 1) as f32 / fh,
                );
                (b.count, bbox)
            })
            .collect();
        boxes.sort_by_key(|(count, _)| std::cmp::Reverse(*count));
        boxes.into_iter().map(|(_, bbox)| bbox).collect()
    }
}

fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        let grandparent = parent[parent[x as usize] as usize];
        parent[x as usize] = grandparent; // path halving
        x = grandparent;
    }
    x
}

fn union(parent: &mut [u32], a: u32, b: u32) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        let (small, large) = if ra < rb { (ra, rb) } else { (rb, ra) };
        parent[large as usize] = small;
    }
}

/// 3×3 mean filter with edge pixels clamped, written into `out`.
fn box_blur_3x3(src: &[u8], w: usize, h: usize, out: &mut Vec<u8>) {
    // Horizontal pass into u16 sums, then vertical pass.
    let mut rows = vec![0u16; w * h];
    for y in 0..h {
        let row = &src[y * w..][..w];
        for x in 0..w {
            let l = row[x.saturating_sub(1)] as u16;
            let r = row[(x + 1).min(w - 1)] as u16;
            rows[y * w + x] = l + row[x] as u16 + r;
        }
    }
    out.resize(w * h, 0);
    for y in 0..h {
        let up = &rows[y.saturating_sub(1) * w..][..w];
        let mid = &rows[y * w..][..w];
        let down = &rows[(y + 1).min(h - 1) * w..][..w];
        for x in 0..w {
            out[y * w + x] = ((up[x] + mid[x] + down[x] + 4) / 9) as u8;
        }
    }
}

/// 3×3 dilation of a binary image, written into `out`.
fn dilate_3x3(src: &[bool], w: usize, h: usize, out: &mut Vec<bool>) {
    out.clear();
    out.resize(w * h, false);
    for y in 0..h {
        for x in 0..w {
            if !src[y * w + x] {
                continue;
            }
            for ny in y.saturating_sub(1)..=(y + 1).min(h - 1) {
                for nx in x.saturating_sub(1)..=(x + 1).min(w - 1) {
                    out[ny * w + nx] = true;
                }
            }
        }
    }
}

/// `true` for every pixel whose centre lies inside any polygon (even-odd rule).
fn rasterise_masks(polygons: &[Vec<f32>], w: usize, h: usize) -> Vec<bool> {
    let mut masked = vec![false; w * h];
    for polygon in polygons {
        let points: Vec<(f32, f32)> = polygon
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| (p[0] * w as f32, p[1] * h as f32))
            .collect();
        if points.len() < 3 {
            continue;
        }
        for y in 0..h {
            let py = y as f32 + 0.5;
            for x in 0..w {
                let px = x as f32 + 0.5;
                if point_in_polygon(px, py, &points) {
                    masked[y * w + x] = true;
                }
            }
        }
    }
    masked
}

fn point_in_polygon(px: f32, py: f32, points: &[(f32, f32)]) -> bool {
    let mut inside = false;
    let mut j = points.len() - 1;
    for i in 0..points.len() {
        let (xi, yi) = points[i];
        let (xj, yj) = points[j];
        if (yi > py) != (yj > py) && px < (xj - xi) * (py - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use chrono::Utc;
    use zoologist_core::yuv::rgb_to_i420;

    use super::*;

    const W: u32 = 640;
    const H: u32 = 360;

    /// A grey frame with an optional white square at `(x, y)` of side `size`.
    fn frame(seq: u64, background: u8, square: Option<(u32, u32, u32)>) -> Frame {
        let mut rgb = vec![background; (W * H * 3) as usize];
        if let Some((sx, sy, size)) = square {
            for y in sy..(sy + size).min(H) {
                for x in sx..(sx + size).min(W) {
                    let p = ((y * W + x) * 3) as usize;
                    rgb[p..p + 3].copy_from_slice(&[255, 255, 255]);
                }
            }
        }
        Frame {
            camera_id: "test".into(),
            seq,
            captured_at: Utc::now(),
            width: W,
            height: H,
            i420: Arc::new(rgb_to_i420(&rgb, W, H).unwrap()),
        }
    }

    fn detector(masks: &[Vec<f32>]) -> MotionDetector {
        MotionDetector::new(&MotionConfig::default(), W, H, masks)
    }

    #[test]
    fn static_scene_has_no_motion() {
        let mut d = detector(&[]);
        let f = frame(0, 60, Some((100, 100, 50)));
        for _ in 0..40 {
            assert!(d.process(&f).is_empty());
        }
    }

    #[test]
    fn moving_square_is_found_and_tracked_left_to_right() {
        let mut d = detector(&[]);
        for i in 0..12 {
            d.process(&frame(i, 20, None));
        }
        let mut hits = 0;
        let mut centres = Vec::new();
        let steps = 40u32;
        for i in 0..steps {
            let x = 40 + i * 12;
            let boxes = d.process(&frame(100 + i as u64, 20, Some((x, 160, 40))));
            if let Some(first) = boxes.first() {
                hits += 1;
                centres.push(first.center().0);
                // The box must cover the square's current position.
                let sx = (x as f32 + 20.0) / W as f32;
                assert!(first.x1 <= sx && sx <= first.x2, "{first:?} misses x={sx}");
            }
        }
        assert!(
            hits as f32 >= 0.9 * steps as f32,
            "motion on only {hits}/{steps} frames"
        );
        assert!(centres.last().unwrap() > centres.first().unwrap());
    }

    #[test]
    fn brightness_jump_resets_instead_of_reporting_motion() {
        let mut d = detector(&[]);
        for i in 0..15 {
            d.process(&frame(i, 40, None));
        }
        // IR switch: the whole image changes at once.
        assert!(d.process(&frame(20, 200, None)).is_empty());
        assert!(d.process(&frame(21, 200, None)).is_empty());
        assert!(d.process(&frame(22, 200, None)).is_empty());
    }

    #[test]
    fn full_mask_never_reports_motion() {
        let full = vec![vec![0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0]];
        let mut d = detector(&full);
        for i in 0..15 {
            d.process(&frame(i, 20, None));
        }
        for i in 0..20u32 {
            let square = Some((40 + i * 10, 100, 60));
            assert!(d.process(&frame(100 + i as u64, 20, square)).is_empty());
        }
    }

    #[test]
    fn partial_mask_hides_only_its_area() {
        // Mask the left half.
        let left = vec![vec![0.0, 0.0, 0.5, 0.0, 0.5, 1.0, 0.0, 1.0]];
        let mut d = detector(&left);
        for i in 0..15 {
            d.process(&frame(i, 20, None));
        }
        assert!(d.process(&frame(20, 20, Some((60, 100, 60)))).is_empty());
        d.process(&frame(21, 20, None));
        let boxes = d.process(&frame(22, 20, Some((460, 100, 60))));
        assert_eq!(boxes.len(), 1, "{boxes:?}");
    }

    #[test]
    fn tiny_changes_are_ignored() {
        let mut d = detector(&[]);
        for i in 0..15 {
            d.process(&frame(i, 20, None));
        }
        // 4×4 pixels at full resolution is far below contour_area.
        assert!(d.process(&frame(20, 20, Some((300, 200, 4)))).is_empty());
    }

    #[test]
    fn two_separate_objects_give_two_boxes() {
        let mut d = detector(&[]);
        for i in 0..15 {
            d.process(&frame(i, 20, None));
        }
        let mut rgb_frame = frame(20, 20, Some((50, 50, 60)));
        // Paint a second square far away by merging two frames' Y planes.
        let second = frame(20, 20, Some((500, 250, 60)));
        let mut data = (*rgb_frame.i420).clone();
        let y_len = (W * H) as usize;
        for (px, other) in data[..y_len].iter_mut().zip(second.i420.iter()) {
            *px = (*px).max(*other);
        }
        rgb_frame.i420 = Arc::new(data);
        assert_eq!(d.process(&rgb_frame).len(), 2);
    }

    #[test]
    fn resolution_change_reinitialises() {
        let mut d = detector(&[]);
        d.process(&frame(0, 20, None));
        let small = Frame {
            width: 320,
            height: 180,
            i420: Arc::new(vec![20; Frame::i420_len(320, 180)]),
            ..frame(1, 20, None)
        };
        assert!(d.process(&small).is_empty());
    }

    #[test]
    fn point_in_polygon_basics() {
        let square = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        assert!(point_in_polygon(5.0, 5.0, &square));
        assert!(!point_in_polygon(15.0, 5.0, &square));
    }

    /// Timing only; run with `cargo test --release -p zoologist-vision -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn motion_benchmark() {
        let mut d = detector(&[]);
        let frames: Vec<Frame> = (0..20u32)
            .map(|i| frame(i as u64, 20, Some((40 + i * 12, 160, 40))))
            .collect();
        for f in &frames {
            d.process(f);
        }
        let n = 500;
        let start = Instant::now();
        for i in 0..n {
            d.process(&frames[i % frames.len()]);
        }
        println!(
            "motion on 640×360: {:?} per frame",
            start.elapsed() / n as u32
        );
    }
}
