//! The CMOS RTC — the real-time clock. By itself it does not know what it holds:
//! universal or local time; the `time` module decides.

use crate::arch::{inb, outb};
use crate::sync::IrqLock;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Time {
    pub hour: u8,
    pub min: u8,
    pub sec: u8,
    pub day: u8,
    pub month: u8,
    pub year: u16,
}

/// The CMOS ports: the register number and its value. The pair of accesses must not be split.
static CMOS: IrqLock<()> = IrqLock::new(());

const REG_A: u8 = 0x0A;
const REG_B: u8 = 0x0B;
const A_UPDATING: u8 = 0x80;
/// The frequency divider in register A: reset (the clock stands still) and the normal run.
const A_DIVIDER: u8 = 0x70;
const A_DIVIDER_RESET: u8 = 0x60;
const A_DIVIDER_RUN: u8 = 0x20;
const B_SET: u8 = 0x80;
const B_BINARY: u8 = 0x04;
const B_24H: u8 = 0x02;
const PM: u8 = 0x80;

fn reg(r: u8) -> u8 {
    outb(0x70, r);
    inb(0x71)
}

fn put(r: u8, v: u8) {
    outb(0x70, r);
    outb(0x71, v);
}

fn bcd(v: u8) -> u8 {
    (v & 0x0F) + (v >> 4) * 10
}

fn to_bcd(v: u8) -> u8 {
    (v / 10) << 4 | v % 10
}

/// What the clock holds now.
pub fn read() -> Time {
    let _cmos = CMOS.lock();
    // Wait while the RTC is not updating its registers.
    for _ in 0..10_000 {
        if reg(REG_A) & A_UPDATING == 0 {
            break;
        }
    }
    let (mut sec, mut min, mut hour) = (reg(0x00), reg(0x02), reg(0x04));
    let (mut day, mut month, mut year) = (reg(0x07), reg(0x08), reg(0x09));
    let b = reg(REG_B);
    if b & B_BINARY == 0 {
        sec = bcd(sec);
        min = bcd(min);
        hour = bcd(hour & 0x7F) | (hour & PM);
        day = bcd(day);
        month = bcd(month);
        year = bcd(year);
    }
    if b & B_24H == 0 {
        // 12-hour mode: midnight and noon are written as 12.
        let pm = hour & PM != 0;
        hour = (hour & 0x7F) % 12 + if pm { 12 } else { 0 };
    }
    Time { hour: hour & 0x7F, min, sec, day, month, year: 2000 + year as u16 }
}

/// Writes the time to the clock. The divider stops during the write, and after
/// it restarts the first second change comes half a second later: this way the clock can
/// be set more precisely than a second if this is called in the middle of second `t`.
pub fn write(t: Time) {
    let _cmos = CMOS.lock();
    let (a, b) = (reg(REG_A), reg(REG_B));
    put(REG_A, (a & !A_DIVIDER) | A_DIVIDER_RESET);
    put(REG_B, b | B_SET);
    let mut hour = t.hour;
    if b & B_24H == 0 {
        let pm = hour >= 12;
        hour = if hour % 12 == 0 { 12 } else { hour % 12 };
        if b & B_BINARY == 0 {
            hour = to_bcd(hour);
        }
        if pm {
            hour |= PM;
        }
    } else if b & B_BINARY == 0 {
        hour = to_bcd(hour);
    }
    let code = |v: u8| if b & B_BINARY == 0 { to_bcd(v) } else { v };
    put(0x00, code(t.sec));
    put(0x02, code(t.min));
    put(0x04, hour);
    put(0x06, code(crate::time::weekday(&t) as u8 + 1));
    put(0x07, code(t.day));
    put(0x08, code(t.month));
    put(0x09, code((t.year % 100) as u8));
    put(REG_B, b & !B_SET);
    put(REG_A, (a & !A_DIVIDER) | A_DIVIDER_RUN);
}
