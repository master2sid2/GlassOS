//! PNG: DEFLATE decompression, row filters, all colour types, Adam7 interlacing.

use super::{Image, checked};
use alloc::vec;
use alloc::vec::Vec;

/// DEFLATE stream bits: the least significant bit of a byte comes first.
struct Bits<'a> {
    data: &'a [u8],
    at: usize,
    hold: u32,
    count: u32,
}

impl Bits<'_> {
    fn need(&mut self, n: u32) -> Result<(), &'static str> {
        while self.count < n {
            let b = *self.data.get(self.at).ok_or("the picture is cut short")?;
            self.at += 1;
            self.hold |= (b as u32) << self.count;
            self.count += 8;
        }
        Ok(())
    }

    fn take(&mut self, n: u32) -> Result<u32, &'static str> {
        if n == 0 {
            return Ok(0);
        }
        self.need(n)?;
        let v = self.hold & ((1u32 << n) - 1);
        self.hold >>= n;
        self.count -= n;
        Ok(v)
    }

    fn align(&mut self) {
        let drop = self.count % 8;
        self.hold >>= drop;
        self.count -= drop;
    }
}

/// A canonical Huffman code: how many codes of each length and the symbols in order.
struct Huffman {
    count: [u16; 16],
    symbol: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Self, &'static str> {
        let mut count = [0u16; 16];
        for &l in lengths {
            count[l as usize] += 1;
        }
        count[0] = 0;
        let mut offset = [0u16; 16];
        for i in 1..15 {
            offset[i + 1] = offset[i] + count[i];
        }
        let mut symbol = vec![0u16; lengths.len()];
        for (s, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbol[offset[l as usize] as usize] = s as u16;
                offset[l as usize] += 1;
            }
        }
        Ok(Self { count, symbol })
    }

    fn decode(&self, b: &mut Bits) -> Result<u16, &'static str> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= b.take(1)? as i32;
            let count = self.count[len] as i32;
            if code - count < first {
                return Ok(self.symbol[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("damaged compressed data")
    }
}

const LENGTH_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LENGTH_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

fn inflate_block(b: &mut Bits, out: &mut Vec<u8>, limit: usize, lit: &Huffman, dist: &Huffman) -> Result<(), &'static str> {
    loop {
        let s = lit.decode(b)? as usize;
        match s {
            0..=255 => out.push(s as u8),
            256 => return Ok(()),
            257..=285 => {
                let len = LENGTH_BASE[s - 257] as usize + b.take(LENGTH_EXTRA[s - 257] as u32)? as usize;
                let d = dist.decode(b)? as usize;
                if d >= 30 {
                    return Err("damaged compressed data");
                }
                let back = DIST_BASE[d] as usize + b.take(DIST_EXTRA[d] as u32)? as usize;
                if back > out.len() {
                    return Err("damaged compressed data");
                }
                let from = out.len() - back;
                for i in 0..len {
                    out.push(out[from + i]);
                }
            }
            _ => return Err("damaged compressed data"),
        }
        if out.len() > limit {
            return Err("damaged compressed data");
        }
    }
}

/// Decompresses a zlib stream; `limit` is how many bytes may come out.
fn inflate(zlib: &[u8], limit: usize) -> Result<Vec<u8>, &'static str> {
    if zlib.len() < 2 {
        return Err("the picture is cut short");
    }
    let mut b = Bits { data: zlib, at: 2, hold: 0, count: 0 };
    let mut out = Vec::with_capacity(limit.min(1 << 28));
    loop {
        let last = b.take(1)? == 1;
        match b.take(2)? {
            0 => {
                b.align();
                let len = b.take(16)? as usize;
                b.take(16)?;
                for _ in 0..len {
                    out.push(b.take(8)? as u8);
                }
            }
            1 => {
                let mut lengths = [8u8; 288];
                lengths[144..256].fill(9);
                lengths[256..280].fill(7);
                inflate_block(&mut b, &mut out, limit, &Huffman::new(&lengths)?, &Huffman::new(&[5u8; 30])?)?;
            }
            2 => {
                let (nlen, ndist, ncode) = (b.take(5)? as usize + 257, b.take(5)? as usize + 1, b.take(4)? as usize + 4);
                const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                let mut code_lengths = [0u8; 19];
                for &slot in ORDER.iter().take(ncode) {
                    code_lengths[slot] = b.take(3)? as u8;
                }
                let codes = Huffman::new(&code_lengths)?;
                let mut lengths = [0u8; 320];
                let mut i = 0;
                while i < nlen + ndist {
                    let s = codes.decode(&mut b)?;
                    let (value, repeat) = match s {
                        0..=15 => (s as u8, 1),
                        16 if i > 0 => (lengths[i - 1], 3 + b.take(2)? as usize),
                        17 => (0, 3 + b.take(3)? as usize),
                        18 => (0, 11 + b.take(7)? as usize),
                        _ => return Err("damaged compressed data"),
                    };
                    if i + repeat > nlen + ndist {
                        return Err("damaged compressed data");
                    }
                    lengths[i..i + repeat].fill(value);
                    i += repeat;
                }
                inflate_block(&mut b, &mut out, limit, &Huffman::new(&lengths[..nlen])?, &Huffman::new(&lengths[nlen..nlen + ndist])?)?;
            }
            _ => return Err("damaged compressed data"),
        }
        if last {
            return Ok(out);
        }
    }
}

fn be32(d: &[u8], at: usize) -> usize {
    u32::from_be_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]]) as usize
}

fn paeth(a: i32, b: i32, c: i32) -> u8 {
    let p = a + b - c;
    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
    (if pa <= pb && pa <= pc { a } else if pb <= pc { b } else { c }) as u8
}

/// Removes the filters from `rows` rows of `stride` bytes starting at `*at` into `out`.
fn unfilter(raw: &[u8], at: &mut usize, stride: usize, rows: usize, bpp: usize, out: &mut [u8]) -> Result<(), &'static str> {
    for y in 0..rows {
        let filter = *raw.get(*at).ok_or("the picture is cut short")?;
        let line = raw.get(*at + 1..*at + 1 + stride).ok_or("the picture is cut short")?;
        *at += stride + 1;
        let (above, rest) = out.split_at_mut(y * stride);
        let row = &mut rest[..stride];
        let up = |i: usize| if y == 0 { 0 } else { above[(y - 1) * stride + i] };
        for i in 0..stride {
            let left = if i >= bpp { row[i - bpp] } else { 0 };
            let corner = if i >= bpp { up(i - bpp) } else { 0 };
            row[i] = line[i].wrapping_add(match filter {
                0 => 0,
                1 => left,
                2 => up(i),
                3 => ((left as u16 + up(i) as u16) / 2) as u8,
                4 => paeth(left as i32, up(i) as i32, corner as i32),
                _ => return Err("damaged picture"),
            });
        }
    }
    Ok(())
}

struct Format {
    color: u8,
    depth: usize,
    palette: Vec<[u8; 3]>,
    alpha: Vec<u8>,
    /// The colour meaning transparency (tRNS for grey and RGB), at the source bit depth.
    key: Option<[u16; 3]>,
}

impl Format {
    fn channels(&self) -> usize {
        match self.color {
            2 => 3,
            4 => 2,
            6 => 4,
            _ => 1,
        }
    }

    /// Sample number `i` of the row.
    fn sample(&self, row: &[u8], i: usize) -> u16 {
        match self.depth {
            16 => u16::from_be_bytes([row[2 * i], row[2 * i + 1]]),
            8 => row[i] as u16,
            d => {
                let bit = i * d;
                (row[bit / 8] >> (8 - d - bit % 8)) as u16 & ((1 << d) - 1)
            }
        }
    }

    /// A sample in 8 bits.
    fn wide(&self, v: u16) -> u32 {
        (match self.depth {
            16 => v >> 8,
            8 => v,
            d => v * 255 / ((1 << d) - 1),
        }) as u32
    }

    fn pixel(&self, row: &[u8], x: usize) -> Result<u32, &'static str> {
        let s = |c: usize| self.sample(row, x * self.channels() + c);
        Ok(match self.color {
            0 => {
                let v = s(0);
                let g = self.wide(v);
                let a = if self.key.is_some_and(|k| k[0] == v) { 0 } else { 255 };
                a << 24 | g << 16 | g << 8 | g
            }
            2 => {
                let (r, g, b) = (s(0), s(1), s(2));
                let a = if self.key == Some([r, g, b]) { 0 } else { 255 };
                a << 24 | self.wide(r) << 16 | self.wide(g) << 8 | self.wide(b)
            }
            3 => {
                let i = s(0) as usize;
                let c = self.palette.get(i).ok_or("damaged picture")?;
                let a = self.alpha.get(i).copied().unwrap_or(255) as u32;
                a << 24 | (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32
            }
            4 => {
                let g = self.wide(s(0));
                self.wide(s(1)) << 24 | g << 16 | g << 8 | g
            }
            _ => self.wide(s(3)) << 24 | self.wide(s(0)) << 16 | self.wide(s(1)) << 8 | self.wide(s(2)),
        })
    }
}

pub fn decode(d: &[u8]) -> Result<Image, &'static str> {
    let mut at = 8;
    let mut size = (0, 0);
    let mut interlaced = false;
    let mut f = Format { color: 0, depth: 8, palette: Vec::new(), alpha: Vec::new(), key: None };
    let mut packed = Vec::new();
    while at + 8 <= d.len() {
        let (len, kind) = (be32(d, at), &d[at + 4..at + 8]);
        let body = d.get(at + 8..at + 8 + len).ok_or("the picture is cut short")?;
        match kind {
            b"IHDR" if len >= 13 => {
                size = (be32(body, 0), be32(body, 4));
                (f.depth, f.color, interlaced) = (body[8] as usize, body[9], body[12] == 1);
                if body[10] != 0 || body[11] != 0 || !matches!(f.color, 0 | 2 | 3 | 4 | 6) || !matches!(f.depth, 1 | 2 | 4 | 8 | 16) {
                    return Err("unsupported PNG");
                }
            }
            b"PLTE" => f.palette = body.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect(),
            b"tRNS" => match f.color {
                3 => f.alpha = body.to_vec(),
                0 if len >= 2 => f.key = Some([u16::from_be_bytes([body[0], body[1]]), 0, 0]),
                2 if len >= 6 => {
                    let w = |i: usize| u16::from_be_bytes([body[i], body[i + 1]]);
                    f.key = Some([w(0), w(2), w(4)]);
                }
                _ => {}
            },
            b"IDAT" => packed.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        at += 12 + len;
    }
    let (w, h) = size;
    let count = checked(w, h)?;
    let bits = f.channels() * f.depth;
    let bpp = bits.div_ceil(8);
    let mut px = vec![0u32; count];
    if !interlaced {
        let stride = (w * bits).div_ceil(8);
        let raw = inflate(&packed, (stride + 1) * h)?;
        let mut out = vec![0u8; stride * h];
        let mut pos = 0;
        unfilter(&raw, &mut pos, stride, h, bpp, &mut out)?;
        for y in 0..h {
            let row = &out[y * stride..(y + 1) * stride];
            for x in 0..w {
                px[y * w + x] = f.pixel(row, x)?;
            }
        }
        return Ok(Image::single(w, h, px, "PNG"));
    }
    // Adam7: seven passes, each with its own pixel grid.
    const PASSES: [(usize, usize, usize, usize); 7] = [(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)];
    let limit: usize = PASSES.iter().map(|&(x0, y0, dx, dy)| {
        let (pw, ph) = ((w + dx - 1 - x0.min(w)) / dx, (h + dy - 1 - y0.min(h)) / dy);
        if pw == 0 { 0 } else { ((pw * bits).div_ceil(8) + 1) * ph }
    }).sum();
    let raw = inflate(&packed, limit)?;
    let mut pos = 0;
    for (x0, y0, dx, dy) in PASSES {
        let (pw, ph) = ((w + dx - 1 - x0.min(w)) / dx, (h + dy - 1 - y0.min(h)) / dy);
        if pw == 0 || ph == 0 {
            continue;
        }
        let stride = (pw * bits).div_ceil(8);
        let mut out = vec![0u8; stride * ph];
        unfilter(&raw, &mut pos, stride, ph, bpp, &mut out)?;
        for py in 0..ph {
            let row = &out[py * stride..(py + 1) * stride];
            for pxi in 0..pw {
                px[(y0 + py * dy) * w + x0 + pxi * dx] = f.pixel(row, pxi)?;
            }
        }
    }
    Ok(Image::single(w, h, px, "PNG"))
}
