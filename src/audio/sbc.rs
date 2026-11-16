//! The SBC codec — the mandatory Bluetooth audio (A2DP) codec: all headphones and speakers
//! understand it. The encoder follows appendix B of the A2DP 1.4 specification: an 8-subband analysis filter,
//! scale factors, "loudness" bit allocation, quantization, a frame with CRC-8.
//! Stereo (each channel separately), 16 blocks, 8 subbands. Independent of Bluetooth.

use alloc::vec::Vec;

const SUBBANDS: usize = 8;
pub const BLOCKS: usize = 16;
/// Samples per channel in one frame.
pub const FRAME_SAMPLES: usize = SUBBANDS * BLOCKS;
const SYNCWORD: u8 = 0x9C;

/// The sampling frequency in the header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rate {
    Hz44100,
    Hz48000,
}

impl Rate {
    fn code(self) -> u8 {
        match self {
            Rate::Hz44100 => 2,
            Rate::Hz48000 => 3,
        }
    }
    pub fn hz(self) -> u32 {
        match self {
            Rate::Hz44100 => 44_100,
            Rate::Hz48000 => 48_000,
        }
    }
}

/// Per-subband loudness offsets for 44.1 and 48 kHz (the offset8 table).
const OFFSET8: [i32; 8] = [-4, 0, 0, 0, 0, 0, 1, 2];

const PROTO: [f32; 80] = [0.0, 0.000156575398, 0.000343256425, 0.000554620202, 0.000823919506, 0.00113992507, 0.00147640169, 0.00178371725, 0.00201182542, 0.00210371989, 0.00199454554, 0.00161656283, 0.000902154502, -0.000178805361, -0.00164973098, -0.00349717454, 0.00565949473, 0.00802941163, 0.0104584443, 0.0127472335, 0.0146525263, 0.0159045603, 0.0162208471, 0.0153184106, 0.0129371806, 0.0088575754, 0.00292408442, -0.00491578024, -0.0146404076, -0.0261098752, -0.0390751381, -0.0531873032, 0.0679989431, 0.0829847578, 0.0975753918, 0.111196689, 0.123264548, 0.133264415, 0.140753505, 0.145389847, 0.146955068, 0.145389847, 0.140753505, 0.133264415, 0.123264548, 0.111196689, 0.0975753918, 0.0829847578, -0.0679989431, -0.0531873032, -0.0390751381, -0.0261098752, -0.0146404076, -0.00491578024, 0.00292408442, 0.0088575754, 0.0129371806, 0.0153184106, 0.0162208471, 0.0159045603, 0.0146525263, 0.0127472335, 0.0104584443, 0.00802941163, -0.00565949473, -0.00349717454, -0.00164973098, -0.000178805361, 0.000902154502, 0.00161656283, 0.00199454554, 0.00210371989, 0.00201182542, 0.00178371725, 0.00147640169, 0.00113992507, 0.000823919506, 0.000554620202, 0.000343256425, 0.000156575398];
const MATRIX: [[f32; 16]; 8] = [[0.707106781, 0.831469612, 0.923879533, 0.98078528, 1.0, 0.98078528, 0.923879533, 0.831469612, 0.707106781, 0.555570233, 0.382683432, 0.195090322, 0.0, -0.195090322, -0.382683432, -0.555570233], [-0.707106781, -0.195090322, 0.382683432, 0.831469612, 1.0, 0.831469612, 0.382683432, -0.195090322, -0.707106781, -0.98078528, -0.923879533, -0.555570233, -0.0, 0.555570233, 0.923879533, 0.98078528], [-0.707106781, -0.98078528, -0.382683432, 0.555570233, 1.0, 0.555570233, -0.382683432, -0.98078528, -0.707106781, 0.195090322, 0.923879533, 0.831469612, 0.0, -0.831469612, -0.923879533, -0.195090322], [0.707106781, -0.555570233, -0.923879533, 0.195090322, 1.0, 0.195090322, -0.923879533, -0.555570233, 0.707106781, 0.831469612, -0.382683432, -0.98078528, -0.0, 0.98078528, 0.382683432, -0.831469612], [0.707106781, 0.555570233, -0.923879533, -0.195090322, 1.0, -0.195090322, -0.923879533, 0.555570233, 0.707106781, -0.831469612, -0.382683432, 0.98078528, 0.0, -0.98078528, 0.382683432, 0.831469612], [-0.707106781, 0.98078528, -0.382683432, -0.555570233, 1.0, -0.555570233, -0.382683432, 0.98078528, -0.707106781, -0.195090322, 0.923879533, -0.831469612, -0.0, 0.831469612, -0.923879533, 0.195090322], [-0.707106781, 0.195090322, 0.382683432, -0.831469612, 1.0, -0.831469612, 0.382683432, 0.195090322, -0.707106781, 0.98078528, -0.923879533, 0.555570233, -0.0, -0.555570233, 0.923879533, -0.98078528], [0.707106781, -0.831469612, 0.923879533, -0.98078528, 1.0, -0.98078528, 0.923879533, -0.831469612, 0.707106781, -0.555570233, 0.382683432, -0.195090322, -0.0, 0.195090322, -0.382683432, 0.555570233]];

/// The encoder of one stream: the filter state of two channels.
pub struct Encoder {
    rate: Rate,
    bitpool: u8,
    x: [[f32; 80]; 2],
}

impl Encoder {
    pub fn new(rate: Rate, bitpool: u8) -> Encoder {
        Encoder { rate, bitpool: bitpool.clamp(2, 250), x: [[0.0; 80]; 2] }
    }

    /// The frame length in bytes (stereo): header, scale factors, samples.
    pub fn frame_len(&self) -> usize {
        4 + (4 * SUBBANDS * 2) / 8 + (BLOCKS * self.bitpool as usize).div_ceil(8)
    }

    /// The analysis filter: 8 new samples of a channel → 8 subbands.
    fn analyze(&mut self, ch: usize, input: &[i16], out: &mut [f32; SUBBANDS]) {
        let x = &mut self.x[ch];
        x.copy_within(0..72, 8);
        for i in 0..SUBBANDS {
            x[7 - i] = input[i] as f32;
        }
        let mut y = [0.0f32; 16];
        for (i, yi) in y.iter_mut().enumerate() {
            *yi = (0..5).map(|j| PROTO[i + 16 * j] * x[i + 16 * j]).sum();
        }
        for (k, o) in out.iter_mut().enumerate() {
            *o = MATRIX[k].iter().zip(&y).map(|(m, v)| m * v).sum();
        }
    }

    /// A frame of `FRAME_SAMPLES` stereo samples (left, right, left, …).
    pub fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) {
        let mut sb = [[[0.0f32; SUBBANDS]; 2]; BLOCKS];
        let mut chan = [0i16; SUBBANDS];
        for (blk, block) in sb.iter_mut().enumerate() {
            for (ch, bands) in block.iter_mut().enumerate() {
                for (i, s) in chan.iter_mut().enumerate() {
                    *s = pcm.get(2 * (blk * SUBBANDS + i) + ch).copied().unwrap_or(0);
                }
                self.analyze(ch, &chan, bands);
            }
        }
        // The scale factor: the smallest 2^(sf+1) larger than the subband peak.
        let mut sf = [[0u8; SUBBANDS]; 2];
        for (ch, row) in sf.iter_mut().enumerate() {
            for (band, f) in row.iter_mut().enumerate() {
                let peak = sb.iter().map(|b| abs(b[ch][band])).fold(0.0f32, f32::max);
                let mut s = 0u8;
                while s < 15 && (2u32 << s) as f32 <= peak {
                    s += 1;
                }
                *f = s;
            }
        }
        let bits = self.allocate(&sf);
        let start = out.len();
        out.extend_from_slice(&[SYNCWORD, self.rate.code() << 6 | 3 << 4 | 2 << 2 | 1, self.bitpool, 0]);
        let mut w = Bits { out, acc: 0, n: 0 };
        for row in &sf {
            for &f in row {
                w.put(f as u32, 4);
            }
        }
        for block in &sb {
            for ch in 0..2 {
                for band in 0..SUBBANDS {
                    let b = bits[ch][band];
                    if b == 0 {
                        continue;
                    }
                    let levels = ((1u32 << b) - 1) as f32;
                    let scale = (2u32 << sf[ch][band]) as f32;
                    let q = ((block[ch][band] / scale + 1.0) * levels / 2.0).clamp(0.0, levels) as u32;
                    w.put(q.min(levels as u32), b as u32);
                }
            }
        }
        w.flush();
        // CRC: two header bytes after the sync word and all scale factors.
        let mut crc = Crc(0x0F);
        crc.byte(out[start + 1]);
        crc.byte(out[start + 2]);
        for i in 0..SUBBANDS {
            crc.byte(out[start + 4 + i]);
        }
        out[start + 3] = crc.0;
    }

    /// Bit allocation over the subbands (stereo, "loudness"), as in B.6.3.2.
    fn allocate(&self, sf: &[[u8; SUBBANDS]; 2]) -> [[u8; SUBBANDS]; 2] {
        let pool = self.bitpool as i32;
        let mut need = [[0i32; SUBBANDS]; 2];
        let mut max_need = 0;
        for ch in 0..2 {
            for band in 0..SUBBANDS {
                need[ch][band] = if sf[ch][band] == 0 {
                    -5
                } else {
                    let loud = sf[ch][band] as i32 - OFFSET8[band];
                    if loud > 0 { loud / 2 } else { loud }
                };
                max_need = max_need.max(need[ch][band]);
            }
        }
        let (mut count, mut slices, mut slice) = (0, 0, max_need + 1);
        loop {
            slice -= 1;
            count += slices;
            slices = 0;
            for row in &need {
                for &n in row {
                    if n > slice + 1 && n < slice + 16 {
                        slices += 1;
                    } else if n == slice + 1 {
                        slices += 2;
                    }
                }
            }
            if count + slices >= pool {
                break;
            }
        }
        if count + slices == pool {
            count += slices;
            slice -= 1;
        }
        let mut bits = [[0i32; SUBBANDS]; 2];
        for ch in 0..2 {
            for band in 0..SUBBANDS {
                bits[ch][band] = if need[ch][band] < slice + 2 { 0 } else { (need[ch][band] - slice).min(16) };
            }
        }
        let (mut ch, mut band) = (0, 0);
        while count < pool && band < SUBBANDS {
            if bits[ch][band] >= 2 && bits[ch][band] < 16 {
                bits[ch][band] += 1;
                count += 1;
            } else if need[ch][band] == slice + 1 && pool > count + 1 {
                bits[ch][band] = 2;
                count += 2;
            }
            if ch == 1 {
                ch = 0;
                band += 1;
            } else {
                ch = 1;
            }
        }
        (ch, band) = (0, 0);
        while count < pool && band < SUBBANDS {
            if bits[ch][band] < 16 {
                bits[ch][band] += 1;
                count += 1;
            }
            if ch == 1 {
                ch = 0;
                band += 1;
            } else {
                ch = 1;
            }
        }
        bits.map(|row| row.map(|b| b as u8))
    }
}

fn abs(v: f32) -> f32 {
    if v < 0.0 { -v } else { v }
}

/// Writing bits, most significant first.
struct Bits<'a> {
    out: &'a mut Vec<u8>,
    acc: u32,
    n: u32,
}

impl Bits<'_> {
    fn put(&mut self, v: u32, len: u32) {
        for i in (0..len).rev() {
            self.acc = self.acc << 1 | (v >> i & 1);
            self.n += 1;
            if self.n == 8 {
                self.out.push(self.acc as u8);
                (self.acc, self.n) = (0, 0);
            }
        }
    }
    fn flush(&mut self) {
        if self.n > 0 {
            self.out.push((self.acc << (8 - self.n)) as u8);
            (self.acc, self.n) = (0, 0);
        }
    }
}

/// The SBC CRC-8: x^8 + x^4 + x^3 + x^2 + 1, initial 0x0F, most significant bit first.
struct Crc(u8);

impl Crc {
    fn byte(&mut self, b: u8) {
        for i in (0..8).rev() {
            let bit = (b >> i & 1) ^ (self.0 >> 7);
            self.0 <<= 1;
            if bit == 1 {
                self.0 ^= 0x1D;
            }
        }
    }
}
