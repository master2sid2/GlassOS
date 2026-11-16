//! Parsing the HID report descriptor (HID 1.11, section 6.2.2): where each field lies in which
//! report. The touchpad layout is built from the fields by the Windows Precision
//! Touchpad rules: contacts, the button, the contact count and the mode switch.

use alloc::vec::Vec;

/// The usage pages and usages the touchpad needs.
const PAGE_DESKTOP: u16 = 0x01;
const PAGE_BUTTON: u16 = 0x09;
const PAGE_DIGITIZER: u16 = 0x0D;
const X: u16 = 0x30;
const Y: u16 = 0x31;
const TOUCHPAD: u16 = 0x05;
const MOUSE: u16 = 0x02;
const WHEEL: u16 = 0x38;
const TIP: u16 = 0x42;
const CONFIDENCE: u16 = 0x47;
const CONTACT_ID: u16 = 0x51;
const CONTACT_COUNT: u16 = 0x54;
const SCAN_TIME: u16 = 0x56;
const INPUT_MODE: u16 = 0x52;
const SURFACE_SWITCH: u16 = 0x57;
const BUTTON_SWITCH: u16 = 0x58;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Input,
    Output,
    Feature,
}

#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub kind: Kind,
    pub report: u8,
    pub page: u16,
    pub usage: u16,
    /// The offset in bits from the start of the report data (after the report id) and the size.
    pub offset: u32,
    pub size: u32,
    pub logical_min: i32,
    pub logical_max: i32,
    pub physical_min: i32,
    pub physical_max: i32,
    pub unit: u32,
    pub unit_exponent: i32,
    /// The number of the logical collection the field is in (each finger has its own).
    pub collection: u32,
    /// The application collection's usage (page and usage).
    pub application: (u16, u16),
}

impl Field {
    /// The field's value in report `data` (without the report id byte).
    pub fn get(&self, data: &[u8]) -> i32 {
        let mut value: u32 = 0;
        for bit in 0..self.size.min(32) {
            let at = self.offset + bit;
            let byte = data.get((at / 8) as usize).copied().unwrap_or(0);
            if byte >> (at % 8) & 1 != 0 {
                value |= 1 << bit;
            }
        }
        if self.logical_min < 0 && self.size < 32 && value >> (self.size - 1) & 1 != 0 {
            (value | !((1u32 << self.size) - 1)) as i32
        } else {
            value as i32
        }
    }

    /// Writes the value into the report data.
    pub fn set(&self, data: &mut [u8], value: i32) {
        for bit in 0..self.size.min(32) {
            let at = self.offset + bit;
            if let Some(byte) = data.get_mut((at / 8) as usize) {
                if (value as u32) >> bit & 1 != 0 {
                    *byte |= 1 << (at % 8);
                } else {
                    *byte &= !(1 << (at % 8));
                }
            }
        }
    }

    /// The physical size of the whole range in millimetres, if described.
    pub fn millimeters(&self) -> Option<f32> {
        let span = (self.physical_max - self.physical_min) as f32;
        if span <= 0.0 {
            return None;
        }
        let mut scale = 1.0f32;
        let mut e = self.unit_exponent;
        while e > 0 {
            scale *= 10.0;
            e -= 1;
        }
        while e < 0 {
            scale /= 10.0;
            e += 1;
        }
        // The unit system and length: 0x11 — centimetres, 0x13 — inches.
        let per_unit = match self.unit & 0xFF {
            0x11 => 10.0,
            0x13 => 25.4,
            _ => return None,
        };
        Some(span * scale * per_unit)
    }
}

#[derive(Clone, Copy, Default)]
struct Globals {
    page: u16,
    logical_min: i32,
    logical_max: i32,
    physical_min: i32,
    physical_max: i32,
    unit: u32,
    unit_exponent: i32,
    size: u32,
    count: u32,
    report: u8,
}

/// All fields of input, output and feature reports. Arrays (keyboard keys and
/// the like) are not needed by the touchpad and are skipped.
pub fn parse(desc: &[u8]) -> Vec<Field> {
    let mut fields = Vec::new();
    let mut g = Globals::default();
    let mut stack: Vec<Globals> = Vec::new();
    let mut usages: Vec<(u16, u16)> = Vec::new();
    let mut usage_range: (Option<u32>, Option<u32>) = (None, None);
    // Open collections: (number, type, usage).
    let mut collections: Vec<(u32, u32, (u16, u16))> = Vec::new();
    let mut next_collection = 0;
    // How many bits are already taken in each report of each kind.
    let mut offsets: Vec<(Kind, u8, u32)> = Vec::new();
    let mut i = 0;
    while i < desc.len() {
        let prefix = desc[i];
        if prefix == 0xFE {
            // A long item: skip it.
            let len = desc.get(i + 1).copied().unwrap_or(0) as usize;
            i += 3 + len;
            continue;
        }
        let size = [0, 1, 2, 4][(prefix & 3) as usize];
        let bytes = &desc[(i + 1).min(desc.len())..(i + 1 + size).min(desc.len())];
        i += 1 + size;
        let mut raw = 0u32;
        for (k, b) in bytes.iter().enumerate() {
            raw |= (*b as u32) << (8 * k);
        }
        let signed = match bytes.len() {
            1 => raw as u8 as i8 as i32,
            2 => raw as u16 as i16 as i32,
            _ => raw as i32,
        };
        let full_usage = |raw: u32, page: u16| if bytes.len() == 4 { ((raw >> 16) as u16, raw as u16) } else { (page, raw as u16) };
        match (prefix >> 2) & 3 {
            // Main items.
            0 => {
                let tag = prefix >> 4;
                match tag {
                    0x8 | 0x9 | 0xB => {
                        let kind = match tag {
                            0x8 => Kind::Input,
                            0x9 => Kind::Output,
                            _ => Kind::Feature,
                        };
                        let slot = match offsets.iter().position(|o| o.0 == kind && o.1 == g.report) {
                            Some(at) => at,
                            None => {
                                offsets.push((kind, g.report, 0));
                                offsets.len() - 1
                            }
                        };
                        let constant = raw & 1 != 0;
                        let variable = raw & 2 != 0;
                        let (collection, application) = (
                            collections.last().map_or(0, |c| c.0),
                            collections.iter().find(|c| c.1 == 1).map_or((0, 0), |c| c.2),
                        );
                        for n in 0..g.count {
                            let usage = if let (Some(min), Some(max)) = usage_range {
                                let u = (min + n).min(max);
                                Some(if min > 0xFFFF { ((u >> 16) as u16, u as u16) } else { (g.page, u as u16) })
                            } else {
                                usages.get(n as usize).or(usages.last()).copied()
                            };
                            if !constant && variable && let Some((page, usage)) = usage {
                                fields.push(Field {
                                    kind,
                                    report: g.report,
                                    page,
                                    usage,
                                    offset: offsets[slot].2,
                                    size: g.size,
                                    logical_min: g.logical_min,
                                    logical_max: g.logical_max,
                                    physical_min: g.physical_min,
                                    physical_max: g.physical_max,
                                    unit: g.unit,
                                    unit_exponent: g.unit_exponent,
                                    collection,
                                    application,
                                });
                            }
                            offsets[slot].2 += g.size;
                        }
                    }
                    0xA => {
                        let usage = usages.first().copied().unwrap_or((g.page, 0));
                        next_collection += 1;
                        collections.push((next_collection, raw, usage));
                    }
                    0xC => {
                        collections.pop();
                    }
                    _ => {}
                }
                usages.clear();
                usage_range = (None, None);
            }
            // Global ones.
            1 => match prefix >> 4 {
                0x0 => g.page = raw as u16,
                0x1 => g.logical_min = signed,
                0x2 => g.logical_max = if signed < g.logical_min { raw as i32 } else { signed },
                0x3 => g.physical_min = signed,
                0x4 => g.physical_max = if signed < g.physical_min { raw as i32 } else { signed },
                0x5 => g.unit_exponent = if raw & 0x8 != 0 { (raw & 0xF) as i32 - 16 } else { (raw & 0xF) as i32 },
                0x6 => g.unit = raw,
                0x7 => g.size = raw,
                0x8 => g.report = raw as u8,
                0x9 => g.count = raw,
                0xA => stack.push(g),
                0xB => g = stack.pop().unwrap_or(g),
                _ => {}
            },
            // Local ones.
            2 => match prefix >> 4 {
                0x0 => usages.push(full_usage(raw, g.page)),
                0x1 => usage_range.0 = Some(raw),
                0x2 => usage_range.1 = Some(raw),
                _ => {}
            },
            _ => {}
        }
    }
    fields
}

/// The report data length in bytes (without the report id).
pub fn report_bytes(fields: &[Field], kind: Kind, report: u8) -> usize {
    fields.iter().filter(|f| f.kind == kind && f.report == report).map(|f| (f.offset + f.size).div_ceil(8) as usize).max().unwrap_or(0)
}

/// The fields of one contact.
#[derive(Clone, Copy, Debug)]
pub struct Finger {
    pub tip: Field,
    pub confidence: Option<Field>,
    pub id: Option<Field>,
    pub x: Field,
    pub y: Field,
}

/// Where everything lies on the touchpad.
#[derive(Clone, Debug)]
pub struct Layout {
    /// The report with contacts.
    pub report: u8,
    pub fingers: Vec<Finger>,
    pub count: Option<Field>,
    pub button: Option<Field>,
    pub scan_time: Option<Field>,
    /// The mode switch: 0 — mouse, 3 — touch.
    pub input_mode: Option<Field>,
    /// Enable the surface and the button (the "selective reporting" feature report).
    pub surface_switch: Option<Field>,
    pub button_switch: Option<Field>,
    /// The pad size in millimetres.
    pub width_mm: f32,
    pub height_mm: f32,
}

fn find(fields: &[Field], kind: Kind, page: u16, usage: u16) -> Option<Field> {
    fields.iter().find(|f| f.kind == kind && f.page == page && f.usage == usage).copied()
}

/// The touchpad layout, if the descriptor contains one.
pub fn touchpad(fields: &[Field]) -> Option<Layout> {
    let pad: Vec<&Field> = fields.iter().filter(|f| f.application == (PAGE_DIGITIZER, TOUCHPAD)).collect();
    let tips: Vec<&&Field> = pad.iter().filter(|f| f.kind == Kind::Input && f.page == PAGE_DIGITIZER && f.usage == TIP).collect();
    let report = tips.first()?.report;
    let in_report: Vec<Field> = pad.iter().filter(|f| f.kind == Kind::Input && f.report == report).map(|f| **f).collect();
    let mut fingers = Vec::new();
    for tip in &tips {
        if tip.report != report {
            continue;
        }
        let same = |page: u16, usage: u16| in_report.iter().find(|f| f.collection == tip.collection && f.page == page && f.usage == usage).copied();
        let (Some(x), Some(y)) = (same(PAGE_DESKTOP, X), same(PAGE_DESKTOP, Y)) else { continue };
        fingers.push(Finger { tip: ***tip, confidence: same(PAGE_DIGITIZER, CONFIDENCE), id: same(PAGE_DIGITIZER, CONTACT_ID), x, y });
    }
    let first = *fingers.first()?;
    let report_level = |page: u16, usage: u16| in_report.iter().find(|f| f.page == page && f.usage == usage && !fingers.iter().any(|g| g.tip.collection == f.collection)).copied();
    let span = |f: &Field| f.millimeters();
    Some(Layout {
        report,
        count: report_level(PAGE_DIGITIZER, CONTACT_COUNT),
        button: report_level(PAGE_BUTTON, 1),
        scan_time: report_level(PAGE_DIGITIZER, SCAN_TIME),
        input_mode: fields.iter().find(|f| f.kind == Kind::Feature && f.page == PAGE_DIGITIZER && f.usage == INPUT_MODE).copied(),
        // The switches live in a separate "device configuration" collection.
        surface_switch: find(fields, Kind::Feature, PAGE_DIGITIZER, SURFACE_SWITCH),
        button_switch: find(fields, Kind::Feature, PAGE_DIGITIZER, BUTTON_SWITCH),
        width_mm: span(&first.x).unwrap_or(100.0),
        height_mm: span(&first.y).unwrap_or(60.0),
        fingers,
    })
}

/// The mouse report the touchpad sends in mouse mode (before switching to touch or after
/// its reset): buttons, motion, wheel.
#[derive(Clone, Debug)]
pub struct Mouse {
    pub report: u8,
    /// Buttons 1, 2, 3 — left, right, middle.
    pub buttons: Vec<Field>,
    pub x: Field,
    pub y: Field,
    pub wheel: Option<Field>,
}

impl Mouse {
    /// Buttons (bits: left, right, middle), motion and wheel from the report data.
    pub fn read(&self, data: &[u8]) -> (u8, i32, i32, i32) {
        let buttons = self.buttons.iter().filter(|b| (1..=3).contains(&b.usage) && b.get(data) != 0).fold(0, |m, b| m | 1 << (b.usage - 1));
        (buttons, self.x.get(data), self.y.get(data), self.wheel.map_or(0, |w| w.get(data)))
    }
}

/// The mouse report from the descriptor, if there is one.
pub fn mouse(fields: &[Field]) -> Option<Mouse> {
    let input = |page: u16, usage: u16| fields.iter().find(|f| f.kind == Kind::Input && f.application == (PAGE_DESKTOP, MOUSE) && f.page == page && f.usage == usage).copied();
    let x = input(PAGE_DESKTOP, X)?;
    let y = input(PAGE_DESKTOP, Y).filter(|y| y.report == x.report)?;
    Some(Mouse {
        report: x.report,
        buttons: fields.iter().filter(|f| f.kind == Kind::Input && f.report == x.report && f.page == PAGE_BUTTON).copied().collect(),
        x,
        y,
        wheel: input(PAGE_DESKTOP, WHEEL).filter(|w| w.report == x.report),
    })
}
