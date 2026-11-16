//! Settings: sections listed on the left, a search field and scrollable contents.
//!
//! A section is a list of rows (`Row`) built from the current system state:
//! a label, a value on the right and a control. Layout, drawing, scrolling,
//! search and mouse hit testing all work from this description, so a new setting
//! is one new row in `rows`.

use super::accounts::{self, AccountDialog};
use super::datetime::{self, Form, FormDialog, ZonePicker};
use super::languages::{self, LanguagePicker};
use super::transfer::size_text;
use super::ui::{self, SCROLL_W, ScrollBar, TextField};
use crate::audio::Sound;
use crate::font::{self, FontId};
use crate::gfx::{Canvas, Color, Rect};
use crate::input::LEFT;
use crate::keyboard::{self, SWITCHES};
use crate::net::{exporter, remote};
use crate::services::{self, Scope};
use crate::output::{self, Arrange, Layout};
use crate::theme::{self, TITLE_PRESETS};
use crate::wallpaper::{self, PRESETS as WALLPAPERS, STYLES as WALLPAPER_STYLES};
use crate::wm::{ACCENT, App, Ctx, TEXT, TEXT_DIM};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

const BG: Color = 0x1C1F2E;
const MIN_ALPHA: u32 = 60;
/// Sidebar: width, margins, height of the search field, an item and a group label.
const SIDE_W: i32 = 204;
const SIDE_PAD: i32 = 10;
const SEARCH_H: i32 = 32;
const ITEM_H: i32 = 32;
const GROUP_H: i32 = 28;
const GROUP_GAP: i32 = 9;
/// Content margins.
const PAD: i32 = 24;
/// Section header: title and description.
const HEAD_H: i32 = 62;
/// Height of a row with a label above the control, and of a whole single-line row.
const LABEL_H: i32 = 28;
const LINE_H: i32 = 44;
const SLIDER_H: i32 = 26;
const ROW_GAP: i32 = 12;
/// A "name — value" line on description pages.
const INFO_H: i32 = 30;
/// A settings group label inside a section.
const HEADING_H: i32 = 42;
/// How many scroll pixels per wheel line.
const WHEEL: i32 = 40;
/// A wrapped description line and a program output line.
const NOTE_LINE_H: i32 = 18;
const LOG_LINE_H: i32 = 17;
const LOG_PAD: i32 = 10;
/// Charge bar: the height of the row under the label and the bar thickness.
const METER_H: i32 = 14;
const METER_BAR: i32 = 6;
/// Row height in the user and language lists.
const LIST_H: i32 = 38;
/// Idle time choices before the screensaver: (minutes, label).
const SAVER_CHOICES: [(u32, &str); 5] = [(0, "Off"), (1, "1 min"), (5, "5 min"), (10, "10 min"), (30, "30 min")];
/// Idle time choices before the display turns off: (minutes, label).
const DISPLAY_OFF_CHOICES: [(u32, &str); 6] = [(0, "Never"), (1, "1 min"), (5, "5 min"), (10, "10 min"), (30, "30 min"), (60, "1 hour")];
const SOUNDS: [(&str, Sound); 5] = [
    ("Notification", Sound::Notification),
    ("Error", Sound::Error),
    ("Critical", Sound::Critical),
    ("Drive in", Sound::DriveIn),
    ("Drive out", Sound::DriveOut),
];
/// Screen diagram: height, margins and how many of its pixels edges snap at.
const MAP_H: i32 = 150;
const MAP_PAD: i32 = 8;
const MAP_PULL: i32 = 6;
/// Screen modes: grid cell width.
const MODE_W: i32 = 132;
/// Wallpaper thumbnail size.
const THUMB_W: i32 = 104;
const THUMB_H: i32 = 66;
const THUMB_GAP: i32 = 10;
/// Round colour swatches.
const SWATCH: i32 = 30;
const SWATCH_STEP: i32 = 46;

#[derive(Clone, Copy, PartialEq)]
enum Section {
    Appearance,
    Wallpaper,
    Screensaver,
    Display,
    Sound,
    Keyboard,
    Power,
    Network,
    Bluetooth,
    Disks,
    DateTime,
    Users,
    Services,
    Monitoring,
    Remote,
    Storage,
    About,
}

/// Everything that configures the system and hardware lives here, in groups. Network, Bluetooth and Disks are
/// window pages inside Settings (see `page`).
const SECTIONS: [(&str, &[Section]); 5] = [
    ("LOOK", &[Section::Appearance, Section::Wallpaper, Section::Screensaver]),
    ("DEVICES", &[Section::Display, Section::Sound, Section::Keyboard, Section::Bluetooth, Section::Power]),
    ("NETWORK", &[Section::Network, Section::Remote]),
    ("STORAGE", &[Section::Disks, Section::Storage]),
    ("SYSTEM", &[Section::DateTime, Section::Users, Section::Services, Section::Monitoring, Section::About]),
];

impl Section {
    fn name(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
            Section::Wallpaper => "Wallpaper",
            Section::Screensaver => "Screensaver",
            Section::Display => "Display",
            Section::Sound => "Sound",
            Section::Keyboard => "Keyboard",
            Section::Power => "Power",
            Section::Network => "Network",
            Section::Bluetooth => "Bluetooth",
            Section::Disks => "Disks",
            Section::DateTime => "Date & Time",
            Section::Users => "Users",
            Section::Services => "Services",
            Section::Monitoring => "Monitoring",
            Section::Remote => "Remote Access",
            Section::Storage => "Storage",
            Section::About => "About",
        }
    }

    fn note(self) -> &'static str {
        match self {
            Section::Appearance => "Colors and opacity, part by part",
            Section::Wallpaper => "First the type, then its colors",
            Section::Screensaver => "What the screen does when nobody is around",
            Section::Display => "Screens, their layout and scale",
            Section::Sound => "Volume and system sounds",
            Section::Keyboard => "Languages and how to switch them",
            Section::Power => "Sensors, sleep and the laptop lid",
            Section::Network => "Connection of this computer",
            Section::Bluetooth => "Wireless devices nearby: pair and connect",
            Section::Disks => "Drives, partitions and formatting",
            Section::DateTime => "Clock, time zone and internet time",
            Section::Users => "Who can sign in",
            Section::Services => "Lua programs that work in the background",
            Section::Monitoring => "Metrics of this computer for Prometheus",
            Section::Remote => "Commands, files and a new kernel from another computer",
            Section::Storage => "Drives and where your files live",
            Section::About => "This computer and the system version",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Slider {
    Title,
    Window,
    Bar,
    Menu,
    Panel,
    Frost,
    Sound,
    Brightness,
    MicVolume,
}

impl Slider {
    /// Value and limits.
    fn range(self, t: &theme::Theme) -> (u32, u32, u32) {
        match self {
            Slider::Title => (t.title_alpha, MIN_ALPHA, 255),
            Slider::Window => (t.window_alpha, MIN_ALPHA, 255),
            Slider::Bar => (t.bar_alpha, MIN_ALPHA, 255),
            Slider::Menu => (t.menu_alpha, MIN_ALPHA, 255),
            Slider::Panel => (t.panel_alpha, MIN_ALPHA, 255),
            Slider::Frost => (t.frost, 0, theme::MAX_FROST),
            Slider::Sound => (t.sound, 0, 256),
            Slider::Brightness => (crate::backlight::get().unwrap_or(100), crate::backlight::MIN, 100),
            Slider::MicVolume => (t.mic_volume, 0, 100),
        }
    }

    fn set(self, r: Rect, x: i32) {
        theme::update(|t| match self {
            Slider::Title => t.title_alpha = ui::slider_value(r, x, MIN_ALPHA, 255),
            Slider::Window => t.window_alpha = ui::slider_value(r, x, MIN_ALPHA, 255),
            Slider::Bar => t.bar_alpha = ui::slider_value(r, x, MIN_ALPHA, 255),
            Slider::Menu => t.menu_alpha = ui::slider_value(r, x, MIN_ALPHA, 255),
            Slider::Panel => t.panel_alpha = ui::slider_value(r, x, MIN_ALPHA, 255),
            Slider::Frost => t.frost = ui::slider_value(r, x, 0, theme::MAX_FROST),
            Slider::Sound => t.sound = ui::slider_value(r, x, 0, 256),
            Slider::Brightness => t.brightness = ui::slider_value(r, x, crate::backlight::MIN, 100),
            Slider::MicVolume => t.mic_volume = ui::slider_value(r, x, 0, 100),
        });
    }
}

/// What pressing a control does.
#[derive(Clone, Copy, PartialEq)]
enum Act {
    Open(Section),
    Slide(Slider),
    TitleColor(usize),
    WallpaperStyle(usize),
    WallpaperColor(usize),
    Bars,
    Saver(u32),
    SaverPreview,
    DisplayOff(u32),
    DisplayOffNow,
    Play(Sound),
    Startup,
    /// Sound goes here (index in the output list) / recording from here; gain; test.
    Output(usize),
    Input(usize),
    MicBoost(u32),
    MicTest,
    Reset(Section),
    Arrange(Arrange),
    Primary(usize),
    Identify,
    Scale(i32),
    Mode(usize),
    Restart,
    RestartSetup,
    RestartUsb,
    Keep,
    Revert,
    Language(usize),
    LanguageAdd,
    LanguageRemove,
    LanguageDefault,
    Switch(usize),
    User(usize),
    UserAdd,
    UserPassword,
    UserRemove,
    AutoSignIn,
    Sleep(bool),
    MemorySleep(bool),
    OpenNetwork,
    OpenBluetooth,
    HardwareReport,
    ZonePick,
    SetClock,
    TimeServer,
    AutoSummer,
    Summer(bool),
    Ntp,
    SyncNow,
    HardwareUtc(bool),
    Exporter,
    Remote,
    /// Debugging: keyboard and mouse from another computer (the debug-tools build flag).
    #[cfg(feature = "debug-tools")]
    DebugInput,
    RemoteShowKey,
    RemoteNewKey,
    ExporterPort,
    ExporterAccess(exporter::Access),
    ExporterAllowed,
    ServiceAdd(Scope),
    Service(usize),
    ServiceAuto,
    ServiceAgain,
    ServiceRun(bool),
    ServiceRestart,
    ServiceRemove,
    /// The only option, nothing to choose yet.
    Nothing,
}

/// A button: label, whether selected, whether enabled and what it does.
struct Choice {
    label: String,
    on: bool,
    enabled: bool,
    act: Act,
}

fn choice(label: impl Into<String>, on: bool, act: Act) -> Choice {
    Choice { label: label.into(), on, enabled: true, act }
}

/// A list item: a tag on the left, a title, a description on the right.
struct Item {
    code: String,
    name: String,
    note: String,
    on: bool,
    act: Act,
}

/// A row's control.
enum Widget {
    /// A settings group label.
    Heading,
    /// Only a value on the right.
    Info,
    /// A grey label without a value.
    Note,
    Slider(Slider),
    Toggle(bool, Act),
    /// One button on the right in the label row.
    Action(Choice),
    /// A wrapping row of buttons; in a grid all buttons have the same width.
    Buttons(Vec<Choice>),
    Grid(Vec<Choice>),
    Swatches(Vec<(Color, bool, Act)>),
    Thumbs(usize, usize),
    List(Vec<Item>),
    Map,
    /// Program output lines: dense, in a monospace font.
    Log(Vec<String>),
    /// A charge bar under the label: percent and colour.
    Meter(u32, Color),
}

struct Row {
    section: Section,
    label: String,
    value: String,
    widget: Widget,
}

impl Row {
    fn new(section: Section, label: impl Into<String>, value: impl Into<String>, widget: Widget) -> Self {
        Self { section, label: label.into(), value: value.into(), widget }
    }

    /// The text the search looks in.
    fn words(&self) -> String {
        let mut text = format!("{} {} {}", self.label, self.value, self.section.name());
        let mut add = |s: &str| {
            text.push(' ');
            text.push_str(s);
        };
        match &self.widget {
            Widget::Action(c) => add(&c.label),
            Widget::Buttons(list) | Widget::Grid(list) => list.iter().for_each(|c| add(&c.label)),
            Widget::List(list) => list.iter().for_each(|i| add(&i.name)),
            _ => {}
        }
        text.to_lowercase()
    }
}

/// A screen being dragged on the diagram.
#[derive(Clone, Copy)]
struct Moving {
    screen: usize,
    /// Where it was pressed.
    from: (i32, i32),
    /// Where the second screen was before and where it is now.
    start: (i32, i32),
    place: (i32, i32),
}

pub struct Settings {
    /// Width and height of the client area.
    cw: i32,
    ch: i32,
    section: Section,
    search: TextField,
    /// Keys go to the search field.
    typing: bool,
    /// The contents scrolled up by this many pixels, out of this many in total.
    scroll: i32,
    total: i32,
    /// Where the scroll bar thumb was grabbed.
    thumb_grab: Option<i32>,
    /// What was drawn last time: hit areas in client coordinates.
    hits: Vec<(Rect, Act)>,
    sidebar: Vec<(Rect, Section)>,
    /// The section list on the left also scrolls if the window is short.
    side_scroll: i32,
    side_total: i32,
    side_grab: Option<i32>,
    dragging: Option<(Slider, Rect)>,
    /// The selected (not yet applied) screen mode.
    mode: Option<(u32, u32)>,
    /// Thumbnails of the colour variants: which wallpaper type they were computed for, and the images.
    thumbnails: Option<(usize, Vec<Vec<u32>>)>,
    /// The selected user in the Users section.
    user: usize,
    /// The selected service (index in the shared list) and the open program picker dialog.
    service: usize,
    adding: Option<(super::dialog::Reply, Scope)>,
    services_seen: u64,
    exporter_seen: u64,
    remote_seen: u64,
    /// The remote access key is shown on screen.
    remote_key_shown: bool,
    /// The selected row in the language list.
    language: usize,
    fs_seen: u64,
    keyboard_seen: u64,
    stats_seen: u64,
    /// Microphone test: the running recording and the level (0..=100); devices seen.
    mic_test: Option<crate::audio::Capture>,
    mic_level: u32,
    devices_seen: (Vec<String>, Vec<String>, Option<String>, Option<String>),
    /// The second and the time-settings revision at the last draw.
    time_seen: (u8, u64),
    caret_on: bool,
    /// How many seconds were shown before the screen layout reverts.
    revert_left: Option<u64>,
    moving: Option<Moving>,
    /// The screen diagram from the last draw.
    map: Rect,
    /// The screen selected on the diagram: its scale and resolution are shown.
    screen: usize,
    /// The "About" lines (collected at boot).
    sysinfo: Vec<String>,
    /// The firmware opens its setup when the system asks.
    firmware_setup: bool,
    /// The section's window page (Network, Bluetooth, Disks) — drawn under the header instead of
    /// rows; created when entering the section.
    page: Option<(Section, Box<dyn App>)>,
    /// What to show on the page when it is created (for example, the Wi-Fi tab of Network).
    page_arg: Option<String>,
    /// The section was changed from outside (a tray shortcut) — redraw.
    dirty: bool,
}

/// A section (and subsection) by name: "keyboard", "network", "network:wifi", "bluetooth"…
fn parse_page(arg: &str) -> (Option<Section>, Option<String>) {
    let (name, sub) = arg.split_once(':').map_or((arg, None), |(a, b)| (a, Some(String::from(b))));
    let section = match name {
        "keyboard" => Section::Keyboard,
        "datetime" => Section::DateTime,
        "network" | "netcfg" => Section::Network,
        "wifi" => return (Some(Section::Network), Some("wifi".into())),
        "bluetooth" | "bt" => Section::Bluetooth,
        "disks" => Section::Disks,
        "storage" => Section::Storage,
        "remote" => Section::Remote,
        "about" => Section::About,
        "power" | "battery" => Section::Power,
        _ => return (None, None),
    };
    (Some(section), sub)
}

/// Charge bar colour: red — time to charge, yellow — less than a third.
fn meter_color(percent: u32, charging: bool) -> Color {
    match percent {
        _ if charging => ACCENT,
        0..=15 => 0xE5484D,
        16..=30 => 0xE5B84D,
        _ => 0x4DC27A,
    }
}

fn percent(alpha: u32) -> String {
    format!("{}%", alpha * 100 / 255)
}

impl Settings {
    /// `section` is the section to open the window at (from the language indicator).
    pub fn new(cw: i32, ch: i32, section: Option<&str>, sysinfo: Vec<String>) -> Self {
        let (section, page_arg) = section.map_or((None, None), parse_page);
        let section = section.unwrap_or(Section::Appearance);
        Self {
            cw,
            ch,
            section,
            search: TextField::new("", 40),
            typing: false,
            scroll: 0,
            total: 0,
            thumb_grab: None,
            hits: Vec::new(),
            sidebar: Vec::new(),
            side_scroll: 0,
            side_total: 0,
            side_grab: None,
            dragging: None,
            mode: crate::display::saved(),
            thumbnails: None,
            user: 0,
            service: 0,
            adding: None,
            services_seen: 0,
            exporter_seen: 0,
            remote_seen: 0,
            remote_key_shown: false,
            language: 0,
            fs_seen: 0,
            keyboard_seen: 0,
            stats_seen: 0,
            mic_test: None,
            mic_level: 0,
            devices_seen: Default::default(),
            time_seen: (0xFF, 0),
            caret_on: false,
            revert_left: None,
            moving: None,
            map: Rect::new(0, 0, 0, 0),
            screen: 0,
            sysinfo,
            firmware_setup: crate::bootvars::setup_supported(),
            page: None,
            page_arg,
            dirty: false,
        }
    }

    /// Sections that have a window page instead of rows.
    fn embeds(s: Section) -> bool {
        matches!(s, Section::Network | Section::Bluetooth | Section::Disks)
    }

    /// A window page is shown (not rows or search results).
    fn page_shown(&self) -> bool {
        !self.searching() && Self::embeds(self.section)
    }

    /// Where the page is: under the section header, to the right of the section list. Network and Bluetooth
    /// are designed for a narrow window — in a wide one their fields and buttons are not stretched across the screen;
    /// Disks needs the width (the partition bar).
    fn page_rect(&self) -> Rect {
        const NARROW: i32 = 600;
        let w = self.cw - SIDE_W - 1;
        let w = if self.section == Section::Disks { w } else { w.min(NARROW) };
        Rect::new(SIDE_W + 1, HEAD_H, w, self.ch - HEAD_H)
    }

    /// Creates the current section's page if it does not exist yet.
    fn ensure_page(&mut self) {
        if !self.page_shown() || self.page.as_ref().is_some_and(|(s, _)| *s == self.section) {
            return;
        }
        let r = self.page_rect();
        let arg = self.page_arg.take();
        let app: Box<dyn App> = match self.section {
            Section::Network => Box::new(super::NetSettings::new(r.w, r.h, arg.as_deref()).embedded()),
            Section::Bluetooth => Box::new(super::BtSettings::new(r.w, r.h).embedded()),
            _ => Box::new(super::disks::Disks::new(r.w, r.h)),
        };
        self.page = Some((self.section, app));
    }

    fn searching(&self) -> bool {
        !self.search.text.trim().is_empty()
    }

    fn search_box(&self) -> Rect {
        Rect::new(SIDE_PAD, 12, SIDE_W - 2 * SIDE_PAD, SEARCH_H)
    }

    /// Where the contents are visible: a screen countdown bar may stand below them.
    fn view(&self) -> Rect {
        let footer = if output::seconds_to_revert().is_some() { ui::BUTTON_H + 20 } else { 0 };
        Rect::new(SIDE_W + 1, 0, self.cw - SIDE_W - 1, self.ch - footer)
    }

    /// Width of the contents.
    fn width(&self) -> i32 {
        self.cw - SIDE_W - 2 * PAD - SCROLL_W
    }

    fn scrollbar(&self) -> ScrollBar {
        let view = self.view();
        let track = Rect::new(self.cw - SCROLL_W - 5, view.y + 6, SCROLL_W, view.h - 12);
        ScrollBar { track, len: self.total.max(0) as usize, rows: view.h as usize, top: self.scroll as usize }
    }

    /// Where the section list is visible (below the search field).
    fn side_view(&self) -> Rect {
        let top = 12 + SEARCH_H + 4;
        Rect::new(0, top, SIDE_W, self.ch - top)
    }

    fn side_bar(&self) -> ScrollBar {
        let view = self.side_view();
        let track = Rect::new(SIDE_W - SCROLL_W - 3, view.y + 4, SCROLL_W, view.h - 8);
        ScrollBar { track, len: self.side_total.max(0) as usize, rows: view.h as usize, top: self.side_scroll as usize }
    }

    fn side_scroll_to(&mut self, top: i32) {
        self.side_scroll = top.clamp(0, (self.side_total - self.side_view().h).max(0));
    }

    fn scroll_to(&mut self, top: i32) {
        self.scroll = top.clamp(0, (self.total - self.view().h).max(0));
    }

    fn open(&mut self, section: Section) {
        self.section = section;
        self.scroll = 0;
        self.search.set_text("");
        self.typing = false;
    }

    /// Thumbnails of the variants: the same wallpaper, only small. Computed when the type changes.
    fn thumbnails(&mut self, style: usize) {
        if self.thumbnails.as_ref().is_none_or(|(s, _)| *s != style) {
            let list = (0..WALLPAPERS.len()).map(|i| wallpaper::generate(wallpaper::number(style, i), THUMB_W, THUMB_H)).collect();
            self.thumbnails = Some((style, list));
        }
    }

    // ------------------------------------------------------------ section rows

    fn rows(&self, s: Section) -> Vec<Row> {
        let t = theme::get();
        let mut rows = Vec::new();
        let row = |label: &str, value: String, widget: Widget| Row::new(s, label, value, widget);
        match s {
            Section::Appearance => {
                let name = TITLE_PRESETS.iter().find(|p| (p.1, p.2) == t.title).map_or("Custom", |p| p.0);
                let colors = TITLE_PRESETS.iter().enumerate().map(|(i, p)| (p.1, t.title.0 == p.1, Act::TitleColor(i))).collect();
                rows.push(row("Global", String::new(), Widget::Heading));
                rows.push(row("Window color", name.into(), Widget::Swatches(colors)));
                let frost = if t.frost == 0 { "clear".into() } else { format!("{} px", t.frost) };
                rows.push(row("Glass frost", frost, Widget::Slider(Slider::Frost)));
                rows.push(row("Windows", String::new(), Widget::Heading));
                rows.push(row("Window opacity", percent(t.window_alpha), Widget::Slider(Slider::Window)));
                rows.push(row("Title bar opacity", percent(t.title_alpha), Widget::Slider(Slider::Title)));
                rows.push(row("Top bar and Dock", String::new(), Widget::Heading));
                rows.push(row("Bar opacity", percent(t.bar_alpha), Widget::Slider(Slider::Bar)));
                rows.push(row("Top bar and Dock on every screen", String::new(), Widget::Toggle(t.bars_everywhere, Act::Bars)));
                rows.push(row("Menus", String::new(), Widget::Heading));
                rows.push(row("Menu opacity", percent(t.menu_alpha), Widget::Slider(Slider::Menu)));
                rows.push(row("Notifications and panels", String::new(), Widget::Heading));
                rows.push(row("Panel opacity", percent(t.panel_alpha), Widget::Slider(Slider::Panel)));
            }
            Section::Wallpaper => {
                let (style, preset) = wallpaper::parts(t.wallpaper);
                let styles = WALLPAPER_STYLES.iter().enumerate().map(|(i, name)| choice(*name, i == style, Act::WallpaperStyle(i))).collect();
                rows.push(row("Type", String::new(), Widget::Buttons(styles)));
                rows.push(row("Colors", WALLPAPERS[preset].name.into(), Widget::Thumbs(style, preset)));
            }
            Section::Screensaver => {
                rows.push(row("Screensaver", String::new(), Widget::Heading));
                let times = SAVER_CHOICES.iter().map(|&(minutes, label)| choice(label, t.saver_minutes == minutes, Act::Saver(minutes))).collect();
                rows.push(row("Start after", String::new(), Widget::Buttons(times)));
                rows.push(row("Try it now", String::new(), Widget::Action(choice("Preview", false, Act::SaverPreview))));
                rows.push(row("Display", String::new(), Widget::Heading));
                let times = DISPLAY_OFF_CHOICES.iter().map(|&(minutes, label)| choice(label, t.display_off_minutes == minutes, Act::DisplayOff(minutes))).collect();
                rows.push(row("Turn off the display after", String::new(), Widget::Buttons(times)));
                rows.push(row("Any key or mouse move turns it back on.", String::new(), Widget::Note));
                rows.push(row("Turn it off right now", String::new(), Widget::Action(choice("Turn Off", false, Act::DisplayOffNow))));
            }
            Section::Display => self.display_rows(&mut rows),
            Section::Sound => {
                let volume = if t.sound == 0 { "off".into() } else { percent(t.sound * 255 / 256) };
                rows.push(row("Volume", volume, Widget::Slider(Slider::Sound)));
                let outputs = crate::audio::outputs();
                let current = crate::audio::current_output();
                if outputs.len() > 1 {
                    let list = outputs.iter().enumerate().map(|(i, name)| choice(name, current.as_ref() == Some(name), Act::Output(i))).collect();
                    rows.push(row("Play through", String::new(), Widget::Buttons(list)));
                } else if let Some(name) = current {
                    rows.push(row("Plays through", name, Widget::Info));
                }
                if crate::audio::available() {
                    let sounds = SOUNDS.iter().map(|&(label, sound)| choice(label, false, Act::Play(sound))).collect();
                    rows.push(row("Listen to system sounds", String::new(), Widget::Buttons(sounds)));
                } else {
                    rows.push(row("No sound card found", String::new(), Widget::Note));
                }
                rows.push(row("Play a sound at startup", String::new(), Widget::Toggle(t.startup_sound, Act::Startup)));
                rows.push(row("Microphone", String::new(), Widget::Heading));
                let inputs = crate::audio::inputs();
                if inputs.is_empty() {
                    rows.push(row("No microphone yet. A Bluetooth headset adds its microphone here when it connects.", String::new(), Widget::Note));
                } else {
                    let current = crate::audio::current_input();
                    let list = inputs.iter().enumerate().map(|(i, name)| choice(name, current.as_ref() == Some(name), Act::Input(i))).collect();
                    rows.push(row("Record from", String::new(), Widget::Buttons(list)));
                    rows.push(row("Microphone volume", format!("{}%", t.mic_volume), Widget::Slider(Slider::MicVolume)));
                    let boosts = [0, 6, 12, 18, 24].iter().map(|&db| choice(&if db == 0 { String::from("Off") } else { format!("+{db} dB") }, t.mic_boost == db, Act::MicBoost(db))).collect();
                    rows.push(row("Microphone boost", String::new(), Widget::Buttons(boosts)));
                    rows.push(row("Test the microphone", String::new(), Widget::Toggle(self.mic_test.is_some(), Act::MicTest)));
                    if self.mic_test.is_some() {
                        let color = match self.mic_level {
                            0..=70 => 0x4DC27A,
                            71..=90 => 0xE5B84D,
                            _ => 0xE5484D,
                        };
                        rows.push(row("Level", String::new(), Widget::Meter(self.mic_level, color)));
                        rows.push(row("Speak: the bar shows how loud the microphone hears you. Green is good, red is too loud. Bluetooth headphones switch to the headset mode meanwhile, so music pauses.", String::new(), Widget::Note));
                    }
                }
            }
            Section::Keyboard => self.keyboard_rows(&mut rows),
            Section::Power => {
                if let Some(b) = crate::battery::get() {
                    rows.push(row("Battery", String::new(), Widget::Heading));
                    let state = match (b.charging, b.on_ac) {
                        (true, _) => "charging",
                        (_, true) => "on AC power, not charging",
                        _ => "on battery",
                    };
                    rows.push(row("This computer", format!("{}% \u{00B7} {state}", b.percent), Widget::Meter(b.percent, meter_color(b.percent, b.charging))));
                    if let Some(m) = b.minutes.filter(|_| !b.on_ac) {
                        rows.push(row("Time left", format!("about {}h {:02}m", m / 60, m % 60), Widget::Info));
                    }
                }
                let devices = crate::battery::devices();
                if !devices.is_empty() {
                    rows.push(row("Device batteries", String::new(), Widget::Heading));
                    for d in devices {
                        rows.push(Row::new(s, &d.name, format!("{} \u{00B7} {}%", d.kind, d.percent), Widget::Meter(d.percent, meter_color(d.percent, false))));
                    }
                }
                rows.push(row("Sensors", String::new(), Widget::Heading));
                for (name, value) in crate::power::lines() {
                    rows.push(row(name, value, Widget::Info));
                }
                if crate::power::can_sleep_deep() || crate::hdmi::running() {
                    rows.push(row("Sleep", String::new(), Widget::Heading));
                }
                if crate::power::can_sleep_deep() {
                    let deep = crate::power::sleeping_deep();
                    let depth = vec![choice("Deep", deep, Act::Sleep(true)), choice("Light", !deep, Act::Sleep(false))];
                    rows.push(row("Sleep when idle", String::new(), Widget::Buttons(depth)));
                }
                if crate::hdmi::running() {
                    let on = crate::hdmi::watermarks_on();
                    rows.push(row("Let memory sleep behind the monitor", String::new(), Widget::Toggle(on, Act::MemorySleep(!on))));
                    rows.push(row("Saves power. Turns itself off if the picture breaks.", String::new(), Widget::Note));
                }
                if output::lid_known() {
                    rows.push(row("Lid", String::new(), Widget::Heading));
                    let unready = |label: &str| Choice { label: label.into(), on: false, enabled: false, act: Act::Nothing };
                    let actions = vec![choice("Turn off the screen", true, Act::Nothing), unready("Sleep"), unready("Hibernate")];
                    rows.push(row("When the lid closes", String::new(), Widget::Buttons(actions)));
                    rows.push(row("Work goes on, on the second screen if there is one.", String::new(), Widget::Note));
                }
            }
            Section::Network => {
                for (name, value) in crate::wm::network_rows() {
                    rows.push(row(name, value, Widget::Info));
                }
                rows.push(row("Networks, passwords, computer name", String::new(), Widget::Action(choice("Open Network", false, Act::OpenNetwork))));
            }
            Section::Bluetooth => {
                let paired = crate::bt::devices().iter().filter(|e| e.paired).count();
                let connected = crate::bt::devices().iter().filter(|e| e.link == Some(crate::bt::Link::Connected)).count();
                rows.push(row("Controller", crate::bt::status(), Widget::Info));
                rows.push(row("Paired devices", format!("{paired}, connected {connected}"), Widget::Info));
                rows.push(row("Find, pair and connect devices", String::new(), Widget::Action(choice("Open Bluetooth", false, Act::OpenBluetooth))));
            }
            Section::Disks => {
                rows.push(row("Drives, partitions and formatting", String::new(), Widget::Action(choice("Open Disks", false, Act::Open(Section::Disks)))));
            }
            Section::DateTime => self.time_rows(&mut rows),
            Section::Users => self.user_rows(&mut rows),
            Section::Services => self.service_rows(&mut rows),
            Section::Monitoring => self.monitoring_rows(&mut rows),
            Section::Remote => self.remote_rows(&mut rows),
            Section::Storage => {
                for v in crate::fs::volumes() {
                    let state = if v.offline { "offline".into() } else { format!("{}, {} of {}", v.kind, size_text(v.used), size_text(v.total)) };
                    rows.push(Row::new(s, v.name, state, Widget::Info));
                }
                let home = if crate::keep::active() { "memory and the USB drive" } else { "memory only" };
                rows.push(row("Settings and home folder are kept in", home.into(), Widget::Info));
            }
            Section::About => {
                rows.push(row(crate::wm::OS_NAME, "version 0.2".into(), Widget::Info));
                // USB mice and keyboards — as they are now (they get plugged and unplugged).
                let usb = crate::usb::list();
                let usb_of = |what: &str| -> Vec<String> {
                    usb.iter().filter_map(|l| l.split_once(": ")).filter_map(|(_, t)| t.strip_prefix(what)).map(|t| format!("USB {}", t.trim())).collect()
                };
                for line in &self.sysinfo {
                    let (name, value) = line.split_once(": ").unwrap_or((line, ""));
                    if name == "Mouse" {
                        let mice = usb_of("mouse ");
                        rows.push(Row::new(s, name, if mice.is_empty() { value.to_string() } else { mice.join(", ") }, Widget::Info));
                        let keyboards = usb_of("keyboard ");
                        rows.push(Row::new(s, "Keyboard", if keyboards.is_empty() { "PS/2".to_string() } else { keyboards.join(", ") }, Widget::Info));
                        continue;
                    }
                    rows.push(Row::new(s, name, value, Widget::Info));
                    // Memory and the GPU — as they are now, not as at boot.
                    if name == "RAM" {
                        let (used, free) = crate::heap_stats();
                        let size = |n: usize| crate::apps::transfer::size_text(n as u64);
                        rows.push(Row::new(s, "Memory in use", format!("{} of {}, {} free", size(used), size(used + free), size(free)), Widget::Info));
                    }
                }
                for (name, value) in crate::gpu::about() {
                    rows.push(Row::new(s, name, value, Widget::Info));
                }
                rows.push(row("Save a report to the USB stick", String::new(), Widget::Action(choice("Hardware report", false, Act::HardwareReport))));
                // For firmwares that do not hear the keyboard before the system (Fast Boot).
                let setup = Choice { label: "Restart into setup".into(), on: false, enabled: self.firmware_setup, act: Act::RestartSetup };
                rows.push(row("Firmware settings (BIOS)", String::new(), Widget::Action(setup)));
                if !self.firmware_setup {
                    rows.push(row("This firmware does not open them on request; its key at power on is often F2 or Del.", String::new(), Widget::Note));
                }
                rows.push(row("Start once from the USB stick", String::new(), Widget::Action(choice("Restart from USB", false, Act::RestartUsb))));
            }
        }
        if self.can_reset(s) {
            rows.push(row("", String::new(), Widget::Buttons(vec![Choice { label: "Reset this section".into(), on: false, enabled: self.differs(s), act: Act::Reset(s) }])));
        }
        rows
    }

    fn can_reset(&self, s: Section) -> bool {
        matches!(s, Section::Appearance | Section::Wallpaper | Section::Screensaver | Section::Sound)
    }

    /// Whether the section differs from the default settings.
    fn differs(&self, s: Section) -> bool {
        let (t, d) = (theme::get(), theme::DEFAULT);
        match s {
            Section::Appearance => {
                (t.title, t.frost, t.bars_everywhere) != (d.title, d.frost, d.bars_everywhere)
                    || [t.title_alpha, t.window_alpha, t.bar_alpha, t.menu_alpha, t.panel_alpha] != [d.title_alpha, d.window_alpha, d.bar_alpha, d.menu_alpha, d.panel_alpha]
            }
            Section::Wallpaper => t.wallpaper != d.wallpaper,
            Section::Screensaver => (t.saver_minutes, t.display_off_minutes) != (d.saver_minutes, d.display_off_minutes),
            Section::Sound => (t.sound, t.startup_sound, t.mic_volume, t.mic_boost) != (d.sound, d.startup_sound, d.mic_volume, d.mic_boost),
            _ => false,
        }
    }

    fn reset(&self, s: Section) {
        let d = theme::DEFAULT;
        theme::update(|t| match s {
            Section::Appearance => {
                (t.title, t.frost, t.bars_everywhere) = (d.title, d.frost, d.bars_everywhere);
                (t.title_alpha, t.window_alpha, t.bar_alpha, t.menu_alpha, t.panel_alpha) = (d.title_alpha, d.window_alpha, d.bar_alpha, d.menu_alpha, d.panel_alpha);
            }
            Section::Wallpaper => t.wallpaper = d.wallpaper,
            Section::Screensaver => (t.saver_minutes, t.display_off_minutes) = (d.saver_minutes, d.display_off_minutes),
            Section::Sound => (t.sound, t.startup_sound, t.mic_volume, t.mic_boost) = (d.sound, d.startup_sound, d.mic_volume, d.mic_boost),
            _ => {}
        });
    }

    fn display_rows(&self, rows: &mut Vec<Row>) {
        let s = Section::Display;
        if crate::backlight::available() {
            let value = crate::backlight::get().map_or(String::new(), |b| format!("{b}%"));
            rows.push(Row::new(s, "Brightness", value, Widget::Slider(Slider::Brightness)));
        }
        if output::count() > 1 {
            let layout = output::current();
            let hint = match layout.arrange {
                Arrange::Extend => "drag a screen to move it".to_string(),
                Arrange::Mirror => "both show one desktop, the scales are linked".to_string(),
                Arrange::Only(i) => format!("only screen {}", i + 1),
            };
            let arranges = [(Arrange::Extend, "Extend"), (Arrange::Mirror, "Mirror"), (Arrange::Only(0), "Only 1"), (Arrange::Only(1), "Only 2")];
            let buttons = arranges.iter().map(|&(a, label)| choice(label, layout.arrange == a, Act::Arrange(a))).collect();
            rows.push(Row::new(s, "Screens", hint, Widget::Buttons(buttons)));
            rows.push(Row::new(s, "", String::new(), Widget::Map));
            let extended = layout.arrange == Arrange::Extend;
            let mains = (0..2)
                .map(|i| Choice { label: format!("Screen {}", i + 1), on: extended && layout.primary == i, enabled: extended, act: Act::Primary(i) })
                .collect();
            rows.push(Row::new(s, "Main screen", String::new(), Widget::Buttons(mains)));
            rows.push(Row::new(s, "Show screen numbers", String::new(), Widget::Action(choice("Identify", false, Act::Identify))));
        }
        let now = self.scale_now();
        let scales = output::SCALES.iter().map(|&scale| choice(format!("{scale}%"), (scale - now).abs() <= 1, Act::Scale(scale))).collect();
        rows.push(Row::new(s, "Scale", String::new(), Widget::Buttons(scales)));

        let (cw, ch) = crate::display::current();
        let monitor = self.own_mode();
        let title = if output::count() > 1 { format!("Screen {} resolution", self.selected() + 1) } else { "Screen resolution".into() };
        let chosen = self.mode.unwrap_or((cw, ch));
        let (value, modes): (String, Vec<Choice>) = if monitor {
            let now = output::monitor_mode();
            let value = now.map_or(String::new(), |m| format!("now {} \u{00D7} {}, {} Hz", m.w, m.h, m.hz));
            (value, output::monitor_modes().into_iter().enumerate().map(|(i, m)| choice(format!("{}\u{00D7}{} {} Hz", m.w, m.h, m.hz), Some(m) == now, Act::Mode(i))).collect())
        } else {
            let list = crate::display::modes().into_iter().enumerate().map(|(i, (w, h))| choice(format!("{w} \u{00D7} {h}"), (w, h) == chosen, Act::Mode(i))).collect();
            (format!("now {cw} \u{00D7} {ch}"), list)
        };
        let count = modes.len();
        rows.push(Row::new(s, title, value, Widget::Grid(modes)));
        let note = if monitor {
            "Picks up at once. Press Keep, or it goes back by itself."
        } else if chosen != (cw, ch) {
            "Restart to switch to the new resolution."
        } else if count <= 1 {
            "The firmware offers no other modes."
        } else {
            "Large modes are slower: the screen is redrawn in software."
        };
        rows.push(Row::new(s, note, String::new(), Widget::Note));
        if !monitor {
            let restart = Choice { label: "Restart now".into(), on: false, enabled: chosen != (cw, ch), act: Act::Restart };
            rows.push(Row::new(s, "Apply the new resolution", String::new(), Widget::Action(restart)));
        }
    }

    fn keyboard_rows(&self, rows: &mut Vec<Row>) {
        let s = Section::Keyboard;
        let languages = keyboard::enabled();
        let items = languages
            .iter()
            .enumerate()
            .map(|(i, &l)| Item {
                code: keyboard::LAYOUTS[l].code.into(),
                name: keyboard::LAYOUTS[l].name.into(),
                note: if l == keyboard::default() { "default".into() } else { String::new() },
                on: i == self.language,
                act: Act::Language(i),
            })
            .collect();
        rows.push(Row::new(s, "Input languages", String::new(), Widget::List(items)));
        let picked = languages.get(self.language).copied().unwrap_or(keyboard::LATIN);
        let actions = vec![
            Choice { label: "Add Language".into(), on: false, enabled: languages.len() < keyboard::LAYOUTS.len(), act: Act::LanguageAdd },
            Choice { label: "Remove".into(), on: false, enabled: picked != keyboard::LATIN, act: Act::LanguageRemove },
            Choice { label: "Make Default".into(), on: false, enabled: picked != keyboard::default(), act: Act::LanguageDefault },
        ];
        rows.push(Row::new(s, "", String::new(), Widget::Buttons(actions)));
        let switches = SWITCHES.iter().enumerate().map(|(i, (switch, label, _))| choice(*label, *switch == keyboard::switch(), Act::Switch(i))).collect();
        rows.push(Row::new(s, "Switch languages with", String::new(), Widget::Buttons(switches)));
    }

    fn time_rows(&self, rows: &mut Vec<Row>) {
        let s = Section::DateTime;
        let row = |label: &str, value: String, widget: Widget| Row::new(s, label, value, widget);
        let admin = crate::access::is_admin();
        let (set, zone, now) = (crate::time::settings(), crate::time::zone(), crate::time::now());
        let button = |label: &str, enabled: bool, act: Act| Choice { label: label.into(), on: false, enabled, act };
        rows.push(row("Clock", String::new(), Widget::Heading));
        rows.push(row(&crate::time::long_date(&now), format!("{:02}:{:02}:{:02}", now.hour, now.min, now.sec), Widget::Info));
        let utc = crate::time::to_time(crate::time::unix());
        rows.push(row("Universal time (UTC)", format!("{:02}:{:02}", utc.hour, utc.min), Widget::Info));
        rows.push(row("Set the date and time by hand", String::new(), Widget::Action(button("Change\u{2026}", admin && !set.ntp, Act::SetClock))));
        if set.ntp {
            rows.push(row("Internet time sets the clock. Turn it off to set it by hand.", String::new(), Widget::Note));
        }

        rows.push(row("Time zone", String::new(), Widget::Heading));
        let shown = if crate::time::zone_chosen() { zone.name.replace('_', " ") } else { format!("{}, guessed from the clock", zone.name) };
        let season = if crate::time::summer_now() { "summer" } else { "winter" };
        let offset = format!("{}, {season}", crate::time::offset_text(crate::time::offset_at(crate::time::unix())));
        rows.push(row(&shown, offset, Widget::Action(button("Change\u{2026}", admin, Act::ZonePick))));
        if zone.rule.is_some() {
            rows.push(row("Change to summer time and back automatically", String::new(), Widget::Toggle(set.auto_summer, Act::AutoSummer)));
        }
        if zone.rule.is_some() && set.auto_summer {
            let next = crate::time::change_text().unwrap_or_default();
            let label = if crate::time::summer_now() { "Summer time now, back to winter time" } else { "Winter time now, summer time from" };
            rows.push(row(label, next, Widget::Info));
        } else {
            let summer = !set.auto_summer && set.summer;
            let kinds = vec![
                Choice { label: "Winter time".into(), on: !summer, enabled: admin, act: Act::Summer(false) },
                Choice { label: "Summer time, one hour ahead".into(), on: summer, enabled: admin, act: Act::Summer(true) },
            ];
            rows.push(row("Clock runs on", String::new(), Widget::Buttons(kinds)));
        }

        rows.push(row("Internet time", String::new(), Widget::Heading));
        rows.push(row("Set the time automatically", String::new(), Widget::Toggle(set.ntp, Act::Ntp)));
        rows.push(row("Server", set.server().into(), Widget::Action(button("Change\u{2026}", admin, Act::TimeServer))));
        let last = match crate::time::last_sync() {
            _ if crate::time::syncing() => "checking\u{2026}".into(),
            None => "not yet".into(),
            Some(r) => {
                let t = crate::time::local(r.at);
                let when = format!("{:02}:{:02}", t.hour, t.min);
                match r.result {
                    Ok(ms) if ms.abs() < 1000 => format!("at {when}, the clock was right"),
                    Ok(ms) => format!("at {when}, moved {} by {}.{} s", if ms > 0 { "ahead" } else { "back" }, ms.abs() / 1000, ms.abs() % 1000 / 100),
                    Err(e) => format!("at {when}: {e}"),
                }
            }
        };
        rows.push(row("Last check", last, Widget::Action(button("Sync Now", !crate::time::syncing(), Act::SyncNow))));

        rows.push(row("Hardware clock", String::new(), Widget::Heading));
        let keeps = vec![
            Choice { label: "Local time".into(), on: !set.rtc_utc, enabled: admin, act: Act::HardwareUtc(false) },
            Choice { label: "Universal time (UTC)".into(), on: set.rtc_utc, enabled: admin, act: Act::HardwareUtc(true) },
        ];
        rows.push(row("The clock in the computer keeps", String::new(), Widget::Buttons(keeps)));
        rows.push(row("Windows keeps local time there, Linux usually UTC.", String::new(), Widget::Note));
        if !admin {
            rows.push(row("Only an administrator can change these settings.", String::new(), Widget::Note));
        }
    }

    fn monitoring_rows(&self, rows: &mut Vec<Row>) {
        let s = Section::Monitoring;
        let row = |label: &str, value: String, widget: Widget| Row::new(s, label, value, widget);
        let admin = crate::access::is_admin();
        let set = exporter::settings();
        let online = crate::net::subnet();
        let button = |label: &str, enabled: bool, act: Act| Choice { label: label.into(), on: false, enabled, act };
        rows.push(row("Prometheus exporter", String::new(), Widget::Heading));
        // Without a network there is nothing to enable: the switch is inactive while it is off.
        let act = if admin && (set.enabled || online.is_some()) { Act::Exporter } else { Act::Nothing };
        rows.push(row("Give metrics to Prometheus", String::new(), Widget::Toggle(set.enabled, act)));
        if online.is_none() && !set.enabled {
            rows.push(row("Needs a network connection.", String::new(), Widget::Note));
        }
        let state = exporter::status();
        let shown = match (state.as_str(), online) {
            ("on", Some((ip, _, _))) => format!("http://{}:{}/metrics", crate::net::ip_str(ip), set.port),
            ("", _) => "off".into(),
            (other, _) => other.into(),
        };
        rows.push(row("Address", shown, Widget::Info));
        rows.push(row("Port", set.port.to_string(), Widget::Action(button("Change\u{2026}", admin, Act::ExporterPort))));
        let kinds = [(exporter::Access::Local, "This network"), (exporter::Access::Listed, "Listed addresses"), (exporter::Access::Anyone, "Anyone")];
        let kinds = kinds.iter().map(|&(a, label)| Choice { label: label.into(), on: set.access == a, enabled: admin, act: Act::ExporterAccess(a) }).collect();
        rows.push(row("Who can read the metrics", String::new(), Widget::Buttons(kinds)));
        if set.access == exporter::Access::Listed {
            let list: Vec<String> = set.allowed.iter().map(|ip| crate::net::ip_str(*ip)).collect();
            let list = if list.is_empty() { "none yet".into() } else { list.join(", ") };
            rows.push(row("Addresses", list, Widget::Action(button("Change\u{2026}", admin, Act::ExporterAllowed))));
        }
        let last = match exporter::last_reader() {
            Some((ip, ago)) => format!("{}, {} ago", crate::net::ip_str(ip), duration_text(ago)),
            None => "not yet".into(),
        };
        rows.push(row("Last read by", last, Widget::Info));
        let refused = exporter::refused();
        if refused > 0 {
            rows.push(row("Refused requests", refused.to_string(), Widget::Info));
        }
        rows.push(row("In prometheus.yml, add this address to the targets of a scrape job. All metrics are named glassos_\u{2026}", String::new(), Widget::Note));
        if !admin {
            rows.push(row("Only an administrator can change these settings.", String::new(), Widget::Note));
        }
    }

    fn remote_rows(&self, rows: &mut Vec<Row>) {
        let s = Section::Remote;
        let row = |label: &str, value: String, widget: Widget| Row::new(s, label, value, widget);
        let admin = crate::access::is_admin();
        let set = remote::settings();
        let online = crate::net::subnet();
        let button = |label: &str, enabled: bool, act: Act| Choice { label: label.into(), on: false, enabled, act };
        rows.push(row("Remote access", String::new(), Widget::Heading));
        rows.push(row("Another computer on this network can run terminal commands here, read the log and files, take screenshots, try a new kernel and restart. Every request is signed with the key; the traffic itself is not encrypted, so use it only on your own network.", String::new(), Widget::Note));
        let act = if admin && (set.enabled || online.is_some()) { Act::Remote } else { Act::Nothing };
        rows.push(row("Allow remote access", String::new(), Widget::Toggle(set.enabled, act)));
        if online.is_none() && !set.enabled {
            rows.push(row("Needs a network connection.", String::new(), Widget::Note));
        }
        let state = remote::status();
        let shown = match (set.enabled, state.as_str(), online) {
            (false, _, _) => "off".into(),
            (true, "on", Some((ip, _, _))) => format!("{}:{}", crate::net::ip_str(ip), set.port),
            (true, other, _) => other.into(),
        };
        rows.push(row("Address", shown, Widget::Info));
        if set.enabled {
            rows.push(row("Commands run as", set.user.clone(), Widget::Info));
            let label = if self.remote_key_shown { "Hide Key" } else { "Show Key" };
            let keys = vec![button(label, admin, Act::RemoteShowKey), button("New Key", admin, Act::RemoteNewKey)];
            rows.push(row("Key", String::new(), Widget::Buttons(keys)));
            if self.remote_key_shown && admin {
                let key = remote::key_text().unwrap_or_else(String::from);
                rows.push(row(&key, String::new(), Widget::Note));
            }
            rows.push(row("To hand the key to the other computer as a file, type remote key > FILE in the terminal, for example into a shared folder. A new key stops the old one.", String::new(), Widget::Note));
            // Debugging (the debug-tools build flag): input from another computer.
            #[cfg(feature = "debug-tools")]
            {
                let on = crate::debug::enabled(crate::debug::INPUT);
                rows.push(row("Keyboard and mouse from the other computer (debug)", String::new(), Widget::Toggle(on, if admin { Act::DebugInput } else { Act::Nothing })));
                rows.push(row("For testing: the other computer can type and click here. Keep it off when not needed.", String::new(), Widget::Note));
            }
        }
        let last = match remote::last_client() {
            Some((ip, ago)) => format!("{}, {} ago", crate::net::ip_str(ip), duration_text(ago)),
            None => "not yet".into(),
        };
        rows.push(row("Last request from", last, Widget::Info));
        if remote::refused() > 0 {
            rows.push(row("Refused requests", remote::refused().to_string(), Widget::Info));
        }
        let kernel = crate::install::BOOT_PATH.lock().clone();
        let kernel = if remote::trial() { format!("{kernel} (trial)") } else { kernel };
        rows.push(row("Kernel", kernel, Widget::Info));
        if !admin {
            rows.push(row("Only an administrator can change these settings.", String::new(), Widget::Note));
        }
    }

    /// Services the current user sees: system ones and their own (an administrator sees all).
    fn visible_services(&self) -> Vec<(usize, services::Service)> {
        let me = crate::access::user().unwrap_or_default();
        let admin = crate::access::is_admin();
        services::list().into_iter().enumerate().filter(|(_, s)| admin || s.scope == Scope::System || s.user == me).collect()
    }

    fn service_rows(&self, rows: &mut Vec<Row>) {
        let s = Section::Services;
        let row = |label: &str, value: String, widget: Widget| Row::new(s, label, value, widget);
        let admin = crate::access::is_admin();
        let me = crate::access::user().unwrap_or_default();
        let button = |label: &str, enabled: bool, act: Act| Choice { label: label.into(), on: false, enabled, act };
        rows.push(row("A service is a Lua program without a window. System services run from startup, even before anyone signs in. Your own services run while you are signed in.", String::new(), Widget::Note));
        let adding = self.adding.is_none();
        let add = vec![button("Add My Service\u{2026}", adding, Act::ServiceAdd(Scope::User)), button("Add System Service\u{2026}", adding && admin, Act::ServiceAdd(Scope::System))];
        rows.push(row("", String::new(), Widget::Buttons(add)));
        let list = self.visible_services();
        if list.is_empty() {
            rows.push(row("No services yet.", String::new(), Widget::Note));
            return;
        }
        let picked = list.iter().find(|(i, _)| *i == self.service).or(list.first()).map(|(i, _)| *i).unwrap_or(0);
        let items = list
            .iter()
            .map(|(i, sv)| Item {
                code: if sv.scope == Scope::System { "SYS".into() } else { "USR".into() },
                name: if sv.scope == Scope::User && sv.user != me { format!("{} ({})", sv.name, sv.user) } else { sv.name.clone() },
                note: state_text(sv),
                on: *i == picked,
                act: Act::Service(*i),
            })
            .collect();
        rows.push(row("", String::new(), Widget::List(items)));
        let Some((_, sv)) = list.iter().find(|(i, _)| *i == picked) else { return };
        let mine = match sv.scope {
            Scope::System => admin,
            Scope::User => sv.user == me || admin,
        };
        rows.push(row(&sv.name, String::new(), Widget::Heading));
        rows.push(row("Program", sv.path.clone(), Widget::Info));
        let kind = if sv.scope == Scope::System { "system service" } else { "user service" };
        rows.push(row("Runs as", format!("{}, {kind}", sv.user), Widget::Info));
        rows.push(row("State", state_text(sv), Widget::Info));
        if sv.restarts > 0 {
            let times = if sv.restarts == 1 { "once".into() } else { format!("{} times", sv.restarts) };
            rows.push(row("Started again", times, Widget::Info));
        }
        let auto = if sv.scope == Scope::System { "Start with the system" } else { "Start when its user signs in" };
        rows.push(row(auto, String::new(), Widget::Toggle(sv.autostart, if mine { Act::ServiceAuto } else { Act::Nothing })));
        rows.push(row("Start again if it stops", String::new(), Widget::Toggle(sv.restart, if mine { Act::ServiceAgain } else { Act::Nothing })));
        let running = sv.running() || matches!(sv.state, services::State::Waiting) || sv.retry_at.is_some();
        let actions = vec![
            button(if running { "Stop" } else { "Start" }, mine, Act::ServiceRun(!running)),
            button("Restart", mine && sv.running(), Act::ServiceRestart),
            button("Remove", mine, Act::ServiceRemove),
        ];
        rows.push(row("", String::new(), Widget::Buttons(actions)));
        let log = services::log(picked, 12);
        if !log.is_empty() {
            rows.push(row("Output", String::new(), Widget::Heading));
            rows.push(row("", String::new(), Widget::Log(log)));
        }
    }

    fn user_rows(&self, rows: &mut Vec<Row>) {
        let s = Section::Users;
        let me = crate::access::user().unwrap_or_default();
        let admin = crate::access::is_admin();
        let list = crate::users::list();
        let items = list
            .iter()
            .enumerate()
            .map(|(i, u)| {
                let role = match (u.admin, u.name == me) {
                    (true, true) => "administrator, you",
                    (true, false) => "administrator",
                    (false, true) => "you",
                    (false, false) => "",
                };
                Item { code: String::new(), name: u.name.clone(), note: role.into(), on: i == self.user, act: Act::User(i) }
            })
            .collect();
        rows.push(Row::new(s, "Users", String::new(), Widget::List(items)));
        let target = list.get(self.user).map(|a| a.name.as_str()).unwrap_or("");
        let actions = vec![
            Choice { label: "Add User".into(), on: false, enabled: admin, act: Act::UserAdd },
            Choice { label: "Set Password".into(), on: false, enabled: admin || target == me, act: Act::UserPassword },
            Choice { label: "Remove".into(), on: false, enabled: admin && !target.is_empty() && target != me, act: Act::UserRemove },
        ];
        rows.push(Row::new(s, "", String::new(), Widget::Buttons(actions)));
        let auto = crate::users::auto().is_some_and(|u| u.name == me);
        rows.push(Row::new(s, "Sign me in automatically", String::new(), Widget::Toggle(auto, Act::AutoSignIn)));
    }

    /// What to show on the right: a section or matches from all sections.
    fn shown(&self) -> Vec<Row> {
        if !self.searching() {
            return self.rows(self.section);
        }
        let query = self.search.text.trim().to_lowercase();
        let all = SECTIONS.iter().flat_map(|(_, list)| list.iter()).flat_map(|&s| self.rows(s));
        all.filter(|r| !r.label.is_empty() && !matches!(r.widget, Widget::Heading) && r.words().contains(&query)).collect()
    }

    // ------------------------------------------------------------ layout

    /// Buttons of a wrapping row: positions relative to the row start and the total height.
    fn flow(&self, widths: &[i32]) -> (Vec<(i32, i32)>, i32) {
        let (mut x, mut y) = (0, 0);
        let mut at = Vec::with_capacity(widths.len());
        for &w in widths {
            if x > 0 && x + w > self.width() {
                (x, y) = (0, y + ui::BUTTON_H + 6);
            }
            at.push((x, y));
            x += w + 6;
        }
        (at, y + ui::BUTTON_H)
    }

    fn widths(list: &[Choice], grid: bool) -> Vec<i32> {
        list.iter().map(|c| if grid { MODE_W } else { ui::button_width(&c.label) }).collect()
    }

    /// Row height. In search results also the section name under the label.
    /// Description lines at the content width.
    fn wrap(&self, text: &str) -> Vec<String> {
        let width = self.width();
        let mut lines: Vec<String> = Vec::new();
        let mut line = String::new();
        for word in text.split(' ') {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if !line.is_empty() && font::measure(FontId::Regular, 13.0, &candidate) > width {
                lines.push(core::mem::replace(&mut line, word.to_string()));
            } else {
                line = candidate;
            }
        }
        lines.push(line);
        lines
    }

    fn height(&self, row: &Row) -> i32 {
        let link = if self.searching() { 18 } else { 0 };
        let label = if row.label.is_empty() { 0 } else { LABEL_H + link };
        match row.widget {
            Widget::Heading => return HEADING_H,
            Widget::Info => return INFO_H + link,
            _ => {}
        }
        ROW_GAP
            + match &row.widget {
                Widget::Heading | Widget::Info => 0,
                Widget::Toggle(..) | Widget::Action(_) => LINE_H - ROW_GAP + link,
                Widget::Note => LABEL_H + (self.wrap(&row.label).len() as i32 - 1) * NOTE_LINE_H,
                Widget::Log(lines) => lines.len() as i32 * LOG_LINE_H + 2 * LOG_PAD,
                Widget::Meter(..) => label - 4 + METER_H,
                Widget::Slider(_) => label + SLIDER_H,
                Widget::Swatches(_) => label + SWATCH + 8,
                Widget::Thumbs(..) => label + THUMB_H + 6,
                Widget::Map => MAP_H,
                Widget::List(list) => label + list.len() as i32 * LIST_H,
                Widget::Buttons(list) => label + self.flow(&Self::widths(list, false)).1,
                Widget::Grid(list) => label + self.flow(&Self::widths(list, true)).1,
            }
            + ROW_GAP
    }

    // ------------------------------------------------------------ drawing

    fn draw_sidebar(&mut self, c: &mut Canvas, a: Rect) {
        c.blend(Rect::new(a.x, a.y, SIDE_W, a.h), 0x000000, 60);
        c.fill(Rect::new(a.x + SIDE_W, a.y, 1, a.h), 0x2E3247);
        let field = self.search_box().offset(a.x, a.y);
        c.fill_round(field, 8, 0x14172A, 256);
        c.stroke_round(field, 8, if self.typing { ACCENT } else { 0xFFFFFF }, if self.typing { 200 } else { 30 });
        self.search.draw(c, field, self.typing, self.caret_on, "Search settings", TEXT);
        self.sidebar.clear();
        let view = self.side_view();
        let saved = c.clip();
        c.set_clip(view.offset(a.x, a.y).intersect(saved));
        let mut y = 12 + SEARCH_H + 8 - self.side_scroll;
        for (n, (group, list)) in SECTIONS.into_iter().enumerate() {
            if n > 0 {
                c.blend(Rect::new(a.x + SIDE_PAD + 10, a.y + y, SIDE_W - 2 * (SIDE_PAD + 10), 1), 0xFFFFFF, 26);
                y += GROUP_GAP;
            }
            c.text(a.x + SIDE_PAD + 10, a.y + y + 9, group, TEXT_DIM, FontId::SemiBold, 11.0);
            y += GROUP_H;
            for &section in list {
                let r = Rect::new(SIDE_PAD, y, SIDE_W - 2 * SIDE_PAD, ITEM_H - 2);
                let on = !self.searching() && section == self.section;
                if on {
                    c.fill_round(r.offset(a.x, a.y), 8, ACCENT, 44);
                }
                c.text(a.x + r.x + 10, a.y + r.y + 7, section.name(), if on { ACCENT } else { TEXT }, FontId::Regular, 14.0);
                let seen = r.intersect(view);
                if !seen.is_empty() {
                    self.sidebar.push((seen, section));
                }
                y += ITEM_H;
            }
            y += 4;
        }
        c.set_clip(saved);
        self.side_total = y + self.side_scroll - view.y + 4;
        self.side_scroll_to(self.side_scroll);
        self.side_bar().draw(c, a, self.side_grab.is_some());
    }

    /// A clickable area: only if visible.
    fn hit(&mut self, r: Rect, act: Act) {
        if !r.intersect(self.view()).is_empty() {
            self.hits.push((r.intersect(self.view()), act));
        }
    }

    fn draw_choice(&mut self, c: &mut Canvas, a: Rect, r: Rect, item: &Choice) {
        let at = r.offset(a.x, a.y);
        if item.on {
            ui::accent_button(c, at, &item.label, item.enabled);
        } else {
            ui::button(c, at, &item.label, item.enabled);
        }
        if item.enabled {
            self.hit(r, item.act);
        }
    }

    fn draw_row(&mut self, c: &mut Canvas, a: Rect, row: &Row, top: i32) {
        let (x, w) = (SIDE_W + PAD, self.width());
        if matches!(row.widget, Widget::Heading) {
            c.text(a.x + x, a.y + top + HEADING_H - 20, &row.label.to_uppercase(), TEXT_DIM, FontId::SemiBold, 11.0);
            return;
        }
        let mut y = top + if matches!(row.widget, Widget::Info) { (INFO_H - 26) / 2 } else { ROW_GAP };
        if !row.label.is_empty() && !matches!(row.widget, Widget::Note) {
            c.text(a.x + x, a.y + y + 4, &row.label, TEXT, FontId::Regular, 14.0);
            if !row.value.is_empty() {
                // For a button and a switch the value stands to their left.
                let room = match &row.widget {
                    Widget::Action(item) => ui::button_width(&item.label) + 14,
                    Widget::Toggle(..) => 58,
                    _ => 0,
                };
                let vw = font::measure(FontId::Regular, 13.0, &row.value);
                c.text(a.x + x + w - room - vw, a.y + y + 5, &row.value, TEXT_DIM, FontId::Regular, 13.0);
            }
            if self.searching() {
                let name = row.section.name();
                c.text(a.x + x, a.y + y + 24, name, ACCENT, FontId::Regular, 12.0);
                let link = Rect::new(x, y + 22, font::measure(FontId::Regular, 12.0, name), 16);
                self.hit(link, Act::Open(row.section));
                y += 18;
            }
            if !matches!(row.widget, Widget::Info | Widget::Toggle(..) | Widget::Action(_)) {
                y += LABEL_H;
            }
        }
        match &row.widget {
            Widget::Heading | Widget::Info => {}
            Widget::Meter(level, color) => {
                let bar = Rect::new(x, y - 2, w, METER_BAR).offset(a.x, a.y);
                c.fill_round(bar, METER_BAR / 2, 0xFFFFFF, 40);
                let fill = (bar.w * (*level).min(100) as i32 / 100).max(METER_BAR);
                c.fill_round(Rect::new(bar.x, bar.y, fill, bar.h), METER_BAR / 2, *color, 256);
            }
            Widget::Note => {
                for (i, line) in self.wrap(&row.label).iter().enumerate() {
                    c.text(a.x + x, a.y + y + 4 + i as i32 * NOTE_LINE_H, line, TEXT_DIM, FontId::Regular, 13.0);
                }
            }
            Widget::Log(lines) => {
                let r = Rect::new(x, y, w, lines.len() as i32 * LOG_LINE_H + 2 * LOG_PAD).offset(a.x, a.y);
                c.fill_round(r, 8, 0x000000, 70);
                let saved = c.clip();
                c.set_clip(r.inset(LOG_PAD).intersect(saved));
                for (i, line) in lines.iter().enumerate() {
                    c.text(r.x + LOG_PAD, r.y + LOG_PAD + i as i32 * LOG_LINE_H, line, TEXT_DIM, FontId::Mono, 12.0);
                }
                c.set_clip(saved);
            }
            Widget::Slider(s) => {
                let r = Rect::new(x + 9, y, w - 18, 22);
                let (value, min, max) = s.range(&theme::get());
                ui::slider(c, r.offset(a.x, a.y), value.clamp(min, max), min, max);
                self.hit(r.inset(-8), Act::Slide(*s));
            }
            Widget::Toggle(on, act) => {
                let r = Rect::new(x + w - 44, y, 44, 24);
                ui::switch(c, r.offset(a.x, a.y), *on);
                if *act == Act::Nothing {
                    // It cannot be switched now: dim it.
                    c.blend(r.offset(a.x, a.y), BG, 150);
                }
                self.hit(Rect::new(x, y, w, 26), *act);
            }
            Widget::Action(item) => {
                let bw = ui::button_width(&item.label);
                self.draw_choice(c, a, Rect::new(x + w - bw, y, bw, ui::BUTTON_H), item);
            }
            Widget::Swatches(list) => {
                for (i, &(color, on, act)) in list.iter().enumerate() {
                    let r = Rect::new(x + 4 + i as i32 * SWATCH_STEP, y + 4, SWATCH, SWATCH);
                    ui::swatch(c, r.offset(a.x, a.y), color, on, BG);
                    self.hit(r.inset(-4), act);
                }
            }
            Widget::Thumbs(style, picked) => {
                self.thumbnails(*style);
                let saved = c.clip();
                for i in 0..WALLPAPERS.len() {
                    let r = Rect::new(x + 3 + i as i32 * (THUMB_W + THUMB_GAP), y + 3, THUMB_W, THUMB_H);
                    let at = r.offset(a.x, a.y);
                    if i == *picked {
                        c.fill_round(at.inset(-3), 11, ACCENT, 256);
                        c.fill_round(at.inset(-1), 9, BG, 256);
                    }
                    if let Some((_, list)) = &self.thumbnails {
                        c.blit(at.x, at.y, THUMB_W, &list[i]);
                    }
                    c.round_mask_on(at, 8, BG);
                    c.set_clip(saved);
                    c.stroke_round(at, 8, 0xFFFFFF, 40);
                    self.hit(r, Act::WallpaperColor(i));
                }
            }
            Widget::Map => {
                self.map = Rect::new(x, y, w, MAP_H);
                self.draw_map(c, a);
            }
            Widget::List(list) => {
                for (i, item) in list.iter().enumerate() {
                    let r = Rect::new(x, y + i as i32 * LIST_H, w, LIST_H - 4);
                    let at = r.offset(a.x, a.y);
                    c.fill_round(at, 8, 0xFFFFFF, if item.on { 30 } else { 10 });
                    let mut nx = at.x + 12;
                    if !item.code.is_empty() {
                        c.text(nx, at.y + 8, &item.code, ACCENT, FontId::SemiBold, 13.0);
                        nx += 36;
                    }
                    c.text(nx, at.y + 8, &item.name, TEXT, FontId::Regular, 14.0);
                    let nw = font::measure(FontId::Regular, 12.5, &item.note);
                    c.text(at.x + at.w - 12 - nw, at.y + 9, &item.note, TEXT_DIM, FontId::Regular, 12.5);
                    self.hit(r, item.act);
                }
            }
            Widget::Buttons(list) | Widget::Grid(list) => {
                let grid = matches!(row.widget, Widget::Grid(_));
                let widths = Self::widths(list, grid);
                for ((item, &(dx, dy)), &bw) in list.iter().zip(&self.flow(&widths).0).zip(&widths) {
                    self.draw_choice(c, a, Rect::new(x + dx, y + dy, bw, ui::BUTTON_H), item);
                }
            }
        }
    }

    fn keep_button(&self) -> Rect {
        Rect::new(SIDE_W + PAD, self.ch - ui::BUTTON_H - 10, ui::button_width("Keep"), ui::BUTTON_H)
    }

    fn revert_button(&self) -> Rect {
        let k = self.keep_button();
        Rect::new(k.x + k.w + 8, k.y, ui::button_width("Revert"), ui::BUTTON_H)
    }

    // ------------------------------------------------------------ actions

    fn run(&mut self, act: Act) -> bool {
        match act {
            Act::Open(section) => self.open(section),
            Act::Slide(_) => {}
            Act::TitleColor(i) => {
                let (_, top, bottom) = TITLE_PRESETS[i];
                theme::update(|t| t.title = (top, bottom));
            }
            Act::WallpaperColor(i) => theme::update(|t| t.wallpaper = wallpaper::number(wallpaper::parts(t.wallpaper).0, i)),
            Act::WallpaperStyle(i) => theme::update(|t| t.wallpaper = wallpaper::number(i, wallpaper::parts(t.wallpaper).1)),
            Act::Bars => theme::update(|t| t.bars_everywhere = !t.bars_everywhere),
            Act::Saver(minutes) => theme::update(|t| t.saver_minutes = minutes),
            Act::SaverPreview => crate::saver::request(),
            Act::DisplayOff(minutes) => theme::update(|t| t.display_off_minutes = minutes),
            Act::DisplayOffNow => crate::saver::display_off(),
            Act::Nothing => {}
            Act::Exporter => Self::report(exporter::change(|s| s.enabled = !s.enabled)),
            Act::Remote => Self::report(remote::set_enabled(!remote::settings().enabled)),
            Act::RemoteShowKey => self.remote_key_shown = !self.remote_key_shown,
            Act::RemoteNewKey => match remote::new_key() {
                Ok(_) => self.remote_key_shown = true,
                Err(e) => Self::report(Err(e)),
            },
            Act::ExporterPort => Self::open_form(Form::Port),
            Act::ExporterAccess(a) => Self::report(exporter::change(|s| s.access = a)),
            Act::ExporterAllowed => Self::open_form(Form::Allowed),
            Act::ServiceAdd(scope) => {
                let reply = super::dialog::request(super::dialog::Mode::Open, &crate::access::home(), "");
                self.adding = Some((reply, scope));
            }
            Act::Service(i) => self.service = i,
            Act::ServiceAuto => {
                let on = services::list().get(self.service).is_some_and(|s| !s.autostart);
                Self::report(services::set_autostart(self.service, on));
            }
            Act::ServiceAgain => {
                let on = services::list().get(self.service).is_some_and(|s| !s.restart);
                Self::report(services::set_restart(self.service, on));
            }
            Act::ServiceRun(on) => Self::report(services::run(self.service, on)),
            Act::ServiceRestart => Self::report(services::restart(self.service)),
            Act::ServiceRemove => {
                Self::report(services::remove(self.service));
                self.service = 0;
            }
            Act::Play(sound) => crate::audio::play(sound),
            Act::Output(i) => {
                if let Some(name) = crate::audio::outputs().get(i) {
                    crate::audio::choose_output(name);
                }
            }
            Act::Input(i) => {
                if let Some(name) = crate::audio::inputs().get(i) {
                    crate::audio::choose_input(name);
                    // The test is running from the old input — start again.
                    if self.mic_test.is_some() {
                        self.mic_test = Some(crate::audio::Capture::start());
                    }
                }
            }
            Act::MicBoost(db) => theme::update(|t| t.mic_boost = db),
            Act::MicTest => {
                self.mic_test = if self.mic_test.is_some() { None } else { Some(crate::audio::Capture::start()) };
                self.mic_level = 0;
            }
            Act::Startup => {
                let on = !theme::get().startup_sound;
                theme::update(|t| t.startup_sound = on);
                if on {
                    crate::audio::welcome();
                }
            }
            Act::Reset(section) => self.reset(section),
            Act::Arrange(arrange) => output::change(Layout { arrange, ..output::current() }),
            Act::Primary(primary) => output::change(Layout { primary, ..output::current() }),
            Act::Identify => output::identify(),
            Act::Scale(scale) => self.set_scale(scale),
            Act::Mode(i) if self.own_mode() => {
                let modes = output::monitor_modes();
                output::change(Layout { mode: (i != 0).then_some(modes[i]), ..output::current() });
            }
            Act::Mode(i) => {
                // The current mode means "as the system decides".
                let m = crate::display::modes()[i];
                self.mode = Some(m);
                crate::display::save(Some(m));
            }
            Act::Restart => crate::wm::request(crate::wm::Request::Restart),
            Act::RestartSetup => crate::wm::request(crate::wm::Request::RestartInto(crate::bootvars::Next::Setup)),
            Act::RestartUsb => match crate::bootvars::usb_stick() {
                Ok((hd, _)) => crate::wm::request(crate::wm::Request::RestartInto(crate::bootvars::Next::Usb(hd))),
                Err(e) => crate::wm::notify("Restart from USB", &e),
            },
            Act::Keep => output::keep(),
            Act::Revert => output::revert(),
            Act::Language(i) => self.language = i,
            Act::LanguageAdd => {
                let picker = LanguagePicker::new();
                let h = picker.height();
                crate::wm::request(crate::wm::Request::Popup { title: "Add Language".into(), app: alloc::boxed::Box::new(picker), w: languages::W, h });
            }
            Act::LanguageRemove => {
                keyboard::remove(self.picked_language());
                self.language = 0;
            }
            Act::LanguageDefault => keyboard::set_default(self.picked_language()),
            Act::Switch(i) => keyboard::set_switch(SWITCHES[i].0),
            Act::User(i) => self.user = i,
            Act::UserAdd => Self::open_account(accounts::Action::Add),
            Act::UserPassword => {
                let me = crate::access::user().unwrap_or_default();
                if let Some(target) = crate::users::list().get(self.user) {
                    Self::open_account(accounts::Action::Password { name: target.name.clone(), check_old: target.name == me && target.has_password() });
                }
            }
            Act::UserRemove => {
                if let Some(target) = crate::users::list().get(self.user) {
                    Self::open_account(accounts::Action::Remove(target.name.clone()));
                }
            }
            Act::AutoSignIn => {
                let me = crate::access::user().unwrap_or_default();
                let on = crate::users::auto().is_some_and(|a| a.name == me);
                crate::users::set_auto(if on { None } else { Some(&me) });
            }
            Act::Sleep(deep) => {
                crate::power::set_deep(deep);
            }
            Act::MemorySleep(on) => {
                if let Err(e) = crate::hdmi::deep_watermarks(on) {
                    crate::wm::notify_with(Some(Sound::Error), "Memory sleep", &e);
                }
            }
            #[cfg(feature = "debug-tools")]
            Act::DebugInput => {
                let on = !crate::debug::enabled(crate::debug::INPUT);
                if let Err(e) = crate::debug::set(crate::debug::INPUT, on) {
                    crate::wm::notify_with(Some(Sound::Error), "Debug", &e);
                }
            }
            Act::OpenNetwork => self.open(Section::Network),
            Act::OpenBluetooth => self.open(Section::Bluetooth),
            Act::ZonePick => {
                let app = alloc::boxed::Box::new(ZonePicker::new());
                crate::wm::request(crate::wm::Request::Popup { title: "Time Zone".into(), app, w: datetime::ZONES_W, h: datetime::ZONES_H });
            }
            Act::SetClock => Self::open_form(Form::Clock),
            Act::TimeServer => Self::open_form(Form::Server),
            Act::AutoSummer => {
                // We no longer switch ourselves: the clock stays as it is now.
                let summer = crate::time::summer_now();
                Self::time_change(|s| {
                    if s.auto_summer {
                        s.summer = summer;
                    }
                    s.auto_summer = !s.auto_summer;
                });
            }
            Act::Summer(on) => Self::time_change(|s| (s.auto_summer, s.summer) = (false, on)),
            Act::Ntp => Self::time_change(|s| s.ntp = !s.ntp),
            Act::SyncNow => crate::time::request_sync(),
            Act::HardwareUtc(on) => Self::time_change(|s| s.rtc_utc = on),
            Act::HardwareReport => {
                let report = crate::hwprobe::run();
                crate::wm::notify("Hardware report", report.last().map_or("Done", |s| s.as_str()));
            }
        }
        true
    }

    fn picked_language(&self) -> usize {
        let languages = keyboard::enabled();
        languages.get(self.language).or(languages.first()).copied().unwrap_or(0)
    }

    fn report(result: Result<(), &'static str>) {
        if let Err(e) = result {
            crate::wm::notify_with(Some(Sound::Error), "Settings", e);
        }
    }

    fn open_form(form: Form) {
        let dialog = FormDialog::new(form);
        let (title, h) = (dialog.title().into(), dialog.height());
        crate::wm::request(crate::wm::Request::Popup { title, app: alloc::boxed::Box::new(dialog), w: datetime::FORM_W, h });
    }

    fn time_change(f: impl FnOnce(&mut crate::time::Settings)) {
        if let Err(e) = crate::time::change(f) {
            crate::wm::notify_with(Some(Sound::Error), "Date & Time", e);
        }
    }

    fn open_account(action: accounts::Action) {
        let dialog = AccountDialog::new(action);
        let (title, h) = (dialog.title(), dialog.height());
        crate::wm::request(crate::wm::Request::Popup { title, app: alloc::boxed::Box::new(dialog), w: accounts::W, h });
    }

    // ------------------------------------------------------------ screens

    /// Whose settings are shown: in mirror mode and with a single screen there is no choice.
    fn selected(&self) -> usize {
        if output::count() < 2 {
            return 0;
        }
        match output::current().arrange {
            Arrange::Extend | Arrange::Mirror => self.screen,
            Arrange::Only(i) => i,
        }
    }

    /// We set the mode of the selected screen ourselves: it changes immediately, without a restart.
    fn own_mode(&self) -> bool {
        self.selected() == 1 || output::own_primary()
    }

    /// The current scale of the selected screen.
    fn scale_now(&self) -> i32 {
        let layout = output::current();
        if layout.arrange == Arrange::Mirror && output::count() > 1 {
            return output::percent_of(self.selected());
        }
        layout.scale[self.selected()]
    }

    fn set_scale(&self, scale: i32) {
        let mut layout = output::current();
        if layout.arrange == Arrange::Mirror && output::count() > 1 {
            // There is one desktop: the monitor's scale is converted to the panel's scale.
            let sizes = output::sizes();
            layout.mirror_scale = if self.selected() == 0 { scale } else { (scale * sizes[0].0 + sizes[1].0 / 2) / sizes[1].0 };
        } else {
            layout.scale[self.selected()] = scale;
        }
        output::change(layout);
    }

    /// How many screen pixels per diagram pixel: any layout fits.
    fn map_scale(&self) -> i32 {
        let sizes = output::desk_sizes();
        let (w, h) = sizes.iter().fold((0, 0), |sum, s| (sum.0 + s.0, sum.1 + s.1));
        let a = self.map.inset(MAP_PAD);
        (w / a.w).max(h / a.h) + 1
    }

    /// Screens on the diagram. While one is dragged, the diagram stays put and the other does not move.
    fn map_screens(&self) -> [Rect; 2] {
        let sizes = output::desk_sizes();
        let ((w1, h1), (w2, h2)) = (sizes[0], sizes[1]);
        let (a, scale) = (self.map, self.map_scale());
        let start = self.moving.map_or_else(output::place, |m| m.start);
        let (left, top) = (start.0.min(0), start.1.min(0));
        let (full_w, full_h) = (w1.max(start.0 + w2) - left, h1.max(start.1 + h2) - top);
        let x0 = a.x + (a.w - full_w / scale) / 2 - left.div_euclid(scale);
        let y0 = a.y + (a.h - full_h / scale) / 2 - top.div_euclid(scale);
        let at = |x: i32, y: i32, w: i32, h: i32| Rect::new(x0 + x.div_euclid(scale), y0 + y.div_euclid(scale), w / scale, h / scale);
        let (first, second) = match self.moving {
            Some(m) if m.screen == 0 => ((start.0 - m.place.0, start.1 - m.place.1), start),
            Some(m) => ((0, 0), m.place),
            None => ((0, 0), start),
        };
        [at(first.0, first.1, w1, h1), at(second.0, second.1, w2, h2)]
    }

    /// Pressing a screen on the diagram selects it. Moving is only possible in extend mode.
    fn map_press(&mut self, x: i32, y: i32) -> bool {
        if self.map.is_empty() || !self.view().contains(x, y) {
            return false;
        }
        let Some(screen) = self.map_screens().iter().position(|r| r.contains(x, y)) else { return false };
        self.screen = screen;
        if output::current().arrange == Arrange::Extend {
            let place = output::place();
            self.moving = Some(Moving { screen, from: (x, y), start: place, place });
        }
        true
    }

    fn map_drag(&mut self, x: i32, y: i32) {
        let scale = self.map_scale();
        let Some(m) = &mut self.moving else { return };
        if output::count() < 2 {
            return;
        }
        let (dx, dy) = ((x - m.from.0) * scale, (y - m.from.1) * scale);
        let target = if m.screen == 0 { (m.start.0 - dx, m.start.1 - dy) } else { (m.start.0 + dx, m.start.1 + dy) };
        m.place = output::snap(target, MAP_PULL * scale);
    }

    /// The diagram: screens with numbers and real resolutions.
    fn draw_map(&self, c: &mut Canvas, a: Rect) {
        let (layout, sizes) = (output::current(), output::sizes());
        let area = self.map.offset(a.x, a.y);
        c.fill_round(area, 8, 0x000000, 70);
        let saved = c.clip();
        c.set_clip(area.intersect(saved));
        for (i, r) in self.map_screens().into_iter().enumerate() {
            let r = r.offset(a.x, a.y);
            let lit = match layout.arrange {
                Arrange::Only(n) => n == i,
                _ => true,
            };
            let main = layout.arrange == Arrange::Extend && layout.primary == i;
            let held = self.moving.is_some_and(|m| m.screen == i);
            c.fill_round(r, 6, if main { ACCENT } else { 0x3A3F55 }, if lit { 220 } else { 70 });
            let chosen = output::count() > 1 && i == self.selected();
            c.stroke_round(r, 6, if chosen { 0xFFFFFF } else { ACCENT }, if held || chosen { 200 } else { 60 });
            let color = match (main, lit) {
                (true, _) => 0x10131F,
                (false, true) => TEXT,
                (false, false) => TEXT_DIM,
            };
            let percent = if layout.arrange == Arrange::Mirror { output::percent_of(i) } else { layout.scale[i] };
            let number = if percent == 100 { format!("{}", i + 1) } else { format!("{}  {percent}%", i + 1) };
            let (w, h) = sizes[i];
            let size = format!("{w} \u{00D7} {h}");
            let middle = r.y + r.h / 2;
            let nw = font::measure(FontId::SemiBold, 18.0, &number);
            c.text(r.x + (r.w - nw) / 2, middle - 24, &number, color, FontId::SemiBold, 18.0);
            let sw = font::measure(FontId::Regular, 12.0, &size);
            c.text(r.x + (r.w - sw) / 2, middle + 3, &size, color, FontId::Regular, 12.0);
        }
        c.set_clip(saved);
    }
}

impl App for Settings {
    fn background(&self) -> Color {
        BG
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, ctx: &Ctx) {
        self.draw_sidebar(c, a);
        self.ensure_page();
        if self.page_shown() {
            self.hits.clear();
            let x = a.x + SIDE_W + PAD;
            c.text(x, a.y + 18, self.section.name(), TEXT, FontId::SemiBold, 20.0);
            c.text(x, a.y + 46, self.section.note(), TEXT_DIM, FontId::Regular, 13.0);
            let r = self.page_rect().offset(a.x, a.y);
            if let Some((_, app)) = self.page.as_mut() {
                let saved = c.clip();
                c.set_clip(r.intersect(saved));
                app.draw(c, r, ctx);
                c.set_clip(saved);
            }
            return;
        }
        let rows = self.shown();
        self.hits.clear();
        self.map = Rect::new(0, 0, 0, 0);
        // Dense rows need a gap from the header; the others have it inside the row already.
        let head = HEAD_H + if matches!(rows.first().map(|r| &r.widget), Some(Widget::Info)) { 8 } else { 0 };
        self.total = head + rows.iter().map(|r| self.height(r)).sum::<i32>() + 20;
        self.scroll_to(self.scroll);
        let view = self.view();
        let saved = c.clip();
        c.set_clip(view.offset(a.x, a.y).intersect(saved));
        let (title, note) = if self.searching() {
            let count = rows.len();
            ("Search".to_string(), if count == 0 { "Nothing found. Try another word.".to_string() } else { format!("Found: {count}") })
        } else {
            (self.section.name().to_string(), self.section.note().to_string())
        };
        let x = a.x + SIDE_W + PAD;
        let mut y = 18 - self.scroll;
        c.text(x, a.y + y, &title, TEXT, FontId::SemiBold, 20.0);
        c.text(x, a.y + y + 28, &note, TEXT_DIM, FontId::Regular, 13.0);
        y = head - self.scroll;
        for row in &rows {
            let h = self.height(row);
            if y + h > 0 && y < view.h {
                self.draw_row(c, a, row, y);
                if !row.label.is_empty() && !matches!(row.widget, Widget::Heading) {
                    c.fill(Rect::new(x, a.y + y + h - 1, self.width(), 1), 0x2E3247);
                }
            }
            y += h;
        }
        c.set_clip(saved);
        self.scrollbar().draw(c, a, self.thumb_grab.is_some());
        if let Some(left) = output::seconds_to_revert() {
            let (keep, revert) = (self.keep_button(), self.revert_button());
            ui::accent_button(c, keep.offset(a.x, a.y), "Keep", true);
            ui::button(c, revert.offset(a.x, a.y), "Revert", true);
            let text = format!("Reverting in {left} s");
            c.text(a.x + revert.x + revert.w + 12, a.y + revert.y + 7, &text, TEXT_DIM, FontId::Regular, 13.0);
            self.hits.push((keep, Act::Keep));
            self.hits.push((revert, Act::Revert));
        }
    }

    fn resize(&mut self, cw: i32, ch: i32) {
        (self.cw, self.ch) = (cw, ch);
        let r = self.page_rect();
        if let Some((_, app)) = self.page.as_mut() {
            app.resize(r.w, r.h);
        }
    }

    fn open_arg(&mut self, arg: &str) -> bool {
        let (Some(section), sub) = parse_page(arg) else { return false };
        // A subsection (the Wi-Fi tab) — create the page anew with it.
        if sub.is_some() {
            self.page = None;
            self.page_arg = sub;
        }
        self.open(section);
        self.dirty = true;
        true
    }

    fn scroll_at(&mut self, x: i32, y: i32, lines: i32) -> bool {
        if self.page_shown() && x > SIDE_W {
            let r = self.page_rect();
            return self.page.as_mut().is_some_and(|(_, app)| app.scroll_at(x - r.x, y - r.y, lines));
        }
        if x < SIDE_W {
            let before = self.side_scroll;
            self.side_scroll_to(self.side_scroll + lines * WHEEL / 2);
            return self.side_scroll != before;
        }
        self.scroll(lines)
    }

    fn scroll(&mut self, lines: i32) -> bool {
        let before = self.scroll;
        self.scroll_to(self.scroll + lines * WHEEL);
        self.scroll != before
    }

    fn key(&mut self, ch: u8, ctx: &Ctx) -> bool {
        use crate::input::{KEY_PAGE_DOWN, KEY_PAGE_UP};
        // Keys go to the window page; whatever it does not take is handled as usual (a letter starts a search).
        if self.page_shown() && !self.typing && self.page.as_mut().is_some_and(|(_, app)| app.key(ch, ctx)) {
            return true;
        }
        match ch {
            KEY_PAGE_UP => return self.scroll(-self.view().h / WHEEL),
            KEY_PAGE_DOWN => return self.scroll(self.view().h / WHEEL),
            0x1B if self.searching() || self.typing => {
                self.search.set_text("");
                self.typing = false;
                return true;
            }
            _ => {}
        }
        // Search starts with the very first letter, no need to click the field.
        if !self.typing && !(0x20..0x7F).contains(&ch) {
            return false;
        }
        self.typing = true;
        self.scroll = 0;
        self.search.key(ch, |c| !c.is_control())
    }

    fn text(&mut self, ch: char, ctx: &Ctx) -> bool {
        if self.page_shown() && !self.typing && self.page.as_mut().is_some_and(|(_, app)| app.text(ch, ctx)) {
            return true;
        }
        self.typing = true;
        self.scroll = 0;
        self.search.type_char(ch, |c| !c.is_control())
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        // To the right of the section list is the window page: events go to it, in its coordinates.
        if self.page_shown() && x > SIDE_W {
            let r = self.page_rect();
            if let Some((_, app)) = self.page.as_mut() {
                self.typing = false;
                return app.mouse(x - r.x, y - r.y, buttons, pressed);
            }
        }
        let selecting = self.search.mouse(self.search_box(), x, y, buttons, pressed);
        if buttons & LEFT == 0 {
            self.dragging = None;
            let grabbed = self.thumb_grab.take().is_some() | self.side_grab.take().is_some();
            let moved = self.moving.take().is_some_and(|m| {
                if m.place != m.start {
                    output::change(Layout { place: Some(m.place), ..output::current() });
                }
                true
            });
            return selecting || grabbed || moved;
        }
        if !pressed {
            if let Some(grab) = self.thumb_grab {
                self.scroll_to(self.scrollbar().top_at(y - grab) as i32);
                return true;
            }
            if let Some(grab) = self.side_grab {
                self.side_scroll_to(self.side_bar().top_at(y - grab) as i32);
                return true;
            }
            if self.moving.is_some() {
                self.map_drag(x, y);
                return true;
            }
            if let Some((s, r)) = self.dragging {
                s.set(r, x);
            }
            return selecting;
        }
        if self.search_box().contains(x, y) {
            self.typing = true;
            return true;
        }
        let was_typing = core::mem::take(&mut self.typing);
        if let Some(grab) = self.side_bar().grab(x, y) {
            self.side_grab = Some(grab);
            self.side_scroll_to(self.side_bar().top_at(y - grab) as i32);
            return true;
        }
        if let Some(&(_, section)) = self.sidebar.iter().find(|(r, _)| r.contains(x, y)) {
            self.open(section);
            return true;
        }
        if let Some(grab) = self.scrollbar().grab(x, y) {
            self.thumb_grab = Some(grab);
            self.scroll_to(self.scrollbar().top_at(y - grab) as i32);
            return true;
        }
        if self.map_press(x, y) {
            return true;
        }
        match self.hits.iter().find(|(r, _)| r.contains(x, y)).copied() {
            Some((r, Act::Slide(s))) => {
                let track = Rect::new(r.x + 8, r.y, r.w - 16, r.h);
                self.dragging = Some((s, track));
                s.set(track, x);
                false
            }
            Some((_, act)) => self.run(act),
            None => was_typing,
        }
    }

    fn tick(&mut self, ctx: &Ctx) -> bool {
        let caret = self.typing && ctx.caret_on();
        let blink = caret != self.caret_on;
        self.caret_on = caret;
        // User windows change the list while it is open; readings refresh by themselves.
        let files = crate::fs::version() != self.fs_seen;
        let languages = keyboard::version() != self.keyboard_seen;
        let stats = crate::stats::version() != self.stats_seen;
        self.fs_seen = crate::fs::version();
        self.keyboard_seen = keyboard::version();
        self.stats_seen = crate::stats::version();
        // Countdown of the new screen layout.
        let left = output::seconds_to_revert();
        let countdown = left != self.revert_left;
        self.revert_left = left;
        let live = matches!(self.section, Section::Power | Section::Network | Section::Bluetooth | Section::Storage);
        let clock = (ctx.time.sec, crate::time::version());
        let ticked = clock != self.time_seen && (matches!(self.section, Section::DateTime | Section::Services | Section::Monitoring | Section::Remote) || self.searching());
        // A program was chosen for a new service.
        if let Some(path) = self.adding.as_ref().and_then(|(reply, _)| super::dialog::take(reply)) {
            let scope = self.adding.take().map_or(Scope::User, |(_, s)| s);
            if let Some(path) = path {
                Self::report(services::add(&path, scope));
                self.service = services::list().len().saturating_sub(1);
            }
        }
        let watched = (services::version(), exporter::version(), remote::version());
        let others = watched != (self.services_seen, self.exporter_seen, self.remote_seen);
        (self.services_seen, self.exporter_seen, self.remote_seen) = watched;
        self.time_seen = clock;
        let page = self.page_shown() && self.page.as_mut().is_some_and(|(_, app)| app.tick(ctx));
        // Sound: the microphone test closes together with the section; level and device list.
        if self.section != Section::Sound {
            self.mic_test = None;
        }
        let mut sound = false;
        if let Some(cap) = &mut self.mic_test {
            let mut fresh = Vec::new();
            cap.take(&mut fresh);
            let peak = fresh.iter().map(|s| (*s as i32).unsigned_abs()).max().unwrap_or(0).max(1) as f32;
            // −60 dB … 0 dB → 0 … 100; falls smoothly.
            let level = (((20.0 * libm::log10f(peak / 32768.0)) + 60.0) * 100.0 / 60.0).clamp(0.0, 100.0) as u32;
            let shown = if fresh.is_empty() { self.mic_level } else { level.max(self.mic_level.saturating_sub(6)) };
            sound |= shown != self.mic_level;
            self.mic_level = shown;
        }
        if self.section == Section::Sound {
            let devices = (crate::audio::outputs(), crate::audio::inputs(), crate::audio::current_output(), crate::audio::current_input());
            sound |= devices != self.devices_seen;
            self.devices_seen = devices;
        }
        let dirty = core::mem::take(&mut self.dirty);
        blink || countdown || files || languages || stats && live || ticked || others || page || dirty || sound
    }
}

/// «5 s», «3 min», «2 h», «4 days».
fn duration_text(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds} s"),
        60..3600 => format!("{} min", seconds / 60),
        3600..86400 => format!("{} h", seconds / 3600),
        _ => format!("{} days", seconds / 86400),
    }
}

/// What is up with a service, briefly.
fn state_text(s: &services::Service) -> String {
    use services::State;
    let now = crate::interrupts::ticks();
    let retry = s.retry_at.map(|at| format!(", again in {}", duration_text(at.saturating_sub(now) / crate::interrupts::HZ + 1)));
    match &s.state {
        State::Running => format!("running for {}", duration_text((now - s.since) / crate::interrupts::HZ)),
        State::Stopped => "stopped".into(),
        State::Waiting => format!("waits for {} to sign in", s.user),
        State::Exited(code) => format!("stopped with code {code}{}", retry.unwrap_or_default()),
        State::Failed(e) => format!("{e}{}", retry.unwrap_or_default()),
    }
}
