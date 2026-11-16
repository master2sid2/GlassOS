use crate::font::{self, FontId};
use crate::gfx::{Canvas, Color, Rect};
use crate::wm::{ACCENT, App, Ctx, TEXT};
use alloc::format;

pub struct Clock {
    last_sec: u8,
}

impl Clock {
    pub fn new() -> Self {
        Self { last_sec: 0xFF }
    }
}

impl App for Clock {
    fn background(&self) -> Color {
        0x161925
    }

    fn tick(&mut self, ctx: &Ctx) -> bool {
        let changed = ctx.time.sec != self.last_sec;
        self.last_sec = ctx.time.sec;
        changed
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, ctx: &Ctx) {
        let t = ctx.time;
        let time = format!("{:02}:{:02}:{:02}", t.hour, t.min, t.sec);
        let tw = font::measure(FontId::SemiBold, 60.0, &time);
        c.text(a.x + (a.w - tw) / 2, a.y + 8, &time, TEXT, FontId::SemiBold, 60.0);
        let date = format!("{:02}.{:02}.{}", t.day, t.month, t.year);
        let dw = font::measure(FontId::Regular, 16.0, &date);
        c.text(a.x + (a.w - dw) / 2, a.y + 92, &date, ACCENT, FontId::Regular, 16.0);
    }
}
