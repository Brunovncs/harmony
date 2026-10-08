//! Draws Harmony's icon (a microphone on a stand, on a rounded square) into `assets/icon.ico`
//! and `assets/icon.png`.
//!
//!     cargo run --release --example icon

use image::{Rgba, RgbaImage};

fn main() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    let sizes = [256u32, 128, 64, 48, 32, 24, 16];
    let frames: Vec<RgbaImage> = sizes.iter().map(|&s| draw(s)).collect();
    frames[0].save(dir.join("icon.png")).unwrap();
    let file = std::fs::File::create(dir.join("icon.ico")).unwrap();
    let encoder = image::codecs::ico::IcoEncoder::new(file);
    let frames: Vec<image::codecs::ico::IcoFrame> = frames
        .iter()
        .map(|f| image::codecs::ico::IcoFrame::as_png(f.as_raw(), f.width(), f.height(), image::ExtendedColorType::Rgba8).unwrap())
        .collect();
    encoder.encode_images(&frames).unwrap();
    println!("{}", dir.join("icon.ico").display());
}

type P = (f32, f32);

/// Distance from `p` to the segment `a`–`b`.
fn segment(p: P, a: P, b: P) -> f32 {
    let (ab, ap) = ((b.0 - a.0, b.1 - a.1), (p.0 - a.0, p.1 - a.1));
    let t = ((ap.0 * ab.0 + ap.1 * ab.1) / (ab.0 * ab.0 + ab.1 * ab.1).max(1e-6)).clamp(0., 1.);
    ((ap.0 - ab.0 * t).powi(2) + (ap.1 - ab.1 * t).powi(2)).sqrt()
}

/// Signed distance to a rounded rectangle centred on `c` with half-size `h`.
fn rounded_rect(p: P, c: P, h: P, r: f32) -> f32 {
    let q = ((p.0 - c.0).abs() - h.0 + r, (p.1 - c.1).abs() - h.1 + r);
    (q.0.max(0.).powi(2) + q.1.max(0.).powi(2)).sqrt() + q.0.max(q.1).min(0.) - r
}

/// Distance to the stand's cradle: a U whose bottom is a half circle around `c`.
fn cradle(p: P, c: P, radius: f32, arms: f32) -> f32 {
    let bowl = if p.1 >= c.1 { (((p.0 - c.0).powi(2) + (p.1 - c.1).powi(2)).sqrt() - radius).abs() } else { f32::INFINITY };
    let left = segment(p, (c.0 - radius, c.1 - arms), (c.0 - radius, c.1));
    let right = segment(p, (c.0 + radius, c.1 - arms), (c.0 + radius, c.1));
    bowl.min(left).min(right)
}

/// The microphone's signed distance in pixels. Small sizes get heavier strokes, so the shape
/// still reads at 16 px.
fn microphone(p: P, s: f32) -> f32 {
    let small = s <= 24.;
    let stroke = if small { (s * 0.105).max(1.7) } else { s * 0.068 };
    let half = stroke / 2.;
    let cx = s / 2.;
    let head_r = s * if small { 0.135 } else { 0.118 };
    let (head_top, head_bottom) = (s * 0.15, s * if small { 0.54 } else { 0.555 });
    let gap = s * if small { 0.06 } else { 0.065 };
    // The capsule, solid.
    let head = segment(p, (cx, head_top + head_r), (cx, head_bottom - head_r)) - head_r;
    let bowl_c = (cx, head_bottom - head_r);
    let bowl_r = head_r + gap + half;
    let arms = s * 0.08;
    let foot_y = s * 0.83;
    let stand = cradle(p, bowl_c, bowl_r, arms).min(segment(p, (cx, bowl_c.1 + bowl_r), (cx, foot_y))).min(segment(
        p,
        (cx - s * 0.14, foot_y),
        (cx + s * 0.14, foot_y),
    )) - half;
    head.min(stand)
}

fn draw(size: u32) -> RgbaImage {
    let s = size as f32;
    let (a, b) = ([0x5b as f32, 0x8c as f32, 0xff as f32], [0xa7 as f32, 0x8b as f32, 0xfa as f32]);
    RgbaImage::from_fn(size, size, |x, y| {
        let p = (x as f32 + 0.5, y as f32 + 0.5);
        let tile = (0.5 - rounded_rect(p, (s / 2., s / 2.), (s / 2., s / 2.), s * 0.24)).clamp(0., 1.);
        if tile == 0. {
            return Rgba([0, 0, 0, 0]);
        }
        // A diagonal gradient, blue to violet, and the microphone in white over it.
        let t = ((p.0 + p.1) / (2. * s)).clamp(0., 1.);
        let mark = (0.5 - microphone(p, s)).clamp(0., 1.);
        let c = [0, 1, 2].map(|i| {
            let base = a[i] + (b[i] - a[i]) * t;
            (base + (255. - base) * mark).round() as u8
        });
        Rgba([c[0], c[1], c[2], (tile * 255.).round() as u8])
    })
}
