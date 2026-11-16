//! The window manager and compositor.
//!
//! Each window has its own surface (an ARGB buffer): it is redrawn only when
//! the window contents change. The screen scene is composed from the wallpaper, shadows, the blurred
//! background under the "glass" parts and the window surfaces — and only in the changed
//! area (damage), not as a whole.

use crate::apps::ui::{self as widgets, Icon};
use crate::audio::Sound;
use crate::apps::{About, BtSettings, Clock, FileDialog, Files, NetSettings, Notepad, Paint, Recorder, Settings, TaskManager, Terminal, Viewer};
use crate::font::{self, FontId};
use crate::fs;
use crate::gfx::{Canvas, Color, Rect, SCALE_100, ShadowMask, mix, premul};
use crate::input::{KEY_MOD_UP, KEY_SWITCH, LEFT, RIGHT};
use crate::interrupts::{self, HZ};
use crate::kprintln;
use crate::lua::{self, LuaWindow};
use crate::rtc::Time;
use crate::stats::{self, Task};
use crate::theme;
use crate::wallpaper;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use core::cell::{Cell, RefCell};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

mod calendar;
mod shot;
mod btmenu;
mod launcher;
mod tray;
pub use tray::net_rows as network_rows;
use crate::sync::IrqLock as Spinlock;

pub const OS_NAME: &str = "GlassOS";

pub const ACCENT: Color = 0x7DD3FC;
pub const TEXT: Color = 0xE6E8F0;
pub const TEXT_DIM: Color = 0x8A90A8;

const TITLE_H: i32 = 36;
const RADIUS: i32 = 10;
const TOPBAR_H: i32 = 30;
/// The fill density of popup panels: as in the Dock, so the glass shows through them.
/// The screen number badge.
const IDENTIFY_SIZE: i32 = 220;
const IDENTIFY_FONT: f32 = 120.0;
const DOCK_H: i32 = 54;
const DOCK_MARGIN: i32 = 12;
/// The margin around a window for its shadow.
const SHADOW_PAD: i32 = 40;
const BLUR: i32 = 8;
/// The width of the zone at the window edge by which it can be dragged.
const GRIP: i32 = 5;
const MIN_W: i32 = 260;
const MIN_H: i32 = 150;

// Window edges for resizing.
const EDGE_LEFT: u8 = 1;
const EDGE_RIGHT: u8 = 2;
const EDGE_BOTTOM: u8 = 4;

/// The mouse cursor shape.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    Arrow,
    ResizeH,
    ResizeV,
    /// The "↖↘" diagonal.
    ResizeNwse,
    /// The "↗↙" diagonal.
    ResizeNesw,
}

fn edge_cursor(edges: u8) -> Cursor {
    match edges {
        EDGE_LEFT | EDGE_RIGHT => Cursor::ResizeH,
        EDGE_BOTTOM => Cursor::ResizeV,
        e if e == EDGE_RIGHT | EDGE_BOTTOM => Cursor::ResizeNwse,
        e if e == EDGE_LEFT | EDGE_BOTTOM => Cursor::ResizeNesw,
        _ => Cursor::Arrow,
    }
}

/// Open windows (bottom to top) — restored at the next login.
const SESSION_FILE: &str = "session.cfg";
/// Program glass: lines `program=opacity,frost` (`-` — the default).
const GLASS_FILE: &str = "glass.cfg";
/// The lowest window background opacity that can be set.
const MIN_ALPHA: u32 = 40;
/// Appearance settings from Settings.
pub const SETTINGS_FILE: &str = "settings.cfg";

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// The system state applications see.
pub struct Ctx {
    pub time: Time,
    pub ticks: u64,
    /// The window that was given the context has focus (the others do not blink their cursor).
    pub focused: bool,
}

impl Ctx {
    /// Whether the blinking caret is visible now: only in the focused window.
    pub fn caret_on(&self) -> bool {
        self.focused && (self.ticks / (crate::interrupts::HZ / 2)) % 2 == 0
    }

    pub fn with_focus(&self, focused: bool) -> Ctx {
        Ctx { focused, ..*self }
    }
}

pub trait App {
    fn background(&self) -> Color;
    fn draw(&mut self, c: &mut Canvas, area: Rect, ctx: &Ctx);
    fn key(&mut self, _ch: u8, _ctx: &Ctx) -> bool {
        false
    }
    /// A non-ASCII character was typed (the rest come through `key`). true — redraw.
    fn text(&mut self, _ch: char, _ctx: &Ctx) -> bool {
        false
    }
    /// Coordinates are local (relative to the client area). `pressed` — the moment of the press.
    fn mouse(&mut self, _x: i32, _y: i32, _buttons: u8, _pressed: bool) -> bool {
        false
    }
    /// The cursor moves over the focused window with no buttons held (for highlighting
    /// menu items and the like). true — redraw.
    fn hover(&mut self, _x: i32, _y: i32) -> bool {
        false
    }
    /// A right click in the client area (the context menu). true — redraw.
    fn context_click(&mut self, _x: i32, _y: i32) -> bool {
        false
    }
    /// The window is given files (its program's file was opened, or files were dropped on it). true — taken.
    fn open_files(&mut self, _paths: &[String]) -> bool {
        false
    }
    /// Whether the window will take files dropped on it.
    fn takes_files(&self) -> bool {
        false
    }
    /// Which part of the client area changed since the last draw (None — all of it).
    fn take_damage(&mut self) -> Option<Rect> {
        None
    }
    /// Below which client area size the window must not shrink (None — the common limit).
    fn min_size(&self) -> Option<(i32, i32)> {
        None
    }
    /// Returns true if the window needs to be redrawn.
    fn tick(&mut self, _ctx: &Ctx) -> bool {
        false
    }
    /// What is open in the window (a directory, a file) — to restore it after a reboot.
    fn location(&self) -> Option<String> {
        None
    }
    /// Wheel scrolling by `lines` lines (> 0 — down). true — a redraw is needed.
    fn scroll(&mut self, _lines: i32) -> bool {
        false
    }
    /// Wheel scrolling over a point (local coordinates) — for windows with
    /// several scrollable areas.
    fn scroll_at(&mut self, _x: i32, _y: i32, lines: i32) -> bool {
        self.scroll(lines)
    }
    /// A custom cursor shape over a client area point (for example, arrows over
    /// a draggable divider). None — the normal arrow.
    fn cursor(&self, _x: i32, _y: i32) -> Option<Cursor> {
        None
    }
    /// The client area changed size (the window was maximized or restored).
    fn resize(&mut self, _cw: i32, _ch: i32) {}
    /// The window moved to a screen with a different scale.
    fn scaled(&mut self, _scale: i32) {}
    /// The application's own memory in bytes (for the task manager).
    fn memory(&self) -> usize {
        0
    }
    /// The application finished — the WM closes the window (for example, a dialog answered).
    /// The window is asked to show `arg` (for example, a Settings section) without opening a second
    /// one of the same kind. true — shown.
    fn open_arg(&mut self, _arg: &str) -> bool {
        false
    }
    fn finished(&self) -> bool {
        false
    }
    /// There are unsaved changes.
    fn unsaved(&self) -> bool {
        false
    }
    /// The window is about to be closed. false — the application asks the user first,
    /// and then closes itself through `finished`.
    fn ask_close(&mut self) -> bool {
        true
    }
    /// The unsaved-changes question is still on screen.
    fn asking(&self) -> bool {
        false
    }
    /// The window title, if the application wants to change it (Some — the new one).
    fn title(&self) -> Option<String> {
        None
    }
}

/// Requests from applications to the window manager (for example, open a file in the editor).
pub enum Request {
    OpenFile(String),
    OpenFolder(String),
    /// Files were dragged out of Files and released outside its window: give them to the window under the cursor.
    DropFiles(Vec<String>),
    /// Show the output of a program without a window in a new terminal.
    Output(Vec<String>),
    /// Run a program (*.lua).
    Run(String),
    /// Open the window of an already running program.
    Window(LuaWindow),
    /// Close the window with this id (End task in the task manager).
    Close(u64),
    /// Open the "Open" / "Save as" dialog.
    Dialog(FileDialog),
    /// A small service window in the centre of the screen (copy progress and the like).
    Popup { title: String, app: Box<dyn App>, w: i32, h: i32 },
    /// A screenshot (area selection).
    Screenshot,
    /// Reboot or shut down, after dealing with unsaved changes first.
    Restart,
    /// Reboot so that the firmware starts with its own setup or with a USB stick.
    RestartInto(crate::bootvars::Next),
    Shutdown,
    /// Open a built-in window by its kind.
    Launch(&'static str),
    /// Open a built-in window with an argument (for example, netcfg on the "wifi" tab).
    LaunchArg(&'static str, String),
}

/// What the desktop asks of the main loop.
#[derive(Clone, Copy)]
pub enum Leave {
    Lock,
    LogOut,
}

// The queue lives in a static, and a LuaWindow holds a pointer to a lua_State. The queue
// is only used from the main (desktop) thread.
unsafe impl Send for Request {}

static REQUESTS: Spinlock<Vec<Request>> = Spinlock::new(Vec::new());

/// Rebuild the whole desktop from scratch (glass caches too): for example, after the blur method changed.
static REPAINT_ALL: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn repaint_all() {
    REPAINT_ALL.store(true, core::sync::atomic::Ordering::Relaxed);
}

pub fn request(r: Request) {
    REQUESTS.lock().push(r);
}

/// Frame composition timing by part (cycles): background, shadows, glass, windows, panels; how many
/// times the glass was blurred again.
pub static COMPOSE_COST: [core::sync::atomic::AtomicU64; 6] = [const { core::sync::atomic::AtomicU64::new(0) }; 6];

fn cost(i: usize, since: u64) -> u64 {
    let now = crate::arch::rdtsc();
    COMPOSE_COST[i].fetch_add(now - since, core::sync::atomic::Ordering::Relaxed);
    now
}

/// Files are dragged out of Files past its window edge: what to write at the cursor (None — not dragging).
static DRAGGING: Spinlock<Option<String>> = Spinlock::new(None);

pub fn drag_label(label: Option<String>) {
    *DRAGGING.lock() = label;
}

/// Traces of a session that must not be left to the next user.
pub fn forget_session() {
    REQUESTS.lock().clear();
    NOTIFICATIONS.lock().clear();
    CLIPBOARD.lock().clear();
    *CLIPBOARD_IMAGE.lock() = None;
}

static NOTIFICATIONS: Spinlock<Vec<(Option<Sound>, String, String)>> = Spinlock::new(Vec::new());

/// A popup notification in the top right corner.
pub fn notify(title: &str, body: &str) {
    notify_with(Some(Sound::Notification), title, body);
}

/// The same with its own sound; None — silent (it was already played).
pub fn notify_with(sound: Option<Sound>, title: &str, body: &str) {
    NOTIFICATIONS.lock().push((sound, title.into(), body.into()));
}

struct Toast {
    title: String,
    body: String,
    until: u64,
}

/// Text lines of a popup notification.
fn toast_lines(body: &str) -> Vec<String> {
    font::wrap(FontId::Regular, 13.0, body, TOAST_W - 26 - 14, TOAST_LINES)
}

fn toast_height(t: &Toast) -> i32 {
    (34 + toast_lines(&t.body).len() as i32 * BODY_LINE + 13).max(TOAST_H)
}

/// A notification in the history (the notification centre at the clock).
struct Note {
    title: String,
    body: String,
    /// When it arrived: hours and minutes.
    at: (u8, u8),
}

const SETTINGS_SIZE: (i32, i32) = (820, 640);

/// How many notifications the history keeps.
const MAX_NOTES: usize = 50;
const CENTER_W: i32 = 360;
const NOTE_H: i32 = 80;
/// Text lines of an entry in the notification centre and of a popup notification.
const NOTE_LINES: usize = 2;
const TOAST_LINES: usize = 4;
const BODY_LINE: i32 = 17;
const CENTER_HEAD: i32 = 52;

const TOAST_W: i32 = 320;
const TOAST_H: i32 = 64;
const MAX_TOASTS: usize = 4;

struct DesktopIcon {
    name: String,
    path: String,
    icon: Icon,
    rect: Rect,
}

const CELL_W: i32 = 96;
const CELL_H: i32 = 90;

/// The shared clipboard (text).
static CLIPBOARD: Spinlock<String> = Spinlock::new(String::new());

pub fn clipboard() -> String {
    CLIPBOARD.lock().clone()
}

pub fn set_clipboard(text: &str) {
    *CLIPBOARD.lock() = text.into();
}

/// A picture in the clipboard: (width, height, 0xRRGGBB pixels).
static CLIPBOARD_IMAGE: Spinlock<Option<(i32, i32, Vec<u32>)>> = Spinlock::new(None);

pub fn set_clipboard_image(w: i32, h: i32, px: Vec<u32>) {
    *CLIPBOARD_IMAGE.lock() = Some((w, h, px));
}

pub fn clipboard_image() -> Option<(i32, i32, Vec<u32>)> {
    CLIPBOARD_IMAGE.lock().clone()
}

#[derive(Clone, Copy)]
enum AppKind {
    Terminal,
    Files,
    Paint,
    Clock,
    Recorder,
    Settings,
    About,
    Tasks,
}

impl AppKind {
    /// The window kind's name in the session file.
    fn name(self) -> &'static str {
        match self {
            AppKind::Terminal => "terminal",
            AppKind::Files => "files",
            AppKind::Paint => "paint",
            AppKind::Clock => "clock",
            AppKind::Recorder => "recorder",
            AppKind::Settings => "settings",
            AppKind::About => "about",
            AppKind::Tasks => "taskmgr",
        }
    }
}

/// The music player (a Lua program) and what it opens.
const PLAYER: &str = "/apps/player.lua";

fn is_audio(path: &str) -> bool {
    crate::audio::is_audio_file(path)
}

const LAUNCHERS: [(AppKind, &str); 7] = [
    (AppKind::Terminal, "Terminal"),
    (AppKind::Files, "Files"),
    (AppKind::Paint, "Paint"),
    (AppKind::Clock, "Clock"),
    (AppKind::Recorder, "Recorder"),
    (AppKind::Settings, "Settings"),
    (AppKind::Tasks, "Tasks"),
];

#[derive(Clone, Copy)]
enum MenuAction {
    Screenshot,
    About,
    Install,
    Lock,
    LogOut,
    Restart,
    Shutdown,
}

const MENU_ITEMS: [(MenuAction, &str); 7] = [
    (MenuAction::Install, "Install GlassOS\u{2026}"),
    (MenuAction::Screenshot, "Screenshot"),
    (MenuAction::About, "About GlassOS"),
    (MenuAction::Lock, "Lock"),
    (MenuAction::LogOut, "Log Out"),
    (MenuAction::Restart, "Restart"),
    (MenuAction::Shutdown, "Shut Down"),
];
const MENU_ITEM_H: i32 = 32;

/// Menu items. "Install GlassOS" only in the live system (from an ISO, without a disk):
/// on an installed one it is pointless.
fn menu_items() -> Vec<(MenuAction, &'static str)> {
    MENU_ITEMS.iter().copied().filter(|(a, _)| !matches!(a, MenuAction::Install) || !fs::has_disk()).collect()
}

/// The client area of a w x h window: below the title, with a bottom margin for the rounding.
fn client_size(w: i32, h: i32) -> (i32, i32) {
    (w - 2, h - TITLE_H - RADIUS)
}

struct Window {
    /// The window's permanent id.
    id: u64,
    /// CPU cycles spent on the window since the last statistics publication.
    cycles: u64,
    title: String,
    /// The window kind for the session: "terminal", "files", "notepad", "lua"...
    kind: &'static str,
    /// The Lua program's script (None for other windows).
    script: Option<String>,
    rect: Rect,
    app: Box<dyn App>,
    surface: Canvas,
    /// Shadows for the window without and with focus.
    shadows: [ShadowMask; 2],
    focused: bool,
    dirty: bool,
    /// Redraw the whole surface (title, focus, size…), not only what
    /// the application changed.
    whole: bool,
    /// The application reported a content change: after the redraw we compare
    /// the picture with the previous one (by `hash`) — if it did not change, the screen is left alone.
    content_changed: bool,
    hash: u64,
    /// The cache of the blurred background under the glass: (rectangle on screen, pixels).
    /// Reset when the window moves or the window order changes.
    backdrop: Option<(Rect, Vec<u32>)>,
    /// The part of the cache that became stale because of changes under the window: when drawing,
    /// only it is blurred again (on screen; empty — the whole cache is valid).
    stale: Rect,
    /// Position and size before maximizing (Some — the window is maximized).
    restore: Option<Rect>,
    /// A second view of the window: in mirror mode screens with different scales show it.
    twin: Option<Twin>,
    /// This window's screen is not working now: its number and the window's place on it. When the screen
    /// comes back, so does the window.
    away: Option<(usize, Rect)>,
    /// The size is being changed with the mouse: no shadow is drawn, its mask is still for the old size.
    resizing: bool,
    /// Minimized to the dock: not drawn and gets no input. Minimized windows are at the bottom of the list.
    minimized: bool,
    /// The window's own glass (None — as set by the application and the theme): background
    /// opacity 40..=255 and frost — the blur radius of what is under the window.
    alpha: Option<u32>,
    frost: Option<u32>,
    /// Where the window drew buttons (in its own coordinates) — for highlighting and pressing.
    buttons: Vec<Rect>,
    /// Which title bar button the cursor is over.
    title_hover: Option<TitleButton>,
    /// Which title bar button is held pressed.
    title_held: Option<TitleButton>,
}

/// A button in the window title bar.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TitleButton {
    Close,
    Maximize,
    Minimize,
}

impl TitleButton {
    const ALL: [Self; 3] = [Self::Close, Self::Maximize, Self::Minimize];

    fn color(self) -> Color {
        match self {
            Self::Close => 0xFF5F57,
            Self::Maximize => 0x28C840,
            Self::Minimize => 0xFEBC2E,
        }
    }

    /// The icon on a highlighted button: what will happen when it is pressed.
    fn draw_mark(self, s: &mut Canvas, cx: i32, cy: i32) {
        const INK: Color = 0x3A1A12;
        let bar = |s: &mut Canvas, r: Rect| s.blend(r, INK, 220);
        match self {
            Self::Close => {
                for d in -3..=3 {
                    bar(s, Rect::new(cx + d, cy + d, 1, 1));
                    bar(s, Rect::new(cx + d, cy - d, 1, 1));
                    if d < 3 {
                        s.blend(Rect::new(cx + d + 1, cy + d, 1, 1), INK, 90);
                        s.blend(Rect::new(cx + d + 1, cy - d, 1, 1), INK, 90);
                    }
                }
            }
            Self::Maximize => {
                bar(s, Rect::new(cx - 4, cy, 9, 1));
                bar(s, Rect::new(cx, cy - 4, 1, 9));
            }
            Self::Minimize => bar(s, Rect::new(cx - 4, cy, 9, 1)),
        }
    }
}

/// How much heap to leave to the kernel when opening and stretching windows.
const WINDOW_RESERVE: usize = 32 << 20;

/// Whether there is enough memory for a w×h window: the surface, the blurred background under it, shadows.
fn window_fits(w: i32, h: i32) -> bool {
    crate::heap_stats().1 >= 3 * (w.max(0) as usize * h.max(0) as usize * 4) + WINDOW_RESERVE
}

/// A window at the second screen's scale.
struct Twin {
    surface: Canvas,
    shadows: [ShadowMask; 2],
    backdrop: Option<(Rect, Vec<u32>)>,
}

/// Window shadows: the masks are computed in the pixels of its surface.
fn shadows_for(surface: &Canvas) -> [ShadowMask; 2] {
    let (w, h, size) = (surface.pw, surface.ph, |v| surface.size(v));
    [ShadowMask::new(w, h, size(RADIUS), size(18), size(4), 110), ShadowMask::new(w, h, size(RADIUS), size(30), size(10), 180)]
}

impl Window {
    fn new(title: String, kind: &'static str, script: Option<String>, rect: Rect, app: Box<dyn App>) -> Self {
        let surface = Canvas::new(rect.w, rect.h);
        let mut window = Self {
            id: 0,
            cycles: 0,
            title,
            kind,
            script,
            rect,
            app,
            shadows: shadows_for(&surface),
            surface,
            focused: false,
            dirty: true,
            whole: true,
            content_changed: false,
            hash: 0,
            backdrop: None,
            stale: Rect::new(0, 0, 0, 0),
            restore: None,
            twin: None,
            away: None,
            resizing: false,
            minimized: false,
            alpha: None,
            frost: None,
            buttons: Vec::new(),
            title_hover: None,
            title_held: None,
        };
        window.fit_screen();
        window
    }

    fn forget_backdrop(&mut self) {
        self.backdrop.take();
        if let Some(twin) = &mut self.twin {
            twin.backdrop.take();
        }
    }

    fn remake_shadows(&mut self) {
        self.shadows = shadows_for(&self.surface);
        if let Some(twin) = &mut self.twin {
            twin.shadows = shadows_for(&twin.surface);
        }
    }

    /// The surface, shadows and background cache for a canvas with this scale. If there is no such scale
    /// (the window is on the border of screens), the main ones are returned.
    fn layer(&mut self, scale: i32) -> (&Canvas, &[ShadowMask; 2], &mut Option<(Rect, Vec<u32>)>) {
        match &mut self.twin {
            Some(twin) if twin.surface.scale == scale => (&twin.surface, &twin.shadows, &mut twin.backdrop),
            _ => (&self.surface, &self.shadows, &mut self.backdrop),
        }
    }

    /// Moves the window and, if the size differs, recreates the surface and shadows.
    fn set_rect(&mut self, r: Rect) {
        let resized = (r.w, r.h) != (self.rect.w, self.rect.h);
        self.set_rect_live(r);
        if resized && (self.rect.w, self.rect.h) == (r.w, r.h) {
            self.remake_shadows();
        }
    }

    /// The window moved to a screen with a different scale: redraw it in that screen's pixels.
    /// true — the surface is new.
    fn fit_screen(&mut self) -> bool {
        let r = self.rect;
        let scales = crate::output::scales_at(r.x + r.w / 2, r.y + TITLE_H / 2);
        let (main, second) = (scales[0], scales.get(1).copied());
        let fits = |scale: i32| window_fits(r.w * scale / SCALE_100, r.h * scale / SCALE_100);
        if (main, second) == (self.surface.scale, self.twin.as_ref().map(|t| t.surface.scale)) || !fits(main) {
            return false;
        }
        self.surface = Canvas::scaled(r.w, r.h, main);
        self.shadows = shadows_for(&self.surface);
        self.backdrop = None;
        self.twin = second.filter(|&s| fits(s)).map(|s| {
            let surface = Canvas::scaled(r.w, r.h, s);
            Twin { shadows: shadows_for(&surface), surface, backdrop: None }
        });
        self.app.scaled(main);
        self.dirty = true;
        self.whole = true;
        true
    }

    /// The same without recomputing shadows — while the size is changed with the mouse (shadows take long to compute).
    fn set_rect_live(&mut self, r: Rect) {
        let resized = (r.w, r.h) != (self.rect.w, self.rect.h);
        // No more memory for the surface — the window keeps its previous size.
        if resized && !window_fits(r.w, r.h) {
            return;
        }
        self.rect = r;
        self.forget_backdrop();
        if resized {
            self.surface = Canvas::scaled(r.w, r.h, self.surface.scale);
            if let Some(twin) = &mut self.twin {
                twin.surface = Canvas::scaled(r.w, r.h, twin.surface.scale);
            }
            let (cw, ch) = client_size(r.w, r.h);
            self.app.resize(cw, ch);
            self.dirty = true;
            self.whole = true;
        }
    }

    /// The window edges under a point (for resizing); 0 — not at an edge.
    fn edges_at(&self, x: i32, y: i32) -> u8 {
        let r = self.rect;
        if self.restore.is_some() || !r.inset(-GRIP).contains(x, y) || y < r.y + TITLE_H {
            return 0;
        }
        let mut e = 0;
        if x < r.x + GRIP {
            e |= EDGE_LEFT;
        }
        if x >= r.x + r.w - GRIP {
            e |= EDGE_RIGHT;
        }
        if y >= r.y + r.h - GRIP {
            e |= EDGE_BOTTOM;
        }
        e
    }

    /// The window's memory: buffer, background cache, shadows and application data.
    fn memory(&self) -> usize {
        let shadows: usize = self.shadows.iter().map(|s| s.bytes()).sum();
        (self.surface.px.len() + self.backdrop.as_ref().map_or(0, |(_, px)| px.len())) * 4 + shadows + self.app.memory()
    }

    fn title_center(&self, button: TitleButton) -> (i32, i32) {
        match button {
            TitleButton::Close => self.close_center(),
            TitleButton::Maximize => self.max_center(),
            TitleButton::Minimize => self.min_center(),
        }
    }

    /// The title bar button at a screen point.
    fn title_button_at(&self, x: i32, y: i32) -> Option<TitleButton> {
        TitleButton::ALL.into_iter().find(|&b| {
            let (cx, cy) = self.title_center(b);
            (x - self.rect.x - cx).abs() <= 10 && (y - self.rect.y - cy).abs() <= 10
        })
    }

    fn max_center(&self) -> (i32, i32) {
        (self.rect.w - 46, TITLE_H / 2)
    }

    fn min_center(&self) -> (i32, i32) {
        (self.rect.w - 70, TITLE_H / 2)
    }

    fn min_button(&self) -> Rect {
        let (cx, cy) = self.min_center();
        Rect::new(self.rect.x + cx - 10, self.rect.y + cy - 10, 20, 20)
    }

    fn max_button(&self) -> Rect {
        let (cx, cy) = self.max_center();
        Rect::new(self.rect.x + cx - 10, self.rect.y + cy - 10, 20, 20)
    }

    fn client_local(&self) -> Rect {
        let (w, h) = client_size(self.rect.w, self.rect.h);
        Rect::new(1, TITLE_H, w, h)
    }

    fn client(&self) -> Rect {
        self.client_local().offset(self.rect.x, self.rect.y)
    }

    /// Everything the window touches on screen, including the shadow.
    fn bounds(&self) -> Rect {
        self.rect.inset(-SHADOW_PAD)
    }

    /// Under which name the glass is remembered: the script path or the built-in application's kind.
    fn glass_key(&self) -> String {
        self.script.clone().unwrap_or_else(|| self.kind.into())
    }

    /// The client area background opacity: the window's own or the one shared by all windows.
    fn opacity(&self) -> u32 {
        self.alpha.unwrap_or_else(|| theme::get().window_alpha)
    }

    /// The blur radius of the background under the window's glass.
    fn blur(&self) -> i32 {
        self.frost.unwrap_or_else(|| theme::get().frost) as i32
    }

    /// The part of the window that covers everything under it (the opaque client area).
    fn opaque(&self) -> Rect {
        if self.opacity() == 255 && !self.minimized { self.client() } else { Rect::new(0, 0, 0, 0) }
    }

    /// The part of the window under which the background is blurred.
    /// The minimum window size: the common limit or the one the application asks for.
    fn min_size(&self) -> (i32, i32) {
        match self.app.min_size() {
            Some((w, h)) => ((w + 2).max(MIN_W), (h + TITLE_H + RADIUS).max(MIN_H)),
            None => (MIN_W, MIN_H),
        }
    }

    fn glass(&self) -> Rect {
        if self.opacity() < 255 { self.rect } else { Rect::new(self.rect.x, self.rect.y, self.rect.w, TITLE_H) }
    }

    fn close_center(&self) -> (i32, i32) {
        (self.rect.w - 22, TITLE_H / 2)
    }

    fn close_button(&self) -> Rect {
        let (cx, cy) = self.close_center();
        Rect::new(self.rect.x + cx - 10, self.rect.y + cy - 10, 20, 20)
    }

    /// Redraws the window's own surface.
    /// `part` — only this piece of the client area changed (in window coordinates):
    /// redraw only it, the title and the rest are already on the surface.
    fn render(&mut self, ctx: &Ctx, part: Option<Rect>) {
        let mut surface = core::mem::replace(&mut self.surface, Canvas::new(0, 0));
        self.paint_some(&mut surface, ctx, part);
        self.surface = surface;
        if let Some(mut twin) = self.twin.take() {
            self.paint_some(&mut twin.surface, ctx, part);
            self.twin = Some(twin);
        }
        self.dirty = false;
        self.whole = false;
    }

    fn paint_some(&mut self, s: &mut Canvas, ctx: &Ctx, part: Option<Rect>) {
        let Some(r) = part else { return self.paint(s, ctx) };
        let client = self.client_local();
        let r = r.intersect(client);
        if r.is_empty() {
            return;
        }
        s.set_clip(r);
        s.set(r, premul(self.app.background(), self.opacity()));
        self.app.draw(s, client, ctx);
        // A piece at the window edge: corner rounding and the outline — as in a full draw.
        let full = s.bounds();
        s.round_mask(full, RADIUS);
        s.stroke_round(full, RADIUS, 0xFFFFFF, if self.focused { 70 } else { 40 });
        s.reset_clip();
    }

    fn paint(&mut self, s: &mut Canvas, ctx: &Ctx) {
        let client = self.client_local();
        let opacity = self.opacity();
        let full = s.bounds();
        let (w, h) = (s.w, s.h);
        s.reset_clip();

        // The title is translucent "glass" with a gradient and a highlight on top.
        // Windows without focus have their colour muted towards grey.
        let th = theme::get();
        let (top, bottom) = if self.focused {
            th.title
        } else {
            (mix(th.title.0, 0x30333F, 150) & 0xFFFFFF, mix(th.title.1, 0x20222C, 150) & 0xFFFFFF)
        };
        // A window's own transparency makes the title more transparent too — proportionally.
        let title_alpha = self.alpha.map_or(th.title_alpha, |a| th.title_alpha * a / 255);
        s.set_gradient(Rect::new(0, 0, w, TITLE_H), top, bottom, title_alpha);
        s.set(Rect::new(0, TITLE_H, w, h - TITLE_H), premul(self.app.background(), opacity));
        s.blend(Rect::new(0, 1, w, 1), 0xFFFFFF, 50);
        s.blend(Rect::new(0, TITLE_H - 1, w, 1), 0x000000, 90);

        s.set_clip(client);
        self.app.draw(s, client, ctx);
        s.reset_clip();

        let title_color = if self.focused { TEXT } else { TEXT_DIM };
        let tw = font::measure(FontId::SemiBold, 13.5, &self.title);
        s.text((w - tw) / 2, 9, &self.title, title_color, FontId::SemiBold, 13.5);

        for button in TitleButton::ALL {
            let (cx, cy) = self.title_center(button);
            let r = Rect::new(cx - 7, cy - 7, 14, 14);
            if self.title_hover == Some(button) && self.title_held == Some(button) {
                // Pressed: darker, the halo denser and narrower.
                s.fill_round(r.inset(-3), 10, button.color(), 120);
                s.fill_round(r, 7, mix(0x000000, button.color(), 70) & 0xFFFFFF, 256);
                button.draw_mark(s, cx, cy);
            } else if self.title_hover == Some(button) {
                // A highlighted button is visible on a window without focus too: a soft halo, a lighter colour.
                s.fill_round(r.inset(-4), 11, button.color(), 70);
                s.fill_round(r, 7, mix(0xFFFFFF, button.color(), 80) & 0xFFFFFF, 256);
                button.draw_mark(s, cx, cy);
            } else if self.focused {
                s.fill_round(r, 7, button.color(), 256);
                s.fill_round(Rect::new(cx - 4, cy - 5, 8, 4), 2, 0xFFFFFF, 60);
            } else {
                s.fill_round(r, 7, 0x5A6078, 256);
            }
        }

        s.round_mask(full, RADIUS);
        s.stroke_round(full, RADIUS, 0xFFFFFF, if self.focused { 70 } else { 40 });
    }
}

/// The finished top bar glass on one screen: the blurred background without labels and icons.
struct BarGlass {
    screen: Rect,
    bar: Rect,
    pixels: Vec<u32>,
}

/// The backing used while a window is dragged or resized: everything under
/// it (without it and without the panels). The frame is then a copy of the backing, the window itself and the panels.
struct Underlay {
    /// The window being moved.
    id: u64,
    /// One canvas per screen.
    canvases: Vec<Canvas>,
    /// Where something changed under the window and the backing must be rebuilt.
    dirty: Vec<Rect>,
}

/// What `compose` builds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    /// The whole scene.
    Full,
    /// The backing: everything except the top window and the panels.
    Under,
    /// The scene on top of a ready backing: the top window and the panels.
    Drag,
}

/// The "Glass" panel (right click on the title): the program's transparency and frost.
struct GlassPanel {
    /// The window the panel was opened from (the setting applies to its whole program).
    id: u64,
    rect: Rect,
    /// Which slider is dragged: 0 — opacity, 1 — frost.
    drag: Option<usize>,
    /// A terminal window: the panel also has the background colour and the scrollback length.
    terminal: bool,
    /// How many lines to keep. Applied when the panel closes: while 5000 is being typed,
    /// the intermediate 500 must not cut the output.
    scrollback: widgets::TextField,
    /// The focused field: keys go to it, not to the window. And the cursor blinking in it.
    typing: bool,
    caret_on: bool,
}

const PANEL_W: i32 = 290;
const PANEL_H: i32 = 214;
/// The background colour row and the scrollback row in the terminal panel.
const PANEL_COLORS_H: i32 = 66;
const PANEL_COLORS_Y: i32 = 180;
const PANEL_SCROLLBACK_H: i32 = 46;
const PANEL_SCROLLBACK_Y: i32 = 222;
const PANEL_FIELD_W: i32 = 96;
const PANEL_FIELD_H: i32 = 30;
const SCROLLBACK_DIGITS: usize = 6;
const PANEL_SWATCH: i32 = 26;
const PANEL_SWATCH_STEP: i32 = 40;

impl GlassPanel {
    fn slider(&self, n: usize) -> Rect {
        Rect::new(self.rect.x + 18, self.rect.y + 76 + n as i32 * 52, self.rect.w - 36, 22)
    }

    fn color_swatch(&self, i: usize) -> Rect {
        Rect::new(self.rect.x + 20 + i as i32 * PANEL_SWATCH_STEP, self.rect.y + PANEL_COLORS_Y, PANEL_SWATCH, PANEL_SWATCH)
    }

    fn scrollback_field(&self) -> Rect {
        Rect::new(self.rect.x + self.rect.w - 18 - PANEL_FIELD_W, self.rect.y + PANEL_SCROLLBACK_Y, PANEL_FIELD_W, PANEL_FIELD_H)
    }

    /// The entered value, if it is within the allowed limits.
    fn scrollback_lines(&self) -> Option<u32> {
        self.scrollback.text.parse().ok().filter(|v| theme::SCROLLBACK_RANGE.contains(v))
    }

    fn reset_button(&self) -> Rect {
        let w = widgets::button_width("Reset");
        Rect::new(self.rect.x + self.rect.w - 16 - w, self.rect.y + self.rect.h - 16 - widgets::BUTTON_H, w, widgets::BUTTON_H)
    }
}

fn load_glass_prefs() -> BTreeMap<String, (Option<u32>, Option<u32>)> {
    let text = fs::read(&crate::access::config(GLASS_FILE)).map(|d| String::from_utf8_lossy(&d).into_owned()).unwrap_or_default();
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.rsplit_once('=')?;
            let (a, f) = value.split_once(',')?;
            let alpha = a.trim().parse::<u32>().ok().map(|a| a.clamp(MIN_ALPHA, 255));
            let frost = f.trim().parse::<u32>().ok().map(|f| f.min(theme::MAX_FROST));
            Some((key.into(), (alpha, frost)))
        })
        .collect()
}

pub struct Wm {
    /// The primary screen size: the panel, Dock and icons are on it.
    w: i32,
    h: i32,
    /// The whole desktop with all screens.
    desk: Rect,
    /// The primary screen on the desktop.
    home: Rect,
    /// The screen the panel, Dock and popups are computed for right now.
    ui: Cell<Rect>,
    /// The screen where the user works with the panel: menus and new windows open there.
    active: Rect,
    /// The screens show their numbers.
    identifying: bool,
    /// The last window is the top one and has focus.
    windows: Vec<Window>,
    wallpaper: Vec<Vec<u32>>,
    wallpaper_id: usize,
    desktop: Vec<DesktopIcon>,
    desktop_version: u64,
    desktop_selected: Option<usize>,
    /// The last click on a desktop icon (tick, index) — for a double click.
    desktop_click: Option<(u64, usize)>,
    toasts: Vec<Toast>,
    /// The notification history (new ones at the end) and how many of them have not been seen.
    notes: Vec<Note>,
    unread: usize,
    /// The notification centre is open: Some(how many entries it is scrolled by).
    center: Option<usize>,
    /// The width of the clock in the panel (the bell stands to its left).
    clock_w: i32,
    now: (u8, u8),
    sysinfo: Vec<String>,
    dock: Vec<Rect>,
    /// Buttons of minimized windows on the right side of the dock: (rectangle, window id).
    dock_min: Vec<(Rect, u64)>,
    /// Where the file drag hint outside Files is drawn.
    drag_pill: Option<Rect>,
    /// The window switcher (Alt+Tab) is open: the selected position, 0 — the top window.
    switcher: Option<usize>,
    buttons: u8,
    drag: Option<(i32, i32)>,
    capture: bool,
    spawned: i32,
    /// Day, hour and minute on the panel clock last time.
    last_min: u32,
    /// Whether the menu is open and which item is under the cursor.
    menu: Option<Option<usize>>,
    /// Screen areas that need to be recomposed (merged when drawing).
    damage: Vec<Rect>,
    theme_version: u64,
    keyboard_version: u64,
    /// The last written contents of the session file.
    saved_session: String,
    /// Do not warn about low memory again before this tick.
    memory_warned: u64,
    /// Glass configured for programs (by `Window::glass_key`).
    glass_prefs: BTreeMap<String, (Option<u32>, Option<u32>)>,
    /// The open glass settings panel.
    glass_panel: Option<GlassPanel>,
    underlay: Option<Underlay>,
    bar_glass: RefCell<Vec<BarGlass>>,
    /// The network icon needs a redraw.
    net_changed: bool,
    /// The last click on a title bar (tick, x, y) — for a double click.
    title_click: Option<(u64, i32, i32)>,
    next_id: u64,
    /// The (top) window is being resized: (edges, the original rectangle, the press point).
    resizing: Option<(u8, Rect, i32, i32)>,
    mouse_pos: (i32, i32),
    /// When statistics were last published: (tick, CPU cycles).
    stats_at: (u64, u64),
    /// The window button under the cursor: (window, its rectangle).
    hover_button: Option<(u64, Rect)>,
    /// The window whose title bar button is highlighted.
    hover_title: Option<u64>,
    /// A held window button: the click goes to the application only if released over it.
    held_button: Option<(u64, Rect)>,
    /// A pressed title bar button: the window and the button. Fires on release over it.
    held_title: Option<(u64, TitleButton)>,
    /// The sound and network icons in the panel.
    tray: tray::Tray,
    /// The program menu (the gear button).
    launcher: launcher::Launcher,
    /// A screenshot is in progress.
    shot: Option<shot::Shot>,
    leave: Option<Leave>,
    /// The open calendar.
    calendar: Option<calendar::Calendar>,
    /// Logout or shutdown waits for an answer about unsaved changes: what we are doing and which window was asked.
    exit: Option<(MenuAction, Option<u64>)>,
    /// What the firmware should start with after this reboot.
    restart_to: crate::bootvars::Next,
}

impl Wm {
    pub fn new(w: i32, h: i32, sysinfo: Vec<String>) -> Self {
        let desk = crate::output::bounds();
        let home = crate::output::primary();
        let mut wm = Self {
            w,
            h,
            desk,
            home,
            ui: Cell::new(home),
            active: home,
            identifying: false,
            windows: Vec::new(),
            wallpaper: wallpaper::desktop(theme::get().wallpaper),
            wallpaper_id: theme::get().wallpaper,
            desktop: Vec::new(),
            desktop_version: u64::MAX,
            desktop_selected: None,
            desktop_click: None,
            toasts: Vec::new(),
            notes: Vec::new(),
            unread: 0,
            center: None,
            clock_w: 0,
            now: (0, 0),
            sysinfo,
            dock: dock_layout(w, h, &[]).0,
            dock_min: Vec::new(),
            drag_pill: None,
            switcher: None,
            buttons: 0,
            drag: None,
            capture: false,
            spawned: 0,
            last_min: u32::MAX,
            menu: None,
            damage: alloc::vec![desk],
            theme_version: theme::version(),
            keyboard_version: u64::MAX,
            saved_session: String::new(),
            memory_warned: 0,
            glass_prefs: load_glass_prefs(),
            glass_panel: None,
            underlay: None,
            bar_glass: RefCell::new(Vec::new()),
            net_changed: false,
            title_click: None,
            next_id: 1,
            resizing: None,
            mouse_pos: (w / 2, h / 2),
            stats_at: (0, crate::arch::rdtsc()),
            hover_button: None,
            hover_title: None,
            held_button: None,
            held_title: None,
            tray: tray::Tray::default(),
            launcher: launcher::Launcher::default(),
            shot: None,
            leave: None,
            calendar: None,
            exit: None,
            restart_to: crate::bootvars::Next::Normal,
        };
        if !wm.restore_session() {
            // The very first start: the file manager and the clock on the right, the terminal on the left.
            let right = w - 40;
            wm.launch("clock", None, Some((right - 380, 500)));
            wm.launch("files", None, Some((right - 720, 56)));
            wm.launch("terminal", None, Some((50, 90)));
        }
        wm
    }

    /// Opens windows from the session file. false — no file (the first start).
    fn restore_session(&mut self) -> bool {
        let Ok(data) = fs::read(&crate::access::config(SESSION_FILE)) else { return false };
        for line in String::from_utf8_lossy(&data).lines() {
            let fields: Vec<&str> = line.split('|').collect();
            if fields.len() < 4 {
                continue;
            }
            // Format `kind|x|y|WxH|M|argument`; files from old versions had no size and no M flag.
            let size = fields[3].split_once('x').and_then(|(w, h)| Some((w.parse::<i32>().ok()?, h.parse::<i32>().ok()?)));
            let rest = &fields[3 + size.is_some() as usize..];
            let (flags, arg) = match rest.first() {
                Some(&f @ ("-" | "M" | "m" | "Mm")) if rest.len() >= 2 => (f, rest[1..].join("|")),
                _ => ("-", rest.join("|")),
            };
            let arg = Some(arg).filter(|a| !a.is_empty());
            let pos = fields[1].parse().ok().zip(fields[2].parse().ok());
            let count = self.windows.len();
            if fields[0] == "lua" {
                // The process opens the window itself when it starts.
                if let Some(path) = arg {
                    lua::launch(&path, Some(lua::Placement { pos, size, maximized: flags.contains('M'), minimized: flags.contains('m') }));
                }
            } else {
                self.launch(fields[0], arg, pos);
            }
            if self.windows.len() > count {
                // Settings and About get their size from their contents — a saved one (from old versions) is ignored.
                if let Some((w, h)) = size.filter(|_| !matches!(fields[0], "settings" | "about" | "netcfg" | "bluetooth")) {
                    let win = self.windows.last_mut().unwrap();
                    let (r, (min_w, min_h)) = (win.rect, win.min_size());
                    win.set_rect(Rect::new(r.x, r.y, w.max(min_w), h.max(min_h)));
                }
                if flags.contains('M') {
                    self.toggle_maximize();
                }
                if flags.contains('m') {
                    self.minimize(self.windows.len() - 1);
                }
            }
        }
        true
    }

    /// A line per window: `kind|x|y|WxH|flags|argument`, bottom to top (the last one has focus).
    /// Flags: M — maximized (the position and size are then from before maximizing), m — minimized, `-` — none.
    fn session_text(&self) -> String {
        self.windows
            .iter()
            // Dialogs and the installer are one-off: they are not reopened after a reboot.
            .filter(|w| w.kind != "dialog" && w.kind != "install")
            .map(|w| {
                let arg = w.script.clone().or_else(|| w.app.location()).unwrap_or_default();
                let r = w.restore.unwrap_or(w.rect);
                let flag = match (w.restore.is_some(), w.minimized) {
                    (false, false) => "-",
                    (true, false) => "M",
                    (false, true) => "m",
                    (true, true) => "Mm",
                };
                format!("{}|{}|{}|{}x{}|{}|{}\n", w.kind, r.x, r.y, r.w, r.h, flag, arg)
            })
            .collect()
    }

    fn close(&mut self, i: usize) {
        let gone = self.windows[i].bounds();
        self.mark_layer(gone, i);
        self.windows.remove(i);
        self.refocus();
        self.relayout_dock();
    }

    /// Once a second: the load of all cores (the main one by idle time in hlt, helpers by their
    /// work) and the tasks with their CPU and memory.
    fn publish_stats(&mut self, ticks: u64) {
        let now = crate::arch::rdtsc();
        let total = (now - self.stats_at.1).max(1);
        self.stats_at = (ticks, now);
        let busy = total.saturating_sub(stats::take_idle());
        crate::power::sample();
        let mut tasks: Vec<Task> = self
            .windows
            .iter_mut()
            .rev()
            .map(|w| {
                let cpu = (core::mem::take(&mut w.cycles) * 1000 / total) as u32;
                Task { id: w.id, title: w.title.clone(), kind: w.kind, cpu, mem: w.memory() }
            })
            .collect();
        tasks.extend(crate::task::take_stats().into_iter().map(|t| Task {
            id: stats::THREAD_ID_BASE + t.id,
            title: t.name,
            kind: if t.user { "process" } else { "system" },
            cpu: (t.cycles * 1000 / total).min(1000) as u32,
            mem: t.mem,
        }));
        let all_cores = total * crate::smp::cpu_count() as u64;
        stats::publish(tasks, ((busy + stats::take_helper_work()) * 1000 / all_cores).min(1000) as u32);
    }

    /// Maximizes the top window to all the space between the panel and the dock, or restores it.
    fn toggle_maximize(&mut self) {
        let Some(win) = self.windows.last() else { return };
        let bounds = win.bounds();
        let screen = crate::output::screen_at(bounds.x + bounds.w / 2, bounds.y + TITLE_H / 2);
        // On the screen with the panel and the Dock, leave room for them.
        let full = if self.bars().contains(&screen) {
            Rect::new(screen.x, screen.y + TOPBAR_H, screen.w, screen.h - TOPBAR_H - DOCK_H - DOCK_MARGIN - 6)
        } else {
            screen
        };
        let Some(win) = self.windows.last_mut() else { return };
        let before = win.bounds();
        match win.restore.take() {
            Some(r) => win.set_rect(r),
            None => {
                win.restore = Some(win.rect);
                win.set_rect(full);
            }
        }
        let after = win.bounds();
        self.mark(before);
        self.mark(after);
    }

    /// Writes the session if it changed (the FS autosave carries it to disk).
    fn save_session(&mut self) {
        if self.drag.is_some() {
            return; // the position is written once the window is released
        }
        let text = self.session_text();
        if text != self.saved_session {
            if fs::write(&crate::access::config(SESSION_FILE), text.as_bytes()).is_ok() {
                self.saved_session = text;
            }
        }
    }

    /// A screen area must be recomposed (something above all windows changed, or the top window).
    fn mark(&mut self, r: Rect) {
        if !r.is_empty() {
            self.damage.push(r);
        }
    }

    /// Layer `layer` (the window with this index) changed in area `r`: windows above it
    /// have a stale part of the background cache — the part the blur reaches.
    fn mark_layer(&mut self, r: Rect, layer: usize) {
        self.mark(r);
        self.mark_underlay(r, layer);
        self.stale_above(r, layer + 1);
    }

    /// A change below the moved window also goes into the backing.
    fn mark_underlay(&mut self, r: Rect, layer: usize) {
        if let Some(u) = &mut self.underlay
            && self.windows.iter().position(|w| w.id == u.id).is_some_and(|top| layer < top)
        {
            u.dirty.push(r);
        }
    }

    /// Marks stale pieces of the background cache of windows starting from `from`. A change
    /// rises through the stack: a window's stale background also changes what is seen through
    /// it, so for windows above the area grows by the blur radius.
    fn stale_above(&mut self, r: Rect, from: usize) {
        let mut changed = r;
        for win in self.windows.iter_mut().skip(from) {
            let hit = win.glass().intersect(changed.inset(-2 * win.blur()));
            if hit.is_empty() {
                continue;
            }
            if win.minimized {
                win.forget_backdrop();
            } else {
                win.stale = win.stale.union(hit);
                changed = changed.union(hit);
            }
        }
    }

    /// The application says the contents changed. The area is marked when
    /// the window is redrawn and it becomes clear whether the picture really changed
    /// (programs with a timer often redraw the same thing).
    /// Only the title changed.
    fn mark_title(&mut self, i: usize) {
        self.windows[i].dirty = true;
        self.windows[i].whole = true;
        let r = self.windows[i].rect;
        self.mark_layer(Rect::new(r.x, r.y, r.w, TITLE_H), i);
    }

    fn mark_content(&mut self, i: usize) {
        self.windows[i].dirty = true;
        self.windows[i].content_changed = true;
    }

    fn spawn(&mut self, kind: AppKind) {
        self.launch(kind.name(), None, None);
    }

    /// Creates a built-in application by kind name (as in the session file).
    fn create(&self, kind: &str, arg: Option<String>) -> Option<(&'static str, String, i32, i32, Box<dyn App>)> {
        let size = |w: i32, h: i32| (w, h, client_size(w, h));
        Some(match kind {
            "terminal" => ("terminal", "Terminal".into(), 620, 380, Box::new(Terminal::new())),
            "files" => {
                let (w, h, (cw, ch)) = size(720, 460);
                let path = arg.filter(|p| fs::is_dir(p)).unwrap_or_else(crate::access::home);
                ("files", "Files".into(), w, h, Box::new(Files::new(path, cw, ch)))
            }
            "notepad" => {
                let path = arg?;
                let (w, h, (cw, ch)) = size(620, 440);
                let title = format!("{} \u{2014} Notepad", fs::name(&path));
                ("notepad", title, w, h, Box::new(Notepad::new(path, cw, ch)))
            }
            "paint" => {
                let (w, h, (cw, ch)) = size(580, 440);
                let app = match arg.filter(|p| fs::exists(p)) {
                    Some(path) => Paint::open(path, cw, ch),
                    None => Paint::new(cw, ch),
                };
                ("paint", "Paint".into(), w, h, Box::new(app))
            }
            "clock" => ("clock", "Clock".into(), 380, 190, Box::new(Clock::new())),
            "menuedit" => {
                let (w, h, (cw, ch)) = size(620, 460);
                ("menuedit", "Edit App Menu".into(), w, h, Box::new(crate::apps::menuedit::MenuEditor::new(cw, ch)))
            }
            "recorder" => ("recorder", "Recorder".into(), 520, 320, Box::new(Recorder::new())),
            "settings" => {
                let (w, h, (cw, ch)) = size(SETTINGS_SIZE.0, SETTINGS_SIZE.1);
                ("settings", "Settings".into(), w, h, Box::new(Settings::new(cw, ch, arg.as_deref(), self.sysinfo.clone())))
            }
            "about" => ("about", "About".into(), 430, 280, Box::new(About::new(self.sysinfo.clone()))),
            "viewer" => {
                let path = arg?;
                ("viewer", fs::name(&path).into(), 860, 640, Box::new(Viewer::new(path)))
            }
            "netcfg" => {
                let (w, h, (cw, ch)) = size(470, 470);
                ("netcfg", "Network".into(), w, h, Box::new(NetSettings::new(cw, ch, arg.as_deref())))
            }
            "bluetooth" => {
                let (w, h, (cw, ch)) = size(500, 480);
                ("bluetooth", "Bluetooth".into(), w, h, Box::new(BtSettings::new(cw, ch)))
            }
            "taskmgr" => ("taskmgr", "Task Manager".into(), 640, 540, Box::new(TaskManager::new())),
            "disks" => {
                let (w, h, (cw, ch)) = size(820, 520);
                ("disks", "Disks".into(), w, h, Box::new(crate::apps::disks::Disks::new(cw, ch)))
            }
            "install" => {
                let (w, h, (cw, ch)) = size(600, 500);
                ("install", "Install GlassOS".into(), w, h, Box::new(crate::apps::Installer::new(cw, ch)))
            }
            _ => return None,
        })
    }

    fn launch(&mut self, kind: &str, arg: Option<String>, pos: Option<(i32, i32)>) {
        // Hardware and network configuration are Settings sections: one window, one entry point (old
        // window names, including those from saved sessions, lead there too).
        let page = match kind {
            "netcfg" => Some(match arg.as_deref() {
                Some(a) if !a.is_empty() => format!("network:{a}"),
                _ => String::from("network"),
            }),
            "bluetooth" => Some(String::from("bluetooth")),
            "disks" => Some(String::from("disks")),
            _ => None,
        };
        if let Some(page) = page {
            self.open_settings(&page, pos);
            return;
        }
        if let Some((kind, title, w, h, app)) = self.create(kind, arg) {
            self.open(title, kind, None, w, h, app, pos);
        }
    }

    /// Settings at section `page`: an already open window switches, otherwise a new one.
    pub(crate) fn open_settings(&mut self, page: &str, pos: Option<(i32, i32)>) {
        match self.windows.iter().rposition(|w| w.kind == "settings") {
            Some(i) => {
                self.windows[i].app.open_arg(page);
                self.activate(i);
            }
            None => {
                if let Some((kind, title, w, h, app)) = self.create("settings", Some(page.into())) {
                    self.open(title, kind, None, w, h, app, pos);
                }
            }
        }
    }

    fn handle_request(&mut self, r: Request) {
        match r {
            // Pictures open in Paint, the rest in Notepad.
            Request::OpenFile(path) if crate::image::is_picture(&path) => self.launch("viewer", Some(path), None),
            // Sound files go to the player (a Lua program), the rest to Notepad.
            Request::OpenFile(path) if is_audio(&path) && fs::exists(PLAYER) => {
                // The player is already open — the file goes to it, not to a second player.
                let open = self.windows.iter().rposition(|w| w.script.as_deref() == Some(PLAYER));
                let taken = open.is_some_and(|i| self.windows[i].app.open_files(core::slice::from_ref(&path)));
                match open {
                    Some(i) if taken => {
                        self.activate(i);
                        let top = self.windows.len() - 1;
                        self.mark_content(top);
                    }
                    _ => lua::launch_with(PLAYER, &[&path], None),
                }
            }
            Request::DropFiles(paths) => {
                let (x, y) = self.mouse_pos;
                if let Some(i) = self.windows.iter().rposition(|w| !w.minimized && w.rect.contains(x, y))
                    && self.windows[i].app.open_files(&paths)
                {
                    self.activate(i);
                    let top = self.windows.len() - 1;
                    self.mark_content(top);
                }
            }
            Request::OpenFile(path) => self.launch("notepad", Some(path), None),
            Request::OpenFolder(path) => self.launch("files", Some(path), None),
            Request::Run(path) => lua::launch(&path, None),
            // The output of a console program — into a new terminal.
            Request::Output(lines) => {
                let app = Box::new(Terminal::with_output(lines));
                self.open("Terminal".into(), "terminal", None, 620, 380, app, None);
            }
            Request::Window(win) => {
                let place = win.place;
                self.open(win.title, "lua", Some(win.path), win.w, win.h, Box::new(win.app), place.and_then(|p| p.pos));
                if let Some(p) = place {
                    if let Some((w, h)) = p.size
                        && let Some(win) = self.windows.last_mut()
                    {
                        let (r, (min_w, min_h)) = (win.rect, win.min_size());
                        win.set_rect(Rect::new(r.x, r.y, w.max(min_w), h.max(min_h)));
                    }
                    if p.maximized {
                        self.toggle_maximize();
                    }
                    if p.minimized {
                        self.minimize(self.windows.len() - 1);
                    }
                }
            }
            Request::Close(id) => {
                if let Some(i) = self.windows.iter().position(|w| w.id == id) {
                    self.close(i);
                }
            }
            Request::Dialog(dialog) => {
                let (w, h) = (520, 440);
                let pos = (self.left() + (self.ui.get().w - w) / 2, self.middle(h) - 20);
                let title = dialog.title().into();
                self.open(title, "dialog", None, w, h, Box::new(dialog), Some(pos));
            }
            Request::Screenshot => self.start_shot(),
            Request::Restart => self.restart(crate::bootvars::Next::Normal),
            Request::RestartInto(next) => self.restart(next),
            Request::Shutdown => self.exit = Some((MenuAction::Shutdown, None)),
            Request::Launch(kind) => self.launch(kind, None, None),
            Request::LaunchArg(kind, arg) => self.launch(kind, Some(arg), None),
            Request::Popup { title, app, w, h } => {
                let pos = (self.left() + (self.ui.get().w - w) / 2, self.middle(h) - 40);
                self.open(title, "dialog", None, w, h, app, Some(pos));
            }
        }
    }

    /// Opens a new window above the others: at `pos` or cascaded.
    fn open(
        &mut self,
        title: String,
        kind: &'static str,
        script: Option<String>,
        w: i32,
        h: i32,
        app: Box<dyn App>,
        pos: Option<(i32, i32)>,
    ) {
        if !window_fits(w, h) {
            kprintln!("wm: no memory for window \"{title}\" ({} MiB free)", crate::heap_stats().1 >> 20);
            // A program in a loop must not flood the screen with identical notifications.
            let now = interrupts::ticks();
            if now >= self.memory_warned {
                self.memory_warned = now + 10 * HZ;
                notify_with(Some(Sound::Error), "Not enough memory", &format!("Close some windows to open {title}"));
            }
            return;
        }
        let (x, y) = match pos {
            // A saved position is adjusted so that the title bar is reachable.
            Some((x, y)) => {
                let screen = crate::output::screen_at(x + w / 2, y);
                let bottom = screen.y + screen.h - TITLE_H - DOCK_H;
                (x.clamp(screen.x + 60 - w, screen.x + screen.w - 60), y.clamp(screen.y + TOPBAR_H, bottom))
            }
            None => {
                let n = self.spawned % 8;
                self.spawned += 1;
                let x = (self.left() + 90 + n * 36).min(self.right() - w).max(self.left());
                // Keep the bottom edge of the window above the dock.
                let y = (self.top() + 70 + n * 30).min(self.bottom() - h - DOCK_H - DOCK_MARGIN - 4).max(self.top() + TOPBAR_H);
                (x, y)
            }
        };
        let mut win = Window::new(title, kind, script, Rect::new(x, y, w, h), app);
        (win.alpha, win.frost) = self.glass_prefs.get(&win.glass_key()).copied().unwrap_or_default();
        win.id = self.next_id;
        self.next_id += 1;
        self.mark(win.bounds());
        self.windows.push(win);
        self.refocus();
        self.relayout_dock();
    }

    /// The top window has focus; windows whose focus changed are redrawn.
    fn refocus(&mut self) {
        let top = self.windows.len().wrapping_sub(1);
        for i in 0..self.windows.len() {
            let win = &mut self.windows[i];
            let focused = i == top && !win.minimized;
            if win.focused != focused {
                win.focused = focused;
                win.dirty = true;
                win.whole = true;
                let r = win.bounds();
                self.mark_layer(r, i);
            }
        }
    }

    fn left(&self) -> i32 {
        self.ui.get().x
    }

    fn right(&self) -> i32 {
        self.ui.get().x + self.ui.get().w
    }

    fn top(&self) -> i32 {
        self.ui.get().y
    }

    fn bottom(&self) -> i32 {
        self.ui.get().y + self.ui.get().h
    }

    /// The top of what stands in the middle of the screen vertically.
    fn middle(&self, h: i32) -> i32 {
        self.top() + (self.ui.get().h - h) / 2
    }

    /// Screens with the panel and the Dock; the first is the primary.
    fn bars(&self) -> Vec<Rect> {
        let mut screens = crate::output::rects();
        screens.dedup();
        if !theme::get().bars_everywhere {
            screens.truncate(1);
        }
        screens
    }

    fn mark_bars(&mut self) {
        for b in self.bars() {
            self.mark(Rect::new(b.x, b.y, b.w, TOPBAR_H));
        }
    }

    /// Draws and lays out the Dock on all screens: the layout is computed for the primary one.
    fn dock_shift(&self) -> (i32, i32) {
        let ui = self.ui.get();
        (ui.x + (ui.w - self.w) / 2, ui.y + ui.h - self.h)
    }

    fn mark_docks(&mut self) {
        let saved = self.ui.get();
        for b in self.bars() {
            self.ui.set(b);
            self.mark(self.dock_rect().inset(-SHADOW_PAD));
        }
        self.ui.set(saved);
    }

    /// The panel, Dock and menus work on the screen the cursor went to; an open
    /// popup keeps them in place.
    fn follow_mouse(&mut self, x: i32, y: i32) {
        if self.menu.is_some() || self.center.is_some() || self.calendar.is_some() || self.switcher.is_some() || self.launcher_open() {
            return;
        }
        let bars = self.bars();
        let screen = crate::output::screen_at(x, y);
        let target = if bars.contains(&screen) { screen } else { bars.first().copied().unwrap_or(screen) };
        if target != self.active {
            self.active = target;
            self.ui.set(target);
        }
    }

    fn os_button(&self) -> Rect {
        Rect::new(self.left() + 6, self.top() + 3, font::measure(FontId::SemiBold, 14.0, OS_NAME) + 24, TOPBAR_H - 6)
    }

    fn menu_rect(&self) -> Rect {
        Rect::new(self.left() + 8, self.top() + TOPBAR_H + 6, 210, menu_items().len() as i32 * MENU_ITEM_H + 12)
    }

    fn menu_item(&self, i: usize) -> Rect {
        let m = self.menu_rect();
        Rect::new(m.x + 6, m.y + 6 + i as i32 * MENU_ITEM_H, m.w - 12, MENU_ITEM_H)
    }

    fn menu_len(&self) -> usize {
        menu_items().len()
    }

    fn dock_rect(&self) -> Rect {
        let first = self.dock[0];
        let last = self.dock_min.last().map_or(self.dock[self.dock.len() - 1], |(r, _)| *r);
        let (dx, dy) = self.dock_shift();
        Rect::new(first.x - 8 + dx, self.h - DOCK_MARGIN - DOCK_H + dy, last.x + last.w - first.x + 16, DOCK_H)
    }

    /// Recomputes the dock (minimized window buttons, indicators) and marks it for redraw.
    fn relayout_dock(&mut self) {
        self.mark_docks();
        let minimized: Vec<(String, u64)> =
            self.windows.iter().filter(|w| w.minimized).map(|w| (w.title.clone(), w.id)).collect();
        let titles: Vec<&str> = minimized.iter().map(|(t, _)| t.as_str()).collect();
        let (launchers, min_rects) = dock_layout(self.w, self.h, &titles);
        self.dock = launchers;
        self.dock_min = min_rects.into_iter().zip(minimized.iter().map(|(_, id)| *id)).collect();
        self.mark_docks();
    }

    /// Raises a window to the top (and restores it from the dock if it is minimized).
    fn activate(&mut self, i: usize) {
        let was_minimized = self.windows[i].minimized;
        if i != self.windows.len() - 1 || was_minimized {
            // The window order changed — the background under the windows above `i` became different.
            let r = self.windows[i].bounds();
            self.mark_layer(r, i);
            let mut win = self.windows.remove(i);
            win.forget_backdrop();
            win.minimized = false;
            self.mark(win.bounds());
            self.windows.push(win);
        }
        self.refocus();
        if was_minimized {
            self.relayout_dock();
        }
    }

    /// Minimizes a window to the dock: it goes to the very bottom of the list and is not drawn.
    fn minimize(&mut self, i: usize) {
        let r = self.windows[i].bounds();
        self.mark_layer(r, i);
        let mut win = self.windows.remove(i);
        win.minimized = true;
        win.forget_backdrop();
        self.windows.insert(0, win);
        self.refocus();
        self.relayout_dock();
    }

    /// An application button in the dock: raise its window, and if it is already active — open a new one.
    fn dock_launch(&mut self, kind: AppKind) {
        let name = kind.name();
        let top_is_kind = self.windows.last().is_some_and(|w| w.kind == name && !w.minimized);
        match self.windows.iter().rposition(|w| w.kind == name) {
            Some(i) if !top_is_kind => self.activate(i),
            _ => self.spawn(kind),
        }
    }

    /// Windows in most-recently-used order (top to bottom) — for Alt+Tab.
    fn switch_order(&self) -> Vec<usize> {
        let visible = self.windows.iter().enumerate().rev().filter(|(_, w)| !w.minimized);
        let hidden = self.windows.iter().enumerate().rev().filter(|(_, w)| w.minimized);
        visible.chain(hidden).map(|(i, _)| i).collect()
    }

    fn switcher_rect(&self) -> Rect {
        let n = self.windows.len() as i32;
        let (w, h) = (420, n * 36 + 52);
        Rect::new(self.left() + (self.ui.get().w - w) / 2, self.middle(h), w, h)
    }

    fn close_menu(&mut self) {
        self.menu = None;
        self.mark(self.menu_rect().inset(-SHADOW_PAD));
        self.mark_bars();
    }

    fn menu_action(&mut self, action: MenuAction) {
        match action {
            MenuAction::Screenshot => self.start_shot(),
            MenuAction::About => self.spawn(AppKind::About),
            MenuAction::Install => self.launch("install", None, None),
            MenuAction::Lock => self.leave = Some(Leave::Lock),
            MenuAction::Restart => self.restart(crate::bootvars::Next::Normal),
            MenuAction::LogOut | MenuAction::Shutdown => self.exit = Some((action, None)),
        }
    }

    pub fn text(&mut self, ch: char, ctx: &Ctx) {
        if self.shot_active() || self.switcher.is_some() || self.glass_panel_text(ch) {
            return;
        }
        if let Some(win) = self.windows.last_mut()
            && !win.minimized
            && win.app.text(ch, ctx)
        {
            let top = self.windows.len() - 1;
            self.mark_content(top);
        }
    }

    pub fn key(&mut self, ch: u8, ctx: &Ctx) {
        if self.launcher_key(ch) {
            return;
        }
        // Volume and brightness (Fn+…) are system keys, programs do not get them.
        if self.media_key(ch) {
            return;
        }
        if self.shot_active() {
            self.shot_key(ch);
            return;
        }
        if self.glass_panel_key(ch) {
            return;
        }
        if self.calendar_key(ch) {
            return;
        }
        if ch == crate::input::KEY_PRINT {
            self.start_shot();
            return;
        }
        // The window switcher: Tab with Alt/Ctrl held moves the selection, release switches.
        match (ch, self.switcher) {
            (KEY_SWITCH, _) if self.windows.len() > 1 => {
                let n = self.windows.len();
                self.switcher = Some(self.switcher.map_or(1, |s| (s + 1) % n));
                self.mark(self.switcher_rect().inset(-SHADOW_PAD));
                return;
            }
            (KEY_MOD_UP | 0x1B, Some(sel)) => {
                self.mark(self.switcher_rect().inset(-SHADOW_PAD));
                self.switcher = None;
                if ch == KEY_MOD_UP {
                    let i = self.switch_order()[sel];
                    self.activate(i);
                }
                return;
            }
            (KEY_SWITCH | KEY_MOD_UP, _) => return,
            _ => {}
        }
        if let Some(win) = self.windows.last_mut()
            && !win.minimized
            && win.app.key(ch, ctx)
        {
            let top = self.windows.len() - 1;
            self.mark_content(top);
        }
    }

    pub fn mouse(&mut self, x: i32, y: i32, buttons: u8) {
        let pressed = buttons & LEFT != 0 && self.buttons & LEFT == 0;
        let released = buttons & LEFT == 0 && self.buttons & LEFT != 0;
        let right = buttons & RIGHT != 0 && self.buttons & RIGHT == 0;
        self.buttons = buttons;
        self.mouse_pos = (x, y);
        if self.shot_active() {
            self.shot_mouse(x, y, buttons, pressed, released);
            return;
        }
        self.follow_mouse(x, y);
        self.tray_hover(x, y);
        self.button_hover(x, y);
        self.title_hover(x, y);
        if self.calendar_mouse(x, y, pressed) {
            return;
        }
        if self.tray_mouse(x, y, pressed, released) {
            return;
        }
        if self.drag.is_none() && self.resizing.is_none() && !self.capture && self.launcher_mouse(x, y, pressed, right) {
            return;
        }
        if self.glass_panel_mouse(x, y, pressed, released, right) {
            return;
        }
        // A right click in a window goes to the application (the context menu); the window is raised.
        if right
            && self.drag.is_none()
            && self.resizing.is_none()
            && !self.capture
            && let Some(i) = self.windows.iter().rposition(|w| !w.minimized && w.rect.contains(x, y))
            && y >= self.windows[i].rect.y + TITLE_H
        {
            self.close_menu();
            self.activate(i);
            let top = self.windows.len() - 1;
            let c = self.windows[top].client();
            if self.windows[top].app.context_click(x - c.x, y - c.y) {
                self.mark_content(top);
            }
            return;
        }

        if let Some((id, button)) = self.held_title {
            if released {
                self.held_title = None;
                if let Some(i) = self.windows.iter().position(|w| w.id == id) {
                    self.windows[i].title_held = None;
                    self.mark_title(i);
                    if self.windows[i].title_button_at(x, y) == Some(button) {
                        self.title_action(i, button);
                    }
                }
            }
            return;
        }

        if let Some((edges, start, x0, y0)) = self.resizing {
            let win = self.windows.last_mut().unwrap();
            let before = win.bounds();
            let (dx, dy) = (x - x0, y - y0);
            let (min_w, min_h) = win.min_size();
            let mut r = start;
            if edges & EDGE_LEFT != 0 {
                let w = (start.w - dx).max(min_w);
                r.x = start.x + start.w - w;
                r.w = w;
            }
            if edges & EDGE_RIGHT != 0 {
                r.w = (start.w + dx).max(min_w);
            }
            if edges & EDGE_BOTTOM != 0 {
                r.h = (start.h + dy).max(min_h);
            }
            win.set_rect_live(r);
            if released {
                self.resizing = None;
                win.resizing = false;
                win.remake_shadows();
            }
            let after = win.bounds();
            self.mark(before);
            self.mark(after);
            return;
        }

        if let Some(hovered) = self.menu {
            let now = (0..self.menu_len()).find(|&i| self.menu_item(i).contains(x, y));
            if now != hovered {
                self.menu = Some(now);
                self.mark(self.menu_rect());
            }
        }

        if let Some((dx, dy)) = self.drag {
            if released {
                self.drag = None;
                self.fit_top_window();
                return;
            }
            let screen = crate::output::screen_at(x, y);
            let (min_y, max_y) = (screen.y + TOPBAR_H, screen.y + screen.h - TITLE_H - 10);
            let win = self.windows.last_mut().unwrap();
            let before = win.bounds();
            let (mut dx, mut dy) = (dx, dy);
            if let Some(r) = win.restore.take() {
                // A maximized window was dragged by its title — restore its previous size under the cursor.
                (dx, dy) = (r.w / 2, TITLE_H / 2);
                self.drag = Some((dx, dy));
                win.set_rect(Rect::new(x - dx, y - dy, r.w, r.h));
            }
            win.set_rect(Rect::new(x - dx, (y - dy).clamp(min_y, max_y), win.rect.w, win.rect.h));
            let after = win.bounds();
            self.mark(before);
            self.mark(after);
            return;
        }

        if self.capture {
            if released {
                self.capture = false;
            }
            if let Some((id, r)) = self.held_button {
                // The button is held: its look changes, the click fires only on release over it.
                let Some(i) = self.windows.iter().position(|w| w.id == id) else {
                    self.held_button = None;
                    return;
                };
                self.mark_content(i);
                if released {
                    self.held_button = None;
                    let win = &mut self.windows[i];
                    let (lx, ly) = (x - win.rect.x, y - win.rect.y);
                    if r.contains(lx, ly) {
                        let c = win.client();
                        win.app.mouse(x - c.x, y - c.y, LEFT, true);
                        win.app.mouse(x - c.x, y - c.y, 0, false);
                    }
                }
                return;
            }
            let Some(win) = self.windows.last_mut() else { return };
            let c = win.client();
            if win.app.mouse(x - c.x, y - c.y, buttons, false) {
                let top = self.windows.len() - 1;
                self.mark_content(top);
            }
            return;
        }

        if !pressed {
            // Just motion — to the focused window (if the cursor is over it).
            if buttons == 0
                && let Some(win) = self.windows.last_mut()
                && !win.minimized
            {
                // The cursor is outside the window: tooltips and highlighting must not linger.
                let c = win.client();
                let (cx, cy) = if c.contains(x, y) { (x - c.x, y - c.y) } else { (-1, -1) };
                if win.app.hover(cx, cy) {
                    let top = self.windows.len() - 1;
                    self.mark_content(top);
                }
            }
            return;
        }

        if self.center_click(x, y) {
            return;
        }
        if self.bell_rect().contains(x, y) {
            self.close_menu();
            self.close_volume();
            self.toggle_center();
            return;
        }

        // While the menu is open, a click either chooses an item or just closes the menu.
        if self.menu.is_some() {
            let item = (0..self.menu_len()).find(|&i| self.menu_item(i).contains(x, y));
            self.close_menu();
            if let Some(i) = item {
                self.menu_action(menu_items()[i].0);
            }
            return;
        }
        if self.os_button().contains(x, y) {
            self.close_volume();
            self.close_launcher();
            self.menu = Some(None);
            self.mark(self.menu_rect().inset(-SHADOW_PAD));
            self.mark_bars();
            return;
        }

        // A click on a notification closes it.
        if let Some(i) = (0..self.toasts.len()).find(|&i| self.toast_rect(i).contains(x, y)) {
            let before = self.toasts_area();
            self.toasts.remove(i);
            self.mark(before);
            return;
        }

        // The dock is drawn over the windows, so it is checked first.
        let (dx, dy) = self.dock_shift();
        let (dock_x, dock_y) = (x - dx, y - dy);
        if let Some(i) = self.dock.iter().position(|r| r.contains(dock_x, dock_y)) {
            self.dock_launch(LAUNCHERS[i].0);
            return;
        }
        if let Some(&(_, id)) = self.dock_min.iter().find(|(r, _)| r.contains(dock_x, dock_y)) {
            if let Some(i) = self.windows.iter().position(|w| w.id == id) {
                self.activate(i);
            }
            return;
        }

        // The window under the cursor — including the edge grab zone outside the window.
        let Some(i) = self.windows.iter().rposition(|w| !w.minimized && w.rect.inset(-GRIP).contains(x, y)) else {
            self.desktop_click(x, y);
            return;
        };
        let edges = self.windows[i].edges_at(x, y);
        if edges == 0 && !self.windows[i].rect.contains(x, y) {
            self.desktop_click(x, y);
            return;
        }
        self.activate(i);
        let win = self.windows.last_mut().unwrap();
        if edges != 0 {
            self.resizing = Some((edges, win.rect, x, y));
            win.resizing = true;
            let r = win.bounds();
            self.mark(r);
            return;
        }
        if y < win.rect.y + TITLE_H {
            let now = interrupts::ticks();
            let double = self
                .title_click
                .is_some_and(|(t, px, py)| now - t < HZ * 2 / 5 && (px - x).abs() < 6 && (py - y).abs() < 6);
            let held = if win.close_button().contains(x, y) {
                Some(TitleButton::Close)
            } else if win.min_button().contains(x, y) {
                Some(TitleButton::Minimize)
            } else if win.max_button().contains(x, y) {
                Some(TitleButton::Maximize)
            } else {
                None
            };
            if let Some(button) = held {
                // A title bar button fires on release; while it is held it looks pressed.
                win.title_held = Some(button);
                win.title_hover = Some(button);
                self.held_title = Some((win.id, button));
                self.title_click = None;
                let top = self.windows.len() - 1;
                self.mark_title(top);
            } else if double {
                self.title_click = None;
                self.toggle_maximize();
            } else {
                self.title_click = Some((now, x, y));
                self.drag = Some((x - win.rect.x, y - win.rect.y));
            }
        } else {
            self.capture = true;
            let (lx, ly) = (x - win.rect.x, y - win.rect.y);
            if let Some(&r) = win.buttons.iter().find(|r| r.contains(lx, ly)) {
                // A button was pressed: show it pressed, the click fires on release.
                self.held_button = Some((win.id, r));
                let top = self.windows.len() - 1;
                self.mark_content(top);
                return;
            }
            let c = win.client();
            if win.app.mouse(x - c.x, y - c.y, buttons, true) {
                let top = self.windows.len() - 1;
                self.mark_content(top);
            }
        }
    }

    /// Tracks which window button the cursor is over; when it changes, the windows are redrawn.
    fn button_hover(&mut self, x: i32, y: i32) {
        let now = self
            .windows
            .iter()
            .rposition(|w| !w.minimized && w.rect.contains(x, y))
            .and_then(|i| {
                let w = &self.windows[i];
                w.buttons.iter().find(|r| r.contains(x - w.rect.x, y - w.rect.y)).map(|&r| (w.id, r))
            });
        if now == self.hover_button {
            return;
        }
        for (id, _) in [self.hover_button, now].into_iter().flatten() {
            if let Some(i) = self.windows.iter().position(|w| w.id == id) {
                self.mark_content(i);
            }
        }
        self.hover_button = now;
    }

    /// What title bar button `i` does.
    fn title_action(&mut self, i: usize, button: TitleButton) {
        match button {
            TitleButton::Close => {
                if self.windows[i].app.ask_close() {
                    self.close(i);
                } else {
                    self.mark_content(i);
                }
            }
            TitleButton::Minimize => self.minimize(i),
            TitleButton::Maximize => {
                if i + 1 == self.windows.len() {
                    self.toggle_maximize();
                }
            }
        }
    }

    /// Title bar button highlighting. Not shown while a window or something on top is being dragged.
    fn title_hover(&mut self, x: i32, y: i32) {
        let top = self.windows.iter().rposition(|w| !w.minimized && w.rect.contains(x, y));
        let free = self.drag.is_none() && self.menu.is_none();
        let now = top.filter(|_| free).and_then(|i| self.windows[i].title_button_at(x, y).map(|b| (self.windows[i].id, b)));
        if let Some(id) = self.hover_title
            && now.is_none_or(|(window, _)| window != id)
            && let Some(i) = self.windows.iter().position(|w| w.id == id)
        {
            self.windows[i].title_hover = None;
            self.mark_title(i);
        }
        if let Some((id, button)) = now
            && let Some(i) = self.windows.iter().position(|w| w.id == id)
            && self.windows[i].title_hover != Some(button)
        {
            self.windows[i].title_hover = Some(button);
            self.mark_title(i);
        }
        self.hover_title = now.map(|(id, _)| id);
    }

    /// The mouse for the glass panel. true — the event was handled by it.
    fn glass_panel_mouse(&mut self, x: i32, y: i32, pressed: bool, released: bool, right: bool) -> bool {
        if right {
            // A right click on the title opens the panel, anywhere else closes it.
            self.close_glass_panel();
            if let Some(i) = self.windows.iter().rposition(|w| !w.minimized && w.rect.contains(x, y))
                && y < self.windows[i].rect.y + TITLE_H
            {
                self.close_menu();
                self.activate(i);
                let screen = crate::output::screen_at(x, y);
                let win = self.windows.last().unwrap();
                let terminal = win.kind == "terminal";
                let height = PANEL_H + if terminal { PANEL_COLORS_H + PANEL_SCROLLBACK_H } else { 0 };
                let rect = Rect::new(
                    (x - PANEL_W / 2).clamp(screen.x + 8, screen.x + screen.w - PANEL_W - 8),
                    (y + 12).clamp(screen.y + TOPBAR_H + 4, screen.y + screen.h - height - 8),
                    PANEL_W,
                    height,
                );
                let scrollback = widgets::TextField::new(&format!("{}", theme::get().scrollback), SCROLLBACK_DIGITS);
                self.glass_panel = Some(GlassPanel { id: win.id, rect, drag: None, terminal, scrollback, typing: false, caret_on: true });
                self.mark(rect.inset(-SHADOW_PAD));
                return true;
            }
            return false;
        }
        let buttons = self.buttons;
        let Some(p) = &mut self.glass_panel else { return false };
        if let Some(n) = p.drag {
            if released {
                p.drag = None;
            } else {
                self.glass_slider(n, x);
            }
            return true;
        }
        if p.terminal {
            let field = p.scrollback_field();
            if p.scrollback.mouse(field, x, y, buttons, pressed) {
                p.typing = true;
                let panel = p.rect;
                self.mark(panel.inset(-SHADOW_PAD));
                return true;
            }
        }
        if !pressed {
            return false;
        }
        if !p.rect.contains(x, y) {
            self.close_glass_panel();
            return false;
        }
        if p.typing {
            p.typing = false;
            let panel = p.rect;
            self.mark(panel.inset(-SHADOW_PAD));
        }
        let Some(p) = &mut self.glass_panel else { return false };
        if let Some(n) = (0..2).find(|&n| p.slider(n).inset(-6).contains(x, y)) {
            p.drag = Some(n);
            self.glass_slider(n, x);
        } else if p.reset_button().contains(x, y) {
            self.update_glass(|g| *g = (None, None));
        } else if p.terminal
            && let Some(i) = (0..theme::TERM_PRESETS.len()).find(|&i| p.color_swatch(i).contains(x, y))
        {
            theme::update(|t| t.term_bg = theme::TERM_PRESETS[i].1);
        }
        true
    }

    fn close_glass_panel(&mut self) {
        if let Some(p) = self.glass_panel.take() {
            if let Some(lines) = p.scrollback_lines() {
                theme::update(|t| t.scrollback = lines);
            }
            self.mark(p.rect.inset(-SHADOW_PAD));
        }
    }

    /// A key in the panel field: Enter and Esc close the panel (Esc — without changes).
    /// While the field has focus, the window under the panel gets no keys.
    fn glass_panel_key(&mut self, ch: u8) -> bool {
        let Some(p) = self.glass_panel.as_mut().filter(|p| p.typing) else { return false };
        match ch {
            b'\n' => self.close_glass_panel(),
            0x1B => {
                p.scrollback.text.clear();
                self.close_glass_panel();
            }
            _ => {
                p.scrollback.key(ch, |c| c.is_ascii_digit());
                let panel = p.rect;
                self.mark(panel.inset(-SHADOW_PAD));
            }
        }
        true
    }

    fn glass_panel_text(&mut self, ch: char) -> bool {
        let Some(p) = self.glass_panel.as_mut().filter(|p| p.typing) else { return false };
        p.scrollback.type_char(ch, |c| c.is_ascii_digit());
        let panel = p.rect;
        self.mark(panel.inset(-SHADOW_PAD));
        true
    }

    fn glass_slider(&mut self, n: usize, x: i32) {
        let Some(p) = &self.glass_panel else { return };
        let r = p.slider(n);
        if n == 0 {
            let alpha = widgets::slider_value(r, x, MIN_ALPHA, 255);
            self.update_glass(|g| g.0 = Some(alpha));
        } else {
            let frost = widgets::slider_value(r, x, 0, theme::MAX_FROST);
            self.update_glass(|g| g.1 = Some(frost));
        }
    }

    /// Changes the glass of the program of the window the panel was opened from: for all its windows
    /// at once and permanently (in the settings file).
    fn update_glass(&mut self, f: impl FnOnce(&mut (Option<u32>, Option<u32>))) {
        let Some(p) = &self.glass_panel else { return };
        let panel = p.rect;
        let Some(win) = self.windows.iter().find(|w| w.id == p.id) else {
            self.close_glass_panel();
            return;
        };
        let key = win.glass_key();
        let mut glass = (win.alpha, win.frost);
        let before = glass;
        f(&mut glass);
        if glass == before {
            return;
        }
        if glass == (None, None) {
            self.glass_prefs.remove(&key);
        } else {
            self.glass_prefs.insert(key.clone(), glass);
        }
        for i in 0..self.windows.len() {
            if self.windows[i].glass_key() != key {
                continue;
            }
            let win = &mut self.windows[i];
            (win.alpha, win.frost) = glass;
            win.dirty = true;
            win.whole = true;
            win.forget_backdrop();
            let r = win.bounds();
            self.mark_layer(r, i);
        }
        self.mark(panel.inset(-SHADOW_PAD));
        let opt = |v: Option<u32>| v.map_or("-".into(), |v| format!("{v}"));
        let text: String = self.glass_prefs.iter().map(|(k, (a, f))| format!("{k}={},{}\n", opt(*a), opt(*f))).collect();
        let _ = fs::write(&crate::access::config(GLASS_FILE), text.as_bytes());
    }

    /// The desktop changed (under all windows): the background caches of windows above this area are stale.
    fn mark_desktop(&mut self, r: Rect) {
        self.mark(r);
        if let Some(u) = &mut self.underlay {
            u.dirty.push(r);
        }
        self.stale_above(r, 0);
    }

    fn desktop_area(&self) -> Rect {
        Rect::new(self.home.x, self.home.y + TOPBAR_H, CELL_W * 4 + 24, self.h - TOPBAR_H - DOCK_H - DOCK_MARGIN)
    }

    /// Re-reads the desktop folder and arranges the icons in columns.
    fn refresh_desktop(&mut self) {
        self.desktop_version = fs::version();
        let desktop = crate::access::desktop();
        let entries = fs::list(&desktop).unwrap_or_default();
        let (left, top) = (self.home.x + 12, self.home.y + TOPBAR_H + 14);
        let bottom = self.home.y + self.h - DOCK_H - DOCK_MARGIN - 10;
        let per_column = ((bottom - top) / CELL_H).max(1) as usize;
        let old: Vec<String> = self.desktop.iter().map(|i| i.name.clone()).collect();
        self.desktop = entries
            .into_iter()
            .enumerate()
            .map(|(n, e)| {
                let path = fs::resolve(&desktop, &e.name);
                let icon = crate::apps::ui::icon_for(&e.name, e.is_dir);
                let (col, row) = ((n / per_column) as i32, (n % per_column) as i32);
                let rect = Rect::new(left + col * CELL_W, top + row * CELL_H, CELL_W - 8, CELL_H - 6);
                DesktopIcon { name: e.name, path, icon, rect }
            })
            .collect();
        let new: Vec<String> = self.desktop.iter().map(|i| i.name.clone()).collect();
        if old != new {
            self.desktop_selected = None;
            let area = self.desktop_area();
            self.mark_desktop(area);
        }
    }

    fn open_path(&mut self, path: String) {
        let r = if fs::is_dir(&path) {
            Request::OpenFolder(path)
        } else if lua::is_program(&path) {
            Request::Run(path)
        } else {
            Request::OpenFile(path)
        };
        self.handle_request(r);
    }

    /// A click on the desktop (outside windows): selects an icon, a double click opens it.
    fn desktop_click(&mut self, x: i32, y: i32) {
        let hit = self.desktop.iter().position(|i| i.rect.contains(x, y));
        let now = interrupts::ticks();
        let double = hit.is_some() && self.desktop_click.is_some_and(|(t, i)| Some(i) == hit && now - t < HZ * 2 / 5);
        if hit != self.desktop_selected {
            for i in [hit, self.desktop_selected].into_iter().flatten() {
                let r = self.desktop[i].rect;
                self.mark_desktop(r);
            }
            self.desktop_selected = hit;
        }
        self.desktop_click = hit.map(|i| (now, i));
        if double && let Some(i) = hit {
            self.desktop_click = None;
            let path = self.desktop[i].path.clone();
            self.open_path(path);
        }
    }

    /// Notifications of different heights: the text wraps at words.
    fn toast_rect(&self, i: usize) -> Rect {
        let home = self.home;
        let above: i32 = self.toasts.iter().take(i).map(|t| toast_height(t) + 10).sum();
        let h = self.toasts.get(i).map_or(TOAST_H, toast_height);
        Rect::new(home.x + home.w - TOAST_W - 14, home.y + TOPBAR_H + 12 + above, TOAST_W, h)
    }

    fn toasts_area(&self) -> Rect {
        let n = self.toasts.len().max(1);
        self.toast_rect(0).union(self.toast_rect(n - 1)).inset(-SHADOW_PAD)
    }

    fn bell_rect(&self) -> Rect {
        Rect::new(self.right() - 18 - self.clock_w - 40, self.top() + 2, 28, 26)
    }

    fn center_rect(&self) -> Rect {
        let max_h = self.ui.get().h - TOPBAR_H - DOCK_H - DOCK_MARGIN - 30;
        let rows = self.notes.len().max(1) as i32;
        Rect::new(self.right() - CENTER_W - 10, self.top() + TOPBAR_H + 8, CENTER_W, (CENTER_HEAD + rows * NOTE_H + 10).min(max_h))
    }

    /// How many entries fit in the centre.
    fn center_rows(&self) -> usize {
        ((self.center_rect().h - CENTER_HEAD - 10) / NOTE_H).max(1) as usize
    }

    /// Entry `n` on screen (n is the index among the visible ones, top to bottom).
    fn note_rect(&self, n: usize) -> Rect {
        let c = self.center_rect();
        Rect::new(c.x + 10, c.y + CENTER_HEAD + n as i32 * NOTE_H, c.w - 20, NOTE_H - 6)
    }

    fn clear_button(&self) -> Rect {
        let c = self.center_rect();
        let w = widgets::button_width("Clear");
        Rect::new(c.x + c.w - 14 - w, c.y + 12, w, widgets::BUTTON_H)
    }

    fn toggle_center(&mut self) {
        let r = self.center_rect().inset(-SHADOW_PAD);
        self.mark(r);
        self.mark_bars();
        if self.center.take().is_none() {
            self.center = Some(0);
            self.unread = 0;
            // Popup notifications are already in the list.
            let toasts = self.toasts_area();
            self.toasts.clear();
            self.mark(toasts);
        }
    }

    /// A click while the centre is open. true — the centre took the click.
    fn center_click(&mut self, x: i32, y: i32) -> bool {
        let Some(scroll) = self.center else { return false };
        let area = self.center_rect();
        if !area.contains(x, y) {
            self.toggle_center();
            // A click on the bell only closes the centre.
            return self.bell_rect().contains(x, y);
        }
        if self.clear_button().contains(x, y) && !self.notes.is_empty() {
            self.notes.clear();
            self.center = Some(0);
        } else if let Some(n) = (0..self.center_rows()).find(|&n| self.note_rect(n).contains(x, y)) {
            // A click on an entry removes it from the history.
            let index = self.notes.len().checked_sub(1 + scroll + n);
            if let Some(i) = index {
                self.notes.remove(i);
            }
        }
        self.mark(area.inset(-SHADOW_PAD));
        true
    }

    /// New notifications go to the history and to the popup stack (if the centre is closed),
    /// expired popups are removed.
    fn update_toasts(&mut self, ticks: u64) {
        let fresh = core::mem::take(&mut *NOTIFICATIONS.lock());
        let expired = self.toasts.iter().any(|t| t.until <= ticks);
        if fresh.is_empty() && !expired {
            return;
        }
        if !fresh.is_empty() {
            let center_before = self.center_rect();
            // Of several notifications at once, the most important one sounds.
            if let Some(sound) = fresh.iter().filter_map(|n| n.0).max() {
                crate::audio::play(sound);
            }
            for (_, title, body) in &fresh {
                self.notes.push(Note { title: title.clone(), body: body.clone(), at: self.now });
            }
            if self.notes.len() > MAX_NOTES {
                self.notes.drain(..self.notes.len() - MAX_NOTES);
            }
            if self.center.is_some() {
                // The centre is open — a new one is visible in it right away.
                self.mark(center_before.inset(-SHADOW_PAD));
                self.mark(self.center_rect().inset(-SHADOW_PAD));
                return;
            }
            self.unread = (self.unread + fresh.len()).min(MAX_NOTES);
            self.mark_bars();
        }
        let before = self.toasts_area();
        self.toasts.retain(|t| t.until > ticks);
        for (_, title, body) in fresh {
            self.toasts.push(Toast { title, body, until: ticks + 4 * HZ });
        }
        if self.toasts.len() > MAX_TOASTS {
            self.toasts.drain(..self.toasts.len() - MAX_TOASTS);
        }
        let after = self.toasts_area();
        self.mark(before);
        self.mark(after);
    }

    /// The mouse wheel scrolls the window under the cursor — even if it is not focused.
    /// There are unsaved changes in at least one window.
    pub fn has_unsaved(&self) -> bool {
        self.windows.iter().any(|w| w.app.unsaved())
    }

    /// Logout or shutdown: asks the windows with unsaved changes in turn. Cancel
    /// in any of them cancels everything.
    fn continue_exit(&mut self) {
        let Some((action, asked)) = self.exit else { return };
        if let Some(w) = asked.and_then(|id| self.windows.iter().find(|w| w.id == id)) {
            if w.app.asking() {
                return;
            }
            if w.app.unsaved() {
                self.exit = None;
                return;
            }
        }
        match self.windows.iter().rposition(|w| w.app.unsaved()) {
            Some(i) => {
                let id = self.windows[i].id;
                self.activate(i);
                let top = self.windows.len() - 1;
                self.windows[top].app.ask_close();
                self.mark_content(top);
                self.exit = Some((action, Some(id)));
            }
            None => {
                self.exit = None;
                match action {
                    MenuAction::LogOut => self.leave = Some(Leave::LogOut),
                    MenuAction::Restart => crate::reboot_into(self.restart_to),
                    _ => crate::shutdown(),
                }
            }
        }
    }

    /// A reboot (after the questions about unsaved changes); `next` is what the firmware starts with.
    fn restart(&mut self, next: crate::bootvars::Next) {
        self.restart_to = next;
        self.exit = Some((MenuAction::Restart, None));
    }

    /// The user asks to lock the screen or log out.
    pub fn take_leave(&mut self) -> Option<Leave> {
        self.leave.take()
    }

    /// The screen is busy with the login: the accumulated areas will be stale before they are needed.
    pub fn skip_frame(&mut self) {
        self.damage.clear();
    }

    /// The screen is ours again: compose it entirely.
    /// The screens were rearranged: everything that depended on their sizes is recomputed.
    /// `before` — where the screens were: windows move together with their screen.
    pub fn relayout(&mut self, w: i32, h: i32, before: &[(usize, Rect)]) {
        (self.w, self.h) = (w, h);
        self.desk = crate::output::bounds();
        self.home = crate::output::primary();
        self.active = self.home;
        self.ui.set(self.active);
        self.wallpaper = wallpaper::desktop(self.wallpaper_id);
        self.underlay = None;
        self.bar_glass.borrow_mut().clear();
        self.relayout_dock();
        self.refresh_desktop();
        let placed = crate::output::placed();
        // In mirror mode the windows gather on one desktop, and afterwards they return to their screens.
        let mirrored = placed.len() > 1 && placed[0].1 == placed[1].1;
        let usable = if mirrored { 1 } else { placed.len() };
        let on_screen = |label: usize| placed[..usable].iter().find(|(l, _)| *l == label).map(|(_, s)| *s);
        let old_screen = |r: Rect| before.iter().find(|(_, s)| s.contains(r.x + r.w / 2, r.y + TITLE_H / 2)).copied();
        for i in 0..self.windows.len() {
            let window = &mut self.windows[i];
            let was = window.rect;
            let mut r = was;
            if let Some((label, old)) = old_screen(r) {
                match on_screen(label) {
                    Some(new) => r = r.offset(new.x - old.x, new.y - old.y),
                    None if window.away.is_none() => window.away = Some((label, r.offset(-old.x, -old.y))),
                    None => {}
                }
            }
            if let Some((label, place)) = window.away
                && let Some(new) = on_screen(label)
            {
                r = place.offset(new.x, new.y);
                window.away = None;
            }
            if let Some(restore) = &mut window.restore {
                *restore = restore.offset(r.x - was.x, r.y - was.y);
            }
            let (x, y) = crate::output::clamp(r.x + r.w / 2, r.y + TITLE_H / 2);
            let screen = crate::output::screen_at(x, y);
            let (nw, nh) = (r.w.min(screen.w), r.h.min(screen.h - TOPBAR_H));
            let nx = (x - nw / 2).clamp(screen.x, screen.x + screen.w - nw);
            let ny = (y - TITLE_H / 2).clamp(screen.y + TOPBAR_H, screen.y + screen.h - nh);
            self.windows[i].set_rect(Rect::new(nx, ny, nw, nh));
            self.windows[i].fit_screen();
            self.windows[i].forget_backdrop();
        }
        self.mark(self.desk);
    }

    /// The window was released: maybe it is on another screen now.
    fn fit_top_window(&mut self) {
        if self.windows.last_mut().is_some_and(Window::fit_screen) {
            self.mark(self.desk);
        }
    }

    pub fn redraw_all(&mut self) {
        self.mark(self.desk);
    }

    pub fn wheel(&mut self, x: i32, y: i32, clicks: i32) {
        if self.launcher_wheel(x, y, clicks) {
            return;
        }
        if self.calendar_wheel(x, y, clicks) {
            return;
        }
        if self.tray_wheel(x, y, clicks) {
            return;
        }
        if let Some(scroll) = self.center
            && self.center_rect().contains(x, y)
        {
            let max = self.notes.len().saturating_sub(self.center_rows());
            let now = (scroll as i32 + clicks).clamp(0, max as i32) as usize;
            if now != scroll {
                self.center = Some(now);
                self.mark(self.center_rect());
            }
            return;
        }
        if let Some(i) = self.windows.iter().rposition(|w| !w.minimized && w.rect.contains(x, y))
            && let c = self.windows[i].client()
            && self.windows[i].app.scroll_at(x - c.x, y - c.y, clicks * 3)
        {
            self.mark_content(i);
        }
    }

    /// Which cursor to show: over a window edge — the resize arrows.
    pub fn cursor(&self) -> Cursor {
        if let Some((edges, ..)) = self.resizing {
            return edge_cursor(edges);
        }
        if self.drag.is_some() || self.menu.is_some() {
            return Cursor::Arrow;
        }
        let (x, y) = self.mouse_pos;
        // The focused window may ask for its own cursor (also while its divider is dragged).
        if let Some(w) = self.windows.last()
            && !w.minimized
            && w.edges_at(x, y) == 0
        {
            let c = w.client();
            if let Some(cursor) = w.app.cursor(x - c.x, y - c.y) {
                return cursor;
            }
        }
        self.windows
            .iter()
            .rev()
            .find(|w| !w.minimized && w.rect.inset(-GRIP).contains(x, y))
            .map_or(Cursor::Arrow, |w| edge_cursor(w.edges_at(x, y)))
    }

    pub fn tick(&mut self, ctx: &Ctx) {
        let identifying = crate::output::identifying();
        if identifying != self.identifying {
            self.identifying = identifying;
            self.mark(self.desk);
        }
        let caret = ctx.ticks / (crate::interrupts::HZ / 2) % 2 == 0;
        if let Some(p) = self.glass_panel.as_mut().filter(|p| p.typing && p.caret_on != caret) {
            p.caret_on = caret;
            let panel = p.rect;
            self.mark(panel.inset(-SHADOW_PAD));
        }
        let requests = core::mem::take(&mut *REQUESTS.lock());
        for r in requests {
            self.handle_request(r);
        }
        // The theme changed — save it and redraw all windows
        // (resetting the background caches above them).
        if theme::version() != self.theme_version {
            self.theme_version = theme::version();
            self.mark(self.desk);
            crate::audio::set_volume(theme::get().sound as i32);
            crate::audio::set_input_level(theme::get().mic_volume, theme::get().mic_boost);
            if theme::get().brightness > 0 {
                crate::backlight::set(theme::get().brightness);
            }
            self.mark_tray();
            let wp = theme::get().wallpaper;
            if wp != self.wallpaper_id {
                self.wallpaper_id = wp;
                self.wallpaper = wallpaper::desktop(wp);
                let screen = self.desk;
                self.mark_desktop(screen);
            }
            let _ = fs::write(&crate::access::config(SETTINGS_FILE), theme::to_config(&theme::get()).as_bytes());
            for i in 0..self.windows.len() {
                // The default frost may have changed — rebuild the background cache entirely.
                self.windows[i].forget_backdrop();
                self.windows[i].dirty = true;
                self.windows[i].whole = true;
                let r = self.windows[i].bounds();
                self.mark_layer(r, i);
            }
        }
        // The input language changed: the indicator in the panel and the user settings.
        if crate::keyboard::version() != self.keyboard_version {
            self.keyboard_version = crate::keyboard::version();
            let _ = fs::write(&crate::access::config(crate::keyboard::CONFIG), crate::keyboard::to_config().as_bytes());
            self.mark_tray();
        }
        for i in 0..self.windows.len() {
            let t = crate::arch::rdtsc();
            let wctx = ctx.with_focus(self.windows[i].focused);
            let changed = self.windows[i].app.tick(&wctx);
            self.windows[i].cycles += crate::arch::rdtsc() - t;
            if changed {
                self.mark_content(i);
            }
            if let Some(title) = self.windows[i].app.title()
                && title != self.windows[i].title
            {
                // The title is drawn outside the client area — mark the whole window.
                self.windows[i].title = title;
                self.windows[i].dirty = true;
                self.windows[i].whole = true;
                let r = self.windows[i].rect;
                self.mark_layer(r, i);
            }
        }
        // Windows whose applications finished (a dialog answered) are closed.
        while let Some(i) = self.windows.iter().position(|w| w.app.finished()) {
            self.close(i);
        }
        self.continue_exit();
        if ctx.ticks >= self.stats_at.0 + HZ {
            self.publish_stats(ctx.ticks);
        }
        self.save_session();
        if fs::version() != self.desktop_version {
            self.refresh_desktop();
        }
        self.now = (ctx.time.hour, ctx.time.min);
        self.update_drag_pill();
        self.launcher_tick();
        self.update_toasts(ctx.ticks);
        self.tray_tick(ctx.ticks);
        let minute = (ctx.time.day as u32) << 16 | (ctx.time.hour as u32) << 8 | ctx.time.min as u32;
        if minute != self.last_min {
            self.last_min = minute;
            self.clock_w = font::measure(FontId::Regular, 14.0, &clock_text(&ctx.time));
            self.mark_bars();
        }
    }

    /// Areas under which the background must be blurred again (no ready cache).
    /// Together with the blur radius of each.
    fn glass_regions(&self) -> Vec<(Rect, i32)> {
        let mut regions: Vec<(Rect, i32)> = self
            .windows
            .iter()
            .filter(|w| !w.minimized && w.backdrop.is_none())
            .map(|w| (w.glass(), w.blur()))
            .collect();
        regions.extend(self.overlay_regions());
        regions
    }

    /// Glass panels over the windows: the top bar, the dock, menus, notifications...
    fn overlay_regions(&self) -> Vec<(Rect, i32)> {
        let mut regions = Vec::new();
        let saved = self.ui.get();
        for b in self.bars() {
            self.ui.set(b);
            regions.push((Rect::new(b.x, b.y, b.w, TOPBAR_H), BLUR));
            regions.push((self.dock_rect(), BLUR));
            regions.push((self.launcher_button(), BLUR));
        }
        self.ui.set(saved);
        regions.extend(self.launcher_regions());
        if self.menu.is_some() {
            regions.push((self.menu_rect(), BLUR));
        }
        if self.switcher.is_some() {
            regions.push((self.switcher_rect(), BLUR));
        }
        if let Some(p) = &self.glass_panel {
            regions.push((p.rect, BLUR));
        }
        if self.center.is_some() {
            regions.push((self.center_rect(), BLUR));
        }
        regions.extend((0..self.toasts.len()).map(|i| (self.toast_rect(i), BLUR)));
        regions.extend(self.tray_popups().into_iter().map(|r| (r, BLUR)));
        regions
    }

    /// Recomposes the scene in the changed areas. Returns them (for output to the screen).
    ///
    /// The areas are not merged into one rectangle: the cursor in one corner of the screen
    /// and the clock in another are two small pieces, not the whole screen.
    pub fn render(&mut self, scenes: &mut [Canvas], ctx: &Ctx) -> Vec<Rect> {
        if let Some(s) = &mut self.shot {
            if s.warmup == 0 {
                return self.render_shot(scenes);
            }
            s.warmup -= 1;
        }
        for i in 0..self.windows.len() {
            let win = &mut self.windows[i];
            if !win.dirty {
                continue;
            }
            let t = crate::arch::rdtsc();
            // Window buttons: where the cursor is (highlighting) and whether the mouse is held over them.
            let (mx, my) = self.mouse_pos;
            let local = (mx - win.rect.x, my - win.rect.y);
            let hovered = self.hover_button.is_some_and(|(id, _)| id == win.id);
            let held = self.held_button.is_some_and(|(id, r)| id == win.id && r.contains(local.0, local.1));
            // The application knows what changed (Lua windows compare frames themselves), and nothing else
            // in the window changed — redraw only this piece, without the title and
            // without hashing the whole surface: for the player's spectrum 30 times a second this is
            // tens of times cheaper.
            let damage = if win.content_changed { win.app.take_damage().filter(|d| d.w > 0 && d.h > 0) } else { None };
            let content_local = win.client_local();
            let part = damage.filter(|_| !win.whole).map(|d| d.offset(content_local.x, content_local.y));
            if part.is_none() {
                crate::apps::ui::begin(hovered.then_some(local), held);
            }
            win.render(&ctx.with_focus(win.focused), part);
            if part.is_none() {
                win.buttons = crate::apps::ui::end();
            }
            let changed = core::mem::take(&mut win.content_changed)
                && (part.is_some() || {
                    let hash = fnv(&win.surface.px);
                    let differs = hash != win.hash;
                    win.hash = hash;
                    differs
                });
            if part.is_some() {
                // The surface hash is stale; the next full redraw recomputes it.
                win.hash = 0;
            }
            win.cycles += crate::arch::rdtsc() - t;
            if changed {
                // Only the client area changes. But if there is no cache of the blurred background under
                // the glass (the window was raised, moved…), mark the whole glass:
                // the cache is built once, otherwise the background would be blurred again every frame.
                let client = win.client();
                let part = damage.map_or(client, |d| d.offset(client.x, client.y).intersect(client));
                let r = if win.backdrop.is_none() && !win.minimized { win.glass().union(client) } else { part };
                self.mark_layer(r, i);
            }
        }
        let screen = self.desk;
        if REPAINT_ALL.swap(false, core::sync::atomic::Ordering::Relaxed) {
            for w in &mut self.windows {
                w.forget_backdrop();
            }
            self.bar_glass.borrow_mut().clear();
            self.mark(screen);
        }
        // The window is being moved or resized by its edge — work on top of the backing.
        let moving = (self.drag.is_some() || self.resizing.is_some()) && !self.windows.is_empty();
        let net_painted = if core::mem::take(&mut self.net_changed) { self.paint_net_icon(scenes, moving) } else { Vec::new() };
        if !moving {
            self.underlay = None;
        } else {
            let id = self.windows.last().unwrap().id;
            if self.underlay.as_ref().is_none_or(|u| u.id != id) {
                self.underlay = Some(Underlay { id, canvases: scenes.iter().map(Canvas::twin).collect(), dirty: alloc::vec![screen] });
            }
            return self.render_moving(scenes, ctx);
        }
        let plain = core::mem::take(&mut self.damage);
        // To re-blur a stale piece of the cache, the scene under it must be composed
        // with a margin for the blur radius. Such areas are composed separately and after the
        // ordinary ones: an ordinary area may touch a stale piece only at its edge (without
        // the margin) — there the old cache lands for now, and the blur area puts the correct
        // result on top.
        let stale: Vec<Rect> = self
            .windows
            .iter()
            .filter(|w| !w.minimized && w.backdrop.is_some() && !w.stale.is_empty())
            .map(|w| w.stale.inset(-2 * w.blur()))
            .collect();
        let glass = self.glass_regions();
        let plain = Self::coalesce(plain, &glass, screen);
        let stale = Self::coalesce(stale, &glass, screen);
        for &d in &plain {
            self.compose_all(scenes, ctx, d, false, Pass::Full, None);
        }
        for &d in &stale {
            self.compose_all(scenes, ctx, d, true, Pass::Full, None);
        }
        for win in &mut self.windows {
            win.stale = Rect::new(0, 0, 0, 0);
        }
        let mut all = plain;
        all.extend(stale);
        all.extend(net_painted);
        all
    }

    /// A frame while the top window is being moved: the backing is updated only where something
    /// changed under the window, and the scene is a copy of the backing, the window and the panels.
    fn render_moving(&mut self, scenes: &mut [Canvas], ctx: &Ctx) -> Vec<Rect> {
        let screen = self.desk;
        let top = self.windows.len() - 1;
        let mut under = self.underlay.take().unwrap();
        let below = &self.windows[..top];
        let stale: Vec<Rect> = below
            .iter()
            .filter(|w| !w.minimized && w.backdrop.is_some() && !w.stale.is_empty())
            .map(|w| w.stale.inset(-2 * w.blur()))
            .collect();
        let glass: Vec<(Rect, i32)> =
            below.iter().filter(|w| !w.minimized && w.backdrop.is_none()).map(|w| (w.glass(), w.blur())).collect();
        let plain = Self::coalesce(core::mem::take(&mut under.dirty), &glass, screen);
        let stale = Self::coalesce(stale, &glass, screen);
        for &d in &plain {
            self.compose_all(&mut under.canvases, ctx, d, false, Pass::Under, None);
        }
        for &d in &stale {
            self.compose_all(&mut under.canvases, ctx, d, true, Pass::Under, None);
        }
        for win in &mut self.windows {
            win.stale = Rect::new(0, 0, 0, 0);
        }
        // Changes under the window are visible on screen too. The window's own glass is blurred
        // straight from the backing, so there is no need to extend the areas by its radius.
        let mut damage = core::mem::take(&mut self.damage);
        damage.extend(plain);
        damage.extend(stale);
        let rects = Self::coalesce(damage, &self.overlay_regions(), screen);
        for &d in &rects {
            self.compose_all(scenes, ctx, d, false, Pass::Drag, Some(&under.canvases));
        }
        self.underlay = Some(under);
        rects
    }

    /// Prepares areas for composing: clips them to the screen, extends them to the whole glass
    /// without a cache (its blur reads pixels around) and merges overlapping ones,
    /// so that each pixel is composed once.
    fn coalesce(rects: Vec<Rect>, glass: &[(Rect, i32)], screen: Rect) -> Vec<Rect> {
        let mut rects: Vec<Rect> = rects.into_iter().map(|r| r.intersect(screen)).filter(|r| !r.is_empty()).collect();
        loop {
            let mut changed = false;
            for r in rects.iter_mut() {
                let grown = glass
                    .iter()
                    .filter(|(g, _)| !g.intersect(*r).is_empty())
                    .fold(*r, |acc, &(g, blur)| acc.union(g.inset(-2 * blur)))
                    .intersect(screen);
                changed |= grown != *r;
                *r = grown;
            }
            let mut i = 0;
            while i < rects.len() {
                match (i + 1..rects.len()).find(|&j| !rects[i].intersect(rects[j]).is_empty()) {
                    Some(j) => {
                        let other = rects.swap_remove(j);
                        rects[i] = rects[i].union(other);
                        changed = true;
                    }
                    None => i += 1,
                }
            }
            if !changed {
                return rects;
            }
        }
    }

    /// Composes the scene in one area `d`. `refresh` — re-blur stale
    /// pieces of the background cache (the area then contains them together with the radius margin).
    /// `pass` — what to compose (see `Pass`); `Pass::Drag` needs the backing `under`.
    fn compose_all(&mut self, scenes: &mut [Canvas], ctx: &Ctx, d: Rect, refresh: bool, pass: Pass, under: Option<&[Canvas]>) {
        for (i, c) in scenes.iter_mut().enumerate() {
            let part = d.intersect(c.bounds());
            if !part.is_empty() {
                self.compose(c, i, ctx, part, refresh, pass, under.map(|u| &u[i]));
            }
        }
    }

    /// Composes piece `d` of the scene of screen number `index`.
    #[allow(clippy::too_many_arguments)]
    fn compose(&mut self, c: &mut Canvas, index: usize, ctx: &Ctx, d: Rect, refresh: bool, pass: Pass, under: Option<&Canvas>) {
        let screen = c.bounds();
        let top = self.windows.len().wrapping_sub(1);
        c.set_clip(d);
        // Which windows we draw: in the backing — all except the top one; on top of it — only the top one.
        let layers = match pass {
            Pass::Full => 0..self.windows.len(),
            Pass::Under => 0..top,
            Pass::Drag => top..self.windows.len(),
        };
        // If the area is entirely under the opaque part of some window, everything
        // below it is invisible — start right from that window. Its background cache
        // is not updated then, so this is only allowed if it is not stale in the area.
        let first = if pass == Pass::Drag {
            None
        } else {
            self.windows[layers.clone()]
                .iter()
                .rposition(|w| w.opaque().intersect(d) == d && !(refresh && !w.stale.intersect(d).is_empty()))
        };
        let background = match (pass, under) {
            (Pass::Drag, Some(u)) => &u.px,
            _ => &self.wallpaper[index],
        };
        let mut t = crate::arch::rdtsc();
        if first.is_none() {
            c.copy_from(background, d);
            if pass != Pass::Drag {
                self.draw_desktop(c);
            }
        }
        t = cost(0, t);
        for (i, win) in self.windows.iter_mut().enumerate() {
            if win.minimized || !layers.contains(&i) || first.is_some_and(|f| i < f) {
                continue;
            }
            let (x, y, focus) = (win.rect.x, win.rect.y, win.focused as usize);
            if first == Some(i) {
                c.composite(win.layer(c.scale).0, x, y);
                continue;
            }
            let glass_all = win.glass();
            let glass = glass_all.intersect(screen);
            let stale = if refresh { win.stale } else { Rect::new(0, 0, 0, 0) };
            let (blur, resizing) = (win.blur(), win.resizing);
            let (surface, shadows, backdrop) = win.layer(c.scale);
            // The shadow mask and the background cache are computed in the window's pixels.
            let own_scale = surface.scale == c.scale;
            if !resizing && own_scale {
                c.apply_shadow(&shadows[focus], x, y);
            }
            t = cost(1, t);
            match backdrop {
                Some((r, pixels)) if own_scale && *r == glass => {
                    let part = stale.intersect(glass).intersect(d);
                    if !part.is_empty() {
                        // Blur only the stale piece and update it in the cache.
                        c.blur(part, blur);
                        c.refresh(pixels, *r, part);
                    }
                    c.write(glass, pixels);
                }
                _ => match (pass, under) {
                    // The window is being moved: the background under it is the backing, no cache is kept (the window will move away).
                    (Pass::Drag, Some(u)) => c.blur_from(u, glass, blur),
                    _ => {
                        COMPOSE_COST[5].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        c.blur(glass, blur);
                        // The glass was rebuilt entirely — remember the blurred background. A window on the
                        // border of screens keeps no cache: it has two scenes.
                        if glass.intersect(d) == glass && glass == glass_all && own_scale {
                            *backdrop = Some((glass, c.read(glass)));
                        }
                    }
                },
            }
            t = cost(2, t);
            c.composite(surface, x, y);
            t = cost(3, t);
        }
        if pass == Pass::Under {
            c.reset_clip();
            return;
        }
        let saved = self.ui.get();
        // Panels are drawn only if the piece being composed touches them: their shadows and glass
        // are not cheap, and the piece is often one window in the middle of the screen.
        for b in self.bars() {
            self.ui.set(b);
            if Rect::new(b.x, b.y, b.w, TOPBAR_H).inset(-SHADOW_PAD).intersect(d).w > 0 {
                self.draw_topbar(c, ctx);
            }
            if self.dock_rect().inset(-SHADOW_PAD).intersect(d).w > 0 {
                self.draw_dock(c);
            }
            if self.launcher_button().inset(-SHADOW_PAD).intersect(d).w > 0 {
                self.draw_launcher_button(c);
            }
        }
        self.ui.set(saved);
        self.draw_toasts(c);
        self.draw_center(c);
        self.draw_tray_popups(c);
        self.draw_launcher_panel(c);
        self.draw_menu(c);
        self.draw_glass_panel(c);
        self.draw_switcher(c);
        self.draw_drag_pill(c);
        self.draw_screen_number(c, index);
        cost(4, t);
        c.reset_clip();
    }

    /// A large screen number: the Identify button in the settings.
    fn draw_screen_number(&self, c: &mut Canvas, index: usize) {
        if !crate::output::identifying() {
            return;
        }
        let Some(&label) = crate::output::labels().get(index) else { return };
        let r = c.bounds();
        let badge = Rect::new(r.x + r.w / 2 - IDENTIFY_SIZE / 2, r.y + r.h / 2 - IDENTIFY_SIZE / 2, IDENTIFY_SIZE, IDENTIFY_SIZE);
        c.fill_round(badge, 28, 0x101325, 230);
        c.stroke_round(badge, 28, ACCENT, 200);
        let text = format!("{label}");
        let tw = font::measure(FontId::SemiBold, IDENTIFY_FONT, &text);
        c.text(badge.x + (badge.w - tw) / 2, badge.y + 40, &text, TEXT, FontId::SemiBold, IDENTIFY_FONT);
    }

    fn draw_topbar(&self, c: &mut Canvas, ctx: &Ctx) {
        let ui = self.ui.get();
        let here = ui == self.active;
        let bar = Rect::new(ui.x, ui.y, ui.w, TOPBAR_H);
        c.blur(bar, BLUR);
        c.blend(bar, 0x0B0D17, theme::get().bar_alpha);
        c.blend(Rect::new(ui.x, ui.y + TOPBAR_H - 1, ui.w, 1), 0xFFFFFF, 18);
        self.keep_bar_glass(c, bar);
        if here && self.menu.is_some() {
            c.fill_round(self.os_button(), 7, 0xFFFFFF, 40);
        }
        c.text(ui.x + 18, ui.y + 7, OS_NAME, TEXT, FontId::SemiBold, 14.0);
        // Someone recently ran commands here over the network: this must be visible.
        if self.tray.remote {
            const LABEL: &str = "Remote";
            let x = ui.x + 18 + font::measure(FontId::SemiBold, 14.0, OS_NAME) + 12;
            let w = font::measure(FontId::SemiBold, 11.0, LABEL) + 14;
            c.fill_round(Rect::new(x, ui.y + 6, w, 18), 9, ACCENT, 200);
            c.text(x + 7, ui.y + 8, LABEL, 0x0B0D17, FontId::SemiBold, 11.0);
        }
        if here && self.calendar.is_some() {
            c.fill_round(self.clock_rect(), 7, 0xFFFFFF, 40);
        }
        let clock = clock_text(&ctx.time);
        let tw = font::measure(FontId::Regular, 14.0, &clock);
        c.text(self.right() - 18 - tw, ui.y + 7, &clock, TEXT, FontId::Regular, 14.0);
        self.draw_bell(c);
        self.draw_tray_icons(c);
    }

    /// Remembers the panel glass while nothing is drawn on it: building it again
    /// is expensive (the whole screen width must be blurred), and the network icon changes often.
    fn keep_bar_glass(&self, c: &Canvas, bar: Rect) {
        let screen = c.bounds();
        if bar.intersect(screen) != bar {
            return;
        }
        let mut kept = self.bar_glass.borrow_mut();
        kept.retain(|k| (k.screen, k.bar) != (screen, bar));
        if c.clip().intersect(bar) == bar {
            kept.push(BarGlass { screen, bar, pixels: c.read(bar) });
        }
    }

    /// Draws the network icon over the remembered glass. An icon covered by a popup
    /// or without remembered glass is composed the usual way. Returns the finished areas.
    fn paint_net_icon(&mut self, scenes: &mut [Canvas], moving: bool) -> Vec<Rect> {
        let overlays = self.overlay_regions();
        let saved = self.ui.get();
        let mut painted = Vec::new();
        for b in self.bars() {
            self.ui.set(b);
            let (icon, bar) = (self.net_icon(), Rect::new(b.x, b.y, b.w, TOPBAR_H));
            let covered = overlays.iter().any(|(g, _)| g.h != TOPBAR_H && !g.inset(-SHADOW_PAD).intersect(icon).is_empty());
            let kept = self.bar_glass.borrow();
            let mut fast = !moving && !covered;
            let targets: Vec<usize> = (0..scenes.len()).filter(|&i| !scenes[i].bounds().intersect(bar).is_empty()).collect();
            let glass = |c: &Canvas| kept.iter().find(|k| (k.screen, k.bar) == (c.bounds(), bar) && k.pixels.len() == c.device(bar).w as usize * c.device(bar).h as usize);
            fast &= targets.iter().all(|&i| glass(&scenes[i]).is_some());
            if fast {
                for &i in &targets {
                    let c = &mut scenes[i];
                    let pixels = glass(c).map_or(&[][..], |k| &k.pixels);
                    c.set_clip(icon);
                    c.write(bar, pixels);
                    self.draw_tray_icons(c);
                    c.reset_clip();
                }
                painted.push(icon);
            }
            drop(kept);
            if !fast {
                self.mark(icon);
            }
        }
        self.ui.set(saved);
        painted
    }

    /// The notification centre bell; the number of unread ones is on it.
    fn draw_bell(&self, c: &mut Canvas) {
        let b = self.bell_rect();
        if self.center.is_some() {
            c.fill_round(b, 7, 0xFFFFFF, 40);
        }
        let (x, y) = (b.x + 7, b.y + 5);
        c.fill_round(Rect::new(x + 2, y, 10, 13), 5, TEXT, 256);
        c.fill_round(Rect::new(x, y + 9, 14, 5), 2, TEXT, 256);
        c.fill_round(Rect::new(x + 5, y + 14, 4, 4), 2, TEXT, 256);
        if self.unread > 0 {
            let label = if self.unread > 9 { "9+".into() } else { format!("{}", self.unread) };
            let w = (font::measure(FontId::SemiBold, 10.0, &label) + 8).max(15);
            let badge = Rect::new(b.x + b.w - w + 2, b.y + 1, w, 15);
            c.fill_round(badge, 7, 0xE5484D, 256);
            c.text(badge.x + 4 + (w - 8 - font::measure(FontId::SemiBold, 10.0, &label)) / 2, badge.y + 1, &label, 0xFFFFFF, FontId::SemiBold, 10.0);
        }
    }

    fn draw_center(&self, c: &mut Canvas) {
        let Some(scroll) = self.center else { return };
        let r = self.center_rect();
        c.shadow(r, 14, 26, 8, 160);
        c.blur_round(r, BLUR, 14);
        c.fill_round(r, 14, 0x161A2C, theme::get().panel_alpha);
        c.stroke_round(r, 14, 0xFFFFFF, 50);
        c.text(r.x + 18, r.y + 16, "Notifications", TEXT, FontId::SemiBold, 15.0);
        widgets::button(c, self.clear_button(), "Clear", !self.notes.is_empty());
        if self.notes.is_empty() {
            let msg = "No notifications";
            let w = font::measure(FontId::Regular, 13.0, msg);
            c.text(r.x + (r.w - w) / 2, r.y + CENTER_HEAD + 18, msg, TEXT_DIM, FontId::Regular, 13.0);
            return;
        }
        let saved = c.clip();
        for (n, note) in self.notes.iter().rev().skip(scroll).take(self.center_rows()).enumerate() {
            let item = self.note_rect(n);
            c.fill_round(item, 10, 0xFFFFFF, 14);
            c.fill_round(Rect::new(item.x + 10, item.y + 12, 3, item.h - 24), 2, ACCENT, 256);
            let time = format!("{:02}:{:02}", note.at.0, note.at.1);
            let tw = font::measure(FontId::Regular, 12.0, &time);
            c.text(item.x + item.w - 12 - tw, item.y + 9, &time, TEXT_DIM, FontId::Regular, 12.0);
            c.set_clip(Rect::new(item.x, item.y, item.w - 20 - tw, item.h).intersect(saved));
            c.text(item.x + 22, item.y + 8, &note.title, TEXT, FontId::SemiBold, 13.5);
            c.set_clip(Rect::new(item.x, item.y, item.w - 10, item.h).intersect(saved));
            for (k, line) in font::wrap(FontId::Regular, 13.0, &note.body, item.w - 22 - 12, NOTE_LINES).iter().enumerate() {
                c.text(item.x + 22, item.y + 30 + k as i32 * BODY_LINE, line, TEXT_DIM, FontId::Regular, 13.0);
            }
            c.set_clip(saved);
        }
        let hidden = self.notes.len().saturating_sub(scroll + self.center_rows());
        if scroll > 0 || hidden > 0 {
            let more = format!("{} more \u{2014} scroll", scroll + hidden);
            c.text(r.x + 18, r.y + 34, &more, TEXT_DIM, FontId::Regular, 11.5);
        }
    }

    fn draw_dock(&self, c: &mut Canvas) {
        let dock = self.dock_rect();
        c.shadow(dock, 18, 20, 4, 120);
        c.blur_round(dock, BLUR, 18);
        c.fill_round(dock, 18, 0x10132A, theme::get().bar_alpha);
        c.blend(Rect::new(dock.x + 18, dock.y + 1, dock.w - 36, 1), 0xFFFFFF, 50);
        c.stroke_round(dock, 18, 0xFFFFFF, 45);
        let (dx, dy) = self.dock_shift();
        for (r, (kind, label)) in self.dock.iter().zip(LAUNCHERS) {
            let r = r.offset(dx, dy);
            c.fill_round(r, 12, 0xFFFFFF, 30);
            c.stroke_round(r, 12, 0xFFFFFF, 25);
            let tw = font::measure(FontId::Regular, 14.0, label);
            c.text(r.x + (r.w - tw) / 2, r.y + 9, label, TEXT, FontId::Regular, 14.0);
            // A dot — the application has open windows.
            if self.windows.iter().any(|w| w.kind == kind.name()) {
                c.fill_round(Rect::new(r.x + r.w / 2 - 2, r.y + r.h - 7, 4, 4), 2, ACCENT, 256);
            }
        }
        if let Some((first, _)) = self.dock_min.first() {
            let first = first.offset(dx, dy);
            c.fill(Rect::new(first.x - 9, first.y + 6, 1, first.h - 12), 0x4A5068);
        }
        for (r, id) in &self.dock_min {
            let Some(w) = self.windows.iter().find(|w| w.id == *id) else { continue };
            let r = r.offset(dx, dy);
            c.fill_round(r, 12, 0xFFFFFF, 16);
            c.stroke_round(r, 12, 0xFFFFFF, 25);
            let saved = c.clip();
            c.set_clip(r.inset(6).intersect(saved));
            c.text(r.x + 14, r.y + 10, &w.title, TEXT_DIM, FontId::Regular, 13.0);
            c.set_clip(saved);
        }
    }

    fn draw_desktop(&self, c: &mut Canvas) {
        for (n, icon) in self.desktop.iter().enumerate() {
            let r = icon.rect;
            if self.desktop_selected == Some(n) {
                c.fill_round(r, 10, 0xFFFFFF, 34);
                c.stroke_round(r, 10, 0xFFFFFF, 40);
            }
            widgets::icon(c, r.x + (r.w - 44) / 2, r.y + 8, 44, icon.icon);
            // The label: cut to the cell width, with a shadow for readability on the wallpaper.
            let mut label = icon.name.clone();
            while label.len() > 3 && font::measure(FontId::Regular, 12.5, &label) > r.w - 6 {
                label.truncate(label.len() - 4);
                label.push_str("...");
            }
            let tw = font::measure(FontId::Regular, 12.5, &label);
            let (tx, ty) = (r.x + (r.w - tw) / 2, r.y + 58);
            c.text(tx + 1, ty + 1, &label, 0x000000, FontId::Regular, 12.5);
            c.text(tx, ty, &label, TEXT, FontId::Regular, 12.5);
        }
    }

    fn draw_toasts(&self, c: &mut Canvas) {
        for (i, t) in self.toasts.iter().enumerate() {
            let r = self.toast_rect(i);
            c.shadow(r, 12, 20, 6, 140);
            c.blur_round(r, BLUR, 12);
            c.fill_round(r, 12, 0x161A2C, theme::get().panel_alpha);
            c.stroke_round(r, 12, 0xFFFFFF, 50);
            c.fill_round(Rect::new(r.x + 12, r.y + 14, 4, r.h - 28), 2, ACCENT, 256);
            let saved = c.clip();
            c.set_clip(r.inset(4).intersect(saved));
            c.text(r.x + 26, r.y + 12, &t.title, TEXT, FontId::SemiBold, 14.0);
            for (k, line) in toast_lines(&t.body).iter().enumerate() {
                c.text(r.x + 26, r.y + 34 + k as i32 * BODY_LINE, line, TEXT_DIM, FontId::Regular, 13.0);
            }
            c.set_clip(saved);
        }
    }

    fn draw_switcher(&self, c: &mut Canvas) {
        let Some(sel) = self.switcher else { return };
        let r = self.switcher_rect();
        c.shadow(r, 14, 30, 10, 170);
        c.blur_round(r, BLUR, 14);
        c.fill_round(r, 14, 0x14172A, theme::get().panel_alpha);
        c.stroke_round(r, 14, 0xFFFFFF, 50);
        c.text(r.x + 20, r.y + 14, "Switch window", TEXT_DIM, FontId::Regular, 12.0);
        for (row, &i) in self.switch_order().iter().enumerate() {
            let w = &self.windows[i];
            let item = Rect::new(r.x + 10, r.y + 38 + row as i32 * 36, r.w - 20, 32);
            if row == sel {
                c.fill_round(item, 8, ACCENT, 70);
            }
            let color = if w.minimized { TEXT_DIM } else { TEXT };
            c.text(item.x + 12, item.y + 7, &w.title, color, FontId::Regular, 14.0);
            let kind = if w.minimized { format!("{} (minimized)", w.kind) } else { w.kind.into() };
            let kw = font::measure(FontId::Regular, 12.0, &kind);
            c.text(item.x + item.w - 12 - kw, item.y + 9, &kind, TEXT_DIM, FontId::Regular, 12.0);
        }
    }

    fn draw_glass_panel(&self, c: &mut Canvas) {
        let Some(p) = &self.glass_panel else { return };
        let Some(win) = self.windows.iter().find(|w| w.id == p.id) else { return };
        let r = p.rect;
        c.shadow(r, 14, 24, 8, 150);
        c.blur_round(r, BLUR, 14);
        c.fill_round(r, 14, 0x1A1D2E, theme::get().panel_alpha);
        c.stroke_round(r, 14, 0xFFFFFF, 50);
        c.text(r.x + 18, r.y + 14, "Glass", TEXT, FontId::SemiBold, 15.0);
        let program = if win.script.is_some() { win.title.clone() } else { capitalize(win.kind) };
        c.text(r.x + 18, r.y + 36, &format!("All {program} windows"), TEXT_DIM, FontId::Regular, 12.5);
        let frost = match win.blur() {
            0 => "clear".into(),
            b => format!("{b} px"),
        };
        let rows = [("Opacity", format!("{}%", win.opacity() * 100 / 255)), ("Frost", frost)];
        for (n, (label, value)) in rows.iter().enumerate() {
            let s = p.slider(n);
            c.text(s.x, s.y - 20, label, TEXT, FontId::Regular, 13.0);
            let vw = font::measure(FontId::Regular, 13.0, value);
            c.text(s.x + s.w - vw, s.y - 20, value, TEXT_DIM, FontId::Regular, 13.0);
        }
        widgets::slider(c, p.slider(0), win.opacity(), MIN_ALPHA, 255);
        widgets::slider(c, p.slider(1), win.blur() as u32, 0, theme::MAX_FROST);
        if p.terminal {
            let background = theme::get().term_bg;
            let name = theme::TERM_PRESETS.iter().find(|t| t.1 == background).map_or("Custom", |t| t.0);
            c.text(r.x + 18, r.y + PANEL_COLORS_Y - 22, "Background", TEXT, FontId::Regular, 13.0);
            let vw = font::measure(FontId::Regular, 13.0, name);
            c.text(r.x + r.w - 18 - vw, r.y + PANEL_COLORS_Y - 22, name, TEXT_DIM, FontId::Regular, 13.0);
            for (i, &(_, color)) in theme::TERM_PRESETS.iter().enumerate() {
                widgets::swatch(c, p.color_swatch(i), color, background == color, 0x1A1D2E);
            }
            let field = p.scrollback_field();
            c.text(r.x + 18, field.y + 7, "Scrollback lines", TEXT, FontId::Regular, 13.0);
            c.fill_round(field, 7, 0x000000, 90);
            c.stroke_round(field, 7, if p.typing { ACCENT } else { 0xFFFFFF }, if p.typing { 160 } else { 30 });
            p.scrollback.draw(c, field, p.typing, p.caret_on, "", TEXT);
        }
        widgets::button(c, p.reset_button(), "Reset", win.alpha.is_some() || win.frost.is_some());
        let (start, end) = (theme::SCROLLBACK_RANGE.start(), theme::SCROLLBACK_RANGE.end());
        let (note, color) = if p.terminal && p.scrollback_lines().is_none() { (format!("Lines: {start} to {end}"), 0xF87171) } else { ("Right-click a title bar".into(), TEXT_DIM) };
        c.text(r.x + 18, p.reset_button().y + 6, &note, color, FontId::Regular, 12.0);
    }

    /// The hint at the cursor while files are dragged outside Files: over the window that will
    /// take them — "Open … in <window>".
    fn drag_pill_info(&self) -> Option<(Rect, String, bool)> {
        let label = DRAGGING.lock().clone()?;
        let (x, y) = self.mouse_pos;
        let target = self.windows.iter().rposition(|w| !w.minimized && w.rect.contains(x, y)).filter(|&i| self.windows[i].app.takes_files());
        let text = match target {
            Some(i) => format!("Open {label} in {}", self.windows[i].title),
            None => label,
        };
        let w = font::measure(FontId::Regular, 13.0, &text) + 36;
        Some((Rect::new(x + 14, y + 10, w, 26), text, target.is_some()))
    }

    fn update_drag_pill(&mut self) {
        let now = self.drag_pill_info().map(|p| p.0);
        if now != self.drag_pill {
            for r in [self.drag_pill, now].into_iter().flatten() {
                self.mark(r.inset(-SHADOW_PAD));
            }
            self.drag_pill = now;
        }
    }

    fn draw_drag_pill(&self, c: &mut Canvas) {
        let Some((pill, text, target)) = self.drag_pill_info() else { return };
        c.shadow(pill, 13, 10, 3, 140);
        c.fill_round(pill, 13, 0x232738, 245);
        c.stroke_round(pill, 13, if target { ACCENT } else { 0xFFFFFF }, if target { 200 } else { 50 });
        widgets::icon(c, pill.x + 8, pill.y + 4, 18, Icon::File);
        c.text(pill.x + 28, pill.y + 5, &text, if target { TEXT } else { TEXT_DIM }, FontId::Regular, 13.0);
    }

    fn draw_menu(&self, c: &mut Canvas) {
        let Some(hovered) = self.menu else { return };
        let m = self.menu_rect();
        c.shadow(m, 12, 24, 8, 150);
        c.blur_round(m, BLUR, 12);
        c.fill_round(m, 12, 0x1A1D2E, theme::get().menu_alpha);
        c.stroke_round(m, 12, 0xFFFFFF, 50);
        for (i, (action, label)) in menu_items().into_iter().enumerate() {
            let r = self.menu_item(i);
            let install = matches!(action, MenuAction::Install);
            if hovered == Some(i) {
                let color = match action {
                    MenuAction::Shutdown => 0xE5484D,
                    MenuAction::Install => ACCENT,
                    _ => 0x3B82F6,
                };
                c.fill_round(r, 7, color, 220);
            } else if install {
                // In the live system installing is the main thing one can do: visible right away.
                c.fill_round(r, 7, ACCENT, 50);
                c.stroke_round(r, 7, ACCENT, 140);
            }
            let (color, weight) = match (install, hovered == Some(i)) {
                (true, true) => (0x0B1020, FontId::SemiBold),
                (true, false) => (ACCENT, FontId::SemiBold),
                _ => (TEXT, FontId::Regular),
            };
            c.text(r.x + 12, r.y + 7, label, color, weight, 14.0);
            if matches!(action, MenuAction::LogOut) {
                let user = crate::access::user().unwrap_or_default();
                let w = font::measure(FontId::Regular, 12.0, &user);
                c.text(r.x + r.w - 12 - w, r.y + 9, &user, TEXT_DIM, FontId::Regular, 12.0);
            }
        }
    }
}

/// A fast hash of a window picture (FNV-1a over the pixels).
fn fnv(px: &[u32]) -> u64 {
    px.iter().fold(0xCBF2_9CE4_8422_2325, |h, &p| (h ^ p as u64).wrapping_mul(0x100_0000_01B3))
}

fn clock_text(t: &Time) -> String {
    let month = MONTHS[(t.month.clamp(1, 12) - 1) as usize];
    format!("{} {} {}    {:02}:{:02}", t.day, month, t.year, t.hour, t.min)
}

fn capitalize(s: &str) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        out.push(if i == 0 { ch.to_ascii_uppercase() } else { ch });
    }
    out
}

/// The dock layout: application buttons, then (after a separator) minimized windows.
fn dock_layout(w: i32, h: i32, minimized: &[&str]) -> (Vec<Rect>, Vec<Rect>) {
    let launch_w: Vec<i32> = LAUNCHERS.iter().map(|(_, l)| font::measure(FontId::Regular, 14.0, l) + 36).collect();
    let min_w: Vec<i32> = minimized.iter().map(|t| (font::measure(FontId::Regular, 13.0, t) + 28).min(150)).collect();
    let gap = 8;
    let sep = if minimized.is_empty() { 0 } else { 18 - gap };
    let all: Vec<i32> = launch_w.iter().chain(&min_w).copied().collect();
    let total: i32 = all.iter().sum::<i32>() + gap * (all.len() as i32 - 1) + sep;
    let mut x = (w - total) / 2;
    let y = h - DOCK_MARGIN - DOCK_H + (DOCK_H - 38) / 2;
    let mut rects = Vec::new();
    for (i, &bw) in all.iter().enumerate() {
        if i == launch_w.len() {
            x += sep;
        }
        rects.push(Rect::new(x, y, bw, 38));
        x += bw + gap;
    }
    let mins = rects.split_off(launch_w.len());
    (rects, mins)
}
