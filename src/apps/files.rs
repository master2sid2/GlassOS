//! The file manager: browsing directories, creating, renaming and deleting files.
//!
//! On the left are devices (the main volume, USB sticks) and favourite places. Several files
//! can be selected: Ctrl+click, Shift+click, a mouse rectangle, Ctrl+A. Right click opens
//! the context menu. The button on the right of the toolbar splits the window into two
//! halves, as in Total Commander: files are dragged from one to the other, and
//! "Copy" / "Cut" put them into the folder of the second half (if the window is not
//! split, it is split for the duration). A background task with a progress window does the copying.
//! The sidebar edge can be dragged with the mouse.
//!
//! The Network section is the network neighbourhood (/network, see net::discovery): work
//! groups, computers, their folders. A folder is mounted with a double click or from the menu;
//! a mounted one lives in /media and comes back after a reboot.

use super::transfer::{self, Job, Mode, size_text};
use super::ui;
use crate::font::{self, FontId};
use crate::fs::{self, Entry};
use crate::gfx::{Canvas, Color, Rect};
use crate::input::{self, KEY_DELETE, KEY_DOWN, KEY_END, KEY_HOME, KEY_UP, LEFT, SHIFTED_NAV};
use crate::interrupts::{self, HZ};
use crate::lua;
use crate::net::{self, discovery, smb};
use crate::sync::IrqLock;
use crate::task;
use crate::wm::{self, ACCENT, App, Ctx, Cursor, Request, TEXT, TEXT_DIM};
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};

const BG: Color = 0x1A1D2B;
const SIDEBAR_BG: Color = 0x151826;
const TOOLBAR: [&str; 6] = ["\u{2191} Up", "New Folder", "New File", "Rename", "Delete", "Edit"];
/// The same buttons, shorter — for a narrow window (or half a window).
const TOOLBAR_SHORT: [&str; 6] = ["\u{2191}", "+ Folder", "+ File", "Rename", "Delete", "Edit"];
const LIST_Y: i32 = 82;
const ROW_H: i32 = 30;
const STATUS_H: i32 = 26;
const SIDEBAR_W: i32 = 176;
const TIP_H: i32 = 26;
/// How many ticks after hovering to show the disk usage.
const TIP_DELAY: u64 = HZ / 3;
/// Sidebar width limits when its edge is dragged.
const SIDEBAR_MIN: i32 = 120;
const SIDEBAR_MAX: i32 = 360;
/// How much to leave for the lists when the sidebar is widened.
const PANES_MIN: i32 = 300;
/// The sidebar width — shared by new windows; 0 — not read from the settings yet.
static SIDE_WIDTH: AtomicI32 = AtomicI32::new(0);
static SHOW_HIDDEN: AtomicBool = AtomicBool::new(false);
/// File manager settings (currently the sidebar width).
const CONFIG: &str = "files.cfg";

/// The sidebar width: from memory, and for the first window from the settings.
fn side_width() -> i32 {
    let w = SIDE_WIDTH.load(Ordering::Relaxed);
    if w > 0 {
        return w;
    }
    let saved = fs::read(&crate::access::config(CONFIG))
        .ok()
        .and_then(|d| {
            let text = String::from_utf8_lossy(&d).into_owned();
            text.lines().find_map(|l| l.strip_prefix("sidebar=").and_then(|v| v.trim().parse().ok()))
        })
        .unwrap_or(SIDEBAR_W)
        .clamp(SIDEBAR_MIN, SIDEBAR_MAX);
    SIDE_WIDTH.store(saved, Ordering::Relaxed);
    saved
}

fn save_side_width(w: i32) {
    SIDE_WIDTH.store(w, Ordering::Relaxed);
    save_config();
}

fn save_config() {
    let text = format!("sidebar={}\nhidden={}\n", side_width(), SHOW_HIDDEN.load(Ordering::Relaxed) as u8);
    let _ = fs::write(&crate::access::config(CONFIG), text.as_bytes());
}

/// Whether to show dot files; each user has their own value.
fn load_show_hidden() {
    let text = fs::read(&crate::access::config(CONFIG)).map(|d| String::from_utf8_lossy(&d).into_owned()).unwrap_or_default();
    SHOW_HIDDEN.store(text.lines().any(|l| l == "hidden=1"), Ordering::Relaxed);
}
/// The "two halves" button.
const SPLIT_W: i32 = 40;
/// Narrower than this the sidebar is not shown.
const SIDEBAR_MIN_CW: i32 = 520;
const DEVICE_H: i32 = 44;
const PLACE_H: i32 = 30;
/// The height of the bar with a label and buttons at the bottom of the destination half.
const PICK_FOOTER: i32 = 72;
/// How far to move the mouse for a click to become a selection rectangle.
const DRAG_START: i32 = 5;

/// A sidebar row.
struct SideItem {
    label: String,
    path: String,
    /// A device: (removable, in-memory volume, used, total); None — a favourite place.
    device: Option<(bool, bool, u64, u64)>,
    /// A network folder.
    network: bool,
    /// A network folder without a connection to the server.
    offline: bool,
}

/// Connecting to a folder in the background: (half, folder in the neighbourhood, whether we mount it, result).
type Connecting = (usize, String, bool, Arc<IrqLock<Option<Result<String, String>>>>);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    ToggleHidden,
    Open,
    Edit,
    Copy,
    Cut,
    Rename,
    Delete,
    NewFolder,
    NewFile,
    SelectAll,
    Eject(usize),
    /// The destination half: put here / new folder / cancel.
    PasteHere,
    PickNewFolder,
    CancelPick,
    ToggleSplit,
    /// The network neighbourhood.
    NetConnect,
    NetConnectAs,
    NetLogIn,
    NetRefresh,
    NetAddComputer,
    ToggleAuto(usize),
    Separator,
}

/// The list geometry of one half.
#[derive(Clone, Copy)]
struct ListArea {
    x: i32,
    w: i32,
    rows: usize,
    len: usize,
    scroll: usize,
}

impl ListArea {
    fn scrollable(&self) -> bool {
        self.bar().needed()
    }

    fn bar(&self) -> ui::ScrollBar {
        ui::ScrollBar { track: self.track(), len: self.len, rows: self.rows, top: self.scroll }
    }

    fn row_rect(&self, row: usize) -> Rect {
        let right = if self.scrollable() { ui::SCROLL_W + 10 } else { 8 };
        Rect::new(self.x + 8, LIST_Y + row as i32 * ROW_H, self.w - 8 - right, ROW_H - 2)
    }

    fn track(&self) -> Rect {
        Rect::new(self.x + self.w - ui::SCROLL_W - 5, LIST_Y, ui::SCROLL_W, self.rows as i32 * ROW_H - 2)
    }

    /// The item index under a point (None — outside the rows or an empty space).
    fn hit(&self, x: i32, y: i32) -> Option<usize> {
        let row = (0..self.rows).find(|&r| self.row_rect(r).inset(-1).contains(x, y))?;
        let i = self.scroll + row;
        (i < self.len).then_some(i)
    }
}

/// One half of the file manager: a folder, its contents, the selection and scrolling.
struct Pane {
    path: String,
    entries: Vec<Entry>,
    /// The focus (last selected) and all selected ones.
    selected: Option<usize>,
    marked: BTreeSet<usize>,
    /// Where to count the range from on Shift+click.
    anchor: Option<usize>,
    scroll: usize,
    /// The new name of the selected item while renaming.
    renaming: Option<ui::TextField>,
    /// For a double click: (row, tick of the last click).
    last_click: (usize, u64),
    /// Used and total on the folder's volume.
    usage: Option<(u64, u64)>,
    /// A folder on a network volume is read by a background thread.
    loading: Option<Loading>,
}

/// What the background thread learned about the folder.
struct Listing {
    path: String,
    exists: bool,
    entries: Vec<Entry>,
    usage: Option<(u64, u64)>,
}

type Loading = Arc<IrqLock<Option<Listing>>>;

impl Pane {
    fn new(path: String) -> Self {
        Self {
            path,
            entries: Vec::new(),
            selected: None,
            marked: BTreeSet::new(),
            anchor: None,
            scroll: 0,
            renaming: None,
            last_click: (usize::MAX, 0),
            usage: None,
            loading: None,
        }
    }

    /// Re-read the folder. A network volume answers in seconds, and during copying it is busy
    /// with whole pieces of a file: the whole desktop would wait together with this window.
    fn reload(&mut self, rows: usize) {
        if !fs::is_remote(&self.path) {
            self.read_now(rows);
        } else if self.loading.is_none() {
            let slot: Loading = Arc::new(IrqLock::new(None));
            let (done, path) = (slot.clone(), self.path.clone());
            task::spawn("files-list", None, move || {
                let exists = fs::is_dir(&path);
                let entries = if exists { fs::list(&path).unwrap_or_default() } else { Vec::new() };
                let usage = fs::usage_of(&path);
                *done.lock() = Some(Listing { path, exists, entries, usage });
            });
            self.loading = Some(slot);
        }
    }

    /// Takes the ready background list. true — the window must be redrawn.
    fn take_listing(&mut self, rows: usize) -> bool {
        let Some(listing) = self.loading.as_ref().and_then(|slot| slot.lock().take()) else { return false };
        self.loading = None;
        if listing.path != self.path {
            // While it was being read, we went to another folder.
            self.reload(rows);
            return false;
        }
        if !listing.exists && self.path != "/" {
            self.path = fs::parent(&self.path);
            self.reload(rows);
            return false;
        }
        self.show(listing.entries, listing.usage, rows);
        true
    }

    /// Reads the folder right away: a new name must be selected as soon as it appears.
    fn read_now(&mut self, rows: usize) {
        self.loading = None;
        // The folder disappeared (the stick was pulled) — go up to an existing one.
        while self.path != "/" && !fs::is_dir(&self.path) {
            self.path = fs::parent(&self.path);
        }
        let entries = fs::list(&self.path).unwrap_or_default();
        let usage = fs::usage_of(&self.path);
        self.show(entries, usage, rows);
    }

    /// Shows the folder contents, keeping the selection by name.
    fn show(&mut self, mut entries: Vec<Entry>, usage: Option<(u64, u64)>, rows: usize) {
        let marked: Vec<String> = self.marked.iter().filter_map(|&i| self.entries.get(i)).map(|e| e.name.clone()).collect();
        let focus = self.selected.and_then(|i| self.entries.get(i)).map(|e| e.name.clone());
        // Service files (settings in the home folder) do not clutter the list.
        if !SHOW_HIDDEN.load(Ordering::Relaxed) {
            entries.retain(|e| !e.name.starts_with('.'));
        }
        self.entries = entries;
        let find = |n: &String| self.entries.iter().position(|e| e.name == *n);
        self.marked = marked.iter().filter_map(find).collect();
        self.selected = focus.as_ref().and_then(find);
        self.anchor = self.anchor.filter(|&a| a < self.entries.len());
        self.scroll = self.scroll.min(self.entries.len().saturating_sub(rows));
        self.usage = usage;
    }

    fn navigate(&mut self, path: String, rows: usize) {
        self.path = path;
        self.entries.clear();
        self.usage = None;
        self.clear_selection();
        self.anchor = None;
        self.scroll = 0;
        self.renaming = None;
        self.reload(rows);
    }

    fn entry_path(&self, i: usize) -> String {
        fs::resolve(&self.path, &self.entries[i].name)
    }

    /// Paths of the selection (or of the focused item).
    fn chosen(&self) -> Vec<String> {
        if self.marked.is_empty() {
            self.selected.map(|i| self.entry_path(i)).into_iter().collect()
        } else {
            self.marked.iter().map(|&i| self.entry_path(i)).collect()
        }
    }

    fn one_chosen(&self) -> Option<usize> {
        match self.marked.len() {
            0 => self.selected,
            1 => self.marked.first().copied(),
            _ => None,
        }
    }

    fn scroll_to(&mut self, i: usize, rows: usize) {
        if i < self.scroll {
            self.scroll = i;
        } else if i >= self.scroll + rows {
            self.scroll = i + 1 - rows;
        }
    }

    /// Select only `i`.
    fn select(&mut self, i: usize, rows: usize) {
        self.selected = Some(i);
        self.anchor = Some(i);
        self.marked = BTreeSet::from([i]);
        self.scroll_to(i, rows);
    }

    /// Select the range from the anchor to `i` (Shift).
    fn select_range(&mut self, i: usize, add: bool, rows: usize) {
        let a = self.anchor.unwrap_or(i);
        if !add {
            self.marked.clear();
        }
        self.marked.extend(a.min(i)..=a.max(i));
        self.selected = Some(i);
        self.scroll_to(i, rows);
    }

    fn toggle(&mut self, i: usize) {
        if !self.marked.remove(&i) {
            self.marked.insert(i);
        }
        self.selected = Some(i);
        self.anchor = Some(i);
    }

    fn clear_selection(&mut self) {
        self.marked.clear();
        self.selected = None;
    }

    fn select_name(&mut self, name: &str, rows: usize) {
        if let Some(i) = self.entries.iter().position(|e| e.name == name) {
            self.select(i, rows);
        }
    }
}

/// "Copy" / "Cut": the files wait until a place is chosen in the second half.
struct Pending {
    mode: Mode,
    sources: Vec<String>,
    /// The half to put them into.
    dest: usize,
    /// The second half was opened for this — close it when done.
    opened: bool,
}

pub struct Files {
    cw: i32,
    ch: i32,
    panes: [Pane; 2],
    /// The half the buttons, keys and menus refer to.
    active: usize,
    /// The window is split into two halves.
    dual: bool,
    /// The sidebar width (its edge can be dragged with the mouse).
    side_width: i32,
    /// The sidebar edge is being dragged: the grab offset.
    side_drag: Option<i32>,
    /// The cursor is over the sidebar edge — highlight it.
    side_hover: bool,
    /// The device under the cursor and since which tick; whether the usage tooltip is already shown.
    device_hover: Option<(usize, u64)>,
    device_tip: bool,
    status: Option<String>,
    fs_version: u64,
    cursor_on: bool,
    side: Vec<SideItem>,
    /// The scroll thumb is being dragged: (half, grab offset from its top).
    thumb_drag: Option<(usize, i32)>,
    /// A press in the list: the point (in list content coordinates) — a rectangle may start from it
    /// (on empty space) or a file drag (on a file).
    press: Option<(i32, i32)>,
    press_item: bool,
    /// Pressed without Ctrl on one of several marked items: the selection stays (they can all be
    /// dragged), and if released without dragging — only this one remains.
    press_keep: Option<usize>,
    /// Dragging: where the cursor is and where we will drop (path, label, highlighted rectangle).
    dnd: Option<(i32, i32, Option<(String, String, Rect)>)>,
    /// The selection rectangle: the starting corner (in content coordinates) and the current point (in window coordinates);
    /// the selection before the rectangle (with Ctrl the rectangle adds to it).
    band: Option<((i32, i32), (i32, i32), BTreeSet<usize>)>,
    menu: Option<ui::Menu<Action>>,
    pending: Option<Pending>,
    job: Option<Job>,
    connecting: Option<Connecting>,
    /// A user name and password were asked: where to go after logging in (half, path, whether the
    /// folder was being mounted) and where the dialog will report success.
    after_login: Option<(usize, String, bool, Arc<IrqLock<Option<String>>>)>,
    /// When folders were last re-read.
    refreshed: u64,
}

/// A resource with a folder inside it, as in an smb:// address.
fn share_address(path: &str) -> &str {
    let (key, _) = discovery::share_of(path).unwrap_or((path, ""));
    let start = key.len() - fs::name(key).len();
    &path[start..]
}

/// A folder from the network neighbourhood.
fn in_network(path: &str) -> bool {
    discovery::depth(path).is_some()
}

/// Computers and groups themselves, not files on them.
fn above_shares(path: &str) -> bool {
    discovery::depth(path).is_some_and(|d| d < 3)
}

impl Files {
    pub fn new(path: String, cw: i32, ch: i32) -> Self {
        load_show_hidden();
        let mut files = Self {
            cw,
            ch,
            panes: [Pane::new(path), Pane::new(String::new())],
            active: 0,
            dual: false,
            side_width: side_width(),
            side_drag: None,
            side_hover: false,
            device_hover: None,
            device_tip: false,
            status: None,
            fs_version: 0,
            cursor_on: true,
            side: Vec::new(),
            thumb_drag: None,
            press: None,
            press_item: false,
            press_keep: None,
            dnd: None,
            band: None,
            menu: None,
            pending: None,
            job: None,
            connecting: None,
            after_login: None,
            refreshed: 0,
        };
        files.refresh();
        files
    }

    // ------------------------------------------------------------ layout

    /// The sidebar width (a narrow window has none).
    fn side_w(&self) -> i32 {
        if self.cw < SIDEBAR_MIN_CW {
            return 0;
        }
        self.side_width.min(self.cw - PANES_MIN).max(SIDEBAR_MIN)
    }

    fn pane_count(&self) -> usize {
        if self.dual { 2 } else { 1 }
    }

    /// Where half `i` is horizontally: (x, width). The window is split exactly in half.
    fn pane_span(&self, i: usize) -> (i32, i32) {
        let sw = self.side_w();
        let w = self.cw - sw;
        if !self.dual {
            return (sw, w);
        }
        let half = w / 2;
        if i == 0 { (sw, half) } else { (sw + half, w - half) }
    }

    fn pane_at(&self, x: i32) -> Option<usize> {
        (0..self.pane_count()).find(|&i| {
            let (px, pw) = self.pane_span(i);
            x >= px && x < px + pw
        })
    }

    /// At the bottom of half `i` — the "put here" buttons.
    fn has_footer(&self, i: usize) -> bool {
        self.pending.as_ref().is_some_and(|p| p.dest == i)
    }

    fn rows(&self, i: usize) -> usize {
        let footer = if self.has_footer(i) { PICK_FOOTER } else { 0 };
        ((self.ch - LIST_Y - STATUS_H - footer) / ROW_H).max(1) as usize
    }

    fn list(&self, i: usize) -> ListArea {
        let (x, w) = self.pane_span(i);
        let p = &self.panes[i];
        ListArea { x, w, rows: self.rows(i), len: p.entries.len(), scroll: p.scroll }
    }

    fn cur(&self) -> &Pane {
        &self.panes[self.active]
    }

    /// The "two halves" button — on the right of the toolbar.
    fn split_rect(&self) -> Rect {
        Rect::new(self.cw - 10 - SPLIT_W, 9, SPLIT_W, ui::BUTTON_H)
    }

    /// Toolbar buttons that fit to the left of the "two halves" button.
    fn toolbar(&self) -> Vec<(Rect, &'static str)> {
        let (x, right) = (self.side_w() + 10, self.split_rect().x - 8);
        let full = ui::button_row(&TOOLBAR, x, 9);
        let labels = if full.last().is_some_and(|r| r.x + r.w <= right) { TOOLBAR } else { TOOLBAR_SHORT };
        ui::button_row(&labels, x, 9).into_iter().zip(labels).filter(|(r, _)| r.x + r.w <= right).collect()
    }

    /// Buttons at the bottom of the destination half: "+ Folder", "Cancel", "Put here".
    fn pick_buttons(&self) -> Option<(Rect, Rect, Rect)> {
        let p = self.pending.as_ref()?;
        let (x, w) = self.pane_span(p.dest);
        let y = self.ch - STATUS_H - PICK_FOOTER + 36;
        let ok_label = if p.mode == Mode::Copy { "Copy here" } else { "Move here" };
        let ok_w = ui::button_width(ok_label);
        let ok = Rect::new(x + w - 10 - ok_w, y, ok_w, ui::BUTTON_H);
        let cancel_w = ui::button_width("Cancel");
        let cancel = Rect::new(ok.x - 6 - cancel_w, y, cancel_w, ui::BUTTON_H);
        let new = Rect::new(x + 10, y, ui::button_width("+ Folder"), ui::BUTTON_H);
        Some((new, cancel, ok))
    }

    /// Where the rename field is (if it is open and the row is visible).
    fn rename_rect(&self) -> Option<Rect> {
        let p = self.cur();
        p.renaming.as_ref()?;
        let row = p.selected?.checked_sub(p.scroll)?;
        let l = self.list(self.active);
        (row < l.rows).then(|| rename_box(l.row_rect(row)))
    }

    /// The cursor is on the sidebar edge (it is dragged by it).
    fn on_divider(&self, x: i32) -> bool {
        let sw = self.side_w();
        sw > 0 && (x - sw).abs() <= 3
    }

    fn side_rect(&self, i: usize) -> Rect {
        let devices = self.side.iter().filter(|s| s.device.is_some()).count();
        let y = if i < devices {
            32 + i as i32 * DEVICE_H
        } else {
            32 + devices as i32 * DEVICE_H + 30 + (i - devices) as i32 * PLACE_H
        };
        let h = if i < devices { DEVICE_H - 4 } else { PLACE_H - 4 };
        Rect::new(8, y, self.side_w() - 16, h)
    }

    /// "Connect to server" — below the favourite places.
    fn connect_rect(&self) -> Rect {
        let y = self.side.len().checked_sub(1).map_or(40, |i| {
            let r = self.side_rect(i);
            r.y + r.h + 18
        });
        Rect::new(8, y, self.side_w() - 16, 28)
    }

    /// The "eject" button of a removable device.
    fn eject_rect(&self, i: usize) -> Rect {
        let r = self.side_rect(i);
        Rect::new(r.x + r.w - 26, r.y + 6, 22, 20)
    }

    /// The active sidebar item for folder `path`: the place with the same path or
    /// the device with the longest match.
    fn side_active(&self, path: &str) -> Option<usize> {
        let under = |p: &str| path == p || p == "/" || path.starts_with(&format!("{p}/"));
        if in_network(path) {
            return self.side.iter().position(|s| s.path == fs::NETWORK);
        }
        if let Some(i) = self.side.iter().position(|s| s.device.is_none() && s.path == path) {
            return Some(i);
        }
        self.side.iter().enumerate().filter(|(_, s)| s.device.is_some() && under(&s.path)).max_by_key(|(_, s)| s.path.len()).map(|(i, _)| i)
    }

    /// The half the sidebar controls: while a place is being chosen — the destination.
    fn side_pane(&self) -> usize {
        self.pending.as_ref().map_or(self.active, |p| p.dest)
    }

    // ------------------------------------------------------------ data

    fn refresh_side(&mut self) {
        self.side = fs::volumes()
            .into_iter()
            .map(|v| SideItem { label: v.name, path: v.path, device: Some((v.removable, v.in_memory, v.used, v.total)), network: v.network, offline: v.offline })
            .collect();
        // Volumes with the same label are told apart by folder: "DATA (disk1)".
        let labels: Vec<String> = self.side.iter().map(|s| s.label.clone()).collect();
        for s in &mut self.side {
            if labels.iter().filter(|l| **l == s.label).count() > 1 {
                s.label = format!("{} ({})", s.label, fs::name(&s.path));
            }
        }
        let places = [("Home", crate::access::home()), ("Desktop", crate::access::desktop()), ("Network", fs::NETWORK.into())];
        self.side.extend(places.into_iter().map(|(label, path)| SideItem { label: label.into(), path, device: None, network: false, offline: false }));
    }

    fn refresh(&mut self) {
        self.refresh_side();
        for i in 0..self.pane_count() {
            let rows = self.rows(i);
            self.panes[i].reload(rows);
        }
        self.fs_version = fs::version();
    }

    /// The same, but the lists are ready to return: something will be selected in them right away.
    fn refresh_now(&mut self) {
        self.refresh_side();
        for i in 0..self.pane_count() {
            let rows = self.rows(i);
            self.panes[i].read_now(rows);
        }
        self.fs_version = fs::version();
    }

    fn navigate(&mut self, i: usize, path: String) {
        let rows = self.rows(i);
        self.panes[i].navigate(path, rows);
    }

    /// Make half `i` active (finish renaming in the previous one).
    fn activate(&mut self, i: usize) {
        if i != self.active {
            self.commit_rename();
            self.active = i;
        }
    }

    // ------------------------------------------------------------ two halves

    /// Splits the window. Manually, the second half opens in the same folder as the first,
    /// and for copying — in the home folder: that is usually where things go.
    fn open_split(&mut self, home: bool) {
        self.dual = true;
        self.panes[1].path = if home { crate::access::home() } else { self.panes[0].path.clone() };
        self.panes[1].selected = None;
        self.panes[1].marked.clear();
        self.panes[1].scroll = 0;
        self.refresh();
    }

    fn close_split(&mut self) {
        if self.job.is_some() {
            return;
        }
        self.commit_rename();
        self.pending = None;
        // The half that was being worked with remains.
        if self.active == 1 {
            self.panes.swap(0, 1);
            self.active = 0;
        }
        self.dual = false;
        self.thumb_drag = None;
        self.refresh();
    }

    fn toggle_split(&mut self) {
        if self.dual { self.close_split() } else { self.open_split(false) }
    }

    // ------------------------------------------------------------ actions

    fn open(&mut self, i: usize) {
        let p = self.cur();
        let path = p.entry_path(i);
        if discovery::depth(&path) == Some(3) {
            self.open_share(path);
        } else if p.entries[i].is_dir {
            self.navigate(self.active, path);
        } else if lua::is_program(&path) {
            wm::request(Request::Run(path));
        } else {
            wm::request(Request::OpenFile(path));
        }
    }

    /// Enters a resource without mounting anything: an already mounted one opens as is.
    /// If access is denied — ask for a user name and password.
    fn open_share(&mut self, path: String) {
        let Some((host, _, port)) = discovery::host_of(&path) else { return };
        if let Some(mounted) = smb::mounted(&host, port, fs::name(&path), "") {
            self.navigate(self.active, mounted);
        } else if fs::is_browsing(&path) {
            self.navigate(self.active, path);
        } else if self.connecting.is_none() {
            let target = path.clone();
            self.in_background(path, false, move || smb::browse(&target).map(|()| String::new()));
        }
    }

    /// Mounts a folder into the system: as a guest or with the remembered login.
    fn connect_share(&mut self, path: String) {
        let Some((host, ip, port)) = discovery::host_of(&path) else { return };
        let Some((key, folder)) = discovery::share_of(&path) else { return };
        let share = String::from(fs::name(key));
        if let Some(mounted) = smb::mounted(&host, port, &share, folder) {
            wm::notify("Already connected", &mounted);
            return;
        }
        if self.connecting.is_some() {
            return;
        }
        let login = smb::login_for(&host, port).unwrap_or_default();
        let name = String::from(fs::name(&path));
        let target = smb::Target { host, port, name, share, folder: folder.into(), last_ip: net::ip_str(ip), auto: true, ..login };
        self.in_background(path, true, move || smb::connect(target));
    }

    fn in_background(&mut self, path: String, mount: bool, work: impl FnOnce() -> Result<String, String> + Send + 'static) {
        self.status = None;
        let outcome = Arc::new(IrqLock::new(None));
        let slot = outcome.clone();
        task::spawn("smb-connect", None, move || {
            let r = work();
            *slot.lock() = Some(r);
        });
        self.connecting = Some((self.active, path, mount, outcome));
    }

    /// Asks for a user name and password. `retry` — where to go once logged in.
    fn ask_login(&mut self, app: super::connect::ConnectDialog, title: &str, retry: Option<(usize, String, bool)>) {
        let app = match retry {
            Some((pane, path, mount)) => {
                let slot = Arc::new(IrqLock::new(None));
                self.after_login = Some((pane, path, mount, slot.clone()));
                app.report_to(slot)
            }
            None => app,
        };
        wm::request(Request::Popup {
            title: title.into(),
            app: alloc::boxed::Box::new(app),
            w: super::connect::W,
            h: super::connect::H,
        });
    }

    /// Logged in through the dialog: go where we were going.
    fn login_done(&mut self) -> bool {
        let Some((_, _, _, slot)) = &self.after_login else { return false };
        let Some(mounted) = slot.lock().take() else { return false };
        let Some((pane, path, mount, _)) = self.after_login.take() else { return false };
        // While we were asking, the second half may have been closed.
        let pane = if self.dual { pane } else { 0 };
        self.activate(pane);
        if mount {
            if !mounted.is_empty() {
                self.navigate(pane, mounted);
            }
        } else if discovery::depth(&path) == Some(3) {
            self.open_share(path);
        } else {
            self.navigate(pane, path);
        }
        true
    }

    /// A background connection finished.
    fn connect_done(&mut self) -> bool {
        let Some((pane, path, mount, outcome)) = &self.connecting else { return false };
        let Some(r) = outcome.lock().take() else { return false };
        let (pane, path, mount) = (*pane, path.clone(), *mount);
        self.connecting = None;
        match r {
            Ok(mounted) if mount => {
                wm::notify("Network folder connected", &mounted);
                self.refresh();
            }
            Ok(_) => self.navigate(pane, path),
            Err(e) if smb::is_login_error(&e) => {
                if let Some((host, ip, port)) = discovery::host_of(&path) {
                    let retry = Some((pane, path.clone(), mount));
                    if mount {
                        self.ask_login(super::connect::ConnectDialog::for_share(&host, ip, port, share_address(&path), &e), "Connect to Folder", retry);
                    } else {
                        self.ask_login(super::connect::ConnectDialog::for_login(&host, ip, port).because(&e), "Log In", retry);
                    }
                }
            }
            Err(e) => self.status = Some(format!("Error: {e}")),
        }
        true
    }

    fn result(&mut self, r: fs::Result<()>) {
        self.status = r.err().map(|e| format!("Error: {e}"));
    }

    fn create(&mut self, dir: bool) {
        let dirpath = self.cur().path.clone();
        if above_shares(&dirpath) {
            return;
        }
        let name = fs::unique_name(&dirpath, if dir { "New Folder" } else { "untitled.txt" });
        let path = fs::resolve(&dirpath, &name);
        let r = if dir { fs::mkdir(&path) } else { fs::write(&path, b"") };
        self.result(r);
        self.refresh_now();
        let rows = self.rows(self.active);
        self.panes[self.active].select_name(&name, rows);
        // Offer to type the name right away.
        let mut field = ui::TextField::new(&name, 255);
        field.select_all();
        self.panes[self.active].renaming = Some(field);
    }

    fn delete(&mut self) {
        let paths = self.cur().chosen();
        for p in &paths {
            if let Err(e) = fs::remove(p) {
                self.status = Some(format!("Error: {}: {e}", fs::name(p)));
                break;
            }
        }
        if paths.len() > 1 && self.status.is_none() {
            self.status = Some(format!("Deleted {}", ui::items(paths.len())));
        }
        self.panes[self.active].clear_selection();
        self.refresh();
    }

    fn start_rename(&mut self) {
        let rows = self.rows(self.active);
        let p = &mut self.panes[self.active];
        if let Some(i) = p.one_chosen() {
            p.select(i, rows);
            // The name without the extension is selected — as in Windows Explorer.
            let name = &p.entries[i].name;
            let stem = match name.rfind('.') {
                Some(dot) if dot > 0 && !p.entries[i].is_dir => dot,
                _ => name.len(),
            };
            let mut field = ui::TextField::new(name, 255);
            field.size = 14.0;
            field.select_to(stem);
            p.renaming = Some(field);
        }
    }

    fn commit_rename(&mut self) {
        let p = &mut self.panes[self.active];
        let (Some(i), Some(field)) = (p.selected, p.renaming.take()) else { return };
        let name = String::from(field.text.trim());
        if name.is_empty() || name == p.entries[i].name {
            return;
        }
        let r = fs::rename(&p.entry_path(i), &fs::resolve(&p.path, &name));
        self.result(r);
        self.refresh_now();
        let rows = self.rows(self.active);
        self.panes[self.active].select_name(&name, rows);
    }

    /// "Copy" / "Cut": the place is chosen in the second half (if the window
    /// is not split — it is split for the duration).
    fn begin_pick(&mut self, mode: Mode) {
        let sources = self.cur().chosen();
        let here = &self.cur().path;
        // From a network folder that is not mounted, files can only be copied.
        if sources.is_empty() || self.job.is_some() || above_shares(here) {
            return;
        }
        let opened = !self.dual || self.pending.as_ref().is_some_and(|p| p.opened);
        self.pending = Some(Pending { mode, sources, dest: 1 - self.active, opened });
        if self.dual {
            // Both halves are already open: the second one stays where it was.
            self.refresh();
        } else {
            self.open_split(true);
        }
    }

    fn cancel_pick(&mut self) {
        if let Some(p) = self.pending.take()
            && p.opened
        {
            // The half was opened only to choose a place: stay where we copied from.
            self.active = 1 - p.dest;
            self.close_split();
        }
    }

    fn paste_here(&mut self) {
        let Some(p) = &self.pending else { return };
        match transfer::start(p.mode, p.sources.clone(), self.panes[p.dest].path.clone()) {
            Ok(job) => self.job = Some(job),
            Err(e) => self.status = Some(format!("Error: {e}")),
        }
    }

    fn run(&mut self, action: Action) {
        self.commit_rename();
        match action {
            Action::ToggleHidden => {
                SHOW_HIDDEN.fetch_xor(true, Ordering::Relaxed);
                save_config();
                // Open Files windows will see this and re-read their folders.
                fs::touch();
            }
            Action::Open => {
                if let Some(i) = self.cur().selected {
                    self.open(i);
                }
            }
            Action::Edit => {
                // Open in the editor — including a program, instead of running it.
                let p = self.cur();
                if let Some(i) = p.one_chosen().filter(|&i| !p.entries[i].is_dir) {
                    wm::request(Request::OpenFile(p.entry_path(i)));
                }
            }
            Action::Copy => self.begin_pick(Mode::Copy),
            Action::Cut => self.begin_pick(Mode::Move),
            Action::Rename => self.start_rename(),
            Action::Delete => self.delete(),
            Action::NewFolder => self.create(true),
            Action::NewFile => self.create(false),
            Action::SelectAll => {
                let p = &mut self.panes[self.active];
                p.marked = (0..p.entries.len()).collect();
            }
            Action::Eject(i) => self.eject(i),
            Action::PasteHere => self.paste_here(),
            Action::PickNewFolder => {
                if let Some(p) = &self.pending {
                    let dir = self.panes[p.dest].path.clone();
                    let name = fs::unique_name(&dir, "New Folder");
                    let r = fs::mkdir(&fs::resolve(&dir, &name));
                    self.result(r);
                    self.refresh();
                }
            }
            Action::CancelPick => self.cancel_pick(),
            Action::ToggleSplit => self.toggle_split(),
            Action::NetConnect | Action::NetConnectAs | Action::NetLogIn => {
                let p = self.cur();
                let path = p.selected.map_or_else(|| p.path.clone(), |i| p.entry_path(i));
                let Some((host, ip, port)) = discovery::host_of(&path) else { return };
                match action {
                    Action::NetConnect => self.connect_share(path),
                    Action::NetConnectAs => {
                        let retry = Some((self.active, path.clone(), true));
                        self.ask_login(super::connect::ConnectDialog::for_share(&host, ip, port, share_address(&path), ""), "Connect to Folder", retry)
                    }
                    _ => {
                        let retry = Some((self.active, path.clone(), false));
                        self.ask_login(super::connect::ConnectDialog::for_login(&host, ip, port), "Log In", retry)
                    }
                }
            }
            Action::NetRefresh => {
                let p = self.cur();
                let path = p.selected.map_or_else(|| p.path.clone(), |i| p.entry_path(i));
                discovery::refresh(&path);
            }
            Action::NetAddComputer => {
                self.ask_login(super::connect::ConnectDialog::new(), "Connect to Server", None);
            }
            Action::ToggleAuto(i) => {
                let path = self.side[i].path.clone();
                smb::set_auto(&path, !smb::is_auto(&path));
            }
            Action::Separator => {}
        }
    }

    fn toolbar_action(&mut self, i: usize) {
        self.commit_rename();
        match i {
            0 => {
                if self.cur().path != "/" {
                    let up = fs::parent(&self.cur().path);
                    self.navigate(self.active, up);
                }
            }
            1 => self.create(true),
            2 => self.create(false),
            3 => self.start_rename(),
            4 => self.delete(),
            _ => self.run(Action::Edit),
        }
    }

    fn eject(&mut self, i: usize) {
        let path = self.side[i].path.clone();
        let label = self.side[i].label.clone();
        let network = self.side[i].network;
        match fs::eject(&path) {
            Ok(()) => {
                if network {
                    wm::notify("Network folder disconnected", &label);
                } else {
                    wm::notify("Safe to remove", &format!("{label} can be unplugged now"));
                }
                for k in 0..self.pane_count() {
                    let p = &self.panes[k].path;
                    if *p == path || p.starts_with(&format!("{path}/")) {
                        self.navigate(k, "/".into());
                    }
                }
                self.refresh();
            }
            Err(e) => self.status = Some(format!("Eject failed: {e}")),
        }
    }

    // ------------------------------------------------------------ mouse

    /// The row "in list content coordinates" (with scrolling) for a window coordinate.
    fn content_y(&self, y: i32) -> i32 {
        y - LIST_Y + self.cur().scroll as i32 * ROW_H
    }

    /// The selection rectangle: the rows it touches are selected.
    fn update_band(&mut self, x: i32, y: i32) {
        let Some((start, _, base)) = self.band.take() else { return };
        // At the edge of the list — scroll, so the rectangle can be dragged further.
        let l = self.list(self.active);
        let p = &mut self.panes[self.active];
        if y < LIST_Y && p.scroll > 0 {
            p.scroll -= 1;
        } else if y > LIST_Y + l.rows as i32 * ROW_H && p.scroll + l.rows < l.len {
            p.scroll += 1;
        }
        let (a, b) = (start.1, y - LIST_Y + p.scroll as i32 * ROW_H);
        let (top, bottom) = (a.min(b).max(0), a.max(b).max(0));
        let rows = (top / ROW_H) as usize..=(bottom / ROW_H) as usize;
        let mut marked = base.clone();
        marked.extend(rows.filter(|&i| i < p.entries.len()));
        p.marked = marked;
        p.selected = p.marked.last().copied();
        self.band = Some((start, (x, y), base));
    }

    fn list_press(&mut self, x: i32, y: i32) -> bool {
        let mods = input::mods();
        let (ctrl, shift) = (mods & input::MOD_CTRL != 0, mods & input::MOD_SHIFT != 0);
        let l = self.list(self.active);
        let rows = l.rows;
        self.press = Some((x, self.content_y(y)));
        self.press_item = l.hit(x, y).is_some();
        let Some(i) = l.hit(x, y) else {
            // Empty space: clear the selection (with Ctrl — keep it, the rectangle will add).
            self.commit_rename();
            if !ctrl {
                self.panes[self.active].clear_selection();
            }
            return true;
        };
        if self.cur().renaming.is_some() && self.cur().selected != Some(i) {
            self.commit_rename();
        }
        let p = &mut self.panes[self.active];
        if shift {
            p.select_range(i, ctrl, rows);
            return true;
        }
        if ctrl {
            p.toggle(i);
            return true;
        }
        let now = interrupts::ticks();
        let double = p.last_click.0 == i && now - p.last_click.1 < HZ * 2 / 5;
        p.last_click = (i, now);
        if p.marked.len() > 1 && p.marked.contains(&i) && !double {
            p.selected = Some(i);
            self.press_keep = Some(i);
            return true;
        }
        p.select(i, rows);
        if double && p.renaming.is_none() {
            p.last_click = (usize::MAX, 0);
            self.press = None;
            self.open(i);
        }
        true
    }

    fn sidebar_click(&mut self, x: i32, y: i32) -> bool {
        self.commit_rename();
        if self.connect_rect().contains(x, y) {
            wm::request(Request::Popup {
                title: "Connect to Server".into(),
                app: alloc::boxed::Box::new(super::connect::ConnectDialog::new()),
                w: super::connect::W,
                h: super::connect::H,
            });
            return true;
        }
        let Some(i) = (0..self.side.len()).find(|&i| self.side_rect(i).contains(x, y)) else { return false };
        if self.side[i].device.is_some_and(|d| d.0) && self.eject_rect(i).inset(-2).contains(x, y) {
            self.eject(i);
            return true;
        }
        let path = self.side[i].path.clone();
        if !fs::is_remote(&path) && !fs::is_dir(&path) {
            let _ = fs::mkdir(&path);
        }
        self.navigate(self.side_pane(), path);
        true
    }

    fn open_menu(&mut self, x: i32, y: i32, items: Vec<(&'static str, Action, bool)>) {
        self.menu = Some(ui::Menu::new(x, y, items, self.cw, self.ch));
    }

    // ------------------------------------------------------------ dragging

    /// Where the files will land if released here: (folder, label, what to highlight).
    /// The target is a sidebar item, a folder in either half or the whole other half.
    fn drop_target(&self, x: i32, y: i32) -> Option<(String, String, Rect)> {
        let label = |p: &str| if p == "/" { String::from("System") } else { String::from(fs::name(p)) };
        let found = if x < self.side_w() {
            let i = (0..self.side.len()).find(|&i| self.side_rect(i).contains(x, y))?;
            (self.side[i].path.clone(), self.side[i].label.clone(), self.side_rect(i))
        } else {
            let k = self.pane_at(x)?;
            let (l, p) = (self.list(k), &self.panes[k]);
            let dragged = |i: &usize| k == self.active && (p.marked.contains(i) || p.selected == Some(*i));
            match l.hit(x, y).filter(|i| p.entries[*i].is_dir && !dragged(i)) {
                Some(i) => (fs::resolve(&p.path, &p.entries[i].name), p.entries[i].name.clone(), l.row_rect(i - p.scroll)),
                None if k != self.active && y >= LIST_Y - 40 => {
                    (p.path.clone(), label(&p.path), Rect::new(l.x + 4, LIST_Y, l.w - 8, l.rows as i32 * ROW_H))
                }
                None => return None,
            }
        };
        // Into the same folder or into itself — nowhere to go.
        let sources = self.cur().chosen();
        let bad = found.0 == self.cur().path || sources.iter().any(|s| found.0 == *s || found.0.starts_with(&format!("{s}/")));
        (!bad).then_some(found)
    }

    /// Move or copy on drop: to another disk or with Ctrl — a copy.
    fn drop_mode(&self, target: &str) -> Mode {
        let first = self.cur().chosen().into_iter().next().unwrap_or_default();
        if input::mods() & input::MOD_CTRL != 0 || !fs::same_volume(&first, target) { Mode::Copy } else { Mode::Move }
    }

    fn drop_on(&mut self, target: String) {
        if self.job.is_some() {
            return;
        }
        let mode = self.drop_mode(&target);
        match transfer::start(mode, self.cur().chosen(), target) {
            Ok(job) => self.job = Some(job),
            Err(e) => self.status = Some(format!("Error: {e}")),
        }
    }

    /// What is being dragged: "name" or "N items".
    fn dnd_what(&self) -> String {
        let chosen = self.cur().chosen();
        if chosen.len() == 1 { format!("\u{201C}{}\u{201D}", fs::name(&chosen[0])) } else { format!("{} items", chosen.len()) }
    }

    fn outside(&self, x: i32, y: i32) -> bool {
        x < 0 || y < 0 || x >= self.cw || y >= self.ch
    }

    /// The drag moved: beyond the window edge the WM draws the hint at the cursor.
    fn dnd_moved(&mut self, x: i32, y: i32) {
        let target = self.drop_target(x, y);
        self.dnd = Some((x, y, target));
        wm::drag_label(self.outside(x, y).then(|| self.dnd_what()));
    }

    fn draw_dnd(&self, c: &mut Canvas, a: Rect) {
        let Some((x, y, target)) = &self.dnd else { return };
        if self.outside(*x, *y) {
            return;
        }
        let what = self.dnd_what();
        let text = match target {
            Some((path, label, r)) => {
                c.stroke_round(r.offset(a.x, a.y), 8, ACCENT, 230);
                c.fill_round(r.offset(a.x, a.y), 8, ACCENT, 40);
                let verb = if self.drop_mode(path) == Mode::Copy { "Copy" } else { "Move" };
                format!("{verb} {what} to {label}")
            }
            None => what,
        };
        let tw = font::measure(FontId::Regular, 13.0, &text);
        // The hint at the cursor, but not beyond the window's right edge.
        let px = (x + 14).min(self.cw - tw - 40).max(4);
        let pill = Rect::new(a.x + px, a.y + y + 10, tw + 36, 26);
        c.shadow(pill, 13, 10, 3, 140);
        c.fill_round(pill, 13, 0x232738, 245);
        c.stroke_round(pill, 13, if target.is_some() { ACCENT } else { 0xFFFFFF }, if target.is_some() { 200 } else { 50 });
        ui::file_icon(c, pill.x + 8, pill.y + 4);
        c.text(pill.x + 28, pill.y + 5, &text, TEXT, FontId::Regular, 13.0);
    }

    // ------------------------------------------------------------ drawing

    /// A list row. `mark` is the opacity of the selection highlight (0 — none),
    /// `ring` — outline it (the focus among several selected).
    fn draw_row(&self, c: &mut Canvas, r: Rect, dir: &str, e: &Entry, mark: u32, ring: bool, renaming: Option<&ui::TextField>) {
        let saved = c.clip();
        if mark > 0 {
            c.fill_round(r, 7, ACCENT, mark);
        }
        if ring {
            c.stroke_round(r, 7, ACCENT, 140);
        }
        // Inside a resource — ordinary folders and files.
        let place = discovery::depth(dir).filter(|&d| d < 3);
        if matches!(place, Some(1)) {
            computer_icon(c, r.x + 10, r.y + 5);
        } else if matches!(place, Some(0)) {
            net_icon(c, r.x + 10, r.y + 4);
        } else if e.is_dir {
            ui::folder_icon(c, r.x + 10, r.y + 5);
        } else {
            ui::icon(c, r.x + 10, r.y + 4, 20, ui::icon_for(&e.name, false));
        }
        let tx = r.x + 42;
        let info = match place {
            Some(2) => {
                let connected = discovery::host_of(dir).is_some_and(|(h, _, p)| smb::mounted(&h, p, &e.name, "").is_some());
                String::from(if connected { "Connected" } else { "" })
            }
            Some(_) => discovery::info(&fs::resolve(dir, &e.name)),
            None if e.is_dir => ui::items(e.size),
            None => size_text(e.size as u64),
        };
        let iw = font::measure(FontId::Regular, 12.0, &info);
        let show_info = r.w > 200;
        match renaming {
            Some(field) => {
                let b = rename_box(r);
                c.fill_round(b, 6, 0x0E1018, 230);
                c.stroke_round(b, 6, ACCENT, 200);
                field.draw(c, b, true, self.cursor_on, "", TEXT);
            }
            None => {
                let room = r.w - (tx - r.x) - if show_info { iw + 20 } else { 8 };
                c.set_clip(Rect::new(tx, r.y, room, r.h).intersect(saved));
                c.text(tx, r.y + 6, &e.name, TEXT, FontId::Regular, 14.0);
                c.set_clip(saved);
            }
        }
        if show_info {
            c.text(r.x + r.w - 12 - iw, r.y + 7, &info, TEXT_DIM, FontId::Regular, 12.0);
        }
    }

    fn path_bar(c: &mut Canvas, r: Rect, path: &str, active: bool) {
        c.fill_round(r, 8, 0x000000, 70);
        if active {
            c.stroke_round(r, 8, ACCENT, 150);
        }
        let saved = c.clip();
        c.set_clip(r.inset(4).intersect(saved));
        c.text(r.x + 12, r.y + 6, path, if active { TEXT } else { TEXT_DIM }, FontId::Regular, 13.0);
        c.set_clip(saved);
    }

    /// The status line of half `i`: the number of files, the selection, the space on the volume.
    fn status_line(&self, i: usize) -> (String, Color) {
        if i == self.active
            && let Some(s) = &self.status
        {
            return (s.clone(), 0xF87171);
        }
        let p = &self.panes[i];
        if i == self.active && self.connecting.is_some() {
            return (String::from("Connecting\u{2026}"), ACCENT);
        }
        let base = if p.marked.len() > 1 {
            let bytes: u64 = p.marked.iter().filter(|&&k| !p.entries[k].is_dir).map(|&k| p.entries[k].size as u64).sum();
            format!("{} of {} selected ({})", p.marked.len(), p.entries.len(), size_text(bytes))
        } else {
            ui::items(p.entries.len())
        };
        let text = match p.usage {
            Some((used, total)) => format!("{base}  \u{00B7}  {} free of {}", size_text(total - used), size_text(total)),
            None => base,
        };
        (text, TEXT_DIM)
    }

    fn draw_pane(&self, c: &mut Canvas, a: Rect, i: usize) {
        let off = |r: Rect| r.offset(a.x, a.y);
        let (px, pw) = self.pane_span(i);
        let p = &self.panes[i];
        let active = i == self.active;
        let footer = self.has_footer(i);
        if footer {
            // The half we put things into is slightly lighter, but stays glass.
            c.blend(off(Rect::new(px, 40, pw, self.ch - 40 - STATUS_H)), 0xFFFFFF, 8);
        }
        if i > 0 {
            c.fill(off(Rect::new(px, 40, 1, self.ch - 40)), 0x3A3F55);
        }
        Self::path_bar(c, off(Rect::new(px + 10, 46, pw - 20, 28)), &p.path, active && self.dual);

        let l = self.list(i);
        // The selection in the inactive half is paler.
        let mark = if active { 60 } else { 28 };
        for (row, k) in (p.scroll..p.entries.len()).take(l.rows).enumerate() {
            let renaming = p.renaming.as_ref().filter(|_| active && p.selected == Some(k));
            let marked = p.marked.contains(&k) || (p.marked.is_empty() && p.selected == Some(k));
            let ring = active && p.selected == Some(k) && p.marked.len() > 1;
            self.draw_row(c, off(l.row_rect(row)), &p.path, &p.entries[k], if marked { mark } else { 0 }, ring, renaming);
        }
        if p.entries.is_empty() {
            let note = if fs::offline(&p.path) {
                String::from("The server is not responding. Reconnecting\u{2026}")
            } else if p.loading.is_some() {
                String::from("Loading\u{2026}")
            } else {
                discovery::note(&p.path).unwrap_or_else(|| String::from("This folder is empty"))
            };
            let saved = c.clip();
            c.set_clip(off(Rect::new(px + 16, LIST_Y, pw - 24, ROW_H)).intersect(saved));
            c.text(a.x + px + 24, a.y + LIST_Y + 8, &note, TEXT_DIM, FontId::Regular, 14.0);
            c.set_clip(saved);
        }
        l.bar().draw(c, a, self.thumb_drag.is_some_and(|(k, _)| k == i));
        if active && let Some((start, now, _)) = &self.band {
            let y0 = start.1 + LIST_Y - p.scroll as i32 * ROW_H;
            let band = Rect::new(start.0.min(now.0), y0.min(now.1), (start.0 - now.0).abs(), (y0 - now.1).abs());
            let clip = c.clip();
            c.set_clip(off(Rect::new(l.x, LIST_Y, l.w, l.rows as i32 * ROW_H)).intersect(clip));
            c.blend(off(band), ACCENT, 40);
            c.stroke_round(off(band), 2, ACCENT, 160);
            c.set_clip(clip);
        }
        if footer {
            self.draw_footer(c, a, px, pw);
        }

        let (status, color) = self.status_line(i);
        let saved = c.clip();
        c.fill(off(Rect::new(px, self.ch - STATUS_H, pw, 1)), 0x2E3247);
        c.set_clip(off(Rect::new(px, self.ch - STATUS_H, pw - 8, STATUS_H)).intersect(saved));
        c.text(a.x + px + 14, a.y + self.ch - STATUS_H + 5, &status, color, FontId::Regular, 12.0);
        c.set_clip(saved);
    }

    /// The bottom of the destination half: what is put where, buttons.
    fn draw_footer(&self, c: &mut Canvas, a: Rect, px: i32, pw: i32) {
        let Some(p) = &self.pending else { return };
        let off = |r: Rect| r.offset(a.x, a.y);
        let top = self.ch - STATUS_H - PICK_FOOTER;
        c.fill(off(Rect::new(px, top, pw, 1)), 0x2E3247);
        let n = p.sources.len();
        let what = if n == 1 { format!("\u{201C}{}\u{201D}", fs::name(&p.sources[0])) } else { format!("{n} items") };
        let verb = if p.mode == Mode::Copy { "Copy" } else { "Move" };
        let saved = c.clip();
        c.set_clip(off(Rect::new(px + 10, top, pw - 20, 34)).intersect(saved));
        c.text(a.x + px + 14, a.y + top + 10, &format!("{verb} {what} to this folder"), ACCENT, FontId::SemiBold, 13.5);
        c.set_clip(saved);
        let (new, cancel, ok) = self.pick_buttons().unwrap();
        let busy = self.job.is_some();
        ui::button(c, off(new), "+ Folder", !busy);
        ui::button(c, off(cancel), "Cancel", !busy);
        let label = if p.mode == Mode::Copy { "Copy here" } else { "Move here" };
        ui::accent_button(c, off(ok), label, !busy);
    }

    /// The "two halves" button: a window icon with a divider; when on — accent coloured.
    fn draw_split_button(&self, c: &mut Canvas, a: Rect) {
        let r = self.split_rect().offset(a.x, a.y);
        if self.dual {
            ui::accent_button(c, r, "", self.job.is_none());
        } else {
            ui::button(c, r, "", true);
        }
        let color = if self.dual { 0x0B1020 } else { TEXT };
        let icon = Rect::new(r.x + r.w / 2 - 9, r.y + 7, 18, 14);
        c.stroke_round(icon, 3, color, 256);
        c.fill(Rect::new(icon.x + 8, icon.y + 1, 2, icon.h - 2), color);
    }

    fn draw_menu(&self, c: &mut Canvas, a: Rect) {
        self.draw_device_tip(c, a);
        if let Some(m) = &self.menu {
            m.draw(c, a, |action| matches!(action, Action::Delete | Action::Eject(_)));
        }
    }

    /// The device under the cursor whose usage can be shown.
    fn device_at(&self, x: i32, y: i32) -> Option<usize> {
        if x >= self.side_w() {
            return None;
        }
        (0..self.side.len()).find(|&i| self.side_rect(i).contains(x, y) && !self.side[i].offline && self.side[i].device.is_some_and(|d| d.3 > 0))
    }

    /// The tooltip under a device: how much is used out of how much.
    fn draw_device_tip(&self, c: &mut Canvas, a: Rect) {
        let Some((i, _)) = self.device_hover.filter(|_| self.device_tip) else { return };
        let Some((_, _, used, total)) = self.side.get(i).and_then(|s| s.device) else { return };
        let text = format!("{} used of {}  \u{00B7}  {} free", size_text(used), size_text(total), size_text(total - used));
        let width = crate::font::measure(FontId::Regular, 13.0, &text) + 20;
        let row = self.side_rect(i).offset(a.x, a.y);
        let r = Rect::new((row.x + 26).min(a.x + a.w - width - 6).max(a.x + 6), row.y + row.h + 2, width, TIP_H);
        c.fill_round(r, 7, 0x161A2C, 256);
        c.stroke_round(r, 7, 0xFFFFFF, 50);
        c.text(r.x + 10, r.y + 5, &text, TEXT, FontId::Regular, 13.0);
    }

    fn draw_sidebar(&self, c: &mut Canvas, a: Rect) {
        let sw = self.side_w();
        let side = Rect::new(a.x, a.y, sw, a.h);
        c.fill(side, SIDEBAR_BG);
        // The edge the panel is dragged by: brighter and wider under the cursor.
        if self.side_hover || self.side_drag.is_some() {
            c.fill(Rect::new(a.x + sw - 2, a.y, 3, a.h), ACCENT);
        } else {
            c.fill(Rect::new(a.x + sw - 1, a.y, 1, a.h), 0x2E3247);
        }
        let devices = self.side.iter().filter(|s| s.device.is_some()).count();
        c.text(a.x + 16, a.y + 12, "DEVICES", TEXT_DIM, FontId::SemiBold, 11.0);
        c.text(a.x + 16, a.y + 32 + devices as i32 * DEVICE_H + 10, "PLACES", TEXT_DIM, FontId::SemiBold, 11.0);
        let active = self.side_active(&self.panes[self.side_pane()].path);
        let saved = c.clip();
        for (i, item) in self.side.iter().enumerate() {
            let r = self.side_rect(i).offset(a.x, a.y);
            if active == Some(i) {
                c.fill_round(r, 8, ACCENT, 45);
            }
            match item.device {
                Some((removable, in_memory, used, total)) => {
                    if item.network {
                        net_icon(c, r.x + 8, r.y + 6)
                    } else if removable {
                        usb_icon(c, r.x + 8, r.y + 6)
                    } else {
                        drive_icon(c, r.x + 8, r.y + 7)
                    }
                    let text_w = r.w - 40 - if removable { 26 } else { 0 };
                    c.set_clip(Rect::new(r.x + 34, r.y, text_w, r.h).intersect(saved));
                    c.text(r.x + 34, r.y + 4, &item.label, if item.offline { TEXT_DIM } else { TEXT }, FontId::Regular, 13.0);
                    c.set_clip(saved);
                    if item.offline {
                        c.text(r.x + 34, r.y + 21, "Offline", 0xFBBF24, FontId::Regular, 11.0);
                        eject_icon(c, self.eject_rect(i).offset(a.x, a.y));
                        continue;
                    }
                    // Volume fill level.
                    let bar = Rect::new(r.x + 34, r.y + 26, r.w - 44, 4);
                    c.fill_round(bar, 2, 0xFFFFFF, 30);
                    if total > 0 {
                        let w = ((bar.w as u64 * used / total) as i32).clamp(2, bar.w);
                        let color = if in_memory { 0xFBBF24 } else if used * 10 > total * 9 { 0xF87171 } else { ACCENT };
                        c.fill_round(Rect::new(bar.x, bar.y, w, bar.h), 2, color, 220);
                    }
                    if removable {
                        eject_icon(c, self.eject_rect(i).offset(a.x, a.y));
                    }
                }
                None => {
                    if item.path == fs::NETWORK {
                        net_icon(c, r.x + 8, r.y + 3);
                    } else {
                        ui::folder_icon(c, r.x + 8, r.y + 3);
                    }
                    c.set_clip(Rect::new(r.x + 34, r.y, r.w - 40, r.h).intersect(saved));
                    c.text(r.x + 34, r.y + 5, &item.label, TEXT, FontId::Regular, 13.0);
                    c.set_clip(saved);
                }
            }
        }
        let r = self.connect_rect().offset(a.x, a.y);
        c.stroke_round(r, 8, 0xFFFFFF, 30);
        c.fill(Rect::new(r.x + 13, r.y + 13, 9, 2), ACCENT);
        c.fill(Rect::new(r.x + 16, r.y + 10, 2, 9), ACCENT);
        c.set_clip(Rect::new(r.x + 32, r.y, r.w - 36, r.h).intersect(saved));
        c.text(r.x + 32, r.y + 6, "Connect to Server", TEXT_DIM, FontId::Regular, 12.5);
        c.set_clip(saved);
    }
}

/// The rename field in list row `r`.
fn rename_box(r: Rect) -> Rect {
    Rect::new(r.x + 36, r.y + 2, 260.min(r.w - 48), r.h - 4)
}

/// A disk: a body and an indicator.
fn drive_icon(c: &mut Canvas, x: i32, y: i32) {
    c.fill_round(Rect::new(x, y, 20, 14), 3, 0x9AA3BE, 256);
    c.fill_round(Rect::new(x + 2, y + 2, 16, 6), 2, 0xC9D0E4, 256);
    c.fill_round(Rect::new(x + 14, y + 10, 3, 2), 1, 0x4ADE80, 256);
}

/// A USB stick: a body and a connector.
fn usb_icon(c: &mut Canvas, x: i32, y: i32) {
    c.fill_round(Rect::new(x + 2, y + 6, 18, 11), 3, ACCENT, 256);
    c.fill(Rect::new(x + 5, y + 1, 12, 6), 0xC9D0E4);
    c.fill(Rect::new(x + 8, y + 3, 2, 2), 0x6B7088);
    c.fill(Rect::new(x + 12, y + 3, 2, 2), 0x6B7088);
}

/// A network folder: a server (two "shelves") and a wire.
fn net_icon(c: &mut Canvas, x: i32, y: i32) {
    c.fill_round(Rect::new(x + 2, y, 18, 7), 2, 0x9AA3BE, 256);
    c.fill_round(Rect::new(x + 2, y + 8, 18, 7), 2, 0x9AA3BE, 256);
    c.fill(Rect::new(x + 15, y + 2, 2, 2), 0x4ADE80);
    c.fill(Rect::new(x + 15, y + 10, 2, 2), 0x4ADE80);
    c.fill(Rect::new(x + 10, y + 15, 2, 3), 0x9AA3BE);
    c.fill(Rect::new(x + 4, y + 18, 14, 2), 0x9AA3BE);
}

/// A computer: a monitor on a stand.
fn computer_icon(c: &mut Canvas, x: i32, y: i32) {
    c.fill_round(Rect::new(x + 1, y, 20, 14), 2, 0x9AA3BE, 256);
    c.fill(Rect::new(x + 3, y + 2, 16, 10), 0x24304A);
    c.fill(Rect::new(x + 9, y + 14, 4, 3), 0x9AA3BE);
    c.fill(Rect::new(x + 5, y + 17, 12, 2), 0x9AA3BE);
}

/// The "eject" icon: a triangle above a bar.
fn eject_icon(c: &mut Canvas, r: Rect) {
    c.fill_round(r, 6, 0xFFFFFF, 18);
    let (cx, y) = (r.x + r.w / 2, r.y + 4);
    for i in 0..6 {
        c.fill(Rect::new(cx - i, y + i, 2 * i + 1, 1), TEXT);
    }
    c.fill(Rect::new(cx - 5, y + 9, 11, 2), TEXT);
}

impl App for Files {
    fn background(&self) -> Color {
        BG
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        (self.cw, self.ch) = (a.w, a.h);
        let off = |r: Rect| r.offset(a.x, a.y);
        if self.side_w() > 0 {
            self.draw_sidebar(c, a);
        }
        let p = self.cur();
        let one = p.one_chosen();
        let any = !p.marked.is_empty() || p.selected.is_some();
        for (i, (r, label)) in self.toolbar().into_iter().enumerate() {
            // In the lists of groups and computers there is nothing to create or change.
            let network = above_shares(&p.path);
            let enabled = match i {
                0 => p.path != "/",
                _ if network => false,
                3 => one.is_some(),
                4 => any,
                5 => one.is_some_and(|i| !p.entries[i].is_dir),
                _ => true,
            };
            ui::button(c, off(r), label, enabled);
        }
        self.draw_split_button(c, a);
        for i in 0..self.pane_count() {
            self.draw_pane(c, a, i);
        }
        self.draw_dnd(c, a);
        self.draw_menu(c, a);
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        if pressed {
            self.device_hover = None;
            self.device_tip = false;
        }
        // The sidebar edge is being dragged — change its width.
        if let Some(grab) = self.side_drag {
            if buttons & LEFT == 0 {
                // Released — remember the width (also after a reboot).
                self.side_drag = None;
                save_side_width(self.side_width);
            } else {
                self.side_width = (x - grab).min(SIDEBAR_MAX.min(self.cw - PANES_MIN)).max(SIDEBAR_MIN);
                SIDE_WIDTH.store(self.side_width, Ordering::Relaxed);
            }
            return true;
        }
        // The rename field: cursor and selection with the mouse.
        if let Some(r) = self.rename_rect() {
            let inside = r.contains(x, y) && buttons & LEFT != 0 && pressed;
            let a = self.active;
            if (inside || !pressed)
                && let Some(f) = &mut self.panes[a].renaming
                && f.mouse(r, x, y, buttons, pressed)
            {
                return true;
            }
        }
        // The thumb is being dragged: move the list while the button is held.
        if let Some((i, grab)) = self.thumb_drag {
            if buttons & LEFT == 0 {
                self.thumb_drag = None;
            } else {
                self.panes[i].scroll = self.list(i).bar().top_at(y - grab);
            }
            return true;
        }
        if !pressed {
            // The button was released — the end of the rectangle (or of an ordinary click).
            if buttons & LEFT == 0 {
                if let Some((_, _, target)) = self.dnd.take() {
                    wm::drag_label(None);
                    if let Some((path, _, _)) = target {
                        self.drop_on(path);
                    } else if x < 0 || y < 0 || x >= self.cw || y >= self.ch {
                        // Released outside the window — the files go to the window under the cursor (for example, the player).
                        wm::request(Request::DropFiles(self.cur().chosen()));
                    }
                    self.press = None;
                    return true;
                }
                // A click on one of the marked items without dragging — only it is selected.
                if let Some(i) = self.press_keep.take() {
                    let rows = self.list(self.active).rows;
                    self.panes[self.active].select(i, rows);
                    self.press = None;
                    return true;
                }
                let had = self.band.is_some();
                self.band = None;
                self.press = None;
                return had;
            }
            if self.dnd.is_some() {
                self.dnd_moved(x, y);
                return true;
            }
            // The mouse is dragged after a press in the list — a selection rectangle or a drag.
            if let Some((px, py)) = self.press {
                if self.band.is_none() {
                    let moved = (x - px).abs().max((self.content_y(y) - py).abs());
                    if moved < DRAG_START {
                        return false;
                    }
                    self.commit_rename();
                    if self.press_item && above_shares(&self.cur().path) {
                        return false;
                    }
                    if self.press_item && self.cur().renaming.is_none() {
                        self.press_keep = None;
                        self.dnd_moved(x, y);
                        return true;
                    }
                    let ctrl = input::mods() & input::MOD_CTRL != 0;
                    let base = if ctrl { self.cur().marked.clone() } else { BTreeSet::new() };
                    self.band = Some(((px, py), (x, y), base));
                }
                self.update_band(x, y);
                return true;
            }
            return false;
        }
        if buttons & LEFT == 0 {
            return false;
        }
        self.status = None;
        if let Some(m) = self.menu.take() {
            // A click with the menu open: an item — run it, elsewhere — just close it.
            if let Some(i) = m.item_at(x, y)
                && m.items[i].2
            {
                self.run(m.items[i].1);
            }
            return true;
        }
        if self.on_divider(x) {
            self.side_drag = Some(x - self.side_w());
            return true;
        }
        if x < self.side_w() {
            return self.sidebar_click(x, y);
        }
        if self.split_rect().contains(x, y) {
            self.commit_rename();
            self.toggle_split();
            return true;
        }
        if let Some(i) = self.toolbar().iter().position(|(r, _)| r.contains(x, y)) {
            self.toolbar_action(i);
            return true;
        }
        let Some(pane) = self.pane_at(x) else { return false };
        if self.has_footer(pane)
            && let Some((new, cancel, ok)) = self.pick_buttons()
        {
            let idle = self.job.is_none();
            if new.contains(x, y) && idle {
                self.run(Action::PickNewFolder);
                return true;
            }
            if cancel.contains(x, y) && idle {
                self.cancel_pick();
                return true;
            }
            if ok.contains(x, y) && idle {
                self.paste_here();
                return true;
            }
        }
        self.activate(pane);
        let l = self.list(pane);
        if let Some(grab) = l.bar().grab(x, y) {
            self.panes[pane].scroll = l.bar().top_at(y - grab);
            self.thumb_drag = Some((pane, grab));
            return true;
        }
        if y < LIST_Y || y >= LIST_Y + l.rows as i32 * ROW_H {
            return true;
        }
        self.list_press(x, y)
    }

    fn hover(&mut self, x: i32, y: i32) -> bool {
        let on = self.on_divider(x);
        let mut changed = on != self.side_hover;
        self.side_hover = on;
        if let Some(m) = &mut self.menu {
            let now = m.item_at(x, y);
            changed |= now != m.hover;
            m.hover = now;
        }
        let device = self.device_at(x, y);
        if device != self.device_hover.map(|(i, _)| i) {
            changed |= self.device_tip;
            self.device_tip = false;
            self.device_hover = device.map(|i| (i, interrupts::ticks()));
        }
        changed
    }

    fn context_click(&mut self, x: i32, y: i32) -> bool {
        self.commit_rename();
        self.menu = None;
        if x < self.side_w() {
            let Some(i) = (0..self.side.len()).find(|&i| self.side_rect(i).contains(x, y)) else { return true };
            if self.side[i].network {
                let auto = smb::is_auto(&self.side[i].path);
                let label = if auto { "Reconnect at startup: On" } else { "Reconnect at startup: Off" };
                self.open_menu(x, y, alloc::vec![(label, Action::ToggleAuto(i), true), ("", Action::Separator, true), ("Disconnect", Action::Eject(i), true)]);
            } else if self.side[i].device.is_some_and(|d| d.0) {
                self.open_menu(x, y, alloc::vec![("Eject", Action::Eject(i), true)]);
            }
            return true;
        }
        let Some(pane) = self.pane_at(x) else { return true };
        self.activate(pane);
        let l = self.list(pane);
        if let Some(depth) = discovery::depth(&self.panes[pane].path) {
            let items = match l.hit(x, y) {
                Some(i) => {
                    let rows = l.rows;
                    self.panes[pane].select(i, rows);
                    let is_dir = self.panes[pane].entries[i].is_dir;
                    match depth {
                        2 => alloc::vec![("Open", Action::Open, true), ("", Action::Separator, true), ("Connect", Action::NetConnect, true), ("Connect as\u{2026}", Action::NetConnectAs, true)],
                        1 => alloc::vec![("Open", Action::Open, true), ("Log In\u{2026}", Action::NetLogIn, true), ("", Action::Separator, true), ("Refresh", Action::NetRefresh, true)],
                        0 => alloc::vec![("Open", Action::Open, true)],
                        _ if is_dir => alloc::vec![
                            ("Open", Action::Open, true),
                            ("", Action::Separator, true),
                            ("Connect", Action::NetConnect, true),
                            ("Connect as\u{2026}", Action::NetConnectAs, true),
                            ("", Action::Separator, true),
                            ("Copy\u{2026}", Action::Copy, self.job.is_none()),
                        ],
                        _ => alloc::vec![("Open", Action::Open, true), ("", Action::Separator, true), ("Copy\u{2026}", Action::Copy, self.job.is_none())],
                    }
                }
                None if depth >= 3 => {
                    self.panes[pane].clear_selection();
                    alloc::vec![("Connect", Action::NetConnect, true), ("Connect as\u{2026}", Action::NetConnectAs, true)]
                }
                None => {
                    self.panes[pane].clear_selection();
                    let mut items = alloc::vec![("Refresh", Action::NetRefresh, true), ("Add Computer\u{2026}", Action::NetAddComputer, true)];
                    if depth == 2 {
                        items.insert(1, ("Log In\u{2026}", Action::NetLogIn, true));
                    }
                    items
                }
            };
            self.open_menu(x, y, items);
            return true;
        }
        let idle = self.job.is_none();
        let split = (if self.dual { "Single Pane" } else { "Split View" }, Action::ToggleSplit, idle);
        match l.hit(x, y) {
            Some(i) => {
                let p = &mut self.panes[pane];
                // A right click on an unselected item — select only it.
                if !p.marked.contains(&i) {
                    p.select(i, l.rows);
                }
                p.selected = Some(i);
                let one = p.one_chosen().is_some();
                let file = one && !p.entries[i].is_dir;
                let n = p.chosen().len();
                let mut items = alloc::vec![
                    ("Open", Action::Open, one),
                    ("Open in Notepad", Action::Edit, file),
                    ("", Action::Separator, true),
                    ("Copy\u{2026}", Action::Copy, idle),
                    ("Cut\u{2026}", Action::Cut, idle),
                    ("", Action::Separator, true),
                    ("Rename", Action::Rename, one),
                ];
                items.push((if n > 1 { "Delete selected" } else { "Delete" }, Action::Delete, true));
                self.open_menu(x, y, items);
            }
            None if self.has_footer(pane) => {
                let label = if self.pending.as_ref().unwrap().mode == Mode::Copy { "Copy here" } else { "Move here" };
                self.open_menu(
                    x,
                    y,
                    alloc::vec![
                        (label, Action::PasteHere, idle),
                        ("New Folder", Action::PickNewFolder, idle),
                        ("", Action::Separator, true),
                        ("Cancel", Action::CancelPick, idle),
                    ],
                );
            }
            None => {
                self.panes[pane].clear_selection();
                let empty = self.panes[pane].entries.is_empty();
                self.open_menu(
                    x,
                    y,
                    alloc::vec![
                        ("New Folder", Action::NewFolder, true),
                        ("New File", Action::NewFile, true),
                        ("", Action::Separator, true),
                        ("Select All", Action::SelectAll, !empty),
                        ("", Action::Separator, true),
                        (if SHOW_HIDDEN.load(Ordering::Relaxed) { "Hide Hidden Files" } else { "Show Hidden Files" }, Action::ToggleHidden, true),
                        split,
                    ],
                );
            }
        }
        true
    }

    fn text(&mut self, ch: char, _ctx: &Ctx) -> bool {
        let a = self.active;
        self.panes[a].renaming.as_mut().is_some_and(|field| field.type_char(ch, |c| !c.is_control()))
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        let a = self.active;
        if let Some(field) = &mut self.panes[a].renaming {
            match ch {
                b'\n' => self.commit_rename(),
                0x1B => self.panes[a].renaming = None,
                _ => return field.key(ch, |c| c != '/'),
            }
            return true;
        }
        if self.menu.take().is_some() && ch == 0x1B {
            return true;
        }
        let rows = self.rows(a);
        let p = &self.panes[a];
        let last = p.entries.len().saturating_sub(1);
        let empty = p.entries.is_empty();
        let (prev, next) = (p.selected.map_or(0, |i| i.saturating_sub(1)), p.selected.map_or(0, |i| (i + 1).min(last)));
        match ch {
            KEY_UP if !empty => self.panes[a].select(prev, rows),
            KEY_DOWN if !empty => self.panes[a].select(next, rows),
            KEY_HOME if !empty => self.panes[a].select(0, rows),
            KEY_END if !empty => self.panes[a].select(last, rows),
            // Shift+arrows — extend the selection.
            k if !empty && k == KEY_UP + SHIFTED_NAV => self.panes[a].select_range(prev, false, rows),
            k if !empty && k == KEY_DOWN + SHIFTED_NAV => self.panes[a].select_range(next, false, rows),
            // Tab — to the other half.
            b'\t' if self.dual => self.activate(1 - a),
            b'\n' => {
                if self.pending.is_some() {
                    self.paste_here();
                } else if let Some(i) = self.cur().one_chosen() {
                    self.open(i);
                }
            }
            0x1B if self.pending.is_some() && self.job.is_none() => self.cancel_pick(),
            0x1B => self.panes[a].clear_selection(),
            0x08 => self.toolbar_action(0),
            0x01 => self.run(Action::SelectAll), // Ctrl+A
            0x03 => self.begin_pick(Mode::Copy), // Ctrl+C
            0x18 => self.begin_pick(Mode::Move), // Ctrl+X
            0x16 if self.pending.is_some() => self.paste_here(), // Ctrl+V
            KEY_DELETE => self.delete(),
            _ => return false,
        }
        true
    }

    fn location(&self) -> Option<String> {
        Some(self.cur().path.clone())
    }

    fn cursor(&self, x: i32, y: i32) -> Option<Cursor> {
        let over = self.on_divider(x) && (0..self.ch).contains(&y);
        (self.side_drag.is_some() || over).then_some(Cursor::ResizeH)
    }

    fn scroll_at(&mut self, x: i32, _y: i32, lines: i32) -> bool {
        let Some(i) = self.pane_at(x) else { return false };
        let max = self.panes[i].entries.len().saturating_sub(self.rows(i));
        let p = &mut self.panes[i];
        let before = p.scroll;
        p.scroll = (p.scroll as i32 + lines).clamp(0, max as i32) as usize;
        p.scroll != before
    }

    fn resize(&mut self, cw: i32, ch: i32) {
        (self.cw, self.ch) = (cw, ch);
        for i in 0..self.pane_count() {
            let rows = self.rows(i);
            if let Some(k) = self.panes[i].selected {
                self.panes[i].scroll_to(k, rows);
            }
        }
    }

    fn tick(&mut self, ctx: &Ctx) -> bool {
        if self.connect_done() || self.login_done() {
            return true;
        }
        if !self.device_tip && self.device_hover.is_some_and(|(_, since)| ctx.ticks >= since + TIP_DELAY) {
            self.device_tip = true;
            return true;
        }
        // Copying finished — choosing a place is no longer needed.
        if let Some(job) = &self.job
            && job.lock().finished_at.is_some()
        {
            let failed = job.lock().error.is_some();
            self.job = None;
            if !failed {
                self.cancel_pick();
            }
            self.refresh();
            return true;
        }
        for i in 0..self.pane_count() {
            let rows = self.rows(i);
            if self.panes[i].take_listing(rows) {
                return true;
            }
        }
        // Someone else (the terminal, the editor, a copy) changed the files — refresh the list.
        // While copying runs, files change by the hundred: once a second is enough,
        // otherwise the window itself slows the copying down by re-reading the network folder.
        if fs::version() != self.fs_version && (self.job.is_none() || ctx.ticks >= self.refreshed + HZ) {
            self.refreshed = ctx.ticks;
            self.refresh();
            return true;
        }
        // The blinking cursor in the rename field.
        let on = ctx.caret_on();
        let changed = on != self.cursor_on;
        self.cursor_on = on;
        changed && self.cur().renaming.is_some()
    }
}
