//! The remote control (AVRCP 1.4 over AVCTP, PSM 0x17). One channel in both directions:
//! - the headphones are the remote, we are the player: their buttons (play, pause, next, previous,
//!   volume) control the player and the system volume;
//! - we are the remote, the headphones are the amplifier: absolute volume. The system slider sets
//!   the volume in the headphones themselves, and the mixer does not attenuate the sound (otherwise the volume
//!   would be reduced twice, and the headphones would be noticeably quieter than with other OSes).
//!   The volume buttons on the headphones move the system slider.
//!
//! An AVCTP packet: [label << 4 | type << 2 | response << 1 | invalid profile] [profile, 2 bytes]
//! and an AV/C frame: [command or response type] [subunit] [opcode] [operands].

use super::Host;
use super::chan::PSM_AVCTP;
use crate::audio::{self, player};
use crate::bt::addr_text;
use crate::input::{self, Event};
use crate::interrupts::{self, HZ};
use crate::kprintln;
use alloc::vec::Vec;

/// The profile in the AVCTP header — "remote control" (A/V Remote Control).
const PID_REMOTE: u16 = 0x110E;

// AV/C command and response types.
const CONTROL: u8 = 0x00;
const STATUS: u8 = 0x01;
const NOTIFY: u8 = 0x03;
const NOT_IMPLEMENTED: u8 = 0x08;
const ACCEPTED: u8 = 0x09;
const REJECTED: u8 = 0x0A;
const STABLE: u8 = 0x0C;
const CHANGED: u8 = 0x0D;
const INTERIM: u8 = 0x0F;

/// The "panel" subunit (9), id 0.
const PANEL: u8 = 0x48;
const OP_VENDOR: u8 = 0x00;
const OP_UNIT_INFO: u8 = 0x30;
const OP_SUBUNIT_INFO: u8 = 0x31;
const OP_PASS_THROUGH: u8 = 0x7C;
/// The Bluetooth SIG company id — on all AVRCP commands.
const BT_SIG: [u8; 3] = [0x00, 0x19, 0x58];

// AVRCP commands (inside a "vendor dependent command").
const GET_CAPABILITIES: u8 = 0x10;
const INFORM_CHARSET: u8 = 0x17;
const INFORM_BATTERY: u8 = 0x18;
const REGISTER_NOTIFICATION: u8 = 0x31;
const SET_ABSOLUTE_VOLUME: u8 = 0x50;
const CAP_COMPANY: u8 = 0x02;
const CAP_EVENTS: u8 = 0x03;
const EVENT_PLAYBACK: u8 = 0x01;
const EVENT_TRACK: u8 = 0x02;
const EVENT_VOLUME: u8 = 0x0D;
const ERR_INVALID_COMMAND: u8 = 0x00;
const ERR_INVALID_PARAMETER: u8 = 0x01;

/// If the headphones did not open the remote control within this time after the music started — we open it.
const OPEN_DELAY: u64 = 3 * HZ;

pub(super) struct Avrcp {
    chan: u16,
    open: bool,
    /// We already tried opening it ourselves (we do not try a second time).
    tried: bool,
    since: u64,
    /// The label of our next command.
    label: u8,
    /// The headphones keep the volume themselves (they subscribed to its changes).
    absolute: bool,
    /// The headphone volume (0..=127) that both sides know.
    synced: Option<u8>,
    /// The headphones subscribed to "playing / paused": the subscription label and what they were told.
    playback: Option<(u8, bool)>,
}

impl Avrcp {
    fn new(now: u64) -> Avrcp {
        Avrcp { chan: 0, open: false, tried: false, since: now, label: 0, absolute: false, synced: None, playback: None }
    }
}

/// System volume (0..=256) ↔ absolute volume (0..=127), with rounding:
/// there and back gives the same value (the system step is finer).
fn to_absolute(v: i32) -> u8 {
    ((v.clamp(0, 256) as u32 * 127 + 128) / 256).min(127) as u8
}

fn from_absolute(v: u8) -> u32 {
    ((v.min(127) as u32 * 256 + 63) / 127).min(256)
}

/// A "vendor dependent command" frame (all AVRCP commands except buttons).
fn vendor(code: u8, pdu: u8, params: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(10 + params.len());
    f.extend_from_slice(&[code, PANEL, OP_VENDOR]);
    f.extend_from_slice(&BT_SIG);
    f.extend_from_slice(&[pdu, 0]);
    f.extend_from_slice(&(params.len() as u16).to_be_bytes());
    f.extend_from_slice(params);
    f
}

fn playback_status(playing: bool) -> u8 {
    // 1 — playing, 2 — paused.
    if playing { 1 } else { 2 }
}

impl Host {
    fn avrcp_chan(&mut self, handle: u16) -> Option<u16> {
        self.conn_mut(handle).and_then(|c| c.avrcp.as_ref()).filter(|a| a.open).map(|a| a.chan)
    }

    fn avrcp_send(&mut self, handle: u16, label: u8, response: bool, frame: &[u8]) {
        let Some(chan) = self.avrcp_chan(handle) else { return };
        let mut p = Vec::with_capacity(3 + frame.len());
        p.push(label << 4 | (response as u8) << 1);
        p.extend_from_slice(&PID_REMOTE.to_be_bytes());
        p.extend_from_slice(frame);
        self.chan_send(handle, chan, &p);
    }

    /// Our command to the headphones (we are the remote).
    fn avrcp_command(&mut self, handle: u16, code: u8, pdu: u8, params: &[u8]) {
        let Some(a) = self.conn_mut(handle).and_then(|c| c.avrcp.as_mut()) else { return };
        let label = a.label;
        a.label = (a.label + 1) & 0x0F;
        self.avrcp_send(handle, label, false, &vendor(code, pdu, params));
    }

    pub(super) fn avrcp_open(&mut self, handle: u16, local: u16) {
        let now = interrupts::ticks();
        let Some(c) = self.conn_mut(handle) else { return };
        let a = c.avrcp.get_or_insert_with(|| Avrcp::new(now));
        // Opened at the same time as the headphones — the second channel is extra.
        if a.open && a.chan != local {
            self.chan_close(handle, local);
            return;
        }
        a.chan = local;
        a.open = true;
        if let Some(addr) = self.addr_of(handle) {
            kprintln!("bt: {} remote control: channel open", addr_text(&addr));
        }
        // What the headphones can report — we need the volume.
        self.avrcp_command(handle, STATUS, GET_CAPABILITIES, &[CAP_EVENTS]);
    }

    pub(super) fn avrcp_closed(&mut self, handle: u16, local: u16) {
        let Some(c) = self.conn_mut(handle) else { return };
        let Some(a) = c.avrcp.as_mut().filter(|a| a.chan == local) else { return };
        a.open = false;
        a.tried = true;
        a.absolute = false;
        a.synced = None;
        a.playback = None;
        if let Some(s) = c.a2dp.as_ref().and_then(|a| a.stream()) {
            s.lock().absolute = false;
        }
    }

    pub(super) fn avrcp_data(&mut self, handle: u16, _local: u16, data: &[u8]) {
        if data.len() < 3 {
            return;
        }
        let label = data[0] >> 4;
        let single = (data[0] >> 2) & 3 == 0;
        let response = data[0] & 2 != 0;
        let pid = u16::from_be_bytes([data[1], data[2]]);
        if pid != PID_REMOTE {
            // A foreign profile: answer "we do not know it" (the IPID flag).
            if !response {
                let Some(chan) = self.avrcp_chan(handle) else { return };
                self.chan_send(handle, chan, &[label << 4 | 0b11, data[1], data[2]]);
            }
            return;
        }
        // We do not need long messages in parts: our commands are short.
        if !single || data.len() < 6 {
            return;
        }
        let frame = &data[3..];
        if response {
            self.avrcp_response(handle, frame);
        } else {
            self.avrcp_request(handle, label, frame);
        }
    }

    /// A command from the headphones (we are the player).
    fn avrcp_request(&mut self, handle: u16, label: u8, frame: &[u8]) {
        let (code, subunit, op, operands) = (frame[0], frame[1], frame[2], &frame[3..]);
        let reply = |h: &mut Host, answer: u8, rest: &[u8]| {
            let mut f = alloc::vec![answer, subunit, op];
            f.extend_from_slice(rest);
            h.avrcp_send(handle, label, true, &f);
        };
        match op {
            OP_UNIT_INFO => reply(self, STABLE, &[0x07, PANEL, BT_SIG[0], BT_SIG[1], BT_SIG[2]]),
            OP_SUBUNIT_INFO => reply(self, STABLE, &[operands.first().copied().unwrap_or(0x07), PANEL, 0xFF, 0xFF, 0xFF]),
            OP_PASS_THROUGH if code == CONTROL && !operands.is_empty() => {
                reply(self, ACCEPTED, operands);
                // A press (a release has the top bit).
                if operands[0] & 0x80 == 0 {
                    self.avrcp_button(handle, operands[0] & 0x7F);
                }
            }
            OP_VENDOR if operands.len() >= 7 && operands[..3] == BT_SIG => {
                let (pdu, params) = (operands[3], &operands[7..]);
                self.avrcp_vendor(handle, label, code, pdu, params);
            }
            _ => reply(self, NOT_IMPLEMENTED, operands),
        }
    }

    fn avrcp_vendor(&mut self, handle: u16, label: u8, code: u8, pdu: u8, params: &[u8]) {
        let answer = |h: &mut Host, answer: u8, p: &[u8]| h.avrcp_send(handle, label, true, &vendor(answer, pdu, p));
        match pdu {
            GET_CAPABILITIES => match params.first().copied() {
                Some(CAP_COMPANY) => answer(self, STABLE, &[CAP_COMPANY, 1, BT_SIG[0], BT_SIG[1], BT_SIG[2]]),
                Some(CAP_EVENTS) => answer(self, STABLE, &[CAP_EVENTS, 2, EVENT_PLAYBACK, EVENT_TRACK]),
                _ => answer(self, REJECTED, &[ERR_INVALID_PARAMETER]),
            },
            REGISTER_NOTIFICATION if code == NOTIFY => match params.first().copied() {
                Some(EVENT_PLAYBACK) => {
                    let playing = player::any_playing();
                    if let Some(a) = self.conn_mut(handle).and_then(|c| c.avrcp.as_mut()) {
                        a.playback = Some((label, playing));
                    }
                    answer(self, INTERIM, &[EVENT_PLAYBACK, playback_status(playing)]);
                }
                // The track: "not selected" (we do not tell the headphones titles yet).
                Some(EVENT_TRACK) => answer(self, INTERIM, &[EVENT_TRACK, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]),
                _ => answer(self, REJECTED, &[ERR_INVALID_PARAMETER]),
            },
            INFORM_CHARSET | INFORM_BATTERY => answer(self, ACCEPTED, &[]),
            _ => answer(self, REJECTED, &[ERR_INVALID_COMMAND]),
        }
    }

    /// A button on the headphones.
    fn avrcp_button(&mut self, handle: u16, op: u8) {
        use player::Media;
        let (name, media, key) = match op {
            0x44 => ("play", Some(Media::Play), None),
            0x46 => ("pause", Some(Media::Pause), None),
            0x45 => ("stop", Some(Media::Stop), None),
            0x4B => ("next", Some(Media::Next), None),
            0x4C => ("previous", Some(Media::Previous), None),
            0x41 => ("volume up", None, Some(input::KEY_VOLUME_UP)),
            0x42 => ("volume down", None, Some(input::KEY_VOLUME_DOWN)),
            0x43 => ("mute", None, Some(input::KEY_MUTE)),
            _ => ("unknown", None, None),
        };
        if let Some(addr) = self.addr_of(handle) {
            kprintln!("bt: {} remote control: button {name} ({op:#04x})", addr_text(&addr));
        }
        if let Some(m) = media {
            player::media(m);
        }
        if let Some(k) = key {
            crate::hid::push(alloc::vec![Event::Key(k)]);
        }
    }

    /// The headphones' answer to our command (we are the remote).
    fn avrcp_response(&mut self, handle: u16, frame: &[u8]) {
        let (code, op) = (frame[0], frame[2]);
        let operands = &frame[3..];
        if op != OP_VENDOR || operands.len() < 7 {
            return;
        }
        let (pdu, params) = (operands[3], &operands[7..]);
        let addr = self.addr_of(handle).map(|a| addr_text(&a)).unwrap_or_default();
        match pdu {
            GET_CAPABILITIES if code == STABLE && params.first() == Some(&CAP_EVENTS) => {
                let n = params.get(1).copied().unwrap_or(0) as usize;
                let events = params.get(2..(2 + n).min(params.len())).unwrap_or(&[]);
                if events.contains(&EVENT_VOLUME) {
                    self.avrcp_command(handle, NOTIFY, REGISTER_NOTIFICATION, &[EVENT_VOLUME, 0, 0, 0, 0]);
                } else {
                    kprintln!("bt: {addr} remote control: no absolute volume (events {events:02x?})");
                }
            }
            REGISTER_NOTIFICATION if params.first() == Some(&EVENT_VOLUME) => {
                let v = params.get(1).copied().unwrap_or(0) & 0x7F;
                let Some(a) = self.conn_mut(handle).and_then(|c| c.avrcp.as_mut()) else { return };
                match code {
                    INTERIM => {
                        // The first subscription: from now on we set the volume (`avrcp_tick` sends ours).
                        if !a.absolute {
                            a.absolute = true;
                            a.synced = None;
                            kprintln!("bt: {addr} remote control: absolute volume, the headphones are at {v}/127");
                        }
                    }
                    CHANGED => {
                        // The volume was turned on the headphones — move the system slider.
                        a.synced = Some(v);
                        let sys = from_absolute(v);
                        audio::set_volume(sys as i32);
                        crate::theme::update(|t| t.sound = sys);
                        // The subscription is one-shot — subscribe again.
                        self.avrcp_command(handle, NOTIFY, REGISTER_NOTIFICATION, &[EVENT_VOLUME, 0, 0, 0, 0]);
                    }
                    _ => {
                        a.absolute = false;
                        kprintln!("bt: {addr} remote control: the headphones refused volume notifications ({code:#04x})");
                    }
                }
            }
            SET_ABSOLUTE_VOLUME if code != ACCEPTED => {
                kprintln!("bt: {addr} remote control: volume not accepted ({code:#04x})");
            }
            _ => {}
        }
    }

    /// Every loop pass: open the remote control if the headphones did not; the system
    /// volume → the headphones; "playing / paused" → subscribed headphones.
    pub(super) fn avrcp_tick(&mut self, now: u64) {
        let handles: Vec<u16> = self.conns.values().filter(|c| c.a2dp.is_some() || c.avrcp.is_some()).filter_map(|c| c.handle).collect();
        for handle in handles {
            let Some(c) = self.conn_mut(handle) else { continue };
            let stream = c.a2dp.as_ref().and_then(|a| a.stream()).cloned();
            if stream.is_none() && c.avrcp.is_none() {
                continue;
            }
            let a = c.avrcp.get_or_insert_with(|| Avrcp::new(now));
            if !a.open {
                if stream.is_some() && !a.tried && now >= a.since + OPEN_DELAY {
                    a.tried = true;
                    if let Some(local) = self.chan_open(handle, PSM_AVCTP)
                        && let Some(a) = self.conn_mut(handle).and_then(|c| c.avrcp.as_mut())
                        && !a.open
                    {
                        a.chan = local;
                    }
                }
                continue;
            }
            if let Some(s) = &stream {
                let mut s = s.lock();
                if s.absolute != a.absolute {
                    s.absolute = a.absolute;
                }
            }
            let volume = a.absolute.then(|| to_absolute(audio::volume())).filter(|v| a.synced != Some(*v));
            if let Some(v) = volume {
                a.synced = Some(v);
            }
            let changed = match a.playback {
                Some((label, was)) if player::any_playing() != was => {
                    a.playback = None;
                    Some((label, !was))
                }
                _ => None,
            };
            if let Some(v) = volume {
                self.avrcp_command(handle, CONTROL, SET_ABSOLUTE_VOLUME, &[v]);
            }
            if let Some((label, playing)) = changed {
                self.avrcp_send(handle, label, true, &vendor(CHANGED, REGISTER_NOTIFICATION, &[EVENT_PLAYBACK, playback_status(playing)]));
            }
        }
    }
}
