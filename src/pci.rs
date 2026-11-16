//! The PCI bus: enumerating devices through the 0xCF8/0xCFC configuration ports.

use crate::arch::{inl, outl};
use alloc::vec::Vec;

#[derive(Clone, Copy)]
pub struct Device {
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
}

fn address(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    0x8000_0000 | (bus as u32) << 16 | (slot as u32) << 11 | (func as u32) << 8 | (offset as u32 & 0xFC)
}

/// The address in 0xCF8 and the data in 0xCFC are a pair: another CPU (or an interrupt) in between
/// would replace the address.
static CONFIG: crate::sync::IrqLock<()> = crate::sync::IrqLock::new(());

pub fn read32(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    let _guard = CONFIG.lock();
    outl(0xCF8, address(bus, slot, func, offset));
    inl(0xCFC)
}

pub fn write32(bus: u8, slot: u8, func: u8, offset: u8, value: u32) {
    let _guard = CONFIG.lock();
    outl(0xCF8, address(bus, slot, func, offset));
    outl(0xCFC, value);
}

impl Device {
    pub fn read(&self, offset: u8) -> u32 {
        read32(self.bus, self.slot, self.func, offset)
    }

    pub fn write(&self, offset: u8, value: u32) {
        write32(self.bus, self.slot, self.func, offset, value)
    }

    /// The memory address from BAR `n` (for a 64-bit BAR it also takes the upper half).
    pub fn bar(&self, n: u8) -> u64 {
        let lo = self.read(0x10 + n * 4);
        if lo & 1 != 0 {
            return (lo & !3) as u64; // I/O ports
        }
        let mut addr = (lo & !0xF) as u64;
        if (lo >> 1) & 3 == 2 {
            addr |= (self.read(0x14 + n * 4) as u64) << 32;
        }
        addr
    }

    /// The address of BAR `n` mapped into kernel memory (for MMIO registers).
    pub fn mmio(&self, n: u8, len: u64) -> usize {
        let addr = self.bar(n);
        crate::paging::map_mmio(addr, len);
        addr as usize
    }

    /// Allows the device to access memory (DMA) and to answer MMIO.
    pub fn enable_bus_master(&self) {
        let cmd = self.read(0x04);
        self.write(0x04, cmd | 0b110);
    }

    /// The offset of the capability with code `id` in the capability list (0 — none).
    #[allow(dead_code)]
    pub fn capability(&self, id: u8) -> u8 {
        // Status bit 4 (0x06) — whether there is a capability list.
        if self.read(0x04) >> 16 & 0x10 == 0 {
            return 0;
        }
        let mut at = (self.read(0x34) & 0xFC) as u8;
        for _ in 0..48 {
            if at == 0 {
                break;
            }
            let head = self.read(at);
            if head as u8 == id {
                return at;
            }
            at = (head >> 8 & 0xFC) as u8;
        }
        0
    }

    /// A function reset (PCIe Function Level Reset), if the device supports it. Needed
    /// when the device is left in an unknown state (for example, after a warm
    /// watchdog reboot): otherwise the very first access to its memory hangs the bus.
    /// FLR clears the configuration, so the BAR address and the command are restored.
    /// Returns false if FLR is not supported (calling it is useless).
    #[allow(dead_code)]
    pub fn function_level_reset(&self, delay_ms: impl Fn(u32)) -> bool {
        let cap = self.capability(0x10); // PCI Express
        if cap == 0 {
            return false;
        }
        // Device Capabilities (cap+0x04), bit 28 — FLR support.
        if self.read(cap + 0x04) & (1 << 28) == 0 {
            return false;
        }
        let bars: [u32; 6] = core::array::from_fn(|n| self.read(0x10 + n as u8 * 4));
        let command = self.read(0x04);
        // Device Control (cap+0x08), bit 15 — Initiate Function Level Reset.
        let ctrl = self.read(cap + 0x08);
        self.write(cap + 0x08, ctrl | 1 << 15);
        delay_ms(100);
        for (n, &v) in bars.iter().enumerate() {
            self.write(0x10 + n as u8 * 4, v);
        }
        self.write(0x04, command);
        true
    }

    /// The bus number behind this bridge (secondary bus), or None — this is not a bridge.
    pub fn secondary_bus(&self) -> Option<u8> {
        (self.header_type() == 1).then(|| (self.read(0x18) >> 8) as u8)
    }

    /// A reset of the bus behind the bridge (Secondary Bus Reset): forcefully reinitializes everything
    /// on it. The only way to revive a device that has no
    /// FLR and hung. Call only on the bridge of the right device (check
    /// `secondary_bus`), otherwise you reset someone else's card. The bridge does not keep the children's configuration —
    /// the device driver restores it.
    #[allow(dead_code)]
    pub fn secondary_bus_reset(&self, delay_ms: impl Fn(u32)) {
        // Bridge Control (0x3E, in the upper word of register 0x3C), bit 6 — Secondary Bus Reset.
        let reg = self.read(0x3C);
        self.write(0x3C, reg | 1 << 22);
        delay_ms(2);
        self.write(0x3C, reg & !(1 << 22));
        // The link trains again: the children need time until they answer again.
        delay_ms(100);
    }
}

/// All functions of all devices on buses 0..=255 (empty slots are skipped).
pub fn scan() -> Vec<Device> {
    let mut out = Vec::new();
    for bus in 0..=255u8 {
        for slot in 0..32u8 {
            let id = read32(bus, slot, 0, 0);
            if id & 0xFFFF == 0xFFFF {
                continue;
            }
            let multi = read32(bus, slot, 0, 0x0C) >> 16 & 0x80 != 0;
            for func in 0..if multi { 8 } else { 1 } {
                let id = read32(bus, slot, func, 0);
                if id & 0xFFFF == 0xFFFF {
                    continue;
                }
                let class = read32(bus, slot, func, 0x08);
                out.push(Device {
                    bus,
                    slot,
                    func,
                    vendor: id as u16,
                    device: (id >> 16) as u16,
                    class: (class >> 24) as u8,
                    subclass: (class >> 16) as u8,
                });
            }
        }
    }
    out
}

impl Device {
    /// The header type: 0 — an ordinary device, 1 — a bridge.
    pub fn header_type(&self) -> u8 {
        (self.read(0x0C) >> 16 & 0x7F) as u8
    }

    /// The size of memory BAR `n` asks for. While measuring, the address is wrong, so
    /// it may only be called for a device nobody is using.
    pub fn bar_size(&self, n: u8) -> u64 {
        let at = 0x10 + n * 4;
        let lo = self.read(at);
        if lo & 1 != 0 {
            return 0;
        }
        let wide = (lo >> 1) & 3 == 2;
        let hi = if wide { self.read(at + 4) } else { 0 };
        let command = self.read(0x04);
        self.write(0x04, command & !0b11);
        self.write(at, 0xFFFF_FFFF);
        let mut mask = (self.read(at) & !0xF) as u64;
        if wide {
            self.write(at + 4, 0xFFFF_FFFF);
            mask |= (self.read(at + 4) as u64) << 32;
            self.write(at + 4, hi);
        } else {
            mask |= 0xFFFF_FFFF_0000_0000;
        }
        self.write(at, lo);
        self.write(0x04, command);
        if mask == 0xFFFF_FFFF_0000_0000 || mask == 0 { 0 } else { (!mask).wrapping_add(1) }
    }

    /// Puts BAR `n` at address `addr` (for a 64-bit one — both halves).
    pub fn set_bar(&self, n: u8, addr: u64) {
        let at = 0x10 + n * 4;
        let lo = self.read(at);
        self.write(at, (addr as u32 & !0xF) | (lo & 0xF));
        if (lo >> 1) & 3 == 2 {
            self.write(at + 4, (addr >> 32) as u32);
        }
    }

    /// Memory addresses taken by the device's BARs (starts only: the sizes of working
    /// devices must not be measured).
    fn bar_bases(&self) -> Vec<u64> {
        let count = match self.header_type() {
            0 => 6,
            1 => 2,
            _ => 0,
        };
        let mut out = Vec::new();
        let mut n = 0;
        while n < count {
            let lo = self.read(0x10 + n * 4);
            let wide = lo & 1 == 0 && (lo >> 1) & 3 == 2;
            if lo & 1 == 0 && self.bar(n) != 0 {
                out.push(self.bar(n));
            }
            n += if wide { 2 } else { 1 };
        }
        out
    }

    /// The bridge's memory windows above 4 GiB: (start, end).
    fn bridge_window(&self) -> Option<(u64, u64)> {
        if self.header_type() != 1 {
            return None;
        }
        let range = self.read(0x24);
        let (base, limit) = (range & 0xFFF0, range >> 16 & 0xFFF0);
        if range & 0xF != 1 {
            return None;
        }
        let base = (base as u64) << 16 | (self.read(0x28) as u64) << 32;
        let end = ((limit as u64) << 16 | 0xF_FFFF | (self.read(0x2C) as u64) << 32) + 1;
        (end > base).then_some((base, end))
    }
}

/// The highest memory address below 4 GiB taken by any device's BAR. Needed
/// to give an address to a device the firmware did not give one (for example, the Wi-Fi card on
/// the BRIX): our own window goes strictly above — so it does not overlap any device.
pub fn top_mem_bar_below_4g(devices: &[Device]) -> u64 {
    devices
        .iter()
        .flat_map(|d| d.bar_bases())
        .filter(|&b| b != 0 && b < 1 << 32)
        .max()
        .unwrap_or(0)
}

const GIB4: u64 = 1 << 32;
/// At most how much to consider taken past the last known BAR start.
const UNKNOWN_SIZE: u64 = 256 << 20;

/// A free address above 4 GiB where a BAR the firmware gave no address can be placed.
/// We take the place right after everything that is already taken. The size of a foreign BAR is learned from the alignment of its
/// start and from the neighbour above, but no more than `UNKNOWN_SIZE`.
pub fn free_mmio(devices: &[Device]) -> Option<u64> {
    let mut bases: Vec<u64> = devices.iter().flat_map(|d| d.bar_bases()).filter(|&b| b >= GIB4).collect();
    bases.sort_unstable();
    let mut top = 0;
    for (i, &base) in bases.iter().enumerate() {
        let aligned = 1u64 << base.trailing_zeros();
        let mut end = base + aligned.min(UNKNOWN_SIZE);
        if let Some(&next) = bases.get(i + 1) {
            end = end.min(next.max(base + 1));
        }
        top = top.max(end);
    }
    for (_, end) in devices.iter().filter_map(|d| d.bridge_window()) {
        top = top.max(end);
    }
    if top == 0 {
        return None;
    }
    let top = (top + 0xF_FFFF) & !0xF_FFFF;
    // No further than the CPU can address.
    let bits = core::arch::x86_64::__cpuid(0x8000_0008).eax & 0xFF;
    (top < 1u64 << bits.min(52)).then_some(top)
}
