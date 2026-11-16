//! The Bay Trail screen: the Valleyview GPU (Celeron N2807 in the Gigabyte BRIX and similar).
//!
//! The firmware of these computers knows few modes: for a 2560×1440 monitor it offered only
//! 1280×1024. So we set the mode ourselves, on top of its configuration: the same pipe and the
//! same HDMI port, but our own timings, pixel clock and frame in memory.
//!
//! The pixel clock comes from a PLL in the DPIO block; it and the port lanes are reached over
//! the internal IOSF bus. The enable order and register values are taken from the open
//! i915 driver (Linux, MIT license): they have been proven on these boards for years.
//!
//! The firmware configuration is remembered in full, and we return to it if the new mode does not
//! start. If the new mode is not confirmed, `output` reverts it.

use crate::gfx::Framebuffer;
use crate::hdmi::{Mode, Resolution};
use crate::hwprobe::{deadline, expired, rd, wr};
use crate::pci;
use crate::sync::IrqLock;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::ops::RangeInclusive;

/// Bay Trail GPUs.
const DEVICES: [u16; 4] = [0x0F30, 0x0F31, 0x0F32, 0x0F33];
/// Registers (2 MiB) followed by the GTT page table (2 MiB).
const MMIO_SIZE: u64 = 4 << 20;
const GTT: usize = 2 << 20;
const GTT_FLUSH: usize = 0x10_1008;
const PTE_VALID: u32 = 1;
const PTE_WRITABLE: u32 = 1 << 1;
const PAGE: usize = 4096;
const CACHE_LINE: usize = 64;
/// Our frame lies in the GTT after the firmware frame, but within the smallest aperture.
const FRAME_MIN: usize = 16 << 20;
const FRAME_ALIGN: usize = 1 << 20;
const APERTURE_MIN: usize = 128 << 20;

/// TSC cycles per microsecond: with a margin, the N2807 runs slower.
const CYCLES_PER_US: u64 = 3000;
const PIPE_WAIT_US: u64 = 100_000;
const FRAME_WAIT_US: u64 = 50_000;
/// How long to wait to see frames running: three frames at 50–60 Hz.
const FRAMES_US: u64 = 60_000;
const PLL_SETTLE_US: u64 = 150;
const PLL_LOCK_US: u64 = 1000;
const PORT_READY_US: u64 = 1000;

/// The display block inside the registers.
const DISPLAY: usize = 0x18_0000;

// The internal IOSF bus.
const SB_DOORBELL: usize = DISPLAY + 0x2100;
const SB_DATA: usize = DISPLAY + 0x2104;
const SB_ADDR: usize = DISPLAY + 0x2108;
const SB_BUSY: u32 = 1;
const SB_ALL_BYTES: u32 = 0xF << 4;
const SB_PORT_DPIO: u32 = 0x12;
const SB_PORT_CCK: u32 = 0x14;
const SB_READ: u32 = 0x00;
const SB_WRITE: u32 = 0x01;
const SB_PRIVATE_READ: u32 = 0x06;
const SB_IDLE_US: u64 = 5_000;
const SB_DONE_US: u64 = 10_000;
const DPIO_CONTROL: usize = DISPLAY + 0x2110;
const DPIO_PHY_STATUS: usize = DISPLAY + 0x6240;

// Pipe clock generators.
const DPLL: [usize; 2] = [DISPLAY + 0x6014, DISPLAY + 0x6018];
const DPLL_MD: [usize; 2] = [DISPLAY + 0x601C, DISPLAY + 0x6020];
const DPLL_VCO: u32 = 1 << 31;
const DPLL_EXT_BUFFER: u32 = 1 << 30;
const DPLL_REF_CLOCK: u32 = 1 << 29;
const DPLL_NO_VGA: u32 = 1 << 28;
const DPLL_LOCK: u32 = 1 << 15;
const DPLL_CRI_CLOCK: u32 = 1 << 14;
const DPLL_INTEGRATED_REF: u32 = 1 << 13;
/// Readiness of the port B and C lanes: bits in pipe A's DPLL, zero — ready.
const PORT_READY: [u32; 2] = [0xF, 0xF << 4];

// Pipes: timings, configuration, the primary plane.
const TRANSCODER: [usize; 2] = [DISPLAY + 0x6_0000, DISPLAY + 0x6_1000];
const PIPE_SOURCE: usize = 0x1C;
const VSYNC_SHIFT: usize = 0x28;
const PIPE: [usize; 2] = [DISPLAY + 0x7_0000, DISPLAY + 0x7_1000];
const PIPE_CONF: usize = 0x08;
const PIPE_STAT: usize = 0x24;
const FRAME_COUNT: usize = 0x40;
const PIPE_ENABLE: u32 = 1 << 31;
const PIPE_ACTIVE: u32 = 1 << 30;
const FIFO_UNDERRUN: u32 = 1 << 31;
const PLANE_CTL: usize = 0x180;
const PLANE_LINEAR: usize = 0x184;
const PLANE_STRIDE: usize = 0x188;
const PLANE_SURF: usize = 0x19C;
const PLANE_TILE: usize = 0x1A4;
const PLANE_LIVE: usize = 0x1AC;
const PLANE_ENABLE: u32 = 1 << 31;
const PLANE_FORMAT: u32 = 0xF << 26;
/// Pixel bytes in memory: blue, green, red (as ours) or the other way round.
const PLANE_BGRX: u32 = 6 << 26;
const PLANE_RGBX: u32 = 14 << 26;
const PLANE_TILED: u32 = 1 << 10;
const STRIDE_UNIT: u32 = 64;
const BYTES_PER_PIXEL: u32 = 4;

// Outputs.
const HDMI: [usize; 2] = [DISPLAY + 0x6_1140, DISPLAY + 0x6_1160];
const DP: [usize; 2] = [DISPLAY + 0x6_4100, DISPLAY + 0x6_4200];
const VGA: usize = DISPLAY + 0x6_1100;
const PORT_ENABLE: u32 = 1 << 31;
const PORT_PIPE_B: u32 = 1 << 30;
const PORT_AUDIO: u32 = 1 << 6;
const PORT_VSYNC_HIGH: u32 = 1 << 4;
const PORT_HSYNC_HIGH: u32 = 1 << 3;
const PANEL_FITTER: usize = DISPLAY + 0x6_1230;
const FITTER_ENABLE: u32 = 1 << 31;
const FITTER_PIPE_SHIFT: u32 = 29;
const HOTPLUG_ENABLE: usize = DISPLAY + 0x6_1110;
const HOTPLUG_STATUS: usize = DISPLAY + 0x6_1114;
/// A monitor on the port B and C connector.
const LIVE: [u32; 2] = [1 << 29, 1 << 28];
const VIDEO_DIP: [usize; 2] = [DISPLAY + 0x6_0200, DISPLAY + 0x6_1170];
/// The DDC bus (GMBUS) and its line pairs: for ports B and C, for VGA.
const GMBUS: usize = DISPLAY + 0x5100;
const DDC_PIN: [u32; 2] = [5, 4];
const DDC_PIN_VGA: u32 = 2;
const EDID_ADDR: u32 = 0x50;
const EDID_BLOCK: usize = 128;
const EDID_HEADER: [u8; 8] = [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0];
const EDID_EXTENSIONS: usize = 126;

// Display buffer (FIFO) watermarks: when a plane asks memory for the next data.
const ARBITER: usize = DISPLAY + 0x7_0030;
const ARBITER2: usize = DISPLAY + 0x7_0060;
const WATERMARKS: usize = DISPLAY + 0x7_0034;
const WATERMARKS_HIGH: usize = DISPLAY + 0x7_0064;
const DRAIN_LATENCY: [usize; 2] = [DISPLAY + 0x7_0050, DISPLAY + 0x7_0054];
/// The drain latency of all four planes, as i915 sets it: "high precision, 2".
/// The firmware computes it from the pixel clock (37 for 108 MHz); with our higher clock
/// such a value makes the plane ask for data late, and the FIFO runs empty.
const DRAIN_ALL: u32 = 0x8282_8282;
const SELF_REFRESH: usize = DISPLAY + 0x6500;
const SELF_REFRESH_ON: u32 = 1 << 15;
const FIFO_SIZE: u32 = 511;
/// The buffer of both pipes while memory is in self-refresh.
const SR_FIFO_SIZE: u32 = 1023;
const SR_SHIFT: u32 = 23;
const SR_HIGH_SHIFT: u32 = 24;
const FIFO_LINE: u64 = 64;
/// How long memory may not answer the display, in tenths of a microsecond.
const MEMORY_LATENCY: u64 = 30;

// DPIO registers (addresses on the IOSF bus): PLLs by pipe number, lanes by port.
const PLL_STEP: u32 = 0x20;
const PLL_DW3: u32 = 0x800C;
const PLL_DW5: u32 = 0x8014;
const PLL_DW7: u32 = 0x801C;
const PLL_DW8: u32 = 0x8040;
const PLL_DW9: u32 = 0x8044;
const PLL_DW10: u32 = 0x8048;
const PLL_DW11: u32 = 0x804C;
const PLL_DW9_ALL: u32 = 0xC044;
const REF_DW13: u32 = 0x80AC;
const COMMON_DW0: u32 = 0x8100;
const LANE_STEP: u32 = 0x200;
const PCS_DW0: u32 = 0x8200;
const PCS_DW1: u32 = 0x8204;
const PCS_DW8: u32 = 0x8220;
const PCS_DW9: u32 = 0x8224;
const PCS_DW11: u32 = 0x822C;
const PCS_DW12: u32 = 0x8230;
const PCS_DW14: u32 = 0x8238;
const PCS_DW23: u32 = 0x825C;
const TX_DW2: u32 = 0x8288;
const TX_DW3: u32 = 0x828C;
const TX_DW4: u32 = 0x8290;
const TX_DW5: u32 = 0x8294;
const TX_DW11: u32 = 0x82AC;
const TX_DW14: u32 = 0x82B8;
const TX3_DW4: [u32; 2] = [0x0690, 0x2A90];
/// Port lanes: reset released, clocking enabled.
const LANES_OUT_OF_RESET: u32 = 1 << 16 | 1 << 7;
const LANE_CLOCKS: u32 = 1 << 22 | 1 << 21 | 1 << 6 | 1 << 5;
/// Lanes in reset: this is how disabling leaves them.
const LANES_RESET: (u32, u32) = (0, 0x00E0_0060);
const PCS_CLOCK_CHANNEL_B: u32 = 1 << 21;
const TX_CALIBRATE: u32 = 1 << 31;

// PLL dividers in DW3.
const DIV_M1_SHIFT: u32 = 8;
const DIV_N_SHIFT: u32 = 12;
const DIV_P2_SHIFT: u32 = 16;
const DIV_P1_SHIFT: u32 = 21;
const DIV_K: u32 = 1 << 24;
const POST_DIV_DIGITAL: u32 = 1 << 28;
const CALIBRATE: u32 = 1 << 11;
const REF_KHZ: u64 = 100_000;
const VCO_KHZ: RangeInclusive<u64> = 4_000_000..=6_000_000;
const DOT_KHZ: RangeInclusive<u64> = 25_000..=270_000;
const M2: RangeInclusive<u64> = 11..=156;
/// The PLL frequency step is no less than 19.2 MHz: this limits the n divider.
const MIN_UPDATE_KHZ: u64 = 19_200;

/// The highest pixel clock on this GPU's HDMI output.
const HDMI_MAX_KHZ: u64 = 225_000;
/// The display core clock if it cannot be read: the lowest possible one.
const CDCLK_FALLBACK_KHZ: u64 = 200_000;
/// Modes we invent ourselves (the native size at a lower refresh rate) are taken with a margin.
const SLOWER_MARGIN_PERCENT: u64 = 95;
const SLOWER_RATES: [u32; 2] = [60, 50];

fn pause_us(us: u64) {
    let end = deadline(us * CYCLES_PER_US);
    while !expired(end) {
        core::hint::spin_loop();
    }
}

/// Waits until the `mask` bits of a register become `value`.
fn wait(mmio: usize, reg: usize, mask: u32, value: u32, us: u64) -> bool {
    let end = deadline(us * CYCLES_PER_US);
    loop {
        if rd(mmio, reg) & mask == value {
            return true;
        }
        if expired(end) {
            return false;
        }
        core::hint::spin_loop();
    }
}

/// Waits for the start of the pipe's next frame (if it is running).
fn wait_frame(mmio: usize, pipe: usize) {
    let reg = PIPE[pipe] + FRAME_COUNT;
    let first = rd(mmio, reg);
    let end = deadline(FRAME_WAIT_US * CYCLES_PER_US);
    while rd(mmio, reg) == first && !expired(end) {
        core::hint::spin_loop();
    }
}

/// One transaction on the IOSF bus. None — the bus did not answer.
/// There is one IOSF bus for everyone (display, GPU): from two CPUs at once — only in turn.
static SIDEBAND: IrqLock<()> = IrqLock::new(());

fn sideband(mmio: usize, port: u32, opcode: u32, addr: u32, value: u32) -> Option<u32> {
    let _guard = SIDEBAND.lock();
    if !wait(mmio, SB_DOORBELL, SB_BUSY, 0, SB_IDLE_US) {
        return None;
    }
    wr(mmio, SB_ADDR, addr);
    wr(mmio, SB_DATA, value);
    wr(mmio, SB_DOORBELL, opcode << 16 | port << 8 | SB_ALL_BYTES | SB_BUSY);
    wait(mmio, SB_DOORBELL, SB_BUSY, 0, SB_DONE_US).then(|| rd(mmio, SB_DATA))
}

fn dpio(mmio: usize, reg: u32) -> u32 {
    sideband(mmio, SB_PORT_DPIO, SB_READ, reg, 0).unwrap_or(!0)
}

fn set_dpio(mmio: usize, reg: u32, value: u32) {
    if sideband(mmio, SB_PORT_DPIO, SB_WRITE, reg, value).is_none() {
        crate::kprintln!("baytrail: DPIO {reg:#x} did not take {value:#010x}");
    }
}

/// The display core clock (CCK block): a divider of the HPLL frequency, in kHz.
fn cdclk_khz(mmio: usize) -> Option<u64> {
    const FUSE: u32 = 0x08;
    const CLOCK_CONTROL: u32 = 0x6B;
    const HPLL_MHZ: [u64; 4] = [800, 1600, 2000, 2400];
    let fuse = sideband(mmio, SB_PORT_CCK, SB_PRIVATE_READ, FUSE, 0)?;
    let control = sideband(mmio, SB_PORT_CCK, SB_PRIVATE_READ, CLOCK_CONTROL, 0)?;
    let vco = HPLL_MHZ[(fuse & 3) as usize] * 1000;
    let divider = (control & 0x1F) as u64 + 1;
    Some((vco * 2 + divider / 2) / divider)
}

/// PLL dividers: a 100 MHz reference × m1·m2 / n gives the VCO frequency, which is divided by p1·p2·5.
#[derive(Clone, Copy)]
struct Dividers {
    n: u64,
    m1: u64,
    m2: u64,
    p1: u64,
    p2: u64,
}

impl Dividers {
    fn from_register(v: u32) -> Self {
        let field = |shift: u32, mask: u32| (v >> shift & mask) as u64;
        Self { n: field(DIV_N_SHIFT, 0xF), m1: field(DIV_M1_SHIFT, 7), m2: field(0, 0xFF), p1: field(DIV_P1_SHIFT, 7), p2: field(DIV_P2_SHIFT, 0x1F) }
    }

    fn register(&self) -> u32 {
        let (n, m1, m2, p1, p2) = (self.n as u32, self.m1 as u32, self.m2 as u32, self.p1 as u32, self.p2 as u32);
        m2 | m1 << DIV_M1_SHIFT | n << DIV_N_SHIFT | p2 << DIV_P2_SHIFT | p1 << DIV_P1_SHIFT | DIV_K | POST_DIV_DIGITAL
    }

    fn p(&self) -> u64 {
        self.p1 * self.p2 * 5
    }

    fn vco(&self) -> u64 {
        if self.n == 0 { 0 } else { (REF_KHZ * self.m1 * self.m2 + self.n / 2) / self.n }
    }

    /// Pixel clock, kHz; zero — the dividers are not set.
    fn khz(&self) -> u64 {
        let p = self.p();
        if p == 0 { 0 } else { (self.vco() + p / 2) / p }
    }

    /// The dividers closest to `target`, chosen as i915 does: with an error below 100 parts per million
    /// a larger p is preferred, otherwise the smaller error.
    fn best(target: u64) -> Option<Self> {
        let mut best: Option<(Self, u64)> = None;
        for n in 1..=REF_KHZ / MIN_UPDATE_KHZ {
            for p1 in [3, 2] {
                let mut p2 = 20;
                while p2 >= 2 {
                    for m1 in [2, 3] {
                        let p = p1 * p2 * 5;
                        let m2 = (target * p * n + REF_KHZ * m1 / 2) / (REF_KHZ * m1);
                        let d = Self { n, m1, m2, p1, p2 };
                        if !M2.contains(&m2) || !VCO_KHZ.contains(&d.vco()) || !DOT_KHZ.contains(&d.khz()) {
                            continue;
                        }
                        let ppm = 1_000_000 * d.khz().abs_diff(target) / target;
                        let (best_p, best_error) = best.map_or((0, 1_000_000), |(b, e)| (b.p(), e));
                        if ppm < 100 && d.p() > best_p {
                            best = Some((d, 0));
                        } else if ppm + 10 < best_error {
                            best = Some((d, ppm));
                        }
                    }
                    p2 -= if p2 > 10 { 2 } else { 1 };
                }
            }
        }
        best.map(|(d, _)| d)
    }
}

/// Pipe timings: total, blanking and sync for lines and for the frame.
fn timing_registers(m: &Mode) -> [u32; 6] {
    let pair = |end: u32, start: u32| (end - 1) << 16 | (start - 1);
    [
        pair(m.htotal, m.width),
        pair(m.htotal, m.width),
        pair(m.hsync_end, m.hsync_start),
        pair(m.vtotal, m.height),
        pair(m.vtotal, m.height),
        pair(m.vsync_end, m.vsync_start),
    ]
}

/// The mode the firmware set: from the timings, the PLL frequency and the port polarity.
fn mode_of(regs: &[u32; 6], khz: u64, port: u32) -> Mode {
    let low = |v: u32| (v & 0x1FFF) + 1;
    let high = |v: u32| (v >> 16 & 0x1FFF) + 1;
    Mode {
        pixel_khz: khz,
        width: low(regs[0]),
        height: low(regs[3]),
        htotal: high(regs[0]),
        hsync_start: low(regs[2]),
        hsync_end: high(regs[2]),
        vtotal: high(regs[3]),
        vsync_start: low(regs[5]),
        vsync_end: high(regs[5]),
        positive_h: port & PORT_HSYNC_HIGH != 0,
        positive_v: port & PORT_VSYNC_HIGH != 0,
    }
}

fn name(m: &Mode) -> String {
    let r = m.resolution();
    format!("{}x{}@{}", r.w, r.h, r.hz)
}

/// How many FIFO lines (64 bytes each) the plane must have in reserve while memory is silent.
fn fifo_need(m: &Mode) -> u32 {
    let lines = MEMORY_LATENCY * m.pixel_khz / (m.htotal as u64 * 10_000);
    ((lines + 1) * m.width as u64 * BYTES_PER_PIXEL as u64).div_ceil(FIFO_LINE) as u32
}

/// Plane (and memory self-refresh) watermarks for mode `m`. The registers hold
/// the free buffer space at which the plane asks for data: if the firmware left
/// more than this mode allows, we reduce it.
fn watermarks(mmio: usize, pipe: usize, (mut low, mut high): (u32, u32), m: &Mode) -> (u32, u32) {
    let need = fifo_need(m);
    let (arbiter, arbiter2) = (rd(mmio, ARBITER), rd(mmio, ARBITER2));
    let (shift, high_shift, fifo) = match pipe {
        0 => (0, 0, arbiter & 0xFF | (arbiter2 & 1) << 8),
        _ => (8, 12, arbiter >> 16 & 0xFF | (arbiter2 >> 8 & 1) << 8),
    };
    let plane = (low >> shift & 0xFF) | (high >> high_shift & 1) << 8;
    if fifo > need && fifo <= FIFO_SIZE && plane > fifo - need {
        let value = fifo - need;
        low = low & !(0xFF << shift) | (value & 0xFF) << shift;
        high = high & !(1 << high_shift) | (value >> 8 & 1) << high_shift;
    }
    if rd(mmio, SELF_REFRESH) & SELF_REFRESH_ON != 0 {
        let sr = (low >> SR_SHIFT & 0x1FF) | (high >> SR_HIGH_SHIFT & 1) << 9;
        if sr > SR_FIFO_SIZE - need {
            let value = SR_FIFO_SIZE - need;
            low = low & !(0x1FF << SR_SHIFT) | (value & 0x1FF) << SR_SHIFT;
            high = high & !(1 << SR_HIGH_SHIFT) | (value >> 9 & 1) << SR_HIGH_SHIFT;
        }
    }
    (low, high)
}

/// The monitor's EDID over the port's DDC lines, both blocks. If that fails — the one the firmware gave.
fn read_edid(mmio: usize, port: usize) -> (Vec<u8>, &'static str) {
    let read = |offset: u32| crate::hwprobe::ddc_read_on(mmio, GMBUS, DDC_PIN[port], EDID_ADDR, offset, EDID_BLOCK);
    match read(0) {
        Ok(mut edid) if edid[..8] == EDID_HEADER => {
            if edid[EDID_EXTENSIONS] > 0
                && let Ok(block) = read(EDID_BLOCK as u32)
            {
                edid.extend(block);
            }
            (edid, "DDC")
        }
        _ => (crate::display::edid(), "the firmware"),
    }
}

/// Refresh rates the monitor accepts (descriptor 0xFD in the EDID).
fn vertical_range(edid: &[u8]) -> Option<(u32, u32)> {
    const RANGE_TAG: u8 = 0xFD;
    let found = edid.get(54..126)?.chunks_exact(18).find(|d| d[..3] == [0, 0, 0] && d[3] == RANGE_TAG)?;
    let low = found[5] as u32 + if found[4] & 2 != 0 { 255 } else { 0 };
    let high = found[6] as u32 + if found[4] & 1 != 0 { 255 } else { 0 };
    Some((low, high))
}

/// The monitor's native size at a lower refresh rate, if the native mode is too much for the output.
fn native_slower(edid: &[u8], limit: u64) -> Option<Mode> {
    let (w, h, khz) = crate::display::native(edid)?;
    let (min_hz, _) = vertical_range(edid)?;
    if khz as u64 <= limit {
        return None;
    }
    SLOWER_RATES
        .into_iter()
        .filter(|&hz| hz >= min_hz)
        .map(|hz| crate::hdmi::reduced_blanking(w, h, hz))
        .find(|m| m.pixel_khz <= limit * SLOWER_MARGIN_PERCENT / 100)
}

/// Modes to choose from, besides the firmware mode: what the monitor offers and the output can drive.
fn offered(edid: &[u8], limit: u64, firmware: &Mode) -> Vec<Mode> {
    let mut found = crate::hdmi::modes_from(edid, limit);
    found.extend(native_slower(edid, limit));
    let mut unique: Vec<Mode> = Vec::new();
    for m in found {
        if m.resolution() != firmware.resolution() && !unique.iter().any(|u| u.resolution() == m.resolution()) {
            unique.push(m);
        }
    }
    unique.sort_by_key(|m| {
        let r = m.resolution();
        core::cmp::Reverse((r.w * r.h, r.hz))
    });
    unique
}

/// The primary plane: configuration, offset, line stride, frame address in the GTT, tile offset.
#[derive(Clone, Copy)]
struct Plane {
    ctl: u32,
    linear: u32,
    stride: u32,
    surface: u32,
    tile: u32,
}

/// Everything a mode change affects: output register values.
#[derive(Clone, Copy)]
struct Setup {
    timings: [u32; 6],
    source: u32,
    conf: u32,
    port: u32,
    /// PLL dividers (DW3) without the calibration bit.
    pll: u32,
    fitter: u32,
    plane: Plane,
    watermarks: (u32, u32),
    /// FIFO drain latency (DDL) of the pipe's planes.
    drain: u32,
}

#[derive(Clone)]
struct Display {
    mmio: usize,
    /// The pipe (0 — A) and HDMI port (0 — B) the firmware enabled.
    pipe: usize,
    port: usize,
    /// The frame access window (BAR 2) and the place of our frame in the GTT.
    aperture: usize,
    frame_gtt: usize,
    /// Our frame's memory and its size.
    memory: Option<(usize, usize)>,
    /// The firmware's scaling on our pipe: then we switch it too.
    fitter: bool,
    firmware: Setup,
    firmware_mode: Mode,
    firmware_frame: Framebuffer,
    modes: Vec<Mode>,
    /// Our mode and its frame; None — the firmware mode.
    current: Option<(Mode, Framebuffer)>,
    /// Registers of the mode now on screen: the monitor wakes up with them.
    setup: Setup,
    /// The signal to the monitor is off (the monitor goes to sleep and turns off its backlight).
    asleep: bool,
    limit_khz: u64,
    cdclk_khz: Option<u64>,
    edid_from: &'static str,
    edid: Vec<u8>,
}

static DISPLAY_STATE: IrqLock<Option<Display>> = IrqLock::new(None);
/// How the last attempt ended: for the report.
static LAST: IrqLock<String> = IrqLock::new(String::new());
/// How many times the display ran out of data (the FIFO ran empty) and when it was logged.
static UNDERRUNS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static UNDERRUN_TOLD: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Underruns are logged at most once every 10 seconds.
const UNDERRUN_LOG_EVERY: u64 = 10 * crate::interrupts::HZ;

fn note(text: String) {
    crate::kprintln!("baytrail: {text}");
    *LAST.lock() = text;
}

impl Display {
    /// How the firmware configured the output. An error — the output is not ours (not HDMI or not understood).
    fn read(gpu: &pci::Device, mmio: usize, firmware_frame: Framebuffer) -> Result<Self, String> {
        let pipe = (0..2).find(|&p| rd(mmio, PIPE[p] + PIPE_CONF) & PIPE_ENABLE != 0).ok_or("no pipe is running")?;
        let on_pipe = |v: u32| v & PORT_ENABLE != 0 && (v & PORT_PIPE_B != 0) == (pipe == 1);
        let Some(port) = (0..2).find(|&i| on_pipe(rd(mmio, HDMI[i]))) else {
            let other = if on_pipe(rd(mmio, VGA)) {
                "VGA"
            } else if (0..2).any(|i| on_pipe(rd(mmio, DP[i]))) {
                "DisplayPort"
            } else {
                "an unknown output"
            };
            return Err(format!("the firmware shows the picture on {other}, not HDMI"));
        };
        let (p, t) = (PIPE[pipe], TRANSCODER[pipe]);
        let ctl = rd(mmio, p + PLANE_CTL);
        if ctl & PLANE_ENABLE == 0 || !matches!(ctl & PLANE_FORMAT, PLANE_BGRX | PLANE_RGBX) {
            return Err(format!("unexpected plane setup {ctl:#010x}"));
        }
        let dpll = rd(mmio, DPLL[pipe]);
        let dw3 = dpio(mmio, PLL_DW3 + pipe as u32 * PLL_STEP);
        let khz = Dividers::from_register(dw3).khz();
        if dpll & DPLL_VCO == 0 || khz == 0 || dw3 == !0 {
            return Err(format!("cannot read the PLL: DPLL {dpll:08x}, DW3 {dw3:08x}"));
        }
        let timings: [u32; 6] = core::array::from_fn(|i| rd(mmio, t + 4 * i));
        let port_value = rd(mmio, HDMI[port]);
        let firmware_mode = mode_of(&timings, khz, port_value);
        let fitter_value = rd(mmio, PANEL_FITTER);
        let fitter = fitter_value & FITTER_ENABLE != 0 && (fitter_value >> FITTER_PIPE_SHIFT & 3) as usize == pipe;
        let plane = Plane {
            ctl,
            linear: rd(mmio, p + PLANE_LINEAR),
            stride: rd(mmio, p + PLANE_STRIDE),
            surface: rd(mmio, p + PLANE_SURF),
            tile: rd(mmio, p + PLANE_TILE),
        };
        let firmware = Setup {
            timings,
            source: rd(mmio, t + PIPE_SOURCE),
            conf: rd(mmio, p + PIPE_CONF),
            port: port_value,
            pll: dw3 & !CALIBRATE,
            fitter: fitter_value,
            plane,
            watermarks: (rd(mmio, WATERMARKS), rd(mmio, WATERMARKS_HIGH)),
            drain: rd(mmio, DRAIN_LATENCY[pipe]),
        };
        let cdclk = cdclk_khz(mmio);
        let limit_khz = HDMI_MAX_KHZ.min(cdclk.unwrap_or(CDCLK_FALLBACK_KHZ) * 9 / 10);
        let (edid, edid_from) = read_edid(mmio, port);
        let modes = offered(&edid, limit_khz, &firmware_mode);
        let used = plane.surface as usize + (plane.stride * firmware_mode.height) as usize;
        let frame_gtt = used.next_multiple_of(FRAME_ALIGN).max(FRAME_MIN);
        let aperture = gpu.bar(2) as usize;
        Ok(Self {
            mmio,
            pipe,
            port,
            aperture,
            frame_gtt,
            memory: None,
            fitter,
            firmware,
            firmware_mode,
            firmware_frame,
            modes,
            current: None,
            setup: firmware,
            asleep: false,
            limit_khz,
            cdclk_khz: cdclk,
            edid_from,
            edid,
        })
    }

    fn screen(&self) -> Framebuffer {
        self.current.map_or(self.firmware_frame, |(_, frame)| frame)
    }

    fn describe(&self) -> String {
        let pipe = if self.pipe == 0 { 'A' } else { 'B' };
        let port = if self.port == 0 { 'B' } else { 'C' };
        let d = Dividers::from_register(self.firmware.pll);
        format!(
            "firmware: pipe {pipe}, HDMI {port}, {} ({} kHz, n {} m {}*{} p {}*{}), surface {:#x} stride {}, cdclk {}",
            name(&self.firmware_mode),
            self.firmware_mode.pixel_khz,
            d.n,
            d.m1,
            d.m2,
            d.p1,
            d.p2,
            self.firmware.plane.surface,
            self.firmware.plane.stride,
            self.cdclk_khz.map_or("unknown".into(), |k| format!("{k} kHz")),
        )
    }

    /// Sets the output from `s`: disables everything in reverse order and enables it again, as i915 does.
    fn program(&self, s: &Setup) -> Result<(), String> {
        let (m, pipe) = (self.mmio, self.pipe);
        let (p, t, port) = (PIPE[pipe], TRANSCODER[pipe], HDMI[self.port]);
        let (pll, lane) = (pipe as u32 * PLL_STEP, self.port as u32 * LANE_STEP);
        let idle = DPLL_REF_CLOCK | DPLL_INTEGRATED_REF | DPLL_NO_VGA | if pipe == 1 { DPLL_CRI_CLOCK } else { 0 };

        // Disable: plane, port, pipe, port lanes, PLL (the reference clock stays).
        wr(m, p + PLANE_CTL, rd(m, p + PLANE_CTL) & !PLANE_ENABLE);
        wr(m, p + PLANE_SURF, rd(m, p + PLANE_SURF));
        wait_frame(m, pipe);
        wr(m, port, rd(m, port) & !(PORT_ENABLE | PORT_AUDIO));
        let _ = rd(m, port);
        wr(m, p + PIPE_CONF, rd(m, p + PIPE_CONF) & !PIPE_ENABLE);
        if !wait(m, p + PIPE_CONF, PIPE_ACTIVE, 0, PIPE_WAIT_US) {
            return Err("the pipe did not stop".into());
        }
        if self.fitter {
            wr(m, PANEL_FITTER, 0);
        }
        // Without a lane reset after a clock change HDMI flickers.
        set_dpio(m, PCS_DW0 + lane, LANES_RESET.0);
        set_dpio(m, PCS_DW1 + lane, LANES_RESET.1);
        wr(m, DPLL[pipe], idle);
        let _ = rd(m, DPLL[pipe]);

        // Pipe timings and configuration, the port still off, lanes back to their initial values.
        for (i, &v) in s.timings.iter().enumerate() {
            wr(m, t + 4 * i, v);
        }
        wr(m, t + VSYNC_SHIFT, 0);
        wr(m, t + PIPE_SOURCE, s.source);
        wr(m, p + PIPE_CONF, s.conf & !PIPE_ENABLE);
        wr(m, port, s.port & !(PORT_ENABLE | PORT_AUDIO));
        let _ = rd(m, port);
        set_dpio(m, PCS_DW0 + lane, LANES_OUT_OF_RESET);
        set_dpio(m, PCS_DW1 + lane, LANE_CLOCKS);
        set_dpio(m, PCS_DW12 + lane, 0x0075_0F00);
        set_dpio(m, TX_DW11 + lane, 0x0000_1500);
        set_dpio(m, TX_DW14 + lane, 0x4040_0000);

        // PLL: dividers, calibration, filter; then enable and wait for lock.
        if pipe == 1 {
            recalibrate_pll_b(m);
        }
        set_dpio(m, PLL_DW9_ALL, 0x0100_000F);
        let dw8 = dpio(m, PLL_DW8 + pll);
        set_dpio(m, PLL_DW8 + pll, dw8 & 0x00FF_FFFF);
        set_dpio(m, COMMON_DW0, 0x610);
        set_dpio(m, PLL_DW3 + pll, s.pll);
        set_dpio(m, PLL_DW3 + pll, s.pll | CALIBRATE);
        set_dpio(m, PLL_DW10 + pll, 0x009F_0003);
        set_dpio(m, PLL_DW5 + pll, if pipe == 0 { 0x0DF7_0000 } else { 0x0DF4_0000 });
        let dw7 = dpio(m, PLL_DW7 + pll);
        set_dpio(m, PLL_DW7 + pll, dw7 & 0xFF00 | 0x01C0_0000);
        set_dpio(m, PLL_DW11 + pll, 0x8787_1000);
        wr(m, DPLL[pipe], idle | DPLL_VCO | DPLL_EXT_BUFFER);
        let _ = rd(m, DPLL[pipe]);
        pause_us(PLL_SETTLE_US);
        if !wait(m, DPLL[pipe], DPLL_LOCK, DPLL_LOCK, PLL_LOCK_US) {
            return Err(format!("the PLL did not lock, DPLL {:08x}", rd(m, DPLL[pipe])));
        }
        wr(m, DPLL_MD[pipe], 0);

        // Port lanes: clock channels and the HDMI signal level (1.0 V, −2 dB). Then the port.
        set_dpio(m, PCS_DW8 + lane, 0x0010_00C4 | if pipe == 1 { PCS_CLOCK_CHANNEL_B } else { 0 });
        set_dpio(m, PCS_DW14 + lane, 0x0076_0018);
        set_dpio(m, PCS_DW23 + lane, 0x0040_0888);
        set_dpio(m, TX_DW5 + lane, 0);
        set_dpio(m, TX_DW4 + lane, 0x2B24_5F5F);
        set_dpio(m, TX_DW2 + lane, 0x5578_B83A);
        set_dpio(m, TX_DW3 + lane, 0x0C78_2040);
        set_dpio(m, TX3_DW4[self.port], 0x2B24_7878);
        set_dpio(m, PCS_DW11 + lane, 0x0003_0000);
        set_dpio(m, PCS_DW9 + lane, 0x0000_2000);
        set_dpio(m, TX_DW5 + lane, TX_CALIBRATE);
        wr(m, port, s.port | PORT_ENABLE);
        let _ = rd(m, port);
        if !wait(m, DPLL[0], PORT_READY[self.port], 0, PORT_READY_US) {
            crate::kprintln!("baytrail: port lanes not ready, DPLL A {:08x}", rd(m, DPLL[0]));
        }

        // Buffer watermarks, scaling, the pipe and the plane with the new frame.
        wr(m, WATERMARKS, s.watermarks.0);
        wr(m, WATERMARKS_HIGH, s.watermarks.1);
        wr(m, DRAIN_LATENCY[pipe], s.drain);
        if self.fitter {
            wr(m, PANEL_FITTER, s.fitter);
        }
        wr(m, p + PIPE_STAT, rd(m, p + PIPE_STAT) | FIFO_UNDERRUN);
        wr(m, p + PIPE_CONF, s.conf | PIPE_ENABLE);
        if !wait(m, p + PIPE_CONF, PIPE_ACTIVE, PIPE_ACTIVE, PIPE_WAIT_US) {
            return Err("the pipe did not start".into());
        }
        wr(m, p + PLANE_STRIDE, s.plane.stride);
        wr(m, p + PLANE_LINEAR, s.plane.linear);
        wr(m, p + PLANE_TILE, s.plane.tile);
        wr(m, p + PLANE_CTL, s.plane.ctl);
        wr(m, p + PLANE_SURF, s.plane.surface);
        let first = rd(m, p + FRAME_COUNT);
        pause_us(FRAMES_US);
        if rd(m, p + FRAME_COUNT) == first {
            return Err("the pipe shows no frames".into());
        }
        Ok(())
    }

    /// Memory of the new frame: pages in the GTT after the firmware frame, written through the aperture.
    fn map_frame(&mut self, size: usize) -> Result<*mut u32, String> {
        if self.frame_gtt + size > APERTURE_MIN {
            return Err("the frame does not fit into the aperture".into());
        }
        let cpu = self.aperture + self.frame_gtt;
        if self.memory.is_none_or(|(_, have)| have < size) {
            let layout = core::alloc::Layout::from_size_align(size, PAGE).map_err(|_| "bad frame size")?;
            let memory = unsafe { alloc::alloc::alloc_zeroed(layout) };
            if memory.is_null() {
                return Err("no memory for the frame".into());
            }
            // The display reads memory bypassing the cache: the cache must be empty, and the frame
            // may only be written through the aperture.
            for line in (0..size).step_by(CACHE_LINE) {
                unsafe { core::arch::x86_64::_mm_clflush(memory.add(line)) };
            }
            let table = self.mmio + GTT + self.frame_gtt / PAGE * 4;
            for page in 0..size / PAGE {
                let addr = memory as u64 + (page * PAGE) as u64;
                let pte = addr as u32 & 0xFFFF_F000 | (addr >> 28) as u32 & 0xFF0 | PTE_WRITABLE | PTE_VALID;
                unsafe { (table as *mut u32).add(page).write_volatile(pte) };
            }
            wr(self.mmio, GTT_FLUSH, 1);
            let _ = rd(self.mmio, GTT_FLUSH);
            if let Some((old, have)) = self.memory.replace((memory as usize, size))
                && let Ok(was) = core::alloc::Layout::from_size_align(have, PAGE)
            {
                unsafe { alloc::alloc::dealloc(old as *mut u8, was) };
            }
        }
        crate::paging::map_frame(cpu as u64, size as u64);
        unsafe { (cpu as *mut u8).write_bytes(0, size) };
        Ok(cpu as *mut u32)
    }

    /// Enables our mode. If it fails — returns to the firmware mode.
    fn enter(&mut self, mode: Mode) -> Result<Framebuffer, String> {
        let dividers = Dividers::best(mode.pixel_khz).ok_or_else(|| format!("no PLL setting for {} kHz", mode.pixel_khz))?;
        let stride = (mode.width * BYTES_PER_PIXEL).next_multiple_of(STRIDE_UNIT);
        let size = ((stride * mode.height) as usize).next_multiple_of(PAGE);
        let base = self.map_frame(size)?;
        let polarity = if mode.positive_h { PORT_HSYNC_HIGH } else { 0 } | if mode.positive_v { PORT_VSYNC_HIGH } else { 0 };
        let fw = self.firmware;
        let setup = Setup {
            timings: timing_registers(&mode),
            source: (mode.width - 1) << 16 | (mode.height - 1),
            conf: fw.conf,
            port: fw.port & !(PORT_HSYNC_HIGH | PORT_VSYNC_HIGH) | polarity,
            pll: dividers.register(),
            fitter: 0,
            plane: Plane { ctl: fw.plane.ctl & !PLANE_TILED, linear: 0, stride, surface: self.frame_gtt as u32, tile: 0 },
            watermarks: watermarks(self.mmio, self.pipe, fw.watermarks, &mode),
            drain: DRAIN_ALL,
        };
        if let Err(e) = self.program(&setup) {
            self.current = None;
            self.setup = self.firmware;
            if let Err(back) = self.program(&self.firmware) {
                crate::kprintln!("baytrail: the firmware mode did not come back: {back}");
            }
            return Err(e);
        }
        let underrun = rd(self.mmio, PIPE[self.pipe] + PIPE_STAT) & FIFO_UNDERRUN != 0;
        note(format!(
            "{} at {} kHz (n {} m {}*{} p {}*{}), watermarks {:08x} {:08x}{}",
            name(&mode),
            dividers.khz(),
            dividers.n,
            dividers.m1,
            dividers.m2,
            dividers.p1,
            dividers.p2,
            setup.watermarks.0,
            setup.watermarks.1,
            if underrun { ", the display ran out of data" } else { "" }
        ));
        self.setup = setup;
        let bgr = fw.plane.ctl & PLANE_FORMAT == PLANE_BGRX;
        let screen = Framebuffer { base, w: mode.width as i32, h: mode.height as i32, stride: (stride / BYTES_PER_PIXEL) as i32, bgr };
        self.current = Some((mode, screen));
        Ok(screen)
    }

    /// Returns to the firmware mode and its frame.
    fn leave(&mut self) -> Framebuffer {
        if self.current.take().is_some() {
            self.setup = self.firmware;
            match self.program(&self.firmware) {
                Ok(()) => note(format!("back to the firmware mode {}", name(&self.firmware_mode))),
                Err(e) => note(format!("the firmware mode did not come back: {e}")),
            }
        }
        self.firmware_frame
    }
}

impl Display {
    /// Blanks the output: plane, port, pipe. Without an HDMI signal the monitor goes to sleep by itself
    /// (a black picture does not put it to sleep — the backlight stays on).
    fn sleep(&mut self) {
        let (m, p, port) = (self.mmio, PIPE[self.pipe], HDMI[self.port]);
        wr(m, p + PLANE_CTL, rd(m, p + PLANE_CTL) & !PLANE_ENABLE);
        wr(m, p + PLANE_SURF, rd(m, p + PLANE_SURF));
        wait_frame(m, self.pipe);
        wr(m, port, rd(m, port) & !(PORT_ENABLE | PORT_AUDIO));
        let _ = rd(m, port);
        wr(m, p + PIPE_CONF, rd(m, p + PIPE_CONF) & !PIPE_ENABLE);
        if !wait(m, p + PIPE_CONF, PIPE_ACTIVE, 0, PIPE_WAIT_US) {
            crate::kprintln!("baytrail: the pipe did not stop for sleep");
        }
        self.asleep = true;
    }

    /// Wakes the output the same way as a mode change: that way the lanes and the PLL come up properly.
    fn wake(&mut self) {
        self.asleep = false;
        let setup = self.setup;
        if let Err(e) = self.program(&setup) {
            note(format!("the screen did not wake: {e}"));
        }
    }
}

/// PLL B sets its amplifier to the limit by itself: set a reasonable value, as i915 does.
fn recalibrate_pll_b(m: usize) {
    let dw9 = PLL_DW9 + PLL_STEP;
    set_dpio(m, dw9, dpio(m, dw9) & 0xFFFF_FF00 | 0x30);
    set_dpio(m, REF_DW13, dpio(m, REF_DW13) & 0x00FF_FFFF | 0x8C00_0000);
    set_dpio(m, dw9, dpio(m, dw9) & 0xFFFF_FF00);
    set_dpio(m, REF_DW13, dpio(m, REF_DW13) & 0x00FF_FFFF | 0xB000_0000);
}

/// A Bay Trail GPU?
pub fn supports(d: &pci::Device) -> bool {
    d.vendor == 0x8086 && DEVICES.contains(&d.device)
}

/// Looks at how the firmware enabled the screen. Does not change the mode: `switch` does that.
pub fn start(devices: &[pci::Device], firmware: Framebuffer) {
    let Some(gpu) = devices.iter().find(|d| supports(d)) else { return };
    let mmio = gpu.mmio(0, MMIO_SIZE);
    match Display::read(gpu, mmio, firmware) {
        Ok(d) => {
            crate::kprintln!("baytrail: {}", d.describe());
            let list: Vec<String> = d.modes.iter().map(name).collect();
            crate::kprintln!(
                "baytrail: {} (EDID from {}, {} bytes), up to {} kHz, offers {}",
                crate::hwprobe::monitor_name(&d.edid),
                d.edid_from,
                d.edid.len(),
                d.limit_khz,
                if list.is_empty() { "nothing else".into() } else { list.join(" ") }
            );
            if d.edid.len() > crate::display::edid().len() {
                crate::display::set_edid(d.edid.clone());
            }
            *DISPLAY_STATE.lock() = Some(d);
        }
        Err(e) => note(format!("{e}; the firmware mode stays")),
    }
}

/// Once a second: whether the display FIFO ran empty (on screen that is a jerk or a stripe).
/// The flag is cleared and counted; logged at most once every 10 seconds.
pub fn watch() {
    use core::sync::atomic::Ordering;
    let Some((mmio, pipe)) = DISPLAY_STATE.lock().as_ref().map(|d| (d.mmio, d.pipe)) else { return };
    let stat = PIPE[pipe] + PIPE_STAT;
    if rd(mmio, stat) & FIFO_UNDERRUN == 0 {
        return;
    }
    wr(mmio, stat, rd(mmio, stat) | FIFO_UNDERRUN);
    let count = UNDERRUNS.fetch_add(1, Ordering::Relaxed) + 1;
    let now = crate::interrupts::ticks();
    let told = UNDERRUN_TOLD.load(Ordering::Relaxed);
    if told == 0 || now >= told + UNDERRUN_LOG_EVERY {
        UNDERRUN_TOLD.store(now, Ordering::Relaxed);
        crate::kprintln!("baytrail: the display ran out of data, {count} times since the start");
    }
}

/// How many times since boot the display ran out of data.
pub fn underruns() -> u64 {
    UNDERRUNS.load(core::sync::atomic::Ordering::Relaxed)
}

/// Debugging over the network: a display register (offset in BAR 0, up to 4 MiB) or DPIO. Only reads
/// and writes at an administrator's request; checks nothing — that is what it is for.
pub fn peek(offset: usize) -> Option<u32> {
    let mmio = DISPLAY_STATE.lock().as_ref().map(|d| d.mmio)?;
    (offset < MMIO_SIZE as usize && offset % 4 == 0).then(|| rd(mmio, offset))
}

pub fn poke(offset: usize, value: u32) -> bool {
    let Some(mmio) = DISPLAY_STATE.lock().as_ref().map(|d| d.mmio) else { return false };
    if offset >= MMIO_SIZE as usize || offset % 4 != 0 {
        return false;
    }
    wr(mmio, offset, value);
    true
}

pub fn dpio_peek(reg: u32) -> Option<u32> {
    let mmio = DISPLAY_STATE.lock().as_ref().map(|d| d.mmio)?;
    Some(dpio(mmio, reg))
}

pub fn dpio_poke(reg: u32, value: u32) -> bool {
    let Some(mmio) = DISPLAY_STATE.lock().as_ref().map(|d| d.mmio) else { return false };
    set_dpio(mmio, reg, value);
    true
}

/// The display was turned off by a timer or a button: remove the signal, the monitor goes to sleep.
pub fn sleep() {
    cursor_hide();
    let Some(mut d) = DISPLAY_STATE.lock().clone() else { return };
    if d.asleep {
        return;
    }
    d.sleep();
    *DISPLAY_STATE.lock() = Some(d);
}

/// The display is turned back on: the signal in the same mode.
pub fn wake() {
    let Some(mut d) = DISPLAY_STATE.lock().clone() else { return };
    if !d.asleep {
        return;
    }
    d.wake();
    *DISPLAY_STATE.lock() = Some(d);
}

// ---------------------------------------------------------------- hardware cursor
//
// The display has its own 64×64 ARGB cursor plane over the frame: the cursor moves by writing
// the position register, the frame under it is not redrawn. Registers (CURCNTR, CURBASE, CURPOS:
// 0x70080/0x70084/0x70088, +0x40 for pipe B), the 64×64 ARGB mode 0x27, gamma (bit 26),
// pipe select (bits 29:28) — as in the i915 driver (MIT).
const CURSOR_REGS: usize = DISPLAY + 0x7_0080;
const CUR_CTL: usize = 0;
const CUR_BASE: usize = 4;
const CUR_POS: usize = 8;
const CURSOR_64_ARGB: u32 = 0x27;
const CURSOR_GAMMA: u32 = 1 << 26;
const PLANE_GAMMA: u32 = 1 << 30;
pub const CURSOR_SIZE: usize = 64;
/// The place of the cursor image in the GTT: just below the top of the smallest aperture.
const CURSOR_GTT: usize = APERTURE_MIN - (64 << 10);

struct HwCursor {
    /// The image in kernel memory (the display reads it bypassing the cache) and what is drawn in it.
    image: *mut u32,
    drawn: u64,
    on: bool,
}

unsafe impl Send for HwCursor {}

static CURSOR: IrqLock<Option<HwCursor>> = IrqLock::new(None);

/// Places the cursor (a 64×64 ARGB image, `key` says what is on it so an identical one is not redrawn)
/// at screen point (x, y) — the image's top-left corner. false — there is no hardware
/// cursor (our mode is not on or there is no room for it), draw it yourself.
pub fn cursor_show(key: u64, draw: impl FnOnce(&mut [u32]), x: i32, y: i32) -> bool {
    let Some((mmio, pipe, frame_end)) = DISPLAY_STATE.lock().as_ref().filter(|d| d.current.is_some() && !d.asleep).map(|d| (d.mmio, d.pipe, d.frame_gtt + d.memory.map_or(0, |(_, size)| size))) else {
        return false;
    };
    if frame_end > CURSOR_GTT {
        return false;
    }
    let mut cursor = CURSOR.lock();
    if cursor.is_none() {
        let layout = core::alloc::Layout::from_size_align(CURSOR_SIZE * CURSOR_SIZE * 4, PAGE).ok();
        let image = layout.map_or(core::ptr::null_mut(), |l| unsafe { alloc::alloc::alloc_zeroed(l) }) as *mut u32;
        if image.is_null() {
            return false;
        }
        let table = mmio + GTT + CURSOR_GTT / PAGE * 4;
        for page in 0..CURSOR_SIZE * CURSOR_SIZE * 4 / PAGE {
            let addr = image as u64 + (page * PAGE) as u64;
            let pte = addr as u32 & 0xFFFF_F000 | (addr >> 28) as u32 & 0xFF0 | PTE_WRITABLE | PTE_VALID;
            unsafe { (table as *mut u32).add(page).write_volatile(pte) };
        }
        wr(mmio, GTT_FLUSH, 1);
        let _ = rd(mmio, GTT_FLUSH);
        *cursor = Some(HwCursor { image, drawn: !key, on: false });
    }
    let c = cursor.as_mut().unwrap();
    if c.drawn != key {
        let px = unsafe { core::slice::from_raw_parts_mut(c.image, CURSOR_SIZE * CURSOR_SIZE) };
        px.fill(0);
        draw(px);
        // The display reads memory bypassing the CPU cache.
        for line in (0..CURSOR_SIZE * CURSOR_SIZE * 4).step_by(CACHE_LINE) {
            unsafe { core::arch::x86_64::_mm_clflush((c.image as *const u8).add(line)) };
        }
        unsafe { core::arch::x86_64::_mm_mfence() };
        c.drawn = key;
    }
    let regs = CURSOR_REGS + pipe * 0x40;
    let coord = |v: i32| if v < 0 { 0x8000 | (-v as u32 & 0xFFF) } else { v as u32 & 0xFFF };
    if !c.on {
        // Gamma — like the primary plane: its table is not loaded by us (white came out
        // pink through it), and if the plane uses gamma, the cursor must too.
        let plane = PIPE[pipe] + PLANE_CTL;
        let gamma = if rd(mmio, plane) & PLANE_GAMMA != 0 { CURSOR_GAMMA } else { 0 };
        wr(mmio, regs + CUR_CTL, CURSOR_64_ARGB | gamma | (pipe as u32) << 28);
        c.on = true;
    }
    wr(mmio, regs + CUR_POS, coord(y) << 16 | coord(x));
    // Writing the address "arms" the update: the new position and image take effect from the next frame.
    wr(mmio, regs + CUR_BASE, CURSOR_GTT as u32);
    true
}

/// Moves an already shown cursor (the image's top-left corner to a screen point).
pub fn cursor_move(x: i32, y: i32) {
    let Some((mmio, pipe)) = DISPLAY_STATE.lock().as_ref().filter(|d| d.current.is_some() && !d.asleep).map(|d| (d.mmio, d.pipe)) else { return };
    let cursor = CURSOR.lock();
    if cursor.as_ref().is_some_and(|c| c.on) {
        let regs = CURSOR_REGS + pipe * 0x40;
        let coord = |v: i32| if v < 0 { 0x8000 | (-v as u32 & 0xFFF) } else { v as u32 & 0xFFF };
        wr(mmio, regs + CUR_POS, coord(y) << 16 | coord(x));
        wr(mmio, regs + CUR_BASE, CURSOR_GTT as u32);
    }
}

/// Hides the hardware cursor (screensaver, blanking, mode change).
pub fn cursor_hide() {
    let Some((mmio, pipe)) = DISPLAY_STATE.lock().as_ref().map(|d| (d.mmio, d.pipe)) else { return };
    let mut cursor = CURSOR.lock();
    if let Some(c) = cursor.as_mut().filter(|c| c.on) {
        let regs = CURSOR_REGS + pipe * 0x40;
        wr(mmio, regs + CUR_CTL, 0);
        wr(mmio, regs + CUR_BASE, 0);
        c.on = false;
    }
}

// The power unit (Punit) and the "north" block (NC): the GPU frequency. Ports and read/write opcodes
// on the bus — as in the i915 driver (MIT): IOSF_PORT_PUNIT 0x04, IOSF_PORT_NC 0x11.
const SB_PORT_PUNIT: u32 = 0x04;
const SB_PORT_NC: u32 = 0x11;
const SB_PRIVATE_WRITE: u32 = 0x07;

pub fn punit_read(mmio: usize, reg: u32) -> Option<u32> {
    sideband(mmio, SB_PORT_PUNIT, SB_PRIVATE_READ, reg, 0)
}

pub fn punit_write(mmio: usize, reg: u32, value: u32) -> bool {
    sideband(mmio, SB_PORT_PUNIT, SB_PRIVATE_WRITE, reg, value).is_some()
}

pub fn nc_read(mmio: usize, reg: u32) -> Option<u32> {
    sideband(mmio, SB_PORT_NC, SB_PRIVATE_READ, reg, 0)
}

/// The GPU address for a CPU address in the aperture (the screen frame): the aperture is a window
/// into the GTT, and the offset in it is the GPU address. None — this is not the Bay Trail aperture.
pub fn graphics_address(cpu: usize) -> Option<u32> {
    let aperture = DISPLAY_STATE.lock().as_ref().map(|d| d.aperture)?;
    (cpu >= aperture && cpu < aperture + APERTURE_MIN).then(|| (cpu - aperture) as u32)
}

/// We set the primary screen's mode.
pub fn active() -> bool {
    DISPLAY_STATE.lock().is_some()
}

/// Modes to choose from: the firmware mode first.
pub fn modes() -> Vec<Resolution> {
    let state = DISPLAY_STATE.lock();
    let Some(d) = state.as_ref() else { return Vec::new() };
    core::iter::once(d.firmware_mode).chain(d.modes.iter().copied()).map(|m| m.resolution()).collect()
}

/// The screen mode now.
pub fn current_mode() -> Option<Resolution> {
    DISPLAY_STATE.lock().as_ref().map(|d| d.current.map_or(d.firmware_mode, |(m, _)| m).resolution())
}

/// Our mode; None — the firmware mode is running.
pub fn chosen() -> Option<Resolution> {
    DISPLAY_STATE.lock().as_ref().and_then(|d| d.current.map(|(m, _)| m.resolution()))
}

/// Switches the screen: to a mode from the list or (None) to the firmware mode. Returns the frame
/// that is now on screen.
pub fn switch(want: Option<Resolution>) -> Option<Framebuffer> {
    let mut d = DISPLAY_STATE.lock().clone()?;
    let same = want.is_some_and(|w| d.current.is_some_and(|(m, _)| m.resolution() == w));
    if !same {
        // A new mode: the cursor is enabled again when it is shown.
        cursor_hide();
    }
    let target = want.and_then(|w| d.modes.iter().copied().find(|m| m.resolution() == w));
    if target.map(|m| m.resolution()) == d.current.map(|(m, _)| m.resolution()) {
        return Some(d.screen());
    }
    let screen = match target {
        Some(mode) => d.enter(mode).unwrap_or_else(|e| {
            note(format!("{}: {e}", name(&mode)));
            let r = mode.resolution();
            crate::wm::notify("Screen resolution", &format!("{}\u{00D7}{} {} Hz did not start: {e}. The firmware mode is back.", r.w, r.h, r.hz));
            d.firmware_frame
        }),
        None => d.leave(),
    };
    // A mode change enables the output again.
    d.asleep = false;
    *DISPLAY_STATE.lock() = Some(d);
    Some(screen)
}

/// Display registers for `hwprobe`: read only.
pub fn report(gpu: &pci::Device) -> String {
    let mmio = gpu.mmio(0, MMIO_SIZE);
    let mut out = String::from("== Bay Trail display\n");
    let reg = |out: &mut String, name: &str, at: usize| {
        let _ = writeln!(out, "{name} {at:06x} = {:08x}", rd(mmio, at));
    };
    for (pipe, label) in [(0, 'A'), (1, 'B')] {
        let (p, t) = (PIPE[pipe], TRANSCODER[pipe]);
        let regs = [
            ("HTOTAL", t),
            ("HBLANK", t + 0x04),
            ("HSYNC", t + 0x08),
            ("VTOTAL", t + 0x0C),
            ("VBLANK", t + 0x10),
            ("VSYNC", t + 0x14),
            ("PIPESRC", t + PIPE_SOURCE),
            ("PIPECONF", p + PIPE_CONF),
            ("PIPESTAT", p + PIPE_STAT),
            ("FRMCOUNT", p + FRAME_COUNT),
            ("DSPCNTR", p + PLANE_CTL),
            ("DSPLINOFF", p + PLANE_LINEAR),
            ("DSPSTRIDE", p + PLANE_STRIDE),
            ("DSPSURF", p + PLANE_SURF),
            ("DSPTILEOFF", p + PLANE_TILE),
            ("DSPSURFLIVE", p + PLANE_LIVE),
            ("DPLL", DPLL[pipe]),
            ("DPLL_MD", DPLL_MD[pipe]),
            ("DDL", DRAIN_LATENCY[pipe]),
            ("VIDEO_DIP", VIDEO_DIP[pipe]),
        ];
        for (name, at) in regs {
            reg(&mut out, &format!("{name}_{label}"), at);
        }
    }
    let regs = [
        ("ADPA", VGA),
        ("HDMIB", HDMI[0]),
        ("HDMIC", HDMI[1]),
        ("DP_B", DP[0]),
        ("DP_C", DP[1]),
        ("PFIT_CONTROL", PANEL_FITTER),
        ("PFIT_RATIOS", PANEL_FITTER + 4),
        ("HOTPLUG_EN", HOTPLUG_ENABLE),
        ("HOTPLUG_STAT", HOTPLUG_STATUS),
        ("DPIO_CTL", DPIO_CONTROL),
        ("DPIO_PHY_STATUS", DPIO_PHY_STATUS),
        ("DSPARB", ARBITER),
        ("DSPARB2", ARBITER2),
        ("DSPFW1", WATERMARKS),
        ("DSPFW2", WATERMARKS + 4),
        ("DSPFW3", WATERMARKS + 8),
        ("DSPHOWM", WATERMARKS_HIGH),
        ("DSPHOWM1", WATERMARKS_HIGH + 4),
        ("FW_BLC_SELF", SELF_REFRESH),
    ];
    for (name, at) in regs {
        reg(&mut out, name, at);
    }
    for ch in 0..2u32 {
        let (pll, lane) = (ch * PLL_STEP, ch * LANE_STEP);
        let regs = [
            ("PLL_DW3", PLL_DW3 + pll),
            ("PLL_DW5", PLL_DW5 + pll),
            ("PLL_DW7", PLL_DW7 + pll),
            ("PLL_DW8", PLL_DW8 + pll),
            ("PLL_DW9", PLL_DW9 + pll),
            ("PLL_DW10", PLL_DW10 + pll),
            ("PLL_DW11", PLL_DW11 + pll),
            ("PCS_DW0", PCS_DW0 + lane),
            ("PCS_DW1", PCS_DW1 + lane),
            ("PCS_DW8", PCS_DW8 + lane),
            ("PCS_DW9", PCS_DW9 + lane),
            ("PCS_DW11", PCS_DW11 + lane),
            ("PCS_DW12", PCS_DW12 + lane),
            ("PCS_DW14", PCS_DW14 + lane),
            ("PCS_DW23", PCS_DW23 + lane),
            ("TX_DW2", TX_DW2 + lane),
            ("TX_DW3", TX_DW3 + lane),
            ("TX_DW4", TX_DW4 + lane),
            ("TX_DW5", TX_DW5 + lane),
            ("TX_DW11", TX_DW11 + lane),
            ("TX_DW14", TX_DW14 + lane),
            ("TX3_DW4", TX3_DW4[ch as usize]),
        ];
        for (name, at) in regs {
            let _ = writeln!(out, "dpio {name} ch{ch} {at:04x} = {:08x}", dpio(mmio, at));
        }
    }
    for (name, at) in [("CMN_DW0", COMMON_DW0), ("REF_DW13", REF_DW13)] {
        let _ = writeln!(out, "dpio {name} {at:04x} = {:08x}", dpio(mmio, at));
    }
    let _ = writeln!(out, "cdclk: {}", cdclk_khz(mmio).map_or("not readable".into(), |k| format!("{k} kHz")));
    let pte = |index: usize| unsafe { ((mmio + GTT + index * 4) as *const u32).read_volatile() };
    let surface = rd(mmio, PIPE[0] + PLANE_SURF) as usize;
    let _ = writeln!(out, "gtt: pte 0 = {:08x}, pte at plane A surface = {:08x}, at 16 MiB = {:08x}", pte(0), pte(surface / PAGE), pte(FRAME_MIN / PAGE));
    let live = rd(mmio, HOTPLUG_STATUS);
    for (name, pin, bit) in [("HDMI B", DDC_PIN[0], Some(LIVE[0])), ("HDMI C", DDC_PIN[1], Some(LIVE[1])), ("VGA", DDC_PIN_VGA, None)] {
        let plugged = bit.map_or(String::new(), |b| format!(", live {}", live & b != 0));
        match crate::hwprobe::ddc_read_on(mmio, GMBUS, pin, EDID_ADDR, 0, EDID_BLOCK) {
            Ok(edid) if edid[..8] == EDID_HEADER => {
                let _ = writeln!(out, "ddc {name} (pin {pin}{plugged}): {}", crate::hwprobe::monitor_name(&edid));
            }
            Ok(_) => {
                let _ = writeln!(out, "ddc {name} (pin {pin}{plugged}): not an EDID");
            }
            Err(e) => {
                let _ = writeln!(out, "ddc {name} (pin {pin}{plugged}): {e}");
            }
        }
    }
    out.push_str(&state());
    out
}

/// What the driver does: for the report.
pub fn state() -> String {
    let mut out = String::new();
    match DISPLAY_STATE.lock().as_ref() {
        Some(d) => {
            let list: Vec<String> = d.modes.iter().map(name).collect();
            let _ = writeln!(out, "driver: {}", d.describe());
            let _ = writeln!(out, "driver: EDID from {}, up to {} kHz, modes {}", d.edid_from, d.limit_khz, list.join(" "));
            let _ = writeln!(out, "driver: now {}", d.current.map_or_else(|| format!("the firmware mode {}", name(&d.firmware_mode)), |(m, _)| name(&m)));
        }
        None => out.push_str("driver: not in use\n"),
    }
    let last = LAST.lock().clone();
    if !last.is_empty() {
        let _ = writeln!(out, "driver: last {last}");
    }
    let _ = writeln!(out, "driver: the display ran out of data {} times since the start", underruns());
    out
}

