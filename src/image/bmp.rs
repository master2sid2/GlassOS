//! BMP: 1, 4, 8, 16, 24 and 32 bits per pixel, RLE4/RLE8, rows bottom-up and top-down.

use super::{Image, checked};
use alloc::vec;

fn le16(d: &[u8], at: usize) -> Option<u32> {
    Some(u16::from_le_bytes([*d.get(at)?, *d.get(at + 1)?]) as u32)
}

fn le32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

/// A channel value by its mask, stretched to 8 bits.
fn channel(v: u32, mask: u32) -> u32 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let max = mask >> shift;
    ((v & mask) >> shift) * 255 / max
}

pub fn decode(d: &[u8]) -> Result<Image, &'static str> {
    decode_inner(d).ok_or("damaged picture").and_then(|r| r)
}

fn decode_inner(d: &[u8]) -> Option<Result<Image, &'static str>> {
    let offset = le32(d, 10)? as usize;
    let header = le32(d, 14)? as usize;
    if header < 40 {
        return Some(Err("unsupported BMP"));
    }
    let (w, raw_h) = (le32(d, 18)? as i32, le32(d, 22)? as i32);
    let (bits, compression) = (le16(d, 28)? as usize, le32(d, 30)?);
    let (top_down, h) = (raw_h < 0, raw_h.unsigned_abs() as usize);
    if w <= 0 {
        return Some(Err("damaged picture"));
    }
    let w = w as usize;
    let count = match checked(w, h) {
        Ok(n) => n,
        Err(e) => return Some(Err(e)),
    };
    let mut palette = vec![0xFF00_0000u32; 256];
    if bits <= 8 {
        let used = le32(d, 46)? as usize;
        let used = if used == 0 { 1 << bits } else { used.min(256) };
        for (i, slot) in palette.iter_mut().enumerate().take(used) {
            *slot = 0xFF00_0000 | (le32(d, 14 + header + i * 4)? & 0xFF_FFFF);
        }
    }
    let mut px = vec![0xFF00_0000u32; count];
    let row_of = |y: usize| if top_down { y } else { h - 1 - y };
    if compression == 1 || compression == 2 {
        let (mut x, mut y, mut at) = (0usize, 0usize, offset);
        let put = |px: &mut [u32], x: usize, y: usize, c: u32| {
            if x < w && y < h {
                px[row_of(y) * w + x] = c;
            }
        };
        loop {
            let (n, v) = (*d.get(at)? as usize, *d.get(at + 1)?);
            at += 2;
            if n > 0 {
                for i in 0..n {
                    let index = if compression == 1 { v } else if i % 2 == 0 { v >> 4 } else { v & 15 };
                    put(&mut px, x, y, palette[index as usize]);
                    x += 1;
                }
                continue;
            }
            match v {
                0 => (x, y) = (0, y + 1),
                1 => break,
                2 => {
                    x += *d.get(at)? as usize;
                    y += *d.get(at + 1)? as usize;
                    at += 2;
                }
                n => {
                    let n = n as usize;
                    for i in 0..n {
                        let index = if compression == 1 {
                            *d.get(at + i)?
                        } else if i % 2 == 0 {
                            *d.get(at + i / 2)? >> 4
                        } else {
                            *d.get(at + i / 2)? & 15
                        };
                        put(&mut px, x, y, palette[index as usize]);
                        x += 1;
                    }
                    let bytes = if compression == 1 { n } else { n.div_ceil(2) };
                    at += bytes + bytes % 2;
                }
            }
        }
        return Some(Ok(Image::single(w, h, px, "BMP")));
    }
    if compression != 0 && compression != 3 && compression != 6 {
        return Some(Err("unsupported BMP"));
    }
    let masks = if compression != 0 {
        [le32(d, 54)?, le32(d, 58)?, le32(d, 62)?, if header >= 56 || compression == 6 { le32(d, 66)? } else { 0 }]
    } else if bits == 16 {
        [0x7C00, 0x03E0, 0x001F, 0]
    } else {
        [0xFF_0000, 0xFF00, 0xFF, 0]
    };
    let stride = (w * bits).div_ceil(32) * 4;
    for y in 0..h {
        let row = d.get(offset + y * stride..offset + (y + 1) * stride)?;
        let out = &mut px[row_of(y) * w..(row_of(y) + 1) * w];
        for (x, p) in out.iter_mut().enumerate() {
            *p = match bits {
                1 | 4 | 8 => {
                    let bit = x * bits;
                    let v = (row[bit / 8] >> (8 - bits - bit % 8)) as usize & ((1 << bits) - 1);
                    palette[v]
                }
                16 | 24 | 32 => {
                    let v = match bits {
                        16 => u16::from_le_bytes([row[2 * x], row[2 * x + 1]]) as u32,
                        24 => u32::from_le_bytes([row[3 * x], row[3 * x + 1], row[3 * x + 2], 0]),
                        _ => u32::from_le_bytes([row[4 * x], row[4 * x + 1], row[4 * x + 2], row[4 * x + 3]]),
                    };
                    let a = if masks[3] != 0 { channel(v, masks[3]) } else { 255 };
                    a << 24 | channel(v, masks[0]) << 16 | channel(v, masks[1]) << 8 | channel(v, masks[2])
                }
                _ => return Some(Err("unsupported BMP")),
            };
        }
    }
    Some(Ok(Image::single(w, h, px, "BMP")))
}
