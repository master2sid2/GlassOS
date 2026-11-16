//! The screensaver: glass balls float over the dimmed wallpaper and refract it together with
//! the clock. Only the places around the balls and the clock are updated, so the screensaver barely
//! loads the CPU. Once a minute the clock moves to a new place: the screen does not burn in.

use crate::font::{self, FontId};
use crate::gfx::{Canvas, Color, Rect, SCALE_100, mix};
use crate::input::Event;
use crate::interrupts::{self, HZ};
use crate::rtc::Time;
use crate::wm::Ctx;
use crate::{arch, theme, wallpaper};
use alloc::format;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

/// For how many seconds after start input does not dismiss the screensaver: otherwise "Preview" would close
/// it with the release of the same button.
const GRACE_TICKS: u64 = HZ;
const FRAME_TICKS: u64 = 3;
/// How far the mouse must move: an accidental bump of the desk does not dismiss the screensaver.
const MOVE_THRESHOLD: i32 = 4;
const DIM: u32 = 120;
const TINT_ALPHA: u32 = 22;
/// Balls: the radius in thousandths of the screen height, the speed in pixels per second per 1000 pixels
/// of height, the glass tint.
const ORBS: [(i32, i32, Color); 5] = [(165, 34, 0x67E8F9), (115, 46, 0xC4B5FD), (92, 58, 0xF9A8D4), (138, 40, 0x93C5FD), (70, 70, 0xFFFFFF)];
/// Flight directions per 256 units of speed.
const HEADINGS: [(i32, i32); 8] = [(181, 181), (-181, 181), (181, -181), (-181, -181), (236, 98), (-98, 236), (98, -236), (-236, -98)];
const FIXED_BITS: i32 = 8;
/// A solid glass ball over the background: the refractive index of the glass and the distance from
/// the ball centre to the background in radii. The background is beyond the focus, so the picture in the ball
/// is upside down and runs forward while it flies, as if the ball were rolling.
const GLASS: f64 = 1.5;
const BACKDROP_DEPTH: f64 = 2.0;
/// Beyond this (in radii from the centre) the ball does not take the background: at the very rim the rays go
/// almost sideways, and there is a highlight there anyway.
const SEEN_MAX: f64 = 1.7;
const RIM_LIGHT: i32 = 170;
const RIM_DARK: i32 = 90;
const SHINE: i32 = 240;
const GLOW: i32 = 70;
const CLOCK_SHARE: f32 = 6.5;
const DATE_SHARE: f32 = 26.0;
const CLOCK_COLOR: Color = 0xE8ECF8;
const DATE_COLOR: Color = 0xA9B3CC;

static REQUESTED: AtomicBool = AtomicBool::new(false);
static DISPLAY_OFF: AtomicBool = AtomicBool::new(false);

/// Show the screensaver now, without waiting for idle time.
pub fn request() {
    REQUESTED.store(true, Ordering::Relaxed);
}

pub fn take_request() -> bool {
    REQUESTED.swap(false, Ordering::Relaxed)
}

/// Turn off the display now, without waiting for idle time.
pub fn display_off() {
    DISPLAY_OFF.store(true, Ordering::Relaxed);
}

pub fn take_display_off() -> bool {
    DISPLAY_OFF.swap(false, Ordering::Relaxed)
}

/// The event says someone is at the computer.
pub fn activity(event: &Event, pointer: (i32, i32)) -> bool {
    match *event {
        Event::Mouse { x, y, buttons, wheel } => buttons != 0 || wheel != 0 || (x - pointer.0).abs() + (y - pointer.1).abs() >= MOVE_THRESHOLD,
        _ => true,
    }
}

/// The idle time after which the screensaver starts. None — off.
pub fn timeout() -> Option<u64> {
    match theme::get().saver_minutes {
        0 => None,
        minutes => Some(minutes as u64 * 60 * HZ),
    }
}

/// The idle time after which the display turns off. None — it does not turn off.
pub fn display_timeout() -> Option<u64> {
    match theme::get().display_off_minutes {
        0 => None,
        minutes => Some(minutes as u64 * 60 * HZ),
    }
}

fn isqrt(n: u32) -> u32 {
    let (mut root, mut bit) = (0u32, 1u32 << 30);
    let mut left = n;
    while bit > left {
        bit >>= 2;
    }
    while bit != 0 {
        if left >= root + bit {
            left -= root + bit;
            root = (root >> 1) + bit;
        } else {
            root >>= 1;
        }
        bit >>= 2;
    }
    root
}

#[derive(Clone, Copy, Default)]
struct Cell {
    /// Where to take the background for this point of the ball.
    dx: i16,
    dy: i16,
    light: u8,
    dark: u8,
    /// 0 — outside the ball, 255 — entirely on it.
    cover: u8,
}

/// A point beyond the screen edge is reflected inside: this way there are no stripes from repeating one pixel at the edge.
fn reflect(at: i32, len: i32) -> i32 {
    let at = if at < 0 { -at } else { at };
    let at = if at >= len { 2 * (len - 1) - at } else { at };
    at.clamp(0, len - 1)
}

/// A highlight as an elongated spot: 0..=256.
fn spot(x: i32, y: i32, cx: i32, cy: i32, size: i32) -> i32 {
    let (dx, dy) = ((x - cx) * 256 / size, (y - cy) * 340 / size);
    let d = 65536 - (dx * dx + dy * dy).min(65536);
    (d >> 8) * (d >> 8) >> 8
}

/// Where a ray falling vertically on the ball at distance `rho` radii from the centre meets the background
/// (in radii from the centre, signed: minus — on the other side). The ray is refracted on
/// entering the glass and on leaving it, following Snell's law.
fn backdrop_hit(rho: f64) -> f64 {
    let refract = |d: (f64, f64), n: (f64, f64), eta: f64| {
        let cos = -(d.0 * n.0 + d.1 * n.1);
        let k = (1.0 - eta * eta * (1.0 - cos * cos)).max(0.0);
        let f = eta * cos - libm::sqrt(k);
        (eta * d.0 + f * n.0, eta * d.1 + f * n.1)
    };
    let p = (rho, libm::sqrt(1.0 - rho * rho));
    let d = refract((0.0, -1.0), p, 1.0 / GLASS);
    // Leaving the ball: the second intersection of the ray with the circle.
    let b = p.0 * d.0 + p.1 * d.1;
    let t = -2.0 * b;
    let q = (p.0 + t * d.0, p.1 + t * d.1);
    let out = refract(d, (-q.0, -q.1), GLASS);
    if out.1 >= -1e-6 {
        return -SEEN_MAX;
    }
    let s = (-BACKDROP_DEPTH - q.1) / out.1;
    (q.0 + s * out.0).clamp(-SEEN_MAX, SEEN_MAX)
}

/// A precomputed ball of radius `r`: per frame only the background remains to be substituted.
fn lens(r: i32) -> Vec<Cell> {
    let side = 2 * r + 1;
    let mut cells = alloc::vec![Cell::default(); (side * side) as usize];
    for y in -r..=r {
        for x in -r..=r {
            let square = x * x + y * y;
            let edge = (r << 4) - isqrt((square as u32) << 8) as i32;
            if edge <= -8 {
                continue;
            }
            // The ball's height above the plane: 256 in the centre, 0 at the edge.
            let inside = ((square as i64 * 65536 / (r * r) as i64) as i32).min(65536);
            let rim = 256 - isqrt((65536 - inside) as u32) as i32;
            let rim2 = rim * rim >> 8;
            let shine = spot(x, y, -r * 38 / 100, -r * 42 / 100, r * 30 / 100) * SHINE >> 8;
            let glow = spot(x, y, r * 32 / 100, r * 52 / 100, r * 55 / 100) * GLOW >> 8;
            let light = (rim2 * rim >> 8) * RIM_LIGHT >> 8;
            let dist = libm::sqrt(square as f64);
            let at = if dist < 0.5 { 0.0 } else { backdrop_hit((dist / r as f64).min(0.999)) * r as f64 / dist };
            let seen = (x as f64 * at, y as f64 * at);
            cells[((y + r) * side + x + r) as usize] = Cell {
                dx: (seen.0 - x as f64) as i16,
                dy: (seen.1 - y as f64) as i16,
                light: (light + shine + glow).min(255) as u8,
                dark: (rim2 * RIM_DARK >> 8).min(255) as u8,
                cover: ((edge + 8) * 16).clamp(1, 255) as u8,
            };
        }
    }
    cells
}

struct Orb {
    radius: i32,
    cells: Vec<Cell>,
    tint: Color,
    /// The centre and the speed per tick: in 1/256 of a pixel.
    x: i32,
    y: i32,
    vx: i32,
    vy: i32,
    /// Its own speed: after a collision the ball slowly returns to it.
    pace: i32,
}

/// What share of the difference from its own speed a ball recovers per tick (in 1/4096).
const PACE_PULL: i32 = 12;

impl Orb {
    fn rect(&self) -> Rect {
        let r = self.radius;
        Rect::new((self.x >> FIXED_BITS) - r, (self.y >> FIXED_BITS) - r, 2 * r + 1, 2 * r + 1)
    }

    fn advance(&mut self, ticks: i32, w: i32, h: i32) {
        if ticks > 0 {
            let speed = libm::hypot(self.vx as f64, self.vy as f64);
            if speed > 0.5 {
                let pull = (PACE_PULL * ticks).min(4096) as f64 / 4096.0;
                let wanted = speed + (self.pace as f64 - speed) * pull;
                self.vx = libm::round(self.vx as f64 * wanted / speed) as i32;
                self.vy = libm::round(self.vy as f64 * wanted / speed) as i32;
            }
        }
        let bounce = |at: &mut i32, speed: &mut i32, limit: i32, r: i32| {
            *at += *speed * ticks;
            let (low, high) = (r << FIXED_BITS, (limit - r) << FIXED_BITS);
            // At the edge the speed points inwards, even if a neighbour pushed the ball there.
            if *at < low {
                *at = low;
                *speed = speed.abs();
            } else if *at > high {
                *at = high;
                *speed = -speed.abs();
            }
        };
        bounce(&mut self.x, &mut self.vx, w, self.radius);
        bounce(&mut self.y, &mut self.vy, h, self.radius);
    }
}

/// Balls repel each other elastically: the mass is by volume, so a large ball
/// hardly changes course, and a small one bounces off. One that overlapped a neighbour (this
/// happens on appearance) moves back.
fn collide(orbs: &mut [Orb]) {
    for i in 0..orbs.len() {
        for j in i + 1..orbs.len() {
            let (left, right) = orbs.split_at_mut(j);
            let (a, b) = (&mut left[i], &mut right[0]);
            let (dx, dy) = ((b.x - a.x) as f64, (b.y - a.y) as f64);
            let reach = ((a.radius + b.radius) << FIXED_BITS) as f64;
            let square = dx * dx + dy * dy;
            if square >= reach * reach || square < 1.0 {
                continue;
            }
            let dist = libm::sqrt(square);
            let (nx, ny) = (dx / dist, dy / dist);
            let mass = |r: i32| (r as f64) * (r as f64) * (r as f64);
            let (ia, ib) = (1.0 / mass(a.radius), 1.0 / mass(b.radius));
            // They are approaching: an exchange of momentum along the line of centres.
            let closing = (b.vx - a.vx) as f64 * nx + (b.vy - a.vy) as f64 * ny;
            if closing < 0.0 {
                let push = -2.0 * closing / (ia + ib);
                a.vx -= libm::round(push * ia * nx) as i32;
                a.vy -= libm::round(push * ia * ny) as i32;
                b.vx += libm::round(push * ib * nx) as i32;
                b.vy += libm::round(push * ib * ny) as i32;
            }
            // Separate them so they do not stick: the light one moves further.
            let overlap = reach - dist;
            let (sa, sb) = (overlap * ia / (ia + ib), overlap * ib / (ia + ib));
            a.x -= libm::round(sa * nx) as i32;
            a.y -= libm::round(sa * ny) as i32;
            b.x += libm::round(sb * nx) as i32;
            b.y += libm::round(sb * ny) as i32;
        }
    }
}

/// Everything needed for one screen.
struct Stage {
    /// The wallpaper with the clock: what is visible between and through the balls.
    backdrop: Canvas,
    /// The wallpaper without the clock.
    plain: Vec<u32>,
    orbs: Vec<Orb>,
    clock: Rect,
}

fn copy_rect(from: &[u32], to: &mut [u32], width: i32, height: i32, r: Rect) {
    let r = r.intersect(Rect::new(0, 0, width, height));
    for y in r.y..r.y + r.h {
        let at = (y * width + r.x) as usize;
        to[at..at + r.w as usize].copy_from_slice(&from[at..at + r.w as usize]);
    }
}

impl Stage {
    fn new(scene: &Canvas, wallpaper: &[u32], random: &mut u64) -> Self {
        let mut backdrop = scene.twin();
        let bounds = backdrop.bounds();
        backdrop.copy_from(wallpaper, bounds);
        backdrop.blend(bounds, 0x000000, DIM);
        let plain = backdrop.px.clone();
        let (w, h) = (scene.pw, scene.ph);
        let orbs = ORBS
            .iter()
            .map(|&(share, speed, tint)| {
                let radius = h * share / 1000;
                let pick = |limit: i32, random: &mut u64| radius + (next(random) % (limit - 2 * radius).max(1) as u64) as i32;
                let (x, y) = (pick(w, random), pick(h, random));
                let (hx, hy) = HEADINGS[(next(random) % HEADINGS.len() as u64) as usize];
                let pace = h * speed / 1000 * (1 << FIXED_BITS) / HZ as i32;
                Orb { radius, cells: lens(radius), tint, x: x << FIXED_BITS, y: y << FIXED_BITS, vx: pace * hx / 256, vy: pace * hy / 256, pace }
            })
            .collect();
        Self { backdrop, plain, orbs, clock: Rect::new(0, 0, 0, 0) }
    }

    /// Moves the clock to a new place. Returns the old and the new one, in screen pixels.
    fn move_clock(&mut self, time: &Time, random: &mut u64) -> [Rect; 2] {
        let (pw, ph) = (self.backdrop.pw, self.backdrop.ph);
        let old = self.clock;
        copy_rect(&self.plain, &mut self.backdrop.px, pw, ph, old);
        let b = self.backdrop.bounds();
        let (size, date_size) = (b.h as f32 / CLOCK_SHARE, b.h as f32 / DATE_SHARE);
        let clock = format!("{:02}:{:02}", time.hour, time.min);
        let date = format!("{:02}.{:02}.{}", time.day, time.month, time.year);
        let width = font::measure(FontId::SemiBold, size, &clock);
        let date_width = font::measure(FontId::Regular, date_size, &date);
        let height = (size * 1.45) as i32 + date_size as i32;
        let (room_x, room_y) = ((b.w - width).max(1) * 7 / 10, (b.h - height).max(1) * 7 / 10);
        let x = b.x + (b.w - width).max(0) * 15 / 100 + (next(random) % room_x as u64) as i32;
        let y = b.y + (b.h - height).max(0) * 15 / 100 + (next(random) % room_y as u64) as i32;
        self.backdrop.text(x, y, &clock, CLOCK_COLOR, FontId::SemiBold, size);
        self.backdrop.text(x + (width - date_width) / 2, y + (size * 1.3) as i32, &date, DATE_COLOR, FontId::Regular, date_size);
        self.clock = self.backdrop.device(Rect::new(x, y, width, height)).inset(-8);
        [old, self.clock]
    }

    fn draw_orb(&self, orb: &Orb, scene: &mut Canvas) {
        let (pw, ph) = (scene.pw, scene.ph);
        let r = orb.radius;
        let side = 2 * r + 1;
        let (left, top) = ((orb.x >> FIXED_BITS) - r, (orb.y >> FIXED_BITS) - r);
        let area = Rect::new(left, top, side, side).intersect(Rect::new(0, 0, pw, ph));
        for py in area.y..area.y + area.h {
            let cells = &orb.cells[((py - top) * side + area.x - left) as usize..][..area.w as usize];
            let row = &mut scene.px[(py * pw + area.x) as usize..][..area.w as usize];
            for (i, (cell, out)) in cells.iter().zip(row).enumerate() {
                if cell.cover == 0 {
                    continue;
                }
                let px = area.x + i as i32;
                let (sx, sy) = (reflect(px + cell.dx as i32, pw), reflect(py + cell.dy as i32, ph));
                let seen = self.backdrop.px[(sy * pw + sx) as usize];
                let glass = mix(orb.tint, mix(0x000000, seen, cell.dark as u32), TINT_ALPHA);
                let lit = mix(0xFFFFFF, glass, cell.light as u32) & 0xFF_FFFF;
                *out = mix(lit, *out, cell.cover as u32 + 1) | 0xFF00_0000;
            }
        }
    }

    /// A frame: the background on the old places, then the balls on the new ones. `damage` is in desktop units.
    fn frame(&mut self, scene: &mut Canvas, ticks: i32, moved: Option<[Rect; 2]>, damage: &mut Vec<Rect>) {
        let (pw, ph) = (scene.pw, scene.ph);
        let mut touched: Vec<Rect> = self.orbs.iter().map(Orb::rect).collect();
        touched.extend(moved.into_iter().flatten());
        for orb in &mut self.orbs {
            orb.advance(ticks, pw, ph);
        }
        collide(&mut self.orbs);
        // Pushing may have pushed a ball beyond the edge: bring it back.
        for orb in &mut self.orbs {
            orb.advance(0, pw, ph);
        }
        for &r in &touched {
            copy_rect(&self.backdrop.px, &mut scene.px, pw, ph, r);
        }
        touched.extend(self.orbs.iter().map(Orb::rect));
        for orb in &self.orbs {
            self.draw_orb(orb, scene);
        }
        let b = scene.bounds();
        let scale = scene.scale;
        let units = |v: i32| v * SCALE_100 / scale;
        damage.extend(touched.iter().filter(|r| !r.is_empty()).map(|r| Rect::new(b.x + units(r.x), b.y + units(r.y), units(r.w), units(r.h)).inset(-2)));
    }
}

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state >> 8
}

pub struct Saver {
    stages: Vec<Stage>,
    started: u64,
    frame_at: u64,
    shown: bool,
    minute: u8,
    random: u64,
}

impl Saver {
    pub fn new(scenes: &[Canvas], ticks: u64) -> Self {
        let mut random = arch::rdtsc() | 1;
        let images = wallpaper::desktop(theme::get().wallpaper);
        let stages = scenes.iter().zip(&images).map(|(scene, image)| Stage::new(scene, image, &mut random)).collect();
        Self { stages, started: ticks, frame_at: ticks, shown: false, minute: u8::MAX, random }
    }

    /// The time when input does not count yet has passed.
    pub fn settled(&self) -> bool {
        interrupts::ticks() >= self.started + GRACE_TICKS
    }

    /// Draws a frame if it is time; returns what to output.
    pub fn render(&mut self, scenes: &mut [Canvas], ctx: &Ctx) -> Vec<Rect> {
        if self.shown && ctx.ticks < self.frame_at + FRAME_TICKS {
            return Vec::new();
        }
        let ticks = (ctx.ticks - self.frame_at) as i32;
        self.frame_at = ctx.ticks;
        let new_minute = ctx.time.min != self.minute;
        self.minute = ctx.time.min;
        let mut damage = Vec::new();
        for (stage, scene) in self.stages.iter_mut().zip(scenes) {
            let moved = new_minute.then(|| stage.move_clock(&ctx.time, &mut self.random));
            if !self.shown {
                scene.px.copy_from_slice(&stage.backdrop.px);
            }
            stage.frame(scene, ticks, moved, &mut damage);
        }
        if !self.shown {
            damage = alloc::vec![crate::output::bounds()];
        }
        self.shown = true;
        damage
    }
}
