//! The "Disks" window: SATA and USB disks, their partitions and free space. Mount and
//! unmount a partition, create a FAT32 partition in free space, format,
//! delete, erase a disk for a new partition table (`crate::parts`).
//! The partition the system runs or booted from is left alone.

use super::ui;
use crate::font::FontId;
use crate::gfx::{Canvas, Color, Rect};
use crate::input::LEFT;
use crate::parts::{self, Drive, Kind, Part, SECTOR, Table, size_text};
use crate::sync::YieldLock;
use crate::wm::{ACCENT, App, Ctx, TEXT, TEXT_DIM};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

const PAD: i32 = 18;
const SIDE_W: i32 = 220;
const DISK_H: i32 = 48;
const ITEM_H: i32 = 34;
const BAR_H: i32 = 28;
/// Where the partition list starts in the right half.
const LIST_Y: i32 = 108;
const DANGER: Color = 0xF87171;
const GOOD: Color = 0x34D399;

/// A list row: a partition or free space (first sector, sector count).
#[derive(Clone)]
enum Item {
    Part(Part),
    Free(u64, u64),
}

impl Item {
    fn start(&self) -> u64 {
        match self {
            Item::Part(p) => p.start,
            Item::Free(s, _) => *s,
        }
    }

    fn sectors(&self) -> u64 {
        match self {
            Item::Part(p) => p.sectors,
            Item::Free(_, n) => *n,
        }
    }
}

struct DiskView {
    drive: Drive,
    model: String,
    sectors: u64,
    /// «GPT, 3 partitions», «empty»…
    contents: String,
    table: Option<Table>,
    items: Vec<Item>,
}

impl DiskView {
    fn load(drive: Drive, model: String, sectors: u64) -> Self {
        let (contents, table) = parts::examine(drive, sectors);
        let mut items: Vec<Item> = Vec::new();
        if let Some(t) = &table {
            items.extend(t.parts.iter().cloned().map(Item::Part));
            if t.slot_free {
                items.extend(t.free.iter().map(|&(s, n)| Item::Free(s, n)));
            }
            items.sort_by_key(|i| i.start());
        }
        Self { drive, model, sectors, contents, table, items }
    }

    fn kind(&self) -> &'static str {
        match self.drive {
            Drive::Sata(_) => "SATA",
            Drive::Usb(_) => "USB",
        }
    }

    /// Where the partition is mounted (if it is).
    fn path(&self, p: &Part) -> Option<String> {
        (p.start <= u32::MAX as u64).then(|| crate::fs::partition_path(self.drive, p.start as u32)).flatten()
    }

    /// Why the partition must not be touched (None — it may be).
    fn protected(&self, p: &Part) -> Option<&'static str> {
        if self.path(p).as_deref() == Some("/") {
            return Some("GlassOS runs from this partition");
        }
        if let Drive::Sata(d) = self.drive
            && crate::fs::boot_partition_start(d) == Some(p.start as u32)
        {
            return Some("the computer started from this partition");
        }
        None
    }
}

fn all_disks() -> Vec<DiskView> {
    let mut out: Vec<DiskView> = crate::ahci::disks().into_iter().map(|(d, sectors, model, _)| DiskView::load(Drive::Sata(d), model, sectors)).collect();
    for (d, blocks) in crate::usb::disks() {
        out.push(DiskView::load(Drive::Usb(d), format!("USB drive {}", d + 1), blocks));
    }
    out
}

/// What to do after confirmation.
#[derive(Clone)]
enum Action {
    Format(Part),
    Delete(Part),
    /// Erase the disk: a new empty GPT.
    Erase,
}

/// The operation progress: the step and the result (Ok — a message for the status line).
type Job = Arc<YieldLock<(String, Option<Result<String, String>>)>>;

enum Stage {
    Browse,
    /// A new partition in free space: the size and the label.
    New { start: u64, max: u64 },
    Confirm(Action),
    Busy(Job),
}

pub struct Disks {
    disks: Vec<DiskView>,
    disk: usize,
    item: Option<usize>,
    stage: Stage,
    /// The fields of the new partition form and which one has focus.
    size: ui::TextField,
    label: ui::TextField,
    focus: usize,
    caret_on: bool,
    /// The result of the last operation: (text, whether it is an error).
    status: Option<(String, bool)>,
    /// The first visible row of the partition list.
    top: usize,
    usb_version: u64,
    cw: i32,
    ch: i32,
}

/// "79.5", "79,5 GB", "500 MB", "1 TB" → sectors (without a unit — gigabytes).
fn parse_size(text: &str) -> Option<u64> {
    let t = text.trim().to_ascii_lowercase().replace(',', ".");
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (num, unit) = (t[..split].trim(), t[split..].trim());
    let mul: f64 = match unit {
        "" | "g" | "gb" => (1u64 << 30) as f64,
        "m" | "mb" => (1u64 << 20) as f64,
        "t" | "tb" => (1u64 << 40) as f64,
        _ => return None,
    };
    let value: f64 = num.parse().ok()?;
    let sectors = (value * mul / SECTOR as f64) as u64;
    (sectors > 0).then_some(sectors / parts::ALIGN * parts::ALIGN)
}

/// The size for the input field: "79.5" (rounded as in the label; more than the free space means all of it).
fn size_field(sectors: u64) -> String {
    let tenths = (sectors * SECTOR as u64 * 10 + (1u64 << 29)) / (1u64 << 30);
    if tenths % 10 == 0 { format!("{}", tenths / 10) } else { format!("{}.{}", tenths / 10, tenths % 10) }
}

/// The label for a new partition: DATA, and if such is already mounted — DATA2, DATA3…
/// Otherwise two identical "DATA" could not be told apart in Files.
fn free_label() -> String {
    let taken: Vec<String> = crate::fs::volumes().into_iter().map(|v| v.name.to_ascii_uppercase()).collect();
    (1..).map(|n| if n == 1 { String::from("DATA") } else { format!("DATA{n}") }).find(|l| !taken.contains(l)).unwrap_or_default()
}

/// A FAT volume label: upper-case ASCII, up to 11 characters.
fn clean_label(text: &str) -> String {
    let l: String = text.chars().filter(|c| c.is_ascii_alphanumeric() || " _-".contains(*c)).take(11).collect::<String>().trim().to_ascii_uppercase();
    if l.is_empty() { "DATA".into() } else { l }
}

impl Disks {
    pub fn new(cw: i32, ch: i32) -> Self {
        let mut size = ui::TextField::new("", 16);
        size.size = 14.0;
        let mut label = ui::TextField::new("DATA", 11);
        label.size = 14.0;
        Self {
            disks: all_disks(),
            disk: 0,
            item: None,
            stage: Stage::Browse,
            size,
            label,
            focus: 0,
            caret_on: true,
            status: None,
            top: 0,
            usb_version: crate::usb::disks_version(),
            cw,
            ch,
        }
    }

    fn view(&self) -> Option<&DiskView> {
        self.disks.get(self.disk)
    }

    fn selected(&self) -> Option<&Item> {
        self.view().and_then(|d| self.item.and_then(|i| d.items.get(i)))
    }

    /// Re-read the disks; select the row that starts at `at` (if any).
    fn reload(&mut self, at: Option<u64>) {
        let drive = self.view().map(|d| d.drive);
        self.disks = all_disks();
        self.disk = drive.and_then(|dr| self.disks.iter().position(|d| d.drive == dr)).unwrap_or(0);
        self.item = at.and_then(|s| self.view().and_then(|d| d.items.iter().position(|i| i.start() == s)));
        self.top = 0;
    }

    fn disk_row(&self, i: usize) -> Rect {
        Rect::new(PAD, PAD + 30 + i as i32 * (DISK_H + 6), SIDE_W - PAD, DISK_H)
    }

    fn right(&self) -> Rect {
        let x = SIDE_W + PAD;
        Rect::new(x, PAD, self.cw - x - PAD, self.ch - 2 * PAD)
    }

    fn bar(&self) -> Rect {
        let r = self.right();
        Rect::new(r.x, r.y + 56, r.w, BAR_H)
    }

    fn visible(&self) -> usize {
        ((self.ch - LIST_Y - PAD - ui::BUTTON_H - 14) / ITEM_H).max(1) as usize
    }

    fn item_row(&self, i: usize) -> Option<Rect> {
        let r = self.right();
        (i >= self.top && i < self.top + self.visible()).then(|| Rect::new(r.x, LIST_Y + (i - self.top) as i32 * ITEM_H, r.w, ITEM_H - 4))
    }

    /// The fields of the new partition form.
    fn field(&self, i: usize) -> Rect {
        let r = self.right();
        Rect::new(r.x + 90, r.y + 70 + i as i32 * 44, 200, 32)
    }

    /// The buttons at the bottom right (the main one is last) for the current step.
    fn labels(&self) -> Vec<&'static str> {
        match &self.stage {
            Stage::Browse => {
                let mut out = Vec::new();
                let Some(d) = self.view() else { return out };
                match self.selected() {
                    Some(Item::Free(..)) => out.push("New partition\u{2026}"),
                    Some(Item::Part(p)) if !p.extended => {
                        let path = d.path(p);
                        if d.protected(p).is_none() {
                            out.push("Delete\u{2026}");
                            out.push("Format\u{2026}");
                        }
                        match path.as_deref() {
                            Some("/") => {}
                            Some(_) => out.push("Unmount"),
                            None if d.protected(p).is_none() && matches!(p.fs, "FAT32" | "FAT16" | "exFAT") => out.push("Mount"),
                            None => {}
                        }
                    }
                    _ => {}
                }
                if self.selected().is_none() && d.items.iter().all(|i| !matches!(i, Item::Part(p) if d.protected(p).is_some())) {
                    out.push("Erase disk\u{2026}");
                }
                out
            }
            Stage::New { .. } => alloc::vec!["Cancel", "Create"],
            Stage::Confirm(Action::Format(_)) => alloc::vec!["Cancel", "Format"],
            Stage::Confirm(Action::Delete(_)) => alloc::vec!["Cancel", "Delete"],
            Stage::Confirm(Action::Erase) => alloc::vec!["Cancel", "Erase"],
            Stage::Busy(_) => Vec::new(),
        }
    }

    fn buttons(&self, labels: &[&str]) -> Vec<Rect> {
        let width: i32 = labels.iter().map(|l| ui::button_width(l) + 6).sum::<i32>() - 6;
        ui::button_row(labels, self.cw - PAD - width, self.ch - PAD - ui::BUTTON_H)
    }

    /// Starts an operation in the background; `work` returns a message and where to select a row afterwards.
    fn run(&mut self, what: &str, work: impl FnOnce(&mut dyn FnMut(&str)) -> Result<String, String> + Send + 'static) {
        let job: Job = Arc::new(YieldLock::new((String::from(what), None)));
        let shared = job.clone();
        crate::task::spawn("disks", None, move || {
            let result = work(&mut |step| shared.lock().0 = String::from(step));
            if let Err(e) = &result {
                crate::kprintln!("disks: {e}");
            }
            shared.lock().1 = Some(result);
        });
        self.stage = Stage::Busy(job);
    }

    fn press(&mut self, label: &str) {
        let Some(d) = self.view() else { return };
        let (drive, total, table) = (d.drive, d.sectors, d.table.clone());
        let paths: Vec<String> = d.items.iter().filter_map(|i| if let Item::Part(p) = i { d.path(p) } else { None }).collect();
        let path_of = |p: &Part| crate::fs::partition_path(drive, p.start as u32);
        let confirmed = match &self.stage {
            Stage::Confirm(a) => Some(a.clone()),
            _ => None,
        };
        match (label, self.selected().cloned()) {
            ("Cancel", _) => self.stage = Stage::Browse,
            ("New partition\u{2026}", Some(Item::Free(start, max))) => {
                self.size.set_text(&size_field(max));
                self.label.set_text(&free_label());
                self.focus = 0;
                self.stage = Stage::New { start, max };
            }
            ("Format\u{2026}", Some(Item::Part(p))) => self.stage = Stage::Confirm(Action::Format(p)),
            ("Delete\u{2026}", Some(Item::Part(p))) => self.stage = Stage::Confirm(Action::Delete(p)),
            ("Erase disk\u{2026}", _) => self.stage = Stage::Confirm(Action::Erase),
            ("Mount", Some(Item::Part(p))) => {
                self.status = Some(match crate::fs::mount_partition(drive, p.start as u32, p.sectors as u32) {
                    Ok(path) => (format!("Mounted at {path}"), false),
                    Err(e) => (format!("Cannot mount: {e}"), true),
                });
                self.reload(Some(p.start));
            }
            ("Unmount", Some(Item::Part(p))) => {
                if let Some(path) = path_of(&p) {
                    self.status = Some(match crate::fs::eject(&path) {
                        Ok(()) => (format!("{path} unmounted"), false),
                        Err(e) => (format!("Cannot unmount: {e}"), true),
                    });
                }
                self.reload(Some(p.start));
            }
            ("Create", _) => {
                let Stage::New { start, max } = self.stage else { return };
                let Some(sectors) = parse_size(&self.size.text).map(|s| s.min(max)).filter(|&s| s >= parts::MIN_FREE) else {
                    self.status = Some((format!("Size: from 64 MB to {}", size_text(max)), true));
                    return;
                };
                if start + sectors > u32::MAX as u64 {
                    self.status = Some(("FAT32 volumes here must end before 2 TB".into(), true));
                    return;
                }
                let Some(table) = table else { return };
                let label = clean_label(&self.label.text);
                self.run("Creating the partition", move |step| {
                    let placed = parts::add(drive, &table, start, sectors, Kind::Data, &label)?;
                    step("Formatting (FAT32)");
                    crate::fs::create_volume(drive.device(placed.start as u32, placed.sectors as u32), &label, &[], &mut |_| {})?;
                    let path = crate::fs::mount_partition(drive, placed.start as u32, placed.sectors as u32)?;
                    Ok(format!("{label}: {} FAT32 at {path}", size_text(placed.sectors)))
                });
            }
            ("Format", _) => {
                let (Some(Action::Format(p)), Some(table)) = (confirmed, table) else { return };
                let label = clean_label(if p.label.is_empty() { "DATA" } else { &p.label });
                let path = path_of(&p);
                self.run("Formatting", move |step| {
                    if let Some(path) = path {
                        crate::fs::eject(&path)?;
                    }
                    if p.start + p.sectors > u32::MAX as u64 {
                        return Err("FAT32 volumes must end before 2 TB".into());
                    }
                    // The entry type is "basic data", the name is the label: other systems will see the partition too.
                    let placed = parts::replace(drive, &table, p.slot, p.sectors, Kind::Data, &label)?;
                    step("Formatting (FAT32)");
                    crate::fs::create_volume(drive.device(placed.start as u32, placed.sectors as u32), &label, &[], &mut |_| {})?;
                    let path = crate::fs::mount_partition(drive, placed.start as u32, placed.sectors as u32)?;
                    Ok(format!("Partition {} is now FAT32 \u{201c}{label}\u{201d} at {path}", p.slot + 1))
                });
            }
            ("Delete", _) => {
                let (Some(Action::Delete(p)), Some(table)) = (confirmed, table) else { return };
                let path = path_of(&p);
                self.run("Deleting the partition", move |_| {
                    if let Some(path) = path {
                        crate::fs::eject(&path)?;
                    }
                    parts::delete(drive, &table, p.slot)?;
                    Ok(format!("Partition {} deleted", p.slot + 1))
                });
            }
            ("Erase", _) => {
                self.run("Erasing the disk", move |_| {
                    for path in paths {
                        crate::fs::eject(&path)?;
                    }
                    parts::create_table(drive, total)?;
                    Ok("The disk is empty now: a new GPT without partitions".into())
                });
            }
            _ => {}
        }
    }

    /// A disk row on the left.
    fn draw_disk(&self, c: &mut Canvas, r: Rect, d: &DiskView, chosen: bool) {
        c.fill_round(r, 9, if chosen { ACCENT } else { 0xFFFFFF }, if chosen { 45 } else { 12 });
        c.stroke_round(r, 9, if chosen { ACCENT } else { 0xFFFFFF }, if chosen { 170 } else { 26 });
        c.text(r.x + 12, r.y + 7, &ui::fit(&d.model, FontId::SemiBold, 13.5, r.w - 24), TEXT, FontId::SemiBold, 13.5);
        let info = format!("{} \u{b7} {}", size_text(d.sectors), d.kind());
        c.text(r.x + 12, r.y + 27, &info, TEXT_DIM, FontId::Regular, 12.5);
    }

    fn draw_bar(&self, c: &mut Canvas, r: Rect, d: &DiskView) {
        c.fill_round(r, 6, 0xFFFFFF, 14);
        let total = d.sectors.max(1);
        for (i, item) in d.items.iter().enumerate() {
            // A 2-pixel gap between neighbouring partitions so the borders are visible.
            let x0 = r.x + (item.start() as u128 * r.w as u128 / total as u128) as i32;
            let w = ((item.sectors() as u128 * r.w as u128 / total as u128) as i32 - 2).max(3);
            let seg = Rect::new(x0, r.y, w.min(r.x + r.w - x0), r.h);
            let chosen = self.item == Some(i);
            match item {
                Item::Part(p) => {
                    let color = if d.protected(p).is_some() { GOOD } else { ACCENT };
                    c.fill_round(seg, 4, color, if chosen { 230 } else { 120 });
                }
                Item::Free(..) => c.stroke_round(seg, 4, 0xFFFFFF, if chosen { 200 } else { 60 }),
            }
            if chosen {
                c.stroke_round(seg.inset(-2), 5, 0xFFFFFF, 220);
            }
        }
    }

    fn draw_item(&self, c: &mut Canvas, r: Rect, d: &DiskView, item: &Item, chosen: bool) {
        if chosen {
            c.fill_round(r, 7, ACCENT, 45);
        }
        let y = r.y + 8;
        let (num, name, fs, size, place, color) = match item {
            Item::Part(p) => {
                let name = if !p.label.is_empty() && !p.name.contains(&p.label) { format!("{} ({})", p.label, p.name) } else { p.name.clone() };
                let place = d.path(p).unwrap_or_default();
                let color = if d.protected(p).is_some() { GOOD } else { TEXT };
                (format!("{}", p.slot + 1), name, p.fs.into(), p.size(), place, color)
            }
            Item::Free(_, n) => (String::new(), "Free space".into(), String::new(), size_text(*n), String::new(), TEXT_DIM),
        };
        let w = r.w;
        c.text(r.x + 10, y, &num, TEXT_DIM, FontId::Regular, 13.5);
        c.text(r.x + 34, y, &ui::fit(&name, FontId::Regular, 13.5, w * 40 / 100 - 34), color, FontId::Regular, 13.5);
        c.text(r.x + w * 40 / 100 + 8, y, &fs, TEXT_DIM, FontId::Regular, 13.5);
        c.text(r.x + w * 55 / 100, y, &size, TEXT, FontId::Regular, 13.5);
        c.text(r.x + w * 72 / 100, y, &ui::fit(&place, FontId::Regular, 13.5, w * 28 / 100 - 8), ACCENT, FontId::Regular, 13.5);
    }

    fn wrap(c: &mut Canvas, x: i32, mut y: i32, w: i32, text: &str, color: Color) -> i32 {
        let mut line = String::new();
        for word in text.split(' ') {
            let next = if line.is_empty() { String::from(word) } else { format!("{line} {word}") };
            if crate::font::measure(FontId::Regular, 14.0, &next) > w && !line.is_empty() {
                c.text(x, y, &line, color, FontId::Regular, 14.0);
                y += 21;
                line = String::from(word);
            } else {
                line = next;
            }
        }
        if !line.is_empty() {
            c.text(x, y, &line, color, FontId::Regular, 14.0);
            y += 21;
        }
        y
    }
}

impl App for Disks {
    fn background(&self) -> Color {
        0x1C1F2E
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        (self.cw, self.ch) = (a.w, a.h);
        c.text(a.x + PAD, a.y + PAD, "Disks", ACCENT, FontId::SemiBold, 18.0);
        if self.disks.is_empty() {
            c.text(a.x + PAD, a.y + PAD + 36, "No SATA or USB disks found.", TEXT_DIM, FontId::Regular, 14.0);
        }
        for (i, d) in self.disks.iter().enumerate() {
            self.draw_disk(c, self.disk_row(i).offset(a.x, a.y), d, i == self.disk);
        }
        let r = self.right().offset(a.x, a.y);
        if let Some(d) = self.view() {
            match &self.stage {
                Stage::Browse => {
                    c.text(r.x, r.y, &ui::fit(&d.model, FontId::SemiBold, 16.0, r.w), TEXT, FontId::SemiBold, 16.0);
                    let table = d.table.as_ref().map_or("no partition table", |t| if t.gpt { "GPT" } else { "MBR" });
                    let info = format!("{} \u{b7} {} \u{b7} {}", size_text(d.sectors), d.kind(), if d.table.is_some() { table } else { &d.contents });
                    c.text(r.x, r.y + 26, &ui::fit(&info, FontId::Regular, 13.0, r.w), TEXT_DIM, FontId::Regular, 13.0);
                    self.draw_bar(c, self.bar().offset(a.x, a.y), d);
                    if d.items.is_empty() {
                        let text = if d.table.is_none() && d.contents != "empty" {
                            format!("GlassOS does not understand what is on this disk ({}). Erase it to make partitions.", d.contents)
                        } else {
                            "No partitions. Erase the disk to make a new partition table.".into()
                        };
                        Self::wrap(c, r.x, a.y + LIST_Y + 8, r.w, &text, TEXT_DIM);
                    }
                    for (i, item) in d.items.iter().enumerate() {
                        if let Some(row) = self.item_row(i) {
                            self.draw_item(c, row.offset(a.x, a.y), d, item, self.item == Some(i));
                        }
                    }
                    if let Some(Item::Part(p)) = self.selected()
                        && let Some(why) = d.protected(p)
                    {
                        let y = a.y + a.h - PAD - ui::BUTTON_H - 22;
                        c.text(r.x, y, &ui::fit(&format!("Not changeable: {why}."), FontId::Regular, 13.0, r.w), GOOD, FontId::Regular, 13.0);
                    }
                }
                Stage::New { max, .. } => {
                    c.text(r.x, r.y, "New partition", TEXT, FontId::SemiBold, 16.0);
                    c.text(r.x, r.y + 26, &format!("In {} of free space on {}", size_text(*max), d.model), TEXT_DIM, FontId::Regular, 13.0);
                    for (i, (name, hint)) in [("Size, GB", "for example 20 or 500 MB"), ("Label", "DATA")].into_iter().enumerate() {
                        let f = self.field(i).offset(a.x, a.y);
                        c.text(r.x, f.y + 7, name, TEXT, FontId::Regular, 14.0);
                        c.fill_round(f, 7, 0x000000, 90);
                        let focused = self.focus == i;
                        c.stroke_round(f, 7, if focused { ACCENT } else { 0xFFFFFF }, if focused { 160 } else { 30 });
                        let field = if i == 0 { &self.size } else { &self.label };
                        field.draw(c, f, focused, self.caret_on, hint, TEXT);
                    }
                    let y = self.field(1).offset(a.x, a.y).y + 50;
                    Self::wrap(c, r.x, y, r.w, "FAT32: Windows, macOS and Linux read it too. One file can be up to 4 GB.", TEXT_DIM);
                }
                Stage::Confirm(action) => {
                    let (question, warn) = match action {
                        Action::Format(p) => (format!("Format partition {} ({}) on {}?", p.slot + 1, p.describe(), d.model), "Everything in it will be lost; it becomes an empty FAT32 volume."),
                        Action::Delete(p) => (format!("Delete partition {} ({}) on {}?", p.slot + 1, p.describe(), d.model), "Its files will be lost; the place becomes free space."),
                        Action::Erase => (format!("Erase {} ({})?", d.model, size_text(d.sectors)), "All partitions and files on the disk will be lost. It gets a new empty GPT."),
                    };
                    let y = Self::wrap(c, r.x, r.y + 4, r.w, &question, TEXT);
                    Self::wrap(c, r.x, y + 8, r.w, warn, DANGER);
                }
                Stage::Busy(job) => {
                    let step = job.lock().0.clone();
                    c.text(r.x, r.y + 4, &format!("{step}\u{2026}"), TEXT, FontId::Regular, 14.0);
                    c.text(r.x, r.y + 30, "Do not unplug the disk or turn the computer off.", TEXT_DIM, FontId::Regular, 13.0);
                }
            }
        }
        if let Some((text, error)) = &self.status
            && matches!(self.stage, Stage::Browse | Stage::New { .. })
        {
            let w = self.cw - SIDE_W - 2 * PAD - self.labels().iter().map(|l| ui::button_width(l) + 6).sum::<i32>();
            c.text(a.x + SIDE_W + PAD, a.y + a.h - PAD - ui::BUTTON_H + 6, &ui::fit(text, FontId::Regular, 13.0, w), if *error { DANGER } else { GOOD }, FontId::Regular, 13.0);
        }
        let labels = self.labels();
        for (i, (r, label)) in self.buttons(&labels).into_iter().zip(&labels).enumerate() {
            let r = r.offset(a.x, a.y);
            let last = i + 1 == labels.len();
            match *label {
                "Delete" | "Erase" | "Format" | "Erase disk\u{2026}" if last => ui::danger_button(c, r, label, true),
                _ if last => ui::accent_button(c, r, label, true),
                _ => ui::button(c, r, label, true),
            }
        }
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        if let Stage::New { .. } = self.stage
            && (!pressed || buttons & LEFT == 0)
        {
            let r = self.field(self.focus);
            let field = if self.focus == 0 { &mut self.size } else { &mut self.label };
            return field.mouse(r, x, y, buttons, pressed);
        }
        if !pressed || buttons & LEFT == 0 {
            return false;
        }
        let labels = self.labels();
        if let Some(label) = self.buttons(&labels).into_iter().zip(&labels).find(|(r, _)| r.contains(x, y)).map(|(_, l)| *l) {
            self.press(label);
            return true;
        }
        match self.stage {
            Stage::Browse => {
                if let Some(i) = (0..self.disks.len()).find(|&i| self.disk_row(i).contains(x, y)) {
                    self.disk = i;
                    self.item = None;
                    self.top = 0;
                    self.status = None;
                    return true;
                }
                let count = self.view().map_or(0, |d| d.items.len());
                if let Some(i) = (0..count).find(|&i| self.item_row(i).is_some_and(|r| r.contains(x, y))) {
                    self.item = Some(i);
                    return true;
                }
                // A click on the bar selects the partition under the cursor.
                let bar = self.bar();
                if bar.contains(x, y)
                    && let Some(d) = self.view()
                {
                    let at = ((x - bar.x) as u128 * d.sectors as u128 / bar.w.max(1) as u128) as u64;
                    self.item = d.items.iter().position(|i| at >= i.start() && at < i.start() + i.sectors());
                    return true;
                }
                // Empty space — clear the selection (then the "Erase disk" button is visible).
                if self.right().contains(x, y) && self.item.is_some() {
                    self.item = None;
                    return true;
                }
                false
            }
            Stage::New { .. } => {
                if let Some(i) = (0..2).find(|&i| self.field(i).contains(x, y)) {
                    self.focus = i;
                    let r = self.field(i);
                    let field = if i == 0 { &mut self.size } else { &mut self.label };
                    field.mouse(r, x, y, buttons, pressed);
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    fn scroll(&mut self, lines: i32) -> bool {
        let rows = self.view().map_or(0, |d| d.items.len());
        let max = rows.saturating_sub(self.visible());
        let top = (self.top as i32 + lines.signum()).clamp(0, max as i32) as usize;
        let changed = top != self.top;
        self.top = top;
        changed
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        self.caret_on = true;
        match (&self.stage, ch) {
            (Stage::New { .. }, 0x1B) | (Stage::Confirm(_), 0x1B) => self.stage = Stage::Browse,
            (Stage::New { .. }, b'\n') => self.press("Create"),
            (Stage::New { .. }, b'\t') => self.focus = 1 - self.focus,
            (Stage::New { .. }, _) => {
                return if self.focus == 0 {
                    self.size.key(ch, |c| c.is_ascii_digit() || ".,mgtbMGTB ".contains(c))
                } else {
                    self.label.key(ch, |c| c.is_ascii_alphanumeric() || " _-".contains(c))
                };
            }
            _ => return false,
        }
        true
    }

    fn tick(&mut self, ctx: &Ctx) -> bool {
        let on = ctx.caret_on();
        let mut changed = on != self.caret_on && matches!(self.stage, Stage::New { .. });
        self.caret_on = on;
        if let Stage::Busy(job) = &self.stage {
            let done = job.lock().1.take();
            if let Some(result) = done {
                self.status = Some(match result {
                    Ok(text) => (text, false),
                    Err(e) => (e, true),
                });
                self.stage = Stage::Browse;
                let at = self.selected().map(|i| i.start());
                self.reload(at);
            }
            return true;
        }
        // A USB stick was inserted or removed — re-read.
        let version = crate::usb::disks_version();
        if version != self.usb_version && matches!(self.stage, Stage::Browse) {
            self.usb_version = version;
            self.reload(None);
            changed = true;
        }
        changed
    }

    fn ask_close(&mut self) -> bool {
        !matches!(self.stage, Stage::Busy(_))
    }
}
