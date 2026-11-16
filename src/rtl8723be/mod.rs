//! Realtek RTL8723BE Wi-Fi (PCIe) — the Wi-Fi + Bluetooth module in the Gigabyte BRIX.
//!
//! The chip does not run the air by itself: the firmware of its 8051 microcontroller helps (calibrations,
//! transmit rate), but the MAC, BB and RF setup and frame receive and transmit
//! are done by the driver. The order of steps and the register value tables come from FreeBSD's rtwn
//! driver (BSD-2-Clause license, RTL8723B — Ahmad Khalifa), the PCIe DMA layout —
//! from OpenBSD's rtwn (ISC). Linux is used for cross-checking only.
//!
//! Stages: power and EFUSE (MAC address, transmitter power); the 8051 firmware; MAC, DMA,
//! BB and RF — frame receive and network discovery by beacons.

mod connect;
mod diag;
mod init;
mod iqk;
mod rx;
mod tables;

use crate::hwprobe::{deadline, expired};
use crate::sync::IrqLock;
use crate::wifi::{self, Network};
use crate::{interrupts, pci, task};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

const DEVICE: (u16, u16) = (0x10EC, 0xB723);
/// The microcontroller firmware (linux-firmware rtlwifi/rtl8723befw_36.bin, Realtek license).
const FIRMWARE: &str = "/firmware/rtw8723b.fw";
/// How long after system start to bring the card up.
const START_DELAY_MS: u64 = 8_000;
/// TSC cycles per microsecond: with a margin, the N2807 runs slower.
const CYCLES_PER_US: u64 = 3000;

// PCI configuration.
const PCI_COMMAND: u8 = 0x04;
const MEMORY_SPACE: u32 = 1 << 1;
const BUS_MASTER: u32 = 1 << 2;
const INTX_OFF: u32 = 1 << 10;
const REGS_BAR: u8 = 2;
/// The size of the card's register window (16 KiB; it used to be read from the BAR, but after a bus reset
/// sizing the BAR is dangerous).
const REGS_SIZE: usize = 0x4000;
/// The "card bring-up started" marker: present at startup — we hung last time, leave the card alone.
const HUNG_MARKER: &str = "/system/wifi.hung";

// MAC registers (common to the family, names as in rtwn).
const SYS_ISO_CTRL: usize = 0x000;
const SYS_FUNC_EN: usize = 0x002;
const APS_FSMCO: usize = 0x004;
const SYS_CLKR: usize = 0x008;
const AFE_MISC: usize = 0x010;
const RSV_CTRL: usize = 0x01C;
const LDOA15_CTRL: usize = 0x020;
const EFUSE_CTRL: usize = 0x030;
const EFUSE_TEST: usize = 0x034;
const PWR_DATA: usize = 0x038;
const GPIO_INTM: usize = 0x048;
const LEDCFG0: usize = 0x04C;
const HSIMR: usize = 0x058;
const BB_PAD_CTRL: usize = 0x064;
const MULTI_FUNC_CTRL: usize = 0x068;
/// PCIe: while powering on, "sleeping" on the bus is forbidden (as in Realtek's tables).
const PCIE_SUSPEND: usize = 0x075;
const MCUFWDL: usize = 0x080;
const EFUSE_ACCESS: usize = 0x0CF;
const SYS_CFG: usize = 0x0F0;
/// Card interrupts: mask and status (main and extended).
const HIMR: usize = 0x0B0;
const HISR: usize = 0x0B4;
const HIMRE: usize = 0x0B8;
const HISRE: usize = 0x0BC;
/// A frame received, receive descriptors ran out, frames left the transmit queues.
const HIMR_RX_TX: u32 = 0xFF;
/// Receive FIFO overflow.
const HIMRE_RX_OVERFLOW: u32 = 1 << 8;
const CR: usize = 0x100;
pub(self) const PCIE_CTRL_REG: usize = 0x300;
const TXPAUSE: usize = 0x522;
const CR_MACRXEN: u8 = 0x80;
/// Transmit queues busy (0x5F8–0x5FB): zero — nothing is being transmitted.
const TX_BUSY: usize = 0x5F8;
const RF_CTRL: usize = 0x01F;
/// Receive DMA control: bit 2 — pause, bit 1 — idle, bit 3 — has something to deliver.
const RXDMA_CONTROL: usize = 0x286;
const RXDMA_STOP: u8 = 0x04;
const RXDMA_IDLE: u8 = 0x02;
/// The PCIe interface DMA debug port (see `pcie_dma_hung`), the high byte.
const DMA_DEBUG: usize = 0x350;
const DMA_DEBUG_ON: u8 = 0x04;
const DMA_RX_HANG: u8 = 0x02;
const DMA_TX_HANG: u8 = 0x01;
const GNT_BT: usize = 0x764;
const ANT_SEL: usize = 0x948;

// Bits.
const ISO_IP2MAC: u8 = 0x20;
const FUNC_BBRSTB: u16 = 0x0001;
const FUNC_BB_GLB_RST: u16 = 0x0002;
const FUNC_ELDR: u16 = 0x1000;
const CLKR_ANA8M: u16 = 0x0002;
const CLKR_LOADER_EN: u16 = 0x0020;
const FSMCO_APFM_ONMAC: u32 = 0x0000_0100;
const FSMCO_APFM_OFF: u32 = 0x0000_0200;
const FSMCO_APFM_RSM: u32 = 0x0000_0400;
const FSMCO_AFSM_HSUS: u32 = 0x0000_0800;
const FSMCO_AFSM_PCIE: u32 = 0x0000_1000;
const FSMCO_APDM_HPDN: u32 = 0x0000_8000;
const FSMCO_RDY_MACON: u32 = 0x0001_0000;
const FSMCO_SUS_HOST: u32 = 0x0002_0000;
const LDOA15_EN: u8 = 0x01;
const EFUSE_VALID: u32 = 0x8000_0000;
const EFUSE_ADDR_SHIFT: u32 = 8;
const EFUSE_ADDR_MASK: u32 = 0x3FF << 8;
const EFUSE_ACCESS_ON: u8 = 0x69;
const EFUSE_ACCESS_OFF: u8 = 0x00;
const EFUSE_TEST_SEL_MASK: u32 = 0x300;
const EFUSE_TEST_PGMEN: u16 = 0x800;
const CR_HCI_TXDMA: u16 = 0x0001;
const CR_HCI_RXDMA: u16 = 0x0002;
const CR_TXDMA: u16 = 0x0004;
const CR_RXDMA: u16 = 0x0008;
const CR_PROTOCOL: u16 = 0x0010;
const CR_SCHEDULE: u16 = 0x0020;
const CR_ENSEC: u16 = 0x0200;
const CR_CALTMR: u16 = 0x0400;
const PWR_DATA_RFE_CTRL_EN: u16 = 0x800;
const LEDCFG0_DPDT_SEL_EN: u32 = 0x80_0000;
const BB_PAD_DPDT_SEL_DATA: u8 = 0x1;
const ANT_SEL_S0: u32 = 0x000;
const SYS_CFG_VENDOR_UMC: u32 = 0x0008_0000;
const SYS_CFG_CHIP_VER_SHIFT: u32 = 12;
const SYS_CFG_TRP_BT_EN: u32 = 0x0100_0000;

// Loading the 8051 firmware.
const HMETFR: usize = 0x1CC;
const FUNC_CPUEN: u32 = 0x0400;
const MCUFWDL_EN: u32 = 0x01;
const MCUFWDL_RDY: u32 = 0x02;
const MCUFWDL_CHKSUM_RPT: u32 = 0x04;
const MCUFWDL_WINTINI_RDY: u32 = 0x40;
const MCUFWDL_RAM_DL_SEL: u32 = 0x80;
const MCUFWDL_PAGE_SHIFT: u32 = 16;
const MCUFWDL_PAGE_MASK: u32 = 0x7 << 16;
const MCUFWDL_ROM_DLEN: u32 = 0x0008_0000;
const FW_WINDOW: usize = 0x1000;
const FW_PAGE: usize = 4096;
const FW_HEADER: usize = 32;
/// The 8723B firmware header signature (without the low four bits).
const FW_SIGNATURE: u16 = 0x530;

// EFUSE: 512 bytes of the card, the PCIe MAC address is at offset 0xD0.
const EFUSE_MAP_LEN: usize = 512;
const EFUSE_MAX_LEN: usize = 512;
const ROM_MAC: usize = 0xD0;
const ROM_ID: u16 = 0x8129;
const ROM_CRYSTAL_CAP: usize = 0xB9;
const ROM_THERMAL: usize = 0xBA;
const ROM_RF_BT_OPT: usize = 0xC3;
const ROM_TX_POWER: usize = 0x10;

/// The card registers.
#[derive(Clone, Copy)]
struct Chip {
    mmio: usize,
}

impl Chip {
    fn r8(&self, reg: usize) -> u8 {
        unsafe { ((self.mmio + reg) as *const u8).read_volatile() }
    }
    fn r16(&self, reg: usize) -> u16 {
        unsafe { ((self.mmio + reg) as *const u16).read_volatile() }
    }
    fn r32(&self, reg: usize) -> u32 {
        unsafe { ((self.mmio + reg) as *const u32).read_volatile() }
    }
    fn w8(&self, reg: usize, v: u8) {
        unsafe { ((self.mmio + reg) as *mut u8).write_volatile(v) }
    }
    fn w16(&self, reg: usize, v: u16) {
        unsafe { ((self.mmio + reg) as *mut u16).write_volatile(v) }
    }
    fn w32(&self, reg: usize, v: u32) {
        unsafe { ((self.mmio + reg) as *mut u32).write_volatile(v) }
    }
    fn set8(&self, reg: usize, clear: u8, set: u8) {
        self.w8(reg, self.r8(reg) & !clear | set);
    }
    fn set16(&self, reg: usize, clear: u16, set: u16) {
        self.w16(reg, self.r16(reg) & !clear | set);
    }
    fn set32(&self, reg: usize, clear: u32, set: u32) {
        self.w32(reg, self.r32(reg) & !clear | set);
    }
    /// Bits of a 32-bit register, but a write of one of its bytes (`shift` is the byte number).
    fn set8_of(&self, reg: usize, clear: u32, set: u32, shift: usize) {
        self.set8(reg + shift, (clear >> (8 * shift)) as u8, (set >> (8 * shift)) as u8);
    }

    fn wait16(&self, reg: usize, mask: u16, value: u16, us: u64) -> bool {
        let end = deadline(us * CYCLES_PER_US);
        loop {
            if self.r16(reg) & mask == value {
                return true;
            }
            if expired(end) {
                return false;
            }
        }
    }

    fn wait32(&self, reg: usize, mask: u32, value: u32, us: u64) -> bool {
        let end = deadline(us * CYCLES_PER_US);
        loop {
            if self.r32(reg) & mask == value {
                return true;
            }
            if expired(end) {
                return false;
            }
        }
    }

    /// Power of the WLAN part: from "card off" to "running" (like r23bu_power_on, plus
    /// the PCIe step from Realtek's tables).
    fn power_on(&self) -> Result<(), String> {
        // Unlock the power and clock registers; do not go to "card off" by itself.
        self.w8(RSV_CTRL, 0);
        self.set8(APS_FSMCO + 1, 0x80, 0);
        // Clear "sleep" and "power down".
        self.set8_of(APS_FSMCO, FSMCO_AFSM_HSUS | FSMCO_APDM_HPDN, 0, 1);
        // GPIO9 does not wake.
        self.set8(GPIO_INTM + 2, 0x01, 0);
        self.set8_of(APS_FSMCO, FSMCO_AFSM_HSUS | FSMCO_AFSM_PCIE, 0, 1);
        // The 1.2 V LDO for the analog part; the BT_GPS_SEL pins — off.
        self.set8(LDOA15_CTRL, 0, LDOA15_EN);
        self.set8(0x067, 0x10, 0);
        pause_us(1000);
        // Analog blocks to the digital ones (remove isolation).
        self.set8(SYS_ISO_CTRL, ISO_IP2MAC, 0);
        self.set8_of(APS_FSMCO, FSMCO_APFM_RSM | FSMCO_AFSM_HSUS | FSMCO_AFSM_PCIE, 0, 1);
        // PCIe: do not sleep while waiting for power to be ready.
        self.set8(PCIE_SUSPEND, 0, 0x01);
        let ready = self.wait32(APS_FSMCO, FSMCO_SUS_HOST, FSMCO_SUS_HOST, 50_000);
        self.set8(PCIE_SUSPEND, 0x01, 0);
        if !ready {
            return Err(format!("power did not come up, APS_FSMCO {:08x}", self.r32(APS_FSMCO)));
        }
        // Release the WLON reset, turn off HWPDN and "sleep".
        self.set8_of(APS_FSMCO, 0, FSMCO_RDY_MACON, 2);
        self.set8_of(APS_FSMCO, FSMCO_APDM_HPDN, 0, 1);
        self.set8_of(APS_FSMCO, FSMCO_AFSM_HSUS | FSMCO_AFSM_PCIE, 0, 1);
        // Enable the MAC and wait until the bit clears by itself.
        self.set8_of(APS_FSMCO, 0, FSMCO_APFM_ONMAC, 1);
        if !self.wait16(APS_FSMCO, FSMCO_APFM_ONMAC as u16, 0, 50_000) {
            return Err(format!("MAC did not turn on, APS_FSMCO {:08x}", self.r32(APS_FSMCO)));
        }
        self.set8(AFE_MISC, 0, 0x40);
        // GPIO9: interrupt on the falling edge, input, pull-up (as Realtek sets it).
        self.set8(GPIO_INTM + 1, 0, 0x02);
        self.set8(0x063, 0, 0x02);
        self.set8(0x062, 0x02, 0);
        self.set8(HSIMR, 0, 0x01);
        self.set8(HSIMR + 2, 0, 0x02);
        self.set8(MULTI_FUNC_CTRL, 0, 0x08);
        self.set8_of(MULTI_FUNC_CTRL, 0, 0x4000, 1);
        // Receive DMA is paused until the card has been given rings and allowed to write to
        // memory (init_pcie clears this): firmware reports come through receive, and a DMA attempt
        // without rings may hang it (see `pcie_dma_hung`).
        self.set8(RXDMA_CONTROL, 0, RXDMA_STOP);
        // MAC blocks: DMA, scheduler, protocol, security, calibration timer.
        self.w8(CR, 0x00);
        self.set16(CR, 0, CR_HCI_TXDMA | CR_TXDMA | CR_HCI_RXDMA | CR_RXDMA | CR_PROTOCOL | CR_SCHEDULE | CR_ENSEC | CR_CALTMR);
        // Coexistence with Bluetooth: the antenna belongs to Wi-Fi.
        self.w8(BB_PAD_CTRL + 3, 0x20);
        self.set16(SYS_FUNC_EN, 0, FUNC_BBRSTB | FUNC_BB_GLB_RST);
        self.w8(GNT_BT + 1, 0x18);
        self.w8(0x76E, 0x04);
        self.w32(ANT_SEL, ANT_SEL_S0);
        self.set16(PWR_DATA, 0, PWR_DATA_RFE_CTRL_EN);
        self.set32(LEDCFG0, 0, LEDCFG0_DPDT_SEL_EN);
        self.set8(BB_PAD_CTRL, BB_PAD_DPDT_SEL_DATA, 0);
        Ok(())
    }

    /// Ask the 8051 to restart by itself; if that does not happen within 5 ms — stop it by force.
    fn stop_mcu(&self) {
        self.w8(HMETFR + 3, 0x20);
        if !self.wait16(SYS_FUNC_EN, FUNC_CPUEN as u16, 0, 5000) {
            self.set8_of(SYS_FUNC_EN as usize, FUNC_CPUEN, 0, 1);
        }
    }

    /// Turning the card off, as Realtek does on disable: stop DMA and transmit,
    /// turn off BB, reset the 8051 and the MAC, put the card into "off". Only
    /// short in-place waits: it is also called on reboot.
    fn power_off(&self) {
        self.w8(PCIE_CTRL_REG + 1, 0xFF);
        self.w8(TXPAUSE, 0xFF);
        self.wait32(TX_BUSY, u32::MAX, 0, 5000);
        self.set8(SYS_FUNC_EN, FUNC_BBRSTB as u8, 0);
        pause_us(1);
        self.set8(SYS_FUNC_EN, FUNC_BB_GLB_RST as u8, 0);
        self.w8(CR, (CR_HCI_TXDMA | CR_HCI_RXDMA) as u8);
        self.set8(CR + 1, 0x02, 0);
        self.w8(0x093, 0x00);
        self.set8(0x553, 0, 0x20);
        // 8051: reset together with the I/O wrapper, download flags to zero.
        self.set8(RSV_CTRL + 1, 0x01, 0);
        self.set8_of(SYS_FUNC_EN, FUNC_CPUEN, 0, 1);
        pause_us(50);
        self.set8(RSV_CTRL + 1, 0, 0x01);
        self.w8(MCUFWDL, 0);
        // Radio off, MAC — turned off through the state machine.
        self.w8(RF_CTRL, 0);
        self.set8(LEDCFG0 + 3, 0x01, 0);
        self.set8(GPIO_INTM + 1, 0x02, 0);
        self.set8_of(APS_FSMCO, 0, FSMCO_RDY_MACON, 2);
        self.set8_of(APS_FSMCO, 0, FSMCO_APFM_OFF, 1);
        self.wait16(APS_FSMCO, FSMCO_APFM_OFF as u16, 0, 50_000);
        self.set8(AFE_MISC, 0x40, 0);
        self.set8_of(APS_FSMCO, 0, FSMCO_APFM_RSM, 1);
        self.set8(RSV_CTRL + 1, 0x01, 0);
        self.set8(RSV_CTRL + 1, 0, 0x01);
        self.w8(RSV_CTRL, 0x0E);
    }

    /// Whether the PCIe interface DMA is hung. The sign is debug port 0x350: bit 26
    /// enables it, bit 25 — receive is hung, bit 24 — transmit. A hung receive looks like
    /// a deaf radio: frames pile up in the card's FIFO, the FIFO overflows (HISRE bit 8), and
    /// nothing reaches the ring, although transmit works. On the BRIX (November 2026) this
    /// survived warm reboots, our MAC shutdown, a bus reset behind the bridge, a reset of the
    /// DMA block through 0x003 and turning off ASPM; only a full power removal cleared it.
    fn pcie_dma_hung(&self) -> bool {
        let v = self.r8(DMA_DEBUG + 3);
        if v & DMA_DEBUG_ON == 0 {
            self.w8(DMA_DEBUG + 3, v | DMA_DEBUG_ON);
            pause_us(2000);
        }
        self.r8(DMA_DEBUG + 3) & (DMA_RX_HANG | DMA_TX_HANG) != 0
    }

    fn download(&self, on: bool) {
        if !on {
            self.set8(MCUFWDL, MCUFWDL_EN as u8, 0);
            return;
        }
        self.set8_of(SYS_FUNC_EN, 0, FUNC_CPUEN, 1);
        for _ in 0..=100 {
            let v = self.r8(MCUFWDL);
            if v & MCUFWDL_EN as u8 != 0 {
                break;
            }
            self.w8(MCUFWDL, v | MCUFWDL_EN as u8);
            pause_us(1000);
        }
        // The 8051 goes into reset (stop booting from ROM).
        self.set8_of(MCUFWDL, MCUFWDL_ROM_DLEN, 0, 2);
    }

    /// Loads the 8051 firmware in 4 KB pages through the 0x1000 window and starts it.
    /// Returns the version from the header.
    fn load_firmware(&self, fw: &[u8]) -> Result<u16, String> {
        let signature = u16::from_le_bytes([fw[0], fw[1]]);
        let (version, image) = if signature >> 4 == FW_SIGNATURE && fw.len() > FW_HEADER {
            (u16::from_le_bytes([fw[4], fw[5]]), &fw[FW_HEADER..])
        } else {
            (0, fw)
        };
        if self.r8(MCUFWDL) & MCUFWDL_RAM_DL_SEL as u8 != 0 {
            self.w8(MCUFWDL, 0);
            self.stop_mcu();
        }
        self.download(true);
        let mut loaded = false;
        for _ in 0..3 {
            // Clear the checksum report (the bit is written with a one).
            self.set8(MCUFWDL, 0, MCUFWDL_CHKSUM_RPT as u8);
            for (page, chunk) in image.chunks(FW_PAGE).enumerate() {
                self.set32(MCUFWDL, MCUFWDL_PAGE_MASK, (page as u32) << MCUFWDL_PAGE_SHIFT);
                for (i, &b) in chunk.iter().enumerate() {
                    self.w8(FW_WINDOW + i, b);
                }
            }
            if self.wait32(MCUFWDL, MCUFWDL_CHKSUM_RPT, MCUFWDL_CHKSUM_RPT, 250_000) {
                loaded = true;
                break;
            }
        }
        self.download(false);
        if !loaded {
            return Err(format!("firmware checksum did not come, MCUFWDL {:08x}", self.r32(MCUFWDL)));
        }
        self.set32(MCUFWDL, MCUFWDL_WINTINI_RDY, MCUFWDL_RDY);
        // Restart the 8051 with the new firmware: the MCU I/O wrapper and the processor itself.
        self.set8(RSV_CTRL + 1, 0x01, 0);
        self.set8_of(SYS_FUNC_EN, FUNC_CPUEN, 0, 1);
        self.set8(RSV_CTRL + 1, 0, 0x01);
        self.set8_of(SYS_FUNC_EN, 0, FUNC_CPUEN, 1);
        if !self.wait32(MCUFWDL, MCUFWDL_WINTINI_RDY, MCUFWDL_WINTINI_RDY, 200_000) {
            return Err(format!("firmware did not start, MCUFWDL {:08x}", self.r32(MCUFWDL)));
        }
        Ok(version)
    }

    fn efuse_byte(&self, addr: usize) -> Result<u8, String> {
        for _ in 0..3 {
            let reg = self.r32(EFUSE_CTRL) & !EFUSE_ADDR_MASK & !EFUSE_VALID | (addr as u32) << EFUSE_ADDR_SHIFT;
            self.w32(EFUSE_CTRL, reg);
            if self.wait32(EFUSE_CTRL, EFUSE_VALID, EFUSE_VALID, 10_000) {
                return Ok(self.r32(EFUSE_CTRL) as u8);
            }
        }
        Err(format!("efuse byte {addr:#x} did not come"))
    }

    /// The logical EFUSE map: records of "header, word mask, words".
    fn read_efuse(&self) -> Result<[u8; EFUSE_MAP_LEN], String> {
        // The Wi-Fi bank (not Bluetooth), no programming.
        self.set32(EFUSE_TEST, EFUSE_TEST_SEL_MASK, 0);
        self.set16(EFUSE_TEST, EFUSE_TEST_PGMEN, 0);
        self.w8(EFUSE_ACCESS, EFUSE_ACCESS_ON);
        let func = self.r16(SYS_FUNC_EN);
        if func & FUNC_ELDR == 0 {
            self.w16(SYS_FUNC_EN, func | FUNC_ELDR);
        }
        let clk = self.r16(SYS_CLKR);
        if clk & (CLKR_LOADER_EN | CLKR_ANA8M) != CLKR_LOADER_EN | CLKR_ANA8M {
            self.w16(SYS_CLKR, clk | CLKR_LOADER_EN | CLKR_ANA8M);
        }
        let mut map = [0xFFu8; EFUSE_MAP_LEN];
        let result = (|| -> Result<usize, String> {
            let mut at = 0;
            let next = |at: &mut usize| -> Result<u8, String> {
                if *at >= EFUSE_MAX_LEN {
                    return Err("efuse ran past its end".into());
                }
                let b = self.efuse_byte(*at)?;
                *at += 1;
                Ok(b)
            };
            let mut head = next(&mut at)?;
            while head != 0xFF {
                // A long header: the offset did not fit in 4 bits.
                let (offset, mask);
                if head & 0x1F == 0x0F {
                    let second = next(&mut at)?;
                    if second & 0x0F == 0x0F {
                        head = next(&mut at)?;
                        continue;
                    }
                    offset = ((second & 0xF0) >> 1 | head >> 5) as usize;
                    mask = second & 0x0F;
                } else {
                    offset = (head >> 4) as usize;
                    mask = head & 0x0F;
                }
                for word in 0..4 {
                    if mask & (1 << word) != 0 {
                        continue;
                    }
                    let addr = offset * 8 + word * 2;
                    let (lo, hi) = (next(&mut at)?, next(&mut at)?);
                    if addr + 1 < EFUSE_MAP_LEN {
                        map[addr] = lo;
                        map[addr + 1] = hi;
                    }
                }
                head = next(&mut at)?;
            }
            Ok(at)
        })();
        self.w8(EFUSE_ACCESS, EFUSE_ACCESS_OFF);
        let used = result?;
        crate::kprintln!("rtl8723be: efuse uses {used} of {EFUSE_MAX_LEN} bytes");
        Ok(map)
    }
}

fn pause_us(us: u64) {
    let end = deadline(us * CYCLES_PER_US);
    while !expired(end) {
        core::hint::spin_loop();
    }
}

/// A long pause: the CPU is given to other threads.
fn sleep_ms(ms: u64) {
    let now = interrupts::ticks();
    task::sleep_until(now + (ms * interrupts::HZ).div_ceil(1000) + 1);
}

/// While hunting for the cause of a hang: a mark in the log and a pause longer than the
/// trial kernel's log write period (1 s) — so the last mark before the hang
/// surely reaches the disk. Turned off by `DIAG`.
const DIAG: bool = false;

fn mark(line: &str) {
    crate::kprintln!("rtl8723be: step: {line}");
    if DIAG {
        sleep_ms(1200);
    }
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

/// What we learned about the card: for the log and hwprobe.
static REPORT: IrqLock<Vec<String>> = IrqLock::new(Vec::new());

fn note(line: String) {
    crate::kprintln!("rtl8723be: {line}");
    REPORT.lock().push(line);
}

pub fn report() -> Vec<String> {
    REPORT.lock().clone()
}

/// The card is present in the computer (its thread is running).
static EXPECTED: AtomicBool = AtomicBool::new(false);

pub fn expected() -> bool {
    EXPECTED.load(Ordering::Relaxed)
}

/// Finds the card and brings it up in its own thread.
pub fn start(devices: &[pci::Device]) {
    let Some(d) = devices.iter().find(|d| (d.vendor, d.device) == DEVICE).copied() else { return };
    // The root port the card sits behind: by its bus number. Needed to forcefully
    // reinitialize the card (its bridge is looked up strictly by bus number — otherwise we would reset
    // a neighbouring card, for example the Ethernet that carries remote access).
    let bridge = devices.iter().find(|b| b.secondary_bus() == Some(d.bus)).copied();
    // The topmost foreign memory BAR below 4 GiB: if we have to assign the card an address
    // ourselves (the BRIX firmware gives it none), the window goes strictly above it — so it
    // does not overlap any device, including the USB controller.
    let top_bar = pci::top_mem_bar_below_4g(devices);
    EXPECTED.store(true, Ordering::Relaxed);
    task::spawn("rtl8723be", None, move || {
        // The card is brought up once the system has started (network, remote access):
        // if something hangs, the log up to this point is already on disk.
        sleep_ms(START_DELAY_MS);
        // Protection against an endless reboot loop: if the previous boot hung while bringing up the card,
        // the watchdog rebooted the board into the same kernel — and it would hang again. Before
        // touching the card we set a marker; after bring-up (successful or not) we remove it. A marker
        // present at startup means we hung last time: leave the card alone.
        if task::as_system(|| crate::fs::exists(HUNG_MARKER)) {
            SETTLED.store(true, Ordering::Relaxed);
            return note("the card hung on the previous boot; leaving Wi-Fi off until the next kernel".into());
        }
        task::as_system(|| {
            let _ = crate::fs::write(HUNG_MARKER, b"");
            let _ = crate::fs::sync();
        });
        // The BRIX firmware does not configure the card's port: its BAR is empty, and the bridge's memory window
        // (00:1c.1) is closed, and a warm reboot does not fix this. The driver used to substitute
        // a hard-coded 0xD0700000 — but that is the USB controller's address (xHCI 00:14.0): writing
        // there killed the keyboard and mouse, and the card itself was never reached.
        // Now, if there is no address, we assign one ourselves: a free 1 MiB strictly above all foreign
        // BARs (USB is not touched) and open the bridge window. The address is computed, not guessed.
        let mut assigned = false;
        let addr = d.bar(REGS_BAR);
        let addr = if addr != 0 {
            addr
        } else if let Some(a) = bridge.and_then(|b| claim_window(&d, &b, top_bar)) {
            assigned = true;
            note(format!(
                "card had no address; opened its bridge window, registers now at {a:#x} (USB at 0xd0700000 untouched)"
            ));
            a
        } else {
            return fail("card has no address and no safe window to give it; Wi-Fi off".into());
        };
        mark(&format!("thread woke up, card registers at {addr:#x}, touching it"));
        // The card is allowed to write to memory only once it learns the new ring addresses:
        // after a warm reboot it may still hold the previous boot's addresses.
        let command = d.read(PCI_COMMAND);
        d.write(PCI_COMMAND, (command | MEMORY_SPACE | INTX_OFF) & !BUS_MASTER);
        let size = REGS_SIZE;
        crate::paging::map_mmio(addr, size as u64);
        let mmio = addr as usize;
        let chip = Chip { mmio };
        *CARD.lock() = Some((d, mmio));
        let cfg = chip.r32(SYS_CFG);
        // If we assigned the address ourselves — make sure the card really answers there
        // (rather than reading emptiness or another device): the word is neither 0 nor all ones. If
        // not — roll back the bridge window and the BAR and leave, touching nothing else.
        if assigned && (cfg == 0 || cfg == 0xFFFF_FFFF) {
            d.write(PCI_COMMAND, command & !MEMORY_SPACE);
            d.set_bar(REGS_BAR, 0);
            if let Some(b) = bridge {
                b.write(0x20, 0x0000_FFF0); // the bridge window is closed again (base > limit)
            }
            *CARD.lock() = None;
            return fail(format!("gave the card a window but it did not answer (SYS_CFG {cfg:08x}); Wi-Fi off"));
        }
        note(format!(
            "{:02x}:{:02x}.{} registers at {:#x} ({} KB), SYS_CFG {cfg:08x}: {} cut {}, Bluetooth {}",
            d.bus,
            d.slot,
            d.func,
            d.bar(REGS_BAR),
            size / 1024,
            if cfg & SYS_CFG_VENDOR_UMC != 0 { "UMC" } else { "TSMC" },
            (b'A' + (cfg >> SYS_CFG_CHIP_VER_SHIFT & 0xF) as u8) as char,
            if cfg & SYS_CFG_TRP_BT_EN != 0 { "on" } else { "off" }
        ));
        note(format!(
            "before power on: APS_FSMCO {:08x}, SYS_FUNC_EN {:04x}, CR {:04x}, MCUFWDL {:08x}, RSV_CTRL {:02x}",
            chip.r32(APS_FSMCO),
            chip.r16(SYS_FUNC_EN),
            chip.r16(CR),
            chip.r32(MCUFWDL),
            chip.r8(RSV_CTRL)
        ));
        mark("read registers");
        if chip.r8(MCUFWDL) != 0 || chip.r32(APS_FSMCO) & FSMCO_RDY_MACON != 0 {
            chip.power_off();
            note(format!("was left on (PCI command {command:04x}), powered off first: APS_FSMCO {:08x}, MCUFWDL {:08x}", chip.r32(APS_FSMCO), chip.r32(MCUFWDL)));
        }
        mark("about to power on");
        if let Err(e) = chip.power_on() {
            return fail(e);
        }
        note(format!("powered: APS_FSMCO {:08x}, SYS_FUNC_EN {:04x}, CR {:04x}", chip.r32(APS_FSMCO), chip.r16(SYS_FUNC_EN), chip.r16(CR)));
        mark("powered on, reading efuse");
        let map = match chip.read_efuse() {
            Ok(map) => map,
            Err(e) => return fail(e),
        };
        let id = u16::from_le_bytes([map[0], map[1]]);
        let mac = &map[ROM_MAC..ROM_MAC + 6];
        note(format!(
            "efuse id {id:04x}{}, MAC {}, crystal {:02x}, thermal {:02x}, bt option {:02x}, cck power {}",
            if id == ROM_ID { "" } else { " (unexpected)" },
            mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":"),
            map[ROM_CRYSTAL_CAP],
            map[ROM_THERMAL],
            map[ROM_RF_BT_OPT],
            hex(&map[ROM_TX_POWER..ROM_TX_POWER + 6])
        ));
        let firmware = task::as_system(|| crate::fs::read(FIRMWARE));
        let firmware = match firmware {
            Ok(f) => f,
            Err(e) => return fail(format!("{FIRMWARE}: {e}")),
        };
        mark("efuse read, loading firmware");
        match chip.load_firmware(&firmware) {
            Ok(version) => note(format!("firmware {version} running, MCUFWDL {:08x}", chip.r32(MCUFWDL))),
            Err(e) => return fail(e),
        }
        mark("firmware running, bringing up radio");
        let mut radio = match bring_up(&chip, &map) {
            Ok(radio) => radio,
            Err(e) => return fail(e),
        };
        // The card came up — there was no hang, remove the marker.
        clear_hung_marker();
        SETTLED.store(true, Ordering::Relaxed);
        // Receive and transmit completion wake the thread with an interrupt (MSI), not with a fast timer.
        let card = *CARD.lock();
        if let Some((d, _)) = card
            && crate::irq::msi_hook(&d, "wifi", wake).is_some()
        {
            chip.w32(HISR, 0xFFFF_FFFF);
            chip.w32(HISRE, 0xFFFF_FFFF);
            chip.w32(HIMR, HIMR_RX_TX);
            chip.w32(HIMRE, HIMRE_RX_OVERFLOW);
            IRQ_ON.store(true, Ordering::Relaxed);
        }
        serve(&chip, &mut radio);
    });
}

/// The radio after startup: the receive ring and the RF channel register.
struct Radio {
    rx: rx::Rx,
    tx: rx::Tx,
    mac: [u8; 6],
    /// Whether to send probe requests (enabled once it is confirmed that receive works).
    active: bool,
    chnlbw: u32,
    /// The WPA2 handshake of the current link: it answers group key renewals.
    hs: Option<crate::wifi::wpa::Handshake>,
    /// The next firmware command mailbox (transmit and the BT arbiter will need it).
    #[allow(dead_code)]
    mailbox: u8,
}

/// MAC, DMA, BB, RF, channel 1, coexistence with Bluetooth.
fn bring_up(chip: &Chip, map: &[u8; EFUSE_MAP_LEN]) -> Result<Radio, String> {
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&map[ROM_MAC..ROM_MAC + 6]);
    let rx = rx::Rx::new()?;
    let (tx_rings, tx) = rx::tx_rings()?;
    let rings = init::Rings { rx: rx.addr(), tx: tx_rings };
    mark("rings allocated, MAC table");
    chip.init_mac();
    mark("MAC table done, DMA");
    chip.init_dma(&rings)?;
    if let Some((d, _)) = *CARD.lock() {
        d.write(PCI_COMMAND, d.read(PCI_COMMAND) | BUS_MASTER);
    }
    mark("DMA done, protocol and PCIe extras");
    chip.init_protocol(&mac);
    chip.init_pcie();
    let ant = init::Antennas::from_efuse(map[ROM_RF_BT_OPT]);
    mark("protocol done, BB");
    chip.init_bb(map[ROM_CRYSTAL_CAP], ant);
    mark("BB done, RF");
    let chnlbw = chip.init_rf();
    mark("RF done, channel");
    chip.set_channel(chnlbw, 1);
    mark("channel set, IQ calibration");
    // Without IQK the 8723B receiver is deaf: it aligns the RX mixer and the CCA threshold. After bringing up
    // BB/RF and setting the channel, before the antenna/coex (IQK does not touch their registers).
    let mut mailbox = 0;
    chip.iq_calib(&mut mailbox, ant);
    chip.init_misc();
    mark("IQK done, coexistence");
    chip.post_init(&mut mailbox, ant)?;
    mark("coexistence done");
    note(format!("antennas: {}, Wi-Fi on {}", if ant.two { "two" } else { "one, shared with Bluetooth" }, if ant.aux { "S0 (aux)" } else { "S1 (main)" }));
    note(format!("coex: {}", chip.coex_regs()));
    note(format!("radio on: RF CHNLBW {chnlbw:05x}, RF 00 {:05x}, RF B0 {:05x}", chip.rf_read(0x00), chip.rf_read(0xB0)));
    note(chip.dump());
    note(format!("rings: rx {:#x}, tx {:#x}; {} receive descriptors armed", rings.rx, rings.tx[0], rx.armed()));
    if chip.pcie_dma_hung() {
        note("receive DMA is hung (0x350 bit 25): the card will hear nothing; a full power-off may be needed".into());
    }
    Ok(Radio { rx, tx, chnlbw, mailbox, mac, active: false, hs: None })
}

/// How long to listen on each channel: an access point beacon comes every ~100 ms.
const DWELL_TICKS: u64 = interrupts::HZ * 3 / 10;
const CHANNELS: core::ops::RangeInclusive<u8> = 1..=13;
const SCAN_WAIT_TICKS: u64 = interrupts::HZ * 10;

/// The card calls with an interrupt; the thread sleeps in `WAKE` while neither it nor the stack has anything to say.
static IRQ_ON: AtomicBool = AtomicBool::new(false);
static WAKE_PENDING: AtomicBool = AtomicBool::new(false);
static WAKE: crate::sync::WaitQueue = crate::sync::WaitQueue::new();

/// Wakes the card thread: an interrupt (receive, transmit completion) or the stack queued a frame.
pub fn wake() {
    WAKE_PENDING.store(true, Ordering::Release);
    WAKE.notify();
}

/// The card and its registers: `stop` needs them on reboot.
static CARD: IrqLock<Option<(pci::Device, usize)>> = IrqLock::new(None);
/// A request to the card thread to stop touching it, and its answer.
static STOP: AtomicBool = AtomicBool::new(false);
static PARKED: AtomicBool = AtomicBool::new(false);

/// Before a reboot and shutdown: the card must not write to memory (after a warm
/// reboot it is not reset and would write to the previous boot's addresses), and
/// the next boot should start with the card off.
pub fn stop() {
    let Some((d, mmio)) = *CARD.lock() else { return };
    STOP.store(true, Ordering::Relaxed);
    let end = deadline(100_000 * CYCLES_PER_US);
    while !PARKED.load(Ordering::Relaxed) && !expired(end) {
        task::yield_now();
    }
    // The main thing is to forbid the card from writing to memory. A full shutdown (as in power_off)
    // is not done here: after it a restart once failed to reach the system, and at
    // startup the driver turns the card off and on by itself anyway.
    // Order: first stop receive and wait until receive DMA hands over what it started,
    // and only then forbid memory writes. In the reverse order a frame arriving
    // in between could hang receive DMA (`pcie_dma_hung`) — the cause is not proven,
    // but with this order a warm reboot goes through cleanly.
    let chip = Chip { mmio };
    chip.set8(CR, CR_MACRXEN, 0);
    chip.set8(RXDMA_CONTROL, 0, RXDMA_STOP);
    chip.wait16(RXDMA_CONTROL, RXDMA_IDLE as u16, RXDMA_IDLE as u16, 2000);
    chip.w8(TXPAUSE, 0xFF);
    chip.w8(PCIE_CTRL_REG + 1, 0xFF);
    d.write(PCI_COMMAND, d.read(PCI_COMMAND) & !BUS_MASTER);
    crate::kprintln!("rtl8723be: stopped");
}

/// The card thread stops here forever if a reboot is in progress.
fn park_if_stopping() {
    if STOP.load(Ordering::Relaxed) {
        PARKED.store(true, Ordering::Relaxed);
        loop {
            task::sleep_until(interrupts::ticks() + interrupts::HZ);
        }
    }
}

static FOUND: IrqLock<Vec<Network>> = IrqLock::new(Vec::new());
static SCANS: AtomicU64 = AtomicU64::new(0);
static WANT_SCAN: AtomicBool = AtomicBool::new(false);
static READY: AtomicBool = AtomicBool::new(false);
/// A request to join a network: the name and the network key. The card thread picks it up in its loop.
static WANT_CONNECT: IrqLock<Option<(String, [u8; 32])>> = IrqLock::new(None);
/// The network we are connecting or connected to (for highlighting in the Network window). None — no
/// network.
static JOINED: IrqLock<Option<String>> = IrqLock::new(None);
/// The link is up and the keys are written: the card thread pumps frames between the network stack and the air.
static CONNECTED: AtomicBool = AtomicBool::new(false);
/// The current link's access point and the shared packet counter (PN) for CCMP: one for the probe and for
/// the pump, so numbers never repeat for a key (otherwise the access point drops them as replays).
static BSSID: IrqLock<[u8; 6]> = IrqLock::new([0; 6]);
static TX_PN: AtomicU64 = AtomicU64::new(0);

/// The tick when the access point was last heard (any of its frames, usually a beacon).
static HEARD: AtomicU64 = AtomicU64::new(0);
/// How long the access point may go unheard before the link is considered lost: beacons
/// come ~10 times a second, so 10 s of silence is not interference.
const SILENCE_LIMIT: u64 = interrupts::HZ * 10;
/// The reconnect test (`wifi drop`): move the radio to another channel, as if the access point vanished.
static DROP_TEST: AtomicBool = AtomicBool::new(false);

pub fn drop_test() {
    DROP_TEST.store(true, Ordering::Relaxed);
    wake();
}

/// The receive rate of the last unicast data frame, in tenths of Mbit/s (5.5 — CCK).
static RX_RATE: AtomicU32 = AtomicU32::new(0);

/// The receive rate, Mbit/s (rounded down). 0 — no link or no data yet.
pub fn rx_speed() -> u32 {
    if CONNECTED.load(Ordering::Relaxed) { RX_RATE.load(Ordering::Relaxed) / 10 } else { 0 }
}

/// The rate index from the receive descriptor in tenths of Mbit/s: 0–3 CCK, 4–11 OFDM,
/// 12–27 HT MCS 0–15 (20 MHz, normal guard interval).
fn rate_tenths(rate: u32) -> u32 {
    const LEGACY: [u32; 12] = [10, 20, 55, 110, 60, 90, 120, 180, 240, 360, 480, 540];
    const MCS: [u32; 8] = [65, 130, 195, 260, 390, 520, 585, 650];
    match rate {
        0..=11 => LEGACY[rate as usize],
        12..=27 => MCS[(rate - 12) as usize % 8] * ((rate - 12) / 8 + 1),
        _ => 0,
    }
}

/// The data frame rate: the index (0–3 CCK 1/2/5.5/11, 4–11 OFDM 6…54 Mbit/s) and fallback.
pub fn set_data_rate(rate: u32, fallback: bool) {
    rx::DATA_RATE.store(rate.min(11), Ordering::Relaxed);
    rx::DATA_FALLBACK.store(fallback, Ordering::Relaxed);
}

pub fn data_rate() -> (u32, bool) {
    (rx::DATA_RATE.load(Ordering::Relaxed), rx::DATA_FALLBACK.load(Ordering::Relaxed))
}

/// Asks the card thread to join a network (from the settings window through wifi::connect).
pub fn start_connect(ssid: String, pmk: [u8; 32]) {
    crate::wifi::set_rtl_status(format!("connecting to {ssid}\u{2026}"));
    *JOINED.lock() = Some(ssid.clone());
    *WANT_CONNECT.lock() = Some((ssid, pmk));
}

/// The network we are connecting/connected to (for the settings window).
pub fn network() -> Option<String> {
    JOINED.lock().clone()
}

/// The driver finished starting the card (successfully or not): the Bluetooth of the same card waits for this
/// so as not to start while the card's power is being turned off and on.
static SETTLED: AtomicBool = AtomicBool::new(false);

pub fn settled() -> bool {
    SETTLED.load(Ordering::Relaxed)
}

/// The minimum bridge window step is 1 MiB; the card registers (16 KiB) fit with room to spare.
const WINDOW: u64 = 0x10_0000;
/// The ceiling for our window: below the typical MMCONFIG (0xE000_0000) and the chipset regions.
const MMIO_CEILING: u64 = 0xE000_0000;

/// Gives the card an address the firmware did not: puts its bridge's non-prefetch memory window
/// on a free 1 MiB strictly above all foreign BARs (`top_bar`) and puts the card's BAR
/// there. Returns the address, or None if this is not the right bridge or there is no room. Writes only to
/// the bridge and card configuration and to a known free address — other devices are not affected.
fn claim_window(card: &pci::Device, bridge: &pci::Device, top_bar: u64) -> Option<u64> {
    // The bridge must be exactly the one the card sits behind (otherwise we would touch another bus).
    if bridge.secondary_bus() != Some(card.bus) {
        return None;
    }
    let slot = (top_bar + 1).next_multiple_of(WINDOW);
    if slot < 0xC000_0000 || slot + WINDOW > MMIO_CEILING {
        return None;
    }
    // Bridge register 0x20: the low word is the base, the high word the limit, in units of 1 MiB,
    // the low 20 bits are implied (the same format as the neighbouring bridge: 0xd060d060 = 1 MiB).
    let base = (slot >> 16) as u32 & 0xFFF0;
    let limit = ((slot + WINDOW - 1) >> 16) as u32 & 0xFFF0;
    bridge.write(0x20, limit << 16 | base);
    // The bridge decodes memory and passes the card's DMA upstream (bus master).
    bridge.write(PCI_COMMAND, bridge.read(PCI_COMMAND) | MEMORY_SPACE | BUS_MASTER);
    card.set_bar(REGS_BAR, slot);
    Some(slot)
}

/// Startup failed (but the card answered and did not hang): to the log, remove the marker,
/// Bluetooth need not wait.
fn fail(e: String) {
    clear_hung_marker();
    SETTLED.store(true, Ordering::Relaxed);
    note(e);
}

fn clear_hung_marker() {
    task::as_system(|| {
        if crate::fs::exists(HUNG_MARKER) {
            let _ = crate::fs::remove(HUNG_MARKER);
            let _ = crate::fs::sync();
        }
    });
}

/// The card is up and listening to the air.
pub fn present() -> bool {
    READY.load(Ordering::Relaxed)
}

/// The last network list found, without a new scan — for the settings window.
pub fn found() -> Vec<Network> {
    FOUND.lock().clone()
}

/// A fresh network scan: a pass over channels 1–13.
pub fn scan() -> Result<Vec<Network>, String> {
    if !present() {
        return Err("rtl8723be: the card is not up".into());
    }
    let seen = SCANS.load(Ordering::Relaxed);
    WANT_SCAN.store(true, Ordering::Relaxed);
    let end = interrupts::ticks() + SCAN_WAIT_TICKS;
    while SCANS.load(Ordering::Relaxed) == seen && interrupts::ticks() < end {
        task::sleep_until(interrupts::ticks() + interrupts::HZ / 4);
    }
    Ok(FOUND.lock().clone())
}

fn merge(found: &mut BTreeMap<[u8; 6], Network>, n: Network) {
    found
        .entry(n.bssid)
        .and_modify(|old| {
            old.signal = old.signal.max(n.signal);
            if old.ssid.is_empty() {
                old.ssid = n.ssid.clone();
            }
        })
        .or_insert(n);
}

/// Listens to the channels in turn and collects access points from beacons and probe responses.
fn scan_pass(chip: &Chip, radio: &mut Radio, verbose: bool) -> Vec<Network> {
    let mut found = BTreeMap::new();
    for channel in CHANNELS {
        chip.set_channel(radio.chnlbw, channel);
        if radio.active && !radio.tx.send(chip, &rx::probe_request(&radio.mac, channel)) {
            crate::kprintln!("rtl8723be: probe request on channel {channel} not queued ({} pending)", radio.tx.pending());
        }
        chip.clear_counters();
        let cca_before = chip.cca();
        let before = radio.rx.counts;
        let mut beacons = 0;
        let end = interrupts::ticks() + DWELL_TICKS;
        loop {
            park_if_stopping();
            radio.rx.poll(|f| {
                let signal = f.signal().unwrap_or(-100);
                if let Some(n) = wifi::network_from(f.data, channel, signal, f.time()) {
                    beacons += 1;
                    merge(&mut found, n);
                }
            });
            if interrupts::ticks() >= end {
                break;
            }
            task::sleep_until(interrupts::ticks() + 1);
        }
        if verbose {
            let c = radio.rx.counts;
            let cca = chip.cca();
            note(format!(
                "ch {channel:>2}: {} frames, {beacons} beacons, {} reports, {} broken; CCA ofdm {} cck {}; PHY ok/fail: ofdm {}/{}, cck {}/{}, ht {}/{}",
                c.frames - before.frames,
                c.reports - before.reports,
                c.broken - before.broken,
                cca.0.wrapping_sub(cca_before.0) & 0xFFFF,
                cca.1.wrapping_sub(cca_before.1) & 0xFFFF,
                chip.counter(init::COUNT_OFDM_OK),
                chip.counter(init::COUNT_OFDM_FAIL),
                chip.counter(init::COUNT_CCK_OK),
                chip.counter(init::COUNT_CCK_FAIL),
                chip.counter(init::COUNT_HT_OK),
                chip.counter(init::COUNT_HT_FAIL)
            ));
        }
    }
    let mut list: Vec<Network> = found.into_values().collect();
    list.sort_by(|a, b| b.signal.cmp(&a.signal));
    list
}

fn mac_text(m: &[u8; 6]) -> String {
    m.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// The link is gone: return the card to scanning, forget the network (then auto-connect
/// takes over again) and scan for networks right away.
fn link_lost(chip: &Chip, radio: &mut Radio, reason: &str) {
    note(format!("link lost: {reason}; reconnecting"));
    CONNECTED.store(false, Ordering::Relaxed);
    crate::wifi::nic_down();
    radio.hs = None;
    chip.leave_mac();
    interrupts::fast_wakeups_for(interrupts::FAST_FOR_WIFI, false);
    let ssid = JOINED.lock().take();
    crate::wifi::set_rtl_status(format!("connection to {} lost — reconnecting", ssid.as_deref().unwrap_or("the network")));
    WANT_SCAN.store(true, Ordering::Relaxed);
}

/// The first scan with a detailed log, then scans on request; the receive ring
/// is drained all the time.
/// While there is a link, pumps frames between the network stack and the card: outgoing Ethernet frames
/// from the stack are wrapped into encrypted 802.11 data frames, incoming ones are unwrapped back into
/// Ethernet. Only this thread touches the card's TX/RX rings; it talks to the stack through queues.
/// Returns whether there was traffic with us specifically (unicast frames in either direction): then the thread
/// should wake often for a while.
///
/// Err — the link is lost (the access point disconnected us); the reason for the log.
fn pump(chip: &Chip, radio: &mut Radio) -> Result<bool, String> {
    let bssid = *BSSID.lock();
    let mac = radio.mac;
    let mut active = send_queued(chip, radio, &bssid);
    let (mut got, mut ours) = (0u32, false);
    let mut eapol: Vec<Vec<u8>> = Vec::new();
    let mut lost = None;
    radio.rx.poll(|f| {
        let d = f.data;
        if d.len() >= 24 && d[10..16] == bssid {
            HEARD.store(interrupts::ticks(), Ordering::Relaxed);
            // Deauthentication (deauth 0xC0) or disassociation (disassoc 0xA0) — to us or to everyone.
            if (d[0] == 0xC0 || d[0] == 0xA0) && (d[4..10] == mac || d[4] & 1 != 0) {
                let reason = d.get(24..26).map_or(0, |r| u16::from_le_bytes([r[0], r[1]]));
                let what = if d[0] == 0xC0 { "deauthenticated" } else { "disassociated" };
                lost = Some(format!("the access point {what} us, reason {reason}"));
            }
        }
        // The access point retransmits a frame (the Retry bit) if it did not hear our acknowledgement:
        // a repeat with the same sender and sequence number is dropped, otherwise the answer would go out twice.
        if d.len() >= 24 && d[0] & 0x0C == 0x08 {
            let id = (<[u8; 6]>::try_from(&d[10..16]).unwrap_or([0; 6]), u16::from_le_bytes([d[22], d[23]]));
            let mut last = LAST_RX.lock();
            if d[1] & 0x08 != 0 && *last == id {
                return;
            }
            *last = id;
        }
        if let Some(eth) = connect::unwrap_eth(d, &mac) {
            // Key changes come as EAPOL frames — we handle them ourselves, the stack does not need them.
            if eth.get(12..14) == Some(&[0x88, 0x8E]) {
                eapol.push(eth[14..].to_vec());
                return;
            }
            // Broadcast frames go at a low rate and say nothing about the link.
            if eth[0] & 1 == 0 {
                ours = true;
                RX_RATE.store(rate_tenths(f.rate()), Ordering::Relaxed);
            }
            crate::wifi::nic_deliver_rx(eth);
            got += 1;
        }
    });
    if let Some(reason) = lost {
        return Err(reason);
    }
    for frame in eapol {
        rekey(chip, radio, &bssid, &frame);
    }
    if got > 0 {
        // Handle right away, in the same pass, and send the answers right away — do not wait until
        // the stack thread wakes up (up to a tick, 10 ms).
        crate::net::poll();
        active |= send_queued(chip, radio, &bssid);
    }
    Ok(active || ours)
}

/// An EAPOL frame on a live link: the access point renews the group key (usually once an hour). If we do not
/// answer, it disconnects us after a few attempts. The answer goes out already encrypted.
fn rekey(chip: &Chip, radio: &mut Radio, bssid: &[u8; 6], frame: &[u8]) {
    let Some(hs) = radio.hs.as_mut() else { return };
    match hs.key_frame(frame) {
        Ok(crate::wifi::wpa::Step::Group { reply, group }) => {
            chip.install_group_key(group);
            let mut eth = Vec::with_capacity(14 + reply.len());
            eth.extend_from_slice(bssid);
            eth.extend_from_slice(&radio.mac);
            eth.extend_from_slice(&[0x88, 0x8E]);
            eth.extend_from_slice(&reply);
            let pn = TX_PN.fetch_add(1, Ordering::Relaxed) + 1;
            if let Some(f) = connect::wrap_eth(&radio.mac, bssid, pn, &eth) {
                radio.tx.send_encrypted(chip, &f);
            }
            note(format!("group key renewed (key {})", group.0));
        }
        Ok(crate::wifi::wpa::Step::Ignore) => {}
        Ok(_) => note("the access point asked to renew the pairwise key: not supported yet".into()),
        Err(e) => note(format!("key frame rejected: {e}")),
    }
}

/// Sends everything the stack queued onto the air. true — something went out.
fn send_queued(chip: &Chip, radio: &mut Radio, bssid: &[u8; 6]) -> bool {
    let mut sent = false;
    // Only while the card has room: the rest waits in the queue until the next pass.
    while radio.tx.has_room()
        && let Some(eth) = crate::wifi::nic_take_tx()
    {
        let pn = TX_PN.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(frame) = connect::wrap_eth(&radio.mac, bssid, pn, &eth) {
            radio.tx.send_encrypted(chip, &frame);
            sent = true;
        }
    }
    sent
}

/// The sender and sequence number of the last received data frame — to drop its repeat.
static LAST_RX: IrqLock<([u8; 6], u16)> = IrqLock::new(([0; 6], 0xFFFF));

fn serve(chip: &Chip, radio: &mut Radio) -> ! {
    let list = scan_pass(chip, radio, true);
    note(format!("scan: {} networks; after scan: {}", list.len(), chip.dump()));
    if radio.rx.counts.frames == 0 {
        diag::fallback(chip, radio);
    } else {
        // Receive works — from now on also scan with probe requests; check that transmit goes out.
        radio.active = true;
        let sent = radio.tx.send(chip, &rx::probe_request(&radio.mac, 13));
        sleep_ms(50);
        note(format!("probe request {}; {} still queued, HISR {:08x}", if sent { "queued" } else { "refused" }, radio.tx.pending(), chip.r32(0x0B4)));
    }
    for n in &list {
        note(format!("  {}  {}", wifi::describe(n), mac_text(&n.bssid)));
    }
    *FOUND.lock() = list;
    SCANS.fetch_add(1, Ordering::Relaxed);
    READY.store(true, Ordering::Relaxed);
    // Until which tick to wake often after traffic with us (see below).
    const ACTIVE_TICKS: u64 = interrupts::HZ * 2;
    let mut active_until = 0u64;
    // Auto-connecting to a saved network: when to try again and how many times it failed.
    let mut auto_at = 0u64;
    let mut failures = 0u64;
    loop {
        park_if_stopping();
        let want = WANT_CONNECT.lock().take();
        if let Some((ssid, pmk)) = want {
            match connect::run(chip, radio, &ssid, pmk) {
                Ok(()) => {
                    failures = 0;
                    // There is a link — remember the network (the key, not the password) so next time it connects by itself.
                    crate::wifi::remember(&ssid, &pmk);
                }
                Err(e) => {
                    failures += 1;
                    // The next attempt uses a fresh scan: the access point may have changed channel.
                    WANT_SCAN.store(true, Ordering::Relaxed);
                    CONNECTED.store(false, Ordering::Relaxed);
                    crate::wifi::nic_down();
                    *JOINED.lock() = None;
                    note(format!("connect to {ssid} failed: {e}"));
                    crate::wifi::set_rtl_status(format!("couldn't connect to {ssid}: {e}"));
                }
            }
        }
        // With a link — pump the stack's frames over the air often; otherwise scan for networks and doze.
        if CONNECTED.load(Ordering::Relaxed) {
            let now = interrupts::ticks();
            let irq = IRQ_ON.load(Ordering::Relaxed);
            if irq {
                // Clear the status bits before handling: a frame arriving later will call again.
                for reg in [HISR, HISRE] {
                    let status = chip.r32(reg);
                    if status != 0 {
                        chip.w32(reg, status);
                    }
                }
            }
            let silent = now.saturating_sub(HEARD.load(Ordering::Relaxed));
            let worked = match pump(chip, radio) {
                Ok(_) if silent > SILENCE_LIMIT => Err(format!("nothing heard from the access point for {} s", silent / interrupts::HZ)),
                other => other,
            };
            let worked = match worked {
                Ok(w) => w,
                Err(reason) => {
                    link_lost(chip, radio, &reason);
                    failures = 0;
                    auto_at = interrupts::ticks();
                    continue;
                }
            };
            if DROP_TEST.swap(false, Ordering::Relaxed) {
                // Another channel: the access point is no longer heard, the silence check will fire.
                let channel = found().iter().find(|n| Some(&n.ssid) == JOINED.lock().as_ref()).map_or(1, |n| n.channel);
                chip.set_channel(radio.chnlbw, if channel > 6 { 1 } else { 13 });
                note("drop test: the radio left the network's channel".into());
            }
            if worked {
                active_until = now + ACTIVE_TICKS;
            }
            if irq {
                // There was work — another pass right away; if not — sleep until an interrupt, a frame from the stack
                // or a tick (connection timers).
                if !worked {
                    WAKE.wait_until(now + 1, || WAKE_PENDING.swap(false, Ordering::AcqRel));
                }
                continue;
            }
            // After traffic with us (and while the stack waits for an answer) wake often — the fast timer and
            // a short pause; otherwise every request over Wi-Fi would lose up to a tick (10 ms) in each
            // direction. In quiet — once a tick again, the timer is released.
            let fast = (now < active_until || crate::net::exchanging()) && interrupts::fast_wakeups_for(interrupts::FAST_FOR_WIFI, true);
            if fast {
                task::nap(crate::net::NAP_CYCLES);
            } else {
                interrupts::fast_wakeups_for(interrupts::FAST_FOR_WIFI, false);
                task::sleep_until(now + 1);
            }
            continue;
        }
        let now = interrupts::ticks();
        if now >= auto_at && JOINED.lock().is_none() {
            // Every half minute (less often after failures): a saved network is nearby — connect.
            auto_at = now + interrupts::HZ * 30 * (1 + failures.min(9));
            let saved = crate::wifi::saved_networks();
            if !saved.is_empty() {
                let best = found().into_iter().filter(|n| saved.contains(&n.ssid)).max_by_key(|n| n.signal);
                match best.and_then(|n| crate::wifi::saved_key(&n.ssid).map(|k| (n.ssid, k))) {
                    Some((ssid, key)) => {
                        note(format!("connecting to the saved network {ssid}"));
                        start_connect(ssid, key);
                        continue;
                    }
                    // No saved networks visible nearby — scan again before the next attempt.
                    None => WANT_SCAN.store(true, Ordering::Relaxed),
                }
            }
        }
        if WANT_SCAN.swap(false, Ordering::Relaxed) {
            let list = scan_pass(chip, radio, false);
            crate::kprintln!("rtl8723be: scan: {} networks", list.len());
            *FOUND.lock() = list;
            SCANS.fetch_add(1, Ordering::Relaxed);
        }
        radio.rx.poll(|_| {});
        task::sleep_until(interrupts::ticks() + interrupts::HZ / 10);
    }
}
