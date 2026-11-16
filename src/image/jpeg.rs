//! JPEG: baseline and progressive, with any chroma subsampling, greyscale, CMYK.
//! The coefficients of all scans accumulate in memory, the picture is assembled at the end,
//! so both kinds take one path. The photo orientation is taken from EXIF.

use super::{Image, checked};
use alloc::vec;
use alloc::vec::Vec;

const ZIGZAG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29,
    22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// The Huffman table decodes this many bits at one glance.
const FAST_BITS: u32 = 9;

#[derive(Clone)]
struct Huffman {
    /// By the first FAST_BITS bits: code length << 8 | symbol; 0 — the code is longer.
    fast: Vec<u16>,
    /// The largest code of each length (left-aligned to 16 bits) and the offset to the symbol.
    max: [u32; 18],
    delta: [i32; 17],
    values: Vec<u8>,
}

impl Huffman {
    fn new(counts: &[u8], values: &[u8]) -> Self {
        let mut fast = vec![0u16; 1 << FAST_BITS];
        let (mut max, mut delta) = ([0u32; 18], [0i32; 17]);
        let (mut code, mut k) = (0u32, 0usize);
        for len in 1..=16u32 {
            delta[len as usize] = k as i32 - code as i32;
            for _ in 0..counts[len as usize - 1] {
                if len <= FAST_BITS && k < values.len() {
                    let first = (code << (FAST_BITS - len)) as usize;
                    for slot in fast.iter_mut().skip(first).take(1 << (FAST_BITS - len)) {
                        *slot = (len as u16) << 8 | values[k] as u16;
                    }
                }
                code += 1;
                k += 1;
            }
            max[len as usize] = code << (16 - len);
            code <<= 1;
        }
        max[17] = u32::MAX;
        Self { fast, max, delta, values: values.to_vec() }
    }
}

/// Bits of the compressed data: removes stuffed zeros, stops at the next marker.
struct Reader<'a> {
    d: &'a [u8],
    at: usize,
    hold: u32,
    count: u32,
}

impl Reader<'_> {
    fn fill(&mut self) {
        while self.count <= 24 {
            let mut byte = 0;
            if let Some(&b) = self.d.get(self.at) {
                if b != 0xFF {
                    byte = b;
                    self.at += 1;
                } else if self.d.get(self.at + 1) == Some(&0) {
                    byte = 0xFF;
                    self.at += 2;
                }
                // Otherwise it is a marker: zeros follow, scans of truncated files are read to the end.
            }
            self.hold |= (byte as u32) << (24 - self.count);
            self.count += 8;
        }
    }

    fn bit(&mut self) -> u32 {
        self.take(1)
    }

    fn take(&mut self, n: u32) -> u32 {
        // In a corrupted file a table may promise any number of bits.
        let n = n.min(16);
        if n == 0 {
            return 0;
        }
        if self.count < n {
            self.fill();
        }
        let v = self.hold >> (32 - n);
        self.hold <<= n;
        self.count -= n;
        v
    }

    fn symbol(&mut self, h: &Huffman) -> u8 {
        if self.count < 16 {
            self.fill();
        }
        let hit = h.fast[(self.hold >> (32 - FAST_BITS)) as usize];
        if hit != 0 {
            let len = (hit >> 8) as u32;
            self.hold <<= len;
            self.count -= len;
            return hit as u8;
        }
        let top = self.hold >> 16;
        let mut len = FAST_BITS as usize + 1;
        while len < 17 && top >= h.max[len] {
            len += 1;
        }
        if len == 17 {
            self.hold <<= 16;
            self.count -= 16;
            return 0;
        }
        let code = (self.hold >> (32 - len)) as i32;
        self.hold <<= len;
        self.count -= len as u32;
        h.values.get((code + h.delta[len]) as usize).copied().unwrap_or(0)
    }

    /// A signed value from `n` bits by the JPEG rule.
    fn signed(&mut self, n: u32) -> i32 {
        let n = n.min(16);
        if n == 0 {
            return 0;
        }
        let v = self.take(n) as i32;
        if v < 1 << (n - 1) { v - (1 << n) + 1 } else { v }
    }

    /// The start of the next interval: skips the RSTn marker.
    fn restart(&mut self) {
        (self.hold, self.count) = (0, 0);
        while self.at + 1 < self.d.len() {
            if self.d[self.at] == 0xFF && (0xD0..=0xD7).contains(&self.d[self.at + 1]) {
                self.at += 2;
                return;
            }
            if self.d[self.at] == 0xFF && self.d[self.at + 1] != 0 {
                return;
            }
            self.at += 1;
        }
    }
}

struct Component {
    id: u8,
    h: usize,
    v: usize,
    quant: usize,
    /// Blocks across with a margin to a whole MCU, and how many are visible in a single-component scan.
    stride: usize,
    rows: usize,
    seen: (usize, usize),
    coefs: Vec<i16>,
    pred: i32,
    dc: usize,
    ac: usize,
}

/// Scan parameters: the coefficient range and the precision bits.
#[derive(Clone, Copy)]
struct Scan {
    start: usize,
    end: usize,
    high: u32,
    low: u32,
}

/// One block of a baseline JPEG.
fn block_baseline(r: &mut Reader, b: &mut [i16], pred: &mut i32, dc: &Huffman, ac: &Huffman) {
    let t = r.symbol(dc) as u32;
    *pred = pred.wrapping_add(r.signed(t));
    b[0] = *pred as i16;
    let mut k = 1;
    while k < 64 {
        let rs = r.symbol(ac);
        let (run, size) = ((rs >> 4) as usize, (rs & 15) as u32);
        if size == 0 {
            if run != 15 {
                return;
            }
            k += 16;
            continue;
        }
        k += run;
        if k > 63 {
            return;
        }
        b[ZIGZAG[k] as usize] = r.signed(size) as i16;
        k += 1;
    }
}

/// The DC coefficient of a progressive scan.
fn block_dc(r: &mut Reader, b: &mut [i16], pred: &mut i32, dc: &Huffman, scan: Scan) {
    if scan.high == 0 {
        let t = r.symbol(dc) as u32;
        *pred = pred.wrapping_add(r.signed(t));
        b[0] = (*pred << scan.low) as i16;
    } else if r.bit() != 0 {
        b[0] |= 1 << scan.low;
    }
}

/// The first pass of the AC coefficients.
fn block_ac_first(r: &mut Reader, b: &mut [i16], ac: &Huffman, scan: Scan, eob: &mut u32) {
    if *eob > 0 {
        *eob -= 1;
        return;
    }
    let mut k = scan.start;
    while k <= scan.end {
        let rs = r.symbol(ac);
        let (run, size) = ((rs >> 4) as u32, (rs & 15) as u32);
        if size == 0 {
            if run < 15 {
                *eob = (1 << run) - 1 + r.take(run);
                return;
            }
            k += 16;
            continue;
        }
        k += run as usize;
        if k > 63 {
            return;
        }
        b[ZIGZAG[k] as usize] = (r.signed(size) << scan.low) as i16;
        k += 1;
    }
}

/// Refines an already non-zero coefficient by one bit.
fn refine(r: &mut Reader, c: &mut i16, bit: i16) {
    if r.bit() != 0 && *c & bit == 0 {
        *c = c.wrapping_add(if *c >= 0 { bit } else { bit.wrapping_neg() });
    }
}

/// The refinement pass of the AC coefficients.
fn block_ac_refine(r: &mut Reader, b: &mut [i16], ac: &Huffman, scan: Scan, eob: &mut u32) {
    let bit = 1i16 << scan.low;
    let mut k = scan.start;
    if *eob == 0 {
        while k <= scan.end {
            let rs = r.symbol(ac);
            let (mut run, size) = ((rs >> 4) as i32, rs & 15);
            let mut value = 0;
            if size == 0 {
                if run < 15 {
                    *eob = (1 << run) + r.take(run as u32);
                    break;
                }
            } else {
                value = if r.bit() != 0 { bit } else { bit.wrapping_neg() };
            }
            // Skip `run` zeros, refining the non-zero ones on the way.
            while k <= scan.end {
                let c = &mut b[ZIGZAG[k] as usize];
                if *c != 0 {
                    refine(r, c, bit);
                } else {
                    if run == 0 {
                        break;
                    }
                    run -= 1;
                }
                k += 1;
            }
            if value != 0 && k <= scan.end {
                b[ZIGZAG[k] as usize] = value;
            }
            k += 1;
        }
    }
    if *eob > 0 {
        while k <= scan.end {
            let c = &mut b[ZIGZAG[k] as usize];
            if *c != 0 {
                refine(r, c, bit);
            }
            k += 1;
        }
        *eob -= 1;
    }
}

struct Decoder {
    w: usize,
    h: usize,
    progressive: bool,
    hmax: usize,
    vmax: usize,
    parts: Vec<Component>,
    quant: [[u16; 64]; 4],
    dc: [Option<Huffman>; 4],
    ac: [Option<Huffman>; 4],
    interval: usize,
    adobe: Option<u8>,
    orientation: u8,
}

impl Decoder {
    /// Parses one scan; returns where the compressed data ended.
    fn scan(&mut self, d: &[u8], at: usize, list: &[usize], scan: Scan) -> Result<usize, &'static str> {
        let mut r = Reader { d, at, hold: 0, count: 0 };
        let (mcus_x, mcus_y) = (self.w.div_ceil(8 * self.hmax), self.h.div_ceil(8 * self.vmax));
        let (units, across) = if list.len() == 1 {
            let (w, h) = self.parts[list[0]].seen;
            (w * h, w)
        } else {
            (mcus_x * mcus_y, mcus_x)
        };
        let (progressive, interval) = (self.progressive, self.interval);
        let mut eob = 0u32;
        let (dc_tables, ac_tables) = (&self.dc, &self.ac);
        let empty = Huffman::new(&[0; 16], &[]);
        for part in &mut self.parts {
            part.pred = 0;
        }
        for unit in 0..units {
            if interval != 0 && unit != 0 && unit % interval == 0 {
                r.restart();
                eob = 0;
                for part in &mut self.parts {
                    part.pred = 0;
                }
            }
            let (ux, uy) = (unit % across, unit / across);
            for &i in list {
                let p = &mut self.parts[i];
                let (dc, ac) = (dc_tables[p.dc].as_ref().unwrap_or(&empty), ac_tables[p.ac].as_ref().unwrap_or(&empty));
                let (nh, nv) = if list.len() == 1 { (1, 1) } else { (p.h, p.v) };
                for v in 0..nv {
                    for h in 0..nh {
                        let (bx, by) = (ux * nh + h, uy * nv + v);
                        let at = (by * p.stride + bx) * 64;
                        let b = &mut p.coefs[at..at + 64];
                        if !progressive {
                            block_baseline(&mut r, b, &mut p.pred, dc, ac);
                        } else if scan.start == 0 {
                            block_dc(&mut r, b, &mut p.pred, dc, scan);
                        } else if scan.high == 0 {
                            block_ac_first(&mut r, b, ac, scan, &mut eob);
                        } else {
                            block_ac_refine(&mut r, b, ac, scan, &mut eob);
                        }
                    }
                }
            }
        }
        Ok(r.at)
    }

    /// A component plane: samples after the inverse transform.
    fn plane(&self, p: &Component) -> Vec<u8> {
        let width = p.stride * 8;
        let mut out = vec![0u8; width * p.rows * 8];
        let table = &self.quant[p.quant];
        for by in 0..p.rows {
            for bx in 0..p.stride {
                let c = &p.coefs[(by * p.stride + bx) * 64..][..64];
                idct(c, table, &mut out[(by * 8) * width + bx * 8..], width);
            }
        }
        out
    }

    /// The plane at full resolution and its line stride. Colour is usually stored more sparsely than brightness:
    /// it is stretched smoothly, otherwise steps are visible at colour borders.
    fn full_plane(&self, p: &Component) -> (Vec<u8>, usize) {
        let plane = self.plane(p);
        let stride = p.stride * 8;
        if p.h == self.hmax && p.v == self.vmax {
            return (plane, stride);
        }
        let (w, h) = (self.w, self.h);
        let (cw, ch) = ((w * p.h).div_ceil(self.hmax), (h * p.v).div_ceil(self.vmax));
        // The position of the pixel centre in chroma samples, in 1/256.
        let place = |i: usize, part: usize, full: usize, last: usize| {
            let at = ((2 * i + 1) * 256 * part / (2 * full)) as i32 - 128;
            let at = at.clamp(0, (last as i32) << 8);
            ((at >> 8) as usize, at & 255)
        };
        let columns: Vec<(usize, i32)> = (0..w).map(|x| place(x, p.h, self.hmax, cw - 1)).collect();
        let mut out = vec![0u8; w * h];
        for (y, row) in out.chunks_exact_mut(w).enumerate() {
            let (i, fy) = place(y, p.v, self.vmax, ch - 1);
            let (top, bottom) = (&plane[i * stride..], &plane[(i + 1).min(ch - 1) * stride..]);
            for (d, &(j, fx)) in row.iter_mut().zip(&columns) {
                let next = (j + 1).min(cw - 1);
                let a = top[j] as i32 * (256 - fx) + top[next] as i32 * fx;
                let b = bottom[j] as i32 * (256 - fx) + bottom[next] as i32 * fx;
                *d = ((a * (256 - fy) + b * fy + (1 << 15)) >> 16) as u8;
            }
        }
        (out, w)
    }

    fn finish(self) -> Result<Image, &'static str> {
        let (w, h) = (self.w, self.h);
        let planes: Vec<(Vec<u8>, usize)> = self.parts.iter().map(|p| self.full_plane(p)).collect();
        let n = self.parts.len();
        let ids: Vec<u8> = self.parts.iter().map(|p| p.id).collect();
        let rgb_ids = n == 3 && ids == [b'R', b'G', b'B'];
        // Without an Adobe marker three channels are YCbCr, and four are CMYK.
        let ycc = match self.adobe {
            Some(t) => t != 0,
            None => n == 3 && !rgb_ids,
        };
        let mut px = vec![0u32; checked(w, h)?];
        let clamp = |v: i32| v.clamp(0, 255) as u32;
        for y in 0..h {
            for x in 0..w {
                let sample = |i: usize| planes[i].0[y * planes[i].1 + x] as i32;
                let (r, g, b) = if n == 1 {
                    let v = sample(0);
                    (v, v, v)
                } else {
                    let (a, b, c) = (sample(0), sample(1), sample(2));
                    let (r, g, bl) = if ycc {
                        let (cb, cr) = (b - 128, c - 128);
                        (a + (91881 * cr + 32768 >> 16), a - (22554 * cb + 46802 * cr - 32768 >> 16), a + (116130 * cb + 32768 >> 16))
                    } else {
                        (a, b, c)
                    };
                    if n == 4 {
                        // In Adobe files the values are inverted.
                        let k = sample(3);
                        let ink = |v: i32| (v.clamp(0, 255) * k + 127) / 255;
                        (ink(r), ink(g), ink(bl))
                    } else {
                        (r, g, bl)
                    }
                };
                px[y * w + x] = 0xFF00_0000 | clamp(r) << 16 | clamp(g) << 8 | clamp(b);
            }
        }
        let (px, w, h) = oriented(px, w, h, self.orientation);
        Ok(Image::single(w, h, px, "JPEG"))
    }
}

/// Rotates the photo the way the camera recorded it (the values of the EXIF Orientation tag).
fn oriented(px: Vec<u32>, w: usize, h: usize, orientation: u8) -> (Vec<u32>, usize, usize) {
    if !(2..=8).contains(&orientation) {
        return (px, w, h);
    }
    let swap = orientation >= 5;
    let (nw, nh) = if swap { (h, w) } else { (w, h) };
    let mut out = vec![0u32; px.len()];
    for y in 0..h {
        for x in 0..w {
            let (mx, my) = (w - 1 - x, h - 1 - y);
            let (nx, ny) = match orientation {
                2 => (mx, y),
                3 => (mx, my),
                4 => (x, my),
                5 => (y, x),
                6 => (my, x),
                7 => (my, mx),
                _ => (y, mx),
            };
            out[ny * nw + nx] = px[y * w + x];
        }
    }
    (out, nw, nh)
}

/// The Orientation value from the EXIF segment.
fn exif_orientation(s: &[u8]) -> Option<u8> {
    let t = s.strip_prefix(b"Exif\0\0")?;
    let le = match t.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |at: usize| -> Option<usize> {
        let b = [*t.get(at)?, *t.get(at + 1)?];
        Some(if le { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) } as usize)
    };
    let u32_at = |at: usize| -> Option<usize> {
        let b: [u8; 4] = t.get(at..at + 4)?.try_into().ok()?;
        Some(if le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) } as usize)
    };
    let ifd = u32_at(4)?;
    for i in 0..u16_at(ifd)? {
        let entry = ifd + 2 + i * 12;
        if u16_at(entry)? == 0x0112 {
            return Some(u16_at(entry + 8)? as u8);
        }
    }
    None
}

/// The inverse cosine transform of a block into samples 0..255 (integer, 12 fraction bits).
fn idct(c: &[i16], q: &[u16; 64], out: &mut [u8], stride: usize) {
    #[inline(always)]
    fn pass(s: [i64; 8]) -> ([i64; 4], [i64; 4]) {
        let p1 = (s[2] + s[6]) * 2217;
        let (t2, t3) = (p1 + s[6] * -7568, p1 + s[2] * 3135);
        let (t0, t1) = ((s[0] + s[4]) << 12, (s[0] - s[4]) << 12);
        let x = [t0 + t3, t1 + t2, t1 - t2, t0 - t3];
        let (a, b, c, d) = (s[7], s[5], s[3], s[1]);
        let (p3, p4, p1, p2) = (a + c, b + d, a + d, b + c);
        let p5 = (p3 + p4) * 4816;
        let (p1, p2, p3, p4) = (p5 + p1 * -3685, p5 + p2 * -10497, p3 * -8034, p4 * -1597);
        let t = [a * 1223 + p1 + p3, b * 8409 + p2 + p4, c * 12586 + p2 + p3, d * 6149 + p1 + p4];
        (x, t)
    }
    // 64 bits: the coefficients of a corrupted file would not fit in 32.
    let mut w = [0i64; 64];
    for i in 0..8 {
        let s: [i64; 8] = core::array::from_fn(|k| c[k * 8 + i] as i64 * q[k * 8 + i] as i64);
        let (x, t) = pass(s);
        let x = x.map(|v| v + 512);
        w[i] = (x[0] + t[3]) >> 10;
        w[56 + i] = (x[0] - t[3]) >> 10;
        w[8 + i] = (x[1] + t[2]) >> 10;
        w[48 + i] = (x[1] - t[2]) >> 10;
        w[16 + i] = (x[2] + t[1]) >> 10;
        w[40 + i] = (x[2] - t[1]) >> 10;
        w[24 + i] = (x[3] + t[0]) >> 10;
        w[32 + i] = (x[3] - t[0]) >> 10;
    }
    for y in 0..8 {
        let s: [i64; 8] = core::array::from_fn(|k| w[y * 8 + k]);
        let (x, t) = pass(s);
        let x = x.map(|v| v + 65536 + (128 << 17));
        let row = &mut out[y * stride..y * stride + 8];
        let v = [x[0] + t[3], x[1] + t[2], x[2] + t[1], x[3] + t[0], x[3] - t[0], x[2] - t[1], x[1] - t[2], x[0] - t[3]];
        for (d, v) in row.iter_mut().zip(v) {
            *d = (v >> 17).clamp(0, 255) as u8;
        }
    }
}

fn be16(d: &[u8], at: usize) -> Result<usize, &'static str> {
    let s = d.get(at..at + 2).ok_or("the picture is cut short")?;
    Ok(u16::from_be_bytes([s[0], s[1]]) as usize)
}

pub fn decode(d: &[u8]) -> Result<Image, &'static str> {
    let mut j = Decoder {
        w: 0,
        h: 0,
        progressive: false,
        hmax: 1,
        vmax: 1,
        parts: Vec::new(),
        quant: [[1; 64]; 4],
        dc: [None, None, None, None],
        ac: [None, None, None, None],
        interval: 0,
        adobe: None,
        orientation: 1,
    };
    let mut at = 2;
    let mut scans = 0;
    while at + 1 < d.len() {
        if d[at] != 0xFF {
            at += 1;
            continue;
        }
        let marker = d[at + 1];
        if marker == 0xFF || marker == 0 || (0xD0..=0xD8).contains(&marker) {
            at += if marker == 0xFF { 1 } else { 2 };
            continue;
        }
        if marker == 0xD9 {
            break;
        }
        let len = be16(d, at + 2)?;
        let Some(s) = d.get(at + 4..at + 2 + len.max(2)) else { break };
        at += 2 + len;
        match marker {
            0xC0..=0xC2 => {
                j.progressive = marker == 0xC2;
                if s.len() < 6 || s[0] != 8 {
                    return Err("unsupported JPEG");
                }
                (j.h, j.w) = (be16(s, 1)?, be16(s, 3)?);
                checked(j.w, j.h)?;
                let n = s[5] as usize;
                if !matches!(n, 1 | 3 | 4) || s.len() < 6 + n * 3 {
                    return Err("unsupported JPEG");
                }
                let list: Vec<(u8, usize, usize, usize)> = (0..n).map(|i| (s[6 + i * 3], (s[7 + i * 3] >> 4) as usize, (s[7 + i * 3] & 15) as usize, (s[8 + i * 3] & 3) as usize)).collect();
                j.hmax = list.iter().map(|c| c.1).max().unwrap_or(1);
                j.vmax = list.iter().map(|c| c.2).max().unwrap_or(1);
                if list.iter().any(|c| !(1..=4).contains(&c.1) || !(1..=4).contains(&c.2)) {
                    return Err("damaged picture");
                }
                let (mcus_x, mcus_y) = (j.w.div_ceil(8 * j.hmax), j.h.div_ceil(8 * j.vmax));
                j.parts = list
                    .iter()
                    .map(|&(id, h, v, quant)| {
                        let (stride, rows) = (mcus_x * h, mcus_y * v);
                        let seen = ((j.w * h).div_ceil(j.hmax).div_ceil(8), (j.h * v).div_ceil(j.vmax).div_ceil(8));
                        Component { id, h, v, quant, stride, rows, seen, coefs: vec![0i16; stride * rows * 64], pred: 0, dc: 0, ac: 0 }
                    })
                    .collect();
            }
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => return Err("unsupported JPEG"),
            0xC4 => {
                let mut p = 0;
                while p + 17 <= s.len() {
                    let (class, slot) = (s[p] >> 4, (s[p] & 3) as usize);
                    let counts = &s[p + 1..p + 17];
                    let total: usize = counts.iter().map(|&c| c as usize).sum();
                    let values = s.get(p + 17..p + 17 + total).ok_or("damaged picture")?;
                    let table = Huffman::new(counts, values);
                    if class == 0 {
                        j.dc[slot] = Some(table);
                    } else {
                        j.ac[slot] = Some(table);
                    }
                    p += 17 + total;
                }
            }
            0xDB => {
                let mut p = 0;
                while p < s.len() {
                    let (wide, slot) = (s[p] >> 4 != 0, (s[p] & 3) as usize);
                    p += 1;
                    for &z in &ZIGZAG {
                        j.quant[slot][z as usize] = if wide {
                            let v = be16(s, p)? as u16;
                            p += 2;
                            v
                        } else {
                            let v = *s.get(p).ok_or("damaged picture")? as u16;
                            p += 1;
                            v
                        };
                    }
                }
            }
            0xDD => j.interval = be16(s, 0)?,
            0xEE if s.starts_with(b"Adobe") && s.len() >= 12 => j.adobe = Some(s[11]),
            0xE1 => j.orientation = exif_orientation(s).unwrap_or(j.orientation),
            0xDA => {
                let n = *s.first().ok_or("damaged picture")? as usize;
                if j.parts.is_empty() || !(1..=4).contains(&n) || s.len() < 1 + n * 2 + 3 {
                    return Err("damaged picture");
                }
                let mut list = Vec::with_capacity(n);
                for i in 0..n {
                    let (id, tables) = (s[1 + i * 2], s[2 + i * 2]);
                    let k = j.parts.iter().position(|p| p.id == id).ok_or("damaged picture")?;
                    (j.parts[k].dc, j.parts[k].ac) = ((tables >> 4 & 3) as usize, (tables & 3) as usize);
                    list.push(k);
                }
                let tail = &s[1 + n * 2..];
                let scan = Scan { start: tail[0] as usize, end: (tail[1] as usize).min(63), high: (tail[2] >> 4) as u32, low: (tail[2] & 15) as u32 };
                at = j.scan(d, at, &list, scan)?;
                scans += 1;
            }
            _ => {}
        }
    }
    if scans == 0 {
        return Err("damaged picture");
    }
    j.finish()
}
