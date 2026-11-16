//! USB hubs (2.0 and 3.x): port power, reset and connecting the devices behind them.
//!
//! The hub ports are polled with GET_PORT_STATUS requests twice a second (the hub's
//! interrupt endpoint is not needed). For low/full-speed devices (keyboard, mouse) behind a
//! USB 2.0 hub the controller sends data through the hub's "translator" — the TT; its slot and port
//! are written into the device context (`Loc::tt`).

use super::xhci::{Ring, Xhci, dma};
use super::{Loc, Usb, ctx};
use crate::interrupts::{self, HZ};
use crate::kprintln;
use crate::task;

// Requests to hub ports (the recipient is "other", i.e. a port).
const GET_STATUS: u8 = 0;
const CLEAR_FEATURE: u8 = 1;
const SET_FEATURE: u8 = 3;
const PORT_RESET: u8 = 4;
const PORT_POWER: u8 = 8;
const C_PORT_CONNECTION: u8 = 16;
const C_PORT_ENABLE: u8 = 17;
const C_PORT_RESET: u8 = 20;
const C_BH_PORT_RESET: u8 = 29;
const C_PORT_LINK_STATE: u8 = 25;
/// USB 3: the hub's depth in the chain (it needs it to parse the route).
const SET_HUB_DEPTH: u8 = 12;

pub struct Hub {
    pub slot: u8,
    pub loc: Loc,
    speed: u32,
    ep0: Ring,
    buf: *mut u8,
    ports: u32,
    /// The ports that already have a configured device (a bit per port).
    attached: u32,
}

unsafe impl Send for Hub {}

pub(super) fn sleep_ms(ms: u64) {
    let until = interrupts::ticks() + (ms * HZ).div_ceil(1000) + 1;
    while interrupts::ticks() < until {
        if task::current_id() != 0 {
            task::yield_now();
        } else {
            core::hint::spin_loop();
        }
    }
}

impl Hub {
    fn port_req(&mut self, hc: &mut Xhci, req: u8, feature: u8, port: u32) -> bool {
        let rt = if req == GET_STATUS { 0xA3 } else { 0x23 };
        let len = if req == GET_STATUS { 4 } else { 0 };
        hc.control(self.slot, &mut self.ep0, [rt, req, feature, 0, port as u8, 0, len as u8, 0], self.buf, len).is_some()
    }

    /// The port status: (wPortStatus, wPortChange).
    fn status(&mut self, hc: &mut Xhci, port: u32) -> Option<(u16, u16)> {
        if !self.port_req(hc, GET_STATUS, 0, port) {
            return None;
        }
        let b = unsafe { core::slice::from_raw_parts(self.buf, 4) };
        Some((u16::from_le_bytes([b[0], b[1]]), u16::from_le_bytes([b[2], b[3]])))
    }

    /// Clears all of the port's change flags.
    fn clear_changes(&mut self, hc: &mut Xhci, port: u32, change: u16) {
        for (bit, feature) in
            [(0, C_PORT_CONNECTION), (1, C_PORT_ENABLE), (4, C_PORT_RESET), (5, C_BH_PORT_RESET), (6, C_PORT_LINK_STATE)]
        {
            if change & 1 << bit != 0 {
                self.port_req(hc, CLEAR_FEATURE, feature, port);
            }
        }
    }

    /// Resets a port and returns the device speed (in xHCI codes) or None.
    fn reset(&mut self, hc: &mut Xhci, port: u32) -> Option<u32> {
        self.port_req(hc, SET_FEATURE, PORT_RESET, port);
        let until = interrupts::ticks() + HZ / 2;
        let mut st = None;
        while interrupts::ticks() < until {
            sleep_ms(10);
            if let Some((s, c)) = self.status(hc, port)
                && s & 1 << 4 == 0
                && c & (1 << 4 | 1 << 5) != 0
            {
                self.clear_changes(hc, port, c);
                st = Some(s);
                break;
            }
        }
        let s = st?;
        if s & 1 == 0 || s & 1 << 1 == 0 {
            return None; // the device was disconnected or the port did not enable
        }
        sleep_ms(10); // reset recovery time
        Some(if self.speed >= 4 {
            4 // behind a USB 3 hub — SuperSpeed only
        } else if s & 1 << 9 != 0 {
            2 // low speed
        } else if s & 1 << 10 != 0 {
            3 // high speed
        } else {
            1 // full speed
        })
    }

    /// Where a device on port `port` of this hub will be.
    fn child(&self, port: u32, speed: u32) -> Loc {
        let tt = if self.speed == 3 && speed < 3 { Some((self.slot, port as u8)) } else { self.loc.tt };
        Loc {
            root: self.loc.root,
            route: self.loc.route | port.min(15) << (4 * self.loc.depth),
            depth: self.loc.depth + 1,
            tt,
        }
    }
}

impl Usb {
    /// The device turned out to be a hub: tell the controller and power the ports.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn attach_hub(
        &mut self,
        slot: u8,
        loc: Loc,
        speed: u32,
        ep0: Ring,
        buf: *mut u8,
        input: *mut u8,
        config: u8,
        (vendor, product): (u16, u16),
    ) -> Result<(), &'static str> {
        if loc.depth >= 5 {
            return Err("too many hubs in a chain");
        }
        let mut hub = Hub { slot, loc, speed, ep0, buf: dma(64), ports: 0, attached: 0 };
        let hc = &mut self.hc;
        hc.control_retry(slot, &mut hub.ep0, [0, 9, config, 0, 0, 0, 0, 0], buf, 0).ok_or("set configuration failed")?;
        if speed >= 4 {
            hc.control(slot, &mut hub.ep0, [0x20, SET_HUB_DEPTH, loc.depth as u8, 0, 0, 0, 0, 0], buf, 0);
        }
        // The hub descriptor: the number of ports and the time until power appears on them.
        let kind = if speed >= 4 { 0x2A } else { 0x29 };
        hc.control(slot, &mut hub.ep0, [0xA0, 6, 0, kind, 0, 0, 12, 0], buf, 12).ok_or("no hub descriptor")?;
        let d = unsafe { core::slice::from_raw_parts(buf, 12) };
        hub.ports = (d[2] as u32).min(15);
        let power_good = (d[5] as u64 * 2).max(100);

        // To the controller: this is a hub with this many ports (and with one TT if USB 2.0).
        let cs = hc.ctx_size;
        unsafe {
            for k in 0..cs * 33 / 4 {
                *(input as *mut u32).add(k) = 0;
            }
            *ctx(input, cs, 0).add(1) = 1;
            let sl = ctx(input, cs, 1);
            super::slot_ctx(sl, &loc, speed, 1);
            *sl |= 1 << 26;
            *sl.add(1) |= hub.ports << 24;
        }
        if !hc.configure_endpoint(slot, input as u64) {
            return Err("configure hub failed");
        }
        for port in 1..=hub.ports {
            hub.port_req(hc, SET_FEATURE, PORT_POWER, port);
        }
        sleep_ms(power_good);
        let usb = if speed >= 4 { "USB 3" } else { "USB 2" };
        let ports = hub.ports;
        self.hubs.push(hub);
        self.report(loc, alloc::format!("{usb} hub {vendor:04x}:{product:04x}, {ports} ports"));
        Ok(())
    }

    /// Checks the ports of hub `i`: connect new devices, forget vanished ones.
    pub(super) fn scan_hub(&mut self, i: usize) {
        for port in 1..=self.hubs[i].ports {
            let hub = &mut self.hubs[i];
            let Some((status, change)) = hub.status(&mut self.hc, port) else { continue };
            if change & 1 != 0 {
                hub.port_req(&mut self.hc, CLEAR_FEATURE, C_PORT_CONNECTION, port);
            }
            let connected = status & 1 != 0;
            let known = hub.attached & 1 << port != 0;
            // The device was replaced while we were not looking: forget the old one first.
            if known && (!connected || change & 1 != 0) {
                hub.attached &= !(1 << port);
                let at = hub.child(port, 0);
                self.detach(Loc { tt: None, ..at });
            }
            let hub = &mut self.hubs[i];
            if connected && hub.attached & 1 << port == 0 {
                hub.attached |= 1 << port;
                sleep_ms(100); // contact bounce on connection
                for attempt in 1..=super::TRIES {
                    let hub = &mut self.hubs[i];
                    let Some(speed) = hub.reset(&mut self.hc, port) else {
                        kprintln!("usb: {}: port did not enable", hub.child(port, 3));
                        break;
                    };
                    let loc = hub.child(port, speed);
                    match self.attach(loc, speed) {
                        Ok(()) => break,
                        Err(e) => {
                            kprintln!("usb: {loc}: {e} (attempt {attempt} of {})", super::TRIES);
                            self.failed(loc, e);
                        }
                    }
                    sleep_ms(100);
                }
            }
        }
    }
}
