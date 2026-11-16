//! The boot splash: a background, a title and a running bar, showing that the system is alive.
//!
//! It appears before everything else, so it depends neither on fonts (parsing them itself
//! takes noticeable time) nor on floating point: the title is baked into a mask
//! (`tools/mksplash.py`), and everything is computed in integers.

use crate::gfx::{Color, Framebuffer, Rect, mix};
use crate::sync::IrqLock;
use crate::{arch, interrupts, task};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

static LOGO: &[u8] = include_bytes!("../assets/splash-logo.bin");
const LOGO_HEADER: usize = 4;
/// The title height is this fraction of the screen height.
const LOGO_SHARE: i32 = 14;
const CENTER_PERCENT: i32 = 45;
const TEXT_COLOR: Color = 0xB9C2DC;
const SHINE_COLOR: Color = 0xFFFFFF;
const BAR_COLOR: Color = 0x7DD3FC;
const BAR_TRACK_ALPHA: u32 = 34;
/// CPU cycles per animation time unit: about a third of a millisecond.
const CLOCK_SHIFT: u32 = 20;
const BAR_PERIOD: u64 = 4096;
const SHINE_SLOWDOWN: u64 = 2;
const FRAME_TICKS: u64 = interrupts::HZ / 30;
const FROM: Color = 0x0F1226;
const TO: Color = 0x241640;
/// Glows: the centre and radius in fractions of the screen (out of 1024), the colour, the strength out of 256.
const GLOWS: [(i32, i32, i32, Color, i32); 3] = [(205, 256, 666, 0x22D3EE, 62), (870, 870, 768, 0xEC4899, 54), (614, 51, 512, 0x818CF8, 36)];
const FULL: i32 = 1024;

static SCREENS: crate::sync::Mutex<Vec<Screen>> = crate::sync::Mutex::new(Vec::new());
static RUNNING: AtomicBool = AtomicBool::new(false);

struct Screen {
    frame: Framebuffer,
    /// The part of the screen with the title and the bar, its background and the frame in progress.
    area: Rect,
    backdrop: Vec<u32>,
    canvas: Vec<u32>,
    /// The title transparency, by rows.
    logo: Vec<u8>,
    logo_size: (i32, i32),
    bar: Rect,
}

fn channels(c: Color) -> [i32; 3] {
    [(c >> 16 & 0xFF) as i32, (c >> 8 & 0xFF) as i32, (c & 0xFF) as i32]
}

/// A background row: a gradient and glows. An ordered dither hides the steps between shades.
fn background_row(y: i32, w: i32, h: i32, offsets: &[[i32; GLOWS.len()]], out: &mut [u32]) {
    let (from, to) = (channels(FROM), channels(TO));
    let rows: [i32; GLOWS.len()] = core::array::from_fn(|i| {
        let d = y * FULL / h - GLOWS[i].1;
        d * d
    });
    for (x, pixel) in out.iter_mut().enumerate() {
        let t = (x as i32 * 256 / w + y * 256 / h) / 2;
        let mut color: [i32; 3] = core::array::from_fn(|c| from[c] * (256 - t) + to[c] * t);
        for (g, glow) in GLOWS.iter().enumerate() {
            let r2 = glow.2 * glow.2;
            let left = r2 - offsets[x][g] - rows[g];
            if left <= 0 {
                continue;
            }
            let k = left * ((256 << 16) / r2) >> 16;
            let a = (k * k >> 8) * glow.4 >> 8;
            let light = channels(glow.3);
            for c in 0..3 {
                color[c] += (light[c] * 256 - color[c]) * a >> 8;
            }
        }
        let dither = if (x as i32 ^ y) & 1 == 0 { 0 } else { 128 };
        let [r, g, b] = color.map(|c| ((c + dither) >> 8).clamp(0, 255) as u32);
        *pixel = 0xFF00_0000 | r << 16 | g << 8 | b;
    }
}

/// The title mask at the needed size: bilinear from the baked one.
fn scaled_logo(height: i32) -> (Vec<u8>, (i32, i32)) {
    let (sw, sh) = (u16::from_le_bytes([LOGO[0], LOGO[1]]) as i32, u16::from_le_bytes([LOGO[2], LOGO[3]]) as i32);
    let source = &LOGO[LOGO_HEADER..];
    let at = |x: i32, y: i32| source[(y.min(sh - 1) * sw + x.min(sw - 1)) as usize] as i32;
    let width = sw * height / sh;
    let mut out = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        let fy = y * sh * 256 / height;
        let (iy, ty) = (fy >> 8, fy & 0xFF);
        for x in 0..width {
            let fx = x * sw * 256 / width;
            let (ix, tx) = (fx >> 8, fx & 0xFF);
            let top = at(ix, iy) * (256 - tx) + at(ix + 1, iy) * tx;
            let bottom = at(ix, iy + 1) * (256 - tx) + at(ix + 1, iy + 1) * tx;
            out.push(((top * (256 - ty) + bottom * ty) >> 16) as u8);
        }
    }
    (out, (width, height))
}

impl Screen {
    fn new(frame: Framebuffer) -> Self {
        let (w, h) = (frame.w, frame.h);
        let (logo, logo_size) = scaled_logo(h / LOGO_SHARE);
        let bar_w = logo_size.0 * 7 / 10;
        let bar_h = (h / 200).max(3);
        let gap = logo_size.1 * 7 / 10;
        let total = logo_size.1 + gap + bar_h;
        let area = Rect::new((w - logo_size.0) / 2, h * CENTER_PERCENT / 100 - total / 2, logo_size.0, total);
        let bar = Rect::new((logo_size.0 - bar_w) / 2, total - bar_h, bar_w, bar_h);
        let offsets: Vec<[i32; GLOWS.len()]> = (0..w)
            .map(|x| {
                core::array::from_fn(|g| {
                    let d = x * FULL / w - GLOWS[g].0;
                    d * d
                })
            })
            .collect();
        let mut row = vec![0u32; w as usize];
        let mut backdrop = Vec::with_capacity((area.w * area.h) as usize);
        for y in 0..h {
            background_row(y, w, h, &offsets, &mut row);
            frame.write(Rect::new(0, y, w, 1), &row);
            if (area.y..area.y + area.h).contains(&y) {
                backdrop.extend_from_slice(&row[area.x as usize..(area.x + area.w) as usize]);
            }
        }
        let canvas = backdrop.clone();
        let mut screen = Self { frame, area, backdrop, canvas, logo, logo_size, bar };
        screen.draw(clock());
        screen
    }

    fn draw(&mut self, time: u64) {
        self.canvas.copy_from_slice(&self.backdrop);
        let (w, h) = self.logo_size;
        // A highlight passes over the letters and leaves past the edge; between passes the title is calm.
        let width = h;
        let cycle = (w + h) as u64 * 2;
        let shine = ((time >> SHINE_SLOWDOWN) % cycle) as i32 - width;
        for y in 0..h {
            for x in 0..w {
                let index = (y * w + x) as usize;
                let alpha = self.logo[index] as u32;
                if alpha == 0 {
                    continue;
                }
                let distance = (x + y / 2 - shine).abs();
                let glint = ((width - distance).max(0) * 256 / width) as u32;
                let color = mix(SHINE_COLOR, TEXT_COLOR | 0xFF00_0000, glint) & 0xFF_FFFF;
                self.canvas[index] = mix(color, self.canvas[index], alpha + (alpha >> 7));
            }
        }
        // The bar runs from left to right and leaves past the edge, bright in the middle.
        let bar = self.bar;
        let head_width = bar.w / 3;
        let head = ((time % BAR_PERIOD) as i32 * (bar.w + 2 * head_width) / BAR_PERIOD as i32) - head_width;
        for y in 0..bar.h {
            for x in 0..bar.w {
                let corner = (x == 0 || x == bar.w - 1) && (y == 0 || y == bar.h - 1);
                if corner {
                    continue;
                }
                let index = ((bar.y + y) * self.area.w + bar.x + x) as usize;
                let track = mix(0xFFFFFF, self.canvas[index], BAR_TRACK_ALPHA);
                let glow = ((head_width - (x - head).abs()).max(0) * 256 / head_width) as u32;
                self.canvas[index] = mix(BAR_COLOR, track, glow);
            }
        }
        self.frame.write(self.area, &self.canvas);
    }
}

fn clock() -> u64 {
    arch::rdtsc() >> CLOCK_SHIFT
}

/// The first picture. Works while the firmware is still alive.
pub fn show(frame: Framebuffer) {
    // The picture is prepared without the lock: it takes long (~0.2 s), and the lock disables interrupts.
    let screen = Screen::new(frame);
    SCREENS.lock().push(screen);
}

/// The next frame: while there is no timer, whoever does long work asks for it.
pub fn step() {
    let time = clock();
    for screen in SCREENS.lock().iter_mut() {
        screen.draw(time);
    }
}

/// From here on a separate thread takes care of the animation.
pub fn animate() {
    RUNNING.store(true, Ordering::Relaxed);
    task::spawn("splash", None, || {
        while RUNNING.load(Ordering::Relaxed) {
            step();
            task::sleep_until(interrupts::ticks() + FRAME_TICKS);
        }
    });
}

/// Other screens were connected or their mode changed: the splash goes on those that work.
pub fn screens(frames: &[Framebuffer]) {
    let same = |a: &Framebuffer, b: &Framebuffer| (a.base, a.w, a.h, a.stride) == (b.base, b.w, b.h, b.stride);
    let added: Vec<Screen> = frames.iter().filter(|f| !SCREENS.lock().iter().any(|s| same(&s.frame, f))).map(|f| Screen::new(*f)).collect();
    let mut list = SCREENS.lock();
    list.retain(|s| frames.iter().any(|f| same(&s.frame, f)));
    list.extend(added);
}

/// The desktop is ready. After returning, the splash no longer touches the screen.
pub fn finish() {
    RUNNING.store(false, Ordering::Relaxed);
    *SCREENS.lock() = Vec::new();
}

/// A mark in the log: how many cycles passed by this boot step.
pub fn mark(step: &str) {
    crate::kprintln!("boot: {step} at {} Mcycles", arch::rdtsc() / 1_000_000);
}
