//! The room behind you on camera, replaced: blurred, or swapped for a picture. A small person
//! segmentation model (MediaPipe's selfie segmenter, Apache 2.0, run with tract) finds you in
//! each frame; the mask is smoothed over time so edges don't flicker, then used to blend you over
//! the new background.

use tract_onnx::prelude::*;

static MODEL: &[u8] = include_bytes!("../../assets/models/selfie_segmentation.onnx");

type Plan = std::sync::Arc<TypedRunnableModel>;

pub struct Segmenter {
    plan: Plan,
    width: usize,
    height: usize,
    /// NCHW or NHWC, as the model wants it.
    channels_first: bool,
}

impl Segmenter {
    pub fn load() -> anyhow::Result<Segmenter> {
        // The export pins a symbolic batch size on every intermediate shape; ignoring those lets the
        // input fix it at 1.
        let model = tract_onnx::onnx()
            .with_ignore_value_info(true)
            .with_ignore_output_shapes(true)
            .model_for_read(&mut std::io::Cursor::new(MODEL))?;
        let input = model.input_fact(0)?.clone();
        log::debug!("segmenter input fact: {input:?}");
        if std::env::var_os("HARMONY_MODEL_DEBUG").is_some() {
            eprintln!("segmenter input fact: {input:?}; outputs {:?}", model.output_fact(0));
        }
        let shape: Vec<Option<usize>> = input
            .shape
            .dims()
            .map(|d| match d {
                tract_onnx::tract_hir::infer::GenericFactoid::Only(v) => v.as_i64().map(|v| v as usize),
                _ => None,
            })
            .collect();
        // The input is [1, H, W, 3] or [1, 3, H, W]; fixed sizes are used as they are, a free
        // size gets the model's native 256 × 256.
        let channels_first = shape.get(1) == Some(&Some(3));
        let (h, w) = if channels_first { (shape.get(2), shape.get(3)) } else { (shape.get(1), shape.get(2)) };
        let height = h.copied().flatten().unwrap_or(256);
        let width = w.copied().flatten().unwrap_or(256);
        let dims: Vec<usize> = if channels_first { vec![1, 3, height, width] } else { vec![1, height, width, 3] };
        let plan = model.with_input_fact(0, f32::fact(dims).into())?.into_optimized()?.into_runnable()?;
        Ok(Segmenter { plan, width, height, channels_first })
    }

    pub fn input_size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// The chance each pixel is you, 0..1, at the model's size, for an RGBA frame at any size.
    pub fn mask(&self, rgba: &[u8], w: usize, h: usize) -> anyhow::Result<Vec<f32>> {
        let (mw, mh) = (self.width, self.height);
        let mut data = vec![0f32; 3 * mh * mw];
        {
            for y in 0..mh {
                let sy = (y * h / mh).min(h - 1);
                for x in 0..mw {
                    let sx = (x * w / mw).min(w - 1);
                    let p = &rgba[(sy * w + sx) * 4..(sy * w + sx) * 4 + 3];
                    for (c, &byte) in p.iter().enumerate() {
                        let v = byte as f32 / 255.;
                        let i = if self.channels_first { c * mh * mw + y * mw + x } else { (y * mw + x) * 3 + c };
                        data[i] = v;
                    }
                }
            }
        }
        let dims: &[usize] = if self.channels_first { &[1, 3, mh, mw] } else { &[1, mh, mw, 3] };
        let input = tract_ndarray::ArrayD::from_shape_vec(dims, data)?.into_tensor();
        let out = self.plan.run(tvec!(input.into()))?;
        let t = out[0].clone().into_tensor();
        let m = t.to_plain_array_view::<f32>()?;
        let flat: Vec<f32> = m.iter().copied().collect();
        // One value per pixel; some exports give two (background, person).
        Ok(if flat.len() == mw * mh * 2 { flat.chunks_exact(2).map(|p| p[1]).collect() } else { flat })
    }
}

/// What to put behind the person.
pub enum Backdrop<'a> {
    Blur(u32),
    /// An RGBA picture at the frame's size.
    Image(&'a [u8]),
}

/// Smooths the mask over time and blends each frame onto the backdrop.
#[derive(Default)]
pub struct Compositor {
    mask: Vec<f32>,
    mask_w: usize,
    mask_h: usize,
    blur: Blur,
    /// Per column: the two mask columns either side and the weight between them.
    columns: Vec<(usize, usize, f32)>,
}

impl Compositor {
    pub fn new() -> Compositor {
        Compositor::default()
    }

    /// Feeds a fresh mask from the model; mixing it with the last one hides flicker.
    pub fn update_mask(&mut self, mask: &[f32], w: usize, h: usize) {
        if self.mask.len() != mask.len() || self.mask_w != w || self.mask_h != h {
            self.mask = mask.to_vec();
            self.mask_w = w;
            self.mask_h = h;
            self.columns.clear();
            return;
        }
        for (m, n) in self.mask.iter_mut().zip(mask) {
            *m = *m * 0.45 + n * 0.55;
        }
    }

    /// Blends `rgba` (w × h) in place over the backdrop.
    pub fn apply(&mut self, rgba: &mut [u8], w: usize, h: usize, backdrop: &Backdrop) {
        if self.mask.is_empty() {
            return;
        }
        let bg: &[u8] = match backdrop {
            Backdrop::Blur(radius) => self.blur.run(rgba, w, h, (*radius as usize).max(1)),
            Backdrop::Image(img) if img.len() == rgba.len() => img,
            Backdrop::Image(_) => return,
        };
        let (mw, mh) = (self.mask_w, self.mask_h);
        if self.columns.len() != w {
            self.columns = (0..w)
                .map(|x| {
                    let fx = (x as f32 + 0.5) * mw as f32 / w as f32 - 0.5;
                    let x0 = (fx.floor().max(0.) as usize).min(mw - 1);
                    (x0, (x0 + 1).min(mw - 1), (fx - x0 as f32).clamp(0., 1.))
                })
                .collect();
        }
        for y in 0..h {
            // Bilinear lookup of the mask, so its edge is smooth at camera resolution.
            let fy = (y as f32 + 0.5) * mh as f32 / h as f32 - 0.5;
            let y0 = (fy.floor().max(0.) as usize).min(mh - 1);
            let y1 = (y0 + 1).min(mh - 1);
            let ty = (fy - y0 as f32).clamp(0., 1.);
            let (r0, r1) = (&self.mask[y0 * mw..(y0 + 1) * mw], &self.mask[y1 * mw..(y1 + 1) * mw]);
            let row = &mut rgba[y * w * 4..(y + 1) * w * 4];
            let bg_row = &bg[y * w * 4..(y + 1) * w * 4];
            for ((px, bp), &(x0, x1, tx)) in row.chunks_exact_mut(4).zip(bg_row.chunks_exact(4)).zip(&self.columns) {
                let a = r0[x0] + (r0[x1] - r0[x0]) * tx;
                let b = r1[x0] + (r1[x1] - r1[x0]) * tx;
                // A soft threshold sharpens the model's blurry edge a little.
                let alpha = (((a + (b - a) * ty) - 0.35) / 0.3).clamp(0., 1.);
                for c in 0..3 {
                    px[c] = (px[c] as f32 * alpha + bp[c] as f32 * (1. - alpha)) as u8;
                }
            }
        }
    }
}

/// A blurred copy of a frame, worked out small: shrunk by a factor (averaging blocks), box
/// blurred there, and stretched back bilinearly. The radius in the small picture is picked so
/// the whole chain spreads a pixel as far as the same blur at full size would.
#[derive(Default)]
struct Blur {
    small: Vec<u8>,
    tmp: Vec<u8>,
    out: Vec<u8>,
}

impl Blur {
    fn run(&mut self, rgba: &[u8], w: usize, h: usize, radius: usize) -> &[u8] {
        let f = (radius / 3).clamp(1, 4);
        let (sw, sh) = (w.div_ceil(f), h.div_ceil(f));
        shrink(rgba, w, h, f, &mut self.small);
        box_blur_with(&mut self.small, &mut self.tmp, sw, sh, small_radius(radius, f));
        self.out.resize(rgba.len(), 255);
        stretch(&self.small, sw, sh, f, &mut self.out, w, h);
        &self.out
    }
}

/// Three box passes of radius r spread a pixel with variance r(r+1) (in pixels²). Shrinking by
/// f and stretching back add about f²/4; the rest is left to the small picture's blur.
fn small_radius(radius: usize, f: usize) -> usize {
    let target = (radius * (radius + 1)) as f32;
    let left = (target - (f * f) as f32 / 4.).max(0.) / (f * f) as f32;
    // r(r+1) = left, solved for r.
    (((1. + 4. * left).sqrt() - 1.) / 2.).round().max(1.) as usize
}

/// Averages f × f blocks of an RGBA picture.
fn shrink(src: &[u8], w: usize, h: usize, f: usize, dst: &mut Vec<u8>) {
    let (sw, sh) = (w.div_ceil(f), h.div_ceil(f));
    dst.clear();
    dst.resize(sw * sh * 4, 255);
    let mut sums = vec![[0u32; 4]; sw];
    for by in 0..sh {
        sums.iter_mut().for_each(|s| *s = [0; 4]);
        for y in by * f..((by + 1) * f).min(h) {
            for (x, p) in src[y * w * 4..(y + 1) * w * 4].chunks_exact(4).enumerate() {
                let s = &mut sums[x / f];
                for c in 0..3 {
                    s[c] += p[c] as u32;
                }
                s[3] += 1;
            }
        }
        for (bx, s) in sums.iter().enumerate() {
            let n = s[3].max(1);
            let d = &mut dst[(by * sw + bx) * 4..(by * sw + bx) * 4 + 3];
            for c in 0..3 {
                d[c] = ((s[c] + n / 2) / n) as u8;
            }
        }
    }
}

/// Bilinear stretch of a picture shrunk by f back to w × h, sampling at block centres.
fn stretch(src: &[u8], sw: usize, sh: usize, f: usize, dst: &mut [u8], w: usize, h: usize) {
    let at = |v: usize, n: usize| {
        let p = ((v as f32 + 0.5) / f as f32 - 0.5).max(0.);
        let i = (p as usize).min(n - 1);
        (i, (i + 1).min(n - 1), p - i as f32)
    };
    let columns: Vec<(usize, usize, f32)> = (0..w).map(|x| at(x, sw)).collect();
    for y in 0..h {
        let (y0, y1, ty) = at(y, sh);
        let (r0, r1) = (&src[y0 * sw * 4..(y0 + 1) * sw * 4], &src[y1 * sw * 4..(y1 + 1) * sw * 4]);
        for (d, &(x0, x1, tx)) in dst[y * w * 4..(y + 1) * w * 4].chunks_exact_mut(4).zip(&columns) {
            for c in 0..3 {
                let top = r0[x0 * 4 + c] as f32 + (r0[x1 * 4 + c] as f32 - r0[x0 * 4 + c] as f32) * tx;
                let bottom = r1[x0 * 4 + c] as f32 + (r1[x1 * 4 + c] as f32 - r1[x0 * 4 + c] as f32) * tx;
                d[c] = (top + (bottom - top) * ty + 0.5) as u8;
            }
        }
    }
}

/// A fast blur of an RGBA picture: three box passes approximate a Gaussian.
fn box_blur_with(rgba: &mut [u8], tmp: &mut Vec<u8>, w: usize, h: usize, radius: usize) {
    tmp.resize(rgba.len(), 255);
    for _ in 0..3 {
        blur_pass(rgba, tmp, w, h, radius, true);
        blur_pass(tmp, rgba, w, h, radius, false);
    }
}

fn blur_pass(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize, horizontal: bool) {
    let (outer, inner) = if horizontal { (h, w) } else { (w, h) };
    let at = |o: usize, i: usize| if horizontal { (o * w + i) * 4 } else { (i * w + o) * 4 };
    let span = (2 * r + 1) as u32;
    for o in 0..outer {
        let mut sum = [0u32; 3];
        for k in 0..=2 * r {
            let i = k.saturating_sub(r).min(inner - 1);
            let p = at(o, i);
            for c in 0..3 {
                sum[c] += src[p + c] as u32;
            }
        }
        for i in 0..inner {
            let p = at(o, i);
            for c in 0..3 {
                dst[p + c] = (sum[c] / span) as u8;
            }
            let out = at(o, i.saturating_sub(r));
            let inn = at(o, (i + r + 1).min(inner - 1));
            for c in 0..3 {
                sum[c] = sum[c] + src[inn + c] as u32 - src[out + c] as u32;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_loads_and_finds_nothing_in_a_blank_frame() {
        let s = match Segmenter::load() {
            Ok(s) => s,
            Err(e) => panic!("model: {e:?}"),
        };
        let (w, h) = s.input_size();
        println!("segmenter input {w}x{h}, channels first: {}", s.channels_first);
        let frame = vec![0u8; 320 * 180 * 4];
        let t = std::time::Instant::now();
        let mask = s.mask(&frame, 320, 180).expect("inference");
        println!("inference took {:?}", t.elapsed());
        assert_eq!(mask.len(), w * h);
        assert!(mask.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn blur_keeps_a_flat_image_flat() {
        let mut img = vec![100u8; 16 * 9 * 4];
        box_blur_with(&mut img, &mut Vec::new(), 16, 9, 3);
        assert!(img.iter().all(|&v| (99..=100).contains(&v)));
    }

    /// Something with edges and texture at camera size: stripes, a gradient and a block.
    fn scene(w: usize, h: usize) -> Vec<u8> {
        let mut img = vec![255u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let p = &mut img[(y * w + x) * 4..];
                p[0] = if (x / 7) % 2 == 0 { 230 } else { 20 };
                p[1] = (x * 255 / w) as u8;
                p[2] = if (200..360).contains(&x) && (90..250).contains(&y) { 240 } else { (y * 255 / h) as u8 };
            }
        }
        img
    }

    #[test]
    fn the_small_blur_looks_like_the_full_one() {
        let (w, h) = (640, 360);
        let source = scene(w, h);
        for radius in [2, 5, 8, 12, 20, 30] {
            let mut full = source.clone();
            box_blur_with(&mut full, &mut Vec::new(), w, h, radius);
            let mut blur = Blur::default();
            let small = blur.run(&source, w, h, radius);
            let diff: u64 = full.iter().zip(small).map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64).sum();
            let mean = diff as f64 / (w * h * 3) as f64;
            println!("radius {radius}: mean difference {mean:.2} levels");
            assert!(mean < 3., "radius {radius}: mean difference {mean:.2} levels");
        }
    }
}
