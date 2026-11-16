//! The image viewer: fit to the window or show as is, wheel zoom around
//! the cursor, dragging with the mouse, the neighbouring files of the folder with the arrows, GIF animation.
//!
//! The file is read and decoded in its own thread: a photo of tens of megapixels
//! must not stop the desktop.

use crate::font::{self, FontId};
use crate::fs;
use crate::gfx::{Canvas, Color, Rect, SCALE_100};
use crate::image::{self, Image};
use crate::input::{KEY_END, KEY_HOME, KEY_LEFT, KEY_RIGHT, LEFT};
use crate::interrupts::HZ;
use crate::smp::{self, SharedMut};
use crate::sync::IrqLock;
use crate::task;
use crate::wm::{App, Ctx, TEXT, TEXT_DIM};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

const BG: Color = 0x14161F;
const INFO_H: i32 = 30;
/// The scale is stored in 1/65536: this much means "one image pixel per screen pixel".
const ONE: i64 = 65536;
const MIN_ZOOM: i64 = ONE / 64;
const MAX_ZOOM: i64 = ONE * 32;
/// The checkerboard under transparent areas: the cell size and two colours.
const CHECK: usize = 12;
const CHECK_COLORS: [u32; 2] = [0x2B2F40, 0x363A4E];

type Slot = Arc<IrqLock<Option<Result<Image, String>>>>;

pub struct Viewer {
    path: String,
    /// The pictures of this folder and the index of the open one.
    siblings: Vec<String>,
    index: usize,
    loading: Option<Slot>,
    image: Option<Image>,
    error: Option<String>,
    frame: usize,
    next_frame: u64,
    /// None — fit to the window.
    zoom: Option<i64>,
    /// The top-left corner of the picture relative to the view area, in screen pixels.
    origin: (i64, i64),
    /// The view area in screen pixels and the screen scale from the last draw.
    view: (i64, i64),
    scale: i32,
    /// The finished frame and the parameters it was built with.
    picture: Vec<u32>,
    built: Option<(usize, i64, (i64, i64), (i64, i64))>,
    drag: Option<(i32, i32, (i64, i64))>,
    last_click: u64,
}

impl Viewer {
    pub fn new(path: String) -> Self {
        let folder = fs::parent(&path);
        let mut siblings: Vec<String> = fs::list(&folder)
            .unwrap_or_default()
            .into_iter()
            .filter(|e| !e.is_dir && image::is_picture(&e.name))
            .map(|e| e.name)
            .collect();
        siblings.sort_by_key(|n| n.to_lowercase());
        let index = siblings.iter().position(|n| n == fs::name(&path)).unwrap_or(0);
        let mut v = Self {
            path: String::new(),
            siblings,
            index,
            loading: None,
            image: None,
            error: None,
            frame: 0,
            next_frame: 0,
            zoom: None,
            origin: (0, 0),
            view: (1, 1),
            scale: SCALE_100,
            picture: Vec::new(),
            built: None,
            drag: None,
            last_click: 0,
        };
        v.load(path);
        v
    }

    fn load(&mut self, path: String) {
        self.path = path.clone();
        (self.image, self.error, self.frame, self.zoom, self.built) = (None, None, 0, None, None);
        let slot: Slot = Arc::new(IrqLock::new(None));
        self.loading = Some(slot.clone());
        task::spawn("picture", None, move || {
            let result = fs::read(&path).map_err(|e| format!("{e}")).and_then(|d| image::decode(&d).map_err(String::from));
            *slot.lock() = Some(result);
        });
    }

    /// A neighbouring file of the folder: `step` — how far forward, wrapping around.
    fn go(&mut self, step: i32) -> bool {
        let n = self.siblings.len() as i32;
        if n < 2 {
            return false;
        }
        self.index = (self.index as i32 + step).rem_euclid(n) as usize;
        let path = fs::resolve(&fs::parent(&self.path), &self.siblings[self.index]);
        self.load(path);
        true
    }

    /// The current scale: a fitted picture is not stretched beyond its natural size.
    fn current_zoom(&self) -> i64 {
        let Some(img) = &self.image else { return ONE };
        self.zoom.unwrap_or_else(|| (self.view.0 * ONE / img.w as i64).min(self.view.1 * ONE / img.h as i64).clamp(MIN_ZOOM, ONE))
    }

    /// Keeps the picture in the view area: smaller than it — centred, larger — without empty edges.
    fn settle(&mut self) {
        let Some(img) = &self.image else { return };
        let z = self.current_zoom();
        let (w, h) = (img.w as i64 * z / ONE, img.h as i64 * z / ONE);
        let place = |at: i64, size: i64, room: i64| if size <= room { (room - size) / 2 } else { at.clamp(room - size, 0) };
        self.origin = (place(self.origin.0, w, self.view.0), place(self.origin.1, h, self.view.1));
    }

    /// A new scale: the image point under `at` (screen pixels) stays in place.
    fn zoom_to(&mut self, zoom: Option<i64>, at: (i64, i64)) {
        let before = self.current_zoom();
        self.zoom = zoom.map(|z| z.clamp(MIN_ZOOM, MAX_ZOOM));
        let after = self.current_zoom();
        self.origin = (at.0 - (at.0 - self.origin.0) * after / before, at.1 - (at.1 - self.origin.1) * after / before);
        self.settle();
    }

    fn center(&self) -> (i64, i64) {
        (self.view.0 / 2, self.view.1 / 2)
    }

    /// Builds the frame in screen pixels: when shrinking it averages the source pixels, otherwise
    /// takes the nearest one. The work is split between the cores by rows.
    fn build(&mut self) {
        let Some(img) = &self.image else { return };
        let z = self.current_zoom();
        let key = (self.frame, z, self.origin, self.view);
        if self.built == Some(key) {
            return;
        }
        self.built = Some(key);
        let (dw, dh) = (self.view.0 as usize, self.view.1 as usize);
        self.picture.resize(dw * dh, 0);
        let (iw, ih) = (img.w as i64, img.h as i64);
        let (ox, oy) = self.origin;
        // Which image pixels fall into a screen column or row (the end is excluded).
        let span = |d: i64, origin: i64, size: i64| {
            let from = (d - origin) * ONE / z;
            let to = (d + 1 - origin) * ONE / z;
            (from < size && to >= 0 && d >= origin).then(|| (from.clamp(0, size - 1) as usize, to.clamp(from + 1, size) as usize))
        };
        let columns: Vec<Option<(usize, usize)>> = (0..dw as i64).map(|x| span(x, ox, iw)).collect();
        let px = &img.frames[self.frame].px;
        let stride = img.w;
        let out = SharedMut(self.picture.as_mut_ptr());
        smp::parallel(dh, &|y| {
            let row = unsafe { core::slice::from_raw_parts_mut(out.get().add(y * dw), dw) };
            let Some((top, bottom)) = span(y as i64, oy, ih) else {
                row.fill(BG | 0xFF00_0000);
                return;
            };
            for (x, (d, column)) in row.iter_mut().zip(&columns).enumerate() {
                let Some((left, right)) = *column else {
                    *d = BG | 0xFF00_0000;
                    continue;
                };
                let (mut a, mut r, mut g, mut b) = (0u64, 0u64, 0u64, 0u64);
                for sy in top..bottom {
                    for &p in &px[sy * stride + left..sy * stride + right] {
                        let alpha = (p >> 24) as u64;
                        a += alpha;
                        r += (p >> 16 & 255) as u64 * alpha;
                        g += (p >> 8 & 255) as u64 * alpha;
                        b += (p & 255) as u64 * alpha;
                    }
                }
                let full = 255 * ((bottom - top) * (right - left)) as u64;
                let under = CHECK_COLORS[(x / CHECK + y / CHECK) & 1];
                let over = |sum: u64, shift: u32| {
                    let back = (under >> shift & 255) as u64;
                    ((sum + back * (full - a)) / full) as u32
                };
                *d = 0xFF00_0000 | over(r, 16) << 16 | over(g, 8) << 8 | over(b, 0);
            }
        });
    }

    fn device(&self, x: i32, y: i32) -> (i64, i64) {
        ((x as i64 * self.scale as i64) / SCALE_100 as i64, (y as i64 * self.scale as i64) / SCALE_100 as i64)
    }

    fn info(&self) -> String {
        let name = fs::name(&self.path);
        let place = if self.siblings.len() > 1 { format!("   {} of {}", self.index + 1, self.siblings.len()) } else { String::new() };
        match &self.image {
            Some(img) => {
                let animated = if img.frames.len() > 1 { format!(", {} frames", img.frames.len()) } else { String::new() };
                let percent = self.current_zoom() * 100 / ONE;
                format!("{name}   {} \u{00D7} {}   {}{animated}   {percent}%{place}", img.w, img.h, img.format)
            }
            None => format!("{name}{place}"),
        }
    }
}

impl App for Viewer {
    fn background(&self) -> Color {
        BG
    }

    fn title(&self) -> Option<String> {
        Some(fs::name(&self.path).into())
    }

    fn location(&self) -> Option<String> {
        Some(self.path.clone())
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        let view = Rect::new(a.x, a.y, a.w, a.h - INFO_H);
        let device = c.device(view);
        self.scale = c.scale;
        let size = (device.w.max(1) as i64, device.h.max(1) as i64);
        if size != self.view {
            self.view = size;
            self.settle();
        }
        if self.image.is_some() {
            self.build();
            c.write(view, &self.picture);
        } else {
            let text = self.error.as_deref().unwrap_or("Loading\u{2026}");
            let w = font::measure(FontId::Regular, 14.0, text);
            c.text(view.x + (view.w - w) / 2, view.y + view.h / 2 - 9, text, TEXT_DIM, FontId::Regular, 14.0);
        }
        let bar = Rect::new(a.x, a.y + a.h - INFO_H, a.w, INFO_H);
        c.fill(bar, 0x1C1F2E);
        c.fill(Rect::new(bar.x, bar.y, bar.w, 1), 0x2E3247);
        c.text(bar.x + 14, bar.y + 7, &self.info(), TEXT, FontId::Regular, 13.0);
    }

    fn tick(&mut self, ctx: &Ctx) -> bool {
        let finished = self.loading.as_ref().and_then(|slot| slot.lock().take());
        if let Some(result) = finished {
            self.loading = None;
            match result {
                Ok(img) => {
                    self.next_frame = ctx.ticks + (img.frames[0].delay_ms as u64 * HZ / 1000).max(1);
                    self.image = Some(img);
                    self.settle();
                }
                Err(e) => self.error = Some(e),
            }
            return true;
        }
        let Some(img) = &self.image else { return false };
        if img.frames.len() < 2 || ctx.ticks < self.next_frame {
            return false;
        }
        self.frame = (self.frame + 1) % img.frames.len();
        self.next_frame = ctx.ticks + (img.frames[self.frame].delay_ms as u64 * HZ / 1000).max(1);
        true
    }

    fn scroll_at(&mut self, x: i32, y: i32, lines: i32) -> bool {
        if self.image.is_none() {
            return false;
        }
        // Each wheel click is a quarter of the scale, however many lines it means.
        let zoom = self.current_zoom();
        self.zoom_to(Some(if lines < 0 { zoom * 5 / 4 } else { zoom * 4 / 5 }), self.device(x, y));
        true
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        match ch {
            KEY_LEFT => self.go(-1),
            KEY_RIGHT | b' ' => self.go(1),
            KEY_HOME => self.go(-(self.index as i32)),
            KEY_END => self.go(self.siblings.len() as i32 - 1 - self.index as i32),
            b'+' | b'=' => {
                self.zoom_to(Some(self.current_zoom() * 5 / 4), self.center());
                true
            }
            b'-' => {
                self.zoom_to(Some(self.current_zoom() * 4 / 5), self.center());
                true
            }
            b'0' | b'f' => {
                self.zoom_to(None, self.center());
                true
            }
            b'1' => {
                self.zoom_to(Some(ONE), self.center());
                true
            }
            _ => false,
        }
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        if buttons & LEFT == 0 {
            self.drag = None;
            return false;
        }
        if pressed {
            let now = crate::interrupts::ticks();
            if now - self.last_click < HZ * 2 / 5 {
                // A double click: fit, or show pixel for pixel.
                self.last_click = 0;
                let target = if self.zoom.is_some() { None } else { Some(ONE) };
                self.zoom_to(target, self.device(x, y));
                return true;
            }
            self.last_click = now;
            self.drag = Some((x, y, self.origin));
            return false;
        }
        let Some((from_x, from_y, start)) = self.drag else { return false };
        let (dx, dy) = self.device(x - from_x, y - from_y);
        self.origin = (start.0 + dx, start.1 + dy);
        self.settle();
        true
    }

    fn memory(&self) -> usize {
        let frames = self.image.as_ref().map_or(0, |i| i.frames.iter().map(|f| f.px.len() * 4).sum());
        frames + self.picture.len() * 4
    }
}
