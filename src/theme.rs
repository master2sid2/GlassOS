//! The configurable appearance theme. The Settings application changes it, and the WM notices
//! the changes by the version number and redraws the windows.

use crate::gfx::Color;
use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicU64, Ordering};
use crate::sync::IrqLock as Spinlock;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// The gradient of the focused window's title: (top, bottom).
    pub title: (Color, Color),
    /// The opacity of titles (0..=255).
    pub title_alpha: u32,
    pub term_bg: Color,
    /// The opacity of windows (0..=255): a right click on the title sets a window its own.
    pub window_alpha: u32,
    /// The opacity of the top bar and the Dock.
    pub bar_alpha: u32,
    /// The opacity of context menus and the system menu.
    pub menu_alpha: u32,
    /// The opacity of notifications and icon panels.
    pub panel_alpha: u32,
    /// The wallpaper number: type and colour variant, see wallpaper::number.
    pub wallpaper: usize,
    /// The default glass frost — the background blur radius (0 — clear glass).
    pub frost: u32,
    /// The volume of system sounds (0..=256; 0 — muted).
    pub sound: u32,
    /// The welcome sound at login.
    pub startup_sound: bool,
    /// The laptop screen brightness in percent (0 — leave it as the firmware set it).
    pub brightness: u32,
    /// The top bar and the Dock on all screens, not only on the primary one.
    pub bars_everywhere: bool,
    /// After how many minutes of idle time the screensaver starts (0 — never).
    pub saver_minutes: u32,
    /// After how many minutes of idle time the display turns off (0 — never).
    pub display_off_minutes: u32,
    /// How many output lines a terminal tab keeps.
    pub scrollback: u32,
    /// The microphone volume (0..=100 %) and its gain (dB, 0..=24).
    pub mic_volume: u32,
    pub mic_boost: u32,
}

/// Limits for the setting: less than a screen is pointless, and 100 thousand lines are already megabytes.
pub const SCROLLBACK_RANGE: core::ops::RangeInclusive<u32> = 100..=100_000;
const MAX_SAVER_MINUTES: u32 = 240;
/// The largest frost (blur radius in pixels).
pub const MAX_FROST: u32 = 16;

pub const TITLE_PRESETS: [(&str, Color, Color); 6] = [
    ("Slate", 0x454C6E, 0x272C44),
    ("Ocean", 0x2F6496, 0x183455),
    ("Violet", 0x60449A, 0x2F2150),
    ("Teal", 0x2C7A73, 0x163F3C),
    ("Rose", 0x94456A, 0x4A2235),
    ("Graphite", 0x55565E, 0x2B2C31),
];

pub const TERM_PRESETS: [(&str, Color); 5] = [
    ("Midnight", 0x10121C),
    ("Navy", 0x0A1530),
    ("Plum", 0x1C0F27),
    ("Forest", 0x0C1D16),
    ("Black", 0x000000),
];

pub const DEFAULT: Theme = Theme {
    title: (TITLE_PRESETS[0].1, TITLE_PRESETS[0].2),
    title_alpha: 175,
    term_bg: TERM_PRESETS[0].1,
    window_alpha: 215,
    bar_alpha: 120,
    menu_alpha: 215,
    panel_alpha: 130,
    wallpaper: 0,
    frost: 8,
    sound: 200,
    startup_sound: true,
    brightness: 0,
    bars_everywhere: false,
    saver_minutes: 10,
    display_off_minutes: 30,
    scrollback: 1000,
    mic_volume: 100,
    mic_boost: 12,
};

static THEME: Spinlock<Theme> = Spinlock::new(DEFAULT);
static VERSION: AtomicU64 = AtomicU64::new(0);

pub fn get() -> Theme {
    *THEME.lock()
}

pub fn update(f: impl FnOnce(&mut Theme)) {
    let mut theme = THEME.lock();
    let before = *theme;
    f(&mut theme);
    if *theme != before {
        VERSION.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn version() -> u64 {
    VERSION.load(Ordering::Relaxed)
}

/// The theme as `key=value` text (colours in hex) for the settings file.
pub fn to_config(t: &Theme) -> String {
    format!(
        "title_top={:06X}\ntitle_bottom={:06X}\ntitle_alpha={}\nterm_bg={:06X}\nwindow_alpha={}\nbar_alpha={}\nmenu_alpha={}\npanel_alpha={}\nwallpaper={}\nfrost={}\nsound={}\nstartup_sound={}\nbrightness={}\nbars_everywhere={}\nsaver_minutes={}\ndisplay_off_minutes={}\nscrollback={}\nmic_volume={}\nmic_boost={}\n",
        t.title.0, t.title.1, t.title_alpha, t.term_bg, t.window_alpha, t.bar_alpha, t.menu_alpha, t.panel_alpha, t.wallpaper, t.frost, t.sound, t.startup_sound as u8, t.brightness, t.bars_everywhere as u8, t.saver_minutes, t.display_off_minutes, t.scrollback, t.mic_volume, t.mic_boost
    )
}

/// Parses the settings file; unknown and corrupted lines are skipped.
pub fn from_config(text: &str) -> Theme {
    let mut t = DEFAULT;
    for (key, value) in text.lines().filter_map(|l| l.split_once('=')) {
        let (key, value) = (key.trim(), value.trim());
        let hex = || u32::from_str_radix(value, 16).ok().map(|c| c & 0xFFFFFF);
        let alpha = || value.parse::<u32>().ok().map(|a| a.clamp(40, 255));
        match key {
            "title_top" => t.title.0 = hex().unwrap_or(t.title.0),
            "title_bottom" => t.title.1 = hex().unwrap_or(t.title.1),
            "title_alpha" => t.title_alpha = alpha().unwrap_or(t.title_alpha),
            "term_bg" => t.term_bg = hex().unwrap_or(t.term_bg),
            "window_alpha" => t.window_alpha = alpha().unwrap_or(t.window_alpha),
            "bar_alpha" => t.bar_alpha = alpha().unwrap_or(t.bar_alpha),
            "menu_alpha" => t.menu_alpha = alpha().unwrap_or(t.menu_alpha),
            "panel_alpha" => t.panel_alpha = alpha().unwrap_or(t.panel_alpha),
            "bars_everywhere" => t.bars_everywhere = value != "0",
            "saver_minutes" => t.saver_minutes = value.parse::<u32>().map_or(t.saver_minutes, |v| v.min(MAX_SAVER_MINUTES)),
            "display_off_minutes" => t.display_off_minutes = value.parse::<u32>().map_or(t.display_off_minutes, |v| v.min(MAX_SAVER_MINUTES)),
            "scrollback" => t.scrollback = value.parse::<u32>().map_or(t.scrollback, |v| v.clamp(*SCROLLBACK_RANGE.start(), *SCROLLBACK_RANGE.end())),
            "wallpaper" => t.wallpaper = value.parse().unwrap_or(0) % crate::wallpaper::COUNT,
            "sound" => t.sound = value.parse::<u32>().map_or(t.sound, |v| v.min(256)),
            "startup_sound" => t.startup_sound = value != "0",
            "brightness" => t.brightness = value.parse::<u32>().map_or(0, |v| v.min(100)),
            "frost" => t.frost = value.parse::<u32>().map_or(t.frost, |f| f.min(MAX_FROST)),
            "mic_volume" => t.mic_volume = value.parse::<u32>().map_or(t.mic_volume, |v| v.min(100)),
            "mic_boost" => t.mic_boost = value.parse::<u32>().map_or(t.mic_boost, |v| v.min(24)),
            _ => {}
        }
    }
    t
}
