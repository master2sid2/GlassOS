//! The I2C controllers of Intel chipsets (the DesignWare core). For now only to learn what sits
//! on the buses: a laptop touchpad may answer here over HID over I2C.

use crate::pci::Device;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

const IC_CON: usize = 0x00;
const IC_TAR: usize = 0x04;
const IC_DATA_CMD: usize = 0x10;
const IC_SS_HCNT: usize = 0x14;
const IC_SS_LCNT: usize = 0x18;
const IC_FS_HCNT: usize = 0x1C;
const IC_FS_LCNT: usize = 0x20;
const IC_SDA_HOLD: usize = 0x7C;
const IC_INTR_MASK: usize = 0x30;
const IC_RAW_INTR: usize = 0x34;
const IC_CLR_TX_ABRT: usize = 0x54;
const IC_ENABLE: usize = 0x6C;
const IC_STATUS: usize = 0x70;
const IC_RXFLR: usize = 0x78;
const IC_ENABLE_STATUS: usize = 0x9C;
const IC_COMP_TYPE: usize = 0xFC;
/// The DesignWare core signature in the type register.
const DESIGNWARE: u32 = 0x4457_0140;
/// The reset release register in the controller's private area.
const LPSS_RESETS: usize = 0x204;
const LPSS_OUT_OF_RESET: u32 = 3;

const CON_MASTER: u32 = 1;
const CON_STANDARD: u32 = 1 << 1;
const CON_FAST: u32 = 2 << 1;
const CON_RESTART: u32 = 1 << 5;
const CON_NO_SLAVE: u32 = 1 << 6;
const CMD_READ: u32 = 1 << 8;
const CMD_STOP: u32 = 1 << 9;
const CMD_RESTART: u32 = 1 << 10;
const STATUS_ACTIVITY: u32 = 1;
const STATUS_TX_ROOM: u32 = 1 << 1;
const STATUS_TX_EMPTY: u32 = 1 << 2;
const RAW_TX_ABORT: u32 = 1 << 6;
/// Halves of the bus period in controller clocks: 216 MHz gives about 100 kHz. If the clock
/// turns out lower, the bus runs slower, which does not matter for a probe.
const HALF_PERIOD: u32 = 1080;
/// Fast mode, 400 kHz: the high and low halves of the period (no shorter than 0.6 and 1.3 µs)
/// and the data hold after the clock falling edge (about 300 ns).
const FAST_HIGH: u32 = 200;
const FAST_LOW: u32 = 320;
const SDA_HOLD: u32 = 64;
/// How long to wait for a device: about 20 ms at 2 GHz.
const WAIT_CYCLES: u64 = 40_000_000;

const PCI_POWER_CAP: u32 = 1;
const PCI_CAP_LIST: u8 = 0x34;

/// Powers up a PCI function (state D0).
fn power_on(d: &Device) {
    let mut at = (d.read(PCI_CAP_LIST) & 0xFF) as u8;
    while at != 0 {
        let cap = d.read(at);
        if cap & 0xFF == PCI_POWER_CAP {
            let control = d.read(at + 4);
            if control & 3 != 0 {
                d.write(at + 4, control & !3);
                crate::hwprobe::wait_tick();
            }
            return;
        }
        at = (cap >> 8 & 0xFF) as u8;
    }
}

pub struct Bus {
    mmio: usize,
    fast: bool,
}

/// The touchpad driver and the probe both use the bus: the exchanges must not interleave.
static LOCK: crate::sync::YieldLock<()> = crate::sync::YieldLock::new(());

impl Bus {
    pub fn open(d: &Device) -> Option<Self> {
        if d.bar(0) == 0 {
            return None;
        }
        power_on(d);
        d.write(0x04, d.read(0x04) | 0b110);
        let bus = Self { mmio: d.mmio(0, 0x1000), fast: false };
        bus.wr(LPSS_RESETS, LPSS_OUT_OF_RESET);
        (bus.rd(IC_COMP_TYPE) == DESIGNWARE).then_some(bus)
    }

    /// Gives the controller an address if the firmware did not (this happens: the address is assigned by
    /// the system). `free` is the free space, moved past what was taken. On failure the address is removed.
    fn place(d: &Device, free: &mut Option<u64>) -> Result<u64, String> {
        let size = d.bar_size(0);
        let Some(at) = *free else { return Err("no free address".into()) };
        if size == 0 || size > 1 << 20 {
            return Err(format!("odd BAR size {size:#x}"));
        }
        let at = (at + size - 1) & !(size - 1);
        d.set_bar(0, at);
        *free = Some(at + size);
        Ok(at)
    }

    /// What the type register answers (for the report when the controller is not recognized).
    fn comp_type(d: &Device) -> u32 {
        if d.bar(0) == 0 {
            return 0;
        }
        crate::hwprobe::rd(d.mmio(0, 0x1000), IC_COMP_TYPE)
    }

    /// From here on the exchange runs at 400 kHz.
    pub fn set_fast(&mut self) {
        self.fast = true;
    }

    fn rd(&self, reg: usize) -> u32 {
        crate::hwprobe::rd(self.mmio, reg)
    }

    fn wr(&self, reg: usize, value: u32) {
        crate::hwprobe::wr(self.mmio, reg, value)
    }

    fn select(&self, addr: u8) -> Result<(), &'static str> {
        self.wr(IC_ENABLE, 0);
        let end = crate::hwprobe::deadline(WAIT_CYCLES);
        while self.rd(IC_ENABLE_STATUS) & 1 != 0 {
            if crate::hwprobe::expired(end) {
                return Err("the controller does not stop");
            }
        }
        let speed = if self.fast { CON_FAST } else { CON_STANDARD };
        self.wr(IC_CON, CON_MASTER | speed | CON_RESTART | CON_NO_SLAVE);
        self.wr(IC_TAR, addr as u32);
        self.wr(IC_SS_HCNT, HALF_PERIOD);
        self.wr(IC_SS_LCNT, HALF_PERIOD);
        self.wr(IC_FS_HCNT, FAST_HIGH);
        self.wr(IC_FS_LCNT, FAST_LOW);
        self.wr(IC_SDA_HOLD, SDA_HOLD);
        self.wr(IC_INTR_MASK, 0);
        self.wr(IC_ENABLE, 1);
        self.rd(IC_CLR_TX_ABRT);
        Ok(())
    }

    /// Writes `send`, then reads `len` bytes. None — the device did not answer.
    pub fn transfer(&self, addr: u8, send: &[u8], len: usize) -> Option<Vec<u8>> {
        let _bus = LOCK.lock();
        self.select(addr).ok()?;
        let mut sent = 0;
        let mut asked = 0;
        let mut out = Vec::with_capacity(len);
        let end = crate::hwprobe::deadline(WAIT_CYCLES + WAIT_CYCLES / 4 * len as u64 / 8);
        while sent < send.len() || asked < len || out.len() < len {
            if self.rd(IC_RAW_INTR) & RAW_TX_ABORT != 0 {
                self.rd(IC_CLR_TX_ABRT);
                return None;
            }
            if crate::hwprobe::expired(end) {
                return None;
            }
            if self.rd(IC_STATUS) & STATUS_TX_ROOM != 0 {
                if sent < send.len() {
                    let last = sent + 1 == send.len() && len == 0;
                    self.wr(IC_DATA_CMD, send[sent] as u32 | if last { CMD_STOP } else { 0 });
                    sent += 1;
                } else if asked < len {
                    let mut cmd = CMD_READ;
                    if asked == 0 && !send.is_empty() {
                        cmd |= CMD_RESTART;
                    }
                    if asked + 1 == len {
                        cmd |= CMD_STOP;
                    }
                    self.wr(IC_DATA_CMD, cmd);
                    asked += 1;
                }
            }
            while self.rd(IC_RXFLR) > 0 && out.len() < len {
                out.push(self.rd(IC_DATA_CMD) as u8);
            }
        }
        // A write without a read: the bytes are still in the controller's queue. Wait until they leave and
        // see whether the device accepted them.
        if len == 0 {
            while self.rd(IC_STATUS) & (STATUS_ACTIVITY | STATUS_TX_EMPTY) != STATUS_TX_EMPTY {
                if self.rd(IC_RAW_INTR) & RAW_TX_ABORT != 0 || crate::hwprobe::expired(end) {
                    self.rd(IC_CLR_TX_ABRT);
                    return None;
                }
            }
            if self.rd(IC_RAW_INTR) & RAW_TX_ABORT != 0 {
                self.rd(IC_CLR_TX_ABRT);
                return None;
            }
        }
        Some(out)
    }

    /// Which addresses answer a read.
    pub fn scan(&self) -> Vec<u8> {
        (0x08..0x78).filter(|&a| self.transfer(a, &[], 1).is_some()).collect()
    }
}

/// The descriptor of a HID over I2C device.
pub struct HidDevice {
    /// The register it was read from.
    pub register: u16,
    pub report_length: usize,
    pub report_register: u16,
    pub input_register: u16,
    pub max_input: usize,
    /// Where to write commands (power, reports) and their data.
    pub command_register: u16,
    pub data_register: u16,
    pub vendor: u16,
    pub product: u16,
}

/// The I2C controllers of Intel chipsets (Skylake…Tiger Lake): SPI and UART have the same class,
/// so they are selected by device ids.
const I2C_IDS: [u16; 34] = [
    0x9d60, 0x9d61, 0x9d62, 0x9d63, 0x9d64, 0x9d65, // Sunrise Point LP
    0xa160, 0xa161, 0xa162, 0xa163, 0xa166, // Sunrise Point H
    0x9de8, 0x9de9, 0x9dea, 0x9deb, 0x9dc5, 0x9dc6, // Cannon Lake LP
    0x02e8, 0x02e9, 0x02ea, 0x02eb, 0x02c5, 0x02c6, // Comet Lake LP
    0x06e8, 0x06e9, 0x06ea, 0x06eb, // Comet Lake H
    0xa0c5, 0xa0c6, 0xa0d8, 0xa0d9, 0xa0e8, 0xa0e9, 0xa0ea, // Tiger Lake LP
];

/// Where devices usually keep the descriptor, without parsing the ACPI tables.
const HID_REGISTERS: [u16; 5] = [0x0020, 0x0001, 0x0030, 0x0010, 0x0000];

/// Asks a device whether it is HID.
pub fn hid_device(bus: &Bus, addr: u8) -> Option<HidDevice> {
    HID_REGISTERS.iter().find_map(|&register| {
        let d = bus.transfer(addr, &register.to_le_bytes(), 30)?;
        let word = |at: usize| u16::from_le_bytes([d[at], d[at + 1]]);
        (word(0) == 30 && word(2) == 0x0100).then(|| HidDevice {
            register,
            report_length: word(4) as usize,
            report_register: word(6),
            input_register: word(8),
            max_input: word(10) as usize,
            command_register: word(16),
            data_register: word(18),
            vendor: word(20),
            product: word(22),
        })
    })
}

/// All I2C controllers ready for exchange: those the firmware gave no address
/// get one. `lines` gets what went wrong.
pub fn controllers(devices: &[Device], lines: &mut Vec<String>) -> Vec<(String, Bus)> {
    let mut out = Vec::new();
    let mut free = None;
    for d in devices.iter().filter(|d| d.vendor == 0x8086 && I2C_IDS.contains(&d.device)) {
        let name = format!("{:02x}:{:02x}.{} {:04x}", d.bus, d.slot, d.func, d.device);
        let placed = d.bar(0) == 0;
        if placed {
            if free.is_none() {
                free = crate::pci::free_mmio(devices);
            }
            match Bus::place(d, &mut free) {
                Ok(at) => lines.push(format!("i2c {name}: firmware gave no address, placed at {at:#x}")),
                Err(e) => {
                    lines.push(format!("i2c {name}: firmware gave no address, {e}"));
                    continue;
                }
            }
        }
        let Some(bus) = Bus::open(d) else {
            lines.push(format!("i2c {name}: not usable, BAR {:#x}, type {:#x}", d.bar(0), Bus::comp_type(d)));
            if placed {
                d.set_bar(0, 0);
            }
            continue;
        };
        out.push((name, bus));
    }
    out
}

/// The probe for the report: lines about the buses and the report descriptors of the HID devices found.
pub fn survey(devices: &[Device]) -> (Vec<String>, Vec<u8>) {
    let (mut lines, mut report) = (Vec::new(), Vec::new());
    for (name, bus) in controllers(devices, &mut lines) {
        let found = bus.scan();
        lines.push(format!("i2c {name}: answers at {found:02x?}"));
        for addr in found {
            let Some(hid) = hid_device(&bus, addr) else { continue };
            lines.push(format!(
                "i2c {name}: {addr:#04x} is HID {:04x}:{:04x}, descriptor at {:#06x}, report map {} bytes at {:#06x}, input {:#06x}, up to {} bytes",
                hid.vendor, hid.product, hid.register, hid.report_length, hid.report_register, hid.input_register, hid.max_input
            ));
            if let Some(map) = bus.transfer(addr, &hid.report_register.to_le_bytes(), hid.report_length.min(4096)) {
                report.extend_from_slice(&map);
            }
        }
    }
    if lines.is_empty() {
        lines.push("i2c: no Intel controllers".into());
    }
    (lines, report)
}
