//! The xHCI USB controller (USB 1.1–3.x): command, event and transfer rings,
//! device addressing and control transfers. Works by polling (and MSI for events where available).

use crate::interrupts::{self, HZ};
use crate::kprintln;
use crate::pci;
use crate::task;
use alloc::alloc::{Layout, alloc_zeroed};
use alloc::collections::VecDeque;

pub const TRB_NORMAL: u32 = 1;
const TRB_SETUP: u32 = 2;
const TRB_DATA: u32 = 3;
const TRB_STATUS: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_DISABLE_SLOT: u32 = 10;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_EP: u32 = 12;
const TRB_EVALUATE_CTX: u32 = 13;
const TRB_RESET_EP: u32 = 14;
const TRB_STOP_EP: u32 = 15;
const TRB_SET_DEQUEUE: u32 = 16;
pub const EV_TRANSFER: u32 = 32;
const EV_COMMAND: u32 = 33;
pub const EV_PORT: u32 = 34;

pub const SUCCESS: u32 = 1;

/// The name of an xHCI completion code — for error messages.
pub fn code_name(code: u32) -> &'static str {
    match code {
        0 => "timeout",
        3 => "babble",
        4 => "transaction error",
        5 => "TRB error",
        6 => "stall",
        11 => "slot not enabled",
        13 => "short packet",
        17 => "parameter error",
        19 => "context state error",
        _ => "error",
    }
}
pub const SHORT_PACKET: u32 = 13;

const RING: usize = 256;

/// Memory for the device: zeroed, page-aligned, not crossing 64 KiB
/// (up to a page in size). Never freed: the rings live together with the controller.
pub fn dma(size: usize) -> *mut u8 {
    unsafe { alloc_zeroed(Layout::from_size_align(size.max(64), 4096).unwrap()) }
}

fn rd(p: *const u32) -> u32 {
    unsafe { p.read_volatile() }
}

fn wr(p: *mut u32, v: u32) {
    unsafe { p.write_volatile(v) }
}

#[derive(Clone, Copy, Debug)]
pub struct Trb(pub [u32; 4]);

impl Trb {
    pub fn kind(&self) -> u32 {
        (self.0[3] >> 10) & 0x3F
    }
    pub fn code(&self) -> u32 {
        self.0[2] >> 24
    }
    pub fn slot(&self) -> u8 {
        (self.0[3] >> 24) as u8
    }
    pub fn pointer(&self) -> u64 {
        self.0[0] as u64 | (self.0[1] as u64) << 32
    }
}

/// A ring we write to (commands or transfers): the last TRB is a link to the start.
pub struct Ring {
    base: *mut [u32; 4],
    index: usize,
    cycle: u32,
}

unsafe impl Send for Ring {}

impl Ring {
    pub fn new() -> Ring {
        let base = dma(RING * 16) as *mut [u32; 4];
        let r = Ring { base, index: 0, cycle: 1 };
        let link = unsafe { &mut *base.add(RING - 1) };
        link[0] = base as u64 as u32;
        link[1] = (base as u64 >> 32) as u32;
        r
    }

    pub fn addr(&self) -> u64 {
        self.base as u64
    }

    /// The address of the next TRB with the current cycle bit (for Set TR Dequeue after an error).
    pub fn dequeue(&self) -> u64 {
        unsafe { self.base.add(self.index) as u64 | self.cycle as u64 }
    }

    /// Puts a TRB; returns its address (the completion event is recognised by it).
    pub fn push(&mut self, param: u64, status: u32, control: u32) -> u64 {
        let slot = unsafe { self.base.add(self.index) };
        let p = slot as *mut u32;
        unsafe {
            wr(p, param as u32);
            wr(p.add(1), (param >> 32) as u32);
            wr(p.add(2), status);
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            wr(p.add(3), (control & !1) | self.cycle);
        }
        let addr = slot as u64;
        self.index += 1;
        if self.index == RING - 1 {
            // A link to the start, toggling the cycle bit.
            let link = unsafe { self.base.add(RING - 1) as *mut u32 };
            unsafe { wr(link.add(3), TRB_LINK << 10 | 1 << 1 | self.cycle) };
            self.index = 0;
            self.cycle ^= 1;
        }
        addr
    }
}

pub struct Xhci {
    op: usize,
    runtime: usize,
    doorbells: usize,
    pub ports: u32,
    pub ctx_size: usize,
    dcbaa: *mut u64,
    commands: Ring,
    events: *mut [u32; 4],
    event_index: usize,
    event_cycle: u32,
    /// Events that arrived while we were waiting for something else.
    pub pending: VecDeque<Trb>,
    /// The completion code of the last failed command or transfer (0 — a timeout).
    pub last_error: u32,
}

unsafe impl Send for Xhci {}

impl Xhci {
    fn op(&self, reg: usize) -> *mut u32 {
        (self.op + reg) as *mut u32
    }

    pub fn portsc(&self, port: u32) -> *mut u32 {
        self.op(0x400 + 0x10 * (port as usize - 1))
    }

    /// Finds the controller, takes it from the firmware, resets and starts it.
    pub fn init(dev: &pci::Device) -> Option<Xhci> {
        dev.enable_bus_master();
        let mmio = dev.mmio(0, 64 * 1024);
        let cap = |r: usize| rd((mmio + r) as *const u32);
        let caplength = cap(0) & 0xFF;
        let hcs1 = cap(0x04);
        let hcs2 = cap(0x08);
        let hcc1 = cap(0x10);
        let (max_slots, ports) = (hcs1 & 0xFF, hcs1 >> 24);
        let ctx_size = if hcc1 & 1 << 2 != 0 { 64 } else { 32 };

        // The firmware (BIOS/UEFI) may have used USB itself — ask it to hand over the controller.
        let mut ext = ((hcc1 >> 16) as usize) * 4;
        while ext != 0 {
            let p = (mmio + ext) as *mut u32;
            let v = rd(p);
            if v & 0xFF == 1 {
                wr(p, v | 1 << 24);
                let until = interrupts::ticks() + HZ;
                while rd(p) & 1 << 16 != 0 && interrupts::ticks() < until {
                    core::hint::spin_loop();
                }
                // SMI interrupts from USB are no longer needed.
                wr((mmio + ext + 4) as *mut u32, 0);
            }
            let next = (v >> 8) & 0xFF;
            ext = if next == 0 { 0 } else { ext + next as usize * 4 };
        }

        let mut x = Xhci {
            op: mmio + caplength as usize,
            runtime: mmio + (cap(0x18) & !0x1F) as usize,
            doorbells: mmio + (cap(0x14) & !0x3) as usize,
            ports,
            ctx_size,
            dcbaa: core::ptr::null_mut(),
            commands: Ring::new(),
            events: dma(RING * 16) as *mut [u32; 4],
            event_index: 0,
            event_cycle: 1,
            pending: VecDeque::new(),
            last_error: 0,
        };
        // Stop, reset, wait until ready.
        wr(x.op(0x00), rd(x.op(0x00)) & !1);
        let wait = |cond: &dyn Fn() -> bool| {
            let until = interrupts::ticks() + HZ;
            while !cond() && interrupts::ticks() < until {
                core::hint::spin_loop();
            }
            cond()
        };
        wait(&|| rd(x.op(0x04)) & 1 != 0);
        wr(x.op(0x00), 1 << 1);
        if !wait(&|| rd(x.op(0x00)) & 1 << 1 == 0 && rd(x.op(0x04)) & 1 << 11 == 0) {
            kprintln!("usb: controller reset timed out");
            return None;
        }
        let slots = max_slots.min(32);
        wr(x.op(0x38), slots);
        x.dcbaa = dma(2048) as *mut u64;
        // The controller's working memory (scratchpad), if it asks for it.
        let scratch = ((hcs2 >> 21) & 0x1F) << 5 | (hcs2 >> 27);
        if scratch > 0 {
            let table = dma(scratch as usize * 8) as *mut u64;
            for i in 0..scratch as usize {
                unsafe { *table.add(i) = dma(4096) as u64 };
            }
            unsafe { *x.dcbaa = table as u64 };
        }
        let dcbaa = x.dcbaa as u64;
        wr(x.op(0x30), dcbaa as u32);
        wr(x.op(0x34), (dcbaa >> 32) as u32);
        let crcr = x.commands.addr() | 1;
        wr(x.op(0x18), crcr as u32);
        wr(x.op(0x1C), (crcr >> 32) as u32);

        // The event ring: one segment and a segment table of one entry.
        let erst = dma(64) as *mut u32;
        let ev = x.events as u64;
        unsafe {
            *erst = ev as u32;
            *erst.add(1) = (ev >> 32) as u32;
            *erst.add(2) = RING as u32;
        }
        let ir = x.runtime + 0x20;
        wr((ir + 0x08) as *mut u32, 1);
        wr((ir + 0x18) as *mut u32, ev as u32);
        wr((ir + 0x1C) as *mut u32, (ev >> 32) as u32);
        wr((ir + 0x10) as *mut u32, erst as u64 as u32);
        wr((ir + 0x14) as *mut u32, (erst as u64 >> 32) as u32);

        wr(x.op(0x00), 1); // run
        if !wait(&|| rd(x.op(0x04)) & 1 == 0) {
            kprintln!("usb: controller did not start");
            return None;
        }
        kprintln!("usb: xHCI with {ports} ports, {slots} slots, {ctx_size}-byte contexts");
        Some(x)
    }

    /// Interrupts from the event ring (through MSI): enable them in the controller.
    pub fn enable_interrupts(&self) {
        let ir = self.runtime + 0x20;
        // No more often than once every 250 µs (the unit is 250 ns).
        wr((ir + 0x04) as *mut u32, 1000);
        wr(ir as *mut u32, 0b11);
        wr(self.op(0x00), rd(self.op(0x00)) | 1 << 2);
    }

    /// Clear the "interrupt happened" status after walking the ring: otherwise there will be no next one.
    pub fn ack_interrupt(&self) {
        wr((self.runtime + 0x20) as *mut u32, 0b11);
        wr(self.op(0x04), 1 << 3);
    }

    pub fn ring_doorbell(&self, slot: u8, target: u32) {
        wr((self.doorbells + slot as usize * 4) as *mut u32, target);
    }

    /// The next event from the ring (if any).
    pub fn next_event(&mut self) -> Option<Trb> {
        if let Some(e) = self.pending.pop_front() {
            return Some(e);
        }
        self.read_event()
    }

    fn read_event(&mut self) -> Option<Trb> {
        let p = unsafe { self.events.add(self.event_index) } as *const u32;
        let control = rd(unsafe { p.add(3) });
        if control & 1 != self.event_cycle {
            return None;
        }
        let trb = Trb([rd(p), rd(unsafe { p.add(1) }), rd(unsafe { p.add(2) }), control]);
        self.event_index += 1;
        if self.event_index == RING {
            self.event_index = 0;
            self.event_cycle ^= 1;
        }
        let erdp = unsafe { self.events.add(self.event_index) } as u64 | 1 << 3;
        let ir = self.runtime + 0x20;
        wr((ir + 0x18) as *mut u32, erdp as u32);
        wr((ir + 0x1C) as *mut u32, (erdp >> 32) as u32);
        Some(trb)
    }

    /// Waits for an event matching `want` (up to a second); sets the others aside.
    pub fn wait_event(&mut self, want: impl Fn(&Trb) -> bool) -> Option<Trb> {
        self.wait_event_for(HZ, want)
    }

    pub fn wait_event_for(&mut self, timeout: u64, want: impl Fn(&Trb) -> bool) -> Option<Trb> {
        let until = interrupts::ticks() + timeout;
        loop {
            while let Some(e) = self.read_event() {
                if want(&e) {
                    return Some(e);
                }
                self.pending.push_back(e);
            }
            if interrupts::ticks() >= until {
                return None;
            }
            if task::current_id() != 0 {
                task::yield_now();
            } else {
                core::hint::spin_loop();
            }
        }
    }

    /// Executes a controller command; Some(the completion event) or None (a timeout).
    pub fn command(&mut self, param: u64, status: u32, control: u32) -> Option<Trb> {
        let addr = self.commands.push(param, status, control);
        self.ring_doorbell(0, 0);
        let e = self.wait_event(|e| e.kind() == EV_COMMAND && e.pointer() == addr);
        self.last_error = match &e {
            Some(e) if e.code() == SUCCESS => self.last_error,
            Some(e) => e.code(),
            None => 0,
        };
        e
    }

    pub fn enable_slot(&mut self) -> Option<u8> {
        let e = self.command(0, 0, TRB_ENABLE_SLOT << 10)?;
        (e.code() == SUCCESS).then(|| e.slot())
    }

    pub fn disable_slot(&mut self, slot: u8) {
        self.command(0, 0, TRB_DISABLE_SLOT << 10 | (slot as u32) << 24);
        self.set_output_context(slot, 0);
    }

    pub fn set_output_context(&mut self, slot: u8, ctx: u64) {
        unsafe { *self.dcbaa.add(slot as usize) = ctx };
    }

    pub fn address_device(&mut self, slot: u8, input: u64) -> bool {
        self.command(input, 0, TRB_ADDRESS_DEVICE << 10 | (slot as u32) << 24).is_some_and(|e| e.code() == SUCCESS)
    }

    pub fn configure_endpoint(&mut self, slot: u8, input: u64) -> bool {
        self.command(input, 0, TRB_CONFIGURE_EP << 10 | (slot as u32) << 24).is_some_and(|e| e.code() == SUCCESS)
    }

    /// After an error (STALL) the endpoint stops: reset it and move the ring to `deq`.
    pub fn recover_endpoint(&mut self, slot: u8, dci: u32, deq: u64) {
        self.command(0, 0, TRB_RESET_EP << 10 | dci << 16 | (slot as u32) << 24);
        self.command(deq, 0, TRB_SET_DEQUEUE << 10 | dci << 16 | (slot as u32) << 24);
    }

    pub fn evaluate_context(&mut self, slot: u8, input: u64) -> bool {
        self.command(input, 0, TRB_EVALUATE_CTX << 10 | (slot as u32) << 24).is_some_and(|e| e.code() == SUCCESS)
    }

    /// A control transfer on EP0. `data` is for reading (IN) or writing (OUT, `dir_in` = false).
    pub fn control(&mut self, slot: u8, ep0: &mut Ring, setup: [u8; 8], buf: *mut u8, len: u16) -> Option<usize> {
        let dir_in = setup[0] & 0x80 != 0;
        let trt = if len == 0 { 0 } else if dir_in { 3 } else { 2 };
        ep0.push(u64::from_le_bytes(setup), 8, TRB_SETUP << 10 | 1 << 6 | trt << 16);
        if len > 0 {
            ep0.push(buf as u64, len as u32, TRB_DATA << 10 | (dir_in as u32) << 16);
        }
        let status_in = len == 0 || !dir_in;
        let last = ep0.push(0, 0, TRB_STATUS << 10 | 1 << 5 | (status_in as u32) << 16);
        self.ring_doorbell(slot, 1);
        // An error in the data stage arrives as an event on the data TRB, not the status one.
        let done = |e: &Trb| {
            e.kind() == EV_TRANSFER
                && e.slot() == slot
                && (e.0[3] >> 16) & 0x1F == 1
                && (e.pointer() == last || e.code() != SUCCESS && e.code() != SHORT_PACKET)
        };
        let Some(e) = self.wait_event_for(2 * HZ, done) else {
            // The device never answered (it was "busy" all the time): remove the request from
            // the endpoint, otherwise the next ones queue up behind it.
            self.command(0, 0, TRB_STOP_EP << 10 | 1 << 16 | (slot as u32) << 24);
            let deq = ep0.dequeue();
            self.command(deq, 0, TRB_SET_DEQUEUE << 10 | 1 << 16 | (slot as u32) << 24);
            self.last_error = 0;
            return None;
        };
        if e.code() != SUCCESS && e.code() != SHORT_PACKET {
            self.last_error = e.code();
            // The endpoint stopped (STALL and the like): reset it, otherwise the next requests will not pass.
            let deq = ep0.dequeue();
            self.recover_endpoint(slot, 1, deq);
            return None;
        }
        Some(len as usize)
    }

    /// A control request with retries (for configuration: slow devices
    /// answer "busy" or glitch at first).
    pub fn control_retry(&mut self, slot: u8, ep0: &mut Ring, setup: [u8; 8], buf: *mut u8, len: u16) -> Option<usize> {
        for attempt in 0..3 {
            if attempt > 0 {
                let until = interrupts::ticks() + HZ / 10;
                while interrupts::ticks() < until {
                    if task::current_id() != 0 {
                        task::yield_now();
                    }
                }
            }
            if let Some(n) = self.control(slot, ep0, setup, buf, len) {
                return Some(n);
            }
        }
        None
    }

    pub fn port_speed(&self, port: u32) -> u32 {
        (rd(self.portsc(port)) >> 10) & 0xF
    }

    pub fn port_connected(&self, port: u32) -> bool {
        rd(self.portsc(port)) & 1 != 0
    }

    /// Resets a port and waits until it is enabled. A USB 3 port enables itself —
    /// it is only reset if `force` (a retry after a failure).
    pub fn reset_port(&mut self, port: u32, force: bool) -> bool {
        let p = self.portsc(port);
        // The "changed" bits (17–23, cleared by writing 1) and PED (bit 1, writing 1
        // disables the port) are left as zeros when writing.
        let keep = |v: u32| v & !(1 << 1 | 0x7F << 17);
        let v = rd(p);
        let pause = || {
            if task::current_id() != 0 {
                task::yield_now();
            }
        };
        if v & 1 << 1 == 0 || force {
            wr(p, keep(v) | (v & 1 << 21) | 1 << 4); // also clear the old PRC
            // The reset is done when PRC (bit 21) is set and PR (bit 4) is cleared.
            let until = interrupts::ticks() + HZ / 2;
            while (rd(p) & 1 << 21 == 0 || rd(p) & 1 << 4 != 0) && interrupts::ticks() < until {
                pause();
            }
        }
        let until = interrupts::ticks() + HZ / 2;
        while rd(p) & 1 << 1 == 0 && interrupts::ticks() < until {
            pause();
        }
        // Clear the change flags so the port can report them again.
        let v = rd(p);
        wr(p, keep(v) | (v & 0x7F << 17));
        rd(p) & 1 << 1 != 0
    }
}
