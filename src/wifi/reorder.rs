//! Block receive (A-MPDU). The access point sends frames in a burst and retransmits only the lost ones,
//! so the sequence numbers arrive out of order. TCP would count every hole as a loss: frames here
//! wait until the firmware says through NSSN that everything up to this number has arrived.

use alloc::vec::Vec;

const NUMBER_MASK: u16 = 0xFFF;
const HALF: u16 = 0x800;

/// Number `a` comes before `b`, taking the wrap through zero into account.
fn before(a: u16, b: u16) -> bool {
    a != b && a.wrapping_sub(b) & NUMBER_MASK >= HALF
}

fn next(number: u16) -> u16 {
    (number + 1) & NUMBER_MASK
}

/// What the firmware said about a received frame.
pub(super) struct Mark {
    pub(super) number: u16,
    /// Everything before this number has been received or will not come anymore.
    pub(super) nssn: u16,
    /// The number is already beyond the window.
    pub(super) old: bool,
    /// A part of an A-MSDU followed by more parts: its NSSN cannot be trusted.
    pub(super) partial: bool,
}

/// A block receive agreement for one TID.
pub(super) struct Session {
    pub(super) tid: u8,
    pub(super) baid: u8,
    head: u16,
    slots: Vec<Vec<Vec<u8>>>,
    stored: usize,
    started: bool,
    /// The tick since which frames wait in the buffer.
    since: u64,
}

impl Session {
    pub(super) fn new(tid: u8, baid: u8, first: u16, window: usize) -> Self {
        Self { tid, baid, head: first & NUMBER_MASK, slots: (0..window).map(|_| Vec::new()).collect(), stored: 0, started: false, since: 0 }
    }

    /// A new frame: frames ready for release go to `out`. There may be no frame
    /// (the handshake is handled separately), but its number is still counted.
    pub(super) fn receive(&mut self, frame: Option<Vec<u8>>, mark: &Mark, now: u64, out: &mut Vec<Vec<u8>>) {
        if !self.started {
            if mark.old {
                out.extend(frame);
                return;
            }
            self.started = true;
        }
        if mark.old {
            return;
        }
        if self.stored == 0 && before(mark.number, mark.nssn) {
            if !mark.partial {
                self.head = mark.nssn;
            }
            out.extend(frame);
            return;
        }
        if self.stored == 0 && mark.number == self.head {
            if !mark.partial {
                self.head = next(self.head);
            }
            out.extend(frame);
            return;
        }
        if let Some(frame) = frame {
            let slot = mark.number as usize % self.slots.len();
            self.slots[slot].push(frame);
            if self.stored == 0 {
                self.since = now;
            }
            self.stored += 1;
        }
        if !mark.partial {
            self.release(mark.nssn, now, out);
        }
    }

    /// Releases everything before `nssn`.
    pub(super) fn release(&mut self, nssn: u16, now: u64, out: &mut Vec<Vec<u8>>) {
        let before_release = self.stored;
        while before(self.head, nssn) {
            let slot = self.head as usize % self.slots.len();
            self.stored -= self.slots[slot].len();
            out.append(&mut self.slots[slot]);
            self.head = next(self.head);
        }
        if self.stored != before_release {
            self.since = now;
        }
    }

    /// The frame never arrived: release what there is, across the hole.
    pub(super) fn expire(&mut self, oldest: u64, now: u64, out: &mut Vec<Vec<u8>>) {
        if self.stored == 0 || self.since > oldest {
            return;
        }
        let mut last = None;
        for step in 0..self.slots.len() as u16 {
            let number = (self.head + step) & NUMBER_MASK;
            let slot = number as usize % self.slots.len();
            if !self.slots[slot].is_empty() {
                out.append(&mut self.slots[slot]);
                last = Some(number);
            }
        }
        if let Some(number) = last {
            self.head = next(number);
        }
        self.stored = 0;
        self.since = now;
    }
}

#[derive(Clone, Copy)]
struct Step(u16, u16, bool);

pub(super) fn self_test() -> bool {
    let mark = |s: Step| Mark { number: s.0, nssn: s.1, old: s.2, partial: false };
    let frame = |n: u8| Some(alloc::vec![n]);
    let order = |out: &Vec<Vec<u8>>| out.iter().map(|f| f[0]).collect::<Vec<u8>>();
    let mut session = Session::new(0, 1, 4090, 64);
    let mut out = Vec::new();
    // In a row, across the 4095 → 0 wrap.
    session.receive(frame(1), &mark(Step(4090, 4091, false)), 0, &mut out);
    session.receive(frame(2), &mark(Step(4091, 4092, false)), 0, &mut out);
    // A hole at 4092: wait, then the firmware releases everything at once.
    session.receive(frame(4), &mark(Step(4093, 4092, false)), 0, &mut out);
    session.receive(frame(5), &mark(Step(4094, 4092, false)), 0, &mut out);
    let held = order(&out) == [1, 2];
    session.receive(frame(3), &mark(Step(4092, 4095, false)), 0, &mut out);
    let filled = order(&out) == [1, 2, 3, 4, 5];
    // A stale number does not pass.
    session.receive(frame(9), &mark(Step(4000, 4095, true)), 0, &mut out);
    let dropped = out.len() == 5;
    // A frame is lost for good: the time is up, release across the hole.
    session.receive(frame(7), &mark(Step(1, 4095, false)), 10, &mut out);
    session.expire(5, 20, &mut out);
    let waiting = out.len() == 5;
    session.expire(10, 20, &mut out);
    let expired = order(&out) == [1, 2, 3, 4, 5, 7] && session.head == 2;
    held && filled && dropped && waiting && expired && before(4095, 0) && !before(0, 4095)
}
