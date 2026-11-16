//! GIF: LZW compression, interlaced frames, transparency and animation.

use super::{Frame, Image, checked};
use alloc::vec;
use alloc::vec::Vec;

/// Reads a chain of data sub-blocks starting at `*at`.
fn blocks(d: &[u8], at: &mut usize) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::new();
    loop {
        let n = *d.get(*at).ok_or("the picture is cut short")? as usize;
        *at += 1;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(d.get(*at..*at + n).ok_or("the picture is cut short")?);
        *at += n;
    }
}

/// Decompresses LZW into colour indices (no more than `limit` of them).
fn lzw(data: &[u8], min_bits: u32, limit: usize) -> Result<Vec<u8>, &'static str> {
    if !(2..=11).contains(&min_bits) {
        return Err("damaged picture");
    }
    let (clear, end) = (1usize << min_bits, (1usize << min_bits) + 1);
    // The dictionary: the previous code, the last symbol and the chain length.
    let mut prefix = vec![0u16; 4096];
    let mut suffix = vec![0u8; 4096];
    let mut length = vec![0u16; 4096];
    for i in 0..clear {
        suffix[i] = i as u8;
        length[i] = 1;
    }
    let mut out = Vec::with_capacity(limit);
    let (mut bits, mut next, mut previous) = (min_bits + 1, end + 1, None::<usize>);
    let (mut hold, mut count) = (0u32, 0u32);
    for &byte in data {
        hold |= (byte as u32) << count;
        count += 8;
        while count >= bits {
            let code = (hold & ((1 << bits) - 1)) as usize;
            hold >>= bits;
            count -= bits;
            if code == clear {
                (bits, next, previous) = (min_bits + 1, end + 1, None);
                continue;
            }
            if code == end || out.len() >= limit {
                return Ok(out);
            }
            let Some(old) = previous else {
                if code >= clear {
                    return Err("damaged picture");
                }
                out.push(code as u8);
                previous = Some(code);
                continue;
            };
            if code > next {
                return Err("damaged picture");
            }
            // The code is not in the dictionary yet: the chain of the previous code plus its first symbol.
            let source = if code == next { old } else { code };
            let len = length[source] as usize;
            let start = out.len();
            out.resize(start + len, 0);
            let mut c = source;
            for i in (0..len).rev() {
                out[start + i] = suffix[c];
                c = prefix[c] as usize;
            }
            let first = out[start];
            if code == next {
                out.push(first);
            }
            if next < 4096 {
                prefix[next] = old as u16;
                suffix[next] = first;
                length[next] = length[old] + 1;
                next += 1;
                if next == 1 << bits && bits < 12 {
                    bits += 1;
                }
            }
            previous = Some(code);
        }
    }
    Ok(out)
}

fn le16(d: &[u8], at: usize) -> Result<usize, &'static str> {
    let s = d.get(at..at + 2).ok_or("the picture is cut short")?;
    Ok(u16::from_le_bytes([s[0], s[1]]) as usize)
}

fn palette(d: &[u8], at: &mut usize, flags: u8) -> Result<Vec<u32>, &'static str> {
    let n = 2usize << (flags & 7);
    let table = d.get(*at..*at + n * 3).ok_or("the picture is cut short")?;
    *at += n * 3;
    Ok(table.chunks_exact(3).map(|c| 0xFF00_0000 | (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32).collect())
}

/// How much memory to give frames: otherwise a long animation would eat all of it.
const MAX_FRAME_BYTES: usize = 512 << 20;

pub fn decode(d: &[u8]) -> Result<Image, &'static str> {
    let (w, h) = (le16(d, 6)?, le16(d, 8)?);
    let count = checked(w, h)?;
    let flags = *d.get(10).ok_or("the picture is cut short")?;
    let mut at = 13;
    let global = if flags & 0x80 != 0 { palette(d, &mut at, flags)? } else { Vec::new() };
    let mut canvas = vec![0u32; count];
    let mut frames = Vec::new();
    let (mut delay, mut key, mut disposal) = (0u32, None::<usize>, 0u8);
    while let Some(&marker) = d.get(at) {
        at += 1;
        match marker {
            0x21 => {
                let label = *d.get(at).ok_or("the picture is cut short")?;
                at += 1;
                let data = blocks(d, &mut at)?;
                if label == 0xF9 && data.len() >= 4 {
                    disposal = data[0] >> 2 & 7;
                    delay = u16::from_le_bytes([data[1], data[2]]) as u32 * 10;
                    key = (data[0] & 1 != 0).then_some(data[3] as usize);
                }
            }
            0x2C => {
                let (fx, fy, fw, fh) = (le16(d, at)?, le16(d, at + 2)?, le16(d, at + 4)?, le16(d, at + 6)?);
                let local = *d.get(at + 8).ok_or("the picture is cut short")?;
                at += 9;
                let own = if local & 0x80 != 0 { palette(d, &mut at, local)? } else { Vec::new() };
                let colors = if own.is_empty() { &global } else { &own };
                let min_bits = *d.get(at).ok_or("the picture is cut short")? as u32;
                at += 1;
                let packed = blocks(d, &mut at)?;
                let indices = lzw(&packed, min_bits, fw * fh)?;
                let before = (disposal == 3).then(|| canvas.clone());
                let row_of = |i: usize| -> usize {
                    if local & 0x40 == 0 {
                        return i;
                    }
                    // An interlaced frame: four passes.
                    let (a, b, c) = (fh.div_ceil(8), (fh + 3) / 8, (fh + 1) / 4);
                    if i < a {
                        i * 8
                    } else if i < a + b {
                        (i - a) * 8 + 4
                    } else if i < a + b + c {
                        (i - a - b) * 4 + 2
                    } else {
                        (i - a - b - c) * 2 + 1
                    }
                };
                for (i, &index) in indices.iter().enumerate() {
                    let (x, y) = (fx + i % fw, fy + row_of(i / fw));
                    if x < w && y < h && Some(index as usize) != key {
                        canvas[y * w + x] = colors.get(index as usize).copied().unwrap_or(0);
                    }
                }
                // Browsers also treat a delay below 20 ms as a file error.
                frames.push(Frame { px: canvas.clone(), delay_ms: if delay < 20 { 100 } else { delay } });
                match disposal {
                    2 => {
                        for y in fy..(fy + fh).min(h) {
                            let end = (fx + fw).min(w);
                            if fx < end {
                                canvas[y * w + fx..y * w + end].fill(0);
                            }
                        }
                    }
                    3 => {
                        if let Some(old) = before {
                            canvas = old;
                        }
                    }
                    _ => {}
                }
                (delay, key, disposal) = (0, None, 0);
                if (frames.len() + 1) * count * 4 > MAX_FRAME_BYTES.min(crate::heap_stats().1 / 2) {
                    break;
                }
            }
            _ => break,
        }
    }
    if frames.is_empty() {
        return Err("damaged picture");
    }
    Ok(Image { w, h, frames, format: "GIF" })
}
