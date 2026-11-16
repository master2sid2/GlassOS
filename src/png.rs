//! PNG: writing pictures (screenshots). Deflate "stored" compression — no packing:
//! the file is larger, but the encoder is tiny, and any program opens it.

use alloc::vec::Vec;

/// CRC-32 (IEEE, as in PNG, GPT, zip). The CPU's `crc32` instruction from SSE4.2 computes
/// another polynomial (Castagnoli), so tables are used here: eight bytes per step.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        let lo = u32::from_le_bytes([c[0], c[1], c[2], c[3]]) ^ crc;
        let hi = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
        let t = &CRC_TABLES;
        crc = t[7][(lo & 0xFF) as usize]
            ^ t[6][(lo >> 8 & 0xFF) as usize]
            ^ t[5][(lo >> 16 & 0xFF) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][(hi & 0xFF) as usize]
            ^ t[2][(hi >> 8 & 0xFF) as usize]
            ^ t[1][(hi >> 16 & 0xFF) as usize]
            ^ t[0][(hi >> 24) as usize];
    }
    for &b in chunks.remainder() {
        crc = CRC_TABLES[0][((crc ^ b as u32) & 0xFF) as usize] ^ crc >> 8;
    }
    !crc
}

/// The bitwise version: the reference for the self-check.
fn crc32_bitwise(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// The compiler computes the tables: `[k][i]` is the CRC of byte `i` followed by `k` zeros.
static CRC_TABLES: [[u32; 256]; 8] = {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = t[k - 1][i];
            t[k][i] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            i += 1;
        }
        k += 1;
    }
    t
};

/// The CRC self-check at startup: table-driven against bitwise, plus a 1 MB benchmark.
pub fn check() {
    let mut data = alloc::vec![0u8; 1 << 20];
    let mut seed = 0x9E37_79B9u32;
    for b in data.iter_mut() {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        *b = seed as u8;
    }
    let mut ok = crc32(b"123456789") == 0xCBF4_3926;
    for len in [0, 1, 7, 8, 9, 63, 1000, 4097] {
        ok &= crc32(&data[3..3 + len]) == crc32_bitwise(&data[3..3 + len]);
    }
    let t = crate::arch::rdtsc();
    let slow = core::hint::black_box(crc32_bitwise(&data));
    let t1 = crate::arch::rdtsc();
    let fast = core::hint::black_box(crc32(&data));
    let t2 = crate::arch::rdtsc();
    ok &= slow == fast;
    crate::kprintln!("crc32: check {}: 1 MB {} → {} Kcycles", if ok { "passed" } else { "FAILED" }, (t1 - t) / 1000, (t2 - t1) / 1000);
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    b << 16 | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// A `w`×`h` picture (0xRRGGBB pixels) → a PNG file (RGB, 8-bit).
pub fn encode(w: i32, h: i32, px: &[u32]) -> Vec<u8> {
    // Rows: a filter byte (0 — no filter) and RGB.
    let mut raw = Vec::with_capacity((w * 3 + 1) as usize * h as usize);
    for row in px.chunks(w as usize).take(h as usize) {
        raw.push(0);
        for &p in row {
            raw.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
        }
    }
    // zlib: a header, "stored" blocks of 65535 bytes, Adler-32.
    let mut z = alloc::vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push((i + 1 == blocks.len()) as u8);
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, RGB
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}
