use crate::font::FontId;
use crate::gfx::{Canvas, Color, Rect};
use crate::wm::{ACCENT, App, Ctx, OS_NAME, TEXT_DIM};
use alloc::string::String;
use alloc::vec::Vec;

pub struct About {
    lines: Vec<String>,
}

impl About {
    pub fn new(lines: Vec<String>) -> Self {
        Self { lines }
    }
}

impl App for About {
    fn background(&self) -> Color {
        0x1C1F2E
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        let (x, mut y) = (a.x + 22, a.y + 14);
        let w = c.text(x, y, OS_NAME, ACCENT, FontId::SemiBold, 28.0);
        c.text(x + w + 10, y + 12, &alloc::format!("version 0.2, build {}", crate::crash::BUILD), TEXT_DIM, FontId::Regular, 13.0);
        y += 52;
        for line in &self.lines {
            c.text(x, y, line, 0xC3C8D8, FontId::Regular, 14.0);
            y += 23;
        }
    }
}
