//! The Bluetooth transport over USB (an E0/01/01 class interface — any vendor): HCI commands
//! go as class control requests, events through the interrupt endpoint, ACL data through
//! the bulk endpoints. It only moves packets between the controller and the stack (`crate::bt`): the stack
//! runs in its own thread and does not occupy the USB thread (commands used to wait for answers right
//! here — and the keyboard and mouse stood still meanwhile).
//!
//! Voice (SCO) is the second interface with isochronous endpoints: while there is no voice channel,
//! it is in alternate setting 0 (no endpoints); when the stack asks for voice we switch to the setting with endpoints
//! of the needed size and push packets every 1 ms.

use super::xhci::{self, Ring, Xhci, dma};
use super::{Loc, Usb, ctx, slot_ctx};
use crate::bt::{self, Kind, Shared};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// How many event endpoint reads to keep queued.
const EVENT_READS: usize = 8;
const EVENT_BUF: usize = 64;
/// An HCI command: opcode (2), length (1), parameters (up to 255).
const COMMAND_BUF: usize = 260;
/// Reads and send buffers for ACL data; a packet with its header fits the buffer with room to spare.
const ACL_READS: usize = 4;
const ACL_TX_BUFS: usize = 8;
const ACL_BUF: usize = 2048;
/// Isochronous voice reads in the queue (one packet per millisecond) and send buffers.
const VOICE_READS: usize = 24;
const VOICE_TX: usize = 64;
/// The voice interface alternate setting for one channel of 16-bit audio (CVSD), as in btusb.
const VOICE_ALT_16BIT: u8 = 2;
const TRB_ISOCH: u32 = 5;
const RING_UNDERRUN: u32 = 14;
const RING_OVERRUN: u32 = 15;
const MISSED_SERVICE: u32 = 23;

/// A voice interface alternate setting: its number and isochronous endpoints (address, size, interval).
#[derive(Clone, Copy, Debug)]
struct VoiceAlt {
    alt: u8,
    input: (u8, u16, u8),
    output: (u8, u16, u8),
}

/// The Bluetooth interface in a configuration: class E0/01/01, the event endpoint, the bulk endpoints.
pub struct Found {
    iface: u8,
    events: (u8, u16, u8),
    bulk_in: (u8, u16),
    bulk_out: (u8, u16),
    /// The voice interface and its alternate settings.
    voice_iface: u8,
    voice_alts: Vec<VoiceAlt>,
}

/// The voice path while it is open.
struct Voice {
    in_dci: u32,
    out_dci: u32,
    in_ring: Ring,
    out_ring: Ring,
    in_mps: usize,
    out_mps: usize,
    /// Queued reads: the TRB address and the buffer.
    reads: VecDeque<(u64, *mut u8)>,
    /// Received bytes (SCO packets are cut into isochronous pieces) and unsent ones.
    rx: Vec<u8>,
    tx: VecDeque<u8>,
    tx_bufs: Vec<*mut u8>,
    tx_next: usize,
    in_flight: usize,
}

pub fn find(conf: &[u8]) -> Option<Found> {
    let (mut iface, mut ours) = (0u8, false);
    let (mut events, mut bin, mut bout) = (None, None, None);
    let mut i = 0;
    // The voice interface: the one following ours, its alternate settings with isochronous endpoints.
    let (mut voice_iface, mut voice_alts, mut alt) = (None, Vec::new(), None::<VoiceAlt>);
    while i + 2 <= conf.len() {
        let (len, ty) = (conf[i] as usize, conf[i + 1]);
        if len == 0 {
            break;
        }
        match ty {
            4 if i + 9 <= conf.len() => {
                if let Some(a) = alt.take().filter(|a| a.input.1 > 0 && a.output.1 > 0) {
                    voice_alts.push(a);
                }
                // The first interface is commands and data; the second (with alternate settings) is voice.
                ours = conf[i + 5] == 0xE0 && conf[i + 6] == 0x01 && conf[i + 7] == 0x01 && conf[i + 3] == 0 && events.is_none();
                if ours {
                    iface = conf[i + 2];
                } else if conf[i + 5] == 0xE0 && conf[i + 6] == 0x01 && conf[i + 7] == 0x01 && events.is_some() && voice_iface.is_none_or(|v| v == conf[i + 2]) {
                    voice_iface = Some(conf[i + 2]);
                    alt = Some(VoiceAlt { alt: conf[i + 3], input: (0, 0, 0), output: (0, 0, 0) });
                }
            }
            5 if i + 7 <= conf.len() && alt.is_some() => {
                let (addr, attr, mps) = (conf[i + 2], conf[i + 3], u16::from_le_bytes([conf[i + 4], conf[i + 5]]));
                if let Some(a) = alt.as_mut().filter(|_| attr & 3 == 1) {
                    let ep = (addr & 0xF, mps & 0x7FF, conf[i + 6]);
                    if addr & 0x80 != 0 { a.input = ep } else { a.output = ep }
                }
            }
            5 if i + 7 <= conf.len() && ours => {
                let (addr, attr, mps) = (conf[i + 2], conf[i + 3], u16::from_le_bytes([conf[i + 4], conf[i + 5]]));
                match (attr & 3, addr & 0x80 != 0) {
                    (3, true) => events = Some((addr & 0xF, mps, conf[i + 6])),
                    (2, true) => bin = Some((addr & 0xF, mps)),
                    (2, false) => bout = Some((addr & 0xF, mps)),
                    _ => {}
                }
            }
            _ => {}
        }
        i += len;
    }
    if let Some(a) = alt.filter(|a| a.input.1 > 0 && a.output.1 > 0) {
        voice_alts.push(a);
    }
    Some(Found { iface, events: events?, bulk_in: bin?, bulk_out: bout?, voice_iface: voice_iface.unwrap_or(iface + 1), voice_alts })
}

pub struct Bt {
    pub slot: u8,
    pub loc: Loc,
    ep0: Ring,
    cmd: *mut u8,
    ev_dci: u32,
    ev_ring: Ring,
    ev_mps: usize,
    ev_reads: VecDeque<*mut u8>,
    /// The start of an event that arrived in several packets.
    partial: Vec<u8>,
    in_dci: u32,
    in_ring: Ring,
    acl_reads: VecDeque<*mut u8>,
    out_dci: u32,
    out_ring: Ring,
    out_mps: usize,
    acl_tx: Vec<*mut u8>,
    tx_next: usize,
    /// The queues shared with the stack.
    pipe: Shared,
    speed: u32,
    /// The endpoints that already exist (needed for the number of contexts when adding voice).
    base_max_dci: u32,
    voice_iface: u8,
    voice_alts: Vec<VoiceAlt>,
    voice: Option<Voice>,
    /// Voice failed — do not try again until the stack asks anew.
    voice_failed: bool,
}

unsafe impl Send for Bt {}

impl Drop for Bt {
    /// The adapter was unplugged — the stack will stop.
    fn drop(&mut self) {
        self.pipe.lock().gone = true;
    }
}

impl Bt {
    fn queue_event_read(&mut self, hc: &Xhci, buf: *mut u8) {
        self.ev_ring.push(buf as u64, self.ev_mps as u32, xhci::TRB_NORMAL << 10 | 1 << 5 | 1 << 2);
        self.ev_reads.push_back(buf);
        hc.ring_doorbell(self.slot, self.ev_dci);
    }

    fn queue_acl_read(&mut self, hc: &Xhci, buf: *mut u8) {
        self.in_ring.push(buf as u64, ACL_BUF as u32, xhci::TRB_NORMAL << 10 | 1 << 5 | 1 << 2);
        self.acl_reads.push_back(buf);
        hc.ring_doorbell(self.slot, self.in_dci);
    }

    /// Sends what the stack queued: commands as class control requests,
    /// ACL data to the bulk endpoint. Does not wait for answers: they come as events.
    pub fn service(&mut self, hc: &mut Xhci) {
        // Voice: the stack asked to open or close the path.
        let want = self.pipe.lock().voice;
        if want != self.voice.is_some() {
            if want && !self.voice_failed {
                if let Err(e) = self.voice_on(hc) {
                    crate::kprintln!("bt: usb: voice path: {e}");
                    self.voice_failed = true;
                }
            } else if !want {
                self.voice_off(hc);
                self.voice_failed = false;
            }
        }
        loop {
            let next = self.pipe.lock().to_controller.pop_front();
            let Some((kind, packet)) = next else { break };
            match kind {
                Kind::Command => {
                    let len = packet.len().min(COMMAND_BUF);
                    unsafe { core::ptr::copy_nonoverlapping(packet.as_ptr(), self.cmd, len) };
                    let setup = [0x20, 0, 0, 0, 0, 0, len as u8, (len >> 8) as u8];
                    if hc.control_retry(self.slot, &mut self.ep0, setup, self.cmd, len as u16).is_none() {
                        crate::kprintln!("bt: usb: the controller did not take a command");
                    }
                }
                Kind::Acl => {
                    let buf = self.acl_tx[self.tx_next];
                    self.tx_next = (self.tx_next + 1) % ACL_TX_BUFS;
                    let n = packet.len().min(ACL_BUF);
                    unsafe { core::ptr::copy_nonoverlapping(packet.as_ptr(), buf, n) };
                    self.out_ring.push(buf as u64, n as u32, xhci::TRB_NORMAL << 10 | 1 << 5);
                    // Exactly on a packet boundary — an empty packet is needed, otherwise the transfer does not end.
                    if n % self.out_mps == 0 {
                        self.out_ring.push(buf as u64, 0, xhci::TRB_NORMAL << 10 | 1 << 5);
                    }
                    hc.ring_doorbell(self.slot, self.out_dci);
                }
                Kind::Sco => {
                    if let Some(v) = &mut self.voice {
                        v.tx.extend(packet.iter().copied());
                    }
                }
                Kind::Event => {}
            }
        }
        self.voice_pump(hc);
    }

    /// Voice: the interface alternate setting with isochronous endpoints, the endpoints in the controller, reads.
    fn voice_on(&mut self, hc: &mut Xhci) -> Result<(), &'static str> {
        let alt = self.voice_alts.iter().find(|a| a.alt == VOICE_ALT_16BIT).or(self.voice_alts.last()).copied().ok_or("the adapter has no voice interface")?;
        let setup = [0x01, 11, alt.alt, 0, self.voice_iface, 0, 0, 0];
        hc.control_retry(self.slot, &mut self.ep0, setup, self.cmd, 0).ok_or("set interface failed")?;
        let (in_dci, out_dci) = (alt.input.0 as u32 * 2 + 1, alt.output.0 as u32 * 2);
        let (in_ring, out_ring) = (Ring::new(), Ring::new());
        let cs = hc.ctx_size;
        let input = dma(cs * 33);
        // The isochronous endpoint interval: full speed — bInterval + 2, high speed — bInterval − 1.
        let interval = |b: u8| if self.speed >= 3 { (b.clamp(1, 16) - 1) as u32 } else { b.clamp(1, 16) as u32 + 2 };
        unsafe {
            *ctx(input, cs, 0).add(1) = 1 | 1 << in_dci | 1 << out_dci;
            slot_ctx(ctx(input, cs, 1), &self.loc, self.speed, self.base_max_dci.max(in_dci).max(out_dci));
            for (dci, ty, ep, ring) in [(in_dci, 5u32, alt.input, &in_ring), (out_dci, 1, alt.output, &out_ring)] {
                let e = ctx(input, cs, 1 + dci as usize);
                *e = interval(ep.2) << 16;
                *e.add(1) = ty << 3 | (ep.1 as u32) << 16;
                let deq = ring.addr() | 1;
                *e.add(2) = deq as u32;
                *e.add(3) = (deq >> 32) as u32;
                *e.add(4) = ep.1 as u32 | (ep.1 as u32) << 16;
            }
        }
        if !hc.configure_endpoint(self.slot, input as u64) {
            let _ = hc.control_retry(self.slot, &mut self.ep0, [0x01, 11, 0, 0, self.voice_iface, 0, 0, 0], self.cmd, 0);
            return Err("configure voice endpoints failed");
        }
        let (in_mps, out_mps) = (alt.input.1 as usize, alt.output.1 as usize);
        let mut v = Voice {
            in_dci,
            out_dci,
            in_ring,
            out_ring,
            in_mps,
            out_mps,
            reads: VecDeque::new(),
            rx: Vec::new(),
            tx: VecDeque::new(),
            tx_bufs: (0..VOICE_TX).map(|_| dma(out_mps.max(64))).collect(),
            tx_next: 0,
            in_flight: 0,
        };
        for _ in 0..VOICE_READS {
            let buf = dma(in_mps.max(64));
            Self::voice_read(&mut v, buf);
        }
        hc.ring_doorbell(self.slot, in_dci);
        crate::kprintln!("bt: usb: voice path open (alternate setting {}, packets {in_mps}/{out_mps} bytes)", alt.alt);
        self.voice = Some(v);
        Ok(())
    }

    fn voice_read(v: &mut Voice, buf: *mut u8) {
        // An isochronous TRB: as soon as possible (SIA), an interrupt on completion.
        let at = v.in_ring.push(buf as u64, v.in_mps as u32, TRB_ISOCH << 10 | 1 << 5 | 1 << 2 | 1 << 31);
        v.reads.push_back((at, buf));
    }

    /// Voice closed: remove the endpoints and return the interface to alternate setting 0.
    fn voice_off(&mut self, hc: &mut Xhci) {
        let Some(v) = self.voice.take() else { return };
        let cs = hc.ctx_size;
        let input = dma(cs * 33);
        unsafe {
            *ctx(input, cs, 0) = 1 << v.in_dci | 1 << v.out_dci;
            *ctx(input, cs, 0).add(1) = 1;
            slot_ctx(ctx(input, cs, 1), &self.loc, self.speed, self.base_max_dci);
        }
        hc.configure_endpoint(self.slot, input as u64);
        let _ = hc.control_retry(self.slot, &mut self.ep0, [0x01, 11, 0, 0, self.voice_iface, 0, 0, 0], self.cmd, 0);
        crate::kprintln!("bt: usb: voice path closed");
    }

    /// Voice to the controller: a stream of SCO packets — in pieces of the isochronous packet size.
    fn voice_pump(&mut self, hc: &mut Xhci) {
        let Some(v) = &mut self.voice else { return };
        let mut queued = false;
        while v.in_flight < VOICE_TX && !v.tx.is_empty() {
            let n = v.out_mps.min(v.tx.len());
            let buf = v.tx_bufs[v.tx_next];
            v.tx_next = (v.tx_next + 1) % VOICE_TX;
            for (i, b) in v.tx.drain(..n).enumerate() {
                unsafe { *buf.add(i) = b };
            }
            v.out_ring.push(buf as u64, n as u32, TRB_ISOCH << 10 | 1 << 5 | 1 << 31);
            v.in_flight += 1;
            queued = true;
        }
        if queued {
            hc.ring_doorbell(self.slot, v.out_dci);
        }
    }

    /// A transfer event: a received HCI event or ACL data goes to the stack, the read is queued again.
    pub fn on_transfer(&mut self, hc: &Xhci, e: &xhci::Trb) -> bool {
        if e.slot() != self.slot {
            return false;
        }
        let dci = (e.0[3] >> 16) & 0x1F;
        let ok = e.code() == xhci::SUCCESS || e.code() == xhci::SHORT_PACKET;
        if dci == self.ev_dci {
            let Some(buf) = self.ev_reads.pop_front() else { return true };
            let n = self.ev_mps - (e.0[2] & 0xFF_FFFF) as usize;
            if ok {
                self.partial.extend_from_slice(unsafe { core::slice::from_raw_parts(buf, n.min(self.ev_mps)) });
            }
            self.queue_event_read(hc, buf);
            // A whole event: code, parameter length, parameters.
            if self.partial.len() >= 2 && self.partial.len() >= 2 + self.partial[1] as usize {
                let len = 2 + self.partial[1] as usize;
                let event = self.partial[..len].to_vec();
                self.partial.clear();
                bt::deliver(&self.pipe, Kind::Event, event);
            }
        } else if let Some(v) = self.voice.as_mut().filter(|v| dci == v.in_dci || dci == v.out_dci) {
            if dci == v.out_dci {
                if e.pointer() != 0 {
                    v.in_flight = v.in_flight.saturating_sub(1);
                }
                return true;
            }
            match v.reads.iter().position(|r| r.0 == e.pointer()) {
                Some(i) => {
                    // Reads skipped earlier came back too (their events did not arrive).
                    for _ in 0..i {
                        if let Some((_, b)) = v.reads.pop_front() {
                            Self::voice_read(v, b);
                        }
                    }
                    let (_, buf) = v.reads.pop_front().unwrap();
                    if ok || e.code() == MISSED_SERVICE {
                        let n = v.in_mps.saturating_sub((e.0[2] & 0xFF_FFFF) as usize).min(v.in_mps);
                        v.rx.extend_from_slice(unsafe { core::slice::from_raw_parts(buf, n) });
                    }
                    Self::voice_read(v, buf);
                }
                None if matches!(e.code(), RING_OVERRUN | RING_UNDERRUN) => {}
                None => return true,
            }
            hc.ring_doorbell(self.slot, v.in_dci);
            // SCO packets: handle (2), length (1), samples.
            while v.rx.len() >= 3 && v.rx.len() >= 3 + v.rx[2] as usize {
                let len = 3 + v.rx[2] as usize;
                let packet: Vec<u8> = v.rx.drain(..len).collect();
                bt::deliver(&self.pipe, Kind::Sco, packet);
            }
        } else if dci == self.in_dci {
            let Some(buf) = self.acl_reads.pop_front() else { return true };
            let n = ACL_BUF - (e.0[2] & 0xFF_FFFF) as usize;
            if ok && n >= 4 {
                bt::deliver(&self.pipe, Kind::Acl, unsafe { core::slice::from_raw_parts(buf, n) }.to_vec());
            }
            self.queue_acl_read(hc, buf);
        }
        true
    }
}

impl Usb {
    /// Sets up a Bluetooth adapter: configuration, the event endpoint, the bulk endpoints; then
    /// the stack in its own thread (vendor setup, startup).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn attach_bt(
        &mut self,
        slot: u8,
        loc: Loc,
        speed: u32,
        mut ep0: Ring,
        buf: *mut u8,
        input: *mut u8,
        config: u8,
        found: Found,
        ids: (u16, u16),
    ) -> Result<(), &'static str> {
        let hc = &mut self.hc;
        hc.control_retry(slot, &mut ep0, [0, 9, config, 0, 0, 0, 0, 0], buf, 0).ok_or("set configuration failed")?;
        let cs = hc.ctx_size;
        let ev_dci = found.events.0 as u32 * 2 + 1;
        let (in_dci, out_dci) = (found.bulk_in.0 as u32 * 2 + 1, found.bulk_out.0 as u32 * 2);
        let (ev_ring, in_ring, out_ring) = (Ring::new(), Ring::new(), Ring::new());
        unsafe {
            for k in 0..cs * 33 / 4 {
                *(input as *mut u32).add(k) = 0;
            }
            *ctx(input, cs, 0).add(1) = 1 | 1 << ev_dci | 1 << in_dci | 1 << out_dci;
            slot_ctx(ctx(input, cs, 1), &loc, speed, ev_dci.max(in_dci).max(out_dci));
            let interval = found.events.2 as u32;
            let xinterval = if speed >= 3 { interval.clamp(1, 16) - 1 } else { 31 - (interval.max(1) * 8).leading_zeros() };
            let points = [(ev_dci, 7u32, found.events.1, &ev_ring, xinterval), (in_dci, 6, found.bulk_in.1, &in_ring, 0), (out_dci, 2, found.bulk_out.1, &out_ring, 0)];
            for (dci, ty, mps, ring, xi) in points {
                let e = ctx(input, cs, 1 + dci as usize);
                *e = xi << 16;
                *e.add(1) = 3 << 1 | ty << 3 | (mps as u32) << 16;
                let deq = ring.addr() | 1;
                *e.add(2) = deq as u32;
                *e.add(3) = (deq >> 32) as u32;
                *e.add(4) = mps as u32 | if ty == 7 { (mps as u32) << 16 } else { 0 };
            }
        }
        if !hc.configure_endpoint(slot, input as u64) {
            return Err("configure Bluetooth endpoints failed");
        }
        let _ = found.iface;
        let pipe = bt::pipe();
        let mut dev = Bt {
            slot,
            loc,
            ep0,
            cmd: dma(COMMAND_BUF),
            ev_dci,
            ev_ring,
            ev_mps: (found.events.1 as usize).clamp(8, EVENT_BUF),
            ev_reads: VecDeque::new(),
            partial: Vec::new(),
            in_dci,
            in_ring,
            acl_reads: VecDeque::new(),
            out_dci,
            out_ring,
            out_mps: (found.bulk_out.1 as usize).max(1),
            acl_tx: (0..ACL_TX_BUFS).map(|_| dma(ACL_BUF)).collect(),
            tx_next: 0,
            pipe: pipe.clone(),
            speed,
            base_max_dci: ev_dci.max(in_dci).max(out_dci),
            voice_iface: found.voice_iface,
            voice_alts: found.voice_alts.clone(),
            voice: None,
            voice_failed: false,
        };
        let alts: Vec<String> = found.voice_alts.iter().map(|a| format!("{}:{}/{}", a.alt, a.input.1, a.output.1)).collect();
        crate::kprintln!("bt: usb: voice interface {} with alternate settings {}", found.voice_iface, if alts.is_empty() { "none".into() } else { alts.join(" ") });
        for _ in 0..EVENT_READS {
            let b = dma(EVENT_BUF);
            dev.queue_event_read(&self.hc, b);
        }
        for _ in 0..ACL_READS {
            let b = dma(ACL_BUF);
            dev.queue_acl_read(&self.hc, b);
        }
        self.report(loc, format!("bluetooth {:04x}:{:04x}", ids.0, ids.1));
        bt::attach(Box::new(bt::PipeHci::new(pipe, format!("USB {:04x}:{:04x}", ids.0, ids.1), Some(ids.0))));
        self.bts.push(dev);
        Ok(())
    }
}
