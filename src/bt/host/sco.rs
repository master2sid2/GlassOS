//! The voice channel (SCO/eSCO) to a headset: the microphone and the call audio, 8 kHz, 16-bit,
//! the air coding (CVSD) is done by the controller. It is opened when someone records sound
//! (`audio::recording`) and closed when they stop; music (A2DP) is paused meanwhile.
//! A headset with a ready service level connection is a sound input in the system.

use super::Host;
use crate::audio::{self, InputRef};
use crate::bt::{Kind, addr_text, name_of};
use crate::interrupts::{self, HZ};
use crate::kprintln;
use alloc::string::String;
use alloc::vec::Vec;

const SETUP_SYNC: u16 = 0x0428;
const DISCONNECT: u16 = 0x0406;
const RATE: u32 = 8000;
/// Voice is 16-bit, linear, CVSD on the air.
const VOICE_SETTING: u16 = 0x0060;
/// Samples in one SCO packet to the controller (48 bytes — three USB isochronous packets).
const PACKET_SAMPLES: usize = 24;
/// Voice samples per tick.
const SAMPLES_PER_TICK: u64 = RATE as u64 / HZ;
/// Parameter variants: eSCO S1 (EV3), then plain SCO (HV3) — for old headsets.
/// (latency, retransmissions, packet types; bits 6–9 — "do not use EDR").
const SETTINGS: [(u16, u8, u16); 2] = [(0x0007, 0x01, 0x0008 | 0x03C0), (0xFFFF, 0xFF, 0x0004 | 0x03C0)];

/// The voice of one link.
#[derive(Default)]
pub(super) struct Sco {
    pub handle: Option<u16>,
    /// Waiting for the open to complete; which parameter variant is being tried.
    pending: bool,
    attempt: usize,
    /// Opening failed — not tried again until the next recording.
    failed: bool,
    started: u64,
    sent: u64,
    input: Option<InputRef>,
}

impl Host {
    /// The headset is ready: it is a sound input (a microphone).
    pub(super) fn sco_headset_ready(&mut self, handle: u16) {
        let Some(addr) = self.addr_of(handle) else { return };
        let name = name_of(&addr);
        let name = if name.trim().is_empty() { String::from("Bluetooth headset") } else { String::from(name.trim()) };
        let Some(c) = self.conn_mut(handle) else { return };
        let s = c.sco.get_or_insert_with(Sco::default);
        if s.input.is_none() {
            s.input = Some(audio::add_input(&name, RATE));
        }
    }

    /// The link is closed: remove the input.
    pub(super) fn sco_gone(s: &mut Sco) {
        if let Some(i) = s.input.take() {
            audio::remove_input(&i);
        }
    }

    /// Every pass: open voice if someone records, close it if they stopped; send
    /// voice to the headset (silence — we have no call audio yet).
    pub(super) fn sco_tick(&mut self, now: u64) {
        let want = audio::recording();
        let list: Vec<(u16, bool, Option<u16>, bool, bool)> = self
            .conns
            .values()
            .filter_map(|c| {
                let s = c.sco.as_ref()?;
                Some((c.handle?, c.hfp.as_ref().is_some_and(|h| h.ready()), s.handle, s.pending, s.failed))
            })
            .collect();
        for (acl, ready, sco, pending, failed) in list {
            match (want, sco) {
                (true, None) if ready && !pending && !failed => self.sco_open(acl),
                (false, Some(h)) => {
                    let mut p = h.to_le_bytes().to_vec();
                    p.push(0x13);
                    self.outbox.push_back((DISCONNECT, p));
                    if let Some(s) = self.conn_mut(acl).and_then(|c| c.sco.as_mut()) {
                        s.handle = None;
                    }
                }
                (false, None) => {
                    if let Some(s) = self.conn_mut(acl).and_then(|c| c.sco.as_mut()) {
                        s.failed = false;
                    }
                }
                (true, Some(h)) => self.sco_send(acl, h, now),
                _ => {}
            }
        }
    }

    fn sco_open(&mut self, acl: u16) {
        let Some(s) = self.conn_mut(acl).and_then(|c| c.sco.as_mut()) else { return };
        let Some(&(latency, retrans, types)) = SETTINGS.get(s.attempt) else {
            s.failed = true;
            return;
        };
        s.pending = true;
        let mut p = acl.to_le_bytes().to_vec();
        p.extend_from_slice(&8000u32.to_le_bytes());
        p.extend_from_slice(&8000u32.to_le_bytes());
        p.extend_from_slice(&latency.to_le_bytes());
        p.extend_from_slice(&VOICE_SETTING.to_le_bytes());
        p.push(retrans);
        p.extend_from_slice(&types.to_le_bytes());
        self.outbox.push_back((SETUP_SYNC, p));
    }

    /// The "voice channel opened" event (or not).
    pub(super) fn sco_complete(&mut self, status: u8, sco: u16, addr: [u8; 6]) {
        let Some(acl) = self.conns.get(&addr).and_then(|c| c.handle) else { return };
        let Some(s) = self.conn_mut(acl).and_then(|c| c.sco.as_mut()) else { return };
        s.pending = false;
        if status != 0 {
            s.attempt += 1;
            kprintln!("bt: {} voice: refused ({}), {}", addr_text(&addr), super::super::reason(status), if s.attempt < SETTINGS.len() { "trying plain SCO" } else { "giving up" });
            if s.attempt >= SETTINGS.len() {
                s.failed = true;
                s.attempt = 0;
            }
            return;
        }
        s.handle = Some(sco);
        s.attempt = 0;
        s.started = interrupts::ticks();
        s.sent = 0;
        kprintln!("bt: {} voice: open (8 kHz), the microphone works", addr_text(&addr));
        self.hci.voice(true);
        self.audio_pause(acl, true);
    }

    /// A voice channel closed (our handle). true — it was this one.
    pub(super) fn sco_closed(&mut self, sco: u16) -> bool {
        let acl = self.conns.values().find(|c| c.sco.as_ref().is_some_and(|s| s.handle == Some(sco))).and_then(|c| c.handle);
        let was_ours = acl.is_some() || self.conns.values().any(|c| c.sco.is_some());
        if let Some(acl) = acl
            && let Some(s) = self.conn_mut(acl).and_then(|c| c.sco.as_mut())
        {
            s.handle = None;
        }
        if !was_ours {
            return false;
        }
        if !self.conns.values().any(|c| c.sco.as_ref().is_some_and(|s| s.handle.is_some())) {
            self.hci.voice(false);
            let acls: Vec<u16> = self.conns.values().filter(|c| c.sco.is_some()).filter_map(|c| c.handle).collect();
            for acl in acls {
                self.audio_pause(acl, false);
            }
            kprintln!("bt: voice: closed");
        }
        acl.is_some()
    }

    /// A voice packet from the controller: the samples go to the sound input.
    pub(super) fn sco_data(&mut self, packet: &[u8]) {
        if packet.len() < 3 {
            return;
        }
        let sco = u16::from_le_bytes([packet[0], packet[1]]) & 0x0FFF;
        let data = &packet[3..(3 + packet[2] as usize).min(packet.len())];
        let samples: Vec<i16> = data.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        if let Some(i) = self.conns.values().find_map(|c| c.sco.as_ref().filter(|s| s.handle == Some(sco)).and_then(|s| s.input.clone())) {
            i.lock().push(&samples);
        }
    }

    /// Voice to the headset at the pace of the clock (8000 samples per second).
    fn sco_send(&mut self, acl: u16, sco: u16, now: u64) {
        let Some(s) = self.conn_mut(acl).and_then(|c| c.sco.as_mut()) else { return };
        let due = now.saturating_sub(s.started) * SAMPLES_PER_TICK + PACKET_SAMPLES as u64 * 2;
        let mut packets = Vec::new();
        while s.sent + PACKET_SAMPLES as u64 <= due {
            let mut p = sco.to_le_bytes().to_vec();
            p.push((PACKET_SAMPLES * 2) as u8);
            p.resize(3 + PACKET_SAMPLES * 2, 0);
            packets.push(p);
            s.sent += PACKET_SAMPLES as u64;
        }
        for p in packets {
            self.hci.send(Kind::Sco, p);
        }
    }
}
