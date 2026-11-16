//! Stage 3: MAC, DMA, BB, the RF part, the channel and coexistence with Bluetooth.
//!
//! The order of steps follows FreeBSD's rtwn_init (r23b_*, r92e_*, r92c_*), the PCIe steps follow
//! r88ee and rtwn_pci. Value tables are in `tables.rs`.

use super::{Chip, pause_us, sleep_ms, tables};
use alloc::format;
use alloc::string::String;

// MAC registers.
const RF_CTRL: usize = 0x01F;
const AFE_XTAL_CTRL: usize = 0x024;
const MAC_PHY_CTRL: usize = 0x02C;
const PWR_DATA: usize = 0x038;
const GPIO_MUXCFG: usize = 0x040;
const LEDCFG0: usize = 0x04C;
const BB_PAD_CTRL: usize = 0x064;
const HIMR: usize = 0x0B0;
const HISR: usize = 0x0B4;
const HIMRE: usize = 0x0B8;
const HISRE: usize = 0x0BC;
const CR: usize = 0x100;
const MSR: usize = 0x102;
const PBP: usize = 0x104;
const TRXDMA_CTRL: usize = 0x10C;
const TRXFF_BNDY: usize = 0x114;
const MCUTST_1: usize = 0x1C0;
const HMEBOX: usize = 0x1D0;
const HMEBOX_EXT: usize = 0x1F0;
const HMETFR: usize = 0x1CC;
const RQPN: usize = 0x200;
const TDECTRL: usize = 0x208;
const RQPN_NPQ: usize = 0x214;
const AUTO_LLT: usize = 0x224;
const PCIE_CTRL_REG: usize = 0x300;
const INT_MIG: usize = 0x304;
pub(super) const BCNQ_DESA: usize = 0x308;
pub(super) const HQ_DESA: usize = 0x310;
pub(super) const MGQ_DESA: usize = 0x318;
pub(super) const VOQ_DESA: usize = 0x320;
pub(super) const VIQ_DESA: usize = 0x328;
pub(super) const BEQ_DESA: usize = 0x330;
pub(super) const BKQ_DESA: usize = 0x338;
pub(super) const RX_DESA: usize = 0x340;
const FWHW_TXQ_CTRL: usize = 0x420;
const HWSEQ_CTRL: usize = 0x423;
const TXPKTBUF_BCNQ_BDNY: usize = 0x424;
const TXPKTBUF_MGQ_BDNY: usize = 0x425;
const SPEC_SIFS: usize = 0x428;
const RL: usize = 0x42A;
const DARFRC: usize = 0x430;
const RARFRC: usize = 0x438;
const TXPKTBUF_WMAC_LBK_BF_HD: usize = 0x45D;
const DATA_SEC: usize = 0x483;
const TX_RPT_CTRL: usize = 0x4EC;
const TX_RPT_MACID_MAX: usize = 0x4ED;
const TX_RPT_TIME: usize = 0x4F0;
const BAR_MODE_CTRL: usize = 0x4CC;
const EDCA_VO_PARAM: usize = 0x500;
const EDCA_VI_PARAM: usize = 0x504;
const EDCA_BE_PARAM: usize = 0x508;
const EDCA_BK_PARAM: usize = 0x50C;
const BCNTCFG: usize = 0x510;
const SIFS_CCK: usize = 0x514;
const SIFS_OFDM: usize = 0x516;
const TXPAUSE: usize = 0x522;
const TBTT_PROHIBIT: usize = 0x540;
const BCN_CTRL0: usize = 0x550;
const BCN_CTRL1: usize = 0x551;
const BCNDMATIM: usize = 0x559;
const BWOPMODE: usize = 0x603;
const BWOPMODE_20MHZ: u8 = 0x04;
const TCR: usize = 0x604;
pub(super) const RCR: usize = 0x608;
const RX_DRVINFO_SZ: usize = 0x60F;
const MACID: usize = 0x610;
const MAR: usize = 0x620;
const MAC_SPEC_SIFS: usize = 0x63A;
const ACKTO: usize = 0x640;
const NAV_UPPER: usize = 0x652;
const RXERR_RPT: usize = 0x664;
const WMAC_TRXPTCL_CTL: usize = 0x668;
const CAMCMD: usize = 0x670;
const SECCFG: usize = 0x680;
const RXFLTMAP0: usize = 0x6A0;
const RXFLTMAP1: usize = 0x6A2;
const RXFLTMAP2: usize = 0x6A4;
const BTCOEX_TABLE: usize = 0x6C0;
const ANT_SEL: usize = 0x948;
/// Internal switch: Wi-Fi on path S1 (Bluetooth on S0) and vice versa.
const ANT_SEL_S1_WIFI: u32 = 0x000;
const ANT_SEL_S0_WIFI: u32 = 0x280;
/// RX antenna selection: 1 — the main (left) one.
const RFPATH_SWITCH: usize = 0x92C;
const SECONDARY_CCA_CTRL: usize = 0x577;
const RXDMA_CONTROL: usize = 0x286;
const RXDMA_STOP: u8 = 0x04;
const LEDCFG0_DPDT_SEL_EN: u32 = 1 << 23;

// BB registers (32-bit).
const FPGA0_RFMOD: usize = 0x800;
const HSSI_PARAM1: usize = 0x820;
const HSSI_PARAM2: usize = 0x824;
const LSSI_PARAM: usize = 0x840;
const FPGA0_RFIFACEOE: usize = 0x860;
const FPGA0_RFIFACESW: usize = 0x870;
const LSSI_READBACK: usize = 0x8A0;
const HSPI_READBACK: usize = 0x8B8;
const FPGA1_RFMOD: usize = 0x900;
const OFDM0_AGCCORE1: usize = 0xC50;
const OFDM0_AGCRSSITABLE: usize = 0xC78;
const OFDM0_TXPSEUDONOISEWGT: usize = 0xCE4;
const OFDM1_LSTF: usize = 0xD00;

// RF registers (over the LSSI serial bus).
const RF_AC: u8 = 0x00;
const RF_IQADJ_G0: u8 = 0x01;
const RF_CHNLBW: u8 = 0x18;
const RF_S0S1: u8 = 0xB0;

// Bits.
const CR_MACTXEN: u8 = 0x40;
const CR_MACRXEN: u8 = 0x80;
const AUTO_LLT_INIT: u32 = 0x0001_0000;
const RQPN_LD: u32 = 0x8000_0000;
const QMAP_MASK: u16 = 0xFFF0;
/// TX queues → PCIe DMA queues (Realtek's value for 8723BE): voice, management
/// and "high" go to the high one, video to the normal one, the rest to the low one.
const QMAP_8723BE: u16 = 0xF5B1;
const TCR_PCIE: u32 = 0x8200;
const SYS_FUNC_EN: usize = 0x002;
const FUNC_BBRSTB: u16 = 0x0001;
const FUNC_BB_GLB_RST: u16 = 0x0002;
const FUNC_DIO_RF: u16 = 0x2000;
const FUNC_ALL_PCIE: u8 = 0xE3;
const RF_CTRL_ON: u8 = 0x07;
const HSSI2_ADDR_LENGTH: u32 = 0x400;
const HSSI2_DATA_LENGTH: u32 = 0x800;
const HSSI2_READ_ADDR_SHIFT: u32 = 23;
const HSSI2_READ_ADDR_MASK: u32 = 0x7F80_0000;
const HSSI2_READ_EDGE: u32 = 0x8000_0000;
const HSSI1_PI: u32 = 0x100;
const LSSI_ADDR_SHIFT: u32 = 20;
const LSSI_ADDR_MASK: u32 = 0x0FF0_0000;
const LSSI_DATA_MASK: u32 = 0x000F_FFFF;
const RFMOD_40MHZ: u32 = 0x1;
const RFMOD_CCK_EN: u32 = 0x0100_0000;
const RFMOD_OFDM_EN: u32 = 0x0200_0000;
const CHNLBW_CHANNEL: u32 = 0x3FF;
const CHNLBW_BW: u32 = 0xC00;
const CHNLBW_LCSTART: u32 = 0x8000;
const RF_AC_MODE: u32 = 0x7_0000;
const RF_AC_STANDBY: u32 = 0x1_0000;
const CRYSTAL_MASK: u32 = 0x00FF_F000;
const CRYSTAL_SHIFT: u32 = 12;
const GPIO_MUXCFG_ENBT: u8 = 0x20;
const TX_QUEUE_ALL: u8 = 0x3F;
const FWHW_TXQ_AMPDU_RTY_NEW: u8 = 0x80;
const FWHW_TXQ_ACK_MGNT: u32 = 0x1000;
const BCN_DIS_TSF_UDT0: u8 = 0x10;
const CAMCMD_POLLING: u32 = 0x8000_0000;
const CAMCMD_CLR: u32 = 0x4000_0000;
const MSR_MASK: u8 = 0x03;
const MSR_NOLINK: u8 = 0x00;
const TX_RPT2_ENA: u8 = 0x02;

/// Receive: our frames, multicast, broadcast, management; each frame gets PHY
/// status (signal level) and a checksum. Foreign BSSIDs are not filtered: we scan for networks.
pub(super) const RCR_SCAN: u32 = RCR_APM | RCR_AM | RCR_AB | RCR_AMF | RCR_HTC_LOC_CTRL | RCR_APP_PHYSTS | RCR_APP_ICV | RCR_APP_MIC | RCR_APPFCS;
const RCR_APM: u32 = 0x0000_0002;
const RCR_AM: u32 = 0x0000_0004;
const RCR_AB: u32 = 0x0000_0008;
const RCR_AMF: u32 = 0x0000_2000;
const RCR_HTC_LOC_CTRL: u32 = 0x0000_4000;
const RCR_APP_PHYSTS: u32 = 0x1000_0000;
const RCR_APP_ICV: u32 = 0x2000_0000;
const RCR_APP_MIC: u32 = 0x4000_0000;
const RCR_APPFCS: u32 = 0x8000_0000;

// The frame queue in card memory: 128-byte pages.
const TX_PAGE_COUNT: u8 = 247;
const PUBQ_PAGES: u32 = 231;
const HPQ_PAGES: u32 = 12;
const LPQ_PAGES: u32 = 2;
const NPQ_PAGES: u8 = 2;
/// Size of the card's receive buffer.
const RX_FIFO: u16 = 0x3F80;
/// PHY status before a frame — 32 bytes, in eight-byte words.
const DRVINFO_WORDS: u8 = 4;
/// A 256-byte page for both receive and transmit.
const PAGE_SIZE_256: u8 = 0x22;
const ACK_TIMEOUT: u8 = 0x40;

// Firmware commands (H2C).
const H2C_PS_TDMA: u8 = 0x60;
const H2C_BT_INFO: u8 = 0x61;
const H2C_BT_WLANACT: u8 = 0x63;
const H2C_ANT_INV: u8 = 0x65;
const H2C_BT_MP_OPER: u8 = 0x67;
const H2C_BT_GRANT: u8 = 0x6E;
const BT_MP_WRITE_VAL: u8 = 0x0D;
const BT_MP_WRITE_ADDR: u8 = 0x0C;
const H2C_BOXES: u8 = 4;

/// Receive counter types in RXERR_RPT (the number is in bits 28–31).
pub(super) const COUNT_OFDM_OK: u32 = 2;
pub(super) const COUNT_OFDM_FAIL: u32 = 3;
pub(super) const COUNT_CCK_OK: u32 = 6;
pub(super) const COUNT_CCK_FAIL: u32 = 7;
pub(super) const COUNT_HT_OK: u32 = 11;
pub(super) const COUNT_HT_FAIL: u32 = 12;
const COUNT_MASK: u32 = 0xF_FFFF;
const COUNT_RESET: u32 = 1 << 27;

/// Where the rings are in memory (physical addresses, below 4 GiB).
pub(super) struct Rings {
    pub rx: u32,
    /// TX rings: beacon, high, management, voice, video, normal, background.
    pub tx: [u32; 7],
}

/// How many antennas the module has and where the main one is connected (EFUSE 0xC3: bit 0 — one
/// antenna shared by Wi-Fi and Bluetooth, bit 6 — it is on the auxiliary path S0).
#[derive(Clone, Copy)]
pub(super) struct Antennas {
    pub two: bool,
    pub aux: bool,
}

impl Antennas {
    pub fn from_efuse(option: u8) -> Self {
        Antennas { two: option & 0x01 == 0, aux: option & 0x40 != 0 }
    }
}

impl Chip {
    // --- BB and RF ---

    /// Read an RF register (like r92e_rf_read).
    pub(super) fn rf_read(&self, addr: u8) -> u32 {
        let val = self.r32(HSSI_PARAM2);
        self.w32(HSSI_PARAM2, (val & !HSSI2_READ_ADDR_MASK | (addr as u32) << HSSI2_READ_ADDR_SHIFT & HSSI2_READ_ADDR_MASK) & !HSSI2_READ_EDGE);
        pause_us(10);
        self.set32(HSSI_PARAM2, HSSI2_READ_EDGE, 0);
        pause_us(100);
        self.set32(HSSI_PARAM2, 0, HSSI2_READ_EDGE);
        pause_us(20);
        let val = if self.r32(HSSI_PARAM1) & HSSI1_PI != 0 { self.r32(HSPI_READBACK) } else { self.r32(LSSI_READBACK) };
        val & LSSI_DATA_MASK
    }

    fn rf_put(&self, addr: u8, val: u32) {
        self.w32(LSSI_PARAM, (addr as u32) << LSSI_ADDR_SHIFT & LSSI_ADDR_MASK | val & LSSI_DATA_MASK);
    }

    /// Write to the RF part (like r23b_rf_write): 0xB6 and 0xB2 are not always written
    /// while warming up, so they are checked and retried.
    pub(super) fn rf_write(&self, addr: u8, val: u32) {
        self.rf_put(addr, val);
        match addr {
            0xB6 => {
                pause_us(1);
                let mut reg = self.rf_read(addr);
                for _ in 0..6 {
                    if reg & !0xFF == val & !0xFF {
                        break;
                    }
                    pause_us(1);
                    self.rf_put(addr, val);
                    pause_us(1);
                    reg = self.rf_read(addr);
                }
            }
            0xB2 => {
                pause_us(1);
                let mut reg = self.rf_read(addr);
                for _ in 0..6 {
                    if reg == val {
                        break;
                    }
                    self.rf_put(addr, val);
                    pause_us(1);
                    self.rf_write(RF_CHNLBW, 0x0FC07);
                    pause_us(1);
                    reg = self.rf_read(addr);
                }
            }
            _ => {}
        }
    }

    fn rf_set(&self, addr: u8, clear: u32, set: u32) {
        self.rf_write(addr, self.rf_read(addr) & !clear | set);
    }

    // --- Firmware commands ---

    /// A command to the 8051 firmware through the mailboxes (like r88e_fw_cmd): the first word
    /// is written last — it starts the processing.
    pub(super) fn h2c(&self, box_no: &mut u8, id: u8, msg: &[u8]) -> Result<(), String> {
        let mut free = false;
        for _ in 0..100 {
            if self.r8(HMETFR) & (1 << *box_no) == 0 {
                free = true;
                break;
            }
            pause_us(2000);
        }
        if !free {
            return Err(format!("firmware mailbox {box_no} stays busy (command {id:#x})"));
        }
        let mut cmd = [0u8; 8];
        cmd[0] = id;
        cmd[1..1 + msg.len()].copy_from_slice(msg);
        let n = *box_no as usize;
        if msg.len() > 3 {
            self.w32(HMEBOX_EXT + 4 * n, u32::from_le_bytes([cmd[4], cmd[5], cmd[6], cmd[7]]));
        }
        self.w32(HMEBOX + 4 * n, u32::from_le_bytes([cmd[0], cmd[1], cmd[2], cmd[3]]));
        *box_no = (*box_no + 1) % H2C_BOXES;
        Ok(())
    }

    /// A register of the Bluetooth part through the firmware (like r23b_write_btreg).
    fn bt_write(&self, box_no: &mut u8, addr: u8, data: u8) -> Result<(), String> {
        self.h2c(box_no, H2C_BT_MP_OPER, &[0, BT_MP_WRITE_VAL, data, 0])?;
        sleep_ms(200);
        self.h2c(box_no, H2C_BT_MP_OPER, &[1, BT_MP_WRITE_ADDR, 0, addr])
    }

    // --- Startup steps ---

    /// MAC values from Realtek's table.
    pub(super) fn init_mac(&self) {
        for &(reg, val) in tables::MAC.iter() {
            self.w8(reg as usize, val);
        }
    }

    /// Queue page table (LLT), page allocation, boundaries, DMA rings.
    pub(super) fn init_dma(&self, rings: &Rings) -> Result<(), String> {
        self.set32(AUTO_LLT, 0, AUTO_LLT_INIT);
        if !self.wait32(AUTO_LLT, AUTO_LLT_INIT, 0, 10_000) {
            return Err(format!("LLT did not initialise, AUTO_LLT {:08x}", self.r32(AUTO_LLT)));
        }
        self.w8(RQPN_NPQ, NPQ_PAGES);
        self.w32(RQPN, PUBQ_PAGES << 16 | LPQ_PAGES << 8 | HPQ_PAGES | RQPN_LD);
        let boundary = TX_PAGE_COUNT + 1;
        self.w8(TXPKTBUF_BCNQ_BDNY, boundary);
        self.w8(TXPKTBUF_MGQ_BDNY, boundary);
        self.w8(TXPKTBUF_WMAC_LBK_BF_HD, boundary);
        self.w8(TRXFF_BNDY, boundary);
        self.w8(TDECTRL + 1, boundary);
        self.set16(TRXDMA_CTRL, QMAP_MASK, QMAP_8723BE);
        self.w32(TCR, TCR_PCIE);
        for (reg, &addr) in [BCNQ_DESA, HQ_DESA, MGQ_DESA, VOQ_DESA, VIQ_DESA, BEQ_DESA, BKQ_DESA].iter().zip(rings.tx.iter()) {
            self.w32(*reg, addr);
            self.w32(*reg + 4, 0);
        }
        self.w32(RX_DESA, rings.rx);
        self.w32(RX_DESA + 4, 0);
        // PCIe DMA queues are not stopped (after another OS this can be 0xFF).
        self.w8(PCIE_CTRL_REG + 1, 0);
        self.w32(INT_MIG, 0);
        self.w32(MCUTST_1, 0);
        self.w16(TRXFF_BNDY + 2, RX_FIFO - 1);
        self.w8(PBP, PAGE_SIZE_256);
        Ok(())
    }

    /// Everything from the PHY status size to enabling MAC receive and transmit.
    pub(super) fn init_protocol(&self, mac: &[u8; 6]) {
        self.w8(RX_DRVINFO_SZ, DRVINFO_WORDS);
        // Interrupts are off: we poll for receive ourselves.
        self.w32(HIMR, 0);
        self.w32(HIMRE, 0);
        self.w32(HISR, u32::MAX);
        self.w32(HISRE, u32::MAX);
        self.set8(MSR, MSR_MASK, MSR_NOLINK);
        self.w32(MACID, u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]));
        self.w16(MACID + 4, u16::from_le_bytes([mac[4], mac[5]]));
        // All multicast addresses; management — all subtypes, control and data not yet.
        self.w32(MAR, u32::MAX);
        self.w32(MAR + 4, u32::MAX);
        self.w16(RXFLTMAP0, 0xFFFF);
        self.w16(RXFLTMAP1, 0);
        self.w16(RXFLTMAP2, 0);
        self.w32(RCR, RCR_SCAN);
        self.w16(RL, 0x30 << 8 | 0x30);
        // EDCA (r92c_init_edca).
        self.w16(SPEC_SIFS, 0x100A);
        self.w16(MAC_SPEC_SIFS, 0x100A);
        self.w16(SIFS_CCK, 0x100A);
        self.w16(SIFS_OFDM, 0x100A);
        self.w32(EDCA_BE_PARAM, 0x005E_A42B);
        self.w32(EDCA_BK_PARAM, 0x0000_A44F);
        self.w32(EDCA_VI_PARAM, 0x005E_A324);
        self.w32(EDCA_VO_PARAM, 0x002F_A226);
        self.set8(FWHW_TXQ_CTRL, 0, FWHW_TXQ_AMPDU_RTY_NEW);
        self.w8(ACKTO, ACK_TIMEOUT);
        // Beacons (rtwn_init_beacon_reg).
        self.w8(BCN_CTRL0, BCN_DIS_TSF_UDT0);
        self.w8(BCN_CTRL1, BCN_DIS_TSF_UDT0);
        self.w16(TBTT_PROHIBIT, 0x6404);
        self.w8(BCNDMATIM, 0x02);
        self.w16(BCNTCFG, 0x660F);
        self.set8(CR, 0, CR_MACTXEN | CR_MACRXEN);
    }

    /// Steps Realtek performs for the PCIe module (FreeBSD's USB path does not have them):
    /// queues, secondary CCA, antenna switch pins, DMA receive.
    pub(super) fn init_pcie(&self) {
        self.w8(FWHW_TXQ_CTRL + 1, 0x1F);
        self.w8(SECONDARY_CCA_CTRL, 0x3);
        self.set32(BB_PAD_CTRL, 1 << 20 | 1 << 24, 0);
        self.set32(GPIO_MUXCFG, 1 << 4, 1 << 3);
        self.set32(LEDCFG0, 1 << 23, 1 << 24);
        self.set32(0x944, 0, 0x3);
        self.set32(0x930, 0xFF, 0x77);
        self.set32(PWR_DATA, 0, 1 << 11);
        self.set8(RXDMA_CONTROL, RXDMA_STOP, 0);
    }

    /// BB: Realtek tables, gain, crystal (r23bu_init_bb + r23b_init_bb_common).
    pub(super) fn init_bb(&self, crystal: u8, ant: Antennas) {
        self.set16(SYS_FUNC_EN, 0, FUNC_BBRSTB | FUNC_BB_GLB_RST | FUNC_DIO_RF);
        self.w8(RF_CTRL, RF_CTRL_ON);
        pause_us(10);
        self.w8(SYS_FUNC_EN, FUNC_ALL_PCIE);
        self.set32(LEDCFG0, 0, LEDCFG0_DPDT_SEL_EN);
        self.w8(AFE_XTAL_CTRL + 1, 0x80);
        // While the tables load, the antenna belongs to Bluetooth (Realtek does this for PCIe).
        self.w32(ANT_SEL, if ant.aux { ANT_SEL_S1_WIFI } else { ANT_SEL_S0_WIFI });
        for &(reg, val) in tables::BB.iter() {
            self.w32(reg as usize, val);
            pause_us(1);
        }
        for &val in tables::AGC.iter() {
            self.w32(OFDM0_AGCRSSITABLE, val);
            pause_us(1);
        }
        self.w32(OFDM0_AGCCORE1, 0x6955_3422);
        pause_us(1);
        self.w32(OFDM0_AGCCORE1, 0x6955_3420);
        pause_us(1);
        self.w32(HSSI_PARAM2, 0x0039_0204);
        pause_us(1);
        let cap = (crystal & 0x3F) as u32;
        self.set32(MAC_PHY_CTRL, CRYSTAL_MASK, (cap | cap << 6) << CRYSTAL_SHIFT & CRYSTAL_MASK);
    }

    /// The RF part: Realtek's table, CCK and OFDM, oscillator calibration (r23b_init_rf).
    /// Returns RF_CHNLBW after the table — the channel register is computed from it.
    pub(super) fn init_rf(&self) -> u32 {
        let sw = self.r32(FPGA0_RFIFACESW);
        let kind = sw & 0x10;
        self.set32(FPGA0_RFIFACEOE, 0, 0x10_0000);
        pause_us(1);
        self.set32(FPGA0_RFIFACEOE, 0, 0x10);
        pause_us(1);
        self.set32(HSSI_PARAM2, HSSI2_ADDR_LENGTH, 0);
        pause_us(1);
        self.set32(HSSI_PARAM2, HSSI2_DATA_LENGTH, 0);
        pause_us(1);
        for &(reg, val) in tables::RF.iter() {
            // Addresses above 0xF8 are not registers but "wait": Realtek uses 50 ms.
            if reg > 0xF8 {
                sleep_ms(50);
                continue;
            }
            self.rf_write(reg, val);
            pause_us(1);
        }
        let sw = self.r32(FPGA0_RFIFACESW);
        self.w32(FPGA0_RFIFACESW, sw & !0x10 | kind);
        let chnlbw = self.rf_read(RF_CHNLBW);
        self.set32(FPGA0_RFMOD, 0, RFMOD_CCK_EN);
        self.set32(FPGA0_RFMOD, 0, RFMOD_OFDM_EN);
        // LCK.
        self.rf_write(RF_S0S1, 0xDFBE0);
        self.rf_write(RF_CHNLBW, 0x8C01);
        sleep_ms(200);
        self.rf_write(RF_S0S1, 0xDFFE0);
        chnlbw
    }

    /// A 2.4 GHz channel, 20 MHz bandwidth (r92e_set_chan + r92e_set_bw20).
    pub(super) fn set_channel(&self, chnlbw: u32, channel: u8) {
        self.rf_write(RF_CHNLBW, chnlbw & !CHNLBW_CHANNEL | channel as u32);
        self.set8(BWOPMODE, 0, BWOPMODE_20MHZ);
        self.set16(WMAC_TRXPTCL_CTL, 0x180, 0);
        self.w8(DATA_SEC, 0);
        self.set32(FPGA0_RFMOD, RFMOD_40MHZ, 0);
        self.set32(FPGA1_RFMOD, RFMOD_40MHZ, 0);
        self.rf_set(RF_CHNLBW, CHNLBW_BW, CHNLBW_BW);
        self.set32(OFDM0_TXPSEUDONOISEWGT, 0xC000_0000, 0);
    }

    /// Prepares the MAC to work as a station in a network: stores the cell id (BSSID), sets the
    /// "infrastructure" role and allows data frames to be received. During a scan RCR only let
    /// management frames through — that is why the first handshake message did not arrive on
    /// connect (it comes as a data frame).
    pub(super) fn join_mac(&self, bssid: &[u8; 6]) {
        const RCR_ADF: u32 = 0x0000_0800;
        const MSR_INFRA: u8 = 0x02;
        const BSSID: usize = 0x618;
        for (i, &b) in bssid.iter().enumerate() {
            self.w8(BSSID + i, b);
        }
        self.set8(MSR, MSR_MASK, MSR_INFRA);
        self.set32(RCR, 0, RCR_ADF);
    }

    /// Returns the MAC to the scanning state: no link, only management frames pass again
    /// (so that after a failed connect scanning works as before).
    pub(super) fn leave_mac(&self) {
        const RCR_ADF: u32 = 0x0000_0800;
        self.set8(MSR, MSR_MASK, MSR_NOLINK);
        self.set32(RCR, RCR_ADF, 0);
    }

    /// Writes the CCMP keys to the card's security table (CAM) and enables hardware
    /// encryption. The pairwise key goes to slot 4, the card finds it by the access point's address;
    /// the group key goes to the slot with its own number, for broadcast frames. The CAM format and
    /// write order follow rtwn (if_rtwn_cam.c, r92c): CTL0 is written last, and it is what
    /// makes the entry valid.
    pub(super) fn install_keys(&self, pairwise: &[u8; 16], group: (u8, [u8; 16]), bssid: &[u8; 6]) {
        const SECCFG_FULL: u16 = 0x00CF; // TX/RX default key (uc+bc), TX encrypt, RX decrypt
        self.cam_entry(4, 0, bssid, pairwise);
        self.cam_entry(group.0, group.0, &[0xFF; 6], &group.1);
        self.w16(SECCFG, SECCFG_FULL);
    }

    /// A new group key after the access point renewed it.
    pub(super) fn install_group_key(&self, group: (u8, [u8; 16])) {
        self.cam_entry(group.0, group.0, &[0xFF; 6], &group.1);
    }

    fn cam_entry(&self, slot: u8, keyid: u8, mac: &[u8; 6], key: &[u8; 16]) {
        const CAM_VALID: u32 = 0x8000;
        const CAM_ALGO_AES: u32 = 4;
        let base = slot as u32 * 8;
        self.cam_write(base + 6, 0);
        self.cam_write(base + 7, 0);
        for i in 0..4u32 {
            let k = i as usize * 4;
            self.cam_write(base + 2 + i, u32::from_le_bytes([key[k], key[k + 1], key[k + 2], key[k + 3]]));
        }
        self.cam_write(base + 1, u32::from_le_bytes([mac[2], mac[3], mac[4], mac[5]]));
        let maclo = u16::from_le_bytes([mac[0], mac[1]]) as u32;
        self.cam_write(base, CAM_ALGO_AES << 2 | (keyid as u32 & 0x3) | maclo << 16 | CAM_VALID);
    }

    fn cam_write(&self, addr: u32, data: u32) {
        const CAMWRITE: usize = 0x674;
        const CAMCMD_WRITE: u32 = 0x0001_0000;
        self.w32(CAMWRITE, data);
        self.w32(CAMCMD, CAMCMD_POLLING | CAMCMD_WRITE | (addr & 0xFFFF));
        self.wait32(CAMCMD, CAMCMD_POLLING, 0, 10_000);
    }

    pub(super) fn channel(&self) -> u8 {
        (self.rf_read(RF_CHNLBW) & CHNLBW_CHANNEL) as u8
    }

    /// The key table is empty, encryption is in software; antenna; sequence numbers; retries.
    pub(super) fn init_misc(&self) {
        self.w32(CAMCMD, CAMCMD_POLLING | CAMCMD_CLR);
        self.w16(SECCFG, 0);
        // Antenna (r23b_init_antsel).
        self.set32(BB_PAD_CTRL, 0x110_0000, 0);
        self.set32(GPIO_MUXCFG, 0x10, 0x8);
        self.set32(LEDCFG0, 0x40_0000, 0x80_0000);
        self.set32(0x944, 0, 0x3);
        self.set32(0x930, 0xFF, 0x77);
        self.set32(PWR_DATA, 0, 0x800);
        self.w8(HWSEQ_CTRL, TX_QUEUE_ALL);
        self.w32(BAR_MODE_CTRL, 0x0201_FFFF);
        self.w8(NAV_UPPER, 0);
        self.set8(GPIO_MUXCFG, GPIO_MUXCFG_ENBT, 0);
        for i in 0..8 {
            self.w8(DARFRC + i, i as u8 + 1);
            self.w8(RARFRC + i, i as u8 + 1);
        }
    }

    /// RF oscillator calibration (r23b_lc_calib).
    fn lc_calib(&self) {
        let txmode = self.r8(OFDM1_LSTF + 3);
        let mut ac = 0;
        if txmode & 0x70 != 0 {
            self.w8(OFDM1_LSTF + 3, txmode & !0x70);
            ac = self.rf_read(RF_AC);
            self.rf_write(RF_AC, ac & !RF_AC_MODE | RF_AC_STANDBY);
        } else {
            self.w8(TXPAUSE, TX_QUEUE_ALL);
        }
        self.rf_write(RF_S0S1, 0xDFBE0);
        self.rf_set(RF_CHNLBW, 0, CHNLBW_LCSTART);
        sleep_ms(100);
        self.rf_write(RF_S0S1, 0xDFFE0);
        if txmode & 0x70 != 0 {
            self.w8(OFDM1_LSTF + 3, txmode);
            self.rf_write(RF_AC, ac);
        } else {
            self.w8(TXPAUSE, 0);
        }
    }

    /// Final steps: calibration, radio on, coexistence with Bluetooth.
    /// A single-antenna module is set up like r23b_post_init (FreeBSD), a two-antenna one like
    /// Realtek does for two antennas: Wi-Fi has its own path S1 (or S0), the Wi-Fi mask is cleared, BT
    /// gets the air through the arbiter (PTA), TDMA is off.
    pub(super) fn post_init(&self, box_no: &mut u8, ant: Antennas) -> Result<(), String> {
        self.w32(RFPATH_SWITCH, 0x1);
        self.set8(TX_RPT_CTRL, 0, TX_RPT2_ENA);
        self.w8(TX_RPT_MACID_MAX, 2);
        self.w16(TX_RPT_TIME, 0xCDF0);
        self.lc_calib();
        self.w8(0x790, self.r8(0x790) & 0xC0 | 0x5);
        self.set8(GPIO_MUXCFG, 0, GPIO_MUXCFG_ENBT);
        if ant.two {
            self.w8(0x778, 0x3);
            self.set8(PWR_DATA + 1, 0, 0x8);
            self.w8(0x974, 0xFF);
            self.set8(0x944, 0, 0x3);
            self.w8(0x930, 0x77);
            self.set8(BB_PAD_CTRL + 3, 0, 0x20);
            // While configuring, the air belongs to Bluetooth.
            self.h2c(box_no, H2C_BT_GRANT, &[1])?;
            self.w32(ANT_SEL, 0);
            self.rf_write(RF_IQADJ_G0, 0);
            // The antenna is "reversed" if Wi-Fi is on the auxiliary path; the switch is internal.
            self.h2c(box_no, H2C_ANT_INV, &[ant.aux as u8, 0])?;
            self.set32(LEDCFG0, 1 << 24, 1 << 23);
            self.set8(BB_PAD_CTRL, 0x1, 0);
            self.w16(ANT_SEL, if ant.aux { ANT_SEL_S0_WIFI as u16 } else { ANT_SEL_S1_WIFI as u16 });
            self.coex_table(0x5555_5555, 0x5555_5555, 0x00FF_FFFF, 0x03);
            self.w8(0x76E, 0x4);
            self.h2c(box_no, H2C_PS_TDMA, &[0, 0, 0, 0x48, 0])?;
            // The air is granted by the arbiter.
            self.h2c(box_no, H2C_BT_GRANT, &[0])?;
            self.rf_write(RF_IQADJ_G0, 0);
        } else {
            self.w8(0x778, 0x1);
            self.rf_write(RF_IQADJ_G0, 0x780);
            self.bt_write(box_no, 0x3C, 0x15)?;
            self.h2c(box_no, H2C_BT_GRANT, &[0])?;
            self.w8(0x76E, 0x4);
            self.set8(BB_PAD_CTRL + 3, 0, 0x20);
            self.set8(PWR_DATA + 1, 0, 0x8);
            self.w8(0x974, 0xFF);
            self.set8(0x944, 0, 0x3);
            self.w8(0x930, 0x77);
            self.set32(LEDCFG0, 0x100_0000, 0x80_0000);
            self.set8(BB_PAD_CTRL, 0x1, 0);
            self.h2c(box_no, H2C_ANT_INV, &[1, 0])?;
            self.w32(ANT_SEL, 0x80);
            self.h2c(box_no, H2C_PS_TDMA, &[0x8, 0, 0, 0, 0])?;
            self.coex_table(0x5555_5555, 0x5555_5555, 0x00FF_FFFF, 0x03);
        }
        self.h2c(box_no, H2C_BT_INFO, &[1])?;
        self.h2c(box_no, H2C_BT_WLANACT, &[0])?;
        self.set32(FWHW_TXQ_CTRL, 0, FWHW_TXQ_ACK_MGNT);
        Ok(())
    }

    /// The Wi-Fi/Bluetooth arbiter table.
    fn coex_table(&self, a: u32, b: u32, c: u32, d: u8) {
        self.w32(BTCOEX_TABLE, a);
        self.w32(BTCOEX_TABLE + 4, b);
        self.w32(BTCOEX_TABLE + 8, c);
        self.w8(BTCOEX_TABLE + 12, d);
    }

    /// Counter of received frames of one type (reset with `clear_counters`).
    pub(super) fn counter(&self, kind: u32) -> u32 {
        self.set32(RXERR_RPT, 0xF000_0000, kind << 28);
        self.r32(RXERR_RPT) & COUNT_MASK
    }

    pub(super) fn clear_counters(&self) {
        self.set32(RXERR_RPT, 0, COUNT_RESET);
        self.set32(RXERR_RPT, COUNT_RESET, 0);
    }

    /// State for the log.
    pub(super) fn dump(&self) -> String {
        format!(
            "CR {:04x}, RCR {:08x}, MSR {:02x}, TRXDMA {:04x}, PCIE_CTRL {:08x}, RX_DESA {:08x}, HISR {:08x}/{:08x}, RXFLTMAP {:04x}, TXPAUSE {:02x}, ch {}",
            self.r16(CR),
            self.r32(RCR),
            self.r8(MSR),
            self.r16(TRXDMA_CTRL),
            self.r32(PCIE_CTRL_REG),
            self.r32(RX_DESA),
            self.r32(HISR),
            self.r32(HISRE),
            self.r16(RXFLTMAP0),
            self.r8(TXPAUSE),
            self.channel()
        )
    }
}
