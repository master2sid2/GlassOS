//! CPU power: deep sleep of the cores when idle, frequency control and sensors
//! (temperature, frequency, watts, time asleep) for the task manager and the `power` command.
//!
//! A plain `hlt` is the shallow C1 sleep: the voltage on the cores and everything around them (the ring bus,
//! the L3 cache) stay on, and a laptop gets warm even without work. So the cores sleep
//! through `mwait` in the deepest state the CPU announced.

use crate::arch::{self, read_msr, write_msr};
use crate::{interrupts, kprintln, pci};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::arch::x86_64::{__cpuid, __cpuid_count};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use crate::sync::IrqLock as Spinlock;

const MSR_PLATFORM_INFO: u32 = 0xCE;
const MSR_MPERF: u32 = 0xE7;
const MSR_APERF: u32 = 0xE8;
const MSR_PKG_CST_CONFIG: u32 = 0xE2;
const MSR_PERF_STATUS: u32 = 0x198;
const MSR_PERF_CTL: u32 = 0x199;
const MSR_PACKAGE_THERM_STATUS: u32 = 0x1B1;
const MSR_TEMPERATURE_TARGET: u32 = 0x1A2;
const MSR_RAPL_UNIT: u32 = 0x606;
const MSR_ENERGY_PACKAGE: u32 = 0x611;
const MSR_ENERGY_CORES: u32 = 0x639;
const MSR_ENERGY_GRAPHICS: u32 = 0x641;
const MSR_ENERGY_PLATFORM: u32 = 0x64D;
const MSR_PM_ENABLE: u32 = 0x770;
const MSR_HWP_CAPABILITIES: u32 = 0x771;
const MSR_HWP_REQUEST: u32 = 0x774;
/// Time in the sleep states of the whole package, in TSC cycles.
const PACKAGE_SLEEP: [(&str, u32); 7] =
    [("PC2", 0x60D), ("PC3", 0x3F8), ("PC6", 0x3F9), ("PC7", 0x3FA), ("PC8", 0x630), ("PC9", 0x631), ("PC10", 0x632)];
/// The same for the core the measurement was taken on.
const CORE_SLEEP: [(&str, u32); 2] = [("C6", 0x3FD), ("C7", 0x3FE)];

/// The "balance speed and saving" preference for the CPU (0 — speed, 255 — saving).
const EPP_BALANCED: u64 = 128;
/// Graphics registers: whether its sleep is enabled, the current state and the accumulated sleep time.
const GT_RC_CONTROL: usize = 0xA090;
const GT_RC_STATE: usize = 0xA094;
const GT_RC6_TIME: usize = 0x138108;

/// The "plain hlt" sleep depth.
const LIGHT: u32 = u32::MAX;
/// The `mwait` hint for the current sleep depth, or LIGHT.
static DEPTH: AtomicU32 = AtomicU32::new(LIGHT);
/// The deepest hint of this CPU (LIGHT — `mwait` cannot be used here).
static DEEPEST: AtomicU32 = AtomicU32::new(LIGHT);
/// The CPU manages its frequency itself (Intel Speed Shift, also known as HWP).
static HWP: AtomicBool = AtomicBool::new(false);
static HWP_FAILED: AtomicUsize = AtomicUsize::new(0);
static GPU_MMIO: AtomicUsize = AtomicUsize::new(0);

/// Which tick we saw last and when by TSC. How long we may see no new ticks
/// after waking before deciding that the timer does not wake us from deep sleep (about two seconds).
static SEEN_TICK: AtomicU64 = AtomicU64::new(0);
static SEEN_AT: AtomicU64 = AtomicU64::new(0);
const NO_TICK_CYCLES: u64 = 4_000_000_000;

fn deepest_hint() -> Option<u32> {
    let intel = {
        let id = __cpuid(0);
        (id.ebx, id.edx, id.ecx) == (0x756E_6547, 0x4965_6E69, 0x6C65_746E)
    };
    if !intel || __cpuid(0).eax < 6 || __cpuid(1).ecx & 1 << 3 == 0 {
        return None;
    }
    // The local APIC must not stop in sleep: the backup alarm relies on it.
    if __cpuid(6).eax & 1 << 2 == 0 {
        return None;
    }
    // Each EDX nibble is how many sub-states a state has; the nibble number is one more than
    // the hint's upper part: 1 — hint 0x0n, 7 — 0x6n (C10).
    let states = __cpuid_count(5, 0).edx;
    let deepest = (1..8u32).rev().find(|i| states >> (i * 4) & 0xF != 0);
    Some(deepest.map_or(0, |i| (i - 1) << 4 | ((states >> (i * 4) & 0xF) - 1)))
}

/// Leaves this logical core's frequency to the CPU's discretion.
fn request_speed() {
    let Some(caps) = read_msr(MSR_HWP_CAPABILITIES) else {
        HWP_FAILED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let (highest, lowest) = (caps & 0xFF, caps >> 24 & 0xFF);
    if !write_msr(MSR_HWP_REQUEST, lowest | highest << 8 | EPP_BALANCED << 24) {
        HWP_FAILED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Called on the main core before the helpers start, once the timer has been checked.
pub fn init() {
    let thermal = __cpuid(6);
    kprintln!(
        "power: firmware left ratio {:#x}, asked {:#x}, HWP enable {:?}, package C limit {:x?}, cpuid.5 {:#x}, cpuid.6 {:#x}",
        read_msr(MSR_PERF_STATUS).map_or(0, |v| v >> 8 & 0xFF),
        read_msr(MSR_PERF_CTL).map_or(0, |v| v >> 8 & 0xFF),
        read_msr(MSR_PM_ENABLE),
        read_msr(MSR_PKG_CST_CONFIG),
        __cpuid_count(5, 0).edx,
        thermal.eax
    );
    if thermal.eax & 1 << 7 != 0 && write_msr(MSR_PM_ENABLE, 1) {
        HWP.store(true, Ordering::Relaxed);
        request_speed();
    }
    interrupts::calibrate_timer();
    if let Some(hint) = deepest_hint()
        && interrupts::arm_backstop()
    {
        DEEPEST.store(hint, Ordering::Relaxed);
        DEPTH.store(hint, Ordering::Relaxed);
    }
    let hint = DEPTH.load(Ordering::Relaxed);
    kprintln!("power: sleep {}, speed {}", depth_text(hint), speed_text());
    *BEFORE.lock() = Some(counters());
}

/// Called on each helper core at start.
pub fn init_helper() {
    if HWP.load(Ordering::Relaxed) {
        request_speed();
    }
}

pub fn attach_gpu(devices: &[pci::Device]) {
    if let Some(gpu) = devices.iter().find(|d| d.vendor == 0x8086 && d.class == 0x03 && d.bus == 0) {
        GPU_MMIO.store(gpu.mmio(0, 2 << 20), Ordering::Relaxed);
    }
}

/// Sleeps until an interrupt as deeply as currently allowed.
pub fn sleep() {
    match DEPTH.load(Ordering::Relaxed) {
        LIGHT => arch::wait_for_interrupt(),
        hint => arch::mwait(hint),
    }
}

/// Idle time of the main core. The timer wakes it; if on this hardware the timer does not wake it from
/// deep sleep, the system wakes on the backup alarm and from then on sleeps with a plain `hlt`.
pub fn idle() {
    // The fast timer wakes us within a fraction of a millisecond anyway: deep sleep costs more here.
    if interrupts::fast() {
        arch::wait_for_interrupt();
        return;
    }
    sleep();
    if DEPTH.load(Ordering::Relaxed) == LIGHT {
        return;
    }
    let (tick, now) = (interrupts::ticks(), arch::rdtsc());
    if tick != SEEN_TICK.load(Ordering::Relaxed) {
        SEEN_TICK.store(tick, Ordering::Relaxed);
        SEEN_AT.store(now, Ordering::Relaxed);
    } else if now - SEEN_AT.load(Ordering::Relaxed) > NO_TICK_CYCLES {
        DEPTH.store(LIGHT, Ordering::Relaxed);
        kprintln!("power: the timer does not wake this CPU from deep sleep, back to light sleep");
    }
}

pub fn can_sleep_deep() -> bool {
    DEEPEST.load(Ordering::Relaxed) != LIGHT
}

pub fn sleeping_deep() -> bool {
    DEPTH.load(Ordering::Relaxed) != LIGHT
}

/// The `power light|deep` command. false — deep sleep is not available here.
pub fn set_deep(deep: bool) -> bool {
    let deepest = DEEPEST.load(Ordering::Relaxed);
    if deep && deepest == LIGHT {
        return false;
    }
    DEPTH.store(if deep { deepest } else { LIGHT }, Ordering::Relaxed);
    true
}

fn depth_text(hint: u32) -> String {
    match hint {
        LIGHT => "light (hlt)".into(),
        h => format!("deep (mwait {h:#04x})"),
    }
}

fn speed_text() -> String {
    if !HWP.load(Ordering::Relaxed) {
        return "left as the firmware set it".into();
    }
    match read_msr(MSR_HWP_REQUEST) {
        Some(r) => {
            let failed = HWP_FAILED.load(Ordering::Relaxed);
            let note = if failed > 0 { format!(", {failed} CPUs refused") } else { String::new() };
            format!("chosen by the CPU, {} to {} MHz{note}", (r & 0xFF) * 100, (r >> 8 & 0xFF) * 100)
        }
        None => "chosen by the CPU".into(),
    }
}

// ---------------------------------------------------------------- sensors

#[derive(Clone, Copy, Default)]
struct Counters {
    tick: u64,
    tsc: u64,
    aperf: Option<u64>,
    mperf: Option<u64>,
    energy: [Option<u64>; 4],
    package: [Option<u64>; 7],
    core: [Option<u64>; 2],
}

fn counters() -> Counters {
    Counters {
        tick: interrupts::ticks(),
        tsc: arch::rdtsc(),
        aperf: read_msr(MSR_APERF),
        mperf: read_msr(MSR_MPERF),
        energy: [MSR_ENERGY_PACKAGE, MSR_ENERGY_CORES, MSR_ENERGY_GRAPHICS, MSR_ENERGY_PLATFORM].map(read_msr),
        package: PACKAGE_SLEEP.map(|(_, msr)| read_msr(msr)),
        core: CORE_SLEEP.map(|(_, msr)| read_msr(msr)),
    }
}

/// Readings for the last second. None — this CPU (or virtual machine) has no
/// such sensor.
#[derive(Clone, Copy, Default)]
pub struct Reading {
    pub celsius: Option<u32>,
    pub mhz: Option<u32>,
    /// Milliwatts: the whole package, its cores, the integrated graphics, the whole laptop.
    pub milliwatts: [Option<u32>; 4],
    /// The share of time asleep, in tenths of a percent.
    package: [Option<u32>; 7],
    core: [Option<u32>; 2],
    /// The share of time core 0 was awake, in tenths of a percent.
    busy: Option<u32>,
}

static BEFORE: Spinlock<Option<Counters>> = Spinlock::new(None);
static LATEST: Spinlock<Reading> =
    Spinlock::new(Reading { celsius: None, mhz: None, milliwatts: [None; 4], package: [None; 7], core: [None; 2], busy: None });
static SAMPLES: AtomicU64 = AtomicU64::new(0);
/// The highest frequency over a minute: when idle only the economical one is visible.
static PEAK_MHZ: AtomicU32 = AtomicU32::new(0);

fn celsius() -> Option<u32> {
    let status = read_msr(MSR_PACKAGE_THERM_STATUS)?;
    let target = read_msr(MSR_TEMPERATURE_TARGET)? >> 16 & 0xFF;
    Some((target as u32).saturating_sub((status >> 16 & 0x7F) as u32))
}

fn tenths(part: Option<u64>, before: Option<u64>, total: u64) -> Option<u32> {
    Some((part?.wrapping_sub(before?) * 1000 / total.max(1)).min(1000) as u32)
}

/// Takes the readings. Call once a second on the main core.
/// Bay Trail, Cherry Trail, Avoton and their relatives count energy in other units.
fn atom_energy_units() -> bool {
    let id = __cpuid(1).eax;
    let (family, model) = (id >> 8 & 0xF, (id >> 4 & 0xF) | (id >> 12 & 0xF0));
    family == 6 && matches!(model, 0x37 | 0x4A | 0x4C | 0x4D | 0x5A | 0x5D)
}

pub fn sample() {
    let Some(before) = core::mem::replace(&mut *BEFORE.lock(), Some(counters())) else { return };
    let now = BEFORE.lock().unwrap_or_default();
    let elapsed_ms = (now.tick - before.tick) * 1000 / interrupts::HZ;
    if elapsed_ms == 0 {
        return;
    }
    let energy_bits = read_msr(MSR_RAPL_UNIT).map(|u| u >> 8 & 0x1F);
    let watts = |i: usize| -> Option<u32> {
        // The energy counters are 32-bit and overflow by themselves.
        let spent = (now.energy[i]? as u32).wrapping_sub(before.energy[i]? as u32) as u64;
        let bits = energy_bits?;
        // On Atom (Silvermont, Airmont) the unit is 2^bits microjoules, not 1/2^bits joules.
        let microjoules = if atom_energy_units() { spent << bits } else { spent * 1_000_000 >> bits };
        (spent != 0).then(|| (microjoules / elapsed_ms) as u32)
    };
    let mhz = (|| -> Option<u32> {
        let base = read_msr(MSR_PLATFORM_INFO)? >> 8 & 0xFF;
        let (a, m) = (now.aperf?.wrapping_sub(before.aperf?), now.mperf?.wrapping_sub(before.mperf?));
        (m != 0).then(|| (a * base * 100 / m) as u32)
    })();
    let total = now.tsc - before.tsc;
    let mut package = [None; 7];
    for (i, slot) in package.iter_mut().enumerate() {
        *slot = tenths(now.package[i], before.package[i], total);
    }
    let core = [tenths(now.core[0], before.core[0], total), tenths(now.core[1], before.core[1], total)];
    let busy = tenths(now.mperf, before.mperf, total);
    let reading = Reading { celsius: celsius(), mhz, milliwatts: [0, 1, 2, 3].map(watts), package, core, busy };
    *LATEST.lock() = reading;
    crate::hdmi::watch_watermarks();
    crate::baytrail::watch();
    PEAK_MHZ.fetch_max(mhz.unwrap_or(0), Ordering::Relaxed);
    // A line to the log once a minute: later dmesg shows how the heating went.
    if SAMPLES.fetch_add(1, Ordering::Relaxed) % 60 == 59 && reading.milliwatts[0].is_some() {
        kprintln!("power: {}, peak {} MHz", reading.line(), PEAK_MHZ.swap(0, Ordering::Relaxed));
    }
}

pub fn reading() -> Reading {
    *LATEST.lock()
}

impl Reading {
    /// For metrics: sleep shares (kind, state, tenths of a percent) and the load of core 0.
    pub fn sleep_shares(&self) -> Vec<(&'static str, &'static str, u32)> {
        let package = PACKAGE_SLEEP.iter().zip(self.package).filter_map(|(&(name, _), t)| Some(("package", name, t?)));
        let core = CORE_SLEEP.iter().zip(self.core).filter_map(|(&(name, _), t)| Some(("core", name, t?)));
        package.chain(core).collect()
    }

    pub fn core_busy(&self) -> Option<u32> {
        self.busy
    }
}

/// The CPU name.
pub fn processor() -> String {
    cpu_name()
}

fn watts_text(mw: u32) -> String {
    format!("{}.{} W", mw / 1000, mw / 100 % 10)
}

fn sleep_text(names: &[(&str, u32)], time: &[Option<u32>]) -> String {
    let parts: Vec<String> = names
        .iter()
        .zip(time)
        .filter_map(|(&(name, _), t)| t.filter(|&t| t >= 5).map(|t| format!("{name} {}%", (t + 5) / 10)))
        .collect();
    if parts.is_empty() { "never".into() } else { parts.join(", ") }
}

impl Reading {
    /// Briefly, for the task manager: "58°C  1.9 GHz  4.2 W".
    pub fn short(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(c) = self.celsius {
            parts.push(format!("{c}\u{b0}C"));
        }
        if let Some(mhz) = self.mhz {
            parts.push(format!("{}.{} GHz", mhz / 1000, mhz / 100 % 10));
        }
        if let Some(mw) = self.milliwatts[0] {
            parts.push(watts_text(mw));
        }
        (!parts.is_empty()).then(|| parts.join("  "))
    }

    fn watts_line(&self) -> Option<String> {
        const NAMES: [&str; 4] = ["processor", "its cores", "its graphics", "whole laptop"];
        let parts: Vec<String> =
            NAMES.iter().zip(self.milliwatts).filter_map(|(name, mw)| mw.map(|mw| format!("{name} {}", watts_text(mw)))).collect();
        (!parts.is_empty()).then(|| parts.join(", "))
    }

    fn line(&self) -> String {
        format!(
            "{}; {}; {}; {}; core 0 busy {}%",
            self.short().unwrap_or_default(),
            self.watts_line().unwrap_or_default(),
            sleep_text(&PACKAGE_SLEEP, &self.package),
            sleep_text(&CORE_SLEEP, &self.core),
            self.busy.unwrap_or(0) / 10
        )
    }
}

fn cpu_name() -> String {
    if __cpuid(0x8000_0000).eax < 0x8000_0004 {
        return "unknown".into();
    }
    let mut bytes = Vec::new();
    for leaf in 0x8000_0002..=0x8000_0004u32 {
        let id = __cpuid(leaf);
        for reg in [id.eax, id.ebx, id.ecx, id.edx] {
            bytes.extend_from_slice(&reg.to_le_bytes());
        }
    }
    String::from_utf8_lossy(&bytes).trim_matches(|c: char| c == '\0' || c == ' ').into()
}

/// What to show in the settings and with the `power` command: lines (title, value).
pub fn lines() -> Vec<(&'static str, String)> {
    let r = reading();
    let mut out = alloc::vec![("Processor", cpu_name())];
    if let Some(now) = r.short() {
        out.push(("Now", now));
    }
    if let Some(watts) = r.watts_line() {
        out.push(("Power", watts));
    }
    if let Some(busy) = r.busy {
        out.push(("Core 0 busy", format!("{}.{}%, peak this minute {} MHz", busy / 10, busy % 10, PEAK_MHZ.load(Ordering::Relaxed))));
    }
    out.push(("Speed", speed_text()));
    out.push(("Sleep", depth_text(DEPTH.load(Ordering::Relaxed))));
    if r.package.iter().any(Option::is_some) {
        out.push(("Whole CPU slept", sleep_text(&PACKAGE_SLEEP, &r.package)));
    }
    if r.core.iter().any(Option::is_some) {
        out.push(("Core 0 slept", sleep_text(&CORE_SLEEP, &r.core)));
    }
    if let Some(limit) = read_msr(MSR_PKG_CST_CONFIG) {
        out.push(("Sleep limit", format!("{limit:#x}")));
    }
    let gpu = GPU_MMIO.load(Ordering::Relaxed);
    if gpu != 0 {
        let rd = |reg| crate::hwprobe::rd(gpu, reg);
        out.push(("Graphics", format!("control {:#x}, state {:#x}, rc6 time {}", rd(GT_RC_CONTROL), rd(GT_RC_STATE), rd(GT_RC6_TIME))));
    }
    out
}

/// The `power` command: the same, and to the log. `hwprobe` takes it to the USB stick, nothing
/// has to be copied from the screen.
pub fn report() -> Vec<(&'static str, String)> {
    let out = lines();
    for (name, value) in &out {
        kprintln!("power: {name}: {value}");
    }
    out
}
