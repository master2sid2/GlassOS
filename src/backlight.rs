//! The brightness of a laptop's built-in screen: the backlight PWM in the Intel GPU.
//!
//! The firmware turns the backlight on; we only change the PWM duty cycle. The display
//! south bridge (PCH) registers differ between generations:
//! - Cannon Point and newer (Comet Lake, Ice/Tiger Lake...): the frequency and the duty cycle are
//!   separate registers 0xC8254 and 0xC8258;
//! - Lynx/Sunrise Point (Haswell…Kaby Lake): 0xC8254 holds the period in the upper 16 bits
//!   and the duty cycle in the lower ones.
//! If the backlight is not on (or the GPU is not Intel), brightness is unavailable.
//!
//! Laptop brightness keys (Dell and many others) send no key code: the firmware catches
//! them and writes a request to the OpRegion — memory shared with the graphics driver (the ASLE mailbox,
//! its address is in GPU register 0xFC). We announce that the driver is ready and handles brightness,
//! poll the mailbox (`requested`) and report the current brightness (CBLV): the firmware computes
//! the next step from it.

use crate::pci;
use crate::sync::IrqLock;
use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const CTL: usize = 0xC8250;
const CTL2: usize = 0xC8254;
const DUTY: usize = 0xC8258;
/// Below this the screen is almost black and easy to "lose".
pub const MIN: u32 = 5;

struct Backlight {
    mmio: usize,
    /// The new register layout (CNP+).
    split: bool,
    max: u32,
}

static BL: IrqLock<Option<Backlight>> = IrqLock::new(None);
/// The ASLE mailbox in the OpRegion (its address in memory), if the firmware provides it.
static ASLE: IrqLock<Option<usize>> = IrqLock::new(None);

const OPREGION_SIZE: u64 = 8 << 10;
const OPREGION_SIGNATURE: &[u8; 16] = b"IntelGraphicsMem";
const OPREGION_MBOXES: usize = 0x58;
const MBOX_ASLE: u32 = 1 << 2;
/// The ACPI mailbox: "driver ready" (DRDY) and the status (CSTS).
const ACPI_MBOX: usize = 0x100;
const ACPI_CSTS: usize = 0x08;
const ACPI_DRDY: usize = 0x00;
const ASLE_MBOX: usize = 0x300;
const ASLE_ARDY: usize = 0x00;
const ASLE_ASLC: usize = 0x04;
const ASLE_TCHE: usize = 0x08;
const ASLE_BCLP: usize = 0x10;
const ASLE_CBLV: usize = 0x18;
/// The "change brightness" request; the answer "failed".
const ASLC_SET_BACKLIGHT: u32 = 1 << 1;
const ASLC_BACKLIGHT_FAILED: u32 = 1 << 12;
/// The technologies the driver handles: brightness.
const TCHE_BACKLIGHT: u32 = 1 << 1;
const VALID: u32 = 1 << 31;
static BLANKED: AtomicBool = AtomicBool::new(false);
static SAVED_DUTY: AtomicU32 = AtomicU32::new(0);

fn rd(b: &Backlight, r: usize) -> u32 {
    unsafe { ((b.mmio + r) as *const u32).read_volatile() }
}

fn wr(b: &Backlight, r: usize, v: u32) {
    unsafe { ((b.mmio + r) as *mut u32).write_volatile(v) }
}

/// South bridges with separate PWM registers (by the ISA/LPC bridge id 00:1f.0).
fn split_layout(lpc: u16) -> bool {
    matches!(lpc >> 8, 0x02 | 0x06 | 0x9D | 0xA3 | 0x43 | 0xA0 | 0x4D | 0x38 | 0x51 | 0x54 | 0x7A | 0x7E)
        && !matches!(lpc, 0x9D40..=0x9D5F) // 9D4x/9D5x — Sunrise Point-LP (the old layout)
}

pub fn init(devices: &[pci::Device]) -> String {
    let Some(gpu) = devices.iter().find(|d| d.vendor == 0x8086 && d.class == 0x03 && d.bus == 0) else {
        return "Brightness: no Intel graphics".into();
    };
    let lpc = devices.iter().find(|d| d.vendor == 0x8086 && d.class == 0x06 && d.subclass == 0x01).map_or(0, |d| d.device);
    let mmio = gpu.mmio(0, 2 << 20);
    let split = split_layout(lpc);
    let mut b = Backlight { mmio, split, max: 0 };
    let ctl = rd(&b, CTL);
    b.max = if split { rd(&b, CTL2) } else { rd(&b, CTL2) >> 16 };
    crate::kprintln!(
        "backlight: gpu {:04x}, pch {lpc:04x} ({} layout), ctl {ctl:#x}, max {}",
        gpu.device,
        if split { "CNP" } else { "SPT" },
        b.max
    );
    if ctl & 1 << 31 == 0 || b.max == 0 {
        return "Brightness: backlight is not controlled by the GPU".into();
    }
    let steps = b.max;
    *BL.lock() = Some(b);
    let keys = if open_asle(gpu) { ", keys through the firmware" } else { "" };
    report_level();
    format!("Brightness: Intel PWM, {steps} steps{keys}")
}

/// The ASLE mailbox: if present — "the driver is ready, we handle brightness".
fn open_asle(gpu: &pci::Device) -> bool {
    let asls = gpu.read(0xFC) as u64;
    if asls == 0 {
        return false;
    }
    crate::paging::map_mmio(asls, OPREGION_SIZE);
    let base = asls as usize;
    let at = |off: usize| unsafe { ((base + off) as *const u32).read_volatile() };
    let put = |off: usize, v: u32| unsafe { ((base + off) as *mut u32).write_volatile(v) };
    let signature: [u8; 16] = core::array::from_fn(|i| unsafe { ((base + i) as *const u8).read_volatile() });
    if &signature != OPREGION_SIGNATURE || at(OPREGION_MBOXES) & MBOX_ASLE == 0 {
        return false;
    }
    put(ACPI_MBOX + ACPI_CSTS, 0);
    put(ACPI_MBOX + ACPI_DRDY, 1);
    put(ASLE_MBOX + ASLE_TCHE, at(ASLE_MBOX + ASLE_TCHE) | TCHE_BACKLIGHT);
    put(ASLE_MBOX + ASLE_ARDY, 1);
    *ASLE.lock() = Some(base + ASLE_MBOX);
    crate::kprintln!("backlight: OpRegion at {asls:#x}, brightness keys come through it");
    true
}

/// A brightness key pressed through the firmware: Some(the brightness in percent being requested).
/// The request is cleared: from here on whoever asked handles it (like an ordinary key).
pub fn requested() -> Option<u32> {
    let asle = (*ASLE.lock())?;
    let at = |off: usize| unsafe { ((asle + off) as *const u32).read_volatile() };
    let put = |off: usize, v: u32| unsafe { ((asle + off) as *mut u32).write_volatile(v) };
    let aslc = at(ASLE_ASLC);
    if aslc & ASLC_SET_BACKLIGHT == 0 {
        return None;
    }
    let bclp = at(ASLE_BCLP);
    let ok = bclp & VALID != 0;
    put(ASLE_ASLC, if ok { 0 } else { ASLC_BACKLIGHT_FAILED });
    ok.then(|| (bclp & 0xFF) * 100 / 255)
}

/// Tell the firmware the current brightness: it computes the next key step from it.
fn report_level() {
    let Some(asle) = *ASLE.lock() else { return };
    let Some(percent) = get() else { return };
    unsafe { ((asle + ASLE_CBLV) as *mut u32).write_volatile(VALID | percent.min(100)) };
}

pub fn available() -> bool {
    BL.lock().is_some()
}

/// The brightness in percent (None — unavailable).
pub fn get() -> Option<u32> {
    let bl = BL.lock();
    let b = bl.as_ref()?;
    let duty = if BLANKED.load(Ordering::Relaxed) { SAVED_DUTY.load(Ordering::Relaxed) } else { duty(b) };
    Some((duty as u64 * 100 / b.max as u64) as u32)
}

fn duty(b: &Backlight) -> u32 {
    if b.split { rd(b, DUTY) } else { rd(b, CTL2) & 0xFFFF }
}

fn set_duty(b: &Backlight, duty: u32) {
    if b.split {
        wr(b, DUTY, duty);
    } else {
        wr(b, CTL2, rd(b, CTL2) & 0xFFFF_0000 | duty & 0xFFFF);
    }
}

/// Turns the backlight off completely or brings it back: the screen is not in use.
pub fn blank(off: bool) {
    let bl = BL.lock();
    let Some(b) = bl.as_ref() else { return };
    let was = BLANKED.swap(off, Ordering::Relaxed);
    if was == off {
        return;
    }
    if off {
        SAVED_DUTY.store(duty(b), Ordering::Relaxed);
        set_duty(b, 0);
    } else {
        set_duty(b, SAVED_DUTY.load(Ordering::Relaxed));
    }
}

/// Sets brightness `percent` (no lower than MIN).
pub fn set(percent: u32) {
    let bl = BL.lock();
    let Some(b) = bl.as_ref() else { return };
    let p = percent.clamp(MIN, 100);
    let duty = (b.max as u64 * p as u64 / 100) as u32;
    // A blanked screen will light up with this brightness.
    if BLANKED.load(Ordering::Relaxed) {
        SAVED_DUTY.store(duty, Ordering::Relaxed);
    } else {
        set_duty(b, duty);
    }
    drop(bl);
    report_level();
}
