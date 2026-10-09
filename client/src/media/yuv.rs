//! The parts of libyuv the Rust bindings leave out: scaling from a borrowed picture, cropping and
//! camera formats. libwebrtc links all of libyuv (with libjpeg-turbo for MJPEG), and
//! its functions are plain C, so they are declared here and wrapped with the bounds the C side
//! trusts the caller to have checked.

use libwebrtc::prelude::VideoBuffer;
use libwebrtc::video_frame::I420Buffer;

/// libyuv's `FilterMode`: averaging, the best when scaling down and what WebRTC itself scales with.
const FILTER_BOX: i32 = 3;

unsafe extern "C" {
    fn I420Scale(
        src_y: *const u8,
        src_stride_y: i32,
        src_u: *const u8,
        src_stride_u: i32,
        src_v: *const u8,
        src_stride_v: i32,
        src_width: i32,
        src_height: i32,
        dst_y: *mut u8,
        dst_stride_y: i32,
        dst_u: *mut u8,
        dst_stride_u: i32,
        dst_v: *mut u8,
        dst_stride_v: i32,
        dst_width: i32,
        dst_height: i32,
        filtering: i32,
    ) -> i32;
    fn ARGBScale(
        src_argb: *const u8,
        src_stride_argb: i32,
        src_width: i32,
        src_height: i32,
        dst_argb: *mut u8,
        dst_stride_argb: i32,
        dst_width: i32,
        dst_height: i32,
        filtering: i32,
    ) -> i32;
    fn ConvertToI420(
        sample: *const u8,
        sample_size: usize,
        dst_y: *mut u8,
        dst_stride_y: i32,
        dst_u: *mut u8,
        dst_stride_u: i32,
        dst_v: *mut u8,
        dst_stride_v: i32,
        crop_x: i32,
        crop_y: i32,
        src_width: i32,
        src_height: i32,
        crop_width: i32,
        crop_height: i32,
        rotation: i32,
        fourcc: u32,
    ) -> i32;
    fn J420ToI420(
        src_y: *const u8,
        src_stride_y: i32,
        src_u: *const u8,
        src_stride_u: i32,
        src_v: *const u8,
        src_stride_v: i32,
        dst_y: *mut u8,
        dst_stride_y: i32,
        dst_u: *mut u8,
        dst_stride_u: i32,
        dst_v: *mut u8,
        dst_stride_v: i32,
        width: i32,
        height: i32,
    ) -> i32;
    fn MJPGToI420(
        sample: *const u8,
        sample_size: usize,
        dst_y: *mut u8,
        dst_stride_y: i32,
        dst_u: *mut u8,
        dst_stride_u: i32,
        dst_v: *mut u8,
        dst_stride_v: i32,
        src_width: i32,
        src_height: i32,
        dst_width: i32,
        dst_height: i32,
    ) -> i32;
}

/// A picture inside another: x, y, width, height, with x and y even so chroma lines up.
pub type Rect = (u32, u32, u32, u32);

/// `slot`'s buffer when it is `w` × `h`, else a new one left there for next time.
pub fn pooled(slot: &mut Option<I420Buffer>, w: u32, h: u32) -> &mut I420Buffer {
    if slot.as_ref().is_none_or(|b| (b.width(), b.height()) != (w, h)) {
        *slot = Some(I420Buffer::new(w, h));
    }
    slot.as_mut().expect("just filled")
}

fn check_rect(b: &I420Buffer, (x, y, w, h): Rect) {
    assert!(x % 2 == 0 && y % 2 == 0, "odd crop origin");
    assert!(w > 0 && h > 0 && x + w <= b.width() && y + h <= b.height(), "crop outside the picture");
}

/// `src` scaled to fill `dst`.
pub fn scale(src: &I420Buffer, dst: &mut I420Buffer) {
    scale_rect(src, (0, 0, src.width(), src.height()), dst);
}

/// Part of `src` scaled to fill `dst`: a crop and a scale in one pass (a plain copy at the same size).
pub fn scale_rect(src: &I420Buffer, rect: Rect, dst: &mut I420Buffer) {
    check_rect(src, rect);
    let (x, y, w, h) = rect;
    let (sy, su, sv) = src.strides();
    let (py, pu, pv) = src.data();
    let (py, pu, pv) = (
        &py[(y * sy + x) as usize..],
        &pu[(y / 2 * su + x / 2) as usize..],
        &pv[(y / 2 * sv + x / 2) as usize..],
    );
    let (dw, dh) = (dst.width(), dst.height());
    let (dy, du, dv) = dst.strides();
    let (qy, qu, qv) = dst.data_mut();
    // SAFETY: the rect is inside `src` and every plane is a whole buffer's, as checked above.
    let r = unsafe {
        I420Scale(
            py.as_ptr(),
            sy as i32,
            pu.as_ptr(),
            su as i32,
            pv.as_ptr(),
            sv as i32,
            w as i32,
            h as i32,
            qy.as_mut_ptr(),
            dy as i32,
            qu.as_mut_ptr(),
            du as i32,
            qv.as_mut_ptr(),
            dv as i32,
            dw as i32,
            dh as i32,
            FILTER_BOX,
        )
    };
    assert_eq!(r, 0, "I420Scale");
}

/// Where the planes of a `w` × `h` I420 picture lie when it is all one buffer: Y, then U, then
/// V, each row right after the last. Each plane's (offset, stride).
pub fn i420_layout(w: u32, h: u32) -> [(usize, usize); 3] {
    let (w, h) = (w as usize, h as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    [(0, w), (w * h, cw), (w * h + cw * ch, cw)]
}

/// The bytes a `w` × `h` I420 picture in one buffer takes (see `i420_layout`).
pub fn i420_len(w: u32, h: u32) -> usize {
    let (w, h) = (w as usize, h as usize);
    w * h + 2 * w.div_ceil(2) * h.div_ceil(2)
}

/// The Y, U and V planes of a `w` × `h` I420 picture in one buffer (see `i420_layout`).
pub fn i420_planes(buf: &mut [u8], w: u32, h: u32) -> (&mut [u8], &mut [u8], &mut [u8]) {
    let [_, (u, _), (v, _)] = i420_layout(w, h);
    let (y, rest) = buf[..i420_len(w, h)].split_at_mut(u);
    let (u, v) = rest.split_at_mut(v - u);
    (y, u, v)
}

/// `src` scaled to fill the planes of a `w` × `h` I420 picture in one buffer (see `i420_planes`).
pub fn scale_into(src: &I420Buffer, qy: &mut [u8], qu: &mut [u8], qv: &mut [u8], w: u32, h: u32) {
    let [(_, dy), (_, du), (_, dv)] = i420_layout(w, h);
    assert!(qy.len() >= dy * h as usize && qu.len().min(qv.len()) >= du * h.div_ceil(2) as usize, "short planes");
    let (sy, su, sv) = src.strides();
    let (py, pu, pv) = src.data();
    // SAFETY: `src` is a whole buffer and the planes hold a whole `w` × `h` picture, as checked.
    let r = unsafe {
        I420Scale(
            py.as_ptr(),
            sy as i32,
            pu.as_ptr(),
            su as i32,
            pv.as_ptr(),
            sv as i32,
            src.width() as i32,
            src.height() as i32,
            qy.as_mut_ptr(),
            dy as i32,
            qu.as_mut_ptr(),
            du as i32,
            qv.as_mut_ptr(),
            dv as i32,
            w as i32,
            h as i32,
            FILTER_BOX,
        )
    };
    assert_eq!(r, 0, "I420Scale");
}

/// A BGRA picture (libyuv's ARGB) scaled into `dst`, `dw` × `dh` and tightly packed.
pub fn scale_bgra(src: &[u8], stride: u32, w: u32, h: u32, dst: &mut [u8], dw: u32, dh: u32) {
    assert!(stride >= w * 4 && src.len() >= (stride * (h - 1) + w * 4) as usize, "short source");
    assert!(dst.len() >= (dw * dh * 4) as usize, "short destination");
    // SAFETY: both pictures are within their slices, as checked above.
    let r = unsafe {
        ARGBScale(src.as_ptr(), stride as i32, w as i32, h as i32, dst.as_mut_ptr(), (dw * 4) as i32, dw as i32, dh as i32, FILTER_BOX)
    };
    assert_eq!(r, 0, "ARGBScale");
}

/// Uncompressed camera formats, as libyuv names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packed {
    Nv12,
    Yuy2,
    /// B, G, R in memory.
    Rgb24,
    /// R, G, B in memory.
    Raw,
    Gray,
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    s[0] as u32 | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24
}

impl Packed {
    fn fourcc(self) -> u32 {
        match self {
            Packed::Nv12 => fourcc(b"NV12"),
            Packed::Yuy2 => fourcc(b"YUY2"),
            Packed::Rgb24 => fourcc(b"24BG"),
            Packed::Raw => fourcc(b"raw "),
            Packed::Gray => fourcc(b"I400"),
        }
    }

    /// The bytes a packed `w` × `h` frame takes.
    pub fn size(self, w: u32, h: u32) -> usize {
        let (w, h) = (w as usize, h as usize);
        match self {
            Packed::Nv12 => w * h + w.div_ceil(2) * 2 * h.div_ceil(2),
            Packed::Yuy2 => w.div_ceil(2) * 4 * h,
            Packed::Rgb24 | Packed::Raw => w * h * 3,
            Packed::Gray => w * h,
        }
    }
}

/// Part of a packed `w` × `h` frame, converted to I420 into `dst` (the size of the part).
pub fn convert(sample: &[u8], format: Packed, w: u32, h: u32, rect: Rect, dst: &mut I420Buffer) -> anyhow::Result<()> {
    let (x, y, cw, ch) = rect;
    anyhow::ensure!(sample.len() >= format.size(w, h), "short frame");
    anyhow::ensure!(x % 2 == 0 && y % 2 == 0 && x + cw <= w && y + ch <= h, "crop outside the frame");
    anyhow::ensure!((dst.width(), dst.height()) == (cw, ch), "wrong destination size");
    let (dy, du, dv) = dst.strides();
    let (qy, qu, qv) = dst.data_mut();
    // SAFETY: the frame holds a whole picture of its format, the crop is inside it and `dst` is
    // exactly the crop's size.
    let r = unsafe {
        ConvertToI420(
            sample.as_ptr(),
            sample.len(),
            qy.as_mut_ptr(),
            dy as i32,
            qu.as_mut_ptr(),
            du as i32,
            qv.as_mut_ptr(),
            dv as i32,
            x as i32,
            y as i32,
            w as i32,
            h as i32,
            cw as i32,
            ch as i32,
            0,
            format.fourcc(),
        )
    };
    anyhow::ensure!(r == 0, "libyuv could not convert a {format:?} frame ({r})");
    Ok(())
}

/// JPEG's full-range YUV (J420) into the limited range encoders and the I420 conversions expect;
/// `dst` is the same size.
pub fn narrow_range(src: &I420Buffer, dst: &mut I420Buffer) {
    let (w, h) = (src.width(), src.height());
    assert_eq!((dst.width(), dst.height()), (w, h), "range size");
    let (sy, su, sv) = src.strides();
    let (py, pu, pv) = src.data();
    let (dy, du, dv) = dst.strides();
    let (qy, qu, qv) = dst.data_mut();
    // SAFETY: both buffers are whole and the same size.
    let r = unsafe {
        J420ToI420(
            py.as_ptr(),
            sy as i32,
            pu.as_ptr(),
            su as i32,
            pv.as_ptr(),
            sv as i32,
            qy.as_mut_ptr(),
            dy as i32,
            qu.as_mut_ptr(),
            du as i32,
            qv.as_mut_ptr(),
            dv as i32,
            w as i32,
            h as i32,
        )
    };
    assert_eq!(r, 0, "J420ToI420");
}

/// A whole MJPEG frame, `w` × `h`, decoded into `dst` (the same size), in JPEG's full range: see
/// `narrow_range`.
pub fn mjpeg(sample: &[u8], w: u32, h: u32, dst: &mut I420Buffer) -> anyhow::Result<()> {
    anyhow::ensure!((dst.width(), dst.height()) == (w, h), "wrong destination size");
    let (dy, du, dv) = dst.strides();
    let (qy, qu, qv) = dst.data_mut();
    // SAFETY: libyuv reads no further than `sample.len()` and writes a `w` × `h` picture, the size
    // of `dst`.
    let r = unsafe {
        MJPGToI420(
            sample.as_ptr(),
            sample.len(),
            qy.as_mut_ptr(),
            dy as i32,
            qu.as_mut_ptr(),
            du as i32,
            qv.as_mut_ptr(),
            dv as i32,
            w as i32,
            h as i32,
            w as i32,
            h as i32,
        )
    };
    anyhow::ensure!(r == 0, "libyuv could not decode an MJPEG frame ({r})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(w: u32, h: u32, y: u8, u: u8, v: u8) -> I420Buffer {
        let mut b = I420Buffer::new(w, h);
        let (py, pu, pv) = b.data_mut();
        py.fill(y);
        pu.fill(u);
        pv.fill(v);
        b
    }

    #[test]
    fn a_crop_scales_with_its_own_content() {
        // Left half dark, right half bright: the right half's crop, shrunk, is all bright.
        let mut src = filled(64, 32, 16, 128, 128);
        let (sy, _, _) = src.strides();
        let (py, _, _) = src.data_mut();
        for row in py.chunks_exact_mut(sy as usize) {
            row[32..64].fill(235);
        }
        let mut dst = I420Buffer::new(16, 16);
        scale_rect(&src, (32, 0, 32, 32), &mut dst);
        assert!(dst.data().0.iter().all(|&p| p == 235));
    }

    #[test]
    fn scaling_fills_a_buffer_s_planes() {
        // Odd sides: chroma rounds up.
        let (w, h) = (31, 17);
        assert_eq!(i420_layout(w, h), [(0, 31), (31 * 17, 16), (31 * 17 + 16 * 9, 16)]);
        let mut buf = vec![0u8; i420_len(w, h)];
        let (y, u, v) = i420_planes(&mut buf, w, h);
        assert_eq!((y.len(), u.len(), v.len()), (31 * 17, 16 * 9, 16 * 9));
        scale_into(&filled(64, 36, 100, 90, 160), y, u, v, w, h);
        // Box filtering at an uneven ratio rounds a pixel here and there.
        let near = |plane: &[u8], value: u8| plane.iter().all(|p| p.abs_diff(value) <= 1);
        assert!(near(y, 100) && near(u, 90) && near(v, 160));
    }

    #[test]
    fn packed_frames_convert_with_their_crop() {
        // Grey: Y 128 (with chroma neutral) wherever the crop lands.
        let (w, h) = (8u32, 6u32);
        let mut nv12 = vec![128u8; Packed::Nv12.size(w, h)];
        nv12[..(w * h) as usize].fill(100);
        let mut dst = I420Buffer::new(4, 4);
        convert(&nv12, Packed::Nv12, w, h, (2, 2, 4, 4), &mut dst).unwrap();
        assert!(dst.data().0.iter().all(|&p| p == 100));
        assert!(dst.data().1.iter().all(|&p| p == 128));
        let yuy2: Vec<u8> = (0..w * h / 2).flat_map(|_| [90, 128, 90, 128]).collect();
        convert(&yuy2, Packed::Yuy2, w, h, (2, 2, 4, 4), &mut dst).unwrap();
        assert!(dst.data().0.iter().all(|&p| p == 90));
        assert!(convert(&yuy2[..10], Packed::Yuy2, w, h, (0, 0, 4, 4), &mut dst).is_err(), "short frame");
    }

    #[test]
    fn bgra_scales_down() {
        let src = [10u8, 20, 30, 255].repeat(64 * 32);
        let mut dst = vec![0u8; 16 * 8 * 4];
        scale_bgra(&src, 64 * 4, 64, 32, &mut dst, 16, 8);
        assert!(dst.chunks_exact(4).all(|p| p == [10, 20, 30, 255]));
    }
}
