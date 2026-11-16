//! Reading pictures: PNG, JPEG, GIF and BMP. The format is detected by the file contents.

mod bmp;
mod gif;
mod jpeg;
mod png;

use alloc::vec::Vec;

/// Larger than this (in pixels) is not opened: a file may claim any size.
pub const MAX_PIXELS: usize = 200_000_000;

/// A frame: 0xAARRGGBB pixels row by row and how long to show it.
pub struct Frame {
    pub px: Vec<u32>,
    pub delay_ms: u32,
}

/// A finished picture. All except animations have one frame.
pub struct Image {
    pub w: usize,
    pub h: usize,
    pub frames: Vec<Frame>,
    /// The format name for the information line.
    pub format: &'static str,
}

impl Image {
    fn single(w: usize, h: usize, px: Vec<u32>, format: &'static str) -> Self {
        Self { w, h, frames: alloc::vec![Frame { px, delay_ms: 0 }], format }
    }
}

/// How many bytes per pixel decoding takes: the pixels and the working buffers.
const BYTES_PER_PIXEL: usize = 12;

/// Checks the claimed picture size: running out of memory would stop the whole system.
fn checked(w: usize, h: usize) -> Result<usize, &'static str> {
    match w.checked_mul(h) {
        Some(n) if n > 0 && n <= MAX_PIXELS => {
            if n * BYTES_PER_PIXEL > crate::heap_stats().1 {
                return Err("not enough memory for this picture");
            }
            Ok(n)
        }
        _ => Err("the picture is too large"),
    }
}

/// Whether it is a picture, judging by the file name.
pub fn is_picture(name: &str) -> bool {
    let lower = name.to_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".bmp"].iter().any(|e| lower.ends_with(e))
}

pub fn decode(data: &[u8]) -> Result<Image, &'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        png::decode(data)
    } else if data.starts_with(&[0xFF, 0xD8]) {
        jpeg::decode(data)
    } else if data.starts_with(b"GIF8") {
        gif::decode(data)
    } else if data.starts_with(b"BM") {
        bmp::decode(data)
    } else {
        Err("not a picture this system can read")
    }
}

/// A `sw`×`sh` picture (0xAARRGGBB) → `tw`×`th` for display: when shrinking each pixel is
/// the average of its area (no "staircase"), when enlarging — the nearest one. Transparent parts
/// are blended with the background `bg`.
pub fn resize(src: &[u32], sw: usize, sh: usize, tw: usize, th: usize, bg: u32) -> Vec<u32> {
    let mut out = Vec::with_capacity(tw * th);
    if sw == 0 || sh == 0 || src.len() < sw * sh {
        out.resize(tw * th, bg);
        return out;
    }
    for ty in 0..th {
        let y0 = ty * sh / th;
        let y1 = ((ty + 1) * sh / th).max(y0 + 1).min(sh);
        for tx in 0..tw {
            let x0 = tx * sw / tw;
            let x1 = ((tx + 1) * sw / tw).max(x0 + 1).min(sw);
            let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
            for y in y0..y1 {
                for &p in &src[y * sw + x0..y * sw + x1] {
                    let a = p >> 24;
                    let mix = |s: u32, d: u32| (s * a + d * (255 - a)) / 255;
                    r += mix(p >> 16 & 0xFF, bg >> 16 & 0xFF);
                    g += mix(p >> 8 & 0xFF, bg >> 8 & 0xFF);
                    b += mix(p & 0xFF, bg & 0xFF);
                    n += 1;
                }
            }
            out.push(0xFF00_0000 | (r / n) << 16 | (g / n) << 8 | b / n);
        }
    }
    out
}
