#![allow(dead_code)]
//! Probing the antenna connection: the RTL8723BE has one radio path for Wi-Fi and Bluetooth, and
//! where the antenna goes on the board (S0/S1, the main or auxiliary connector, one or two
//! antennas) depends on the module. We try the variants and see where the air is heard.

use super::{Chip, Radio, note, sleep_ms};
use crate::{interrupts, task, wifi};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

const LEDCFG0: usize = 0x04C;
const ANT_SEL: usize = 0x948;
const H2C_ANT_INV: u8 = 0x65;

/// A connection variant: the ANT_SEL value, bits 23/24 of LEDCFG0 (who controls
/// the antenna switch), the "antenna reversed" firmware command.
#[derive(Clone, Copy)]
pub(super) struct Variant {
    pub name: &'static str,
    pub ant_sel: u32,
    /// true — Wi-Fi/BT control the switch (two antennas), false — DPDT_SEL (one).
    pub two: bool,
    pub inv: [u8; 2],
}

pub(super) const VARIANTS: [Variant; 6] = [
    Variant { name: "1ant PTA aux 0x080", ant_sel: 0x080, two: false, inv: [1, 0] },
    Variant { name: "1ant PTA main 0x200", ant_sel: 0x200, two: false, inv: [0, 0] },
    Variant { name: "1ant S0 0x000", ant_sel: 0x000, two: false, inv: [1, 0] },
    Variant { name: "1ant S1 0x280", ant_sel: 0x280, two: false, inv: [0, 0] },
    Variant { name: "2ant main 0x000", ant_sel: 0x000, two: true, inv: [1, 1] },
    Variant { name: "2ant aux 0x280", ant_sel: 0x280, two: true, inv: [0, 1] },
];

impl Chip {
    pub(super) fn apply(&self, v: &Variant, mailbox: &mut u8) {
        if v.two {
            self.set32(LEDCFG0, 1 << 23, 1 << 24);
            self.w32(0x974, 0x3FF);
            self.w8(0xCB4, 0x77);
        } else {
            self.set32(LEDCFG0, 1 << 24, 1 << 23);
            self.w8(0x974, 0xFF);
        }
        if let Err(e) = self.h2c(mailbox, H2C_ANT_INV, &v.inv) {
            note(e);
        }
        self.w32(ANT_SEL, v.ant_sel);
    }

    /// BB counters: how many times the OFDM and CCK receivers sensed a busy channel (CCA).
    pub(super) fn cca(&self) -> (u32, u32) {
        self.set32(0xC00, 0, 1 << 31);
        self.set32(0xD00, 0, 1 << 31);
        self.set32(0xA2C, 0, 1 << 12 | 1 << 14);
        let ofdm = self.r32(0xDA4) & 0xFFFF;
        let v = self.r32(0xA60);
        let cck = (v & 0xFF) << 8 | (v >> 8) & 0xFF;
        self.set32(0xC00, 1 << 31, 0);
        self.set32(0xD00, 1 << 31, 0);
        self.set32(0xA2C, 1 << 12 | 1 << 14, 0);
        (ofdm, cck)
    }

    pub(super) fn coex_regs(&self) -> String {
        format!(
            "0x4c {:08x}, 0x64 {:08x}, 0x67 {:02x}, 0x765 {:02x}, 0x76e {:02x}, 0x860 {:08x}, 0x870 {:08x}, 0x92c {:08x}, 0x930 {:08x}, 0x944 {:08x}, 0x948 {:08x}, 0x974 {:08x}, 0xcb4 {:08x}, 0x6c0 {:08x} {:08x} {:08x} {:08x}, 0xc50 {:08x}, RF 00 {:05x} 01 {:05x} 18 {:05x}; BB 800 {:08x} 880 {:08x} 884 {:08x} 88c {:08x} c04 {:08x} d04 {:08x} a00 {:08x}; 603 {:02x} 790 {:02x} 778 {:02x} 286 {:02x}",
            self.r32(0x4C),
            self.r32(0x64),
            self.r8(0x67),
            self.r8(0x765),
            self.r8(0x76E),
            self.r32(0x860),
            self.r32(0x870),
            self.r32(0x92C),
            self.r32(0x930),
            self.r32(0x944),
            self.r32(0x948),
            self.r32(0x974),
            self.r32(0xCB4),
            self.r32(0x6C0),
            self.r32(0x6C4),
            self.r32(0x6C8),
            self.r32(0x6CC),
            self.r32(0xC50),
            self.rf_read(0x00),
            self.rf_read(0x01),
            self.rf_read(0x18),
            self.r32(0x800),
            self.r32(0x880),
            self.r32(0x884),
            self.r32(0x88C),
            self.r32(0xC04),
            self.r32(0xD04),
            self.r32(0xA00),
            self.r8(0x603),
            self.r8(0x790),
            self.r8(0x778),
            self.r8(0x286)
        )
    }
}

/// Listens on a channel for `ticks` ticks: frames, beacons, CCA.
fn listen(chip: &Chip, radio: &mut Radio, channel: u8, ticks: u64) -> (u64, u64, u32, u32) {
    chip.set_channel(radio.chnlbw, channel);
    let (o0, c0) = chip.cca();
    let before = radio.rx.counts.frames;
    let mut beacons = 0;
    let end = interrupts::ticks() + ticks;
    loop {
        super::park_if_stopping();
        radio.rx.poll(|f| {
            if wifi::network_from(f.data, channel, -100, 0).is_some() {
                beacons += 1;
            }
        });
        if interrupts::ticks() >= end {
            break;
        }
        task::sleep_until(interrupts::ticks() + 1);
    }
    let (o1, c1) = chip.cca();
    (radio.rx.counts.frames - before, beacons, o1.wrapping_sub(o0) & 0xFFFF, c1.wrapping_sub(c0) & 0xFFFF)
}

/// Tries the variants, logs what is heard with each one, and returns the best.
pub(super) fn pick_antenna(chip: &Chip, radio: &mut Radio, mailbox: &mut u8) -> usize {
    note(format!("coex before: {}", chip.coex_regs()));
    let mut best = (0, 0u64);
    for (i, v) in VARIANTS.iter().enumerate() {
        chip.apply(v, mailbox);
        sleep_ms(20);
        let mut line: Vec<String> = Vec::new();
        let mut total = 0;
        for channel in [1u8, 6, 11] {
            let (frames, beacons, ofdm, cck) = listen(chip, radio, channel, interrupts::HZ);
            total += frames;
            line.push(format!("ch{channel} {frames}f/{beacons}b cca {ofdm}/{cck}"));
        }
        note(format!("antenna {}: {}", v.name, line.join(", ")));
        if total > best.1 {
            best = (i, total);
        }
    }
    chip.apply(&VARIANTS[best.0], mailbox);
    note(format!("antenna: using {}; coex now: {}", VARIANTS[best.0].name, chip.coex_regs()));
    best.0
}

/// Steps the Realtek driver performs for the PCIe module that FreeBSD's USB path
/// lacks. Applied one by one, listening on channel 6 after each.
pub(super) fn try_steps(chip: &Chip, radio: &mut Radio) -> usize {
    let raw = chip.efuse_byte(0x1FA).unwrap_or(0xFF);
    note(format!("efuse raw 1fa {raw:02x}; SYS_CFG+3 {:02x}, 0x286 {:02x}, 0x577 {:02x}, 0x08 {:04x}, 0x16 {:02x}, 0x40 {:08x}", chip.r8(0x0F3), chip.r8(0x286), chip.r8(0x577), chip.r16(0x08), chip.r8(0x16), chip.r32(0x40)));
    let steps: [(&str, &dyn Fn()); 6] = [
        ("baseline", &|| {}),
        ("pcie mac extras", &|| {
            chip.set8(0x008, 0, 0x08);
            chip.set8(0x041, 0x10, 0);
            chip.w8(0x577, 0x3);
            chip.w8(0x421, 0x1F);
            chip.set8(0x286, 0x04, 0);
            chip.set8(0x303, 0, 0x77);
            if chip.r8(0x0F3) & 1 != 0 {
                chip.set8(0x07C, 0, 0x40);
            }
        }),
        ("dpdt fixed board", &|| {
            chip.set32(0x064, 1 << 20 | 1 << 24, 0);
            chip.set32(0x040, 1 << 4, 1 << 3);
            chip.set32(0x04C, 1 << 23, 1 << 24);
            chip.set32(0x04C, 0, 1 << 23);
            chip.set32(0x944, 0, 0x3);
            chip.set32(0x930, 0xFF, 0x77);
            chip.set32(0x038, 0, 1 << 11);
        }),
        ("regulator and pa bias", &|| {
            if raw & 1 == 0 {
                let v = chip.rf_read(0x15);
                chip.rf_write(0x15, v & !0x0F | 0x05);
            }
            if raw & 0x10 == 0 {
                let v = chip.r8(0x16) & 0x0F;
                chip.w8(0x16, v | 0x80);
                pause();
                chip.w8(0x16, v | 0x90);
            }
        }),
        ("rf mode rx", &|| {
            let v = chip.rf_read(0x00);
            chip.rf_write(0x00, v & !0xF_0000 | 0x3_0000);
        }),
        ("rf_env by software", &|| {
            chip.set32(0x870, 0, 0x10);
            chip.set32(0x860, 0, 0x10);
        }),
    ];
    let mut first_alive = usize::MAX;
    for (i, (name, step)) in steps.iter().enumerate() {
        step();
        sleep_ms(20);
        let (frames, beacons, ofdm, cck) = listen(chip, radio, 6, interrupts::HZ);
        note(format!("step {name}: ch6 {frames}f/{beacons}b cca {ofdm}/{cck}; RF 00 {:05x}, 0x4c {:08x}, 0x64 {:08x}", chip.rf_read(0x00), chip.r32(0x4C), chip.r32(0x64)));
        if (frames > 0 || ofdm + cck > 0) && first_alive == usize::MAX {
            first_alive = i;
        }
    }
    first_alive
}

fn pause() {
    super::pause_us(10);
}

/// Wi-Fi/Bluetooth arbitration (PTA) variants: one after another, cumulatively, channel 6.
pub(super) fn try_coex(chip: &Chip, radio: &mut Radio, mailbox: &mut u8) {
    let raw = |chip: &Chip| format!("da0 {:08x} da4 {:08x} da8 {:08x} a58 {:08x} a5c {:08x} a60 {:08x} cf0 {:08x} 49d {:02x} 765 {:02x} 76e {:02x} 6cc {:08x}", chip.r32(0xDA0), chip.r32(0xDA4), chip.r32(0xDA8), chip.r32(0xA58), chip.r32(0xA5C), chip.r32(0xA60), chip.r32(0xCF0), chip.r8(0x49D), chip.r8(0x765), chip.r8(0x76E), chip.r32(0x6CC));
    note(format!("coex raw: {}", raw(chip)));
    let steps: [(&str, &dyn Fn(&mut u8)); 8] = [
        ("baseline", &|_| {}),
        ("wlan_act by PTA 0x76e=0c", &|_| chip.w8(0x76E, 0x0C)),
        ("gnt_bt sw low 0x765=08", &|_| chip.w8(0x765, 0x08)),
        ("table 0xaaaaaaaa", &|_| {
            chip.w32(0x6C0, 0xAAAA_AAAA);
            chip.w32(0x6C4, 0xAAAA_AAAA);
        }),
        ("table 0xffffffff", &|_| {
            chip.w32(0x6C0, 0xFFFF_FFFF);
            chip.w32(0x6C4, 0xFFFF_FFFF);
        }),
        ("table 0x00000000", &|_| {
            chip.w32(0x6C0, 0);
            chip.w32(0x6C4, 0);
        }),
        ("break table off 0x6cc=0", &|_| chip.w32(0x6CC, 0)),
        ("bt grant low again", &|m| {
            if let Err(e) = chip.h2c(m, 0x6E, &[0]) {
                note(e);
            }
        }),
    ];
    for (name, step) in steps.iter() {
        step(mailbox);
        sleep_ms(20);
        let (frames, beacons, ofdm, cck) = listen(chip, radio, 6, interrupts::HZ);
        note(format!("coex {name}: ch6 {frames}f/{beacons}b cca {ofdm}/{cck}; {}", raw(chip)));
    }
}

/// If the air is silent with the EFUSE configuration: variants on top of it, each separately
/// (the configuration is restored after it), channels 1, 6, 11 for a second each.
pub(super) fn fallback(chip: &Chip, radio: &mut Radio) {
    let base = (chip.r32(ANT_SEL), chip.rf_read(0x01), chip.r32(0x92C), chip.r8(0x765), chip.r32(LEDCFG0), chip.r8(0x76E));
    let restore = |chip: &Chip| {
        chip.w32(ANT_SEL, base.0);
        chip.rf_write(0x01, base.1);
        chip.w32(0x92C, base.2);
        chip.w8(0x765, base.3);
        chip.w32(LEDCFG0, base.4);
        chip.w8(0x76E, base.5);
    };
    let variants: [(&str, &dyn Fn()); 8] = [
        ("as configured", &|| {}),
        ("rf01=0x780", &|| chip.rf_write(0x01, 0x780)),
        ("ant_sel=0x280", &|| chip.w32(ANT_SEL, 0x280)),
        ("ant_sel=0x280 rf01=0x780", &|| {
            chip.w32(ANT_SEL, 0x280);
            chip.rf_write(0x01, 0x780);
        }),
        ("rx antenna aux 0x92c=2", &|| chip.w32(0x92C, 0x2)),
        ("gnt_bt sw low 0x765=08", &|| chip.w8(0x765, 0x08)),
        ("0x4c[24]=1 (ext switch)", &|| chip.set32(LEDCFG0, 1 << 23, 1 << 24)),
        ("wlan_act 0x76e=0c", &|| chip.w8(0x76E, 0x0C)),
    ];
    for (name, apply) in variants.iter() {
        apply();
        sleep_ms(20);
        let mut line: Vec<String> = Vec::new();
        for channel in [1u8, 6, 11] {
            let (frames, beacons, ofdm, cck) = listen(chip, radio, channel, interrupts::HZ);
            line.push(format!("ch{channel} {frames}f/{beacons}b cca {ofdm}/{cck}"));
        }
        note(format!("variant {name}: {}", line.join(", ")));
        restore(chip);
    }
}
