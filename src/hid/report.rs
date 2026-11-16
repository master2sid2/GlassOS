//! The HID report descriptor (Report Descriptor / Report Map): it tells where in a report
//! the keys, modifiers, media keys, buttons and mouse motion are. Shared by any bus:
//! USB provides the descriptor with a GET_DESCRIPTOR request, Bluetooth LE with the Report Map characteristic.

use alloc::vec::Vec;

const PAGE_DESKTOP: u16 = 0x01;
const PAGE_KEYBOARD: u16 = 0x07;
const PAGE_BUTTON: u16 = 0x09;
const PAGE_CONSUMER: u16 = 0x0C;
const USAGE_X: u16 = 0x30;
const USAGE_Y: u16 = 0x31;
const USAGE_WHEEL: u16 = 0x38;
/// The Input item flags: constant (padding), variable (otherwise — an array).
const FLAG_CONSTANT: u32 = 1;
const FLAG_VARIABLE: u32 = 2;
/// This many fields and reports are enough for any device; more means the descriptor is corrupted.
const MAX_FIELDS: usize = 256;

/// An input report field.
#[derive(Clone, Debug)]
struct Field {
    page: u16,
    /// The usages in order (Usage) or a range (Usage Minimum..Maximum).
    usages: Vec<u32>,
    range: Option<(u32, u32)>,
    offset: u32,
    size: u32,
    count: u32,
    variable: bool,
    logical_min: i32,
}

impl Field {
    /// The usage of the i-th value of a variable or of value `v` of an array.
    fn usage(&self, i: u32) -> Option<(u16, u16)> {
        let full = match self.range {
            Some((lo, hi)) => (lo + i <= hi).then_some(lo + i)?,
            None => *self.usages.get(i as usize).or(self.usages.last())?,
        };
        let page = if full > 0xFFFF { (full >> 16) as u16 } else { self.page };
        Some((page, full as u16))
    }
}

/// An input report: the id (0 — no ids) and the fields.
#[derive(Clone, Debug)]
pub struct Report {
    pub id: u8,
    fields: Vec<Field>,
}

/// What was found in the report.
#[derive(Default, Debug)]
pub struct Decoded {
    /// Modifiers (bits as in the boot report) and the pressed keys.
    pub keyboard: Option<(u8, Vec<u8>)>,
    /// The pressed media keys (the Consumer page).
    pub consumer: Option<Vec<u16>>,
    /// The mouse: motion, buttons, wheel.
    pub mouse: Option<(i32, i32, u8, i32)>,
}

/// The parsed descriptor.
#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub reports: Vec<Report>,
    /// Reports start with an id.
    pub numbered: bool,
}

impl Report {
    pub fn has_keyboard(&self) -> bool {
        self.fields.iter().any(|f| f.page == PAGE_KEYBOARD)
    }
    pub fn has_consumer(&self) -> bool {
        self.fields.iter().any(|f| f.page == PAGE_CONSUMER)
    }
    pub fn has_mouse(&self) -> bool {
        self.fields.iter().any(|f| f.page == PAGE_DESKTOP && (0..f.count).any(|i| f.usage(i).is_some_and(|u| u == (PAGE_DESKTOP, USAGE_X))))
    }

    /// A short name for the log.
    pub fn kind(&self) -> &'static str {
        match (self.has_keyboard(), self.has_consumer(), self.has_mouse()) {
            (true, _, _) => "keys",
            (_, _, true) => "mouse",
            (_, true, _) => "media",
            _ => "other",
        }
    }

    /// The field values of a report (the data without the id).
    pub fn decode(&self, data: &[u8]) -> Decoded {
        let mut d = Decoded::default();
        for f in &self.fields {
            for i in 0..f.count {
                let Some(raw) = bits(data, f.offset + i * f.size, f.size) else { break };
                let value = if f.logical_min < 0 { sign(raw, f.size) } else { raw as i32 };
                let usage = if f.variable {
                    f.usage(i).map(|u| (u, value))
                } else {
                    // An array: the value is the usage number; out of range — "nothing".
                    let index = value - f.logical_min;
                    if value == 0 && f.logical_min == 0 && f.range.is_some_and(|r| r.0 == 0) {
                        None
                    } else if index >= 0 {
                        f.usage(index as u32).map(|u| (u, 1))
                    } else {
                        None
                    }
                };
                let Some(((page, u), v)) = usage else { continue };
                match page {
                    PAGE_KEYBOARD => {
                        let k = d.keyboard.get_or_insert((0, Vec::new()));
                        match u {
                            0xE0..=0xE7 if v != 0 => k.0 |= 1 << (u - 0xE0),
                            0x04..=0xDF if v != 0 => k.1.push(u as u8),
                            _ => {}
                        }
                    }
                    PAGE_CONSUMER => {
                        let c = d.consumer.get_or_insert(Vec::new());
                        if v != 0 && u != 0 {
                            c.push(u);
                        }
                    }
                    PAGE_BUTTON => {
                        let m = d.mouse.get_or_insert((0, 0, 0, 0));
                        if v != 0 && (1..=8).contains(&u) {
                            m.2 |= 1 << (u - 1);
                        }
                    }
                    PAGE_DESKTOP => {
                        let m = d.mouse.get_or_insert((0, 0, 0, 0));
                        match u {
                            USAGE_X => m.0 = value,
                            USAGE_Y => m.1 = value,
                            USAGE_WHEEL => m.3 = value,
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }
        d
    }
}

impl Layout {
    /// A report by id and its data (from a packet where the id is the first byte, if there are ids).
    pub fn find<'a>(&self, packet: &'a [u8]) -> Option<(&Report, &'a [u8])> {
        if self.numbered {
            let (&id, rest) = packet.split_first()?;
            self.reports.iter().find(|r| r.id == id).map(|r| (r, rest))
        } else {
            self.reports.first().map(|r| (r, packet))
        }
    }
}

/// `size` bits from bit `at` (least significant bits first, as in HID).
fn bits(data: &[u8], at: u32, size: u32) -> Option<u32> {
    if size == 0 || size > 32 || (at + size).div_ceil(8) as usize > data.len() {
        return None;
    }
    let mut v: u64 = 0;
    let first = (at / 8) as usize;
    let last = ((at + size - 1) / 8) as usize;
    for (n, &b) in data[first..=last].iter().enumerate() {
        v |= (b as u64) << (8 * n);
    }
    Some(((v >> (at % 8)) & ((1u64 << size) - 1)) as u32)
}

fn sign(v: u32, size: u32) -> i32 {
    if size >= 32 {
        return v as i32;
    }
    let shift = 32 - size;
    ((v << shift) as i32) >> shift
}

/// The state of the "global" items (Push/Pop save it).
#[derive(Clone, Copy, Default)]
struct Globals {
    page: u16,
    logical_min: i32,
    size: u32,
    count: u32,
    id: u8,
}

/// Parsing the descriptor. None — corrupted or not understood.
pub fn parse(desc: &[u8]) -> Option<Layout> {
    let mut g = Globals::default();
    let mut stack: Vec<Globals> = Vec::new();
    let mut usages: Vec<u32> = Vec::new();
    let (mut usage_min, mut usage_max) = (None, None);
    // How many bits are already taken in the input report of each id.
    let mut used: Vec<(u8, u32)> = Vec::new();
    let mut layout = Layout::default();
    let mut fields = 0;
    let mut at = 0;
    while at < desc.len() {
        let b = desc[at];
        if b == 0xFE {
            // A long item: skip it.
            let len = *desc.get(at + 1)? as usize;
            at += 3 + len;
            continue;
        }
        let len = [0, 1, 2, 4][(b & 3) as usize];
        let raw = desc.get(at + 1..at + 1 + len)?;
        at += 1 + len;
        let mut v: u32 = 0;
        for (n, &x) in raw.iter().enumerate() {
            v |= (x as u32) << (8 * n);
        }
        let signed = if len == 0 { 0 } else { sign(v, 8 * len as u32) };
        match (b >> 2) & 3 {
            // Main.
            0 => match b >> 4 {
                0x8 => {
                    // Input.
                    fields += 1;
                    if fields > MAX_FIELDS {
                        return None;
                    }
                    let slot = match used.iter_mut().find(|u| u.0 == g.id) {
                        Some(s) => s,
                        None => {
                            used.push((g.id, 0));
                            used.last_mut().unwrap()
                        }
                    };
                    let offset = slot.1;
                    slot.1 += g.size * g.count;
                    if v & FLAG_CONSTANT == 0 {
                        let range = match (usage_min, usage_max) {
                            (Some(lo), Some(hi)) if usages.is_empty() => Some((lo, hi)),
                            _ => None,
                        };
                        let field = Field {
                            page: g.page,
                            usages: core::mem::take(&mut usages),
                            range,
                            offset,
                            size: g.size,
                            count: g.count,
                            variable: v & FLAG_VARIABLE != 0,
                            logical_min: g.logical_min,
                        };
                        match layout.reports.iter_mut().find(|r| r.id == g.id) {
                            Some(r) => r.fields.push(field),
                            None => layout.reports.push(Report { id: g.id, fields: alloc::vec![field] }),
                        }
                    }
                    usages.clear();
                    (usage_min, usage_max) = (None, None);
                }
                // Output, Feature, Collection, End Collection: the usages are reset.
                0x9 | 0xB | 0xA | 0xC => {
                    usages.clear();
                    (usage_min, usage_max) = (None, None);
                }
                _ => {}
            },
            // Global.
            1 => match b >> 4 {
                0x0 => g.page = v as u16,
                0x1 => g.logical_min = signed,
                0x7 => g.size = v,
                0x8 => {
                    g.id = v as u8;
                    layout.numbered = true;
                }
                0x9 => g.count = v,
                0xA => stack.push(g),
                0xB => g = stack.pop()?,
                _ => {}
            },
            // Local.
            2 => {
                let full = if len == 4 { v } else { v & 0xFFFF };
                match b >> 4 {
                    0x0 => usages.push(full),
                    0x1 => usage_min = Some(full),
                    0x2 => usage_max = Some(full),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    (!layout.reports.is_empty()).then_some(layout)
}
