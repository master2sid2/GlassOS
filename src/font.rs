//! TrueType fonts (built into the kernel) and a cache of rasterized glyphs.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use fontdue::{Font, FontSettings};
use crate::sync::IrqLock as Spinlock;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FontId {
    Regular,
    SemiBold,
    Mono,
}

pub struct Glyph {
    pub xmin: i32,
    pub ymin: i32,
    pub w: usize,
    pub h: usize,
    pub advance: f32,
    /// Pixel coverage 0..=255.
    pub bitmap: Vec<u8>,
}

struct Fonts {
    fonts: [Font; 3],
    /// Cyrillic: the same typefaces, as separate files; the main files do not contain it.
    cyrillic: [Font; 3],
    cache: BTreeMap<(FontId, char, u32), Glyph>,
}

static FONTS: Spinlock<Option<Fonts>> = Spinlock::new(None);

pub fn init() {
    let load = |bytes: &[u8]| Font::from_bytes(bytes, FontSettings::default()).expect("bad font");
    *FONTS.lock() = Some(Fonts {
        fonts: [
            load(include_bytes!("../assets/fonts/Inter-Regular.ttf")),
            load(include_bytes!("../assets/fonts/Inter-SemiBold.ttf")),
            load(include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf")),
        ],
        cyrillic: [
            load(include_bytes!("../assets/fonts/Inter-Cyrillic-Regular.ttf")),
            load(include_bytes!("../assets/fonts/Inter-Cyrillic-SemiBold.ttf")),
            load(include_bytes!("../assets/fonts/JetBrainsMono-Cyrillic-Regular.ttf")),
        ],
        cache: BTreeMap::new(),
    });
}

/// The font that has the character: the main one, and if it lacks the character — the Cyrillic one.
fn pick<'a>(fonts: &'a [Font; 3], cyrillic: &'a [Font; 3], font: FontId, ch: char) -> &'a Font {
    let main = &fonts[font as usize];
    if main.lookup_glyph_index(ch) == 0 && cyrillic[font as usize].lookup_glyph_index(ch) != 0 { &cyrillic[font as usize] } else { main }
}

pub fn with_glyph<R>(font: FontId, size: f32, ch: char, f: impl FnOnce(&Glyph) -> R) -> R {
    let mut guard = FONTS.lock();
    let Fonts { fonts, cyrillic, cache } = guard.as_mut().expect("fonts not initialized");
    let glyph = cache.entry((font, ch, size.to_bits())).or_insert_with(|| {
        let (m, bitmap) = pick(fonts, cyrillic, font, ch).rasterize(ch, size);
        Glyph { xmin: m.xmin, ymin: m.ymin, w: m.width, h: m.height, advance: m.advance_width, bitmap }
    });
    f(glyph)
}

/// Like with_glyph, but does not wait for the lock (for the panic screen: the crashed code may have held it).
pub fn try_with_glyph<R>(font: FontId, size: f32, ch: char, f: impl FnOnce(&Glyph) -> R) -> Option<R> {
    let mut guard = FONTS.try_lock()?;
    let Fonts { fonts, cyrillic, cache } = guard.as_mut()?;
    let glyph = cache.entry((font, ch, size.to_bits())).or_insert_with(|| {
        let (m, bitmap) = pick(fonts, cyrillic, font, ch).rasterize(ch, size);
        Glyph { xmin: m.xmin, ymin: m.ymin, w: m.width, h: m.height, advance: m.advance_width, bitmap }
    });
    Some(f(glyph))
}

pub fn ascent(font: FontId, size: f32) -> f32 {
    let guard = FONTS.lock();
    let fonts = guard.as_ref().expect("fonts not initialized");
    fonts.fonts[font as usize].horizontal_line_metrics(size).map_or(size, |m| m.ascent)
}

/// The width of a digit slot: the widest of the ten. Digits stand in equal cells,
/// otherwise with a changing number (copying, the clock) everything after it creeps.
pub fn digit_cell(font: FontId, size: f32) -> f32 {
    ('0'..='9').map(|d| with_glyph(font, size, d, |g| g.advance)).fold(0.0, f32::max)
}

/// Text in lines no wider than `width` pixels, by words. No more than `max` lines: if
/// it does not fit, the last one ends with an ellipsis.
pub fn wrap(font: FontId, size: f32, text: &str, width: i32, max: usize) -> alloc::vec::Vec<alloc::string::String> {
    use alloc::string::{String, ToString};
    let mut lines: alloc::vec::Vec<String> = alloc::vec::Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let candidate = if line.is_empty() { word.to_string() } else { alloc::format!("{line} {word}") };
        if !line.is_empty() && measure(font, size, &candidate) > width {
            lines.push(core::mem::replace(&mut line, word.to_string()));
        } else {
            line = candidate;
        }
    }
    lines.push(line);
    if lines.len() > max.max(1) {
        lines.truncate(max.max(1));
        if let Some(last) = lines.last_mut() {
            while !last.is_empty() && measure(font, size, &alloc::format!("{last}\u{2026}")) > width {
                last.pop();
            }
            last.push('\u{2026}');
        }
    }
    lines
}

pub fn measure(font: FontId, size: f32, s: &str) -> i32 {
    let cell = digit_cell(font, size);
    let width: f32 = s.chars().map(|ch| if ch.is_ascii_digit() { cell } else { with_glyph(font, size, ch, |g| g.advance) }).sum();
    libm::roundf(width) as i32
}
