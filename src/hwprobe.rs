//! A hardware snapshot for drivers that cannot be tested in a VM: the Intel GPU
//! (connectors, display state, monitor EDID) and the Wi-Fi module.
//!
//! Display settings are not changed. Only the monitor buses (GMBUS and AUX) are touched, and,
//! for the duration of the probe, the power of ports B and C: nobody uses them anyway. The result
//! goes to the USB stick (or to /glassos-hw).

use crate::{fs, pci};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

const DIR: &str = "glassos-hw";
const GPU_MMIO: u64 = 16 << 20;
const WIFI_MMIO: u64 = 16 << 10;
/// OpRegion — the firmware table for the graphics driver, it holds the VBT.
const OPREGION_SIZE: usize = 8 << 10;
/// The bus wait limit in TSC cycles (about 0.1 s): an EDID at 100 kHz is read in 15 ms.
/// The cycle counter runs with interrupts disabled too, the timer is not suitable here.
const BUS_CYCLES: u64 = 200_000_000;

const SOUTH_CLOCK_GATE: usize = 0xC2020;
const GMBUS_GATE_OFF: u32 = 1 << 31;
/// The DDC bus (GMBUS) of newer GPUs: in the south bridge. On Bay Trail it is in the display block.
const PCH_GMBUS: usize = 0xC5100;
const GMBUS_SELECT: usize = 0x0;
const GMBUS_COMMAND: usize = 0x4;
const GMBUS_STATUS: usize = 0x8;
const GMBUS_DATA: usize = 0xC;
const CLEAR_INT: u32 = 1 << 31;
const SW_READY: u32 = 1 << 30;
const CYCLE_INDEX_WAIT: u32 = 3 << 25;
const CYCLE_STOP: u32 = 4 << 25;
const HW_READY: u32 = 1 << 11;
const NAK: u32 = 1 << 10;
const ACTIVE: u32 = 1 << 9;

const SDEISR: usize = 0xC4000;
/// The live state of the port B and C connectors in SDEISR.
const HPD_LIVE: [(&str, u32); 2] = [("B", 1 << 21), ("C", 1 << 22)];
/// The driver's request to power the port and its acknowledgement.
const POWER_DRIVER: usize = 0x45404;
const POWER_BIOS: usize = 0x45400;
const DDI_POWER: [(u32, u32); 2] = [(1 << 5, 1 << 4), (1 << 7, 1 << 6)];

const AUX_CTL: [usize; 2] = [0x64110, 0x64210];
const AUX_DATA_REGS: usize = 5;
const AUX_BUSY: u32 = 1 << 31;
const AUX_DONE: u32 = 1 << 30;
const AUX_TIMEOUT: u32 = 1 << 28;
const AUX_RECEIVE_ERROR: u32 = 1 << 25;
const AUX_START: u32 = AUX_BUSY | AUX_DONE | AUX_TIMEOUT | AUX_RECEIVE_ERROR | 3 << 26 | 0x23F;
const AUX_RETRIES: usize = 5;
/// AUX commands: DPCD read, I2C write and read with the bus held.
const AUX_NATIVE_READ: u8 = 0x90;
const AUX_I2C_WRITE_MOT: u8 = 0x40;
const AUX_I2C_READ_MOT: u8 = 0x50;
const AUX_I2C_READ: u8 = 0x10;
const AUX_CHUNK: usize = 16;

/// Display registers: (name, first, count, step).
const GPU_REGS: &[(&str, usize, usize, usize)] = &[
    ("FUSE_STATUS", 0x42000, 1, 0),
    ("PWR_WELL_CTL", 0x45400, 4, 4),
    ("DC_STATE_EN", 0x45504, 1, 0),
    ("CDCLK_CTL", 0x46000, 1, 0),
    ("LCPLL_CTL", 0x46010, 2, 4),
    ("WRPLL_CTL", 0x46040, 2, 0x20),
    ("TRANS_CLK_SEL", 0x46140, 3, 4),
    ("DPLL_CFGCR", 0x6C040, 6, 4),
    ("DPLL_CTRL", 0x6C058, 3, 4),
    ("DDI_BUF_CTL", 0x64000, 5, 0x100),
    ("DP_TP_CTL", 0x64040, 5, 0x100),
    ("AUX_CH_CTL", 0x64010, 4, 0x100),
    ("TRANS_TIMING_A", 0x60000, 8, 4),
    ("TRANS_TIMING_B", 0x61000, 8, 4),
    ("TRANS_TIMING_C", 0x62000, 8, 4),
    ("TRANS_TIMING_EDP", 0x6F000, 8, 4),
    ("TRANS_DDI_FUNC_CTL", 0x60400, 3, 0x1000),
    ("TRANS_DDI_FUNC_EDP", 0x6F400, 1, 0),
    ("PIPECONF", 0x70008, 3, 0x1000),
    ("PIPECONF_EDP", 0x7F008, 1, 0),
    ("PLANE_CTL", 0x70180, 3, 0x1000),
    ("PLANE_STRIDE", 0x70188, 3, 0x1000),
    ("PLANE_SIZE", 0x70190, 3, 0x1000),
    ("PLANE_SURF", 0x7019C, 3, 0x1000),
    ("PS_CTRL", 0x68180, 3, 0x800),
    ("PP_STATUS", 0xC7200, 2, 4),
    ("SFUSE_STRAP", 0xC2014, 1, 0),
    ("SDEISR", 0xC4000, 1, 0),
    ("SHOTPLUG_CTL", 0xC4030, 1, 0),
    ("SHOTPLUG_CTL2", 0xC403C, 1, 0),
    ("DE_PORT_ISR", 0x44440, 1, 0),
    ("PIPE_IIR", 0x44408, 2, 0x10),
];

/// CSR registers of the Wi-Fi module.
const WIFI_REGS: &[(&str, usize)] = &[
    ("HW_IF_CONFIG", 0x000),
    ("INT", 0x008),
    ("INT_MASK", 0x00C),
    ("FH_INT", 0x010),
    ("RESET", 0x020),
    ("GP_CNTRL", 0x024),
    ("HW_REV", 0x028),
    ("EEPROM_REG", 0x02C),
    ("GIO", 0x03C),
    ("GP_UCODE", 0x048),
    ("GP_DRIVER", 0x050),
    ("UCODE_DRV_GP1", 0x054),
    ("HW_RF_ID", 0x09C),
];

pub fn deadline(cycles: u64) -> u64 {
    crate::arch::rdtsc() + cycles
}

pub fn expired(end: u64) -> bool {
    crate::arch::rdtsc() >= end
}

pub fn rd(mmio: usize, reg: usize) -> u32 {
    unsafe { ((mmio + reg) as *const u32).read_volatile() }
}

pub fn wr(mmio: usize, reg: usize, value: u32) {
    unsafe { ((mmio + reg) as *mut u32).write_volatile(value) }
}

fn read_phys(phys: u64, len: usize) -> Vec<u8> {
    crate::paging::map_mmio(phys, len as u64);
    (0..len).map(|i| unsafe { ((phys as usize + i) as *const u8).read_volatile() }).collect()
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

fn config_space(out: &mut String, d: &pci::Device) {
    for chunk in (0..=255u8).step_by(4).collect::<Vec<_>>().chunks(4) {
        let line: Vec<String> = chunk.iter().map(|&at| format!("{:08x}", d.read(at))).collect();
        out.push_str(&format!("  {:02x}: {}\n", chunk[0], line.join(" ")));
    }
}

fn pci_list(out: &mut String, devices: &[pci::Device]) {
    out.push_str("== PCI\n");
    for d in devices {
        let (sub, rev) = (d.read(0x2C), d.read(0x08) as u8);
        out.push_str(&format!(
            "{:02x}:{:02x}.{} {:04x}:{:04x} rev {rev:02x} class {:02x}{:02x} sub {:04x}:{:04x}\n",
            d.bus, d.slot, d.func, d.vendor, d.device, d.class, d.subclass, sub as u16, sub >> 16
        ));
    }
}

fn wait_bus(mmio: usize, bus: usize) -> Result<(), &'static str> {
    let end = deadline(BUS_CYCLES);
    while !expired(end) {
        let status = rd(mmio, bus + GMBUS_STATUS);
        if status & NAK != 0 {
            return Err("no answer");
        }
        if status & HW_READY != 0 {
            return Ok(());
        }
    }
    Err("timeout")
}

fn release_bus(mmio: usize, bus: usize, stop: bool) {
    if stop {
        wr(mmio, bus + GMBUS_COMMAND, SW_READY | CYCLE_STOP);
        let end = deadline(BUS_CYCLES);
        while !expired(end) && rd(mmio, bus + GMBUS_STATUS) & (ACTIVE | NAK) == ACTIVE {
            core::hint::spin_loop();
        }
    }
    if rd(mmio, bus + GMBUS_STATUS) & NAK != 0 {
        wr(mmio, bus + GMBUS_COMMAND, CLEAR_INT);
        wr(mmio, bus + GMBUS_COMMAND, 0);
    }
    wr(mmio, bus + GMBUS_SELECT, 0);
}

/// Reads a device on the DDC bus whose registers are at address `bus`: `pin` is the line pair
/// number, `addr` the I2C address.
pub fn ddc_read_on(mmio: usize, bus: usize, pin: u32, addr: u32, offset: u32, len: usize) -> Result<Vec<u8>, &'static str> {
    wr(mmio, bus + GMBUS_SELECT, pin);
    let read = SW_READY | CYCLE_INDEX_WAIT | (len as u32) << 16 | offset << 8 | addr << 1 | 1;
    wr(mmio, bus + GMBUS_COMMAND, read);
    let mut data = Vec::with_capacity(len);
    while data.len() < len {
        if let Err(e) = wait_bus(mmio, bus) {
            release_bus(mmio, bus, false);
            return Err(e);
        }
        let word = rd(mmio, bus + GMBUS_DATA).to_le_bytes();
        let take = (len - data.len()).min(4);
        data.extend_from_slice(&word[..take]);
    }
    release_bus(mmio, bus, true);
    Ok(data)
}

fn ddc_read(mmio: usize, pin: u32, addr: u32, offset: u32, len: usize) -> Result<Vec<u8>, &'static str> {
    ddc_read_on(mmio, PCH_GMBUS, pin, addr, offset, len)
}

/// The monitor name from the EDID (descriptor 0xFC).
pub fn monitor_name(edid: &[u8]) -> String {
    for block in edid.get(54..126).unwrap_or(&[]).chunks(18) {
        if block[..3] == [0, 0, 0] && block[3] == 0xFC {
            return block[5..].iter().take_while(|&&b| b != b'\n').map(|&b| b as char).collect::<String>().trim().into();
        }
    }
    "unnamed".into()
}

fn ddc_scan(out: &mut String, mmio: usize, summary: &mut Vec<String>) {
    const EDID_HEADER: [u8; 8] = [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0];
    out.push_str("== DDC\n");
    let gate = rd(mmio, SOUTH_CLOCK_GATE);
    wr(mmio, SOUTH_CLOCK_GATE, gate | GMBUS_GATE_OFF);
    // VBT: port B on pair 1, port C on pair 2.
    for pin in 1..=2 {
        match ddc_read(mmio, pin, 0x50, 0, 128) {
            Ok(edid) if edid[..8] == EDID_HEADER => {
                summary.push(format!("Monitor on DDC pin {pin}: {}", monitor_name(&edid)));
                out.push_str(&format!("pin {pin} edid {}\n", hex(&edid)));
            }
            Ok(edid) => out.push_str(&format!("pin {pin} data {}\n", hex(&edid))),
            Err(e) => out.push_str(&format!("pin {pin} {e}\n")),
        }
        // A DP dual mode adapter: the signature at address 0x40.
        if let Ok(id) = ddc_read(mmio, pin, 0x40, 0, 32) {
            out.push_str(&format!("pin {pin} adaptor {}\n", hex(&id)));
        }
    }
    wr(mmio, SOUTH_CLOCK_GATE, gate);
}

/// One AUX transaction: returns the answer, the first byte is its status.
fn aux_transfer(mmio: usize, ctl: usize, send: &[u8]) -> Result<Vec<u8>, &'static str> {
    let data = ctl + 4;
    for (i, chunk) in send.chunks(4).enumerate() {
        let word = chunk.iter().enumerate().fold(0u32, |w, (j, &b)| w | (b as u32) << (24 - 8 * j));
        wr(mmio, data + i * 4, word);
    }
    wr(mmio, ctl, AUX_START | (send.len() as u32) << 20);
    let end = deadline(BUS_CYCLES);
    while rd(mmio, ctl) & AUX_BUSY != 0 {
        if expired(end) {
            return Err("busy");
        }
    }
    let status = rd(mmio, ctl);
    wr(mmio, ctl, status | AUX_DONE | AUX_TIMEOUT | AUX_RECEIVE_ERROR);
    if status & AUX_TIMEOUT != 0 {
        return Err("timeout");
    }
    if status & AUX_RECEIVE_ERROR != 0 {
        return Err("receive error");
    }
    let len = (status >> 20 & 0x1F) as usize;
    let mut reply = Vec::with_capacity(len);
    for i in 0..AUX_DATA_REGS {
        reply.extend_from_slice(&rd(mmio, data + i * 4).to_be_bytes());
    }
    reply.truncate(len);
    Ok(reply)
}

/// A transaction with retries: the sink may ask to wait (DEFER).
pub fn aux(mmio: usize, ctl: usize, send: &[u8]) -> Result<Vec<u8>, &'static str> {
    const DEFER: u8 = 0x20;
    const I2C_DEFER: u8 = 0x80;
    let mut last = Err("no reply");
    for _ in 0..AUX_RETRIES {
        last = aux_transfer(mmio, ctl, send);
        match &last {
            Err("timeout") => return last,
            Ok(reply) if reply.first().is_some_and(|b| b & (DEFER | I2C_DEFER) == 0) => return last,
            _ => {}
        }
        wait_tick();
    }
    last
}

pub fn wait_tick() {
    let end = deadline(BUS_CYCLES / 100);
    while !expired(end) {
        core::hint::spin_loop();
    }
}

/// Reading an I2C device through AUX.
pub fn aux_i2c(mmio: usize, ctl: usize, addr: u8, offset: u8, len: usize) -> Result<Vec<u8>, &'static str> {
    const ACK_MASK: u8 = 0xF0;
    let ack = |reply: Vec<u8>| if reply.first().is_some_and(|b| b & ACK_MASK == 0) { Ok(reply) } else { Err("no answer") };
    ack(aux(mmio, ctl, &[AUX_I2C_WRITE_MOT, 0, addr, 0, offset])?)?;
    let mut data = Vec::with_capacity(len);
    while data.len() < len {
        let want = (len - data.len()).min(AUX_CHUNK);
        let reply = ack(aux(mmio, ctl, &[AUX_I2C_READ_MOT, 0, addr, want as u8 - 1])?)?;
        if reply.len() < 2 {
            break;
        }
        data.extend_from_slice(&reply[1..]);
    }
    let _ = aux(mmio, ctl, &[AUX_I2C_READ, 0, addr]);
    Ok(data)
}

fn aux_scan(out: &mut String, mmio: usize, label: &str, summary: &mut Vec<String>) {
    for (port, ctl) in AUX_CTL.iter().enumerate() {
        let name = HPD_LIVE[port].0;
        match aux(mmio, *ctl, &[AUX_NATIVE_READ, 0, 0, 15]) {
            Ok(dpcd) => {
                out.push_str(&format!("{label} aux {name} dpcd {}\n", hex(&dpcd)));
                summary.push(format!("Port {name}: AUX answers ({label})"));
            }
            Err(e) => out.push_str(&format!("{label} aux {name} dpcd {e}\n")),
        }
        match aux_i2c(mmio, *ctl, 0x50, 0, 128) {
            Ok(edid) => {
                out.push_str(&format!("{label} aux {name} edid {}\n", hex(&edid)));
                summary.push(format!("Port {name}: monitor over AUX: {}", monitor_name(&edid)));
            }
            Err(e) => out.push_str(&format!("{label} aux {name} edid {e}\n")),
        }
        if let Ok(id) = aux_i2c(mmio, *ctl, 0x40, 0, 32) {
            out.push_str(&format!("{label} aux {name} adaptor {}\n", hex(&id)));
        }
    }
}

/// Checks ports B and C: the live connector, AUX and DDC, with and without port power.
fn ports(out: &mut String, mmio: usize, summary: &mut Vec<String>) {
    out.push_str("== Ports\n");
    let live = rd(mmio, SDEISR);
    for (name, bit) in HPD_LIVE {
        summary.push(format!("Port {name}: {}", if live & bit != 0 { "connected" } else { "nothing detected" }));
    }
    aux_scan(out, mmio, "off", summary);
    let driver = rd(mmio, POWER_DRIVER);
    let bios = rd(mmio, POWER_BIOS);
    out.push_str(&format!("driver {driver:08x} bios {bios:08x}\n"));
    wr(mmio, POWER_DRIVER, driver | DDI_POWER[0].0 | DDI_POWER[1].0);
    let end = deadline(BUS_CYCLES);
    while !expired(end) && rd(mmio, POWER_DRIVER) & (DDI_POWER[0].1 | DDI_POWER[1].1) != DDI_POWER[0].1 | DDI_POWER[1].1 {
        core::hint::spin_loop();
    }
    out.push_str(&format!("powered {:08x} sdeisr {:08x}\n", rd(mmio, POWER_DRIVER), rd(mmio, SDEISR)));
    aux_scan(out, mmio, "on", summary);
    ddc_scan(out, mmio, summary);
    wr(mmio, POWER_DRIVER, driver);
}

/// Copies the OpRegion and the separately stored VBT, returns their files.
fn opregion(out: &mut String, asls: u64, files: &mut Vec<(&'static str, Vec<u8>)>) {
    const VERSION: usize = 0x10;
    const RVDA: usize = 0x3BA;
    const RVDS: usize = 0x3C2;
    const MAX_VBT: u32 = 64 << 10;
    let region = read_phys(asls, OPREGION_SIZE);
    let field = |at: usize, n: usize| region[at..at + n].iter().rev().fold(0u64, |v, &b| v << 8 | b as u64);
    let version = field(VERSION, 4);
    let (rvda, rvds) = (field(RVDA, 8), field(RVDS, 4) as u32);
    out.push_str(&format!("== OpRegion at {asls:#x}, signature {}, version {version:#x}, rvda {rvda:#x}, rvds {rvds:#x}\n", String::from_utf8_lossy(&region[..16])));
    if rvda != 0 && rvds > 0 && rvds <= MAX_VBT {
        for base in [rvda, asls + rvda] {
            if base < 1 << 39 && read_phys(base, 4) == *b"$VBT" {
                files.push(("vbt.bin", read_phys(base, rvds as usize)));
                break;
            }
        }
    }
    files.push(("opregion.bin", region));
}

fn gpu(out: &mut String, d: &pci::Device, files: &mut Vec<(&'static str, Vec<u8>)>, summary: &mut Vec<String>) {
    summary.push(format!("Graphics: {:04x}:{:04x}", d.vendor, d.device));
    out.push_str("== GPU config\n");
    config_space(out, d);
    if crate::baytrail::supports(d) {
        // Bay Trail has other registers: the table below is for newer GPUs.
        out.push_str(&crate::baytrail::report(d));
    } else {
        let mmio = d.mmio(0, GPU_MMIO);
        out.push_str("== GPU registers\n");
        for &(name, first, count, step) in GPU_REGS {
            for i in 0..count {
                let reg = first + i * step;
                out.push_str(&format!("{name}[{i}] {reg:05x} = {:08x}\n", rd(mmio, reg)));
            }
        }
        ddc_scan(out, mmio, summary);
        ports(out, mmio, summary);
    }
    let asls = d.read(0xFC) as u64;
    if asls == 0 {
        summary.push("VBT: the firmware gave no OpRegion".into());
    } else {
        opregion(out, asls, files);
        summary.push(format!("VBT: saved from {asls:#x}"));
    }
}

fn wifi(out: &mut String, d: &pci::Device, summary: &mut Vec<String>) {
    const MEMORY_DECODE: u32 = 1 << 1;
    let sub = d.read(0x2C);
    out.push_str("== Wi-Fi config\n");
    config_space(out, d);
    if d.bar(0) == 0 {
        summary.push(format!("Wi-Fi: {:04x}:{:04x} sub {:04x}:{:04x}, no address assigned", d.vendor, d.device, sub as u16, sub >> 16));
        return;
    }
    let command = d.read(0x04);
    d.write(0x04, command | MEMORY_DECODE);
    let mmio = d.mmio(0, WIFI_MMIO);
    out.push_str("== Wi-Fi registers\n");
    for &(name, reg) in WIFI_REGS {
        out.push_str(&format!("{name} {reg:03x} = {:08x}\n", rd(mmio, reg)));
    }
    summary.push(format!(
        "Wi-Fi: {:04x}:{:04x} sub {:04x}:{:04x}, hw {:08x}, rf {:08x}",
        d.vendor, d.device, sub as u16, sub >> 16, rd(mmio, 0x028), rd(mmio, 0x09C)
    ));
    d.write(0x04, command);
}

/// The folder for the snapshot: on the USB stick, if there is one.
fn target() -> String {
    let stick = fs::volumes().into_iter().find(|v| v.removable && !v.network && !v.offline);
    match stick {
        Some(v) => format!("{}/{DIR}", v.path),
        None => format!("/{DIR}"),
    }
}

/// Takes the hardware snapshot and returns a short summary for the terminal.
pub fn run() -> Vec<String> {
    let devices = pci::scan();
    let mut out = String::new();
    let mut files = Vec::new();
    let mut summary = Vec::new();
    pci_list(&mut out, &devices);
    match devices.iter().find(|d| d.vendor == 0x8086 && d.class == 0x03 && d.bus == 0) {
        Some(d) => gpu(&mut out, d, &mut files, &mut summary),
        None => summary.push("Graphics: no Intel GPU".into()),
    }
    match devices.iter().find(|d| d.vendor == 0x8086 && d.class == 0x02 && d.subclass == 0x80) {
        Some(d) => wifi(&mut out, d, &mut summary),
        None => summary.push("Wi-Fi: no Intel module".into()),
    }
    let realtek = crate::rtl8723be::report();
    if !realtek.is_empty() {
        out.push_str("== RTL8723BE\n");
        for line in &realtek {
            out.push_str(line);
            out.push('\n');
        }
        summary.push(format!("Wi-Fi: Realtek RTL8723BE, {}", realtek.last().cloned().unwrap_or_default()));
    }
    out.push_str("== Display\n");
    let modes: Vec<String> = crate::display::modes().iter().map(|(w, h)| format!("{w}x{h}")).collect();
    let (cw, chh) = crate::display::current();
    out.push_str(&format!("firmware modes: {}\ncurrent: {cw}x{chh}\n", modes.join(" ")));
    let edid = crate::display::edid();
    out.push_str(&format!("monitor: {}\n", crate::display::describe_edid(&edid)));
    if crate::baytrail::active() {
        let now = crate::baytrail::current_mode().map_or(String::new(), |m| format!("{}x{}@{}", m.w, m.h, m.hz));
        out.push_str(&format!("own mode setting: now {now}\n"));
        summary.push(format!("Screen: {now}, set by GlassOS"));
    }
    if !edid.is_empty() {
        summary.push(format!("Monitor: {}", crate::display::describe_edid(&edid)));
        files.push(("edid.bin", edid));
    }
    out.push_str("== I2C\n");
    let free = match pci::free_mmio(&devices) {
        Some(at) => format!("pci: free memory for BARs from {at:#x}"),
        None => "pci: no free memory for BARs above 4 GiB".into(),
    };
    crate::kprintln!("{free}");
    out.push_str(&free);
    out.push('\n');
    let (lines, report_map) = crate::i2c::survey(&devices);
    for line in lines {
        crate::kprintln!("{line}");
        if line.contains(" is HID ") {
            summary.push(line.clone());
        }
        out.push_str(&line);
        out.push('\n');
    }
    if !report_map.is_empty() {
        files.push(("touchpad.bin", report_map));
    }
    out.push_str("== SATA\n");
    for line in crate::ahci::report() {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("== Boot\n");
    for line in crate::bootvars::list() {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("== USB\n");
    for line in crate::usb::list() {
        out.push_str(&line);
        out.push('\n');
    }
    files.push(("report.txt", out.into_bytes()));
    files.push(("touchpad.txt", (crate::touchpad::status().join("\n") + "\n").into_bytes()));
    files.push(("dmesg.txt", crate::serial::log_text().into_bytes()));

    save(&files, &mut summary);
    summary
}

/// Puts a file into the snapshot folder and returns a line for the terminal.
pub fn save_file(name: &'static str, data: &[u8]) -> String {
    let mut summary = Vec::new();
    save(&[(name, data.to_vec())], &mut summary);
    summary.join("; ")
}

/// Moves the previous boot's file aside under another name (the old `to` is lost).
pub fn keep_previous(name: &str, to: &str) {
    let dir = target();
    let (from, to) = (format!("{dir}/{name}"), format!("{dir}/{to}"));
    if fs::exists(&from) {
        let _ = fs::remove(&to);
        let _ = fs::rename(&from, &to);
    }
}

/// Puts files into the snapshot folder and saves them to disk right away.
fn save(files: &[(&str, Vec<u8>)], summary: &mut Vec<String>) {
    let dir = target();
    if !fs::exists(&dir)
        && let Err(e) = fs::mkdir(&dir)
    {
        summary.push(format!("cannot create {dir}: {e}"));
        return;
    }
    for (name, data) in files {
        if let Err(e) = fs::write(&format!("{dir}/{name}"), data) {
            summary.push(format!("cannot write {name}: {e}"));
        }
    }
    if let Err(e) = fs::sync() {
        summary.push(format!("not saved: {e}"));
    }
    summary.push(format!("Saved to {dir}"));
}
