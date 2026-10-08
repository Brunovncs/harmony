//! Draws the camera backgrounds that ship with Harmony into `assets/backgrounds/`. They are made
//! here rather than downloaded, so they carry no licence of their own and can be remade.
//!
//!     cargo run --release --example backgrounds

use image::{Rgb, RgbImage};
use std::f32::consts::{PI, TAU};

const W: u32 = 1280;
const H: u32 = 720;

fn main() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/backgrounds");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, img) in
        [("aurora", aurora()), ("dusk", dusk()), ("studio", studio()), ("ocean", ocean()), ("bokeh", bokeh()), ("pixels", pixels())]
    {
        let path = dir.join(format!("{name}.jpg"));
        let mut out = std::fs::File::create(&path).unwrap();
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 88);
        enc.encode_image(&img).unwrap();
        println!("{}", path.display());
    }
}

type C = [f32; 3];

fn hex(v: u32) -> C {
    [((v >> 16) & 255) as f32 / 255., ((v >> 8) & 255) as f32 / 255., (v & 255) as f32 / 255.]
}

fn mix(a: C, b: C, t: f32) -> C {
    let t = t.clamp(0., 1.);
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

fn add(a: C, b: C, k: f32) -> C {
    [a[0] + b[0] * k, a[1] + b[1] * k, a[2] + b[2] * k]
}

fn smooth(t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    t * t * (3. - 2. * t)
}

/// A small deterministic hash, for grain and scatter that look the same every time.
fn rand(i: u32) -> f32 {
    let mut x = i.wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
    x ^= x >> 16;
    x = x.wrapping_mul(0x7FEB_352D);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846C_A68B);
    x ^= x >> 16;
    (x as f32) / (u32::MAX as f32)
}

fn noise(x: f32, y: f32, seed: u32) -> f32 {
    // Value noise, bilinear between hashed lattice points.
    let (xi, yi) = (x.floor() as i32, y.floor() as i32);
    let (fx, fy) = (smooth(x - xi as f32), smooth(y - yi as f32));
    let h = |a: i32, b: i32| rand((a as u32).wrapping_mul(73_856_093) ^ (b as u32).wrapping_mul(19_349_663) ^ seed);
    let top = h(xi, yi) + (h(xi + 1, yi) - h(xi, yi)) * fx;
    let bottom = h(xi, yi + 1) + (h(xi + 1, yi + 1) - h(xi, yi + 1)) * fx;
    top + (bottom - top) * fy
}

fn fbm(x: f32, y: f32, seed: u32) -> f32 {
    let (mut v, mut amp, mut f) = (0., 0.5, 1.);
    for o in 0..5 {
        v += noise(x * f, y * f, seed + o) * amp;
        amp *= 0.5;
        f *= 2.;
    }
    v
}

fn finish(px: impl Fn(f32, f32) -> C) -> RgbImage {
    RgbImage::from_fn(W, H, |x, y| {
        let (u, v) = (x as f32 / W as f32, y as f32 / H as f32);
        let c = px(u, v);
        // A touch of grain keeps gradients from banding once the camera encoder has had them.
        let g = (rand(x * 7919 + y * 104_729) - 0.5) * 0.018;
        Rgb([0, 1, 2].map(|i| ((c[i] + g).clamp(0., 1.) * 255.).round() as u8))
    })
}

fn vignette(c: C, u: f32, v: f32, k: f32) -> C {
    let d = ((u - 0.5).powi(2) + (v - 0.5).powi(2)).sqrt();
    let f = 1. - smooth((d - 0.35) / 0.55) * k;
    [c[0] * f, c[1] * f, c[2] * f]
}

/// Night sky with ribbons of green and violet light.
fn aurora() -> RgbImage {
    finish(|u, v| {
        let mut c = mix(hex(0x070a17), hex(0x111a33), v);
        for (seed, col, base, amp) in [(3, hex(0x3fe0a0), 0.34, 0.09), (11, hex(0x8b6cff), 0.46, 0.07), (17, hex(0x38bdf8), 0.28, 0.05)] {
            let wave = base + (u * TAU * 0.9 + seed as f32).sin() * amp + (fbm(u * 3., 0.5, seed) - 0.5) * 0.12;
            let d = v - wave;
            // A bright lower edge fading upward, like curtains.
            let curtain = if d < 0. { (-(d * d) / 0.012).exp() } else { (-(d * d) / 0.0012).exp() };
            let streak = 0.6 + 0.4 * fbm(u * 40., v * 2., seed + 5);
            c = add(c, col, curtain * streak * 0.55);
        }
        // Stars.
        let s = rand(((u * 900.) as u32) * 4099 + (v * 520.) as u32);
        if s > 0.9965 && v < 0.75 {
            c = add(c, [1., 1., 1.], (s - 0.9965) * 220.);
        }
        let ground = smooth((v - 0.86) / 0.1);
        mix(vignette(c, u, v, 0.35), hex(0x04060c), ground * (0.7 + 0.3 * fbm(u * 6., 0., 9)))
    })
}

/// A warm evening sky over hills, the sun low and soft.
fn dusk() -> RgbImage {
    finish(|u, v| {
        let sky = if v < 0.55 {
            mix(hex(0x2a1b4d), hex(0xf28c5a), smooth(v / 0.55))
        } else {
            mix(hex(0xf28c5a), hex(0xffd29a), (v - 0.55) / 0.1)
        };
        let sun = (-(((u - 0.68) * 1.78).powi(2) + (v - 0.58).powi(2)) / 0.006).exp();
        let glow = (-(((u - 0.68) * 1.78).powi(2) + (v - 0.58).powi(2)) / 0.08).exp();
        let mut c = add(add(sky, hex(0xfff1c7), sun * 0.9), hex(0xffb36b), glow * 0.35);
        for (i, (h, col)) in [(0.66, hex(0x5a2f55)), (0.74, hex(0x3b2147)), (0.83, hex(0x24152f))].into_iter().enumerate() {
            let ridge = h + (fbm(u * 2.5, i as f32, 31 + i as u32) - 0.5) * 0.12;
            if v > ridge {
                c = mix(c, col, 0.9 + 0.1 * smooth((v - ridge) / 0.2));
            }
        }
        vignette(c, u, v, 0.4)
    })
}

/// A photographer's grey backdrop, lit softly from above left.
fn studio() -> RgbImage {
    finish(|u, v| {
        let light = (-(((u - 0.38) * 1.4).powi(2) + (v - 0.32).powi(2)) / 0.22).exp();
        let c = mix(hex(0x2b2e33), hex(0x8a9099), light);
        let cloth = (fbm(u * 5., v * 3., 41) - 0.5) * 0.06;
        vignette(add(c, [1., 1., 1.], cloth), u, v, 0.55)
    })
}

/// Deep water with light falling through it.
fn ocean() -> RgbImage {
    finish(|u, v| {
        let mut c = mix(hex(0x1b6f9c), hex(0x061a33), smooth(v * 1.1));
        let caustic = (fbm(u * 9. + v * 3., v * 6., 51) - 0.5).abs();
        c = add(c, hex(0x7fe3ff), (0.08 - caustic).max(0.) * 2.2 * (1. - v));
        for k in 0..6 {
            let x = 0.1 + k as f32 * 0.17 + (k as f32 * 1.7).sin() * 0.04;
            let ray = (-((u - x - v * 0.18).powi(2)) / 0.0016).exp() * (1. - v).powf(1.5);
            c = add(c, hex(0xbff3ff), ray * 0.16);
        }
        vignette(c, u, v, 0.45)
    })
}

/// Out-of-focus lights in a dark room.
fn bokeh() -> RgbImage {
    let lights: Vec<(f32, f32, f32, C, f32)> = (0..70)
        .map(|i| {
            let palette = [hex(0xffb547), hex(0xff7aa8), hex(0x8b6cff), hex(0x5cc2ff), hex(0xffe3a3)];
            (
                rand(i * 3 + 1),
                rand(i * 3 + 2) * 0.9,
                0.02 + rand(i * 5 + 7) * 0.06,
                palette[i as usize % palette.len()],
                0.15 + rand(i * 11) * 0.35,
            )
        })
        .collect();
    finish(move |u, v| {
        let mut c = mix(hex(0x14101f), hex(0x07060c), v);
        for &(x, y, r, col, a) in &lights {
            let d = (((u - x) * 1.78).powi(2) + (v - y).powi(2)).sqrt();
            // A disc with a slightly brighter rim, as lens bokeh has.
            let disc = 1. - smooth((d - r) / (r * 0.12));
            let rim = (-((d - r * 0.92).powi(2)) / (r * r * 0.004)).exp() * 0.4;
            c = add(c, col, (disc * 0.55 + rim) * a);
        }
        vignette(c, u, v, 0.5)
    })
}

/// A field of soft pixels in Harmony's violet, a nod to Texel.
fn pixels() -> RgbImage {
    finish(|u, v| {
        let cells = 40.;
        let (cx, cy) = ((u * cells).floor(), (v * cells * 9. / 16.).floor());
        let id = (cx as u32) * 131 + cy as u32 * 977;
        let n = fbm(cx / 9., cy / 9., 61);
        let base = mix(hex(0x0f0b1e), hex(0x2a1f55), n);
        let lit = if rand(id) > 0.93 { 0.5 + rand(id + 1) * 0.5 } else { 0. };
        let col = [hex(0xa78bfa), hex(0x7cbd3a), hex(0x5cc2ff), hex(0xffb547)][(rand(id + 2) * 4.) as usize % 4];
        let (fx, fy) = ((u * cells).fract(), (v * cells * 9. / 16.).fract());
        let inset = (fx > 0.08 && fx < 0.92 && fy > 0.08 && fy < 0.92) as u8 as f32;
        let c = add(base, col, lit * inset * 0.55);
        let pulse = 0.5 + 0.5 * ((u - 0.5) * PI).cos() * ((v - 0.5) * PI).cos();
        vignette(mix(c, base, 0.3 * (1. - pulse)), u, v, 0.5)
    })
}
