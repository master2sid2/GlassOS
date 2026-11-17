//! Screens. All of them are windows into one shared desktop: the compositor draws the whole
//! scene, and each screen shows its own part of it. Screens can be placed any way, but
//! edge to edge; the primary one has the top bar, the Dock and new windows.

use crate::font::{self, FontId};
use crate::gfx::{self, Canvas, Framebuffer, Rect};
use crate::gfx::{PERCENT, SCALE_100};
use crate::hdmi::{self, Panel, Resolution};
use crate::pci;
use crate::sync::IrqLock;
use crate::wm::Cursor;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

/// How to show the desktop on several screens.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Arrange {
    /// The screens extend each other.
    Extend,
    /// All show one desktop, and each draws it in its own pixels.
    Mirror,
    /// One screen works (index in connection order, from zero).
    Only(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub arrange: Arrange,
    /// The primary screen in extend mode.
    pub primary: usize,
    /// Where the second screen stands relative to the first. None — on the right, top-aligned.
    pub place: Option<(i32, i32)>,
    /// Each screen's scale in percent.
    pub scale: [i32; 2],
    /// The built-in screen's scale in mirror mode, in percent; 0 — the normal one. The monitor's scale
    /// is computed from it: both screens show the same desktop.
    pub mirror_scale: i32,
    /// Monitor mode; None — the native one.
    pub mode: Option<Resolution>,
}

/// Scales to choose from.
pub const SCALES: [i32; 6] = [100, 125, 133, 150, 175, 200];
const DEFAULT: Layout = Layout { arrange: Arrange::Extend, primary: 0, place: None, scale: [100; 2], mirror_scale: 0, mode: None };

const CONFIG: &str = "/system/screens.cfg";
/// A marker: the primary screen is trying the saved mode. If the computer was turned off before
/// the mode had worked for a minute, the next boot uses the firmware mode: the screen may have
/// stayed black, and there is no other way to the settings.
const TRYING: &str = "/system/screens.try";
const SETTLE_SECONDS: u64 = 60;
/// How many seconds to wait for a new layout to be confirmed.
const CONFIRM_SECONDS: u64 = 15;
const IDENTIFY_SECONDS: u64 = 3;
/// How much shared border to leave between screens: the cursor needs somewhere to cross.
const TOUCH: i32 = 200;
/// The connector must be in the new state this many checks in a row: the contact bounces.
const STEADY_CHECKS: usize = 2;
/// The margin around the cursor on the scene: it is bigger when scaled.
const CURSOR_MARGIN: i32 = 8;
/// Modes of the virtual machine's second screen.
const VIRTUAL_MODES: [Resolution; 4] = [
    Resolution { w: 1280, h: 720, hz: 60 },
    Resolution { w: 1920, h: 1080, hz: 60 },
    Resolution { w: 1600, h: 900, hz: 60 },
    Resolution { w: 1024, h: 768, hz: 60 },
];
/// Try to enable the monitor this many times in a row, then wait for a new connection.
const ATTEMPTS: usize = 3;

/// Positions of the working screens on the shared desktop, the first is the primary.
static LAYOUT: IrqLock<Vec<Rect>> = IrqLock::new(Vec::new());
/// The numbers of these screens for the user (from one).
static LABELS: IrqLock<Vec<usize>> = IrqLock::new(Vec::new());
/// All frames, including hidden ones: the shutdown message. By then the scene is gone, and
/// the pipes return to their own frames.
static FRAMES: IrqLock<Vec<Framebuffer>> = IrqLock::new(Vec::new());
/// Sizes of the connected screens in connection order: pixels and desktop units.
static SIZES: IrqLock<Vec<(i32, i32)>> = IrqLock::new(Vec::new());
static DESK_SIZES: IrqLock<Vec<(i32, i32)>> = IrqLock::new(Vec::new());
/// The working screens: pixels in width and height, scale.
static SHOWN: IrqLock<Vec<(i32, i32, i32)>> = IrqLock::new(Vec::new());
static COUNT: AtomicUsize = AtomicUsize::new(1);
static MONITOR_MODE: IrqLock<Option<Resolution>> = IrqLock::new(None);
/// The monitor was disconnected by a command: for testing without pulling the cable.
static DETACHED: AtomicBool = AtomicBool::new(false);
static CURRENT: IrqLock<Layout> = IrqLock::new(DEFAULT);
/// A request to change the layout: the main loop carries it out.
static REQUEST: IrqLock<Option<Layout>> = IrqLock::new(None);
/// The previous layout and the tick at which to return to it without confirmation.
static PENDING: IrqLock<Option<(Layout, u64)>> = IrqLock::new(None);
/// The lid: 0 — unknown (none or not a laptop), 1 — open, 2 — closed.
static LID: AtomicU8 = AtomicU8::new(0);
const LID_OPEN: u8 = 1;
const LID_CLOSED: u8 = 2;
/// The lid sensor is ignored: the state is set by a command, for testing.
static LID_FORCED: AtomicBool = AtomicBool::new(false);
/// The display is off: nothing to draw, the backlight and the monitor are off.
static DARK: AtomicBool = AtomicBool::new(false);
/// Until which tick to show the screen numbers.
static IDENTIFY_UNTIL: AtomicU64 = AtomicU64::new(0);
/// The tick after which the primary screen's saved mode counts as working (0 — not trying).
static SETTLE_AT: AtomicU64 = AtomicU64::new(0);

pub struct Screens {
    /// All connected screens: the first is the built-in one.
    list: Vec<Framebuffer>,
    /// The ones working now, the primary first, and their scenes.
    shown: Vec<Framebuffer>,
    scenes: Vec<Canvas>,
    /// The virtual machine's second screen: it is always there.
    virtual_second: Option<Virtual>,
    /// How many checks in a row the connector differs from what we think.
    steady: usize,
    attempts: usize,
    /// The monitor did not come back after sleep.
    lost: bool,
    /// Frame output still being done by the accelerator: a fence and what it writes (screen, pixels).
    pending: IrqLock<Option<(crate::accel::Fence, Vec<(usize, Rect)>)>>,
}

/// QEMU's second graphics card.
#[derive(Clone, Copy)]
struct Virtual {
    regs: usize,
    frame: Framebuffer,
    mode: Resolution,
}

impl Virtual {
    fn set_mode(&mut self, mode: Resolution) {
        const DISPI: usize = 0x500;
        const XRES: usize = 1;
        const YRES: usize = 2;
        const BPP: usize = 3;
        const ENABLE: usize = 4;
        const VIRT_WIDTH: usize = 6;
        const ENABLED_LINEAR: u16 = 0x41;
        let write = |index: usize, value: u16| unsafe { ((self.regs + DISPI + index * 2) as *mut u16).write_volatile(value) };
        write(ENABLE, 0);
        write(XRES, mode.w as u16);
        write(YRES, mode.h as u16);
        write(BPP, 32);
        write(VIRT_WIDTH, mode.w as u16);
        write(ENABLE, ENABLED_LINEAR);
        (self.frame.w, self.frame.h, self.frame.stride) = (mode.w, mode.h, mode.w);
        self.mode = mode;
    }
}

/// Screen size in desktop units.
fn desk_size(points: (i32, i32), scale: i32) -> (i32, i32) {
    let up = |v: i32| (v * SCALE_100 + scale - 1) / scale;
    (up(points.0), up(points.1))
}

impl Screens {
    pub fn new(primary: Framebuffer) -> Self {
        let mut screens = Self { list: alloc::vec![primary], shown: Vec::new(), scenes: Vec::new(), virtual_second: None, steady: 0, attempts: 0, lost: false, pending: IrqLock::new(None) };
        *SIZES.lock() = alloc::vec![(primary.w, primary.h)];
        screens.apply(current());
        screens
    }

    pub fn add(&mut self, screen: Framebuffer) {
        crate::kprintln!("output: screen {} is {}x{}", self.list.len() + 1, screen.w, screen.h);
        self.list.push(screen);
        SIZES.lock().push((screen.w, screen.h));
        COUNT.store(self.list.len(), Ordering::Relaxed);
        self.apply(current());
    }

    fn connected(&self) -> bool {
        !DETACHED.load(Ordering::Relaxed) && (self.virtual_second.is_some() || hdmi::plugged())
    }

    fn attach(&mut self) -> bool {
        let Some(screen) = self.virtual_second.map(|v| v.frame).or_else(hdmi::resume) else { return false };
        self.add(screen);
        if let Some(layout) = saved() {
            self.apply(layout);
        }
        true
    }

    fn detach(&mut self) {
        hdmi::show(false);
        self.list.truncate(1);
        SIZES.lock().truncate(1);
        COUNT.store(1, Ordering::Relaxed);
        *PENDING.lock() = None;
        crate::kprintln!("output: screen 2 is gone");
        self.apply(current());
    }

    /// Looks at the monitor connector. true — the screens changed, the scene must be rebuilt.
    pub fn watch(&mut self) -> bool {
        if core::mem::take(&mut self.lost) {
            self.detach();
            return true;
        }
        let connected = self.connected();
        if connected == (self.list.len() > 1) {
            self.steady = 0;
            if !connected {
                self.attempts = 0;
            }
            return false;
        }
        self.steady += 1;
        if self.steady < STEADY_CHECKS || (connected && self.attempts >= ATTEMPTS) {
            return false;
        }
        self.steady = 0;
        if connected {
            self.attempts += 1;
            self.attach()
        } else {
            self.detach();
            true
        }
    }

    /// Brings the screen whose mode we set — the monitor or the Bay Trail primary
    /// screen — to the layout's mode. If that fails, the layout keeps what there is.
    fn apply_mode(&mut self, layout: Layout) -> Layout {
        if crate::baytrail::active() {
            if !dark()
                && let Some(screen) = crate::baytrail::switch(layout.mode)
            {
                self.list[0] = screen;
            }
            return Layout { mode: crate::baytrail::chosen(), ..layout };
        }
        // A sleeping monitor is only turned on when the display comes back.
        if self.list.len() < 2 || dark() {
            return layout;
        }
        if let Some(screen) = &mut self.virtual_second {
            let want = layout.mode.unwrap_or(VIRTUAL_MODES[0]);
            if want != screen.mode {
                screen.set_mode(want);
            }
            self.list[1] = screen.frame;
            return layout;
        }
        match hdmi::switch(layout.mode) {
            Some(screen) => self.list[1] = screen,
            None => crate::kprintln!("output: the monitor did not come back"),
        }
        let working = hdmi::current_mode().filter(|_| layout.mode.is_some());
        Layout { mode: working, ..layout }
    }

    fn mirrored(&self, layout: Layout) -> bool {
        layout.arrange == Arrange::Mirror && self.list.len() > 1
    }

    /// The scale of screen `i` in this layout.
    fn scale_of(&self, layout: Layout, i: usize) -> i32 {
        if !self.mirrored(layout) {
            return layout.scale[i.min(1)] * PERCENT;
        }
        let panel_scale = if layout.mirror_scale == 0 { SCALE_100 } else { layout.mirror_scale * PERCENT };
        if i == 0 {
            return panel_scale;
        }
        // The panel defines the desktop. The monitor shows all of it; if its aspect ratio
        // differs, margins remain at the edges.
        let panel = self.list[0];
        let monitor = self.list[self.list.len() - 1];
        let (w, h) = desk_size((panel.w, panel.h), panel_scale);
        let fit = |points: i32, units: i32| (points * SCALE_100 + units - 1) / units;
        fit(monitor.w, w).min(fit(monitor.h, h))
    }

    /// Rearranges the screens. The main loop rebuilds the scene afterwards.
    pub fn apply(&mut self, layout: Layout) {
        // The scenes are about to be recreated: the accelerator must not read the old memory.
        self.settle();
        let chosen = self.apply_mode(layout);
        let count = self.list.len();
        // With the lid closed only the monitor works; the chosen layout does not change.
        let layout = if lid_closed() && count > 1 { Layout { arrange: Arrange::Only(count - 1), ..chosen } } else { chosen };
        *MONITOR_MODE.lock() = self.virtual_second.map(|v| v.mode).or_else(hdmi::current_mode);
        *SIZES.lock() = self.list.iter().map(|f| (f.w, f.h)).collect();
        *DESK_SIZES.lock() = self.list.iter().enumerate().map(|(i, f)| desk_size((f.w, f.h), self.scale_of(layout, i))).collect();
        let order: Vec<usize> = match layout.arrange {
            Arrange::Extend => {
                let primary = layout.primary.min(count - 1);
                core::iter::once(primary).chain((0..count).filter(|&i| i != primary)).collect()
            }
            Arrange::Mirror if count > 1 => alloc::vec![0, count - 1],
            Arrange::Mirror => alloc::vec![0],
            Arrange::Only(i) => alloc::vec![i.min(count - 1)],
        };
        // The scene starts at the top-left corner: a screen left of or above the first one shifts it.
        let second = if layout.arrange == Arrange::Extend && count > 1 { place_of(layout) } else { (0, 0) };
        let origin = (second.0.min(0), second.1.min(0));
        self.shown.clear();
        self.scenes.clear();
        let mirrored = self.mirrored(layout);
        let desk = desk_size((self.list[0].w, self.list[0].h), self.scale_of(layout, 0));
        for &i in &order {
            let screen = self.list[i];
            let scale = self.scale_of(layout, i);
            let at = if i == 0 { (0, 0) } else { second };
            let (w, h) = desk_size((screen.w, screen.h), scale);
            let (x, y) = if mirrored { (-(w - desk.0) / 2, -(h - desk.1) / 2) } else { (at.0 - origin.0, at.1 - origin.1) };
            self.shown.push(screen);
            self.scenes.push(Canvas::place(Rect::new(x, y, w, h), screen.w, screen.h, scale));
        }
        let panel = if order.contains(&0) { Panel::Own } else { Panel::Off };
        hdmi::show(order.contains(&1));
        hdmi::panel(panel);
        crate::backlight::blank(panel == Panel::Off || lid_closed() || dark());
        *CURRENT.lock() = chosen;
        *LAYOUT.lock() = self.scenes.iter().map(|c| if mirrored { Rect::new(0, 0, desk.0, desk.1) } else { c.bounds() }).collect();
        if mirrored {
            // The scene is wider than the desktop: the screen's margins stay black.
            for (screen, scene) in self.shown.iter().zip(&self.scenes) {
                screen.present(scene, scene.bounds());
            }
        }
        *SHOWN.lock() = self.scenes.iter().map(|c| (c.pw, c.ph, c.scale)).collect();
        *LABELS.lock() = order.iter().map(|i| i + 1).collect();
        *FRAMES.lock() = self.list.clone();
    }

    /// Turns the displays off. The monitor stays in the layout: windows on it do not move anywhere.
    pub fn darken(&mut self) {
        self.settle();
        DARK.store(true, Ordering::Relaxed);
        crate::backlight::blank(true);
        for (screen, scene) in self.shown.iter().zip(&mut self.scenes) {
            let all = scene.bounds();
            scene.fill(all, 0);
            screen.present(scene, all);
        }
        hdmi::sleep();
        crate::baytrail::sleep();
    }

    /// Turns the displays back on.
    pub fn brighten(&mut self) {
        DARK.store(false, Ordering::Relaxed);
        crate::baytrail::wake();
        if self.list.len() > 1 && self.virtual_second.is_none() {
            match hdmi::wake() {
                Some(screen) => self.list[1] = screen,
                // The monitor disappeared while asleep: the connector check takes care of it from here.
                None => self.lost = true,
            }
        }
        self.apply(current());
    }

    /// The working screens.
    pub fn shown(&self) -> &[Framebuffer] {
        &self.shown
    }

    /// Scenes of the working screens: the desktop draws here. The accelerator may still be reading
    /// them to output the previous frame — wait for it first.
    pub fn scenes(&mut self) -> &mut [Canvas] {
        self.settle();
        &mut self.scenes
    }

    /// Waits for the accelerator to output the frame.
    fn settle(&self) {
        let Some((fence, _)) = self.pending.lock().take() else { return };
        if let Some(a) = crate::accel::get()
            && !a.wait(fence)
        {
            crate::accel::drop_active("it stopped answering");
        }
    }

    /// Waits for the accelerator only if it writes to these pixels of screen `i`.
    fn settle_over(&self, i: usize, device: Rect) {
        let busy = self.pending.lock().as_ref().is_some_and(|(_, rects)| rects.iter().any(|&(s, r)| s == i && !r.intersect(device).is_empty()));
        if busy {
            self.settle();
        }
    }

    /// Outputs a piece of the scenes to the screens: with the accelerator if there is one and the screen's frame
    /// is visible to it (then we do not wait — the CPU is free), otherwise with the CPU.
    pub fn present(&self, r: Rect) {
        let accel = crate::accel::get();
        let mut queued = Vec::new();
        for (i, (screen, scene)) in self.shown.iter().zip(&self.scenes).enumerate() {
            let d = scene.device(r).intersect(Rect::new(0, 0, screen.w.min(scene.pw), screen.h.min(scene.ph)));
            if d.is_empty() {
                continue;
            }
            let gpu = accel.filter(|_| screen.bgr).and_then(|a| {
                let dst = crate::gpu::graphics_address(screen.base as usize)?;
                let src = a.import(scene.px.as_ptr() as usize, scene.px.len() * 4)?;
                let dst = crate::accel::Surface { addr: dst, pitch: screen.stride as u32 * 4 };
                let src = crate::accel::Surface { addr: src, pitch: scene.pw as u32 * 4 };
                let (x, y, w, h) = (d.x as u32, d.y as u32, d.w as u32, d.h as u32);
                a.copy(dst, x, y, src, x, y, w, h).then_some(())
            });
            if gpu.is_some() {
                queued.push((i, d));
            } else {
                // The CPU writes to the frame itself: the accelerator must no longer write there.
                self.settle_over(i, d);
                screen.present(scene, r);
            }
        }
        if !queued.is_empty()
            && let Some(fence) = accel.and_then(|a| a.submit())
        {
            let mut pending = self.pending.lock();
            let mut rects = pending.take().map(|(_, r)| r).unwrap_or_default();
            rects.extend(queued);
            *pending = Some((fence, rects));
        }
    }

    /// Output under the cursor: the piece is small, the CPU does it without waiting for the output
    /// of the whole frame, if the accelerator does not write here. Screens with a hardware cursor are left alone:
    /// their frame under the cursor is not damaged.
    pub fn present_small(&self, r: Rect) {
        for (i, (screen, scene)) in self.shown.iter().zip(&self.scenes).enumerate() {
            if self.hw_cursor(i) {
                continue;
            }
            self.settle_over(i, scene.device(r));
            screen.present(scene, r);
        }
    }

    /// On this screen the cursor is a display plane (Bay Trail, primary screen in our mode).
    fn hw_cursor(&self, i: usize) -> bool {
        HW_CURSOR.load(Ordering::Relaxed) && self.shown.get(i).is_some_and(|s| crate::baytrail::graphics_address(s.base as usize).is_some())
    }

    pub fn draw_cursor(&self, kind: Cursor, x: i32, y: i32) {
        for (i, (screen, scene)) in self.shown.iter().zip(&self.scenes).enumerate() {
            let zoom = (scene.scale + SCALE_100 / 2) / SCALE_100;
            let at = scene.device(Rect::new(x, y, 0, 0));
            if self.hw_cursor(i) {
                // A 64×64 image: the corner is the top-left corner of the cursor shape.
                let shape = gfx::cursor_rect(kind, 0, 0);
                let (ox, oy) = (shape.x * zoom, shape.y * zoom);
                let key = kind as u64 | (zoom as u64) << 8;
                let shown = crate::baytrail::cursor_show(key, |px| gfx::cursor_image(kind, zoom, px, crate::baytrail::CURSOR_SIZE), at.x + ox, at.y + oy);
                if shown {
                    *FOLLOW.lock() = Some(Follow { x: scene.x, y: scene.y, scale: scene.scale, ox, oy });
                    continue;
                }
                *FOLLOW.lock() = None;
                // That failed — from now on this screen draws the cursor itself.
                HW_CURSOR.store(false, Ordering::Relaxed);
                screen.present(scene, cursor_area(kind, x, y));
            }
            if !cursor_area(kind, x, y).intersect(scene.bounds()).is_empty() {
                self.settle_over(i, scene.device(cursor_area(kind, x, y)));
                gfx::draw_cursor(screen, kind, at.x, at.y, zoom);
            }
        }
    }

    /// The cursor is not needed (screensaver, screen off).
    pub fn hide_cursor(&self) {
        *FOLLOW.lock() = None;
        crate::baytrail::cursor_hide();
    }
}

/// How to place the hardware cursor at a desktop point without waiting for a frame: the origin and scale
/// of its screen's scene and the offset of the cursor image. Set by `draw_cursor`.
#[derive(Clone, Copy)]
struct Follow {
    x: i32,
    y: i32,
    scale: i32,
    ox: i32,
    oy: i32,
}

static FOLLOW: crate::sync::IrqLock<Option<Follow>> = crate::sync::IrqLock::new(None);

/// The mouse moved: the hardware cursor goes there right away (from the device thread). The
/// desktop later puts it at the same place — or changes the image.
pub fn follow_pointer(x: i32, y: i32) {
    let Some(f) = *FOLLOW.lock() else { return };
    let at = |v: i32, origin: i32| ((v - origin) * f.scale).div_euclid(SCALE_100);
    crate::baytrail::cursor_move(at(x, f.x) + f.ox, at(y, f.y) + f.oy);
}

/// The cursor as a display plane where one exists (it turns itself off if that fails).
static HW_CURSOR: AtomicBool = AtomicBool::new(true);

/// What the cursor covers on the scene.
pub fn cursor_area(kind: Cursor, x: i32, y: i32) -> Rect {
    gfx::cursor_rect(kind, x, y).inset(-CURSOR_MARGIN)
}

/// Which mode the monitor runs in (on Bay Trail — the primary screen).
pub fn monitor_mode() -> Option<Resolution> {
    if crate::baytrail::active() {
        return crate::baytrail::current_mode();
    }
    *MONITOR_MODE.lock()
}

/// Monitor modes to choose from: the native one first. On Bay Trail — modes of the primary screen,
/// the firmware mode first.
pub fn monitor_modes() -> Vec<Resolution> {
    if crate::baytrail::active() {
        return crate::baytrail::modes();
    }
    let modes = hdmi::modes();
    if modes.is_empty() { VIRTUAL_MODES.to_vec() } else { modes }
}

/// We set the primary screen's mode, not the firmware: it changes immediately, like the monitor's.
pub fn own_primary() -> bool {
    crate::baytrail::active()
}

/// Whether the computer has a lid we know about.
pub fn lid_known() -> bool {
    LID.load(Ordering::Relaxed) != 0
}

pub fn lid_closed() -> bool {
    LID.load(Ordering::Relaxed) == LID_CLOSED
}

pub fn dark() -> bool {
    DARK.load(Ordering::Relaxed)
}

fn set_lid(state: u8) {
    let was = LID.swap(state, Ordering::Relaxed);
    if was == state || (was == 0 && state == LID_OPEN) {
        return;
    }
    crate::kprintln!("output: lid {}", if state == LID_CLOSED { "closed" } else { "open" });
    // Nothing to confirm: the lid is already closed. The main loop rebuilds the layout.
    PENDING.lock().take();
    let mut request = REQUEST.lock();
    if request.is_none() {
        *request = Some(current());
    }
}

/// What the lid sensor shows.
pub fn lid_seen(open: bool) {
    if !LID_FORCED.load(Ordering::Relaxed) {
        set_lid(if open { LID_OPEN } else { LID_CLOSED });
    }
}

/// A lid for testing without a laptop: Some(open), None — listen to the sensor again.
pub fn force_lid(open: Option<bool>) {
    LID_FORCED.store(open.is_some(), Ordering::Relaxed);
    if let Some(open) = open {
        set_lid(if open { LID_OPEN } else { LID_CLOSED });
    }
}

/// How many screens are connected.
pub fn count() -> usize {
    COUNT.load(Ordering::Relaxed)
}

pub fn current() -> Layout {
    *CURRENT.lock()
}

/// Requests a new layout. If it is not confirmed, the previous one returns: the new one may have
/// left the user without a picture.
pub fn change(layout: Layout) {
    let previous = current();
    if layout == previous {
        return;
    }
    let mut pending = PENDING.lock();
    let back = pending.map_or(previous, |(p, _)| p);
    *pending = Some((back, crate::interrupts::ticks() + CONFIRM_SECONDS * crate::interrupts::HZ));
    *REQUEST.lock() = Some(layout);
}

/// The new layout works: remember it.
pub fn keep() {
    if PENDING.lock().take().is_some() {
        save(current());
    }
}

pub fn revert() {
    if let Some((back, _)) = PENDING.lock().take() {
        *REQUEST.lock() = Some(back);
    }
}

/// How many seconds are left for confirmation.
pub fn seconds_to_revert() -> Option<u64> {
    let until = (*PENDING.lock())?.1;
    Some(until.saturating_sub(crate::interrupts::ticks()).div_ceil(crate::interrupts::HZ))
}

/// The layout for boot. The primary screen's mode is taken from it only if last time
/// it worked for a minute (or the computer was shut down properly).
pub fn saved_for_boot() -> Option<Layout> {
    let layout = saved()?;
    if !own_primary() || layout.mode.is_none() {
        return Some(layout);
    }
    // The main loop only saves its own file changes: the marker is written to disk right away.
    let failed = crate::task::as_system(|| crate::fs::exists(TRYING));
    if failed {
        crate::task::as_system(|| crate::fs::remove(TRYING).ok());
        let _ = crate::fs::sync();
        crate::kprintln!("output: the saved screen mode did not settle last time, the firmware mode stays for this start");
        return Some(Layout { mode: None, ..layout });
    }
    crate::task::as_system(|| crate::fs::write(TRYING, b"1\n").ok());
    let _ = crate::fs::sync();
    SETTLE_AT.store(crate::interrupts::ticks() + SETTLE_SECONDS * crate::interrupts::HZ, Ordering::Relaxed);
    Some(layout)
}

/// The saved mode worked long enough (or the computer is shutting down itself): remove the marker.
pub fn settled() {
    if SETTLE_AT.swap(0, Ordering::Relaxed) != 0 {
        crate::task::as_system(|| crate::fs::remove(TRYING).ok());
    }
}

/// What the main loop should do: a new layout or its revert by timer.
pub fn take_request() -> Option<Layout> {
    let settle = SETTLE_AT.load(Ordering::Relaxed);
    if settle != 0 && crate::interrupts::ticks() >= settle {
        settled();
    }
    let expired = PENDING.lock().is_some_and(|(_, until)| crate::interrupts::ticks() >= until);
    if expired {
        revert();
    }
    REQUEST.lock().take()
}

pub fn identify() {
    IDENTIFY_UNTIL.store(crate::interrupts::ticks() + IDENTIFY_SECONDS * crate::interrupts::HZ, Ordering::Relaxed);
}

pub fn identifying() -> bool {
    crate::interrupts::ticks() < IDENTIFY_UNTIL.load(Ordering::Relaxed)
}

pub fn labels() -> Vec<usize> {
    LABELS.lock().clone()
}

/// The working screens: the number for the user and the place on the desktop.
pub fn placed() -> Vec<(usize, Rect)> {
    LABELS.lock().iter().copied().zip(LAYOUT.lock().iter().copied()).collect()
}

pub fn sizes() -> Vec<(i32, i32)> {
    SIZES.lock().clone()
}

/// Screen sizes in desktop units: how much they take in extend mode.
pub fn desk_sizes() -> Vec<(i32, i32)> {
    DESK_SIZES.lock().clone()
}

/// The working screens: pixels in width and height, scale.
pub fn shown() -> Vec<(i32, i32, i32)> {
    SHOWN.lock().clone()
}

/// The scale of working screen `i` in percent.
pub fn percent_of(i: usize) -> i32 {
    SHOWN.lock().get(i).map_or(100, |s| (s.2 + PERCENT / 2) / PERCENT)
}

/// Scales of the screens containing this point, larger first. In mirror mode there are two.
pub fn scales_at(x: i32, y: i32) -> Vec<i32> {
    let shown = SHOWN.lock();
    let list = LAYOUT.lock();
    let mut scales: Vec<i32> = list.iter().zip(shown.iter()).filter(|(r, _)| r.contains(x, y)).map(|(_, s)| s.2).collect();
    if scales.is_empty() {
        scales.extend(shown.first().map(|s| s.2));
    }
    scales.sort_unstable_by_key(|&s| core::cmp::Reverse(s));
    scales.dedup();
    if scales.is_empty() {
        scales.push(SCALE_100);
    }
    scales
}

/// The value snapped to the nearest mark and allowed along the first screen's side
/// of length `a`: there the edges of the second one, of length `b`, line up or are centred.
fn along(at: i32, a: i32, b: i32, pull: i32) -> i32 {
    let marks = [0, a - b, (a - b) / 2];
    let at = marks.into_iter().filter(|m| (m - at).abs() <= pull).min_by_key(|m| (m - at).abs()).unwrap_or(at);
    at.clamp(TOUCH - b, a - TOUCH)
}

/// Puts the second screen against the nearest side of the first: the cursor could not cross a gap.
/// `pull` is how far edges snap.
pub fn snap((x, y): (i32, i32), pull: i32) -> (i32, i32) {
    let sizes = DESK_SIZES.lock();
    let [(w1, h1), (w2, h2)] = sizes[..] else { return (x, y) };
    let (across_x, across_y) = (along(x, w1, w2, pull), along(y, h1, h2, pull));
    let sides = [(w1, across_y), (-w2, across_y), (across_x, h1), (across_x, -h2)];
    sides.into_iter().min_by_key(|&(sx, sy)| (sx - x).abs() + (sy - y).abs()).unwrap_or((w1, 0))
}

/// Disconnect the monitor or bring it back: like pulling and inserting the cable.
pub fn set_detached(on: bool) {
    DETACHED.store(on, Ordering::Relaxed);
}

fn place_of(layout: Layout) -> (i32, i32) {
    let right = DESK_SIZES.lock()[0].0;
    snap(layout.place.unwrap_or((right, 0)), 0)
}

/// Where the second screen stands relative to the first.
pub fn place() -> (i32, i32) {
    place_of(current())
}

fn save(layout: Layout) {
    let arrange = match layout.arrange {
        Arrange::Extend => String::from("extend"),
        Arrange::Mirror => String::from("mirror"),
        Arrange::Only(i) => format!("only{}", i + 1),
    };
    let mut text = format!("arrange={arrange}\nprimary={}\n", layout.primary + 1);
    if let Some((x, y)) = layout.place {
        text += &format!("place={x},{y}\n");
    }
    text += &format!("scale={},{}\nmirror={}\n", layout.scale[0], layout.scale[1], layout.mirror_scale);
    if let Some(m) = layout.mode {
        text += &format!("mode={},{},{}\n", m.w, m.h, m.hz);
    }
    crate::task::as_system(|| {
        let _ = crate::fs::mkdir("/system");
        let _ = crate::fs::write(CONFIG, text.as_bytes());
    });
}

/// The layout saved on disk.
pub fn saved() -> Option<Layout> {
    let data = crate::task::as_system(|| crate::fs::read(CONFIG)).ok()?;
    let text = String::from_utf8_lossy(&data);
    let value = |key: &str| text.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=')).map(str::trim);
    let arrange = match value("arrange")? {
        "mirror" => Arrange::Mirror,
        "only1" => Arrange::Only(0),
        "only2" => Arrange::Only(1),
        _ => Arrange::Extend,
    };
    let primary = value("primary").and_then(|p| p.parse::<usize>().ok()).unwrap_or(1).saturating_sub(1);
    let numbers = |key: &str| -> Vec<i32> { value(key).map_or(Vec::new(), |v| v.split(',').filter_map(|n| n.parse().ok()).collect()) };
    let place = match numbers("place")[..] {
        [x, y] => Some((x, y)),
        _ => None,
    };
    let scale = match numbers("scale")[..] {
        [a, b] => [a, b],
        _ => DEFAULT.scale,
    };
    let mode = match numbers("mode")[..] {
        [w, h, hz] => Some(Resolution { w, h, hz }),
        _ => None,
    };
    let mirror_scale = numbers("mirror").first().copied().unwrap_or(0);
    Some(Layout { arrange, primary, place, scale, mirror_scale, mode })
}

pub fn rects() -> Vec<Rect> {
    LAYOUT.lock().clone()
}

/// The primary screen on the desktop.
pub fn primary() -> Rect {
    LAYOUT.lock().first().copied().unwrap_or(Rect::new(0, 0, 0, 0))
}

/// The rectangle of the shared desktop. Corners may be empty if the screens differ in size.
pub fn bounds() -> Rect {
    let list = LAYOUT.lock();
    let w = list.iter().map(|r| r.x + r.w).max().unwrap_or(0);
    let h = list.iter().map(|r| r.y + r.h).max().unwrap_or(0);
    Rect::new(0, 0, w, h)
}

/// The point on some screen nearest to a point: the cursor does not go into empty corners.
pub fn clamp(x: i32, y: i32) -> (i32, i32) {
    let list = LAYOUT.lock();
    let nearest = |r: &Rect| (x.clamp(r.x, r.x + r.w - 1), y.clamp(r.y, r.y + r.h - 1));
    let distance = |(px, py): (i32, i32)| (px - x).abs() + (py - y).abs();
    list.iter().map(nearest).min_by_key(|&p| distance(p)).unwrap_or((x, y))
}

/// The screen containing this point (the primary one if the point is in an empty corner).
pub fn screen_at(x: i32, y: i32) -> Rect {
    let list = LAYOUT.lock();
    let inside = |r: &&Rect| x >= r.x && y >= r.y && x < r.x + r.w && y < r.y + r.h;
    list.iter().find(inside).or(list.first()).copied().unwrap_or(Rect::new(0, 0, 0, 0))
}

/// Frames of all screens in connection order (the first is primary): for a screenshot.
pub fn frames() -> Vec<Framebuffer> {
    FRAMES.lock().clone()
}

/// Blanks the screens and writes a message in the middle: shutdown takes longer than one wants to wait blindly.
pub fn announce(text: &str) {
    const TEXT_H: i32 = 60;
    const SIZE: f32 = 26.0;
    let width = font::measure(FontId::Regular, SIZE, text) + 8;
    let mut label = Canvas::new(width, TEXT_H);
    label.text(4, 12, text, crate::wm::TEXT, FontId::Regular, SIZE);
    let frames = FRAMES.lock().clone();
    for screen in frames {
        unsafe { screen.base.write_bytes(0, (screen.stride * screen.h) as usize) };
        let (left, top) = ((screen.w - width) / 2, (screen.h - TEXT_H) / 2);
        for (i, &px) in label.px.iter().enumerate() {
            let (x, y) = (i as i32 % width, i as i32 / width);
            if px & 0xFFFFFF != 0 {
                screen.put(left + x, top + y, px);
            }
        }
    }
}

/// The virtual machine's screen: QEMU's second graphics card (secondary-vga).
fn bochs(devices: &[pci::Device]) -> Option<Virtual> {
    const DISPLAY_OTHER: u8 = 0x80;
    let d = devices.iter().find(|d| (d.vendor, d.device) == (0x1234, 0x1111) && d.class == 3 && d.subclass == DISPLAY_OTHER)?;
    let regs = d.mmio(2, 4096);
    let frame = d.bar(0) as usize;
    crate::paging::map_frame(frame as u64, 16 << 20);
    let mode = VIRTUAL_MODES[0];
    let mut screen = Virtual { regs, frame: Framebuffer { base: frame as *mut u32, w: mode.w, h: mode.h, stride: mode.w, bgr: true }, mode };
    screen.set_mode(mode);
    Some(screen)
}

/// Finds connected screens other than the one the firmware already enabled. The firmware screen
/// on Bay Trail is taken by its own driver: the firmware there knows few modes.
pub fn detect(devices: &[pci::Device], screens: &mut Screens) {
    crate::baytrail::start(devices, screens.list[0]);
    if let Some(screen) = hdmi::start(devices) {
        screens.add(screen);
    } else if let Some(screen) = bochs(devices) {
        screens.virtual_second = Some(screen);
        screens.add(screen.frame);
    }
}
