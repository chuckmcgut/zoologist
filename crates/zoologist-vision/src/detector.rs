//! Object detection with tract (plan Step 4.2).
//!
//! A [`DetectorModel`] takes a square RGB image of `input_size × input_size` and returns
//! detections (person, vehicle, animal) with boxes normalised to that square. It is `Sync`:
//! one loaded model is shared by every detector worker thread.

use std::path::Path;
use std::sync::Arc;

use tract_onnx::prelude::*;
use zoologist_core::config::{ClassSet, DetectorOutput, ModelConfig};
use zoologist_core::{BBox, Detection, Label};

/// Overlapping boxes of the same class above this IoU are merged by NMS.
pub const NMS_IOU: f32 = 0.45;

/// Errors loading or running a model.
#[derive(Debug, thiserror::Error)]
pub enum DetectorError {
    #[error("cannot load {path}: {source}")]
    Load {
        path: String,
        #[source]
        source: TractError,
    },
    #[error("inference failed: {0}")]
    Run(#[from] TractError),
    #[error("{0}")]
    Invalid(String),
}

type Plan = Arc<TypedRunnableModel>;

/// A loaded object detector.
pub struct DetectorModel {
    plan: Plan,
    input_size: u32,
    kind: DetectorOutput,
    classes: ClassSet,
    threshold: f32,
    id: String,
}

impl DetectorModel {
    /// Loads and optimises the ONNX file for a fixed `1×3×S×S` input.
    pub fn load(cfg: &ModelConfig) -> Result<Self, DetectorError> {
        let s = cfg.input_size as usize;
        let path = cfg.path.display().to_string();
        let plan = tract_onnx::onnx()
            .model_for_path(&cfg.path)
            .and_then(|m| m.with_input_fact(0, f32::fact([1, 3, s, s]).into()))
            .and_then(|m| m.into_optimized())
            .and_then(|m| m.into_runnable())
            .map_err(|source| DetectorError::Load {
                path: path.clone(),
                source,
            })?;
        let id = cfg
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("detector")
            .to_string();
        tracing::info!(model = %id, input = s, "detector loaded");
        Ok(Self {
            plan,
            input_size: cfg.input_size,
            kind: cfg.kind,
            classes: cfg.classes,
            threshold: cfg.score_threshold,
            id,
        })
    }

    /// Model name (the file name without extension).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Side of the square input, in pixels.
    pub fn input_size(&self) -> u32 {
        self.input_size
    }

    /// Detects objects in `rgb`, a packed RGB image of `input_size × input_size`. Boxes are
    /// normalised to that square. Only person, vehicle and animal classes are returned.
    pub fn detect(&self, rgb: &[u8]) -> Result<Vec<Detection>, DetectorError> {
        let s = self.input_size as usize;
        if rgb.len() != s * s * 3 {
            return Err(DetectorError::Invalid(format!(
                "expected a {s}×{s} RGB image ({} bytes), got {} bytes",
                s * s * 3,
                rgb.len()
            )));
        }
        // HWC u8 → NCHW f32 in 0..1.
        let plane = s * s;
        let mut data = vec![0f32; 3 * plane];
        for (i, px) in rgb.as_chunks::<3>().0.iter().enumerate() {
            data[i] = f32::from(px[0]) / 255.0;
            data[plane + i] = f32::from(px[1]) / 255.0;
            data[2 * plane + i] = f32::from(px[2]) / 255.0;
        }
        let input = Tensor::from_shape(&[1, 3, s, s], &data)?;
        let output = self.plan.run(tvec!(input.into()))?;
        let view = output[0].to_plain_array_view::<f32>()?;
        let shape = view.shape().to_vec();
        let values: Vec<f32> = view.iter().copied().collect();
        let raw = decode(
            self.kind,
            &shape,
            &values,
            self.threshold,
            self.input_size as f32,
        )?;
        Ok(nms(raw, NMS_IOU)
            .into_iter()
            .filter_map(|(class, score, bbox)| {
                let (label, raw_class) = map_class(self.classes, class)?;
                Some(Detection {
                    label,
                    raw_class: raw_class.to_string(),
                    score,
                    bbox,
                })
            })
            .collect())
    }
}

/// A candidate box before NMS: (class index, score, normalised box).
type Candidate = (usize, f32, BBox);

/// Decodes a raw output tensor into candidates above `threshold`.
fn decode(
    kind: DetectorOutput,
    shape: &[usize],
    v: &[f32],
    threshold: f32,
    size: f32,
) -> Result<Vec<Candidate>, DetectorError> {
    let bad = || DetectorError::Invalid(format!("unexpected output shape {shape:?} for {kind:?}"));
    let from_center = |cx: f32, cy: f32, w: f32, h: f32| {
        BBox::new(
            (cx - w / 2.0) / size,
            (cy - h / 2.0) / size,
            (cx + w / 2.0) / size,
            (cy + h / 2.0) / size,
        )
        .clamp()
    };
    let mut out = Vec::new();
    match kind {
        // [1, N, 5 + C]: cx, cy, w, h, objectness, class scores.
        DetectorOutput::Yolov5 => {
            let [1, n, stride] = shape else {
                return Err(bad());
            };
            if *stride < 6 {
                return Err(bad());
            }
            for row in v.chunks_exact(*stride).take(*n) {
                let (class, best) = argmax(&row[5..]);
                let score = row[4] * best;
                if score >= threshold {
                    out.push((class, score, from_center(row[0], row[1], row[2], row[3])));
                }
            }
        }
        // [1, 4 + C, N]: rows are attributes, columns are candidates.
        DetectorOutput::Yolov8 => {
            let [1, attrs, n] = shape else {
                return Err(bad());
            };
            if *attrs < 5 {
                return Err(bad());
            }
            let at = |a: usize, i: usize| v[a * n + i];
            for i in 0..*n {
                let (mut class, mut score) = (0, f32::MIN);
                for c in 0..attrs - 4 {
                    let s = at(4 + c, i);
                    if s > score {
                        (class, score) = (c, s);
                    }
                }
                if score >= threshold {
                    out.push((
                        class,
                        score,
                        from_center(at(0, i), at(1, i), at(2, i), at(3, i)),
                    ));
                }
            }
        }
        // [1, N, 6]: x1, y1, x2, y2 (pixels), score, class. Already NMS-ed.
        DetectorOutput::YoloE2e => {
            let [1, _, 6] = shape else { return Err(bad()) };
            for row in v.as_chunks::<6>().0 {
                if row[4] >= threshold {
                    let b = BBox::new(row[0] / size, row[1] / size, row[2] / size, row[3] / size);
                    out.push((row[5].max(0.0) as usize, row[4], b.clamp()));
                }
            }
        }
    }
    Ok(out)
}

fn argmax(scores: &[f32]) -> (usize, f32) {
    scores.iter().copied().enumerate().fold(
        (0, f32::MIN),
        |best, (i, s)| if s > best.1 { (i, s) } else { best },
    )
}

/// Class-wise non-maximum suppression: keeps the highest-scoring box and drops boxes of the same
/// class overlapping it by more than `iou`. Returns boxes by descending score.
pub(crate) fn nms(mut candidates: Vec<Candidate>, iou: f32) -> Vec<Candidate> {
    candidates.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut kept: Vec<Candidate> = Vec::new();
    for c in candidates {
        if kept.iter().all(|k| k.0 != c.0 || k.2.iou(&c.2) <= iou) {
            kept.push(c);
        }
    }
    kept
}

/// The 80 COCO class names, in model order.
const COCO: [&str; 80] = [
    "person",
    "bicycle",
    "car",
    "motorcycle",
    "airplane",
    "bus",
    "train",
    "truck",
    "boat",
    "traffic light",
    "fire hydrant",
    "stop sign",
    "parking meter",
    "bench",
    "bird",
    "cat",
    "dog",
    "horse",
    "sheep",
    "cow",
    "elephant",
    "bear",
    "zebra",
    "giraffe",
    "backpack",
    "umbrella",
    "handbag",
    "tie",
    "suitcase",
    "frisbee",
    "skis",
    "snowboard",
    "sports ball",
    "kite",
    "baseball bat",
    "baseball glove",
    "skateboard",
    "surfboard",
    "tennis racket",
    "bottle",
    "wine glass",
    "cup",
    "fork",
    "knife",
    "spoon",
    "bowl",
    "banana",
    "apple",
    "sandwich",
    "orange",
    "broccoli",
    "carrot",
    "hot dog",
    "pizza",
    "donut",
    "cake",
    "chair",
    "couch",
    "potted plant",
    "bed",
    "dining table",
    "toilet",
    "tv",
    "laptop",
    "mouse",
    "remote",
    "keyboard",
    "cell phone",
    "microwave",
    "oven",
    "toaster",
    "sink",
    "refrigerator",
    "book",
    "clock",
    "vase",
    "scissors",
    "teddy bear",
    "hair drier",
    "toothbrush",
];

/// Maps a model class index to a [`Label`] and the model's class name (plan §3.2).
/// Returns `None` for classes Zoologist ignores (furniture, food, …).
pub fn map_class(classes: ClassSet, index: usize) -> Option<(Label, &'static str)> {
    match classes {
        ClassSet::Megadetector => match index {
            0 => Some((Label::Animal, "animal")),
            1 => Some((Label::Person, "person")),
            2 => Some((Label::Vehicle, "vehicle")),
            _ => None,
        },
        ClassSet::Coco => {
            let name = *COCO.get(index)?;
            let label = match name {
                "person" => Label::Person,
                "bicycle" | "car" | "motorcycle" | "bus" | "train" | "truck" | "boat" => {
                    Label::Vehicle
                }
                "bird" | "cat" | "dog" | "horse" | "sheep" | "cow" | "elephant" | "bear"
                | "zebra" | "giraffe" => Label::Animal,
                _ => return None,
            };
            Some((label, name))
        }
    }
}

/// Stretches a packed RGB image to `size × size` (bilinear), for running the detector on a
/// whole picture (the `detect` command and tests).
pub fn resize_rgb(
    rgb: &[u8],
    width: u32,
    height: u32,
    size: u32,
) -> Result<Vec<u8>, DetectorError> {
    use fast_image_resize::images::{Image, ImageRef};
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
    let invalid = |e: &dyn std::fmt::Display| DetectorError::Invalid(e.to_string());
    let src = ImageRef::new(width, height, rgb, PixelType::U8x3).map_err(|e| invalid(&e))?;
    let mut dst = Image::new(size, size, PixelType::U8x3);
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear));
    Resizer::new()
        .resize(&src, &mut dst, &options)
        .map_err(|e| invalid(&e))?;
    Ok(dst.into_vec())
}

/// `true` if `path` exists (tests skip model-dependent checks when models are not installed).
pub fn model_available(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn b(x1: f32, y1: f32, x2: f32, y2: f32) -> BBox {
        BBox::new(x1, y1, x2, y2)
    }

    #[test]
    fn nms_keeps_best_per_class_and_separate_objects() {
        let boxes = vec![
            (0, 0.9, b(0.1, 0.1, 0.4, 0.4)),
            (0, 0.8, b(0.12, 0.1, 0.42, 0.4)), // duplicate of the first
            (1, 0.7, b(0.12, 0.1, 0.42, 0.4)), // same place, other class: kept
            (0, 0.6, b(0.6, 0.6, 0.9, 0.9)),   // another object
        ];
        let kept = nms(boxes, NMS_IOU);
        let scores: Vec<f32> = kept.iter().map(|k| k.1).collect();
        assert_eq!(scores, vec![0.9, 0.7, 0.6]);
    }

    #[test]
    fn decodes_yolov5_layout() {
        // Two candidates of 5 + 3 values, 320 px input.
        let v = [
            160.0, 160.0, 64.0, 32.0, 0.9, 0.1, 0.8, 0.1, // person 0.72
            10.0, 10.0, 4.0, 4.0, 0.1, 0.9, 0.0, 0.0, // animal 0.09: below threshold
        ];
        let out = decode(DetectorOutput::Yolov5, &[1, 2, 8], &v, 0.3, 320.0).unwrap();
        assert_eq!(out.len(), 1);
        let (class, score, bbox) = out[0];
        assert_eq!(class, 1);
        assert!((score - 0.72).abs() < 1e-6);
        assert_eq!(bbox, b(0.4, 0.45, 0.6, 0.55));
    }

    #[test]
    fn decodes_yolov8_layout() {
        // 4 + 3 attributes × 2 candidates, stored attribute-major.
        let (cx, cy, w, h) = ([100.0, 300.0], [100.0, 20.0], [40.0, 10.0], [40.0, 10.0]);
        let scores = [[0.05, 0.6], [0.02, 0.01], [0.9, 0.0]];
        let mut v = Vec::new();
        for row in [cx, cy, w, h] {
            v.extend(row);
        }
        for row in scores {
            v.extend(row);
        }
        let out = decode(DetectorOutput::Yolov8, &[1, 7, 2], &v, 0.5, 400.0).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].0, out[0].1), (2, 0.9));
        assert_eq!(out[0].2, b(0.2, 0.2, 0.3, 0.3));
        assert_eq!((out[1].0, out[1].1), (0, 0.6));
        assert!(decode(DetectorOutput::Yolov8, &[1, 3, 2], &v, 0.5, 400.0).is_err());
    }

    #[test]
    fn decodes_end_to_end_layout() {
        let v = [
            32.0, 64.0, 96.0, 128.0, 0.8, 2.0, 0.0, 0.0, 1.0, 1.0, 0.1, 0.0,
        ];
        let out = decode(DetectorOutput::YoloE2e, &[1, 2, 6], &v, 0.5, 320.0).unwrap();
        assert_eq!(out, vec![(2, 0.8, b(0.1, 0.2, 0.3, 0.4))]);
    }

    #[test]
    fn class_mapping() {
        assert_eq!(
            map_class(ClassSet::Megadetector, 0),
            Some((Label::Animal, "animal"))
        );
        assert_eq!(map_class(ClassSet::Megadetector, 3), None);
        assert_eq!(
            map_class(ClassSet::Coco, 0),
            Some((Label::Person, "person"))
        );
        assert_eq!(
            map_class(ClassSet::Coco, 7),
            Some((Label::Vehicle, "truck"))
        );
        assert_eq!(map_class(ClassSet::Coco, 16), Some((Label::Animal, "dog")));
        assert_eq!(map_class(ClassSet::Coco, 56), None); // chair
        assert_eq!(map_class(ClassSet::Coco, 80), None);
    }

    // ---- Golden tests against onnxruntime (skipped when models/ is not installed) ----

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    /// Runs `model` on every test photo and compares with `tools/fixtures/golden/<model>.json`.
    /// Every confident golden detection (score ≥ 0.5) must be found (same class, IoU ≥ 0.9, score
    /// within 0.05), and every confident detection of ours must be in the golden list.
    /// Weak detections are not compared: Python (PIL) and Rust resize the photo slightly
    /// differently, which moves scores near the threshold by a few hundredths. The model
    /// numerics themselves are checked exactly by `spikes/tract-bench`.
    fn golden_check(model: &str, size: u32, kind: DetectorOutput, classes: ClassSet) {
        let path = root().join(format!("models/{model}_{size}.onnx"));
        if !model_available(&path) {
            eprintln!(
                "{} not installed (scripts/fetch-models.sh); skipping",
                path.display()
            );
            return;
        }
        let detector = DetectorModel::load(&ModelConfig {
            path,
            kind,
            classes,
            input_size: size,
            score_threshold: 0.2,
        })
        .unwrap();
        let golden: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root().join(format!("tools/fixtures/golden/{model}_{size}.json")))
                .unwrap(),
        )
        .unwrap();
        for (photo, expected) in golden.as_object().unwrap() {
            let img = image::open(root().join("tools/fixtures/images").join(photo))
                .unwrap()
                .to_rgb8();
            let input = resize_rgb(img.as_raw(), img.width(), img.height(), size).unwrap();
            let ours = detector.detect(&input).unwrap();
            let expected: Vec<(String, f32, BBox)> = expected
                .as_array()
                .unwrap()
                .iter()
                .map(|d| {
                    let bb: Vec<f32> = d["bbox"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_f64().unwrap() as f32)
                        .collect();
                    (
                        d["class"].as_str().unwrap().to_string(),
                        d["score"].as_f64().unwrap() as f32,
                        b(bb[0], bb[1], bb[2], bb[3]),
                    )
                })
                .collect();
            for (class, score, bbox) in expected.iter().filter(|e| e.1 >= 0.5) {
                assert!(
                    ours.iter().any(|d| &d.raw_class == class
                        && d.bbox.iou(bbox) >= 0.9
                        && (d.score - score).abs() <= 0.05),
                    "{model}_{size} {photo}: missing {class} {score} {bbox:?}; got {ours:?}"
                );
            }
            for d in ours.iter().filter(|d| d.score >= 0.5) {
                assert!(
                    expected
                        .iter()
                        .any(|(c, _, bb)| c == &d.raw_class && d.bbox.iou(bb) >= 0.9),
                    "{model}_{size} {photo}: extra {d:?}"
                );
            }
        }
    }

    #[test]
    fn golden_md_sorrel() {
        golden_check(
            "md_v1000_sorrel",
            320,
            DetectorOutput::Yolov8,
            ClassSet::Megadetector,
        );
        golden_check(
            "md_v1000_sorrel",
            640,
            DetectorOutput::Yolov8,
            ClassSet::Megadetector,
        );
    }

    #[test]
    fn golden_md_spruce() {
        golden_check(
            "md_v1000_spruce",
            320,
            DetectorOutput::Yolov5,
            ClassSet::Megadetector,
        );
        golden_check(
            "md_v1000_spruce",
            640,
            DetectorOutput::Yolov5,
            ClassSet::Megadetector,
        );
    }

    #[test]
    fn golden_coco() {
        golden_check("yolo26n_coco", 640, DetectorOutput::Yolov8, ClassSet::Coco);
    }
}
