//! The trial kernel watchdog: the TCO timer of the Bay Trail chipset.
//!
//! A trial kernel boots once (BootNext). If it hangs hard, the watchdog
//! resets the board, and it boots the main kernel — remote access comes back without
//! the power button. The watchdog thread kicks the timer while the scheduler is alive.
//!
//! The registers come from Intel's chipset documentation (TCO in the ACPI I/O space, the
//! "no reboot" bit in the PMC); the offsets were checked against Linux's iTCO_wdt/lpc_ich.

use crate::arch::{inw, outw};
use crate::{interrupts, pci, task};
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

/// The Bay Trail LPC bridge.
const LPC: (u16, u16) = (0x8086, 0x0F1C);
const LPC_ACPI_BASE: u8 = 0x40;
const LPC_PMC_BASE: u8 = 0x44;
const ACPI_BASE_MASK: u32 = 0xFF80;
const PMC_BASE_MASK: u32 = 0xFFFF_FE00;
/// The PMC register with the NO_REBOOT bit (bit 4).
const PMC_CFG: u64 = 0x08;
const NO_REBOOT: u32 = 1 << 4;

const TCO_OFFSET: u16 = 0x60;
const TCO_RLD: u16 = 0x00;
const TCO1_STS: u16 = 0x04;
const TCO2_STS: u16 = 0x06;
const TCO1_CNT: u16 = 0x08;
const TCO_TMR: u16 = 0x12;
const TCO_TMR_HALT: u16 = 1 << 11;
const TCO_TIMEOUT: u16 = 1 << 3;
const TCO_SECOND_TIMEOUT: u16 = 1 << 1;
/// The timer counts in 0.6 s ticks; 50 ticks are 30 s (the reset comes after 30–60 s).
const TICKS: u16 = 50;
const KICK_SECONDS: u64 = 5;

/// The TCO base, if the watchdog is running.
static TCO: AtomicU16 = AtomicU16::new(0);
/// The previous boot ended with the timer firing.
static FIRED: AtomicBool = AtomicBool::new(false);

/// When the main loop last completed a round (ticks).
static ALIVE: AtomicU64 = AtomicU64::new(0);
/// The main loop has been silent this long — consider the system hung, stop kicking the timer.
const MAIN_SILENT_SECONDS: u64 = 20;

/// A panic: the timer is no longer kicked — the board resets, and after login the report
/// is shown (a panic on another core does not stop the main loop).
static GAVE_UP: AtomicBool = AtomicBool::new(false);

pub fn give_up() {
    GAVE_UP.store(true, Ordering::Relaxed);
}

/// The main loop is alive (called every round).
pub fn alive() {
    ALIVE.store(interrupts::ticks(), Ordering::Relaxed);
}

pub fn fired_last_time() -> bool {
    FIRED.load(Ordering::Relaxed)
}

/// Starts the watchdog on Bay Trail. Not only the trial kernel needs it: the Wi-Fi driver
/// talks to a capricious card, and if it hangs the bus hard, the watchdog thread cannot
/// kick the timer — the board resets itself (the thread kicks the timer every
/// 5 s, so in normal operation it never fires).
pub fn start(devices: &[pci::Device]) {
    let Some(lpc) = devices.iter().find(|d| (d.vendor, d.device) == LPC) else { return };
    let acpi = lpc.read(LPC_ACPI_BASE) & ACPI_BASE_MASK;
    let pmc = lpc.read(LPC_PMC_BASE) & PMC_BASE_MASK;
    // Whether the timer fired last time: the second-timeout bit survives a board
    // reset (cleared by writing a one).
    if acpi != 0 {
        let tco = acpi as u16 + TCO_OFFSET;
        if inw(tco + TCO2_STS) & TCO_SECOND_TIMEOUT != 0 {
            FIRED.store(true, Ordering::Relaxed);
            outw(tco + TCO2_STS, TCO_SECOND_TIMEOUT);
            crate::kprintln!("watchdog: it restarted the board last time (the previous start hung)");
        }
    }
    // On Bay Trail there is a Wi-Fi card (its accesses are the main hang risk) or a trial
    // kernel is running — then we watch; otherwise the board is left alone.
    let risky = crate::net::remote::trial() || devices.iter().any(|d| (d.vendor, d.device) == (0x10EC, 0xB723));
    if !risky {
        return;
    }
    if acpi == 0 || pmc == 0 {
        crate::kprintln!("watchdog: no ACPI or PMC base ({acpi:#x}, {pmc:#x})");
        return;
    }
    let cfg = pmc as u64 + PMC_CFG;
    crate::paging::map_mmio(cfg & !0xFFF, 0x1000);
    let cfg = cfg as *mut u32;
    let before = unsafe { cfg.read_volatile() };
    unsafe { cfg.write_volatile(before & !NO_REBOOT) };
    if unsafe { cfg.read_volatile() } & NO_REBOOT != 0 {
        crate::kprintln!("watchdog: the firmware keeps NO_REBOOT set, a hang will not restart the board");
        return;
    }
    let tco = acpi as u16 + TCO_OFFSET;
    outw(tco + TCO_TMR, inw(tco + TCO_TMR) & 0xFC00 | TICKS);
    outw(tco + TCO1_STS, TCO_TIMEOUT);
    outw(tco + TCO2_STS, TCO_SECOND_TIMEOUT);
    outw(tco + TCO_RLD, 1);
    outw(tco + TCO1_CNT, inw(tco + TCO1_CNT) & !TCO_TMR_HALT);
    // Before the main loop, devices are still starting: give it a minute's head start.
    ALIVE.store(interrupts::ticks() + 60 * interrupts::HZ, Ordering::Relaxed);
    TCO.store(tco, Ordering::Relaxed);
    crate::kprintln!("watchdog: armed, a hang restarts the board in 30-60 s (TCO {tco:#x})");
    task::spawn("watchdog", None, || {
        loop {
            let tco = TCO.load(Ordering::Relaxed);
            if tco == 0 {
                return;
            }
            // Kick only while the main loop (screen, input) is alive: this thread may
            // run on another core when the main one already hangs.
            let quiet = interrupts::ticks().saturating_sub(ALIVE.load(Ordering::Relaxed));
            // And while some CPU is stuck on a lock: it will only get worse.
            if quiet < MAIN_SILENT_SECONDS * interrupts::HZ && !crate::sync::someone_stuck() && !GAVE_UP.load(Ordering::Relaxed) {
                outw(tco + TCO_RLD, 1);
            }
            task::sleep_until(interrupts::ticks() + KICK_SECONDS * interrupts::HZ);
        }
    });
}

/// Stops the timer (before a reboot and shutdown, after "keep" for a kernel).
pub fn stop() {
    let tco = TCO.swap(0, Ordering::Relaxed);
    if tco != 0 {
        outw(tco + TCO1_CNT, inw(tco + TCO1_CNT) | TCO_TMR_HALT);
    }
}
