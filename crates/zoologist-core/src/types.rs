//! Domain types shared by the whole pipeline (plan §3.1).

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Stable camera id from the config, e.g. `"driveway"`. Matches `^[a-z0-9_-]+$`.
pub type CameraId = String;

/// High-level category shown in the UI and charts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Label {
    Person,
    Vehicle,
    Animal,
    Motion,
}

impl Label {
    /// Every label, in display order.
    pub const ALL: [Label; 4] = [Label::Person, Label::Vehicle, Label::Animal, Label::Motion];

    /// The lower-case name used in config, the API and storage.
    pub fn as_str(self) -> &'static str {
        match self {
            Label::Person => "person",
            Label::Vehicle => "vehicle",
            Label::Animal => "animal",
            Label::Motion => "motion",
        }
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Label {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Label::ALL
            .into_iter()
            .find(|label| label.as_str() == s)
            .ok_or_else(|| {
                format!("unknown label {s:?} (expected person, vehicle, animal or motion)")
            })
    }
}

/// Axis-aligned box in normalised coordinates (0.0..=1.0) of the detect frame.
/// `x1 <= x2` and `y1 <= y2` for any box built by this crate.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BBox {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl BBox {
    /// Builds a box, swapping coordinates if they are given in the wrong order.
    pub fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        Self {
            x1: x1.min(x2),
            y1: y1.min(y2),
            x2: x1.max(x2),
            y2: y1.max(y2),
        }
    }

    /// Width.
    pub fn width(&self) -> f32 {
        (self.x2 - self.x1).max(0.0)
    }

    /// Height.
    pub fn height(&self) -> f32 {
        (self.y2 - self.y1).max(0.0)
    }

    /// Area (width × height), 0 for degenerate boxes.
    pub fn area(&self) -> f32 {
        self.width() * self.height()
    }

    /// Centre point `(x, y)`.
    pub fn center(&self) -> (f32, f32) {
        ((self.x1 + self.x2) / 2.0, (self.y1 + self.y2) / 2.0)
    }

    /// Intersection over union: 1.0 for identical boxes, 0.0 for disjoint ones.
    pub fn iou(&self, other: &BBox) -> f32 {
        let ix = (self.x2.min(other.x2) - self.x1.max(other.x1)).max(0.0);
        let iy = (self.y2.min(other.y2) - self.y1.max(other.y1)).max(0.0);
        let intersection = ix * iy;
        let union = self.area() + other.area() - intersection;
        if union <= 0.0 {
            0.0
        } else {
            intersection / union
        }
    }

    /// Smallest box containing both boxes.
    pub fn union(&self, other: &BBox) -> BBox {
        BBox {
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
            x2: self.x2.max(other.x2),
            y2: self.y2.max(other.y2),
        }
    }

    /// Grows the box by `frac` of its width and height in total (half on each side),
    /// e.g. `expand(0.2)` makes it 20 % wider and taller around the same centre.
    pub fn expand(&self, frac: f32) -> BBox {
        let dx = self.width() * frac / 2.0;
        let dy = self.height() * frac / 2.0;
        BBox {
            x1: self.x1 - dx,
            y1: self.y1 - dy,
            x2: self.x2 + dx,
            y2: self.y2 + dy,
        }
    }

    /// Clamps every coordinate into `0.0..=1.0`.
    pub fn clamp(&self) -> BBox {
        BBox {
            x1: self.x1.clamp(0.0, 1.0),
            y1: self.y1.clamp(0.0, 1.0),
            x2: self.x2.clamp(0.0, 1.0),
            y2: self.y2.clamp(0.0, 1.0),
        }
    }

    /// Pixel rectangle `(x, y, width, height)` in a `w × h` frame, clamped to the frame and at
    /// least 1×1 pixel.
    pub fn to_pixels(&self, w: u32, h: u32) -> (u32, u32, u32, u32) {
        let b = self.clamp();
        let x1 = ((b.x1 * w as f32).floor() as u32).min(w.saturating_sub(1));
        let y1 = ((b.y1 * h as f32).floor() as u32).min(h.saturating_sub(1));
        let x2 = ((b.x2 * w as f32).ceil() as u32).clamp(x1 + 1, w.max(1));
        let y2 = ((b.y2 * h as f32).ceil() as u32).clamp(y1 + 1, h.max(1));
        (x1, y1, x2 - x1, y2 - y1)
    }
}

/// One decoded frame in I420 layout: the Y plane (`width × height` bytes), then the U and V
/// planes (`width/2 × height/2` bytes each). Width and height are even.
#[derive(Clone)]
pub struct Frame {
    pub camera_id: CameraId,
    /// Increments per frame, per camera.
    pub seq: u64,
    /// Wall clock when the source received the access unit (file sources: file start + pts).
    pub captured_at: DateTime<Utc>,
    pub width: u32,
    pub height: u32,
    /// `width * height * 3 / 2` bytes.
    pub i420: Arc<Vec<u8>>,
}

impl Frame {
    /// Expected length of the I420 buffer for a `width × height` frame.
    pub fn i420_len(width: u32, height: u32) -> usize {
        (width as usize * height as usize) * 3 / 2
    }

    /// The luma (brightness) plane, `width × height` bytes.
    pub fn y_plane(&self) -> &[u8] {
        &self.i420[..self.width as usize * self.height as usize]
    }

    /// The U (Cb) plane, `width/2 × height/2` bytes.
    pub fn u_plane(&self) -> &[u8] {
        let y = self.width as usize * self.height as usize;
        &self.i420[y..y + y / 4]
    }

    /// The V (Cr) plane, `width/2 × height/2` bytes.
    pub fn v_plane(&self) -> &[u8] {
        let y = self.width as usize * self.height as usize;
        &self.i420[y + y / 4..y + y / 2]
    }
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frame")
            .field("camera_id", &self.camera_id)
            .field("seq", &self.seq)
            .field("captured_at", &self.captured_at)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// One detector output on one frame.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Detection {
    /// Never [`Label::Motion`].
    pub label: Label,
    /// The model's own class name, e.g. `"dog"`, `"car"`, `"animal"`.
    pub raw_class: String,
    pub score: f32,
    pub bbox: BBox,
}

/// Species result for an animal event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpeciesGuess {
    pub scientific_name: String,
    pub common_name: String,
    /// 0..1 after voting across crops.
    pub score: f32,
    /// e.g. `"speciesnet"` or `"bioclip"`.
    pub model_id: String,
    /// Top-5 common names with scores.
    pub candidates: Vec<(String, f32)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn iou_identical_disjoint_and_half_overlap() {
        let a = BBox::new(0.0, 0.0, 0.5, 0.5);
        assert!(close(a.iou(&a), 1.0));
        assert!(close(a.iou(&BBox::new(0.6, 0.6, 0.9, 0.9)), 0.0));
        // Shifted by half its width: intersection 1/2, union 3/2 → 1/3.
        let b = BBox::new(0.25, 0.0, 0.75, 0.5);
        assert!(close(a.iou(&b), 1.0 / 3.0));
    }

    #[test]
    fn iou_with_degenerate_box_is_zero() {
        let a = BBox::new(0.1, 0.1, 0.1, 0.1);
        assert_eq!(a.iou(&a), 0.0);
    }

    #[test]
    fn new_orders_coordinates() {
        let b = BBox::new(0.8, 0.9, 0.2, 0.1);
        assert_eq!(
            b,
            BBox {
                x1: 0.2,
                y1: 0.1,
                x2: 0.8,
                y2: 0.9
            }
        );
    }

    #[test]
    fn expand_keeps_centre() {
        let b = BBox::new(0.4, 0.4, 0.6, 0.6).expand(0.5);
        assert!(close(b.x1, 0.35) && close(b.x2, 0.65));
        assert_eq!(b.center(), (0.5, 0.5));
    }

    #[test]
    fn to_pixels_clamps_and_is_never_empty() {
        assert_eq!(
            BBox::new(0.0, 0.0, 0.5, 0.5).to_pixels(640, 360),
            (0, 0, 320, 180)
        );
        assert_eq!(
            BBox::new(-0.2, 0.75, 1.3, 1.5).to_pixels(640, 360),
            (0, 270, 640, 90)
        );
        let (_, _, w, h) = BBox::new(0.5, 0.5, 0.5, 0.5).to_pixels(640, 360);
        assert_eq!((w, h), (1, 1));
    }

    #[test]
    fn label_round_trips_through_text_and_json() {
        for label in Label::ALL {
            assert_eq!(label.as_str().parse::<Label>().unwrap(), label);
            let json = serde_json::to_string(&label).unwrap();
            assert_eq!(json, format!("\"{}\"", label.as_str()));
        }
        assert!("cat".parse::<Label>().is_err());
    }

    #[test]
    fn frame_planes_have_the_right_sizes() {
        let frame = Frame {
            camera_id: "cam".into(),
            seq: 0,
            captured_at: Utc::now(),
            width: 4,
            height: 2,
            i420: Arc::new((0..12).collect()),
        };
        assert_eq!(frame.y_plane(), &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(frame.u_plane(), &[8, 9]);
        assert_eq!(frame.v_plane(), &[10, 11]);
    }
}
