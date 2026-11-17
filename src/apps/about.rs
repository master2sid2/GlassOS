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

    /// The window size that fits the text: the hardware lines differ from machine to machine.
    pub fn size(&self) -> (i32, i32) {
        let title = crate::font::measure(FontId::SemiBold, 28.0, OS_NAME) + 10 + crate::font::measure(FontId::Regular, 13.0, &self.version());
        let widest = self.lines.iter().map(|l| crate::font::measure(FontId::Regular, 14.0, l)).max().unwrap_or(0).max(title);
        (widest + 2 * MARGIN, TOP + LINE * self.lines.len() as i32 + 16)
    }

    fn version(&self) -> String {
        alloc::format!("version {}, build {}", crate::os_version!(), crate::crash::BUILD)
    }
}

const MARGIN: i32 = 22;
const TOP: i32 = 66;
const LINE: i32 = 23;

impl App for About {
    fn background(&self) -> Color {
        0x1C1F2E
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        let (x, mut y) = (a.x + MARGIN, a.y + 14);
        let w = c.text(x, y, OS_NAME, ACCENT, FontId::SemiBold, 28.0);
        c.text(x + w + 10, y + 12, &self.version(), TEXT_DIM, FontId::Regular, 13.0);
        y += TOP - 14;
        for line in &self.lines {
            c.text(x, y, line, 0xC3C8D8, FontId::Regular, 14.0);
            y += LINE;
        }
    }
}
