//! IQ calibration (IQK) of the RTL8723B receiver and transmitter.
//!
//! Without it the 8723B receive path is practically deaf: the RX mixer is not aligned, and the CCA threshold
//! stays wrong — exactly what we saw (0 frames, CCA 0, false alarm counters 0).
//! A port from FreeBSD rtwn `rtl8723b/r23b_calib.c` (© 2026 Ahmad Khalifa, BSD-2-Clause):
//! r23b_iq_calib and everything it calls. Only path A is computed (as the original does for PCIe).

use super::init::Antennas;
use super::{Chip, note, sleep_ms};
use alloc::format;

// BB registers (values from FreeBSD's r92c_reg.h / r23b_reg.h).
const FPGA0_IQK: usize = 0xE28;
const TX_IQK: usize = 0xE40;
const RX_IQK: usize = 0xE44;
const TX_IQK_TONE0: usize = 0xE30;
const TX_IQK_TONE1: usize = 0xE50;
const RX_IQK_TONE0: usize = 0xE34;
const RX_IQK_TONE1: usize = 0xE54;
const TX_IQK_PI0: usize = 0xE38;
const TX_IQK_PI1: usize = 0xE58;
const RX_IQK_PI0: usize = 0xE3C;
const RX_IQK_PI1: usize = 0xE5C;
const IQK_AGC_PTS: usize = 0xE48;
const IQK_AGC_RSP: usize = 0xE4C;
const TX_POWER_IQK_BEFORE0: usize = 0xE94;
const TX_POWER_IQK_AFTER0: usize = 0xE9C;
const RX_POWER_IQK_BEFORE0: usize = 0xEA4;
const RX_POWER_IQK_AFTER0: usize = 0xEAC;
const CCK0_AFESETTING: usize = 0xA04;
const OFDM0_TRXPATHENA: usize = 0xC04;
const OFDM0_TRMUXPAR: usize = 0xC08;
const FPGA0_RFIFACESW1: usize = 0x874;
const OFDM0_RXIQIMBALANCE0: usize = 0xC14;
const OFDM0_ECCATHRESHOLD: usize = 0xC4C;
const OFDM0_TXIQIMBALANCE0: usize = 0xC80;
const OFDM0_RXIQEXTANTA: usize = 0xCA0;
const TXPAUSE: usize = 0x522;
const BCN_CTRL0: usize = 0x550;
const BCN_CTRL1: usize = 0x551;
const BCN_CTRL_EN_BCN: u8 = 0x08;
const GPIO_MUXCFG: usize = 0x040;
const GPIO_MUXCFG_ENBT: u32 = 0x20;
const ANT_SEL: usize = 0x948;
const ANT_SEL_S0: u32 = 0x000;
const ANT_SEL_S1: u32 = 0x280;
const GNT_BT: usize = 0x764;

// RF registers.
const RF_WE_LUT: u8 = 0xEF;
const RF_RCK_OS: u8 = 0x30;
const RF_TXPA_G0: u8 = 0x31;
const RF_TXPA_G1: u8 = 0x32;

// All transmit queues except the beacon (AC|MGT|HIGH) — for the duration of the calibration.
const TXPAUSE_IQK: u8 = 0x0F | 0x10 | 0x20;
// Clearing the upper 24 bits of the IQK register (keep the low byte).
const KEEP_LOW_BYTE: u32 = 0xFFFF_FF00;
const TOLERANCE: i32 = 5;
// The firmware command "starting/finished calibration" (r23b_fw_cmd.h).
const CMD_WLAN_CALIB: u8 = 0x6D;

/// Registers saved before the calibration and restored after it.
const ADAA: [usize; 16] = [
    0x85C, 0xE6C, 0xE70, 0xE74, 0xE78, 0xE7C, 0xE80, 0xE84, 0xE88, 0xE8C, 0xED0, 0xED4, 0xED8,
    0xEDC, 0xEE0, 0xEEC,
];
const IQK_BB: [usize; 9] = [0xC04, 0xC08, 0x874, 0xB68, 0xB6C, 0x870, 0x860, 0x864, 0xA04];

#[derive(Clone, Copy, Default)]
struct IqkResult {
    before: i32,
    after: i32,
}

#[derive(Clone, Copy)]
enum Stage {
    Tx,
    TxRx,
    Rx,
}

struct RegVals {
    adaa: [u32; 16],
    iqk_bb: [u32; 9],
    txpause: u8,
    bcn_ctrl: [u8; 2],
    gpio_muxcfg: u32,
}

/// The calibration result field: bits [25:16].
fn ms(v: u32) -> u32 {
    (v & 0x03FF_0000) >> 16
}

/// A signed 10-bit value as u32 (like the R23B_IQK_SIGN macro): for a bit-exact
/// match with the original's unsigned arithmetic when writing the corrections.
fn sign(r: i32) -> u32 {
    if r & 0x200 != 0 {
        0xFFFF_FC00 | r as u32
    } else {
        r as u32
    }
}

impl Chip {
    /// Set bits in an RF register (clear is always 0 for us; init's own rf_set is private).
    fn rf_or(&self, addr: u8, set: u32) {
        self.rf_write(addr, self.rf_read(addr) | set);
    }

    fn iqk_chain(&self, stage: Stage, ant_sel: u32) {
        let rf_path = self.r32(ANT_SEL);
        self.set32(FPGA0_IQK, KEEP_LOW_BYTE, 0);
        self.rf_or(RF_WE_LUT, 0x80000);
        match stage {
            Stage::Tx => {
                self.rf_write(RF_RCK_OS, 0x20000);
                self.rf_write(RF_TXPA_G0, 0x0003F);
                self.rf_write(RF_TXPA_G1, 0xC7F87);
                self.w32(TX_IQK, 0x0100_7C00);
            }
            Stage::TxRx => {
                self.rf_write(RF_RCK_OS, 0x30000);
                self.rf_write(RF_TXPA_G0, 0x0001F);
                self.rf_write(RF_TXPA_G1, 0xF7FB7);
                self.w32(TX_IQK, 0x0100_7C00);
            }
            Stage::Rx => {
                self.rf_write(RF_RCK_OS, 0x30000);
                self.rf_write(RF_TXPA_G0, 0x0001F);
                self.rf_write(RF_TXPA_G1, 0xF7D77);
                self.rf_write(0xDF, 0xF80);
                self.rf_write(0x55, 0x4021F);
            }
        }
        self.w32(RX_IQK, 0x0100_4800);
        match stage {
            Stage::Tx | Stage::TxRx => {
                self.w32(TX_IQK_TONE0, 0x1800_8C1C);
                self.w32(RX_IQK_TONE0, 0x3800_8C1C);
            }
            Stage::Rx => {
                self.w32(TX_IQK_TONE0, 0x3800_8C1C);
                self.w32(RX_IQK_TONE0, 0x1800_8C1C);
            }
        }
        self.w32(TX_IQK_TONE1, 0x3800_8C1C);
        self.w32(RX_IQK_TONE1, 0x3800_8C1C);
        match stage {
            Stage::Tx => {
                self.w32(TX_IQK_PI0, 0x8214_03EA);
                self.w32(RX_IQK_PI0, 0x2811_0000);
            }
            Stage::TxRx => {
                self.w32(TX_IQK_PI0, 0x8216_0FF0);
                self.w32(RX_IQK_PI0, 0x2811_0000);
            }
            Stage::Rx => {
                self.w32(TX_IQK_PI0, 0x8211_0000);
                self.w32(RX_IQK_PI0, 0x2816_001F);
            }
        }
        self.w32(TX_IQK_PI1, 0x8211_0000);
        self.w32(RX_IQK_PI1, 0x2811_0000);
        match stage {
            Stage::Tx => self.w32(IQK_AGC_RSP, 0x0046_2911),
            Stage::TxRx => self.w32(IQK_AGC_RSP, 0x0046_A911),
            Stage::Rx => self.w32(IQK_AGC_RSP, 0x0046_A8D1),
        }
        // Enter IQK mode.
        self.set32(FPGA0_IQK, KEEP_LOW_BYTE, 0x8080_0000);
        self.w32(ANT_SEL, ant_sel);
        self.w32(GNT_BT, 0x0000_0800); // GNT_BT = 0
        // LO and IQ calibration in one go.
        self.w32(IQK_AGC_PTS, 0xF900_0000);
        self.w32(IQK_AGC_PTS, 0xF800_0000);
        sleep_ms(20);
        self.w32(ANT_SEL, rf_path);
        self.w32(GNT_BT, 0x0000_1800); // GNT_BT = 1
        self.set32(FPGA0_IQK, KEEP_LOW_BYTE, 0); // leave IQK mode
    }

    fn iqk_tx_result(&self) -> Option<IqkResult> {
        if self.r32(RX_POWER_IQK_AFTER0) & 0x1000_0000 != 0 {
            return None;
        }
        let before = ms(self.r32(TX_POWER_IQK_BEFORE0));
        if before == 0x142 || before >= 0x110 || before <= 0xF0 {
            return None;
        }
        let after = ms(self.r32(TX_POWER_IQK_AFTER0));
        if after == 0x42 {
            return None;
        }
        let adj = if after & 0x200 != 0 { 0x400 - after } else { after };
        (adj < 0xF).then_some(IqkResult { before: before as i32, after: after as i32 })
    }

    fn iqk_rx_result(&self) -> Option<IqkResult> {
        let after = ms(self.r32(RX_POWER_IQK_AFTER0));
        let before = ms(self.r32(RX_POWER_IQK_BEFORE0));
        self.set32(FPGA0_IQK, KEEP_LOW_BYTE, 0); // leave IQK mode
        self.rf_write(0xDF, 0x780);
        let adj = if after & 0x200 != 0 { 0x400 - after } else { after };
        if after & 0x0800_0000 != 0 {
            return None;
        }
        if before == 0x132 || before >= 0x110 || before <= 0xF0 {
            return None;
        }
        if after == 0x36 || adj >= 0xF {
            return None;
        }
        Some(IqkResult { before: before as i32, after: after as i32 })
    }

    fn iqk_save(&self) -> RegVals {
        let mut v = RegVals {
            adaa: [0; 16],
            iqk_bb: [0; 9],
            txpause: 0,
            bcn_ctrl: [0; 2],
            gpio_muxcfg: 0,
        };
        for (i, &reg) in ADAA.iter().enumerate() {
            v.adaa[i] = self.r32(reg);
        }
        v.txpause = self.r8(TXPAUSE);
        v.bcn_ctrl = [self.r8(BCN_CTRL0), self.r8(BCN_CTRL1)];
        v.gpio_muxcfg = self.r32(GPIO_MUXCFG);
        for (i, &reg) in IQK_BB.iter().enumerate() {
            v.iqk_bb[i] = self.r32(reg);
        }
        v
    }

    fn iqk_restore(&self, v: &RegVals) {
        for (i, &reg) in ADAA.iter().enumerate() {
            self.w32(reg, v.adaa[i]);
        }
        self.w8(TXPAUSE, v.txpause);
        self.w8(BCN_CTRL0, v.bcn_ctrl[0]);
        self.w8(BCN_CTRL1, v.bcn_ctrl[1]);
        self.w32(GPIO_MUXCFG, v.gpio_muxcfg);
        for (i, &reg) in IQK_BB.iter().enumerate() {
            self.w32(reg, v.iqk_bb[i]);
        }
    }

    fn iqk_run(&self, ant_sel: u32, v: &RegVals) -> (IqkResult, IqkResult) {
        for &reg in ADAA.iter() {
            self.w32(reg, 0x01C0_0014);
        }
        // MAC settings: hold transmit and the beacon, clear ENBT.
        self.w8(TXPAUSE, TXPAUSE_IQK);
        self.w8(BCN_CTRL0, v.bcn_ctrl[0] & !BCN_CTRL_EN_BCN);
        self.w8(BCN_CTRL1, v.bcn_ctrl[1] & !BCN_CTRL_EN_BCN);
        self.w8(GPIO_MUXCFG, (v.gpio_muxcfg & !GPIO_MUXCFG_ENBT) as u8);
        // BB settings.
        self.set32(CCK0_AFESETTING, 0, 0x0F00_0000);
        self.w32(OFDM0_TRXPATHENA, 0x03A0_5600);
        self.w32(OFDM0_TRMUXPAR, 0x0008_00E4);
        self.w32(FPGA0_RFIFACESW1, 0x2220_4000);
        // IQK settings.
        self.set32(FPGA0_IQK, KEEP_LOW_BYTE, 0);
        self.rf_or(RF_WE_LUT, 0x80000);
        self.rf_write(RF_RCK_OS, 0x30000);
        self.rf_write(RF_TXPA_G0, 0x0001F);
        self.rf_write(RF_TXPA_G1, 0xF7FB7);
        self.rf_or(0xED, 0x20);
        self.rf_write(0x43, 0x60FBD);

        let mut tx = IqkResult::default();
        let mut rx = IqkResult::default();
        // Path A Tx calibration.
        for _ in 0..2 {
            self.iqk_chain(Stage::Tx, ant_sel);
            if let Some(r) = self.iqk_tx_result() {
                tx = r;
                self.set32(FPGA0_IQK, KEEP_LOW_BYTE, 0);
                break;
            }
        }
        // Path A Rx calibration.
        for _ in 0..2 {
            self.iqk_chain(Stage::TxRx, ant_sel);
            let Some(mid) = self.iqk_tx_result() else { continue };
            self.w32(TX_IQK, 0x8000_7C00 | (mid.before as u32) << 16 | mid.after as u32);
            self.iqk_chain(Stage::Rx, ant_sel);
            if let Some(r) = self.iqk_rx_result() {
                rx = r;
                break;
            }
        }
        self.set32(FPGA0_IQK, KEEP_LOW_BYTE, 0);
        (tx, rx)
    }

    /// IQ calibration: two runs, a third one if they disagree, pick a consistent pair and
    /// write the Tx/Rx corrections. Call after bringing up BB/RF and setting the channel.
    pub(super) fn iq_calib(&self, box_no: &mut u8, ant: Antennas) {
        let ant_sel = if ant.two { ANT_SEL_S1 } else { ANT_SEL_S0 };
        // Tell the firmware we are starting calibration (so its own does not interfere). If it fails — no matter.
        let _ = self.h2c(box_no, CMD_WLAN_CALIB, &[1]);
        let gnt_bt = self.r32(GNT_BT);

        let vals = self.iqk_save();
        let (tx0, rx0) = self.iqk_run(ant_sel, &vals);
        let (tx1, rx1) = self.iqk_run(ant_sel, &vals);
        self.iqk_restore(&vals);

        let mut tx_cand = None;
        let mut rx_cand = None;
        let mut ok = compare(tx0, rx0, tx1, rx1, &mut tx_cand, &mut rx_cand);
        if !ok {
            let (tx2, rx2) = self.iqk_run(ant_sel, &vals);
            self.iqk_restore(&vals);
            ok = compare(tx0, rx0, tx2, rx2, &mut tx_cand, &mut rx_cand)
                || compare(tx1, rx1, tx2, rx2, &mut tx_cand, &mut rx_cand);
        }
        let _ = ok;

        let tx = tx_cand.unwrap_or_default();
        let rx = rx_cand.unwrap_or_default();

        // Write the path A corrections only if the Tx IQK succeeded.
        if tx.before != 0 {
            let oldval = (self.r32(OFDM0_TXIQIMBALANCE0) >> 22) & 0x3FF;
            let val = sign(tx.before).wrapping_mul(oldval) >> 7;
            self.set32(OFDM0_TXIQIMBALANCE0, 0x3FF, (val >> 1) & 0x3FF);
            self.set32(OFDM0_ECCATHRESHOLD, 0x8000_0000, val << 31);

            let val = sign(tx.after).wrapping_mul(oldval) >> 7;
            self.set32(TX_POWER_IQK_BEFORE0, 0xF000_0000, (((val >> 1) & 0x3C0) >> 6) << 28);
            self.set32(OFDM0_TXIQIMBALANCE0, 0x3F_0000, ((val >> 1) & 0x3F) << 16);
            // In the original the set value (val<<29)&1 is always 0 — it simply clears bit 28.
            self.set32(OFDM0_ECCATHRESHOLD, 0x1000_0000, 0);

            if rx.before != 0 {
                self.set32(OFDM0_RXIQIMBALANCE0, 0x3FF, rx.before as u32);
                self.set32(OFDM0_RXIQIMBALANCE0, 0xFC00, (rx.after as u32 & 0x3F) << 10);
                self.set32(OFDM0_RXIQEXTANTA, 0xF000_0000, ((rx.after as u32 >> 6) & 0xF) << 28);
            }
        }

        self.w32(GNT_BT, gnt_bt);
        self.rf_or(RF_WE_LUT, 0x80000);
        self.rf_write(RF_RCK_OS, 0x18000);
        self.rf_write(RF_TXPA_G0, 0x0001F);
        self.rf_write(RF_TXPA_G0, 0xE6177);
        self.rf_or(0xED, 0x20);
        self.rf_write(0x43, 0x300BD);
        let _ = self.h2c(box_no, CMD_WLAN_CALIB, &[0]);

        note(format!(
            "IQK {}: tx {:#x}/{:#x}, rx {:#x}/{:#x}, ECCATHR {:08x} TXIQ {:08x} RXIQ {:08x}",
            if tx.before != 0 { "applied" } else { "failed (receiver may stay deaf)" },
            tx.before,
            tx.after,
            rx.before,
            rx.after,
            self.r32(OFDM0_ECCATHRESHOLD),
            self.r32(OFDM0_TXIQIMBALANCE0),
            self.r32(OFDM0_RXIQIMBALANCE0),
        ));
    }
}

/// Compares two pairs of results (r23b_iq_calib_compare_results): whether they are usable, and which
/// candidates to pick. The candidates are filled in place — like the pointers in the original.
fn compare(
    tx1: IqkResult,
    rx1: IqkResult,
    tx2: IqkResult,
    rx2: IqkResult,
    tx_cand: &mut Option<IqkResult>,
    rx_cand: &mut Option<IqkResult>,
) -> bool {
    let mut tx_good = false;
    if (tx1.before - tx2.before).abs() <= TOLERANCE
        && (sign(tx1.after) as i32).wrapping_sub(sign(tx2.after) as i32).abs() <= TOLERANCE
    {
        *tx_cand = Some(tx1);
        tx_good = true;
    }
    if (rx1.before - rx2.before).abs() > TOLERANCE {
        if tx_good && rx1.before + rx1.after == 0 {
            *tx_cand = Some(tx2);
        }
        return false;
    }
    if (sign(rx1.after) as i32).wrapping_sub(sign(rx2.after) as i32).abs() > TOLERANCE {
        return false;
    }
    *rx_cand = Some(rx1);
    tx_good
}
