//! Hashes for network protocols: MD4 and MD5 (the NTLMv2 login in SMB), SHA-256
//! (SMB2 packet signing) and HMAC on top of them. Integer arithmetic only.

use alloc::vec::Vec;

// ---------------------------------------------------------------- MD4 / MD5

/// MD-style padding: 0x80, zeros, the length in bits (little-endian).
fn md_pad(data: &[u8]) -> Vec<u8> {
    let mut m = data.to_vec();
    m.push(0x80);
    while m.len() % 64 != 56 {
        m.push(0);
    }
    m.extend_from_slice(&((data.len() as u64) * 8).to_le_bytes());
    m
}

fn words_le(block: &[u8]) -> [u32; 16] {
    let mut x = [0u32; 16];
    for (i, w) in x.iter_mut().enumerate() {
        *w = u32::from_le_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
    }
    x
}

fn digest_le(s: [u32; 4]) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (i, v) in s.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    out
}

pub fn md4(data: &[u8]) -> [u8; 16] {
    let mut s = [0x67452301u32, 0xefcdab89, 0x98badcfe, 0x10325476];
    for block in md_pad(data).chunks(64) {
        let x = words_le(block);
        let [mut a, mut b, mut c, mut d] = s;
        let f = |x: u32, y: u32, z: u32| (x & y) | (!x & z);
        let g = |x: u32, y: u32, z: u32| (x & y) | (x & z) | (y & z);
        let h = |x: u32, y: u32, z: u32| x ^ y ^ z;
        for i in 0..16 {
            let k = i;
            let r = [3, 7, 11, 19][i % 4];
            let t = a.wrapping_add(f(b, c, d)).wrapping_add(x[k]).rotate_left(r);
            (a, b, c, d) = (d, t, b, c);
        }
        for i in 0..16 {
            let k = (i % 4) * 4 + i / 4;
            let r = [3, 5, 9, 13][i % 4];
            let t = a.wrapping_add(g(b, c, d)).wrapping_add(x[k]).wrapping_add(0x5A827999).rotate_left(r);
            (a, b, c, d) = (d, t, b, c);
        }
        const ORDER: [usize; 16] = [0, 8, 4, 12, 2, 10, 6, 14, 1, 9, 5, 13, 3, 11, 7, 15];
        for (i, &k) in ORDER.iter().enumerate() {
            let r = [3, 9, 11, 15][i % 4];
            let t = a.wrapping_add(h(b, c, d)).wrapping_add(x[k]).wrapping_add(0x6ED9EBA1).rotate_left(r);
            (a, b, c, d) = (d, t, b, c);
        }
        s = [s[0].wrapping_add(a), s[1].wrapping_add(b), s[2].wrapping_add(c), s[3].wrapping_add(d)];
    }
    digest_le(s)
}

pub fn md5(data: &[u8]) -> [u8; 16] {
    const R: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14,
        20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6,
        10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501, 0x698098d8,
        0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340,
        0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87,
        0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c,
        0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039,
        0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92,
        0xffeff47d, 0x85845dd1, 0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];
    let mut s = [0x67452301u32, 0xefcdab89, 0x98badcfe, 0x10325476];
    for block in md_pad(data).chunks(64) {
        let x = words_le(block);
        let [mut a, mut b, mut c, mut d] = s;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let t = d;
            d = c;
            c = b;
            b = b.wrapping_add(a.wrapping_add(f).wrapping_add(K[i]).wrapping_add(x[g]).rotate_left(R[i]));
            a = t;
        }
        s = [s[0].wrapping_add(a), s[1].wrapping_add(b), s[2].wrapping_add(c), s[3].wrapping_add(d)];
    }
    digest_le(s)
}

// ---------------------------------------------------------------- SHA-256

const K256: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
    0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
    0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
    0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
    0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
    0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
    0xc67178f2,
];

/// One SHA-256 block. The message schedule is a sliding window of 16, the rounds are unrolled
/// by a macro: the variables are not shuffled, only their roles change.
#[inline(always)]
fn sha256_block(h: &mut [u32; 8], block: &[u8]) {
    let mut w = [0u32; 16];
    for (i, word) in w.iter_mut().enumerate() {
        *word = u32::from_be_bytes([block[i * 4], block[i * 4 + 1], block[i * 4 + 2], block[i * 4 + 3]]);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
    macro_rules! round {
        ($a:ident, $b:ident, $c:ident, $d:ident, $e:ident, $f:ident, $g:ident, $h:ident, $i:expr) => {{
            let i: usize = $i;
            let wi = if i < 16 {
                w[i]
            } else {
                let w15 = w[(i + 1) & 15];
                let w2 = w[(i + 14) & 15];
                let s0 = w15.rotate_right(7) ^ w15.rotate_right(18) ^ (w15 >> 3);
                let s1 = w2.rotate_right(17) ^ w2.rotate_right(19) ^ (w2 >> 10);
                let v = w[i & 15].wrapping_add(s0).wrapping_add(w[(i + 9) & 15]).wrapping_add(s1);
                w[i & 15] = v;
                v
            };
            let t1 = $h
                .wrapping_add($e.rotate_right(6) ^ $e.rotate_right(11) ^ $e.rotate_right(25))
                .wrapping_add(($e & $f) ^ (!$e & $g))
                .wrapping_add(K256[i])
                .wrapping_add(wi);
            let t2 = ($a.rotate_right(2) ^ $a.rotate_right(13) ^ $a.rotate_right(22)).wrapping_add(($a & $b) ^ ($a & $c) ^ ($b & $c));
            $d = $d.wrapping_add(t1);
            $h = t1.wrapping_add(t2);
        }};
    }
    let mut i = 0;
    while i < 64 {
        round!(a, b, c, d, e, f, g, hh, i);
        round!(hh, a, b, c, d, e, f, g, i + 1);
        round!(g, hh, a, b, c, d, e, f, i + 2);
        round!(f, g, hh, a, b, c, d, e, i + 3);
        round!(e, f, g, hh, a, b, c, d, i + 4);
        round!(d, e, f, g, hh, a, b, c, i + 5);
        round!(c, d, e, f, g, hh, a, b, i + 6);
        round!(b, c, d, e, f, g, hh, a, i + 7);
        i += 8;
    }
    for (x, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
        *x = x.wrapping_add(v);
    }
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    // Whole blocks straight from the data, without a copy; the padded tail goes to its own buffer.
    let mut blocks = data.chunks_exact(64);
    for block in &mut blocks {
        sha256_block(&mut h, block);
    }
    let rest = blocks.remainder();
    let mut tail = [0u8; 128];
    tail[..rest.len()].copy_from_slice(rest);
    tail[rest.len()] = 0x80;
    let end = if rest.len() < 56 { 64 } else { 128 };
    tail[end - 8..end].copy_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in tail[..end].chunks_exact(64) {
        sha256_block(&mut h, block);
    }
    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

// ---------------------------------------------------------------- HMAC

fn hmac<const N: usize>(hash: fn(&[u8]) -> [u8; N], key: &[u8], msg: &[u8]) -> [u8; N] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..N].copy_from_slice(&hash(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    inner.extend_from_slice(msg);
    let mut outer: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    outer.extend_from_slice(&hash(&inner));
    hash(&outer)
}

pub fn hmac_md5(key: &[u8], msg: &[u8]) -> [u8; 16] {
    hmac(md5, key, msg)
}

pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    hmac(sha256, key, msg)
}

// ---------------------------------------------------------------- RC4

/// RC4 (encrypts and decrypts the same way): the session key in NTLM KEY_EXCH.
pub fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut s: [u8; 256] = core::array::from_fn(|i| i as u8);
    let mut j = 0u8;
    for i in 0..256 {
        j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }
    let (mut i, mut j) = (0u8, 0u8);
    data.iter()
        .map(|&b| {
            i = i.wrapping_add(1);
            j = j.wrapping_add(s[i as usize]);
            s.swap(i as usize, j as usize);
            b ^ s[s[i as usize].wrapping_add(s[j as usize]) as usize]
        })
        .collect()
}

/// PBKDF2-HMAC-SHA256 with a single result block (32 bytes).
pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut msg = salt.to_vec();
    msg.extend_from_slice(&1u32.to_be_bytes());
    let mut block = hmac_sha256(password, &msg);
    let mut out = block;
    for _ in 1..iterations {
        block = hmac_sha256(password, &block);
        for (o, b) in out.iter_mut().zip(block) {
            *o ^= b;
        }
    }
    out
}

/// Random bytes: CPU noise (RDSEED/RDRAND), if available, and everything that is hard
/// to guess from outside, passed through SHA-256.
pub fn random32() -> [u8; 32] {
    use core::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut seed = alloc::vec::Vec::new();
    for _ in 0..8 {
        let v = crate::cpu::random_u64().unwrap_or(0);
        seed.extend_from_slice(&v.to_le_bytes());
        seed.extend_from_slice(&crate::arch::rdtsc().to_le_bytes());
    }
    seed.extend_from_slice(&COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    seed.extend_from_slice(&crate::interrupts::ticks().to_le_bytes());
    sha256(&seed)
}

// ---------------------------------------------------------------- SHA-1 and WPA2

pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(word.try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, &word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6u32),
            };
            let next = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(word);
            (e, d, c, b, a) = (d, c, b.rotate_left(30), a, next);
        }
        for (s, v) in h.iter_mut().zip([a, b, c, d, e]) {
            *s = s.wrapping_add(v);
        }
    }
    let mut out = [0u8; 20];
    for (chunk, v) in out.chunks_exact_mut(4).zip(h) {
        chunk.copy_from_slice(&v.to_be_bytes());
    }
    out
}

pub fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; 20] {
    hmac(sha1, key, msg)
}

/// The WPA2 network key from a password: PBKDF2-HMAC-SHA1, 4096 iterations, 32 bytes.
pub fn wpa_psk(password: &[u8], ssid: &[u8]) -> [u8; 32] {
    const ITERATIONS: u32 = 4096;
    let mut out = [0u8; 32];
    for (index, chunk) in out.chunks_mut(20).enumerate() {
        let mut msg = ssid.to_vec();
        msg.extend_from_slice(&(index as u32 + 1).to_be_bytes());
        let mut block = hmac_sha1(password, &msg);
        let mut sum = block;
        for _ in 1..ITERATIONS {
            block = hmac_sha1(password, &block);
            for (o, b) in sum.iter_mut().zip(block) {
                *o ^= b;
            }
        }
        chunk.copy_from_slice(&sum[..chunk.len()]);
    }
    out
}

fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut out = 0;
    while b != 0 {
        if b & 1 != 0 {
            out ^= a;
        }
        a = (a << 1) ^ if a & 0x80 != 0 { 0x1B } else { 0 };
        b >>= 1;
    }
    out
}

/// The AES tables are computed in place: keys are expanded rarely.
fn aes_sboxes() -> ([u8; 256], [u8; 256]) {
    let (mut sbox, mut inverse) = ([0u8; 256], [0u8; 256]);
    let (mut p, mut q) = (1u8, 1u8);
    loop {
        p = p ^ (p << 1) ^ if p & 0x80 != 0 { 0x1B } else { 0 };
        q ^= q << 1;
        q ^= q << 2;
        q ^= q << 4;
        if q & 0x80 != 0 {
            q ^= 0x09;
        }
        let value = q ^ q.rotate_left(1) ^ q.rotate_left(2) ^ q.rotate_left(3) ^ q.rotate_left(4) ^ 0x63;
        sbox[p as usize] = value;
        if p == 1 {
            break;
        }
    }
    sbox[0] = 0x63;
    for (i, &s) in sbox.iter().enumerate() {
        inverse[s as usize] = i as u8;
    }
    (sbox, inverse)
}

fn aes_round_keys(key: &[u8; 16], sbox: &[u8; 256]) -> [[u8; 16]; 11] {
    let mut keys = [[0u8; 16]; 11];
    keys[0] = *key;
    let mut rcon = 1u8;
    for round in 1..11 {
        let last = keys[round - 1];
        let mut temp = [sbox[last[13] as usize] ^ rcon, sbox[last[14] as usize], sbox[last[15] as usize], sbox[last[12] as usize]];
        rcon = gf_mul(rcon, 2);
        for word in 0..4 {
            for byte in 0..4 {
                temp[byte] ^= last[word * 4 + byte];
                keys[round][word * 4 + byte] = temp[byte];
            }
        }
    }
    keys
}

fn aes_decrypt(block: &mut [u8; 16], keys: &[[u8; 16]; 11], inverse: &[u8; 256]) {
    if crate::cpu::has(crate::cpu::AES) {
        *block = unsafe { aesni::decrypt(keys, block) };
        return;
    }
    aes_decrypt_soft(block, keys, inverse);
}

fn aes_decrypt_soft(block: &mut [u8; 16], keys: &[[u8; 16]; 11], inverse: &[u8; 256]) {
    let xor = |block: &mut [u8; 16], key: &[u8; 16]| block.iter_mut().zip(key).for_each(|(b, k)| *b ^= k);
    xor(block, &keys[10]);
    for round in (0..10).rev() {
        let old = *block;
        for (i, b) in block.iter_mut().enumerate() {
            // The inverse row shift and byte substitution.
            *b = inverse[old[(i + 16 - 4 * (i % 4)) % 16] as usize];
        }
        xor(block, &keys[round]);
        if round == 0 {
            break;
        }
        let old = *block;
        for column in 0..4 {
            let c = &old[column * 4..column * 4 + 4];
            for row in 0..4 {
                block[column * 4 + row] = gf_mul(c[row], 14) ^ gf_mul(c[(row + 1) % 4], 11) ^ gf_mul(c[(row + 2) % 4], 13) ^ gf_mul(c[(row + 3) % 4], 9);
            }
        }
    }
}

/// Encrypts an AES-128 block (Bluetooth LE pairing). Checked against the FIPS-197 vector.
pub fn aes128_encrypt(key: &[u8; 16], block: &[u8; 16]) -> [u8; 16] {
    let (sbox, _) = aes_sboxes();
    let keys = aes_round_keys(key, &sbox);
    if crate::cpu::has(crate::cpu::AES) {
        return unsafe { aesni::encrypt(&keys, block) };
    }
    aes_encrypt_soft(&keys, &sbox, block)
}

fn aes_encrypt_soft(keys: &[[u8; 16]; 11], sbox: &[u8; 256], block: &[u8; 16]) -> [u8; 16] {
    let xor = |block: &mut [u8; 16], key: &[u8; 16]| block.iter_mut().zip(key).for_each(|(b, k)| *b ^= k);
    let mut s = *block;
    xor(&mut s, &keys[0]);
    for round in 1..11 {
        let old = s;
        for (i, b) in s.iter_mut().enumerate() {
            // Byte substitution and row shift: row r — left by r.
            *b = sbox[old[(i + 4 * (i % 4)) % 16] as usize];
        }
        if round < 10 {
            let old = s;
            for column in 0..4 {
                let c = &old[column * 4..column * 4 + 4];
                for row in 0..4 {
                    s[column * 4 + row] = gf_mul(c[row], 2) ^ gf_mul(c[(row + 1) % 4], 3) ^ c[(row + 2) % 4] ^ c[(row + 3) % 4];
                }
            }
        }
        xor(&mut s, &keys[round]);
    }
    s
}

/// Unwraps keys wrapped per RFC 3394. None — the check did not match.
pub fn aes_unwrap(kek: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    const CHECK: u8 = 0xA6;
    if data.len() < 16 || data.len() % 8 != 0 {
        return None;
    }
    let (sbox, inverse) = aes_sboxes();
    let keys = aes_round_keys(kek, &sbox);
    let n = data.len() / 8 - 1;
    let mut a: [u8; 8] = data[..8].try_into().ok()?;
    let mut r = data[8..].to_vec();
    for j in (0..6).rev() {
        for i in (0..n).rev() {
            let t = (n * j + i + 1) as u64;
            let mut block = [0u8; 16];
            block[..8].copy_from_slice(&(u64::from_be_bytes(a) ^ t).to_be_bytes());
            block[8..].copy_from_slice(&r[i * 8..i * 8 + 8]);
            aes_decrypt(&mut block, &keys, &inverse);
            a.copy_from_slice(&block[..8]);
            r[i * 8..i * 8 + 8].copy_from_slice(&block[8..]);
        }
    }
    (a == [CHECK; 8]).then_some(r)
}

/// AES on CPU instructions (AES-NI): a round is one instruction. The round keys are the same
/// as those `aes_round_keys` expands (bytes in FIPS-197 order).
mod aesni {
    use core::arch::x86_64::*;

    fn load(b: &[u8; 16]) -> __m128i {
        unsafe { _mm_loadu_si128(b.as_ptr() as *const __m128i) }
    }

    fn store(v: __m128i) -> [u8; 16] {
        let mut out = [0u8; 16];
        unsafe { _mm_storeu_si128(out.as_mut_ptr() as *mut __m128i, v) };
        out
    }

    #[target_feature(enable = "aes")]
    pub unsafe fn encrypt(keys: &[[u8; 16]; 11], block: &[u8; 16]) -> [u8; 16] {
        let mut s = _mm_xor_si128(load(block), load(&keys[0]));
        for key in &keys[1..10] {
            s = _mm_aesenc_si128(s, load(key));
        }
        store(_mm_aesenclast_si128(s, load(&keys[10])))
    }

    /// The inverse cipher in the "equivalent" form: the middle keys need InvMixColumns.
    #[target_feature(enable = "aes")]
    pub unsafe fn decrypt(keys: &[[u8; 16]; 11], block: &[u8; 16]) -> [u8; 16] {
        let mut s = _mm_xor_si128(load(block), load(&keys[10]));
        for key in keys[1..10].iter().rev() {
            s = _mm_aesdec_si128(s, _mm_aesimc_si128(load(key)));
        }
        store(_mm_aesdeclast_si128(s, load(&keys[0])))
    }
}

/// A self-check at startup: AES on CPU instructions against the simple version
/// (the FIPS-197 vector and random blocks). On a mismatch the instructions are disabled. Plus a benchmark.
pub fn check() {
    use crate::cpu;
    const KEY: [u8; 16] = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f];
    const PLAIN: [u8; 16] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
    const CIPHER: [u8; 16] = [0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4, 0xc5, 0x5a];
    let (sbox, inverse) = aes_sboxes();
    let keys = aes_round_keys(&KEY, &sbox);
    let soft_ok = aes_encrypt_soft(&keys, &sbox, &PLAIN) == CIPHER && {
        let mut b = CIPHER;
        aes_decrypt_soft(&mut b, &keys, &inverse);
        b == PLAIN
    };
    let mut line = alloc::format!("cpu: {}", cpu::brand());
    if cpu::has(cpu::AES) {
        let mut ok = soft_ok;
        let mut block = random32();
        for round in 0..64 {
            let key: [u8; 16] = core::array::from_fn(|i| block[i]);
            let data: [u8; 16] = core::array::from_fn(|i| block[16 + i]);
            let keys = aes_round_keys(&key, &sbox);
            let fast = unsafe { aesni::encrypt(&keys, &data) };
            let mut back = fast;
            aes_decrypt_soft(&mut back, &keys, &inverse);
            ok &= fast == aes_encrypt_soft(&keys, &sbox, &data) && back == data && unsafe { aesni::decrypt(&keys, &fast) } == data;
            if round == 0 {
                ok &= unsafe { aesni::encrypt(&aes_round_keys(&KEY, &sbox), &PLAIN) } == CIPHER;
            }
            block = sha256(&block);
        }
        let time = |f: &mut dyn FnMut()| {
            let t = crate::arch::rdtsc();
            f();
            crate::arch::rdtsc() - t
        };
        let mut b = PLAIN;
        let soft = time(&mut || (0..1000).for_each(|_| b = core::hint::black_box(aes_encrypt_soft(&keys, &sbox, &b))));
        let fast = time(&mut || (0..1000).for_each(|_| b = core::hint::black_box(unsafe { aesni::encrypt(&keys, &b) })));
        if ok {
            line += &alloc::format!("; AES-NI check passed: block {} → {} cycles", soft / 1000, fast / 1000);
        } else {
            cpu::disable(cpu::AES);
            line += "; AES-NI gave wrong results, using the software AES";
        }
    } else {
        line += "; no AES-NI, software AES";
    }
    if !soft_ok {
        line += "; software AES FAILED the FIPS-197 check";
    }
    line += match (cpu::has(cpu::RDSEED), cpu::has(cpu::RDRAND)) {
        (true, _) => "; random: RDSEED",
        (false, true) => "; random: RDRAND",
        _ => "; random: no hardware source, timers only",
    };
    if (cpu::has(cpu::RDSEED) || cpu::has(cpu::RDRAND)) && cpu::random_u64().is_none() {
        line += " (it does not answer)";
    }
    crate::kprintln!("{line}");
    crate::png::check();
}
