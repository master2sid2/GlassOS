//! Graphics: canvases (the screen scene and window surfaces) and output to the GOP framebuffer.
//!
//! Pixels are 0xAARRGGBB with premultiplied alpha. The screen scene is always
//! opaque; window surfaces have real alpha — the compositor uses it to blend
//! windows with the scene.
//!
//! The drawing primitives are integer-only: they were written when the kernel target had
//! no SSE and f32 was emulated in software. Anti-aliasing uses an integer
//! SDF (signed distance field) in 1/16 of a pixel.

pub mod simd;

use crate::font::{self, FontId};
use crate::smp::{self, SharedMut};
use crate::wm::Cursor;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use libm::roundf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }

    pub fn intersect(&self, o: Rect) -> Rect {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = (self.x + self.w).min(o.x + o.w);
        let y1 = (self.y + self.h).min(o.y + o.h);
        if x1 <= x0 || y1 <= y0 {
            // An empty intersection is empty in both dimensions, otherwise loops over
            // rows would walk "zero-width rows" with a wrong offset.
            return Rect::new(x0, y0, 0, 0);
        }
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    pub fn union(&self, o: Rect) -> Rect {
        if self.is_empty() {
            return o;
        }
        if o.is_empty() {
            return *self;
        }
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = (self.x + self.w).max(o.x + o.w);
        let y1 = (self.y + self.h).max(o.y + o.h);
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    pub fn inset(&self, d: i32) -> Rect {
        Rect::new(self.x + d, self.y + d, self.w - 2 * d, self.h - 2 * d)
    }

    pub fn offset(&self, dx: i32, dy: i32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }
}

/// A colour in 0x00RRGGBB format (alpha is given separately).
pub type Color = u32;

/// Multiplies all four channels by f / 256 (f in 0..=256).
#[inline]
fn scale(c: u32, f: u32) -> u32 {
    let rb = ((c & 0x00FF_00FF) * f >> 8) & 0x00FF_00FF;
    let ag = ((((c >> 8) & 0x00FF_00FF) * f >> 8) & 0x00FF_00FF) << 8;
    rb | ag
}

/// Opaque colour `src` with transparency alpha (0..=256) over `dst` (source-over).
#[inline]
pub fn mix(src: Color, dst: u32, alpha: u32) -> u32 {
    scale(src | 0xFF00_0000, alpha) + scale(dst, 256 - alpha)
}

/// A colour with alpha 0..=255 in premultiplied form.
pub fn premul(c: Color, a: u32) -> u32 {
    scale(c | 0xFF00_0000, a + (a >> 7))
}

/// Signed distance from the centre of pixel (x, y) to a rounded rectangle
/// in 1/16 of a pixel (< 0 — inside).
fn sdf_round(x: i32, y: i32, r: Rect, radius: i32) -> i32 {
    let (px, py) = (x * 16 + 8, y * 16 + 8);
    let (hw, hh) = (r.w * 8, r.h * 8);
    let qx = (px - (r.x * 16 + hw)).abs() - (hw - radius * 16);
    let qy = (py - (r.y * 16 + hh)).abs() - (hh - radius * 16);
    let outside = if qx > 0 && qy > 0 {
        ((qx as u64 * qx as u64 + qy as u64 * qy as u64).isqrt()) as i32
    } else {
        qx.max(qy)
    };
    outside - radius * 16
}

/// The fraction of a pixel covered by the shape: 0..=16.
fn coverage(d: i32) -> u32 {
    (8 - d).clamp(0, 16) as u32
}

/// The scale is stored in hundredths of a percent: in mirror mode the desktop must fit the screen exactly,
/// and 2560 to 1920 is 133.33%.
pub const SCALE_100: i32 = 10_000;
pub const PERCENT: i32 = SCALE_100 / 100;

/// Screen pixels for logical size `v` at scale `scale`.
fn points(v: i32, scale: i32) -> i32 {
    (v * scale).div_euclid(SCALE_100)
}

/// A canvas. Everyone who draws works in logical units, and the buffer stores screen pixels:
/// when scaled, shapes and text are computed for those pixels, so they stay sharp.
pub struct Canvas {
    /// Size in logical units.
    pub w: i32,
    pub h: i32,
    /// Buffer size in pixels.
    pub pw: i32,
    pub ph: i32,
    pub px: Vec<u32>,
    /// Scale in percent.
    pub scale: i32,
    /// The canvas's top-left corner on the desktop.
    pub(crate) x: i32,
    pub(crate) y: i32,
    /// Clip in pixels and in the units of whoever set it.
    clip: Rect,
    view: Rect,
    /// Nothing may be drawn beyond this rectangle, whatever clip is set
    /// (a partial frame of a program window).
    limit: Option<Rect>,
}

impl Canvas {
    pub fn new(w: i32, h: i32) -> Self {
        Self::scaled(w, h, SCALE_100)
    }

    pub fn scaled(w: i32, h: i32, scale: i32) -> Self {
        Self::place(Rect::new(0, 0, w, h), points(w, scale), points(h, scale), scale)
    }

    /// A canvas with a place on the desktop: `r` in logical units, `pw`×`ph` in screen pixels.
    pub fn place(r: Rect, pw: i32, ph: i32, scale: i32) -> Self {
        let clip = Rect::new(0, 0, pw, ph);
        Self { w: r.w, h: r.h, pw, ph, px: vec![0; (pw * ph) as usize], scale, x: r.x, y: r.y, clip, view: r, limit: None }
    }

    /// Where this canvas differs from `other` of the same size: a rectangle in
    /// logical units (with a one-pixel margin). None — different sizes, Some(empty) — equal.
    pub fn diff_bounds(&self, other: &Canvas) -> Option<Rect> {
        if (self.pw, self.ph, self.scale) != (other.pw, other.ph, other.scale) {
            return None;
        }
        let w = self.pw as usize;
        let rows = |y: usize| (&self.px[y * w..(y + 1) * w], &other.px[y * w..(y + 1) * w]);
        let differs = |y: usize| {
            let (a, b) = rows(y);
            a != b
        };
        let Some(top) = (0..self.ph as usize).find(|&y| differs(y)) else { return Some(Rect::new(0, 0, 0, 0)) };
        let bottom = (top..self.ph as usize).rev().find(|&y| differs(y)).unwrap_or(top);
        let (mut left, mut right) = (w, 0);
        for y in top..=bottom {
            let (a, b) = rows(y);
            if let Some(l) = (0..left.min(w)).find(|&x| a[x] != b[x]) {
                left = l;
            }
            if let Some(r) = (right..w).rev().find(|&x| a[x] != b[x]) {
                right = r;
            }
        }
        // Pixels → logical units, rounded outwards.
        let to = |v: usize| (v as i32 * SCALE_100) / self.scale;
        let (x0, y0) = (to(left).saturating_sub(1), to(top).saturating_sub(1));
        let (x1, y1) = (to(right + 1) + 2, to(bottom + 1) + 2);
        Some(Rect::new(x0, y0, (x1 - x0).min(self.w - x0), (y1 - y0).min(self.h - y0)))
    }

    /// An empty canvas with the same place and scale.
    pub fn twin(&self) -> Self {
        Self::place(self.bounds(), self.pw, self.ph, self.scale)
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(self.x, self.y, self.w, self.h)
    }

    fn all_points(&self) -> Rect {
        Rect::new(0, 0, self.pw, self.ph)
    }

    /// A rectangle in buffer pixels. Adjacent areas join without gaps.
    pub fn device(&self, r: Rect) -> Rect {
        let (x, y) = (points(r.x - self.x, self.scale), points(r.y - self.y, self.scale));
        Rect::new(x, y, points(r.x + r.w - self.x, self.scale) - x, points(r.y + r.h - self.y, self.scale) - y)
    }

    /// A length in pixels: thicknesses and radii do not vanish when scaled.
    pub fn size(&self, v: i32) -> i32 {
        let scaled = (v * self.scale + SCALE_100 / 2) / SCALE_100;
        if v > 0 { scaled.max(1) } else { scaled }
    }

    pub fn set_clip(&mut self, r: Rect) {
        let r = match self.limit {
            Some(l) => r.intersect(l),
            None => r,
        };
        self.view = r.intersect(self.bounds());
        self.clip = self.device(self.view).intersect(self.all_points());
    }

    pub fn clip(&self) -> Rect {
        self.view
    }

    pub fn reset_clip(&mut self) {
        self.set_clip(self.bounds());
    }

    /// Limit all drawing to rectangle `r` (None — remove the limit).
    pub fn set_limit(&mut self, r: Option<Rect>) {
        self.limit = r;
        self.reset_clip();
    }

    #[inline]
    fn blend_px(&mut self, x: i32, y: i32, c: Color, alpha: u32) {
        if alpha == 0 || !self.clip.contains(x, y) {
            return;
        }
        let p = &mut self.px[(y * self.pw + x) as usize];
        *p = if alpha >= 256 { c | 0xFF00_0000 } else { mix(c, *p, alpha) };
    }

    fn rows(&mut self, r: Rect, mut f: impl FnMut(&mut [u32], i32)) {
        let r = r.intersect(self.clip);
        for y in r.y..r.y + r.h {
            let row = (y * self.pw + r.x) as usize;
            f(&mut self.px[row..row + r.w as usize], y);
        }
    }

    /// A gear centred at (cx, cy): `teeth` teeth up to radius `radius`, rotated by
    /// `angle` radians, with a hole in the middle. Anti-aliased in screen pixels.
    pub fn gear(&mut self, cx: i32, cy: i32, radius: i32, teeth: u32, angle: f32, c: Color, alpha: u32) {
        let center = self.device(Rect::new(cx, cy, 0, 0));
        let zoom = self.scale as f32 / SCALE_100 as f32;
        let outer = radius as f32 * zoom;
        let (body, hole) = (outer * 0.72, outer * 0.32);
        let period = core::f32::consts::TAU / teeth.max(3) as f32;
        // Half the tooth width (in angle): narrower at the tip than at the base.
        let reach = outer as i32 + 2;
        let clamp = |v: f32| v.clamp(0.0, 1.0);
        for y in center.y - reach..=center.y + reach {
            for x in center.x - reach..=center.x + reach {
                let (dx, dy) = (x as f32 + 0.5 - center.x as f32, y as f32 + 0.5 - center.y as f32);
                let d = libm::sqrtf(dx * dx + dy * dy);
                let theta = libm::atan2f(dy, dx) - angle;
                let q = theta / period;
                let phase = q - libm::floorf(q);
                // How many pixels from the tooth edge (positive — inside): the width narrows towards the tip.
                let half = 0.27 - 0.07 * clamp((d - body) / (outer - body).max(1.0));
                let tooth = clamp((half - (phase - 0.5).abs()) * period * d + 0.5);
                let shape = clamp(body + 0.5 - d).max(tooth.min(clamp(outer + 0.5 - d)));
                let cover = shape.min(clamp(d - hole + 0.5));
                self.blend_px(x, y, c, (cover * alpha as f32) as u32);
            }
        }
    }

    /// An arc above point (cx, cy), ±45° from vertical. Anti-aliased in screen pixels,
    /// so it stays sharp at any scale.
    pub fn arc_up(&mut self, cx: i32, cy: i32, radius: i32, width: i32, c: Color, alpha: u32) {
        let center = self.device(Rect::new(cx, cy, 0, 0));
        let zoom = self.scale as f32 / SCALE_100 as f32;
        let (r, half) = (radius as f32 * zoom, width as f32 * zoom / 2.0);
        let reach = (r + half) as i32 + 2;
        for y in center.y - reach..center.y {
            for x in center.x - reach..=center.x + reach {
                let (dx, dy) = (x as f32 + 0.5 - center.x as f32, center.y as f32 - y as f32 - 0.5);
                let ring = half + 0.5 - (libm::sqrtf(dx * dx + dy * dy) - r).abs();
                let sector = (dy - dx.abs()) * core::f32::consts::FRAC_1_SQRT_2 + 0.5;
                let cover = ring.min(sector).clamp(0.0, 1.0);
                self.blend_px(x, y, c, (cover * alpha as f32) as u32);
            }
        }
    }

    /// Writes a pixel as is (together with alpha), without blending.
    pub fn set(&mut self, r: Rect, argb: u32) {
        let r = self.device(r);
        self.rows(r, |span, _| span.fill(argb));
    }

    pub fn fill(&mut self, r: Rect, c: Color) {
        self.set(r, c | 0xFF00_0000);
    }

    /// A translucent fill; alpha 0..=256.
    pub fn blend(&mut self, r: Rect, c: Color, alpha: u32) {
        let r = self.device(r);
        self.rows(r, |span, _| span.iter_mut().for_each(|p| *p = mix(c, *p, alpha)));
    }

    /// A vertical gradient with alpha 0..=255; pixels are replaced.
    pub fn set_gradient(&mut self, r: Rect, top: Color, bottom: Color, alpha: u32) {
        let r = self.device(r);
        let h = r.h.max(1) as u32;
        let (ry, rh) = (r.y, r.h);
        self.rows(r, |span, y| {
            let t = ((y - ry).clamp(0, rh) as u32 * 256) / h;
            span.fill(premul(scale(top, 256 - t) + scale(bottom, t), alpha));
        });
    }

    /// A rounded rectangle with anti-aliased corners; alpha 0..=256.
    pub fn fill_round(&mut self, r: Rect, radius: i32, c: Color, alpha: u32) {
        let (r, radius) = (self.device(r), self.size(radius));
        let radius = radius.min(r.w / 2).min(r.h / 2).max(0);
        let area = r.intersect(self.clip);
        for y in area.y..area.y + area.h {
            let in_corner_rows = y < r.y + radius || y >= r.y + r.h - radius;
            // Anti-aliasing is only needed in the corners; the middle of a row is filled entirely.
            let (mid0, mid1) = if in_corner_rows { (r.x + radius, r.x + r.w - radius) } else { (r.x, r.x + r.w) };
            let (mid0, mid1) = (mid0.max(area.x), mid1.min(area.x + area.w));
            if mid1 > mid0 {
                let row = (y * self.pw) as usize;
                let span = &mut self.px[row + mid0 as usize..row + mid1 as usize];
                if alpha >= 256 {
                    span.fill(c | 0xFF00_0000);
                } else {
                    span.iter_mut().for_each(|p| *p = mix(c, *p, alpha));
                }
            }
            if !in_corner_rows {
                continue;
            }
            for x in (area.x..mid0).chain(mid1.max(area.x)..area.x + area.w) {
                let cov = coverage(sdf_round(x, y, r, radius));
                self.blend_px(x, y, c, alpha * cov / 16);
            }
        }
    }

    /// Calls `f(x, y)` for the rectangle's pixels no farther than `band`
    /// from its edge (skips the middle of rows).
    fn for_edge_band(&mut self, r: Rect, band: i32, mut f: impl FnMut(&mut Self, i32, i32)) {
        let area = r.intersect(self.clip);
        for y in area.y..area.y + area.h {
            let skip_middle = y >= r.y + band && y < r.y + r.h - band;
            let mut x = area.x;
            while x < area.x + area.w {
                if skip_middle && x == (r.x + band).max(area.x) && x < r.x + r.w - band {
                    x = r.x + r.w - band;
                    continue;
                }
                f(self, x, y);
                x += 1;
            }
        }
    }

    /// Trims corners to the rounding: multiplies pixels by the shape's coverage.
    pub fn round_mask(&mut self, r: Rect, radius: i32) {
        let (r, radius) = (self.device(r), self.size(radius));
        self.for_edge_band(r, radius, |c, x, y| {
            let cov = coverage(sdf_round(x, y, r, radius));
            if cov < 16 {
                let p = &mut c.px[(y * c.pw + x) as usize];
                *p = scale(*p, cov * 16);
            }
        });
    }

    /// Rounds a rectangle's corners by painting them outside with background colour `bg`.
    pub fn round_mask_on(&mut self, r: Rect, radius: i32, bg: Color) {
        let (r, radius) = (self.device(r), self.size(radius));
        self.for_edge_band(r, radius, |c, x, y| {
            let cov = coverage(sdf_round(x, y, r, radius));
            if cov < 16 {
                c.blend_px(x, y, bg, (16 - cov) * 16);
            }
        });
    }

    /// A rounded rectangle outline one logical unit thick.
    pub fn stroke_round(&mut self, r: Rect, radius: i32, c: Color, alpha: u32) {
        let (r, radius, width) = (self.device(r), self.size(radius), self.size(1));
        self.for_edge_band(r, radius.max(width), |cv, x, y| {
            let d = sdf_round(x, y, r, radius);
            let ring = coverage(d) - coverage(d + 16 * width);
            cv.blend_px(x, y, c, alpha * ring / 16);
        });
    }

    /// A soft shadow around a shape shifted down by `dy` (no caching — for
    /// the dock and menus). Pixels under the shape itself are left alone.
    pub fn shadow(&mut self, r: Rect, radius: i32, spread: i32, dy: i32, alpha: u32) {
        let d = self.device(r);
        let mask = ShadowMask::new(d.w, d.h, self.size(radius), self.size(spread), self.size(dy), alpha);
        self.shadow_at(&mask, d.x, d.y);
    }

    /// A ready shadow of a shape with its corner at (x, y): the mask is computed in this canvas's pixels.
    pub fn apply_shadow(&mut self, m: &ShadowMask, x: i32, y: i32) {
        let at = self.device(Rect::new(x, y, 0, 0));
        self.shadow_at(m, at.x, at.y);
    }

    fn shadow_at(&mut self, m: &ShadowMask, x: i32, y: i32) {
        let (ox, oy) = (x + m.dx, y + m.dy);
        let area = Rect::new(ox, oy, m.w, m.h).intersect(self.clip);
        self.par_rows(area, &|row, py| {
            let mrow = ((py - oy) * m.w + (area.x - ox)) as usize;
            simd::shade_row(row, &m.alpha[mrow..mrow + area.w as usize]);
        });
    }

    /// Copies the pixels of `r` from a buffer of the same size as the canvas (wallpaper, backing).
    pub fn copy_from(&mut self, src: &[u32], r: Rect) {
        let w = self.pw;
        let area = self.device(r).intersect(self.all_points());
        self.par_rows(area, &|row, y| {
            let at = (y * w + area.x) as usize;
            row.copy_from_slice(&src[at..at + row.len()]);
        });
    }

    /// Calls `f(row, y)` for each row of `area` (inside the canvas) — in bands on all cores.
    fn par_rows(&mut self, area: Rect, f: &(dyn Fn(&mut [u32], i32) + Sync)) {
        if area.is_empty() {
            return;
        }
        let (w, len) = (self.pw, self.px.len());
        let px = SharedMut(self.px.as_mut_ptr());
        let rows = bands(area.h as usize);
        spread(area.w as usize * area.h as usize, rows.len(), &|i| {
            let all = unsafe { core::slice::from_raw_parts_mut(px.get(), len) };
            for y in rows[i].clone().map(|r| area.y + r as i32) {
                let at = (y * w + area.x) as usize;
                f(&mut all[at..at + area.w as usize], y);
            }
        });
    }

    /// Blur (box blur twice along each axis ≈ Gaussian) — "frosted glass".
    /// Reads pixels within 2*radius around `r`, writes only inside `r ∩ clip`.
    pub fn blur(&mut self, r: Rect, radius: i32) {
        self.blur_impl(None, r, radius);
    }

    /// Blur under a rounded panel: beyond the rounded corners the background stays as it was,
    /// otherwise sharp blurred corners show through the translucent fill.
    pub fn blur_round(&mut self, r: Rect, radius: i32, corner: i32) {
        let (d, k) = (self.device(r), self.size(corner));
        let spots = [(d.x, d.y), (d.x + d.w - k, d.y), (d.x, d.y + d.h - k), (d.x + d.w - k, d.y + d.h - k)];
        let saved: Vec<(Rect, Vec<u32>)> = spots
            .iter()
            .map(|&(x, y)| {
                let q = Rect::new(x, y, k, k).intersect(self.clip);
                let mut px = Vec::with_capacity((q.w.max(0) * q.h.max(0)) as usize);
                for yy in q.y..q.y + q.h {
                    let row = (yy * self.pw + q.x) as usize;
                    px.extend_from_slice(&self.px[row..row + q.w as usize]);
                }
                (q, px)
            })
            .collect();
        self.blur(r, radius);
        for (q, px) in saved {
            for yy in q.y..q.y + q.h {
                for xx in q.x..q.x + q.w {
                    let cov = coverage(sdf_round(xx, yy, d, k));
                    if cov < 16 {
                        let old = px[((yy - q.y) * q.w + (xx - q.x)) as usize];
                        let p = &mut self.px[(yy * self.pw + xx) as usize];
                        *p = if cov == 0 { old } else { scale(*p, cov * 16) + scale(old, 256 - cov * 16) };
                    }
                }
            }
        }
    }

    /// Blurs `src` (a canvas of the same size) and puts the result into `r` of this canvas.
    /// The source is valid everywhere, so the area need not be extended by the radius.
    pub fn blur_from(&mut self, src: &Canvas, r: Rect, radius: i32) {
        self.blur_impl(Some(src), r, radius);
    }

    fn blur_impl(&mut self, from: Option<&Canvas>, r: Rect, radius: i32) {
        let (r, radius) = (self.device(r).intersect(self.clip), self.size(radius));
        if r.is_empty() {
            return;
        }
        if self.blur_accel(from, r, radius) {
            return;
        }
        let src = r.inset(-2 * radius).intersect(self.all_points());
        let (w, h) = (src.w as usize, src.h as usize);
        let mut buf = Vec::with_capacity(w * h);
        let px = from.map_or(&self.px, |c| &c.px);
        for y in src.y..src.y + src.h {
            let row = (y * self.pw + src.x) as usize;
            buf.extend_from_slice(&px[row..row + w]);
        }
        blur_buffer(&mut buf, w, h, radius as usize);
        for y in r.y..r.y + r.h {
            let s = (y - src.y) as usize * w + (r.x - src.x) as usize;
            let d = (y * self.pw + r.x) as usize;
            self.px[d..d + r.w as usize].copy_from_slice(&buf[s..s + r.w as usize]);
        }
    }

    /// Window composition on the GPU: `area` is in this canvas's pixels, `at` is where the corner of `src` goes.
    fn over_accel(&mut self, src: &Canvas, area: Rect, at: Rect) -> bool {
        let Some(a) = crate::accel::get() else { return false };
        if area.is_empty() || area.x < 0 || area.y < 0 {
            return false;
        }
        let image = |c: &Canvas| {
            let addr = a.import(c.px.as_ptr() as usize, c.px.len() * 4)?;
            Some(crate::accel::Image { addr, w: c.pw as u32, h: c.ph as u32, pitch: c.pw as u32 * 4 })
        };
        let (Some(dst), Some(img)) = (image(self), image(src)) else { return false };
        let from = ((area.x - at.x) as u32, (area.y - at.y) as u32, area.w as u32, area.h as u32);
        a.over(dst, (area.x as u32, area.y as u32), img, from)
    }

    /// Blur on the GPU (if there is one and it succeeded). `r` is in device pixels.
    fn blur_accel(&mut self, from: Option<&Canvas>, r: Rect, radius: i32) -> bool {
        let Some(a) = crate::accel::get() else { return false };
        if r.x < 0 || r.y < 0 || radius <= 0 {
            return false;
        }
        let image = |c: &Canvas| {
            let addr = a.import(c.px.as_ptr() as usize, c.px.len() * 4)?;
            Some(crate::accel::Image { addr, w: c.pw as u32, h: c.ph as u32, pitch: c.pw as u32 * 4 })
        };
        let Some(dst) = image(self) else { return false };
        let src = match from {
            Some(c) if (c.pw, c.ph) == (self.pw, self.ph) => match image(c) {
                Some(i) => i,
                None => return false,
            },
            Some(_) => return false,
            None => dst,
        };
        a.blur(dst, src, (r.x as u32, r.y as u32, r.w as u32, r.h as u32), radius as u32)
    }

    /// A copy of a rectangle's pixels (it must lie inside the canvas).
    pub fn read(&self, r: Rect) -> Vec<u32> {
        let r = self.device(r);
        let mut out = Vec::with_capacity((r.w * r.h) as usize);
        for y in r.y..r.y + r.h {
            let row = (y * self.pw + r.x) as usize;
            out.extend_from_slice(&self.px[row..row + r.w as usize]);
        }
        out
    }

    /// Writes pixels previously taken with `read(r)`, within the clip.
    pub fn write(&mut self, r: Rect, src: &[u32]) {
        let r = self.device(r);
        let area = r.intersect(self.clip);
        for y in area.y..area.y + area.h {
            let s = ((y - r.y) * r.w + (area.x - r.x)) as usize;
            let d = (y * self.pw + area.x) as usize;
            self.px[d..d + area.w as usize].copy_from_slice(&src[s..s + area.w as usize]);
        }
    }

    /// Updates piece `part` of this canvas in the cache `pixels` of rectangle `whole`.
    pub fn refresh(&self, pixels: &mut [u32], whole: Rect, part: Rect) {
        let (whole, part) = (self.device(whole), self.device(part));
        for y in part.y..part.y + part.h {
            let src = (y * self.pw + part.x) as usize;
            let dst = ((y - whole.y) * whole.w + (part.x - whole.x)) as usize;
            pixels[dst..dst + part.w as usize].copy_from_slice(&self.px[src..src + part.w as usize]);
        }
    }

    /// Blends canvas `src` at point (x, y), fitting its pixels to ours.
    fn resample(&mut self, src: &Canvas, x: i32, y: i32, mix_pixel: fn(u32, u32) -> u32) {
        const FRACTION: i32 = 8;
        const ONE: i32 = 1 << FRACTION;
        let at = self.device(Rect::new(x, y, src.w, src.h));
        let area = at.intersect(self.clip);
        self.par_rows(area, &|row, py| {
            let sy = (py - at.y) * ONE / at.h * src.ph + ((py - at.y) * ONE % at.h) * src.ph / at.h;
            let (y0, fy) = ((sy >> FRACTION).min(src.ph - 1), (sy & (ONE - 1)) as u32);
            let y1 = (y0 + 1).min(src.ph - 1);
            for (i, d) in row.iter_mut().enumerate() {
                let dx = area.x + i as i32 - at.x;
                let sx = dx * ONE / at.w * src.pw + (dx * ONE % at.w) * src.pw / at.w;
                let (x0, fx) = ((sx >> FRACTION).min(src.pw - 1), (sx & (ONE - 1)) as u32);
                let x1 = (x0 + 1).min(src.pw - 1);
                let at_px = |px: i32, py: i32| src.px[(py * src.pw + px) as usize];
                let top = lerp(at_px(x0, y0), at_px(x1, y0), fx);
                let bottom = lerp(at_px(x0, y1), at_px(x1, y1), fx);
                *d = mix_pixel(lerp(top, bottom, fy), *d);
            }
        });
    }

    /// Puts canvas `src` at point (x, y). A different scale is adapted to this canvas.
    fn lay(&mut self, src: &Canvas, x: i32, y: i32, mix_pixel: fn(u32, u32) -> u32) {
        if src.scale != self.scale {
            self.resample(src, x, y, mix_pixel);
            return;
        }
        let at = self.device(Rect::new(x, y, 0, 0));
        // Not given to the GPU: this is sometimes a plain replace (`replace`), and the GPU path only does
        // blending; the CPU copies no slower (it is the same memory).
        let area = Rect::new(at.x, at.y, src.pw, src.ph).intersect(self.clip);
        self.par_rows(area, &|row, py| {
            let srow = ((py - at.y) * src.pw + (area.x - at.x)) as usize;
            for (d, &s) in row.iter_mut().zip(&src.px[srow..srow + area.w as usize]) {
                *d = mix_pixel(s, *d);
            }
        });
    }

    /// Composites a window surface (premultiplied alpha) at point (x, y).
    pub fn composite(&mut self, src: &Canvas, x: i32, y: i32) {
        if src.scale != self.scale {
            self.resample(src, x, y, over);
            return;
        }
        let at = self.device(Rect::new(x, y, 0, 0));
        let area = Rect::new(at.x, at.y, src.pw, src.ph).intersect(self.clip);
        // A large piece goes to the GPU (the CPU places a small one faster than it would wait).
        if area.w * area.h >= ACCEL_MIN_PIXELS && self.over_accel(src, area, at) {
            return;
        }
        self.par_rows(area, &|row, py| {
            let srow = ((py - at.y) * src.pw + (area.x - at.x)) as usize;
            simd::over_row(row, &src.px[srow..srow + area.w as usize]);
        });
    }

    /// Replaces pixels with canvas `src` without blending.
    pub fn replace(&mut self, src: &Canvas, x: i32, y: i32) {
        self.lay(src, x, y, |s, _| s);
    }

    /// Draws a line of text; (x, y) is the line's top-left corner. Returns the width.
    pub fn text(&mut self, x: i32, y: i32, s: &str, c: Color, font: FontId, size: f32) -> i32 {
        let start = self.device(Rect::new(x, y, 0, 0));
        let size = size * self.scale as f32 / SCALE_100 as f32;
        let baseline = roundf(start.y as f32 + font::ascent(font, size)) as i32;
        let mut pen = start.x as f32;
        let cell = if s.bytes().any(|b| b.is_ascii_digit()) { font::digit_cell(font, size) } else { 0.0 };
        for ch in s.chars() {
            pen += font::with_glyph(font, size, ch, |g| {
                // A digit is centred in its cell.
                let (shift, step) = if ch.is_ascii_digit() { ((cell - g.advance) / 2.0, cell) } else { (0.0, g.advance) };
                let gx = roundf(pen + shift) as i32 + g.xmin;
                let gy = baseline - g.ymin - g.h as i32;
                for (row, line) in g.bitmap.chunks_exact(g.w.max(1)).enumerate() {
                    for (col, &cov) in line.iter().enumerate() {
                        self.blend_px(gx + col as i32, gy + row as i32, c, cov as u32 + (cov as u32 >> 7));
                    }
                }
                step
            });
        }
        (roundf(pen) as i32 - start.x) * SCALE_100 / self.scale
    }

    /// Copies the opaque image `src` (width `w`) to position (x, y).
    pub fn blit(&mut self, x: i32, y: i32, w: i32, src: &[u32]) {
        let h = src.len() as i32 / w;
        self.blit_into(Rect::new(x, y, w, h), w, src);
    }

    /// Stretches the opaque image `src` (width `w`) over rectangle `r`.
    /// If `r` has as many pixels as the image — a one-to-one copy.
    pub fn blit_into(&mut self, r: Rect, w: i32, src: &[u32]) {
        if w <= 0 || src.len() < w as usize {
            return;
        }
        let h = src.len() as i32 / w;
        let at = self.device(r);
        if at.w <= 0 || at.h <= 0 {
            return;
        }
        let r = at.intersect(self.clip);
        for row in r.y..r.y + r.h {
            let line = (row - at.y) * h / at.h * w;
            let d = (row * self.pw + r.x) as usize;
            for (i, dst) in self.px[d..d + r.w as usize].iter_mut().enumerate() {
                *dst = src[(line + (r.x + i as i32 - at.x) * w / at.w) as usize] | 0xFF00_0000;
            }
        }
    }
}

/// A mix of two pixels over all channels: `t` out of 256 is the share of the second.
fn lerp(a: u32, b: u32, t: u32) -> u32 {
    let inverse = 256 - t;
    let low = ((a & 0x00FF_00FF) * inverse + (b & 0x00FF_00FF) * t) >> 8 & 0x00FF_00FF;
    let high = ((a >> 8 & 0x00FF_00FF) * inverse + (b >> 8 & 0x00FF_00FF) * t) & 0xFF00_FF00;
    low | high
}

/// Box blur twice over the rows and columns of a `w`×`h` buffer (≈ Gaussian blur).
fn blur_buffer(buf: &mut Vec<u32>, w: usize, h: usize, rad: usize) {
    for _ in 0..2 {
        // Rows and columns are independent — hand them out in bands to all cores.
        let shared = SharedMut(buf.as_mut_ptr());
        let rows = bands(h);
        spread(w * h, rows.len(), &|i| {
            let all = unsafe { core::slice::from_raw_parts_mut(shared.get(), w * h) };
            let mut tmp = vec![0u32; w];
            for y in rows[i].clone() {
                box_pass(all, y * w, 1, w, rad, &mut tmp);
            }
        });
        let src = buf.clone();
        let cols = bands(w);
        spread(w * h, cols.len(), &|i| {
            let all = unsafe { core::slice::from_raw_parts_mut(shared.get(), w * h) };
            box_pass_vertical(all, &src, w, h, rad, cols[i].clone());
        });
    }
}

/// Checks and benchmarks the SSE versions against the simple ones (once at startup, to the log).
pub fn bench() {
    // After boot: while the network and Bluetooth come up, the benchmark lies.
    crate::task::sleep_until(crate::interrupts::ticks() + 20 * crate::interrupts::HZ);
    let (w, h) = (640usize, 480usize);
    let mut seed = 0x1234_5678u32;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    // Premultiplied pixels with different alpha (and whole transparent/opaque pieces).
    let src: Vec<u32> = (0..w * h)
        .map(|i| {
            let a = match (i / 64) % 4 { 0 => 255, 1 => 0, _ => next() & 0xFF };
            scale(next() | 0xFF00_0000, a + (a >> 7)) & 0x00FF_FFFF | a << 24
        })
        .collect();
    let dst: Vec<u32> = (0..w * h).map(|_| next() | 0xFF00_0000).collect();
    // Best of three: the thread may be preempted, and the cycles count that time too.
    let time = |f: &mut dyn FnMut()| {
        (0..3)
            .map(|_| {
                let t = crate::arch::rdtsc();
                f();
                crate::arch::rdtsc() - t
            })
            .min()
            .unwrap_or(0)
    };
    let (mut a, mut b) = (dst.clone(), dst.clone());
    let plain = time(&mut || a.iter_mut().zip(&src).for_each(|(d, &s)| *d = over(s, *d)));
    let fast = time(&mut || simd::over_row(&mut b, &src));
    let ok_over = a == b;
    let mask: Vec<u8> = (0..w * h).map(|_| next() as u8).collect();
    let (mut a2, mut b2) = (dst.clone(), dst.clone());
    let plain_sh = time(&mut || a2.iter_mut().zip(&mask).for_each(|(p, &m)| if m != 0 { *p = scale(*p, 256 - m as u32) }));
    let fast_sh = time(&mut || simd::shade_row(&mut b2, &mask));
    let ok_shade = a2 == b2;
    crate::kprintln!(
        "gfx: SSE check {}: overlay {} → {} Kcycles, shadow {} → {} (640×480)",
        if ok_over && ok_shade { "passed" } else { "FAILED" },
        plain / 1000,
        fast / 1000,
        plain_sh / 1000,
        fast_sh / 1000
    );
}

/// A pixel with premultiplied alpha over another one.
#[inline]
fn over(s: u32, d: u32) -> u32 {
    match s >> 24 {
        255 => s,
        0 => d,
        a => s + scale(d, 256 - a - (a >> 7)),
    }
}

/// One box blur pass along a line of `len` pixels with step `stride`.
/// Channels R and B are summed together — in the 16-bit halves of one u32.
fn box_pass(buf: &mut [u32], start: usize, stride: usize, len: usize, r: usize, tmp: &mut [u32]) {
    for (i, t) in tmp[..len].iter_mut().enumerate() {
        *t = buf[start + i * stride];
    }
    let last = len - 1;
    let inv = 65536 / (2 * r as u32 + 1);
    let (mut rb, mut g) = (0u32, 0u32);
    let add = |p: u32, rb: &mut u32, g: &mut u32| {
        *rb += p & 0x00FF_00FF;
        *g += (p >> 8) & 0xFF;
    };
    for j in 0..=2 * r {
        add(tmp[j.saturating_sub(r).min(last)], &mut rb, &mut g);
    }
    let mut out = start;
    for i in 0..len {
        buf[out] = 0xFF00_0000 | ((rb >> 16) * inv >> 16) << 16 | (g * inv >> 16) << 8 | ((rb & 0xFFFF) * inv >> 16);
        out += stride;
        let (old, new) = (tmp[i.saturating_sub(r)], tmp[(i + r + 1).min(last)]);
        rb = rb + (new & 0x00FF_00FF) - (old & 0x00FF_00FF);
        g = g + ((new >> 8) & 0xFF) - ((old >> 8) & 0xFF);
    }
}

/// The same pass as `box_pass`, but vertical for all columns at once: the window
/// sums are kept per column and we walk row by row — memory is read sequentially rather than
/// jumping across rows (several times faster on large areas).
/// Columns `cols` are taken from `src` (a copy before the pass), the result goes to `buf`.
fn box_pass_vertical(buf: &mut [u32], src: &[u32], w: usize, h: usize, r: usize, cols: core::ops::Range<usize>) {
    let (x0, n) = (cols.start, cols.len());
    let row = |y: usize| &src[y * w + x0..y * w + x0 + n];
    let last = h - 1;
    let inv = 65536 / (2 * r as u32 + 1);
    let (mut rb, mut g) = (vec![0u32; n], vec![0u32; n]);
    for j in 0..=2 * r {
        for (x, &p) in row(j.saturating_sub(r).min(last)).iter().enumerate() {
            rb[x] += p & 0x00FF_00FF;
            g[x] += (p >> 8) & 0xFF;
        }
    }
    for i in 0..h {
        let out = &mut buf[i * w + x0..i * w + x0 + n];
        for x in 0..n {
            out[x] = 0xFF00_0000 | ((rb[x] >> 16) * inv >> 16) << 16 | (g[x] * inv >> 16) << 8 | ((rb[x] & 0xFFFF) * inv >> 16);
        }
        let (old, new) = (row(i.saturating_sub(r)), row((i + r + 1).min(last)));
        for x in 0..n {
            rb[x] = rb[x] + (new[x] & 0x00FF_00FF) - (old[x] & 0x00FF_00FF);
            g[x] = g[x] + ((new[x] >> 8) & 0xFF) - ((old[x] >> 8) & 0xFF);
        }
    }
}

/// Splits `0..n` into bands for the cores: no smaller than 32, a couple of bands per core.
fn bands(n: usize) -> Vec<core::ops::Range<usize>> {
    let parts = (smp::cpu_count() * 2).min(n.div_ceil(32)).max(1);
    let step = n.div_ceil(parts);
    (0..parts).map(|i| i * step..((i + 1) * step).min(n)).filter(|r| !r.is_empty()).collect()
}

/// Below this (in pixels) the work runs on one core: a blinking cursor or a clock second
/// is not worth waking all the others from deep sleep.
const SMALL_JOB: usize = 100_000;

/// Hands `f(0)`, …, `f(n - 1)` out to the cores if there is enough work for all.
fn spread(pixels: usize, n: usize, f: &(dyn Fn(usize) + Sync)) {
    if pixels < SMALL_JOB {
        (0..n).for_each(f);
    } else {
        smp::parallel(n, f);
    }
}

/// A precomputed shadow mask (values are the opacity of black, 0..=255).
pub struct ShadowMask {
    /// Offset of the mask relative to the shape's top-left corner.
    pub dx: i32,
    pub dy: i32,
    pub w: i32,
    pub h: i32,
    alpha: Vec<u8>,
}

impl ShadowMask {
    pub fn bytes(&self) -> usize {
        self.alpha.len()
    }

    pub fn new(w: i32, h: i32, radius: i32, spread: i32, offset_y: i32, alpha: u32) -> Self {
        let shape = Rect::new(0, 0, w, h);
        let cast = shape.offset(0, offset_y);
        let outer = cast.inset(-spread);
        let s16 = spread * 16;
        let mut mask = vec![0u8; (outer.w * outer.h) as usize];
        for y in outer.y..outer.y + outer.h {
            for x in outer.x..outer.x + outer.w {
                // No shadow is needed under the shape itself (especially under translucent windows).
                let covered = coverage(sdf_round(x, y, shape, radius));
                if covered == 16 {
                    continue;
                }
                let d = sdf_round(x, y, cast, radius).max(0);
                if d < s16 {
                    let t = (s16 - d) as u32;
                    let a = alpha * t / s16 as u32 * t / s16 as u32 * (16 - covered) / 16;
                    mask[((y - outer.y) * outer.w + (x - outer.x)) as usize] = a.min(255) as u8;
                }
            }
        }
        Self { dx: outer.x, dy: outer.y, w: outer.w, h: outer.h, alpha: mask }
    }
}

/// The linear framebuffer UEFI GOP gave us.
#[derive(Clone, Copy)]
pub struct Framebuffer {
    pub base: *mut u32,
    pub w: i32,
    pub h: i32,
    pub stride: i32,
    /// true — BGRx format (matches our 0xAARRGGBB in little-endian), false — RGBx.
    pub bgr: bool,
}

// The frame does not belong to a thread: it is device memory.
unsafe impl Send for Framebuffer {}

impl Framebuffer {
    fn convert(&self, c: Color) -> u32 {
        if self.bgr { c } else { (c & 0xFF) << 16 | (c & 0xFF00) | (c >> 16) & 0xFF }
    }

    /// Writes ready pixels to the screen, in rows of `r.w`.
    pub fn write(&self, r: Rect, px: &[u32]) {
        for (y, row) in px.chunks_exact(r.w as usize).enumerate() {
            let d = unsafe { self.base.add(((r.y + y as i32) * self.stride + r.x) as usize) };
            for (i, &p) in row.iter().enumerate() {
                unsafe { d.add(i).write_volatile(self.convert(p)) };
            }
        }
    }

    /// Puts a piece of frame `src` on the screen: `r` is in canvas units.
    pub fn present(&self, src: &Canvas, r: Rect) {
        let r = src.device(r).intersect(Rect::new(0, 0, self.w.min(src.pw), self.h.min(src.ph)));
        for y in r.y..r.y + r.h {
            let s = &src.px[(y * src.pw + r.x) as usize..][..r.w as usize];
            let d = unsafe { self.base.add((y * self.stride + r.x) as usize) };
            if self.bgr {
                unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), d, s.len()) };
            } else {
                for (i, &p) in s.iter().enumerate() {
                    unsafe { d.add(i).write_volatile(self.convert(p)) };
                }
            }
        }
    }

    pub fn put(&self, x: i32, y: i32, c: Color) {
        if x >= 0 && y >= 0 && x < self.w && y < self.h {
            unsafe { self.base.add((y * self.stride + x) as usize).write_volatile(self.convert(c)) };
        }
    }
}

/// From which piece size (in pixels) composition is handed to the GPU.
const ACCEL_MIN_PIXELS: i32 = 256 * 256;

/// The framebuffer for the panic screen (filled once at startup).
static CRASH_FB: AtomicU64 = AtomicU64::new(0);
static CRASH_GEOMETRY: AtomicU64 = AtomicU64::new(0);

impl Framebuffer {
    /// Remembers the framebuffer so a panic can draw without locks or the heap.
    pub fn register_for_crash(&self) {
        CRASH_FB.store(self.base as u64, Ordering::Relaxed);
        let g = (self.w as u64) | (self.h as u64) << 16 | (self.stride as u64) << 32 | (self.bgr as u64) << 63;
        CRASH_GEOMETRY.store(g, Ordering::Relaxed);
    }
}

/// The "kernel panic" screen: dims the screen and shows the reason.
pub fn crash_screen(title: &str, detail: &str) {
    let base = CRASH_FB.load(Ordering::Relaxed) as *mut u32;
    if base.is_null() {
        return;
    }
    let g = CRASH_GEOMETRY.load(Ordering::Relaxed);
    let fb = Framebuffer {
        base,
        w: (g & 0xFFFF) as i32,
        h: ((g >> 16) & 0xFFFF) as i32,
        stride: ((g >> 32) & 0x7FFF_FFFF) as i32,
        bgr: g >> 63 != 0,
    };
    for y in 0..fb.h {
        for x in 0..fb.w {
            let p = unsafe { fb.base.add((y * fb.stride + x) as usize) };
            unsafe { p.write_volatile(scale(p.read_volatile(), 70)) };
        }
    }
    let (pw, ph) = (640, 170);
    let (px, py) = ((fb.w - pw) / 2, (fb.h - ph) / 2);
    for y in py..py + ph {
        for x in px..px + pw {
            fb.put(x, y, if y < py + 6 { 0xE5484D } else { 0x1A1D2B });
        }
    }
    let lines = [
        ("Kernel panic", 0xFFFFFF, 24.0),
        (title, 0xF87171, 16.0),
        (detail, 0xC3C8D8, 14.0),
        ("The system has been halted. Details are in the serial log.", 0x8A90A8, 13.0),
    ];
    let mut y = py + 24;
    for (text, color, size) in lines {
        crash_text(&fb, px + 28, y, text, color, size);
        y += size as i32 + 16;
    }
}

fn crash_text(fb: &Framebuffer, x: i32, y: i32, s: &str, color: Color, size: f32) {
    let font = if size > 20.0 { FontId::SemiBold } else { FontId::Regular };
    let baseline = y + size as i32;
    let mut pen = x;
    for ch in s.chars() {
        let advance = font::try_with_glyph(font, size, ch, |g| {
            let gy = baseline - g.ymin - g.h as i32;
            for (row, line) in g.bitmap.chunks_exact(g.w.max(1)).enumerate() {
                for (col, &cov) in line.iter().enumerate() {
                    if cov > 100 {
                        fb.put(pen + g.xmin + col as i32, gy + row as i32, color);
                    }
                }
            }
            roundf(g.advance) as i32
        });
        match advance {
            Some(a) => pen += a,
            None => return, // the fonts are held by the crashed code — do without text
        }
    }
}

const ARROW: &[&str] = &[
    "X",
    "XX",
    "X.X",
    "X..X",
    "X...X",
    "X....X",
    "X.....X",
    "X......X",
    "X.......X",
    "X........X",
    "X.....XXXXX",
    "X..X..X",
    "X.X X..X",
    "XX  X..X",
    "X    X..X",
    "     X..X",
    "      XX",
];

const RESIZE_H: &[&str] = &[
    "   X       X   ",
    "  XX       XX  ",
    " X.XXXXXXXXX.X ",
    "X.............X",
    " X.XXXXXXXXX.X ",
    "  XX       XX  ",
    "   X       X   ",
];

const RESIZE_NWSE: &[&str] = &[
    "XXXXXX       ",
    "X....X       ",
    "X...X        ",
    "X..X.X       ",
    "X.X X.X      ",
    "XX   X.X     ",
    "      X.X    ",
    "       X.X   XX",
    "        X.X X.X",
    "         X.X..X",
    "          X...X",
    "         X....X",
    "         XXXXXX",
];

/// A cursor pixel: Some(true) — outline (black), Some(false) — fill (white).
fn cursor_pixel(kind: Cursor, dx: i32, dy: i32) -> Option<bool> {
    let (map, x, y) = match kind {
        Cursor::Arrow => (ARROW, dx, dy),
        Cursor::ResizeH => (RESIZE_H, dx, dy),
        Cursor::ResizeV => (RESIZE_H, dy, dx),
        Cursor::ResizeNwse => (RESIZE_NWSE, dx, dy),
        Cursor::ResizeNesw => (RESIZE_NWSE, 14 - dx, dy),
    };
    let row = map.get(usize::try_from(y).ok()?)?.as_bytes();
    match row.get(usize::try_from(x).ok()?)? {
        b'X' => Some(true),
        b'.' => Some(false),
        _ => None,
    }
}

/// Where the cursor is drawn from: the arrow from its tip, the others from the centre.
pub fn cursor_rect(kind: Cursor, x: i32, y: i32) -> Rect {
    match kind {
        Cursor::Arrow => Rect::new(x, y, 11, 17),
        _ => Rect::new(x - 7, y - 7, 15, 15),
    }
}

/// The cursor image for a display plane (`size`×`size` ARGB): the shape from the top-left corner.
pub fn cursor_image(kind: Cursor, zoom: i32, px: &mut [u32], size: usize) {
    let r = cursor_rect(kind, 0, 0);
    for dy in 0..r.h {
        for dx in 0..r.w {
            if let Some(outline) = cursor_pixel(kind, dx, dy) {
                let color = if outline { 0xFF00_0000 } else { 0xFFFF_FFFF };
                for zy in 0..zoom {
                    for zx in 0..zoom {
                        let (x, y) = ((dx * zoom + zx) as usize, (dy * zoom + zy) as usize);
                        if x < size && y < size {
                            px[y * size + x] = color;
                        }
                    }
                }
            }
        }
    }
}

/// The cursor is drawn straight into the framebuffer over the scene: when the mouse moves
/// there is no need to redraw the whole screen. (x, y) are screen pixels, `zoom` is how many times
/// larger to draw.
pub fn draw_cursor(fb: &Framebuffer, kind: Cursor, x: i32, y: i32, zoom: i32) {
    let r = cursor_rect(kind, 0, 0);
    for dy in 0..r.h {
        for dx in 0..r.w {
            if let Some(outline) = cursor_pixel(kind, dx, dy) {
                let color = if outline { 0x000000 } else { 0xFFFFFF };
                for (zx, zy) in (0..zoom).flat_map(|zx| (0..zoom).map(move |zy| (zx, zy))) {
                    fb.put(x + (r.x + dx) * zoom + zx, y + (r.y + dy) * zoom + zy, color);
                }
            }
        }
    }
}
