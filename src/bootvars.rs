//! Boot entries in firmware NVRAM (the `Boot####` and `BootOrder` variables)
//! through UEFI runtime services — they keep working after exiting boot services.
//!
//! Many firmwares (AMI on mini PCs and laptops) look for `\EFI\BOOT\BOOTX64.EFI`
//! only on USB sticks, and from an internal disk boot only what is recorded in
//! NVRAM, like "Windows Boot Manager". The installer records "GlassOS" the same way.
//!
//! From here we also ask the firmware about the next boot: open its setup
//! (`OsIndications`, like "UEFI Firmware Settings" in Windows) or boot once
//! from a USB stick (`BootNext`). This helps when the keyboard does not work before the
//! system boots: with Fast Boot the firmware does not enable USB and does not
//! hear F2 or F12.

use crate::parts::{self, Drive};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use uefi::runtime::{self, VariableAttributes, VariableVendor};

/// The partition to boot from: the "hard drive" node of a UEFI device path.
#[derive(Clone, Copy)]
pub struct HardDrive {
    /// Partition number in the table (from 1).
    pub number: u32,
    pub start: u64,
    pub sectors: u64,
    /// Partition GUID (GPT) or the disk signature in the first 4 bytes (MBR).
    pub signature: [u8; 16],
    pub gpt: bool,
}

/// What the firmware should start the next boot with.
#[derive(Clone, Copy)]
pub enum Next {
    /// As usual, by the boot order.
    Normal,
    /// Its own setup (BIOS setup).
    Setup,
    /// Once — from the USB stick partition.
    Usb(HardDrive),
    /// Once — the trial kernel from the system partition (`TRIAL_KERNEL`). If it hangs,
    /// the previous one starts after a power cycle.
    Trial(HardDrive),
}

const LOAD_OPTION_ACTIVE: u32 = 1;
/// OsIndications bit: open the firmware setup on the next boot.
const BOOT_TO_FW_UI: u64 = 1;
/// The USB stick boot entry we write for `Next::Usb`.
const USB_STICK: &str = "GlassOS: USB stick";
/// The loader the firmware looks for on removable media.
const FALLBACK: &str = "\\EFI\\BOOT\\BOOTX64.EFI";
/// The trial kernel boot entry and its file on the system partition.
const TRIAL: &str = "GlassOS: trial kernel";
pub const TRIAL_KERNEL: &str = "\\EFI\\GLASSOS\\TRIAL.EFI";

fn name(n: u16) -> uefi::CString16 {
    uefi::CString16::try_from(format!("Boot{n:04X}").as_str()).unwrap()
}

/// Firmware services must not be called from two CPUs at once: one lock for all
/// (it also disables interrupts for the duration of the call).
static FIRMWARE: crate::sync::IrqLock<()> = crate::sync::IrqLock::new(());

fn firmware<R>(f: impl FnOnce() -> R) -> R {
    let _guard = FIRMWARE.lock();
    f()
}

fn cstr(s: &str) -> uefi::CString16 {
    uefi::CString16::try_from(s).unwrap()
}

/// A whole variable (None — missing or unreadable).
fn get(var: &uefi::CStr16) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 4096];
    firmware(|| runtime::get_variable(var, &VariableVendor::GLOBAL_VARIABLE, &mut buf).map(|(data, _)| data.to_vec())).ok()
}

/// Whether the variable exists (even if it is longer than our buffer).
fn exists(var: &uefi::CStr16) -> bool {
    let mut buf = [0u8; 1];
    match firmware(|| runtime::get_variable(var, &VariableVendor::GLOBAL_VARIABLE, &mut buf).map(|_| ())) {
        Ok(()) => true,
        Err(e) => e.status() == uefi::Status::BUFFER_TOO_SMALL,
    }
}

fn set(var: &uefi::CStr16, data: &[u8]) -> Result<(), String> {
    let attrs = VariableAttributes::NON_VOLATILE | VariableAttributes::BOOTSERVICE_ACCESS | VariableAttributes::RUNTIME_ACCESS;
    firmware(|| runtime::set_variable(var, &VariableVendor::GLOBAL_VARIABLE, attrs, data)).map_err(|e| format!("{var}: {:?}", e.status()))
}

/// Hang note: written straight from the stuck CPU, so no heap and no waiting
/// for the firmware lock. Survives a watchdog reboot.
const NOTE_VENDOR: VariableVendor = VariableVendor(uefi::guid!("6b1e0f52-8a3c-4d6e-9f21-3c5a7b9d0e14"));

pub fn save_note(text: &[u8]) {
    let Some(_guard) = FIRMWARE.try_lock() else { return };
    let attrs = VariableAttributes::NON_VOLATILE | VariableAttributes::BOOTSERVICE_ACCESS | VariableAttributes::RUNTIME_ACCESS;
    let _ = runtime::set_variable(uefi::cstr16!("GlassOSStuck"), &NOTE_VENDOR, attrs, &text[..text.len().min(1024)]);
}

/// Boot record (build, trial or not, shut down cleanly or not) — for the crash report (`crash`).
pub fn set_run_record(text: &str) {
    let attrs = VariableAttributes::NON_VOLATILE | VariableAttributes::BOOTSERVICE_ACCESS | VariableAttributes::RUNTIME_ACCESS;
    let _ = firmware(|| runtime::set_variable(uefi::cstr16!("GlassOSRun"), &NOTE_VENDOR, attrs, text.as_bytes()));
}

pub fn run_record() -> Option<String> {
    let mut buf = vec![0u8; 256];
    firmware(|| runtime::get_variable(uefi::cstr16!("GlassOSRun"), &NOTE_VENDOR, &mut buf).map(|(data, _)| String::from_utf8_lossy(data).into_owned())).ok()
}

/// The previous boot's note (and erase it).
pub fn take_note() -> Option<String> {
    let mut buf = vec![0u8; 1024];
    let var = uefi::cstr16!("GlassOSStuck");
    let text = firmware(|| runtime::get_variable(var, &NOTE_VENDOR, &mut buf).map(|(data, _)| String::from_utf8_lossy(data).into_owned())).ok()?;
    let _ = firmware(|| runtime::delete_variable(var, &NOTE_VENDOR));
    Some(text)
}

/// An 8-byte variable (OsIndications, OsIndicationsSupported).
fn get_u64(var: &str) -> Option<u64> {
    let data = get(&cstr(var))?;
    Some(u64::from_le_bytes(data.get(..8)?.try_into().ok()?))
}

/// Entry number from a 2-byte variable (BootNext, BootCurrent).
fn get_u16(var: &str) -> Option<u16> {
    get(&cstr(var)).filter(|d| d.len() >= 2).map(|d| u16::from_le_bytes([d[0], d[1]]))
}

fn order() -> Vec<u16> {
    get(&cstr("BootOrder")).map_or(Vec::new(), |d| d.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect())
}

fn set_order(order: &[u16]) -> Result<(), String> {
    let bytes: Vec<u8> = order.iter().flat_map(|o| o.to_le_bytes()).collect();
    set(&cstr("BootOrder"), &bytes)
}

/// Number of the boot entry with description `description`: an existing one (in the
/// order or outside it) or the first free one.
fn number_for(description: &str, order: &[u16]) -> Result<u16, String> {
    let ours = |n: u16| get(&name(n)).as_deref().and_then(parse).is_some_and(|(_, d, _)| d == description);
    if let Some(n) = order.iter().copied().find(|&n| ours(n)) {
        return Ok(n);
    }
    if let Some(n) = (0..0x100u16).find(|&n| !order.contains(&n) && ours(n)) {
        return Ok(n);
    }
    (0..0x1000u16).find(|&n| !order.contains(&n) && !exists(&name(n))).ok_or_else(|| "no free boot option number".into())
}

/// A boot entry (EFI_LOAD_OPTION): attributes, path length, description, path.
fn load_option(description: &str, hd: &HardDrive, file: &str) -> Vec<u8> {
    let mut path = vec![4u8, 1, 42, 0]; // media / hard drive, 42 bytes
    path.extend(hd.number.to_le_bytes());
    path.extend(hd.start.to_le_bytes());
    path.extend(hd.sectors.to_le_bytes());
    path.extend(hd.signature);
    let kind = if hd.gpt { 2 } else { 1 };
    path.extend([kind, kind]); // partition format and signature type
    let file: Vec<u16> = file.encode_utf16().chain([0]).collect();
    path.extend([4u8, 4]); // media / file path
    path.extend((4 + file.len() as u16 * 2).to_le_bytes());
    file.iter().for_each(|u| path.extend(u.to_le_bytes()));
    path.extend([0x7F, 0xFF, 4, 0]); // end of path
    let mut option = Vec::new();
    option.extend(LOAD_OPTION_ACTIVE.to_le_bytes());
    option.extend((path.len() as u16).to_le_bytes());
    description.encode_utf16().chain([0]).for_each(|u| option.extend(u.to_le_bytes()));
    option.extend(path);
    option
}

/// Description and path of a boot entry.
fn parse(option: &[u8]) -> Option<(u32, String, &[u8])> {
    let attrs = u32::from_le_bytes(option.get(0..4)?.try_into().ok()?);
    let path_len = u16::from_le_bytes(option.get(4..6)?.try_into().ok()?) as usize;
    let units: Vec<u16> = option[6..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
    let at = 6 + (units.len() + 1) * 2;
    Some((attrs, String::from_utf16_lossy(&units), option.get(at..at + path_len)?))
}

/// Writes (or updates) boot entry `description` and puts it first.
/// Returns its number.
pub fn add(description: &str, hd: &HardDrive, file: &str) -> Result<u16, String> {
    let order = order();
    let n = number_for(description, &order)?;
    set(&name(n), &load_option(description, hd, file))?;
    let mut new_order = vec![n];
    new_order.extend(order.iter().copied().filter(|&o| o != n));
    set_order(&new_order)?;
    crate::kprintln!("bootvars: Boot{n:04X} \"{description}\" first of {}", new_order.len());
    Ok(n)
}

/// Whether the firmware can open its setup on the next boot when the system asks.
pub fn setup_supported() -> bool {
    get_u64("OsIndicationsSupported").is_some_and(|v| v & BOOT_TO_FW_UI != 0)
}

/// A USB stick the firmware can boot from: a mounted partition that holds
/// `\EFI\BOOT\BOOTX64.EFI` (for Ventoy — the VTOYEFI partition, for a written ISO — its
/// EFI volume). Returns the boot path node and a human-readable label.
pub fn usb_stick() -> Result<(HardDrive, String), String> {
    for (disk, blocks) in crate::usb::disks() {
        let drive = Drive::Usb(disk);
        let (_, Some(table)) = parts::examine(drive, blocks) else { continue };
        for p in table.parts.iter().filter(|p| !p.extended) {
            let Some(path) = crate::fs::partition_path(drive, p.start as u32) else { continue };
            if crate::fs::exists(&crate::fs::resolve(&path, &FALLBACK[1..].replace('\\', "/"))) {
                let at = table.placed(p);
                let hd = HardDrive { number: at.number, start: at.start, sectors: at.sectors, signature: at.signature, gpt: at.gpt };
                let label = if p.label.is_empty() { String::new() } else { format!(" \"{}\"", p.label) };
                return Ok((hd, format!("partition {}{label} at {path}", at.number)));
            }
        }
    }
    Err("no USB stick with a boot loader (\\EFI\\BOOT\\BOOTX64.EFI) is plugged in".into())
}

/// Asks the firmware to start the next boot with `next`. Called right before
/// the reboot: if it is cancelled, the request is not left pending.
pub fn prepare(next: Next) -> Result<(), String> {
    match next {
        Next::Normal => Ok(()),
        Next::Setup => {
            let now = get_u64("OsIndications").unwrap_or(0);
            set(&cstr("OsIndications"), &(now | BOOT_TO_FW_UI).to_le_bytes())?;
            crate::kprintln!("bootvars: the firmware setup opens on the next start");
            Ok(())
        }
        Next::Usb(hd) => once(USB_STICK, &hd, FALLBACK),
        Next::Trial(hd) => once(TRIAL, &hd, TRIAL_KERNEL),
    }
}

/// Boot entry `description` for file `file` on partition `hd`, and BootNext pointing to it.
fn once(description: &str, hd: &HardDrive, file: &str) -> Result<(), String> {
    let mut order = order();
    let n = number_for(description, &order)?;
    set(&name(n), &load_option(description, hd, file))?;
    // At the end of the order: the usual order does not change, and firmwares that
    // look for BootNext only among entries in the order will find it too.
    if !order.is_empty() && !order.contains(&n) {
        order.push(n);
        set_order(&order)?;
    }
    set(&cstr("BootNext"), &n.to_le_bytes())?;
    crate::kprintln!("bootvars: BootNext = Boot{n:04X} \"{description}\", partition {}", hd.number);
    Ok(())
}

/// Briefly, where a path leads: "HD 1 GPT \EFI\BOOT\BOOTX64.EFI", "USB", "PXE"…
fn describe_path(mut path: &[u8]) -> String {
    let mut parts: Vec<String> = Vec::new();
    while path.len() >= 4 {
        let (ty, sub, len) = (path[0], path[1], u16::from_le_bytes([path[2], path[3]]) as usize);
        if len < 4 || len > path.len() || ty == 0x7F {
            break;
        }
        let node = &path[..len];
        match (ty, sub) {
            (4, 1) if len >= 42 => {
                let number = u32::from_le_bytes(node[4..8].try_into().unwrap());
                parts.push(format!("HD {number} {}", if node[40] == 2 { "GPT" } else { "MBR" }));
            }
            (4, 4) => {
                let units: Vec<u16> = node[4..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
                parts.push(String::from_utf16_lossy(&units));
            }
            (3, 5) => parts.push("USB".into()),
            (3, 0x12) => parts.push("SATA".into()),
            (3, 0x17) => parts.push("NVMe".into()),
            (3, 11) | (3, 12) | (3, 24) => parts.push("network".into()),
            (5, _) => parts.push("legacy BIOS".into()),
            (4, 6) | (4, 7) => parts.push("firmware app".into()),
            _ => {}
        }
        path = &path[len..];
    }
    parts.dedup();
    parts.join(" ")
}

/// The firmware boot order — for the `bootorder` command and the hwprobe report.
pub fn list() -> Vec<String> {
    let current = get_u16("BootCurrent");
    let order = order();
    let mut out: Vec<String> = order
        .iter()
        .map(|&n| match get(&name(n)).as_deref().and_then(parse) {
            Some((attrs, desc, path)) => {
                let mark = if current == Some(n) { "*" } else { " " };
                let off = if attrs & LOAD_OPTION_ACTIVE == 0 { "  (off)" } else { "" };
                format!("{mark} Boot{n:04X}  {desc:<28} {}{off}", describe_path(path))
            }
            None => format!("  Boot{n:04X}  (not readable)"),
        })
        .collect();
    if order.is_empty() {
        out.push("no BootOrder (the firmware does not share it, or it is empty)".into());
    }
    if let Some(n) = get_u16("BootNext") {
        out.push(format!("next start, once: Boot{n:04X}"));
    }
    let supported = get_u64("OsIndicationsSupported");
    out.push(match supported {
        Some(v) if v & BOOT_TO_FW_UI != 0 => "the firmware opens its setup on request: reboot bios".into(),
        Some(v) => format!("the firmware cannot open its setup on request (OsIndicationsSupported {v:#x})"),
        None => "the firmware cannot open its setup on request (no OsIndicationsSupported)".into(),
    });
    out
}
