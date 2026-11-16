//! AC97 (Intel ICH): the mixer is the NAM ports, DMA the NABM ports (the PCM out block at 0x10).

use super::{BUFFERS, FRAMES, Hw, RATE};
use crate::arch::{inb, inw, outb, outl, outw};
use crate::interrupts;
use crate::kprintln;
use crate::pci;
use alloc::boxed::Box;

const NAM_RESET: u16 = 0x00;
const NAM_MASTER: u16 = 0x02;
const NAM_PCM: u16 = 0x18;
const NAM_EXT_CTRL: u16 = 0x2A;
const NAM_FRONT_RATE: u16 = 0x2C;
const PO_BDBAR: u16 = 0x10;
const PO_CIV: u16 = 0x14;
const PO_LVI: u16 = 0x15;
const PO_SR: u16 = 0x16;
const PO_CR: u16 = 0x1B;
const GLOB_CNT: u16 = 0x2C;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Bd {
    addr: u32,
    samples: u16,
    flags: u16,
}

#[repr(C, align(8))]
struct BdList([Bd; BUFFERS]);

pub struct Ac97 {
    nabm: u16,
    list: Box<BdList>,
}

pub fn probe(devices: &[pci::Device]) -> Option<Ac97> {
    let dev = devices.iter().find(|d| d.vendor == 0x8086 && matches!(d.device, 0x2415 | 0x24C5 | 0x24D5 | 0x266E | 0x27DE))?;
    dev.write(0x04, dev.read(0x04) | 0b101); // I/O ports + DMA
    let (nam, nabm) = (dev.bar(0) as u16, dev.bar(1) as u16);

    // A cold codec reset, the volume to the maximum, 48 kHz.
    outl(nabm + GLOB_CNT, 0x2);
    let t = interrupts::ticks();
    while interrupts::ticks() < t + 2 {
        core::hint::spin_loop();
    }
    outw(nam + NAM_RESET, 1);
    outw(nam + NAM_MASTER, 0);
    outw(nam + NAM_PCM, 0x0808);
    outw(nam + NAM_EXT_CTRL, inw(nam + NAM_EXT_CTRL) | 1);
    outw(nam + NAM_FRONT_RATE, RATE as u16);
    let hw = Ac97 { nabm, list: Box::new(BdList([Bd::default(); BUFFERS])) };
    hw.reset();
    kprintln!("audio: AC97 at ports {nam:#x}/{nabm:#x}, {RATE} Hz");
    Some(hw)
}

impl Ac97 {
    fn reset(&self) {
        outb(self.nabm + PO_CR, 0x02);
        while inb(self.nabm + PO_CR) & 0x02 != 0 {
            core::hint::spin_loop();
        }
    }
}

impl Hw for Ac97 {
    fn start(&mut self, buffers: &[u64], last: usize) {
        self.reset();
        for (bd, &addr) in self.list.0.iter_mut().zip(buffers) {
            *bd = Bd { addr: addr as u32, samples: (FRAMES * 2) as u16, flags: 0 };
        }
        outl(self.nabm + PO_BDBAR, self.list.0.as_ptr() as u32);
        outb(self.nabm + PO_LVI, last as u8);
        outb(self.nabm + PO_CR, 0x01);
    }

    fn stop(&mut self) {
        outb(self.nabm + PO_CR, 0x00);
    }

    fn position(&self) -> usize {
        inb(self.nabm + PO_CIV) as usize
    }

    fn queued(&mut self, i: usize) {
        outb(self.nabm + PO_LVI, i as u8);
    }

    fn underrun(&mut self) -> bool {
        if inw(self.nabm + PO_SR) & 0x01 != 0 {
            outw(self.nabm + PO_SR, 0x1C);
            return true;
        }
        false
    }
}
