//! A monitor on the laptop's HDMI port (Comet Lake-U GPU).
//!
//! On the Latitude 7410 board the HDMI connector is wired to port DDI B through a
//! DisplayPort-to-HDMI converter, so the port works as plain DisplayPort: EDID and configuration
//! go over AUX, and the picture over four lanes. The converter outputs HDMI itself.
//!
//! The built-in screen was set up by the firmware; its pipe A is left alone. The monitor gets
//! pipe B and its own frame in memory: the frame is mapped into the GGTT, and we write it through
//! the aperture (BAR 2). Writes there reach memory immediately, so the display will not miss them.
//!
//! The monitor runs in the chosen mode, the native one by default.
//!
//! If the monitor is disconnected, the port keeps running: when the cable comes back, the monitor
//! does not have to lock onto the signal again after link training.

use crate::gfx::Framebuffer;
use crate::hwprobe::{aux, aux_i2c, deadline, expired, rd, wr};
use crate::pci;
use crate::sync::IrqLock;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const GPU: (u16, u16) = (0x8086, 0x9B41);
const GPU_MMIO: u64 = 16 << 20;

/// TSC cycles per microsecond: with a margin, the laptop's frequency is lower.
const CYCLES_PER_US: u64 = 3000;

const POWER_DRIVER: usize = 0x45404;
const DDI_B_POWER_REQUEST: u32 = 1 << 5;
const DDI_B_POWER_STATE: u32 = 1 << 4;

const DPLL_CTRL1: usize = 0x6C058;
const DPLL_CTRL2: usize = 0x6C05C;
const DPLL_STATUS: usize = 0x6C060;
const DPLL1_CTL: usize = 0x46014;
const PLL_ENABLE: u32 = 1 << 31;
const DPLL1_LOCK: u32 = 1 << 8;
/// DPLL 1: its own configuration and link rate, the other bits without spread spectrum and HDMI.
const DPLL1_MASK: u32 = 0x3F << 6;

/// DisplayPort link rate.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Link {
    /// Symbols per millisecond per lane: the same number of bytes.
    khz: u64,
    dpcd: u8,
    dpll: u32,
    /// Which patterns the second training phase uses: in the port and at the sink.
    pattern: (u32, u8),
}

const HBR: Link = Link { khz: 270_000, dpcd: 0x0A, dpll: 1 << 6 | 1 << 7, pattern: (TP_PATTERN_2, PATTERN_2) };
const HBR2: Link = Link { khz: 540_000, dpcd: 0x14, dpll: 1 << 6, pattern: (TP_PATTERN_3, PATTERN_3) };
/// Port B: clock from DPLL 1 instead of the reset values.
const DDI_B_CLOCK_MASK: u32 = 1 << 16 | 3 << 4;
const DDI_B_CLOCK_DPLL1: u32 = 1 << 4 | 1 << 3;
const DDI_B_CLOCK_OFF: u32 = 1 << 16;

const TRANS_CLK_SEL_B: usize = 0x46144;
const TRANS_CLK_PORT_B: u32 = 2 << 29;

const BMU_CR0: usize = 0x6C00C;
const BMU_B_MASK: u32 = 7 << 11 | 1 << 24;
const BMU_B_DISABLE: u32 = 1 << 24;
const BMU_B_SHIFT: u32 = 11;

const DDI_BUF_CTL_B: usize = 0x64100;
const DDI_ENABLE: u32 = 1 << 31;
const DDI_IDLE: u32 = 1 << 7;
const DDI_TRANS_SHIFT: u32 = 24;
const LANES: u32 = 4;
const DDI_WIDTH: u32 = (LANES - 1) << 1;

const DDI_TRANS_B: usize = 0x64E60;
/// Transmitter settings for signal levels: low word, high word, gain.
/// Values for the U series (Kaby Lake and newer) — from the open tables of the i915 driver.
const LEVELS: [(u32, u32, u32); 9] = [
    (0x0000201B, 0x000000A1, 0),
    (0x00005012, 0x00000088, 0),
    (0x80007011, 0x000000CD, 3),
    (0x80009010, 0x000000C0, 3),
    (0x0000201B, 0x0000009D, 0),
    (0x80005012, 0x000000C0, 3),
    (0x80007011, 0x000000C0, 3),
    (0x00002016, 0x0000004F, 0),
    (0x80005012, 0x000000C0, 3),
];

const DP_TP_CTL_B: usize = 0x64140;
const DP_TP_STATUS_B: usize = 0x64144;
const TP_ENABLE: u32 = 1 << 31;
const TP_ENHANCED: u32 = 1 << 18;
const TP_PATTERN_1: u32 = 0 << 8;
const TP_PATTERN_2: u32 = 1 << 8;
const TP_PATTERN_3: u32 = 4 << 8;
const TP_IDLE: u32 = 2 << 8;
const TP_NORMAL: u32 = 3 << 8;
const TP_IDLE_DONE: u32 = 1 << 25;

const AUX_B: usize = 0x64110;
const AUX_NATIVE_WRITE: u8 = 0x80;
const AUX_NATIVE_READ: u8 = 0x90;
const AUX_REPLY_MASK: u8 = 0xF0;

// Sink registers (DPCD).
const DPCD_MAX_RATE: u16 = 0x01;
const DPCD_DOWNSTREAM: u16 = 0x05;
const DPCD_TRAINING_INTERVAL: u16 = 0x0E;
const DPCD_PORT_CAPS: u16 = 0x80;
const DPCD_LINK_RATE: u16 = 0x100;
const DPCD_TRAINING_PATTERN: u16 = 0x102;
const DPCD_LANE_SET: u16 = 0x103;
const DPCD_DOWNSPREAD: u16 = 0x107;
const DPCD_STATUS: u16 = 0x202;
const DPCD_POWER: u16 = 0x600;
const ENHANCED_FRAMING: u8 = 1 << 7;
const CODING_8B10B: u8 = 1;
const SCRAMBLING_OFF: u8 = 1 << 5;
const PATTERN_1: u8 = 1;
const PATTERN_2: u8 = 2;
const PATTERN_3: u8 = 3;
const MAX_SWING_REACHED: u8 = 1 << 2;
const MAX_PRE_REACHED: u8 = 1 << 5;
const LANE_CR_DONE: u8 = 1;
const LANE_EQ_DONE: u8 = 7;
const INTERLANE_ALIGNED: u8 = 1;
const STEP_US: u64 = 500;
const TRAIN_STEPS: usize = 20;

const EDID_SIZE: usize = 128;
const EDID_ADDR: u8 = 0x50;
const EDID_FIRST_TIMING: usize = 54;
const EDID_TIMING_SIZE: usize = 18;
const EDID_TIMINGS: usize = 4;
const EDID_STANDARD: core::ops::Range<usize> = 38..54;
const EDID_EXTENSIONS: usize = 126;
const CTA_TAG: u8 = 2;
const CTA_VIDEO_BLOCK: u8 = 2;
/// Beyond this the monitor mostly lists low modes with a high refresh rate: not needed.
const STANDARD_MAX_HZ: i32 = 75;
/// The GPU cannot go higher at a 675 MHz clock.
const PIXEL_MAX_KHZ: u64 = 600_000;
const TMDS_STEP_KHZ: u64 = 2500;
const PORT_DETAILED: u8 = 1 << 4;
const PORT_TYPE_HDMI: u8 = 3;
const BITS_PER_PIXEL: u64 = 24;
const BYTES_PER_PIXEL: u32 = 4;
const M_N_DIVIDER: u64 = 0x80_0000;
const TU_SIZE_64: u32 = 63 << 25;

const TRANS_B: usize = 0x61000;
const TIMING_HTOTAL: usize = TRANS_B;
const TIMING_HBLANK: usize = TRANS_B + 0x04;
const TIMING_HSYNC: usize = TRANS_B + 0x08;
const TIMING_VTOTAL: usize = TRANS_B + 0x0C;
const TIMING_VBLANK: usize = TRANS_B + 0x10;
const TIMING_VSYNC: usize = TRANS_B + 0x14;
const PIPE_SOURCE: usize = TRANS_B + 0x1C;
const DATA_M: usize = TRANS_B + 0x30;
const DATA_N: usize = TRANS_B + 0x34;
const LINK_M: usize = TRANS_B + 0x40;
const LINK_N: usize = TRANS_B + 0x44;
const TRANS_DDI_FUNC_B: usize = 0x61400;
const FUNC_ENABLE: u32 = 1 << 31;
const FUNC_PORT_B: u32 = 1 << 28;
const FUNC_DP_SST: u32 = 2 << 24;
const FUNC_POSITIVE_V: u32 = 1 << 17;
const FUNC_POSITIVE_H: u32 = 1 << 16;
const MSA_MISC_B: usize = 0x61410;
const MSA_SYNC_8BPC: u32 = 1 | 1 << 5;
const PIPE_CONF_B: usize = 0x71008;
const PIPE_MISC_B: usize = 0x71030;
const PIPE_ENABLE: u32 = 1 << 31;
const PIPE_ACTIVE: u32 = 1 << 30;
const LINETIME_B: usize = 0x45274;
const FRAME_COUNT_B: usize = 0x71040;
const PIPE_ISR_B: usize = 0x44410;
const PIPE_IIR_B: usize = 0x44418;
const SDEISR: usize = 0xC4000;
const PORT_B_LIVE: u32 = 1 << 21;

const PLANE_A: usize = 0x70000;
const PLANE_B: usize = 0x71000;
const PLANE_CTL: usize = 0x180;
const PLANE_STRIDE: usize = 0x188;
const PLANE_POS: usize = 0x18C;
const PLANE_SIZE: usize = 0x190;
const PLANE_SURF: usize = 0x19C;
const PLANE_OFFSET: usize = 0x1A4;
const PLANE_WM: usize = 0x240;
const PLANE_WM_TRANS: usize = 0x268;
const PLANE_BUF_CFG: usize = 0x27C;
/// Watermark levels: level zero is mandatory, higher ones let memory sleep for longer and longer.
const WM_LEVELS: usize = 8;
const WM_ENABLE: u32 = 1 << 31;
const WM_BLOCKS_MASK: u32 = 0x7FF;
const WM_LINES_SHIFT: u32 = 14;
/// Pipe B buffer watermark: three lines of 20 blocks (2560 pixels) plus one block.
const WM_B: u32 = WM_ENABLE | 3 << WM_LINES_SHIFT | 61;
/// The display buffer: 896 blocks in total, 4 of them taken by the service path.
const DDB_LAST: u32 = 891;
const CURSOR_BUF_CFG_A: usize = 0x7017C;
const SETTLE_US: u64 = 40_000;
const FAREWELL_US: u64 = 600_000;

/// GGTT page table: starts 8 MiB past the registers.
const GGTT: usize = 8 << 20;
const GGTT_FLUSH: usize = 0x101008;
const PAGE: usize = 4096;
const PTE_PRESENT: u64 = 1;
const CACHE_LINE: usize = 64;
/// Address of the monitor frame in the GGTT: the firmware frame is at the very start.
const FRAME_GGTT: usize = 128 << 20;
const FRAME_ALIGN: usize = 256 << 10;
const STRIDE_UNIT: u32 = 64;
const PLANE_CTL_XRGB: u32 = 1 << 31 | 4 << 24;
const APERTURE_BAR: u8 = 0x18;
const SIZE_MASK: u32 = !0xF;

fn pause_us(us: u64) {
    let end = deadline(us * CYCLES_PER_US);
    while !expired(end) {
        core::hint::spin_loop();
    }
}

fn wait(mmio: usize, reg: usize, mask: u32, value: u32) -> bool {
    let end = deadline(100_000 * CYCLES_PER_US);
    while !expired(end) {
        if rd(mmio, reg) & mask == value {
            return true;
        }
    }
    false
}

fn dpcd_write(mmio: usize, addr: u16, data: &[u8]) -> Result<(), String> {
    let mut send = alloc::vec![AUX_NATIVE_WRITE, (addr >> 8) as u8, addr as u8, data.len() as u8 - 1];
    send.extend_from_slice(data);
    match aux(mmio, AUX_B, &send) {
        Ok(reply) if reply.first().is_some_and(|b| b & AUX_REPLY_MASK == 0) => Ok(()),
        Ok(reply) => Err(format!("DPCD {addr:#x} refused: {reply:02x?}")),
        Err(e) => Err(format!("DPCD {addr:#x}: {e}")),
    }
}

fn dpcd_read(mmio: usize, addr: u16, len: usize) -> Result<Vec<u8>, String> {
    let send = [AUX_NATIVE_READ, (addr >> 8) as u8, addr as u8, len as u8 - 1];
    match aux(mmio, AUX_B, &send) {
        Ok(reply) if reply.len() > len && reply[0] & AUX_REPLY_MASK == 0 => Ok(reply[1..].into()),
        Ok(reply) => Err(format!("DPCD {addr:#x} refused: {reply:02x?}")),
        Err(e) => Err(format!("DPCD {addr:#x}: {e}")),
    }
}

/// Row number of the `LEVELS` table for signal swing and pre-emphasis.
fn level_index(swing: u8, pre: u8) -> usize {
    const FIRST: [usize; 3] = [0, 4, 7];
    FIRST[swing as usize] + pre as usize
}

fn max_pre(swing: u8) -> u8 {
    3 - swing
}

/// Sets the signal level in the transmitter and reports it to the sink.
fn set_levels(mmio: usize, swing: u8, pre: u8) -> Result<(), String> {
    const MAX_SWING: u8 = 2;
    let level = level_index(swing, pre);
    let boost = LEVELS[level].2;
    let bmu = rd(mmio, BMU_CR0) & !BMU_B_MASK;
    wr(mmio, BMU_CR0, bmu | if boost == 0 { BMU_B_DISABLE } else { boost << BMU_B_SHIFT });
    wr(mmio, DDI_BUF_CTL_B, DDI_ENABLE | DDI_WIDTH | (level as u32) << DDI_TRANS_SHIFT);
    let mut lane = swing | pre << 3;
    if swing == MAX_SWING {
        lane |= MAX_SWING_REACHED;
    }
    if pre == max_pre(swing) {
        lane |= MAX_PRE_REACHED;
    }
    dpcd_write(mmio, DPCD_LANE_SET, &[lane; LANES as usize])
}

/// What the sink asks to change: the largest values over all lanes.
fn requested(status: &[u8]) -> (u8, u8) {
    let adjust = u32::from(status[4]) | u32::from(status[5]) << 8;
    let (mut swing, mut pre) = (0, 0);
    for lane in 0..LANES {
        let value = (adjust >> (lane * 4)) as u8;
        swing = swing.max(value & 3);
        pre = pre.max(value >> 2 & 3);
    }
    let swing = swing.min(2);
    (swing, pre.min(max_pre(swing)))
}

fn lanes_ready(status: &[u8], mask: u8) -> bool {
    let word = u32::from(status[0]) | u32::from(status[1]) << 8;
    (0..LANES).all(|lane| (word >> (lane * 4)) as u8 & mask == mask)
}

/// How long to wait between equalization checks: the sink says how much it needs.
fn training_wait(mmio: usize) -> u64 {
    const UNIT_US: u64 = 4000;
    const VALUE: u8 = 0x7F;
    let interval = dpcd_read(mmio, DPCD_TRAINING_INTERVAL, 1).map_or(0, |v| v[0] & VALUE) as u64;
    if interval == 0 { STEP_US } else { interval * UNIT_US }
}

fn train(mmio: usize, link: Link) -> Result<(), String> {
    // Phase 1: the sink adjusts its clock recovery.
    dpcd_write(mmio, DPCD_TRAINING_PATTERN, &[PATTERN_1 | SCRAMBLING_OFF])?;
    let (mut swing, mut pre) = (0, 0);
    let mut ready = false;
    for _ in 0..TRAIN_STEPS {
        set_levels(mmio, swing, pre)?;
        pause_us(STEP_US);
        let status = dpcd_read(mmio, DPCD_STATUS, 6)?;
        if lanes_ready(&status, LANE_CR_DONE) {
            ready = true;
            break;
        }
        (swing, pre) = requested(&status);
    }
    if !ready {
        return Err("clock recovery failed".into());
    }
    crate::kprintln!("hdmi: clock recovered, swing {swing}, pre {pre}");

    // Phase 2: lane equalization.
    let wait_us = training_wait(mmio);
    wr(mmio, DP_TP_CTL_B, TP_ENABLE | TP_ENHANCED | link.pattern.0);
    dpcd_write(mmio, DPCD_TRAINING_PATTERN, &[link.pattern.1 | SCRAMBLING_OFF])?;
    for _ in 0..TRAIN_STEPS {
        set_levels(mmio, swing, pre)?;
        pause_us(wait_us);
        let status = dpcd_read(mmio, DPCD_STATUS, 6)?;
        if lanes_ready(&status, LANE_EQ_DONE) && status[2] & INTERLANE_ALIGNED != 0 {
            crate::kprintln!("hdmi: link trained, swing {swing}, pre {pre}");
            return dpcd_write(mmio, DPCD_TRAINING_PATTERN, &[0]);
        }
        (swing, pre) = requested(&status);
    }
    Err("channel equalization failed".into())
}

/// Power, clock and signal levels of port B.
fn start_port(mmio: usize, link: Link) -> Result<(), String> {
    let power = rd(mmio, POWER_DRIVER);
    wr(mmio, POWER_DRIVER, power | DDI_B_POWER_REQUEST);
    if !wait(mmio, POWER_DRIVER, DDI_B_POWER_STATE, DDI_B_POWER_STATE) {
        return Err("port B did not power up".into());
    }
    wr(mmio, DPLL_CTRL1, rd(mmio, DPLL_CTRL1) & !DPLL1_MASK | link.dpll);
    wr(mmio, DPLL1_CTL, rd(mmio, DPLL1_CTL) | PLL_ENABLE);
    if !wait(mmio, DPLL_STATUS, DPLL1_LOCK, DPLL1_LOCK) {
        return Err("DPLL 1 did not lock".into());
    }
    wr(mmio, DPLL_CTRL2, rd(mmio, DPLL_CTRL2) & !DDI_B_CLOCK_MASK | DDI_B_CLOCK_DPLL1);
    wr(mmio, TRANS_CLK_SEL_B, TRANS_CLK_PORT_B);
    for (i, &(lo, hi, _)) in LEVELS.iter().enumerate() {
        wr(mmio, DDI_TRANS_B + i * 8, lo);
        wr(mmio, DDI_TRANS_B + i * 8 + 4, hi);
    }
    crate::kprintln!("hdmi: port powered, DPLL 1 locked");

    dpcd_write(mmio, DPCD_POWER, &[1])?;
    pause_us(1000);
    dpcd_write(mmio, DPCD_LINK_RATE, &[link.dpcd, LANES as u8 | ENHANCED_FRAMING])?;
    dpcd_write(mmio, DPCD_DOWNSPREAD, &[0, CODING_8B10B])?;

    wr(mmio, DP_TP_CTL_B, TP_ENABLE | TP_ENHANCED | TP_PATTERN_1);
    wr(mmio, DDI_BUF_CTL_B, DDI_ENABLE | DDI_WIDTH);
    if !wait(mmio, DDI_BUF_CTL_B, DDI_IDLE, 0) {
        return Err("port B stayed idle".into());
    }
    Ok(())
}

/// The monitor mode as the user sees it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub w: i32,
    pub h: i32,
    pub hz: i32,
}

/// A video mode from an EDID detailed timing block. The Bay Trail display driver uses it too.
#[derive(Clone, Copy)]
pub(crate) struct Mode {
    pub(crate) pixel_khz: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) htotal: u32,
    pub(crate) hsync_start: u32,
    pub(crate) hsync_end: u32,
    pub(crate) vtotal: u32,
    pub(crate) vsync_start: u32,
    pub(crate) vsync_end: u32,
    pub(crate) positive_h: bool,
    pub(crate) positive_v: bool,
}

impl Mode {
    /// A mode with standard margins: sync right after the front porch.
    const fn timing(pixel_khz: u64, width: u32, height: u32, h: (u32, u32, u32), v: (u32, u32, u32), positive: (bool, bool)) -> Self {
        Self {
            pixel_khz,
            width,
            height,
            htotal: h.0,
            hsync_start: width + h.1,
            hsync_end: width + h.1 + h.2,
            vtotal: v.0,
            vsync_start: height + v.1,
            vsync_end: height + v.1 + v.2,
            positive_h: positive.0,
            positive_v: positive.1,
        }
    }

    pub(crate) fn resolution(&self) -> Resolution {
        let line_points = self.htotal as u64 * self.vtotal as u64;
        let refresh = (self.pixel_khz * 1000 + line_points / 2) / line_points;
        Resolution { w: self.width as i32, h: self.height as i32, hz: refresh as i32 }
    }

    fn bytes_per_ms(&self) -> u64 {
        self.pixel_khz * BITS_PER_PIXEL / 8
    }
}

/// Modes from the standards: EDID refers to them by number or by size.
/// The first number is the CTA-861 mode number, zero — not listed there.
const KNOWN: [(u8, Mode); 11] = [
    (1, Mode::timing(25_175, 640, 480, (800, 16, 96), (525, 10, 2), (false, false))),
    (0, Mode::timing(40_000, 800, 600, (1056, 40, 128), (628, 1, 4), (true, true))),
    (0, Mode::timing(65_000, 1024, 768, (1344, 24, 136), (806, 3, 6), (false, false))),
    (4, Mode::timing(74_250, 1280, 720, (1650, 110, 40), (750, 5, 5), (true, true))),
    (19, Mode::timing(74_250, 1280, 720, (1980, 440, 40), (750, 5, 5), (true, true))),
    (0, Mode::timing(108_000, 1280, 1024, (1688, 48, 112), (1066, 1, 3), (true, true))),
    (16, Mode::timing(148_500, 1920, 1080, (2200, 88, 44), (1125, 4, 5), (true, true))),
    (31, Mode::timing(148_500, 1920, 1080, (2640, 528, 44), (1125, 4, 5), (true, true))),
    (63, Mode::timing(297_000, 1920, 1080, (2200, 88, 44), (1125, 4, 5), (true, true))),
    (64, Mode::timing(297_000, 1920, 1080, (2640, 528, 44), (1125, 4, 5), (true, true))),
    (95, Mode::timing(297_000, 3840, 2160, (4400, 176, 88), (2250, 8, 10), (true, true))),
];

/// A mode with reduced blanking (CVT, the RB variant) for sizes not in the table.
pub(crate) fn reduced_blanking(w: u32, h: u32, hz: u32) -> Mode {
    const H_BLANK: u32 = 160;
    const H_FRONT: u32 = 48;
    const H_SYNC: u32 = 32;
    const V_FRONT: u32 = 3;
    const V_BACK_MIN: u32 = 6;
    const V_BLANK_NS: u64 = 460_000;
    const STEP_KHZ: u64 = 250;
    let v_sync = match (w * 100 / h.max(1), w) {
        (133, _) => 4,
        (177, _) => 5,
        (160, _) => 6,
        (125, _) => 7,
        _ => 10,
    };
    let line_ns = (1_000_000_000 / hz as u64 - V_BLANK_NS) / h as u64;
    let blank = ((V_BLANK_NS / line_ns) as u32 + 1).max(V_FRONT + v_sync + V_BACK_MIN);
    let (htotal, vtotal) = (w + H_BLANK, h + blank);
    let pixel_khz = (hz as u64 * htotal as u64 * vtotal as u64 / 1000) / STEP_KHZ * STEP_KHZ;
    Mode::timing(pixel_khz, w, h, (htotal, H_FRONT, H_SYNC), (vtotal, V_FRONT, v_sync), (true, false))
}

/// A standard EDID mode: two bytes, size and refresh rate.
fn standard_timing(bytes: &[u8]) -> Option<Mode> {
    const UNUSED: u8 = 1;
    if bytes[0] == UNUSED || bytes[0] == 0 {
        return None;
    }
    let width = (bytes[0] as u32 + 31) * 8;
    let height = match bytes[1] >> 6 {
        0 => width * 10 / 16,
        1 => width * 3 / 4,
        2 => width * 4 / 5,
        _ => width * 9 / 16,
    };
    let hz = (bytes[1] & 0x3F) as u32 + 60;
    if hz as i32 > STANDARD_MAX_HZ {
        return None;
    }
    let same = |m: &&(u8, Mode)| (m.1.width, m.1.height) == (width, height) && m.1.resolution().hz == hz as i32;
    Some(KNOWN.iter().find(same).map_or_else(|| reduced_blanking(width, height, hz), |m| m.1))
}

/// Detailed timing blocks from the CTA extension block, and modes named by number.
fn cta_modes(block: &[u8], out: &mut Vec<Mode>) {
    const TIMINGS_AT: usize = 2;
    const VIC_MASK: u8 = 0x7F;
    if block.first() != Some(&CTA_TAG) || block.len() < EDID_SIZE {
        return;
    }
    let first_timing = block[TIMINGS_AT] as usize;
    let mut at = 4;
    while at < first_timing.min(EDID_SIZE) {
        let (kind, len) = (block[at] >> 5, (block[at] & 0x1F) as usize);
        if kind == CTA_VIDEO_BLOCK {
            for &vic in block.get(at + 1..at + 1 + len).unwrap_or(&[]) {
                out.extend(KNOWN.iter().find(|m| m.0 == vic & VIC_MASK).map(|m| m.1));
            }
        }
        at += 1 + len;
    }
    let mut timing = first_timing.max(4);
    while timing + EDID_TIMING_SIZE <= EDID_SIZE - 1 {
        out.extend(parse_timing(&block[timing..timing + EDID_TIMING_SIZE]));
        timing += EDID_TIMING_SIZE;
    }
}

/// Everything the monitor offers that we can output: the native mode first.
pub(crate) fn modes_from(edid: &[u8], limit_khz: u64) -> Vec<Mode> {
    let mut found = Vec::new();
    for i in 0..EDID_TIMINGS {
        let at = EDID_FIRST_TIMING + i * EDID_TIMING_SIZE;
        found.extend(edid.get(at..at + EDID_TIMING_SIZE).and_then(parse_timing));
    }
    if let Some(block) = edid.get(EDID_SIZE..) {
        cta_modes(block, &mut found);
    }
    found.extend(edid.get(EDID_STANDARD).unwrap_or(&[]).chunks_exact(2).filter_map(standard_timing));
    found.retain(|m| m.pixel_khz <= limit_khz);
    let mut unique: Vec<Mode> = Vec::new();
    for mode in found {
        if !unique.iter().any(|m| m.resolution() == mode.resolution()) {
            unique.push(mode);
        }
    }
    let native = unique.first().copied();
    unique.sort_by_key(|m| {
        let r = m.resolution();
        core::cmp::Reverse((r.w * r.h, r.hz))
    });
    if let Some(i) = native.and_then(|n| unique.iter().position(|m| m.resolution() == n.resolution())) {
        let mode = unique.remove(i);
        unique.insert(0, mode);
    }
    unique
}

fn parse_timing(t: &[u8]) -> Option<Mode> {
    const CLOCK_UNIT_KHZ: u64 = 10;
    let at = |i: usize| t[i] as u32;
    let clock = u16::from_le_bytes([t[0], t[1]]) as u64 * CLOCK_UNIT_KHZ;
    if clock == 0 {
        return None;
    }
    let width = at(2) | (at(4) & 0xF0) << 4;
    let hblank = at(3) | (at(4) & 0x0F) << 8;
    let height = at(5) | (at(7) & 0xF0) << 4;
    let vblank = at(6) | (at(7) & 0x0F) << 8;
    let hfront = at(8) | (at(11) & 0xC0) << 2;
    let hsync = at(9) | (at(11) & 0x30) << 4;
    let vfront = at(10) >> 4 | (at(11) & 0x0C) << 2;
    let vsync = at(10) & 0x0F | (at(11) & 0x03) << 4;
    Some(Mode {
        pixel_khz: clock,
        width,
        height,
        htotal: width + hblank,
        hsync_start: width + hfront,
        hsync_end: width + hfront + hsync,
        vtotal: height + vblank,
        vsync_start: height + vfront,
        vsync_end: height + vfront + vsync,
        positive_h: t[17] & 2 != 0,
        positive_v: t[17] & 4 != 0,
    })
}

fn read_edid(mmio: usize) -> Result<Vec<u8>, String> {
    let mut edid = aux_i2c(mmio, AUX_B, EDID_ADDR, 0, EDID_SIZE)?;
    if edid.get(EDID_EXTENSIONS).is_some_and(|&n| n > 0)
        && let Ok(block) = aux_i2c(mmio, AUX_B, EDID_ADDR, EDID_SIZE as u8, EDID_SIZE)
    {
        edid.extend(block);
    }
    Ok(edid)
}

/// The fastest link the sink can handle.
fn fastest_link(mmio: usize) -> Link {
    let rate = dpcd_read(mmio, DPCD_MAX_RATE, 1).map_or(0, |v| v[0]);
    if rate >= HBR2.dpcd { HBR2 } else { HBR }
}

/// The highest pixel clock: limited by the link, the GPU and the HDMI output.
fn pixel_limit(mmio: usize) -> u64 {
    let link = fastest_link(mmio);
    let mut limit = (link.khz * LANES as u64 * 8 / BITS_PER_PIXEL).min(PIXEL_MAX_KHZ);
    let detailed = dpcd_read(mmio, DPCD_DOWNSTREAM, 1).is_ok_and(|v| v[0] & PORT_DETAILED != 0);
    if detailed
        && let Ok(caps) = dpcd_read(mmio, DPCD_PORT_CAPS, 2)
    {
        crate::kprintln!("hdmi: port caps {caps:02x?}");
        if caps[0] & 7 == PORT_TYPE_HDMI && caps[1] != 0 {
            limit = limit.min(caps[1] as u64 * TMDS_STEP_KHZ);
        }
    }
    limit
}

fn ratio(value: u64, divisor: u64) -> u32 {
    (value * M_N_DIVIDER / divisor) as u32
}

fn start_pipe(mmio: usize, m: &Mode, link: Link) {
    wr(mmio, TIMING_HTOTAL, (m.htotal - 1) << 16 | (m.width - 1));
    wr(mmio, TIMING_HBLANK, (m.htotal - 1) << 16 | (m.width - 1));
    wr(mmio, TIMING_HSYNC, (m.hsync_end - 1) << 16 | (m.hsync_start - 1));
    wr(mmio, TIMING_VTOTAL, (m.vtotal - 1) << 16 | (m.height - 1));
    wr(mmio, TIMING_VBLANK, (m.vtotal - 1) << 16 | (m.height - 1));
    wr(mmio, TIMING_VSYNC, (m.vsync_end - 1) << 16 | (m.vsync_start - 1));
    wr(mmio, PIPE_SOURCE, (m.width - 1) << 16 | (m.height - 1));
    let data = ratio(m.pixel_khz * BITS_PER_PIXEL, link.khz * 8 * LANES as u64);
    wr(mmio, DATA_M, TU_SIZE_64 | data);
    wr(mmio, DATA_N, M_N_DIVIDER as u32);
    wr(mmio, LINK_M, ratio(m.pixel_khz, link.khz));
    wr(mmio, LINK_N, M_N_DIVIDER as u32);
    wr(mmio, MSA_MISC_B, MSA_SYNC_8BPC);
    wr(mmio, PIPE_MISC_B, 0);
    wr(mmio, LINETIME_B, (m.htotal as u64 * 8000).div_ceil(m.pixel_khz) as u32);
    let sync = if m.positive_h { FUNC_POSITIVE_H } else { 0 } | if m.positive_v { FUNC_POSITIVE_V } else { 0 };
    wr(mmio, TRANS_DDI_FUNC_B, FUNC_ENABLE | FUNC_PORT_B | FUNC_DP_SST | sync | DDI_WIDTH);
}

/// Size of the frame access window (BAR 2). Read by writing ones, like any BAR.
fn aperture_size(gpu: &pci::Device) -> usize {
    let bar = gpu.read(APERTURE_BAR);
    gpu.write(APERTURE_BAR, !0);
    let size = (!(gpu.read(APERTURE_BAR) & SIZE_MASK)).wrapping_add(1);
    gpu.write(APERTURE_BAR, bar);
    size as usize
}

/// Frame memory visible to the display, and the address for writing through the aperture.
/// The monitor may be connected again and again: the previous frame fits the same size.
fn map_frame(gpu: &pci::Device, mmio: usize, size: usize) -> Result<*mut u32, String> {
    let aperture = aperture_size(gpu);
    if FRAME_GGTT + size > aperture {
        return Err(format!("aperture of {} MiB is too small", aperture >> 20));
    }
    let cpu = gpu.bar(2) + FRAME_GGTT as u64;
    let layout = core::alloc::Layout::from_size_align(size, PAGE).map_err(|_| "bad frame size")?;
    let old = FRAME_MEMORY.lock().take();
    if let Some((_, mapped)) = old.filter(|o| o.1 == size) {
        *FRAME_MEMORY.lock() = old;
        unsafe { (cpu as *mut u8).write_bytes(0, mapped) };
        return Ok(cpu as *mut u32);
    }
    if let Some((memory, mapped)) = old {
        let was = core::alloc::Layout::from_size_align(mapped, PAGE).map_err(|_| "bad frame size")?;
        unsafe { alloc::alloc::dealloc(memory as *mut u8, was) };
    }
    let memory = unsafe { alloc::alloc::alloc(layout) };
    if memory.is_null() {
        return Err("no memory for the frame".into());
    }
    *FRAME_MEMORY.lock() = Some((memory as usize, size));
    // The display reads memory bypassing the CPU cache: the cache must be empty, and this
    // frame may only be written through the aperture.
    for line in (0..size).step_by(CACHE_LINE) {
        unsafe { core::arch::x86_64::_mm_clflush(memory.add(line)) };
    }
    let table = mmio + GGTT + FRAME_GGTT / PAGE * 8;
    for page in 0..size.div_ceil(PAGE) {
        let pte = (memory as u64 + (page * PAGE) as u64) | PTE_PRESENT;
        unsafe { (table as *mut u64).add(page).write_volatile(pte) };
    }
    wr(mmio, GGTT_FLUSH, 1);
    crate::paging::map_frame(cpu, size as u64);
    unsafe { (cpu as *mut u8).write_bytes(0, size) };
    Ok(cpu as *mut u32)
}

const PCODE_MAILBOX: usize = 0x138124;
const PCODE_DATA: usize = 0x138128;
const PCODE_DATA1: usize = 0x13812C;
const PCODE_BUSY: u32 = 1 << 31;
const PCODE_MEMORY_LATENCY: u32 = 6;
/// Size of a display buffer block.
const BLOCK: u64 = 512;
const PIPE_UNDERRUN: u32 = 1 << 31;
const WM_MAX_LINES: u64 = 31;

/// Asks the CPU firmware for the four memory wake-up latency values.
fn pcode_latency(mmio: usize, part: u32) -> Option<[u8; 4]> {
    if !wait(mmio, PCODE_MAILBOX, PCODE_BUSY, 0) {
        return None;
    }
    wr(mmio, PCODE_DATA, part);
    wr(mmio, PCODE_DATA1, 0);
    wr(mmio, PCODE_MAILBOX, PCODE_BUSY | PCODE_MEMORY_LATENCY);
    wait(mmio, PCODE_MAILBOX, PCODE_BUSY, 0).then(|| rd(mmio, PCODE_DATA).to_le_bytes())
}

/// How many microseconds memory takes to wake from each sleep level.
fn memory_latency(mmio: usize) -> Option<[u8; WM_LEVELS]> {
    let (low, high) = (pcode_latency(mmio, 0)?, pcode_latency(mmio, 1)?);
    Some([low[0], low[1], low[2], low[3], high[0], high[1], high[2], high[3]])
}

fn plane_levels(mmio: usize, plane: usize) -> [u32; WM_LEVELS] {
    core::array::from_fn(|level| rd(mmio, plane + PLANE_WM + 4 * level))
}

/// The plane's reserve for `latency` µs without memory: buffer blocks and lines.
fn level_for(m: &Mode, latency: u64) -> (u64, u64) {
    let bytes_per_ms = m.pixel_khz as u64 * BYTES_PER_PIXEL as u64;
    let by_time = (latency * bytes_per_ms).div_ceil(1000 * BLOCK);
    let line_us_x1000 = m.htotal as u64 * 1_000_000 / m.pixel_khz as u64;
    let lines = (latency * 1000).div_ceil(line_us_x1000);
    let per_line = (m.width as u64 * BYTES_PER_PIXEL as u64).div_ceil(BLOCK);
    (by_time.min(lines * per_line) + 1, lines)
}

/// Lets memory sleep while the monitor shows a picture: without these levels the CPU
/// does not go deeper than PC2. We compute the values ourselves, so a watchdog checks for output underruns.
/// Returns how many levels are enabled.
pub fn deep_watermarks(on: bool) -> Result<usize, String> {
    let mmio = RUNNING.load(Ordering::Relaxed);
    let Some((mode, _)) = *OUTPUT.lock() else { return Err("no monitor is running".into()) };
    if mmio == 0 {
        return Err("no monitor is running".into());
    }
    let room = {
        let buf = rd(mmio, PLANE_B + PLANE_BUF_CFG);
        ((buf >> 16 & 0xFFF) - (buf & 0xFFF)) as u64
    };
    let mut enabled = 0;
    if on {
        let latency = memory_latency(mmio).ok_or("the processor does not report memory latency")?;
        crate::kprintln!("hdmi: memory wakes in {latency:?} us, display buffer {room} blocks");
        let mut last = (WM_B & WM_BLOCKS_MASK) as u64;
        for level in 1..WM_LEVELS {
            let us = latency[level] as u64;
            // A zero latency means this level and the following ones do not exist.
            if us == 0 {
                break;
            }
            let (blocks, lines) = level_for(&mode, us);
            let blocks = blocks.max(last);
            if blocks >= room || lines > WM_MAX_LINES {
                break;
            }
            last = blocks;
            wr(mmio, PLANE_B + PLANE_WM + 4 * level, WM_ENABLE | (lines as u32) << WM_LINES_SHIFT | blocks as u32);
            enabled = level;
        }
    }
    for level in enabled + 1..WM_LEVELS {
        wr(mmio, PLANE_B + PLANE_WM + 4 * level, 0);
    }
    // Writing the frame address latches the new values on the next frame.
    wr(mmio, PLANE_B + PLANE_SURF, FRAME_GGTT as u32);
    wr(mmio, PIPE_IIR_B, PIPE_UNDERRUN);
    WATERMARKS.store(enabled != 0, Ordering::Relaxed);
    crate::kprintln!("hdmi: plane B wm {:08x?}", plane_levels(mmio, PLANE_B));
    Ok(enabled)
}

pub fn running() -> bool {
    RUNNING.load(Ordering::Relaxed) != 0
}

pub fn watermarks_on() -> bool {
    WATERMARKS.load(Ordering::Relaxed)
}

/// Once a second: whether the output underran because memory was allowed to sleep for too long.
pub fn watch_watermarks() {
    let mmio = RUNNING.load(Ordering::Relaxed);
    if WATERMARKS.load(Ordering::Relaxed) && mmio != 0 && rd(mmio, PIPE_IIR_B) & PIPE_UNDERRUN != 0 {
        crate::kprintln!("hdmi: the display ran out of data, memory sleep is off again");
        let _ = deep_watermarks(false);
    }
}

/// Points plane B at the monitor frame in the free part of the display buffer.
fn start_plane(mmio: usize, m: &Mode, stride: u32) -> Result<(), String> {
    let plane_a = rd(mmio, PLANE_A + PLANE_BUF_CFG);
    let cursor_a = rd(mmio, CURSOR_BUF_CFG_A);
    let first = (plane_a >> 16 & 0xFFF).max(cursor_a >> 16 & 0xFFF) + 1;
    crate::kprintln!("hdmi: display buffer from {first} to {DDB_LAST}, plane A {plane_a:08x}, cursor A {cursor_a:08x}");
    if first + (WM_B & WM_BLOCKS_MASK) > DDB_LAST {
        return Err("no room in the display buffer".into());
    }
    wr(mmio, PLANE_B + PLANE_STRIDE, stride / STRIDE_UNIT);
    wr(mmio, PLANE_B + PLANE_POS, 0);
    wr(mmio, PLANE_B + PLANE_SIZE, (m.height - 1) << 16 | (m.width - 1));
    wr(mmio, PLANE_B + PLANE_OFFSET, 0);
    wr(mmio, PLANE_B + PLANE_WM, WM_B);
    for level in 1..WM_LEVELS {
        wr(mmio, PLANE_B + PLANE_WM + 4 * level, 0);
    }
    wr(mmio, PLANE_B + PLANE_WM_TRANS, WM_B);
    wr(mmio, PLANE_B + PLANE_BUF_CFG, DDB_LAST << 16 | first);
    wr(mmio, PLANE_B + PLANE_CTL, PLANE_CTL_XRGB);
    wr(mmio, PLANE_B + PLANE_SURF, FRAME_GGTT as u32);
    Ok(())
}

fn enable(gpu: &pci::Device, mmio: usize, want: Option<Resolution>) -> Result<Framebuffer, String> {
    if rd(mmio, PIPE_CONF_B) & PIPE_ENABLE != 0 {
        return Err("pipe B is already running".into());
    }
    let edid = read_edid(mmio)?;
    let modes = modes_from(&edid, pixel_limit(mmio));
    let chosen = want.and_then(|w| modes.iter().find(|m| m.resolution() == w)).or(modes.first());
    let mode = *chosen.ok_or("no usable mode in EDID")?;
    let link = if mode.bytes_per_ms() < HBR.khz * LANES as u64 { HBR } else { HBR2 };
    let list: Vec<String> = modes.iter().map(|m| m.resolution()).map(|r| format!("{}x{}@{}", r.w, r.h, r.hz)).collect();
    crate::kprintln!("hdmi: {} offers {}", crate::hwprobe::monitor_name(&edid), list.join(" "));
    crate::kprintln!("hdmi: using {}x{}, {} kHz, link {} MHz", mode.width, mode.height, mode.pixel_khz, link.khz / 1000);
    let stride = (mode.width * BYTES_PER_PIXEL).next_multiple_of(STRIDE_UNIT);
    let size = (stride * mode.height) as usize;
    let frame = map_frame(gpu, mmio, size.next_multiple_of(FRAME_ALIGN))?;
    start_port(mmio, link)?;
    train(mmio, link)?;
    wr(mmio, DP_TP_CTL_B, TP_ENABLE | TP_ENHANCED | TP_IDLE);
    // Not a problem: this is only a delay before starting the pipe, the log will show it.
    if !wait(mmio, DP_TP_STATUS_B, TP_IDLE_DONE, TP_IDLE_DONE) {
        crate::kprintln!("hdmi: no idle report, status {:08x}", rd(mmio, DP_TP_STATUS_B));
    }
    start_pipe(mmio, &mode, link);
    start_plane(mmio, &mode, stride)?;
    wr(mmio, PIPE_CONF_B, PIPE_ENABLE);
    if !wait(mmio, PIPE_CONF_B, PIPE_ACTIVE, PIPE_ACTIVE) {
        return Err("pipe B did not start".into());
    }
    wr(mmio, DP_TP_CTL_B, TP_ENABLE | TP_ENHANCED | TP_NORMAL);
    pause_us(SETTLE_US);
    crate::kprintln!(
        "hdmi: pipe B running, frames {}, isr {:08x}, sticky {:08x}, ddi {:08x}",
        rd(mmio, FRAME_COUNT_B),
        rd(mmio, PIPE_ISR_B),
        rd(mmio, PIPE_IIR_B),
        rd(mmio, DDI_BUF_CTL_B)
    );
    crate::kprintln!("hdmi: memory wakes in {:?} us, plane A wm {:08x?}, plane B wm {:08x?}", memory_latency(mmio), plane_levels(mmio, PLANE_A), plane_levels(mmio, PLANE_B));
    *OUTPUT.lock() = Some((mode, stride));
    *MODES.lock() = modes;
    *EDID.lock() = edid;
    let screen = Framebuffer { base: frame, w: mode.width as i32, h: mode.height as i32, stride: (stride / BYTES_PER_PIXEL) as i32, bgr: true };
    *SCREEN.lock() = Some(screen);
    Ok(screen)
}

/// Register address while the monitor is running.
static RUNNING: AtomicUsize = AtomicUsize::new(0);
/// Higher watermark levels are enabled.
static WATERMARKS: AtomicBool = AtomicBool::new(false);
/// Whether memory sleep was enabled when the monitor went to sleep.
static SLEPT_WATERMARKS: AtomicBool = AtomicBool::new(false);
/// This computer's GPU, if we know it.
static GPU_DEVICE: IrqLock<Option<(pci::Device, usize)>> = IrqLock::new(None);
/// Monitor frame memory and its size. The frame pages are mapped into the GGTT.
static FRAME_MEMORY: IrqLock<Option<(usize, usize)>> = IrqLock::new(None);
/// The monitor mode and the line stride of its frame.
static OUTPUT: IrqLock<Option<(Mode, u32)>> = IrqLock::new(None);
/// What the monitor can do, its EDID and frame, while the port is running.
static MODES: IrqLock<Vec<Mode>> = IrqLock::new(Vec::new());
static EDID: IrqLock<Vec<u8>> = IrqLock::new(Vec::new());
static SCREEN: IrqLock<Option<Framebuffer>> = IrqLock::new(None);
/// Which mode was requested; None — the native one.
static WANTED: IrqLock<Option<Resolution>> = IrqLock::new(None);
static PANEL: IrqLock<Panel> = IrqLock::new(Panel::Own);
/// The panel plane configuration as the firmware left it.
static PANEL_CTL: IrqLock<u32> = IrqLock::new(0);

/// What the built-in screen shows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    /// Its own frame, as the firmware set it up.
    Own,
    Off,
}

/// Whether to show its frame on the monitor.
pub fn show(own: bool) {
    let mmio = RUNNING.load(Ordering::Relaxed);
    if mmio == 0 {
        return;
    }
    wr(mmio, PLANE_B + PLANE_CTL, if own { PLANE_CTL_XRGB } else { 0 });
    wr(mmio, PLANE_B + PLANE_SURF, FRAME_GGTT as u32);
}

/// Switches the built-in screen. Its pipe keeps running.
pub fn panel(want: Panel) {
    let mmio = RUNNING.load(Ordering::Relaxed);
    let was = *PANEL.lock();
    if mmio == 0 || was == want {
        return;
    }
    if was == Panel::Own {
        *PANEL_CTL.lock() = rd(mmio, PLANE_A + PLANE_CTL);
    }
    let ctl = if want == Panel::Own { *PANEL_CTL.lock() } else { 0 };
    wr(mmio, PLANE_A + PLANE_CTL, ctl);
    wr(mmio, PLANE_A + PLANE_SURF, rd(mmio, PLANE_A + PLANE_SURF));
    *PANEL.lock() = want;
}

/// The monitor sleeps: no signal, the monitor itself goes to standby.
pub fn sleep() {
    if RUNNING.load(Ordering::Relaxed) != 0 {
        SLEPT_WATERMARKS.store(WATERMARKS.load(Ordering::Relaxed), Ordering::Relaxed);
        shut_down();
    }
}

/// The monitor wakes up: the same mode and settings as before.
pub fn wake() -> Option<Framebuffer> {
    let frame = restart()?;
    if SLEPT_WATERMARKS.swap(false, Ordering::Relaxed)
        && let Err(e) = deep_watermarks(true)
    {
        crate::kprintln!("hdmi: {e}");
    }
    Some(frame)
}

/// Blanks the monitor before the computer shuts down.
pub fn stop() {
    if RUNNING.load(Ordering::Relaxed) != 0 {
        // The shutdown message must have time to reach the monitor.
        pause_us(FAREWELL_US);
        shut_down();
    }
}

/// Turns the port off in reverse order so the firmware gets it free.
pub fn shut_down() {
    if RUNNING.load(Ordering::Relaxed) == 0 {
        return;
    }
    panel(Panel::Own);
    let mmio = RUNNING.swap(0, Ordering::Relaxed);
    WATERMARKS.store(false, Ordering::Relaxed);
    *OUTPUT.lock() = None;
    *SCREEN.lock() = None;
    wr(mmio, PLANE_B + PLANE_CTL, 0);
    wr(mmio, PLANE_B + PLANE_SURF, 0);
    wr(mmio, PIPE_CONF_B, 0);
    wait(mmio, PIPE_CONF_B, PIPE_ACTIVE, 0);
    wr(mmio, TRANS_DDI_FUNC_B, 0);
    wr(mmio, DDI_BUF_CTL_B, 0);
    wait(mmio, DDI_BUF_CTL_B, DDI_IDLE, DDI_IDLE);
    wr(mmio, DP_TP_CTL_B, 0);
    wr(mmio, TRANS_CLK_SEL_B, 0);
    wr(mmio, DPLL_CTRL2, rd(mmio, DPLL_CTRL2) | DDI_B_CLOCK_OFF);
    wr(mmio, DPLL1_CTL, rd(mmio, DPLL1_CTL) & !PLL_ENABLE);
    wr(mmio, POWER_DRIVER, rd(mmio, POWER_DRIVER) & !DDI_B_POWER_REQUEST);
}

/// Enables the monitor on HDMI, if there is one. A failure does not affect the built-in screen.
pub fn start(devices: &[pci::Device]) -> Option<Framebuffer> {
    let gpu = devices.iter().find(|d| (d.vendor, d.device) == GPU)?;
    let mmio = gpu.mmio(0, GPU_MMIO);
    *GPU_DEVICE.lock() = Some((*gpu, mmio));
    restart()
}

/// Modes of the connected monitor: the native one first.
pub fn modes() -> Vec<Resolution> {
    MODES.lock().iter().map(Mode::resolution).collect()
}

pub fn current_mode() -> Option<Resolution> {
    OUTPUT.lock().map(|(mode, _)| mode.resolution())
}

/// Switches the monitor to another mode. If that fails, returns to the previous one.
pub fn switch(want: Option<Resolution>) -> Option<Framebuffer> {
    let was = current_mode();
    if want.or(modes().first().copied()) == was {
        return *SCREEN.lock();
    }
    shut_down();
    *WANTED.lock() = want;
    restart().or_else(|| {
        crate::kprintln!("hdmi: cannot use that mode, going back");
        *WANTED.lock() = was;
        restart()
    })
}

/// The monitor is on the connector again. If it is the same monitor and the link is intact, the picture is already there.
pub fn resume() -> Option<Framebuffer> {
    let (_, mmio) = (*GPU_DEVICE.lock())?;
    if RUNNING.load(Ordering::Relaxed) != 0 {
        let same = read_edid(mmio).is_ok_and(|edid| edid == *EDID.lock());
        let linked = dpcd_read(mmio, DPCD_STATUS, 3).is_ok_and(|s| lanes_ready(&s, LANE_EQ_DONE) && s[2] & INTERLANE_ALIGNED != 0);
        crate::kprintln!("hdmi: back on the port, same monitor {same}, link {linked}");
        if same && linked {
            show(true);
            return *SCREEN.lock();
        }
        shut_down();
    }
    restart()
}

/// Whether a monitor is connected to the connector.
pub fn plugged() -> bool {
    let gpu = *GPU_DEVICE.lock();
    gpu.is_some_and(|(_, mmio)| rd(mmio, SDEISR) & PORT_B_LIVE != 0)
}

/// Enables a monitor connected to the connector.
pub fn restart() -> Option<Framebuffer> {
    if !plugged() {
        return None;
    }
    let (gpu, mmio) = (*GPU_DEVICE.lock())?;
    let want = *WANTED.lock();
    match enable(&gpu, mmio, want) {
        Ok(frame) => {
            RUNNING.store(mmio, Ordering::Relaxed);
            Some(frame)
        }
        Err(e) => {
            crate::kprintln!("hdmi: {e}");
            None
        }
    }
}
