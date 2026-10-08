//! Draws Harmony's icon (five bars of a sound wave on a rounded square) into
//! `assets/icon.ico` and `assets/icon.png`.
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

/// Coverage of a rounded rectangle at a pixel, antialiased by 4×4 supersampling.
fn rounded(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> f32 {
    let mut hits = 0;
    for sy in 0..4 {
        for sx in 0..4 {
            let (px, py) = (x + (sx as f32 + 0.5) / 4., y + (sy as f32 + 0.5) / 4.);
            let cx = px.max(x0 + r).min(x1 - r);
            let cy = py.max(y0 + r).min(y1 - r);
            if (px - cx).powi(2) + (py - cy).powi(2) <= r * r && px >= x0 && px <= x1 && py >= y0 && py <= y1 {
                hits += 1;
            }
        }
    }
    hits as f32 / 16.
}

fn draw(size: u32) -> RgbaImage {
    let s = size as f32;
    let (a, b) = ([0x5b as f32, 0x8c as f32, 0xff as f32], [0xa7 as f32, 0x8b as f32, 0xfa as f32]);
    // Five bars of a sound wave, centred, as in the window's own mark.
    let heights = [0.18, 0.36, 0.52, 0.36, 0.18];
    let bw = 0.085;
    let gap = 0.055;
    let start = 0.5 - (5. * bw + 4. * gap) / 2.;
    let bars: Vec<(f32, f32)> = heights.iter().enumerate().map(|(i, h)| (start + i as f32 * (bw + gap), *h)).collect();
    RgbaImage::from_fn(size, size, |x, y| {
        let (fx, fy) = (x as f32, y as f32);
        let tile = rounded(fx, fy, 0., 0., s, s, s * 0.24);
        if tile == 0. {
            return Rgba([0, 0, 0, 0]);
        }
        // A diagonal gradient, blue to violet.
        let t = ((fx + fy) / (2. * s)).clamp(0., 1.);
        let mut c = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t];
        for &(left, height) in &bars {
            let x0 = s * left;
            let (y0, y1) = (s * (0.5 - height / 2.), s * (0.5 + height / 2.));
            let cover = rounded(fx, fy, x0, y0, x0 + s * bw, y1, s * bw / 2.);
            for ch in c.iter_mut() {
                *ch = *ch + (255. - *ch) * cover;
            }
        }
        Rgba([c[0] as u8, c[1] as u8, c[2] as u8, (tile * 255.) as u8])
    })
}
