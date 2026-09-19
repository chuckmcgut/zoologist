//! YUV and image helpers (plan Step 1.3).
//!
//! Frames are I420 (see [`Frame`]). Motion works on the Y plane alone. Only the small regions
//! that go to a neural network, plus snapshots, are ever converted to RGB.
//!
//! Colour conversion uses BT.601 limited range ("studio swing"), which is what IP cameras send,
//! in integer arithmetic.

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

use crate::Frame;

/// Errors from the resize library. Inputs are validated first, so these indicate a bug.
#[derive(Debug, thiserror::Error)]
pub enum YuvError {
    #[error("image buffer error: {0}")]
    Buffer(#[from] fast_image_resize::ImageBufferError),
    #[error("resize error: {0}")]
    Resize(#[from] fast_image_resize::ResizeError),
    #[error("invalid input: {0}")]
    Invalid(String),
}

/// A pixel rectangle inside a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl PixelRect {
    /// The rectangle clamped to a `frame_w × frame_h` frame and aligned to even coordinates and
    /// sizes (I420 chroma covers 2×2 pixel blocks). Never empty for a non-empty frame.
    pub fn aligned(self, frame_w: u32, frame_h: u32) -> PixelRect {
        let x = (self.x.min(frame_w.saturating_sub(2))) & !1;
        let y = (self.y.min(frame_h.saturating_sub(2))) & !1;
        let w = (self.w.max(2).min(frame_w - x) + 1) & !1;
        let h = (self.h.max(2).min(frame_h - y) + 1) & !1;
        PixelRect {
            x,
            y,
            w: w.min(frame_w - x),
            h: h.min(frame_h - y),
        }
    }
}

/// Downscales a grayscale plane to `out_w` wide (keeping the aspect ratio) with a box filter.
/// Returns the pixels, width and height. Upscaling is never done: if `out_w >= w`, the plane is
/// copied unchanged.
pub fn downscale_y(y: &[u8], w: u32, h: u32, out_w: u32) -> Result<(Vec<u8>, u32, u32), YuvError> {
    if w == 0 || h == 0 || y.len() < (w * h) as usize {
        return Err(YuvError::Invalid(format!(
            "plane of {} bytes is not {w}×{h}",
            y.len()
        )));
    }
    if out_w >= w {
        return Ok((y[..(w * h) as usize].to_vec(), w, h));
    }
    let out_w = out_w.max(1);
    let out_h = ((h as u64 * out_w as u64 + w as u64 / 2) / w as u64).max(1) as u32;
    let src = ImageRef::new(w, h, y, PixelType::U8)?;
    let mut dst = Image::new(out_w, out_h, PixelType::U8);
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Box));
    Resizer::new().resize(&src, &mut dst, &options)?;
    Ok((dst.into_vec(), out_w, out_h))
}

/// Converts one pixel from limited-range BT.601 YUV to RGB.
#[inline]
pub fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let c = 298 * (y as i32 - 16);
    let d = u as i32 - 128;
    let e = v as i32 - 128;
    let clip = |x: i32| (x >> 8).clamp(0, 255) as u8;
    [
        clip(c + 409 * e + 128),
        clip(c - 100 * d - 208 * e + 128),
        clip(c + 516 * d + 128),
    ]
}

/// Converts one pixel from RGB to limited-range BT.601 YUV.
#[inline]
pub fn rgb_to_yuv(r: u8, g: u8, b: u8) -> [u8; 3] {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    [
        (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8,
        (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8,
        (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8,
    ]
}

/// Converts a packed RGB image (`w × h × 3`, even `w` and `h`) to I420. Chroma is the average of
/// each 2×2 block. Mostly used to build test frames.
pub fn rgb_to_i420(rgb: &[u8], w: u32, h: u32) -> Result<Vec<u8>, YuvError> {
    if !w.is_multiple_of(2) || !h.is_multiple_of(2) || rgb.len() < (w * h * 3) as usize {
        return Err(YuvError::Invalid(format!(
            "need even dimensions and {}×{}×3 bytes",
            w, h
        )));
    }
    let (w, h) = (w as usize, h as usize);
    let mut out = vec![0u8; w * h * 3 / 2];
    let (y_plane, chroma) = out.split_at_mut(w * h);
    let (u_plane, v_plane) = chroma.split_at_mut(w * h / 4);
    for row in 0..h {
        for col in 0..w {
            let p = (row * w + col) * 3;
            y_plane[row * w + col] = rgb_to_yuv(rgb[p], rgb[p + 1], rgb[p + 2])[0];
        }
    }
    for row in (0..h).step_by(2) {
        for col in (0..w).step_by(2) {
            let (mut u, mut v) = (0u32, 0u32);
            for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                let p = ((row + dy) * w + col + dx) * 3;
                let [_, pu, pv] = rgb_to_yuv(rgb[p], rgb[p + 1], rgb[p + 2]);
                u += pu as u32;
                v += pv as u32;
            }
            let c = (row / 2) * (w / 2) + col / 2;
            u_plane[c] = ((u + 2) / 4) as u8;
            v_plane[c] = ((v + 2) / 4) as u8;
        }
    }
    Ok(out)
}

/// Converts a rectangle of an I420 frame to packed RGB (`rect.w × rect.h × 3`), writing into
/// `dst` (which is resized, so a reused buffer never reallocates once large enough).
/// `rect` must already be [`PixelRect::aligned`].
pub fn i420_rect_to_rgb(frame: &Frame, rect: PixelRect, dst: &mut Vec<u8>) {
    let fw = frame.width as usize;
    let planes = PlanesRef {
        y: &frame.y_plane()[rect.y as usize * fw + rect.x as usize..],
        y_stride: fw,
        u: &frame.u_plane()[(rect.y as usize / 2) * (fw / 2) + rect.x as usize / 2..],
        v: &frame.v_plane()[(rect.y as usize / 2) * (fw / 2) + rect.x as usize / 2..],
        c_stride: fw / 2,
    };
    planes_to_rgb(&planes, rect.w as usize, rect.h as usize, dst);
}

/// Borrowed I420 planes starting at the top-left pixel of the area to convert.
struct PlanesRef<'a> {
    y: &'a [u8],
    y_stride: usize,
    u: &'a [u8],
    v: &'a [u8],
    c_stride: usize,
}

/// Converts a `w × h` area (both even) of I420 planes to packed RGB. Each pair of pixels shares
/// one chroma sample, so the chroma terms are computed once per pair.
fn planes_to_rgb(p: &PlanesRef<'_>, w: usize, h: usize, dst: &mut Vec<u8>) {
    dst.resize(w * h * 3, 0);
    let clip = |x: i32| (x >> 8).clamp(0, 255) as u8;
    for (row, out_row) in dst.chunks_mut(w * 3).enumerate().take(h) {
        let y_row = &p.y[row * p.y_stride..][..w];
        let c = (row / 2) * p.c_stride;
        let (u_row, v_row) = (&p.u[c..][..w / 2], &p.v[c..][..w / 2]);
        let pairs = out_row.as_chunks_mut::<6>().0.iter_mut();
        let lumas = y_row.as_chunks::<2>().0.iter();
        for ((out, luma), (&u, &v)) in pairs.zip(lumas).zip(u_row.iter().zip(v_row)) {
            let d = u as i32 - 128;
            let e = v as i32 - 128;
            let (r_off, g_off, b_off) = (409 * e + 128, -100 * d - 208 * e + 128, 516 * d + 128);
            for (i, &luma) in luma.iter().enumerate() {
                let c = 298 * (luma as i32 - 16);
                out[i * 3] = clip(c + r_off);
                out[i * 3 + 1] = clip(c + g_off);
                out[i * 3 + 2] = clip(c + b_off);
            }
        }
    }
}

/// The whole frame as packed RGB (`width × height × 3`), for snapshots.
pub fn i420_to_rgb_full(frame: &Frame) -> Vec<u8> {
    let mut rgb = Vec::new();
    let rect = PixelRect {
        x: 0,
        y: 0,
        w: frame.width,
        h: frame.height,
    };
    i420_rect_to_rgb(frame, rect, &mut rgb);
    rgb
}

/// Crops regions out of frames, converts them to RGB and resizes them to a square network
/// input. Keeps its buffers between calls, so one `RgbCropper` per worker thread allocates
/// only on the first few calls.
///
/// The Y, U and V planes are cropped and resized separately (fast single-channel SIMD resizes),
/// and colour conversion runs only on the small output.
pub struct RgbCropper {
    resizer: Resizer,
    alg: ResizeAlg,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    scratch: Vec<u8>,
}

impl Default for RgbCropper {
    fn default() -> Self {
        Self::new()
    }
}

impl RgbCropper {
    /// A cropper using bilinear resampling.
    pub fn new() -> Self {
        Self {
            resizer: Resizer::new(),
            alg: ResizeAlg::Convolution(FilterType::Bilinear),
            y: Vec::new(),
            u: Vec::new(),
            v: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// Crops `region` (clamped and even-aligned), converts it to RGB and resizes it to
    /// `out × out` into `dst` (`out * out * 3` bytes). `out` must be even. A non-square region
    /// is stretched; use [`RgbCropper::crop_letterboxed`] to keep its aspect ratio.
    pub fn crop(
        &mut self,
        frame: &Frame,
        region: PixelRect,
        out: u32,
        dst: &mut Vec<u8>,
    ) -> Result<(), YuvError> {
        self.crop_to(frame, region, out, out, dst)
    }

    /// Like [`RgbCropper::crop`] but to an `out_w × out_h` output (both even).
    pub fn crop_to(
        &mut self,
        frame: &Frame,
        region: PixelRect,
        out_w: u32,
        out_h: u32,
        dst: &mut Vec<u8>,
    ) -> Result<(), YuvError> {
        let even = |v: u32| v >= 2 && v.is_multiple_of(2);
        if !even(out_w) || !even(out_h) || frame.width < 2 || frame.height < 2 {
            return Err(YuvError::Invalid(format!(
                "output {out_w}×{out_h} must be even and the frame non-empty"
            )));
        }
        let r = region.aligned(frame.width, frame.height);
        let (fw, fh) = (frame.width, frame.height);
        resize_plane(
            &mut self.resizer,
            self.alg,
            (frame.y_plane(), fw, fh),
            (r.x, r.y, r.w, r.h),
            (out_w, out_h),
            &mut self.y,
        )?;
        let chroma_crop = (r.x / 2, r.y / 2, r.w / 2, r.h / 2);
        let chroma_out = (out_w / 2, out_h / 2);
        resize_plane(
            &mut self.resizer,
            self.alg,
            (frame.u_plane(), fw / 2, fh / 2),
            chroma_crop,
            chroma_out,
            &mut self.u,
        )?;
        resize_plane(
            &mut self.resizer,
            self.alg,
            (frame.v_plane(), fw / 2, fh / 2),
            chroma_crop,
            chroma_out,
            &mut self.v,
        )?;
        let planes = PlanesRef {
            y: &self.y,
            y_stride: out_w as usize,
            u: &self.u,
            v: &self.v,
            c_stride: (out_w / 2) as usize,
        };
        planes_to_rgb(&planes, out_w as usize, out_h as usize, dst);
        Ok(())
    }

    /// Crops `region`, scales it to fit a `size × size` square without changing its aspect
    /// ratio, and pads the rest with grey (114, as YOLO models are trained). Returns how the
    /// region was placed, to map detections back (`Letterbox::to_source` gives pixels relative
    /// to the region's top-left corner).
    pub fn crop_letterboxed(
        &mut self,
        frame: &Frame,
        region: PixelRect,
        size: u32,
        dst: &mut Vec<u8>,
    ) -> Result<Letterbox, YuvError> {
        let r = region.aligned(frame.width, frame.height);
        let scale = size as f32 / r.w.max(r.h) as f32;
        let even = |v: f32| ((v.round() as u32) & !1).clamp(2, size);
        let (ow, oh) = (even(r.w as f32 * scale), even(r.h as f32 * scale));
        let mut scaled = std::mem::take(&mut self.scratch);
        self.crop_to(frame, r, ow, oh, &mut scaled)?;
        let (pad_x, pad_y) = ((size - ow) / 2, (size - oh) / 2);
        dst.clear();
        dst.resize((size * size * 3) as usize, 114);
        for row in 0..oh as usize {
            let from = &scaled[row * ow as usize * 3..][..ow as usize * 3];
            let at = ((row + pad_y as usize) * size as usize + pad_x as usize) * 3;
            dst[at..at + from.len()].copy_from_slice(from);
        }
        self.scratch = scaled;
        Ok(Letterbox {
            scale: ow as f32 / r.w as f32,
            pad_x: pad_x as f32,
            pad_y: pad_y as f32,
            size,
        })
    }
}

/// Crops `(x, y, w, h)` out of a single-channel plane and resizes it to `out_w × out_h`.
fn resize_plane(
    resizer: &mut Resizer,
    alg: ResizeAlg,
    (plane, pw, ph): (&[u8], u32, u32),
    (x, y, w, h): (u32, u32, u32, u32),
    (out_w, out_h): (u32, u32),
    dst: &mut Vec<u8>,
) -> Result<(), YuvError> {
    let src = ImageRef::new(pw, ph, plane, PixelType::U8)?;
    dst.resize((out_w * out_h) as usize, 0);
    let mut dst_image = Image::from_slice_u8(out_w, out_h, dst.as_mut_slice(), PixelType::U8)?;
    let options = ResizeOptions::new()
        .resize_alg(alg)
        .crop(x as f64, y as f64, w as f64, h as f64);
    resizer.resize(&src, &mut dst_image, &options)?;
    Ok(())
}

/// Scales a whole I420 picture to `out_w × out_h` (both even) with bilinear filtering.
pub fn scale_i420(src: &[u8], w: u32, h: u32, out_w: u32, out_h: u32) -> Result<Vec<u8>, YuvError> {
    let valid = |a: u32, b: u32| a >= 2 && b >= 2 && a.is_multiple_of(2) && b.is_multiple_of(2);
    if !valid(w, h) || !valid(out_w, out_h) || src.len() < Frame::i420_len(w, h) {
        return Err(YuvError::Invalid(format!(
            "cannot scale {w}×{h} to {out_w}×{out_h}"
        )));
    }
    let (y_len, c_len) = ((w * h) as usize, (w * h / 4) as usize);
    let (oy_len, oc_len) = ((out_w * out_h) as usize, (out_w * out_h / 4) as usize);
    let mut out = vec![0u8; oy_len + 2 * oc_len];
    let mut resizer = Resizer::new();
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear));
    let planes = [
        (0, y_len, w, h, 0, oy_len, out_w, out_h),
        (
            y_len,
            c_len,
            w / 2,
            h / 2,
            oy_len,
            oc_len,
            out_w / 2,
            out_h / 2,
        ),
        (
            y_len + c_len,
            c_len,
            w / 2,
            h / 2,
            oy_len + oc_len,
            oc_len,
            out_w / 2,
            out_h / 2,
        ),
    ];
    for (src_off, src_len, sw, sh, dst_off, dst_len, dw, dh) in planes {
        let src_img = ImageRef::new(sw, sh, &src[src_off..src_off + src_len], PixelType::U8)?;
        let mut dst_img =
            Image::from_slice_u8(dw, dh, &mut out[dst_off..dst_off + dst_len], PixelType::U8)?;
        resizer.resize(&src_img, &mut dst_img, &options)?;
    }
    Ok(out)
}

/// How an image was letterboxed into a square model input: scaled by `scale`, then padded by
/// `pad_x` / `pad_y` pixels on the left / top.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Letterbox {
    pub scale: f32,
    pub pad_x: f32,
    pub pad_y: f32,
    pub size: u32,
}

impl Letterbox {
    /// Parameters for fitting a `src_w × src_h` image into a `size × size` square without
    /// changing its aspect ratio.
    pub fn new(src_w: u32, src_h: u32, size: u32) -> Letterbox {
        let scale = (size as f32 / src_w as f32).min(size as f32 / src_h as f32);
        Letterbox {
            scale,
            pad_x: (size as f32 - src_w as f32 * scale) / 2.0,
            pad_y: (size as f32 - src_h as f32 * scale) / 2.0,
            size,
        }
    }

    /// Maps a point in model-input pixels back to source-image pixels.
    pub fn to_source(&self, x: f32, y: f32) -> (f32, f32) {
        ((x - self.pad_x) / self.scale, (y - self.pad_y) / self.scale)
    }

    /// Maps a point in source-image pixels to model-input pixels.
    pub fn to_input(&self, x: f32, y: f32) -> (f32, f32) {
        (x * self.scale + self.pad_x, y * self.scale + self.pad_y)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use chrono::Utc;

    use super::*;

    fn solid_frame(w: u32, h: u32, rgb: [u8; 3]) -> Frame {
        let pixels: Vec<u8> = (0..w * h).flat_map(|_| rgb).collect();
        Frame {
            camera_id: "test".into(),
            seq: 0,
            captured_at: Utc::now(),
            width: w,
            height: h,
            i420: Arc::new(rgb_to_i420(&pixels, w, h).unwrap()),
        }
    }

    fn assert_close(actual: &[u8], expected: [u8; 3], tolerance: i32) {
        for (a, e) in actual.iter().zip(expected) {
            assert!(
                (*a as i32 - e as i32).abs() <= tolerance,
                "got {actual:?}, expected {expected:?} ± {tolerance}"
            );
        }
    }

    #[test]
    fn solid_colours_round_trip() {
        for colour in [
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [128, 128, 128],
            [0, 0, 0],
            [255, 255, 255],
        ] {
            let frame = solid_frame(8, 6, colour);
            let rgb = i420_to_rgb_full(&frame);
            assert_eq!(rgb.len(), 8 * 6 * 3);
            for px in rgb.as_chunks::<3>().0 {
                assert_close(px, colour, 2);
            }
        }
    }

    #[test]
    fn rect_conversion_reads_the_right_pixels() {
        // Left half red, right half blue.
        let (w, h) = (8u32, 4u32);
        let pixels: Vec<u8> = (0..h)
            .flat_map(|_| (0..w).flat_map(|x| if x < 4 { [255, 0, 0] } else { [0, 0, 255] }))
            .collect();
        let frame = Frame {
            camera_id: "t".into(),
            seq: 0,
            captured_at: Utc::now(),
            width: w,
            height: h,
            i420: Arc::new(rgb_to_i420(&pixels, w, h).unwrap()),
        };
        let mut out = Vec::new();
        i420_rect_to_rgb(
            &frame,
            PixelRect {
                x: 4,
                y: 2,
                w: 4,
                h: 2,
            },
            &mut out,
        );
        assert_eq!(out.len(), 4 * 2 * 3);
        for px in out.as_chunks::<3>().0 {
            assert_close(px, [0, 0, 255], 2);
        }
    }

    #[test]
    fn aligned_clamps_to_frame_and_even_numbers() {
        let r = PixelRect {
            x: 631,
            y: 351,
            w: 100,
            h: 100,
        }
        .aligned(640, 360);
        assert_eq!(
            r,
            PixelRect {
                x: 630,
                y: 350,
                w: 10,
                h: 10
            }
        );
        let r = PixelRect {
            x: 3,
            y: 5,
            w: 7,
            h: 1,
        }
        .aligned(640, 360);
        assert_eq!(
            r,
            PixelRect {
                x: 2,
                y: 4,
                w: 8,
                h: 2
            }
        );
        let r = PixelRect {
            x: 0,
            y: 0,
            w: 5000,
            h: 5000,
        }
        .aligned(640, 360);
        assert_eq!(
            r,
            PixelRect {
                x: 0,
                y: 0,
                w: 640,
                h: 360
            }
        );
    }

    #[test]
    fn cropper_resizes_to_square_output() {
        let frame = solid_frame(640, 360, [10, 200, 30]);
        let mut cropper = RgbCropper::new();
        let mut out = Vec::new();
        cropper
            .crop(
                &frame,
                PixelRect {
                    x: 600,
                    y: 300,
                    w: 200,
                    h: 200,
                },
                320,
                &mut out,
            )
            .unwrap();
        assert_eq!(out.len(), 320 * 320 * 3);
        assert_close(&out[..3], [10, 200, 30], 3);
        assert_close(&out[out.len() - 3..], [10, 200, 30], 3);
    }

    #[test]
    fn cropper_takes_pixels_from_the_requested_region() {
        // Left half red, right half blue.
        let (w, h) = (640u32, 360u32);
        let pixels: Vec<u8> = (0..h)
            .flat_map(|_| (0..w).flat_map(|x| if x < 320 { [255, 0, 0] } else { [0, 0, 255] }))
            .collect();
        let frame = Frame {
            camera_id: "t".into(),
            seq: 0,
            captured_at: Utc::now(),
            width: w,
            height: h,
            i420: Arc::new(rgb_to_i420(&pixels, w, h).unwrap()),
        };
        let mut cropper = RgbCropper::new();
        let mut out = Vec::new();

        cropper
            .crop(
                &frame,
                PixelRect {
                    x: 360,
                    y: 0,
                    w: 280,
                    h: 280,
                },
                64,
                &mut out,
            )
            .unwrap();
        assert!(
            out.as_chunks::<3>()
                .0
                .iter()
                .all(|px| px[2] > 240 && px[0] < 15)
        );

        cropper
            .crop(
                &frame,
                PixelRect {
                    x: 0,
                    y: 0,
                    w: 640,
                    h: 360,
                },
                64,
                &mut out,
            )
            .unwrap();
        let first_row = &out[..64 * 3];
        assert_close(&first_row[..3], [255, 0, 0], 3);
        assert_close(&first_row[first_row.len() - 3..], [0, 0, 255], 3);

        assert!(
            cropper
                .crop(
                    &frame,
                    PixelRect {
                        x: 0,
                        y: 0,
                        w: 10,
                        h: 10
                    },
                    63,
                    &mut out
                )
                .is_err()
        );
    }

    #[test]
    fn letterboxed_crop_keeps_aspect_ratio_and_pads_with_grey() {
        let frame = solid_frame(640, 360, [200, 30, 30]);
        let mut cropper = RgbCropper::new();
        let mut out = Vec::new();
        let full = PixelRect {
            x: 0,
            y: 0,
            w: 640,
            h: 360,
        };
        let lb = cropper
            .crop_letterboxed(&frame, full, 320, &mut out)
            .unwrap();
        assert_eq!(out.len(), 320 * 320 * 3);
        assert_eq!((lb.scale, lb.pad_x, lb.pad_y), (0.5, 0.0, 70.0));
        let px = |x: usize, y: usize| &out[(y * 320 + x) * 3..][..3];
        assert_eq!(px(10, 10), &[114, 114, 114]); // padding above
        assert_close(px(10, 160), [200, 30, 30], 3); // picture
        assert_eq!(px(10, 300), &[114, 114, 114]); // padding below
        // A point in the model input maps back to the region.
        assert_eq!(lb.to_source(160.0, 160.0), (320.0, 180.0));
    }

    #[test]
    fn downscale_keeps_aspect_ratio_and_never_upscales() {
        let y = vec![100u8; 640 * 360];
        let (small, w, h) = downscale_y(&y, 640, 360, 320).unwrap();
        assert_eq!((w, h, small.len()), (320, 180, 320 * 180));
        assert!(small.iter().all(|&p| p == 100));
        let (same, w, h) = downscale_y(&y, 640, 360, 1000).unwrap();
        assert_eq!((w, h, same.len()), (640, 360, 640 * 360));
        assert!(downscale_y(&y[..10], 640, 360, 320).is_err());
    }

    #[test]
    fn scale_i420_halves_a_solid_frame() {
        let frame = solid_frame(1280, 720, [200, 40, 90]);
        let out = scale_i420(&frame.i420, 1280, 720, 640, 360).unwrap();
        assert_eq!(out.len(), Frame::i420_len(640, 360));
        let small = Frame {
            width: 640,
            height: 360,
            i420: Arc::new(out),
            ..frame
        };
        for px in i420_to_rgb_full(&small).as_chunks::<3>().0 {
            assert_close(px, [200, 40, 90], 3);
        }
        assert!(scale_i420(&[0; 10], 1280, 720, 640, 360).is_err());
    }

    #[test]
    fn letterbox_maps_both_ways() {
        let lb = Letterbox::new(640, 360, 320);
        assert_eq!(lb.scale, 0.5);
        assert_eq!((lb.pad_x, lb.pad_y), (0.0, 70.0));
        let (x, y) = lb.to_input(640.0, 360.0);
        assert_eq!((x, y), (320.0, 250.0));
        assert_eq!(lb.to_source(x, y), (640.0, 360.0));
    }

    /// Timing only; run with `cargo test --release -p zoologist-core -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn crop_benchmark() {
        let frame = solid_frame(640, 360, [90, 120, 150]);
        let mut cropper = RgbCropper::new();
        let mut out = Vec::new();
        let region = PixelRect {
            x: 100,
            y: 0,
            w: 360,
            h: 360,
        };
        cropper.crop(&frame, region, 320, &mut out).unwrap();
        let n = 1000;
        let start = Instant::now();
        for _ in 0..n {
            cropper.crop(&frame, region, 320, &mut out).unwrap();
        }
        let per = start.elapsed() / n;
        println!("360×360 crop + YUV→RGB + resize to 320×320: {per:?} per call");
    }
}
