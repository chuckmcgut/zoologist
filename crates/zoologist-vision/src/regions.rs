//! Choosing which parts of a frame go to the object detector (plan Step 4.1).
//!
//! Running the detector on a square crop around the motion, instead of the whole frame, keeps
//! small or distant animals large enough for a 320×320 model (Frigate's approach).

use zoologist_core::BBox;
use zoologist_core::yuv::PixelRect;

/// Motion boxes closer than this (as a fraction of their size) are treated as one object.
const CLUSTER_EXPAND: f32 = 0.2;
/// Padding around a cluster before it becomes a square region.
const REGION_PADDING: f32 = 0.2;
/// Regions overlapping more than this fraction of the smaller one are merged.
const MERGE_OVERLAP: f32 = 0.5;

/// Picks up to `max` detector regions for a `frame_w × frame_h` frame.
///
/// - `motion`: motion boxes from the motion detector (normalised).
/// - `keepalive`: boxes of active tracks that are due for a detection even without motion.
///
/// Each region is a square of at least `model_size` pixels (never larger than the frame's
/// shorter side), centred on the object and shifted inside the frame. When an object is too
/// big for any square, the region is the whole frame (not square); the detector letterboxes it.
/// Motion regions come first, largest first; keep-alive regions follow.
pub fn select_regions(
    motion: &[BBox],
    keepalive: &[BBox],
    frame_w: u32,
    frame_h: u32,
    model_size: u32,
    max: usize,
) -> Vec<PixelRect> {
    if frame_w == 0 || frame_h == 0 || max == 0 {
        return Vec::new();
    }
    let mut clusters = cluster(motion);
    clusters.sort_by(|a, b| b.area().total_cmp(&a.area()));
    let mut regions: Vec<PixelRect> = clusters
        .iter()
        .chain(keepalive)
        .map(|b| square_around(b, frame_w, frame_h, model_size))
        .collect();
    merge_overlapping(&mut regions, frame_w, frame_h, model_size);
    regions.truncate(max);
    regions
}

/// Covers the whole frame with overlapping squares as tall as its shorter side (a 1536×432
/// panorama gives four 432×432 tiles). Used when there is no motion to go by, e.g. at the start
/// of a battery camera's clip, where the animal is already in view.
pub fn tile_regions(frame_w: u32, frame_h: u32) -> Vec<PixelRect> {
    if frame_w == 0 || frame_h == 0 {
        return Vec::new();
    }
    let side = frame_w.min(frame_h);
    let long = frame_w.max(frame_h);
    let n = long.div_ceil(side).max(1);
    (0..n)
        .map(|i| {
            let pos = if n == 1 {
                0
            } else {
                (long - side) * i / (n - 1)
            };
            if frame_w >= frame_h {
                PixelRect {
                    x: pos,
                    y: 0,
                    w: side,
                    h: side,
                }
            } else {
                PixelRect {
                    x: 0,
                    y: pos,
                    w: side,
                    h: side,
                }
            }
        })
        .collect()
}

/// Groups boxes whose slightly expanded versions touch, returning each group's bounding box.
fn cluster(boxes: &[BBox]) -> Vec<BBox> {
    let mut groups: Vec<BBox> = Vec::new();
    for b in boxes {
        let mut merged = *b;
        // Absorb every existing group that touches this box; repeat until nothing changes.
        loop {
            let grown = merged.expand(CLUSTER_EXPAND);
            let before = groups.len();
            groups.retain(|g| {
                if touches(&grown, &g.expand(CLUSTER_EXPAND)) {
                    merged = merged.union(g);
                    false
                } else {
                    true
                }
            });
            if groups.len() == before {
                break;
            }
        }
        groups.push(merged);
    }
    groups
}

fn touches(a: &BBox, b: &BBox) -> bool {
    a.x1 <= b.x2 && b.x1 <= a.x2 && a.y1 <= b.y2 && b.y1 <= a.y2
}

/// A square region around `b` (normalised), in pixels.
fn square_around(b: &BBox, frame_w: u32, frame_h: u32, model_size: u32) -> PixelRect {
    let (fw, fh) = (frame_w as f32, frame_h as f32);
    let short_side = frame_w.min(frame_h);
    let w = b.width() * fw * (1.0 + REGION_PADDING);
    let h = b.height() * fh * (1.0 + REGION_PADDING);
    let wanted = w.max(h).ceil() as u32;
    if wanted > short_side {
        return full_frame(frame_w, frame_h);
    }
    let side = wanted.max(model_size).min(short_side);
    let (cx, cy) = b.center();
    let x = (cx * fw - side as f32 / 2.0).clamp(0.0, (frame_w - side) as f32) as u32;
    let y = (cy * fh - side as f32 / 2.0).clamp(0.0, (frame_h - side) as f32) as u32;
    PixelRect {
        x,
        y,
        w: side,
        h: side,
    }
}

fn full_frame(frame_w: u32, frame_h: u32) -> PixelRect {
    PixelRect {
        x: 0,
        y: 0,
        w: frame_w,
        h: frame_h,
    }
}

/// Replaces pairs of regions that mostly overlap with one region covering both.
fn merge_overlapping(regions: &mut Vec<PixelRect>, frame_w: u32, frame_h: u32, model_size: u32) {
    let mut i = 0;
    while i < regions.len() {
        let mut j = i + 1;
        let mut merged = false;
        while j < regions.len() {
            let (a, b) = (regions[i], regions[j]);
            let smaller = (a.w * a.h).min(b.w * b.h) as f32;
            if overlap_area(&a, &b) as f32 > MERGE_OVERLAP * smaller {
                let union = to_bbox(&a, frame_w, frame_h).union(&to_bbox(&b, frame_w, frame_h));
                // The union already includes padding, so do not pad again.
                let unpadded = union.expand(-REGION_PADDING / (1.0 + REGION_PADDING));
                regions[i] = square_around(&unpadded, frame_w, frame_h, model_size);
                regions.remove(j);
                merged = true;
            } else {
                j += 1;
            }
        }
        if !merged {
            i += 1;
        }
    }
}

fn overlap_area(a: &PixelRect, b: &PixelRect) -> u32 {
    let w = (a.x + a.w).min(b.x + b.w).saturating_sub(a.x.max(b.x));
    let h = (a.y + a.h).min(b.y + b.h).saturating_sub(a.y.max(b.y));
    w * h
}

fn to_bbox(r: &PixelRect, frame_w: u32, frame_h: u32) -> BBox {
    let (fw, fh) = (frame_w as f32, frame_h as f32);
    BBox::new(
        r.x as f32 / fw,
        r.y as f32 / fh,
        (r.x + r.w) as f32 / fw,
        (r.y + r.h) as f32 / fh,
    )
}

/// Maps a box that is normalised to `region` back to normalised full-frame coordinates.
pub fn region_to_frame(b: &BBox, region: &PixelRect, frame_w: u32, frame_h: u32) -> BBox {
    let (fw, fh) = (frame_w as f32, frame_h as f32);
    let map_x = |x: f32| (region.x as f32 + x * region.w as f32) / fw;
    let map_y = |y: f32| (region.y as f32 + y * region.h as f32) / fh;
    BBox::new(map_x(b.x1), map_y(b.y1), map_x(b.x2), map_y(b.y2)).clamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 640;
    const H: u32 = 360;

    fn px(x1: f32, y1: f32, x2: f32, y2: f32) -> BBox {
        BBox::new(x1 / W as f32, y1 / H as f32, x2 / W as f32, y2 / H as f32)
    }

    fn contains(r: &PixelRect, b: &BBox) -> bool {
        let (x1, y1) = (b.x1 * W as f32, b.y1 * H as f32);
        let (x2, y2) = (b.x2 * W as f32, b.y2 * H as f32);
        r.x as f32 <= x1 && r.y as f32 <= y1 && (r.x + r.w) as f32 >= x2 && (r.y + r.h) as f32 >= y2
    }

    #[test]
    fn small_box_gets_a_model_sized_square_around_it() {
        let b = px(300.0, 150.0, 340.0, 190.0);
        let regions = select_regions(&[b], &[], W, H, 320, 2);
        assert_eq!(regions.len(), 1);
        let r = regions[0];
        assert_eq!((r.w, r.h), (320, 320));
        assert!(contains(&r, &b));
        assert!(r.x + r.w <= W && r.y + r.h <= H);
    }

    #[test]
    fn two_far_apart_boxes_give_two_regions() {
        let left = px(10.0, 10.0, 40.0, 40.0);
        let right = px(600.0, 320.0, 630.0, 350.0);
        let regions = select_regions(&[left, right], &[], W, H, 256, 4);
        assert_eq!(regions.len(), 2);
        assert!(regions.iter().any(|r| contains(r, &left)));
        assert!(regions.iter().any(|r| contains(r, &right)));
    }

    #[test]
    fn nearby_boxes_are_one_cluster() {
        // Two parts of one moving animal, a few pixels apart.
        let a = px(200.0, 100.0, 240.0, 140.0);
        let b = px(244.0, 104.0, 280.0, 150.0);
        let regions = select_regions(&[a, b], &[], W, H, 320, 4);
        assert_eq!(regions.len(), 1);
        assert!(contains(&regions[0], &a) && contains(&regions[0], &b));
    }

    #[test]
    fn edge_box_is_shifted_inside_the_frame() {
        let b = px(620.0, 340.0, 640.0, 360.0);
        let r = select_regions(&[b], &[], W, H, 320, 1)[0];
        assert_eq!((r.x, r.y, r.w, r.h), (320, 40, 320, 320));
    }

    #[test]
    fn huge_object_uses_the_whole_frame() {
        let b = px(20.0, 30.0, 600.0, 330.0);
        let r = select_regions(&[b], &[], W, H, 320, 1)[0];
        assert_eq!((r.x, r.y, r.w, r.h), (0, 0, W, H));
    }

    #[test]
    fn medium_object_gets_a_bigger_square() {
        let b = px(100.0, 50.0, 350.0, 300.0); // 250 px, +20 % = 300
        let r = select_regions(&[b], &[], W, H, 256, 1)[0];
        assert_eq!((r.w, r.h), (300, 300));
        assert!(contains(&r, &b));
    }

    #[test]
    fn overlapping_regions_merge_and_max_is_respected() {
        let a = px(100.0, 100.0, 120.0, 120.0);
        let b = px(160.0, 110.0, 180.0, 130.0); // separate clusters, but their squares overlap
        let regions = select_regions(&[a, b], &[], W, H, 320, 4);
        assert_eq!(regions.len(), 1);
        assert!(contains(&regions[0], &a) && contains(&regions[0], &b));

        let many: Vec<BBox> = (0..6)
            .map(|i| px(i as f32 * 100.0, 10.0, i as f32 * 100.0 + 10.0, 20.0))
            .collect();
        assert!(select_regions(&many, &[], W, H, 64, 3).len() <= 3);
    }

    #[test]
    fn keepalive_regions_come_after_motion() {
        let motion = px(10.0, 10.0, 30.0, 30.0);
        let track = px(560.0, 280.0, 600.0, 330.0);
        let regions = select_regions(&[motion], &[track], W, H, 128, 4);
        assert_eq!(regions.len(), 2);
        assert!(contains(&regions[0], &motion));
        assert!(contains(&regions[1], &track));
        assert!(select_regions(&[], &[], W, H, 320, 4).is_empty());
    }

    #[test]
    fn region_mapping_round_trip() {
        let region = PixelRect {
            x: 100,
            y: 20,
            w: 320,
            h: 320,
        };
        let inside = BBox::new(0.25, 0.5, 0.75, 1.0);
        let mapped = region_to_frame(&inside, &region, W, H);
        assert!((mapped.x1 - 180.0 / 640.0).abs() < 1e-6);
        assert!((mapped.y1 - 180.0 / 360.0).abs() < 1e-6);
        assert!((mapped.x2 - 340.0 / 640.0).abs() < 1e-6);
        assert!((mapped.y2 - 340.0 / 360.0).abs() < 1e-6);
    }

    #[test]
    fn tiles_cover_panoramas_with_squares() {
        let tiles = tile_regions(1536, 432);
        assert_eq!(tiles.len(), 4);
        assert!(tiles.iter().all(|t| t.w == 432 && t.h == 432 && t.y == 0));
        assert_eq!(tiles[0].x, 0);
        assert_eq!(tiles[3].x + 432, 1536);
        assert_eq!(tile_regions(640, 640).len(), 1);
        assert_eq!(tile_regions(360, 640).len(), 2);
        assert!(tile_regions(0, 10).is_empty());
    }
}
