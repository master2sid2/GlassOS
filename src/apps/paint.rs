//! A paint program. Pictures are saved and opened in BMP format.

use super::dialog::{self, Mode, Reply};
use super::ui;
use crate::font::FontId;
use crate::fs;
use crate::gfx::{Canvas, Color, Rect};
use crate::input::LEFT;
use crate::wm::{ACCENT, App, Ctx};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

const PALETTE: [u32; 9] = [0x111111, 0xF8FAFC, 0xF87171, 0xFB923C, 0xFACC15, 0x4ADE80, 0x38BDF8, 0xA78BFA, 0xF472B6];
const TOOLBAR_H: i32 = 46;
const CANVAS_PAD: i32 = 10;
const BG: Color = 0x1C1F2E;

pub struct Paint {
    /// The width of the client area.
    cw: i32,
    /// The size of the drawing canvas.
    w: i32,
    h: i32,
    px: Vec<u32>,
    color: u32,
    last: Option<(i32, i32)>,
    /// The picture file (None — not saved yet).
    path: Option<String>,
    dialog: Option<(Mode, Reply)>,
    status: Option<String>,
    /// There are unsaved changes.
    modified: bool,
    /// Asking whether to save them, and what to do afterwards.
    prompt: Option<Then>,
    /// What to do once a file is chosen and written.
    after_save: Option<Then>,
    done: bool,
}

/// What waits for the answer to the unsaved-changes question.
#[derive(Clone, Copy)]
enum Then {
    Close,
    OpenFile,
}

impl Paint {
    pub fn new(cw: i32, ch: i32) -> Self {
        let (w, h) = (cw - 2 * CANVAS_PAD, ch - TOOLBAR_H - CANVAS_PAD);
        Self {
            cw,
            w,
            h,
            px: vec![0xFFFFFF; (w * h) as usize],
            color: PALETTE[0],
            last: None,
            path: None,
            dialog: None,
            status: None,
            modified: false,
            prompt: None,
            after_save: None,
            done: false,
        }
    }

    /// The paint program with a picture from a file.
    pub fn open(path: String, cw: i32, ch: i32) -> Self {
        let mut p = Self::new(cw, ch);
        p.load(path);
        p
    }

    fn button(&self, n: i32) -> Rect {
        let labels = ["Clear", "Save\u{2026}", "Open\u{2026}"];
        let mut x = self.cw - CANVAS_PAD;
        for label in &labels[..=n as usize] {
            x -= ui::button_width(label) + 6;
        }
        Rect::new(x + 6, 9, ui::button_width(labels[n as usize]), ui::BUTTON_H)
    }

    fn load(&mut self, path: String) {
        match fs::read(&path).ok().and_then(|d| bmp_decode(&d)) {
            Some((w, h, img)) => {
                self.px.fill(0xFFFFFF);
                for y in 0..h.min(self.h) {
                    for x in 0..w.min(self.w) {
                        self.px[(y * self.w + x) as usize] = img[(y * w + x) as usize];
                    }
                }
                self.status = Some(format!("{} \u{00d7} {}", w, h));
                self.path = Some(path);
                self.modified = false;
            }
            None => self.status = Some("Not a BMP image".into()),
        }
    }

    fn save(&mut self, path: String) {
        let data = bmp_encode(self.w, self.h, &self.px);
        self.status = Some(match fs::write(&path, &data) {
            Ok(()) => {
                self.modified = false;
                "Saved".into()
            }
            Err(e) => format!("Error: {e}"),
        });
        self.path = Some(path);
    }

    /// The choice in the unsaved-changes question: save or not. Cancel is `self.prompt = None`.
    fn finish_prompt(&mut self, save: bool) {
        let Some(then) = self.prompt.take() else { return };
        if save {
            let Some(path) = self.path.clone() else {
                // There is no file yet: choose a name first, the rest once it is written.
                self.after_save = Some(then);
                self.ask(Mode::Save);
                return;
            };
            self.save(path);
            if self.modified {
                return; // the write failed: the reason is already in the status line, the window stays
            }
        }
        self.proceed(then);
    }

    fn proceed(&mut self, then: Then) {
        match then {
            Then::Close => self.done = true,
            Then::OpenFile => {
                self.modified = false;
                self.ask(Mode::Open);
            }
        }
    }

    fn ask(&mut self, mode: Mode) {
        if mode == Mode::Open && self.modified {
            self.prompt = Some(Then::OpenFile);
            return;
        }
        if self.dialog.is_none() {
            let (dir, name) = match &self.path {
                Some(p) => (fs::parent(p), String::from(fs::name(p))),
                None => (crate::access::home(), String::from("drawing.bmp")),
            };
            self.dialog = Some((mode, dialog::request(mode, &dir, &name)));
        }
    }

    fn swatch(i: usize) -> Rect {
        Rect::new(CANVAS_PAD + 2 + i as i32 * 30, 12, 22, 22)
    }


    fn dot(&mut self, x: i32, y: i32) {
        for py in y - 2..=y + 2 {
            for px in x - 2..=x + 2 {
                if px >= 0 && py >= 0 && px < self.w && py < self.h {
                    self.px[(py * self.w + px) as usize] = self.color;
                }
            }
        }
    }

    fn line(&mut self, (x0, y0): (i32, i32), (x1, y1): (i32, i32)) {
        let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
        let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
        let (mut x, mut y, mut err) = (x0, y0, dx + dy);
        loop {
            self.dot(x, y);
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }
}

impl App for Paint {
    fn background(&self) -> Color {
        BG
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        for (i, &color) in PALETTE.iter().enumerate() {
            ui::swatch(c, Self::swatch(i).offset(a.x, a.y), color, color == self.color, BG);
        }
        ui::button(c, self.button(0).offset(a.x, a.y), "Clear", true);
        ui::button(c, self.button(1).offset(a.x, a.y), "Save\u{2026}", self.dialog.is_none());
        ui::button(c, self.button(2).offset(a.x, a.y), "Open\u{2026}", self.dialog.is_none());
        if let Some(status) = &self.status {
            let w = crate::font::measure(FontId::Regular, 12.0, status);
            let x = self.button(2).x - 12 - w;
            if x > Self::swatch(PALETTE.len() - 1).x + 30 {
                c.text(a.x + x, a.y + 15, status, ACCENT, FontId::Regular, 12.0);
            }
        }
        c.blit(a.x + CANVAS_PAD, a.y + TOOLBAR_H, self.w, &self.px);
        if self.prompt.is_some() {
            let name = self.path.as_deref().map_or("this drawing", fs::name);
            ui::draw_save_prompt(c, a, (self.cw, self.h + TOOLBAR_H + CANVAS_PAD), name);
        }
    }

    fn memory(&self) -> usize {
        self.px.len() * 4
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        if self.prompt.is_some() {
            match ch {
                b'\n' => self.finish_prompt(true),
                0x1B => self.prompt = None,
                _ => return false,
            }
            return true;
        }
        match ch {
            // Ctrl+S: to the same file, and if there is none yet — ask for a name.
            0x13 => match self.path.clone() {
                Some(path) => self.save(path),
                None => self.ask(Mode::Save),
            },
            0x0F => self.ask(Mode::Open),
            // Ctrl+V: a picture from the clipboard (for example, a screenshot) — to the top-left corner.
            0x16 => {
                let Some((w, h, px)) = crate::wm::clipboard_image() else { return false };
                for y in 0..h.min(self.h) {
                    for x in 0..w.min(self.w) {
                        self.px[(y * self.w + x) as usize] = px[(y * w + x) as usize];
                    }
                }
                self.status = Some(format!("Pasted {w} \u{00D7} {h}"));
                self.modified = true;
            }
            _ => return false,
        }
        true
    }

    fn tick(&mut self, _ctx: &Ctx) -> bool {
        let Some((mode, reply)) = &self.dialog else { return false };
        let Some(answer) = dialog::take(reply) else { return false };
        let mode = *mode;
        self.dialog = None;
        match (mode, answer) {
            (Mode::Open | Mode::OpenMany, Some(path)) => self.load(path),
            (Mode::Save, Some(path)) => {
                self.save(path);
                if !self.modified && let Some(then) = self.after_save.take() {
                    self.proceed(then);
                }
            }
            (_, None) => self.after_save = None,
        }
        true
    }

    fn unsaved(&self) -> bool {
        self.modified
    }

    fn ask_close(&mut self) -> bool {
        if self.modified {
            self.prompt = Some(Then::Close);
        }
        !self.modified
    }

    fn asking(&self) -> bool {
        self.prompt.is_some()
    }

    fn finished(&self) -> bool {
        self.done
    }

    fn location(&self) -> Option<String> {
        self.path.clone()
    }

    fn title(&self) -> Option<String> {
        Some(match &self.path {
            Some(p) => format!("{} \u{2014} Paint", fs::name(p)),
            None => "Paint".into(),
        })
    }

    /// A new canvas size; the drawing is kept (whatever does not fit is cropped).
    fn resize(&mut self, cw: i32, ch: i32) {
        let (w, h) = (cw - 2 * CANVAS_PAD, ch - TOOLBAR_H - CANVAS_PAD);
        let mut px = vec![0xFFFFFF; (w * h) as usize];
        let copy_w = w.min(self.w) as usize;
        for y in 0..h.min(self.h) {
            let (src, dst) = ((y * self.w) as usize, (y * w) as usize);
            px[dst..dst + copy_w].copy_from_slice(&self.px[src..src + copy_w]);
        }
        (self.cw, self.w, self.h, self.px) = (cw, w, h, px);
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        if self.prompt.is_some() {
            if pressed {
                let [save, skip, cancel] = ui::save_prompt_buttons((self.cw, self.h + TOOLBAR_H + CANVAS_PAD));
                if save.contains(x, y) {
                    self.finish_prompt(true);
                } else if skip.contains(x, y) {
                    self.finish_prompt(false);
                } else if cancel.contains(x, y) {
                    self.prompt = None;
                }
            }
            return true;
        }
        if pressed && y < TOOLBAR_H {
            if let Some(i) = (0..PALETTE.len()).find(|&i| Self::swatch(i).inset(-3).contains(x, y)) {
                self.color = PALETTE[i];
            } else if self.button(0).contains(x, y) {
                self.px.fill(0xFFFFFF);
                self.modified = true;
            } else if self.button(1).contains(x, y) {
                self.ask(Mode::Save);
            } else if self.button(2).contains(x, y) {
                self.ask(Mode::Open);
            }
            return true;
        }
        if buttons & LEFT == 0 || y < TOOLBAR_H {
            self.last = None;
            return false;
        }
        let p = (x - CANVAS_PAD, y - TOOLBAR_H);
        match self.last {
            Some(prev) => self.line(prev, p),
            None => self.dot(p.0, p.1),
        }
        self.last = Some(p);
        self.modified = true;
        true
    }
}

/// A picture in BMP: 24 bits per pixel, rows bottom to top, aligned to 4 bytes.
fn bmp_encode(w: i32, h: i32, px: &[u32]) -> Vec<u8> {
    let row = (w as usize * 3).div_ceil(4) * 4;
    let size = 54 + row * h as usize;
    let mut out = Vec::with_capacity(size);
    let le32 = |out: &mut Vec<u8>, v: u32| out.extend_from_slice(&v.to_le_bytes());
    out.extend_from_slice(b"BM");
    le32(&mut out, size as u32);
    le32(&mut out, 0);
    le32(&mut out, 54);
    le32(&mut out, 40);
    le32(&mut out, w as u32);
    le32(&mut out, h as u32);
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    for v in [0u32, (row * h as usize) as u32, 2835, 2835, 0, 0] {
        le32(&mut out, v);
    }
    for y in (0..h).rev() {
        let start = out.len();
        for x in 0..w {
            let p = px[(y * w + x) as usize];
            out.extend_from_slice(&[p as u8, (p >> 8) as u8, (p >> 16) as u8]);
        }
        out.resize(start + row, 0);
    }
    out
}

/// Parses an uncompressed BMP (24 or 32 bits per pixel). Returns (width, height, 0xRRGGBB pixels).
fn bmp_decode(d: &[u8]) -> Option<(i32, i32, Vec<u32>)> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(d.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(d.get(at..at + 4)?.try_into().ok()?));
    if d.get(..2)? != b"BM" {
        return None;
    }
    let offset = u32_at(10)? as usize;
    let w = u32_at(18)? as i32;
    let raw_h = u32_at(22)? as i32;
    let (h, top_down) = (raw_h.abs(), raw_h < 0);
    let bpp = u16_at(28)? as usize;
    let compression = u32_at(30)?;
    if !(bpp == 24 || bpp == 32) || !(compression == 0 || compression == 3) || w <= 0 || h <= 0 || w > 4096 || h > 4096 {
        return None;
    }
    let bytes = bpp / 8;
    let row = (w as usize * bytes).div_ceil(4) * 4;
    let mut px = vec![0u32; (w * h) as usize];
    for y in 0..h {
        let src_row = if top_down { y } else { h - 1 - y } as usize;
        let base = offset + src_row * row;
        let line = d.get(base..base + w as usize * bytes)?;
        for x in 0..w as usize {
            let p = &line[x * bytes..];
            px[y as usize * w as usize + x] = (p[2] as u32) << 16 | (p[1] as u32) << 8 | p[0] as u32;
        }
    }
    Some((w, h, px))
}
