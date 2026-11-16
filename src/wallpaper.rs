//! Wallpapers. They all share one base: a diagonal gradient and soft coloured "glows".
//! A type (`STYLES`) draws its own pattern on top of it, a colour variant (`PRESETS`) sets the paints.
//! The wallpaper number in the settings is type × number of variants + variant.
//!
//! The picture is computed at half resolution and stretched bilinearly — for a smooth gradient this
//! is invisible, and it is four times less work (this dates from when the kernel had no SSE and
//! floating point was emulated in software).

use crate::gfx::Color;
use crate::smp::{self, SharedMut};
use alloc::vec;
use alloc::vec::Vec;
use libm::{powf, sinf, sqrtf};

pub const STYLES: [&str; 6] = ["Glow", "Waves", "Mountains", "Polygons", "Stars", "Bubbles"];
/// How many wallpapers there are in total: each type in each colour variant.
pub const COUNT: usize = STYLES.len() * PRESETS.len();

/// The wallpaper number by type and colour variant.
pub fn number(style: usize, preset: usize) -> usize {
    style * PRESETS.len() + preset
}

/// The type and colour variant of the wallpaper with this number.
pub fn parts(number: usize) -> (usize, usize) {
    let n = number.min(COUNT - 1);
    (n / PRESETS.len(), n % PRESETS.len())
}

pub struct Preset {
    pub name: &'static str,
    /// The gradient colours: top left and bottom right.
    pub from: Color,
    pub to: Color,
    /// Glows: (x, y, radius — fractions of the screen; colour; strength 0..1).
    pub glows: [(f32, f32, f32, Color, f32); 3],
}

pub const PRESETS: [Preset; 5] = [
    Preset {
        name: "Aurora",
        from: 0x0F1226,
        to: 0x241640,
        glows: [(0.20, 0.25, 0.65, 0x22D3EE, 0.28), (0.85, 0.85, 0.75, 0xEC4899, 0.24), (0.60, 0.05, 0.50, 0x818CF8, 0.16)],
    },
    Preset {
        name: "Sunset",
        from: 0x1A0F2E,
        to: 0x3D1530,
        glows: [(0.75, 0.85, 0.70, 0xFB923C, 0.30), (0.20, 0.90, 0.60, 0xF472B6, 0.25), (0.50, 0.10, 0.50, 0x8B5CF6, 0.18)],
    },
    Preset {
        name: "Ocean",
        from: 0x041628,
        to: 0x0B2D4A,
        glows: [(0.80, 0.20, 0.60, 0x22D3EE, 0.25), (0.15, 0.80, 0.70, 0x3B82F6, 0.25), (0.50, 0.50, 0.40, 0x14B8A6, 0.10)],
    },
    Preset {
        name: "Forest",
        from: 0x0A1A14,
        to: 0x13291E,
        glows: [(0.20, 0.20, 0.60, 0x4ADE80, 0.18), (0.85, 0.75, 0.60, 0xA3E635, 0.14), (0.60, 0.05, 0.50, 0x2DD4BF, 0.12)],
    },
    Preset {
        name: "Graphite",
        from: 0x101114,
        to: 0x1F2126,
        glows: [(0.20, 0.20, 0.60, 0x9CA3AF, 0.10), (0.85, 0.80, 0.70, 0x64748B, 0.14), (0.60, 0.00, 0.50, 0x60A5FA, 0.06)],
    },
];

fn rgb(c: Color) -> [f32; 3] {
    [((c >> 16) & 0xFF) as f32, ((c >> 8) & 0xFF) as f32, (c & 0xFF) as f32]
}

/// The wallpaper colour at point (x, y) of (w, h) — channels as numbers 0..255.
fn sample(p: &Preset, x: f32, y: f32, w: f32, h: f32) -> [f32; 3] {
    let (c0, c1) = (rgb(p.from), rgb(p.to));
    let t = (x / w) * 0.55 + (y / h) * 0.45;
    let mut col = [0.0f32; 3];
    for i in 0..3 {
        col[i] = c0[i] + (c1[i] - c0[i]) * t;
    }
    for &(gx, gy, radius, color, strength) in &p.glows {
        let (dx, dy) = (x - gx * w, y - gy * h);
        let d = sqrtf(dx * dx + dy * dy) / (radius * h);
        if d < 1.0 {
            let k = powf(1.0 - d, 2.0) * strength;
            let gc = rgb(color);
            for i in 0..3 {
                col[i] += (gc[i] - col[i]) * k;
            }
        }
    }
    col
}

/// Wallpaper number `number` of size w×h.
pub fn generate(number: usize, w: i32, h: i32) -> Vec<u32> {
    let (style, preset) = parts(number);
    let p = &PRESETS[preset];
    let mut px = backdrop(p, w, h);
    let mut art = Art { px: &mut px, w, h, p };
    match style {
        1 => art.waves(),
        2 => art.mountains(),
        3 => art.polygons(),
        4 => art.stars(),
        5 => art.bubbles(),
        _ => {}
    }
    px
}

fn backdrop(p: &Preset, w: i32, h: i32) -> Vec<u32> {
    // A grid at half resolution (with a one-pixel margin at the edges), values ×16.
    let (sw, sh) = (w / 2 + 2, h / 2 + 2);
    let mut small = vec![[0i32; 3]; (sw * sh) as usize];
    let grid = SharedMut(small.as_mut_ptr());
    // Wallpapers take long to compute: the rows are independent, hand them to all cores.
    smp::parallel(sh as usize, &|sy| {
        let sy = sy as i32;
        for sx in 0..sw {
            let col = sample(p, (sx * 2) as f32, (sy * 2) as f32, w as f32, h as f32);
            unsafe { *grid.get().add((sy * sw + sx) as usize) = col.map(|v| (v * 16.0) as i32) };
        }
    });
    let small = &small;
    let mut px = vec![0u32; (w * h) as usize];
    let out = SharedMut(px.as_mut_ptr());
    smp::parallel(h as usize, &|y| {
        let y = y as i32;
        let (sy, fy) = (y / 2, y % 2);
        for x in 0..w {
            let (sx, fx) = (x / 2, x % 2);
            let at = |dx: i32, dy: i32| small[((sy + dy) * sw + sx + dx) as usize];
            let (a, b, c, d) = (at(0, 0), at(1, 0), at(0, 1), at(1, 1));
            // Light dithering so there are no bands on the gradient.
            let dither = ((x * 7 + y * 13) % 4) * 4 - 6;
            let ch = |i: usize| {
                let top = a[i] * (2 - fx) + b[i] * fx;
                let bottom = c[i] * (2 - fx) + d[i] * fx;
                (((top * (2 - fy) + bottom * fy) / 4 + dither) / 16).clamp(0, 255) as u32
            };
            unsafe { *out.get().add((y * w + x) as usize) = 0xFF00_0000 | ch(0) << 16 | ch(1) << 8 | ch(2) };
        }
    });
    px
}

/// A random number from a seed: the same pattern on every boot.
fn scatter(n: u32) -> u32 {
    let n = n.wrapping_mul(0x9E37_79B1) ^ n >> 15;
    let n = n.wrapping_mul(0x85EB_CA77) ^ n >> 13;
    n.wrapping_mul(0xC2B2_AE3D) ^ n >> 16
}

/// A random fraction 0..1.
fn unit(n: u32) -> f32 {
    (scatter(n) >> 16) as f32 / 65536.0
}

/// A colour mix: `alpha` is the share of `to`, 0..=256.
fn mix(from: u32, to: u32, alpha: i32) -> u32 {
    let ch = |shift: u32| {
        let (a, b) = ((from >> shift & 0xFF) as i32, (to >> shift & 0xFF) as i32);
        ((a + (b - a) * alpha / 256) as u32) << shift
    };
    0xFF00_0000 | ch(16) | ch(8) | ch(0)
}

/// The wallpaper canvas and the variant's paints.
struct Art<'a> {
    px: &'a mut [u32],
    w: i32,
    h: i32,
    p: &'a Preset,
}

impl Art<'_> {
    fn at(&mut self, x: i32, y: i32) -> Option<&mut u32> {
        ((0..self.w).contains(&x) && (0..self.h).contains(&y)).then(|| &mut self.px[(y * self.w + x) as usize])
    }

    fn blend(&mut self, x: i32, y: i32, color: u32, alpha: i32) {
        if let Some(p) = self.at(x, y) {
            *p = mix(*p, color, alpha);
        }
    }

    /// Silk ribbons: brighter at the crest, fading out with depth.
    fn waves(&mut self) {
        const LAYERS: i32 = 6;
        let (w, h) = (self.w as f32, self.h as f32);
        for layer in 0..LAYERS {
            let (l, color) = (layer as f32, self.p.glows[layer as usize % 3].3);
            let base = h * (0.34 + 0.115 * l);
            let (slow, fast) = (core::f32::consts::TAU * (0.6 + 0.31 * l) / w, core::f32::consts::TAU * (1.7 + 0.53 * l) / w);
            let fade = (h * 0.36) as i32;
            for x in 0..self.w {
                let fx = x as f32;
                let crest = base + h * 0.075 * sinf(fx * slow + l * 1.9) + h * 0.026 * sinf(fx * fast + l * 0.8);
                let top = crest as i32;
                for y in top.max(0)..(top + fade).min(self.h) {
                    let depth = y - top;
                    let dither = (x * 7 + y * 13) % 4;
                    let alpha = 32 * (fade - depth) / fade + if depth < 2 { 28 } else { 0 } + (dither > 1) as i32;
                    self.blend(x, y, color, alpha);
                }
            }
        }
    }

    /// Ridges one after another: the far ones sink into haze, the nearest is almost black.
    fn mountains(&mut self) {
        const LAYERS: u32 = 5;
        let (w, h) = (self.w as f32, self.h as f32);
        let haze = mix(self.p.to, self.p.glows[0].3, 96);
        let night = mix(self.p.from, 0, 150);
        for layer in 0..LAYERS {
            let near = layer as i32 * 256 / (LAYERS as i32 - 1);
            let color = mix(haze, night, near);
            let mist = mix(color, haze, 60);
            let base = h * (0.56 + 0.095 * layer as f32);
            let height = h * (0.30 - 0.035 * layer as f32);
            // Broken noise: sharp peaks come out by themselves.
            let ridge = |x: f32| {
                let (mut sum, mut amp, mut scale) = (0.0, 1.0, (3.0 + layer as f32) / w);
                for octave in 0..5u32 {
                    let t = x * scale + octave as f32 * 7.0;
                    let (cell, part) = (t as u32, t - (t as u32) as f32);
                    let key = layer * 4096 + octave * 512;
                    let (a, b) = (unit(key + cell), unit(key + cell + 1));
                    sum += (a + (b - a) * part) * amp;
                    amp *= 0.5;
                    scale *= 2.1;
                }
                sum / 1.94
            };
            for x in 0..self.w {
                let top = base - height * ridge(x as f32);
                let (first, edge) = (top as i32, ((top - (top as i32) as f32) * 256.0) as i32);
                self.blend(x, first, color, 256 - edge);
                let fall = (h * 0.30) as i32;
                for y in (first + 1).max(0)..self.h {
                    let shade = mix(color, mist, ((y - first) * 256 / fall).min(256));
                    self.blend(x, y, shade, 256);
                }
            }
        }
    }

    /// A triangle mosaic: each is filled with the base colour at its centre, slightly lighter or darker.
    fn polygons(&mut self) {
        let cell = (self.h / 7).max(10);
        let (cols, rows) = (self.w / cell + 3, self.h / cell + 3);
        let corner = |cx: i32, cy: i32| {
            let id = (cy * 1021 + cx) as u32;
            let shift = |n: u32| ((unit(n) - 0.5) * 0.76 * cell as f32) as i32;
            ((cx - 1) * cell + shift(id * 2), (cy - 1) * cell + shift(id * 2 + 1))
        };
        let base = self.px.to_vec();
        for cy in 0..rows {
            for cx in 0..cols {
                let (a, b, c, d) = (corner(cx, cy), corner(cx + 1, cy), corner(cx, cy + 1), corner(cx + 1, cy + 1));
                let id = (cy * 2039 + cx) as u32;
                self.triangle(&base, [a, b, c], id * 2);
                self.triangle(&base, [b, d, c], id * 2 + 1);
            }
        }
    }

    fn triangle(&mut self, base: &[u32], [a, b, c]: [(i32, i32); 3], id: u32) {
        let (w, h) = (self.w, self.h);
        let (mx, my) = (((a.0 + b.0 + c.0) / 3).clamp(0, w - 1), ((a.1 + b.1 + c.1) / 3).clamp(0, h - 1));
        let light = (unit(id) * 30.0) as i32;
        let tone = base[(my * w + mx) as usize];
        let color = if id % 3 == 0 { mix(tone, 0xFFFFFF, light / 2) } else { mix(tone, self.p.glows[(id % 3) as usize].3, light) };
        let side = |p: (i32, i32), q: (i32, i32), x: i32, y: i32| (q.0 - p.0) * (y - p.1) - (q.1 - p.1) * (x - p.0);
        for y in a.1.min(b.1).min(c.1).max(0)..=a.1.max(b.1).max(c.1).min(h - 1) {
            for x in a.0.min(b.0).min(c.0).max(0)..=a.0.max(b.0).max(c.0).min(w - 1) {
                let (e0, e1, e2) = (side(a, b, x, y), side(b, c, x, y), side(c, a, x, y));
                if (e0 >= 0 && e1 >= 0 && e2 >= 0) || (e0 <= 0 && e1 <= 0 && e2 <= 0) {
                    self.px[(y * w + x) as usize] = color;
                }
            }
        }
    }

    /// Stars: half are scattered over the whole sky, half gathered into a milky band.
    fn stars(&mut self) {
        let (w, h) = (self.w as f32, self.h as f32);
        let count = (self.w * self.h / 1700).max(40) as u32;
        for n in 0..count {
            let x = unit(n * 4) * w;
            let y = if n % 2 == 0 {
                unit(n * 4 + 1) * h
            } else {
                let spread = unit(n * 4 + 1) + unit(n * 4 + 2) + unit(n * 4 + 3) - 1.5;
                h * (0.82 - 0.66 * x / w) + spread * h * 0.22
            };
            let (x, y) = (x as i32, y as i32);
            let glow = self.p.glows[n as usize % 3].3;
            let color = mix(0xFFFFFF, glow, (unit(n * 5) * 110.0) as i32);
            let bright = unit(n * 7);
            let alpha = (50.0 + bright * bright * 206.0) as i32;
            self.blend(x, y, color, alpha);
            // Rare bright ones — with rays.
            if n % 37 == 0 {
                self.blend(x, y, 0xFFFFFF, 256);
                for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                    self.blend(x + dx, y + dy, color, 150);
                    self.blend(x + 2 * dx, y + 2 * dy, color, 50);
                }
            }
        }
    }

    /// Blurred circles of light: an almost transparent fill, the edge slightly brighter.
    fn bubbles(&mut self) {
        const COUNT: u32 = 34;
        let (w, h) = (self.w as f32, self.h as f32);
        for n in 0..COUNT {
            let (cx, cy) = ((unit(n * 3) * w) as i32, (unit(n * 3 + 1) * h) as i32);
            let r = ((0.03 + unit(n * 3 + 2) * unit(n * 3 + 2) * 0.15) * h).max(3.0) as i32;
            let (color, fill) = (self.p.glows[n as usize % 3].3, 12 + (n as i32 * 7) % 26);
            let (outer, rim) = (r * r, (r - 2).max(0) * (r - 2).max(0));
            for y in (cy - r).max(0)..(cy + r + 1).min(self.h) {
                for x in (cx - r).max(0)..(cx + r + 1).min(self.w) {
                    let d = (x - cx) * (x - cx) + (y - cy) * (y - cy);
                    if d <= outer {
                        self.blend(x, y, color, if d >= rim { fill * 2 + 14 } else { fill });
                    }
                }
            }
        }
    }
}

/// Wallpapers of the working screens: each has its own picture in its pixels.
pub fn desktop(preset: usize) -> Vec<Vec<u32>> {
    crate::output::shown().into_iter().map(|(w, h, _)| generate(preset, w, h)).collect()
}
