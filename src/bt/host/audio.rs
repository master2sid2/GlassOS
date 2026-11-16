//! Sound over Bluetooth: music to headphones and speakers — the A2DP profile (we are the source).
//! AVDTP signalling over its own L2CAP channel (PSM 0x19), the stream over a second such
//! channel: RTP packets with SBC frames. We start ourselves (found an audio sink over SDP) or
//! answer when the headphones connect to us. The sound is taken from the mixer stream
//! (`audio::open_stream`) at the pace of the clock: 480 frames per tick at 48 kHz.

use super::Host;
use super::chan::PSM_AVDTP;
use super::sdp::{Service, UUID_AUDIO_SINK};
use crate::audio::{self, StreamRef, sbc};
use crate::bt::{addr_text, name_of};
use crate::interrupts::{self, HZ};
use crate::kprintln;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

// AVDTP signals.
const DISCOVER: u8 = 0x01;
const GET_CAPABILITIES: u8 = 0x02;
const SET_CONFIGURATION: u8 = 0x03;
const GET_CONFIGURATION: u8 = 0x04;
const OPEN: u8 = 0x06;
const START: u8 = 0x07;
const CLOSE: u8 = 0x08;
const SUSPEND: u8 = 0x09;
const ABORT: u8 = 0x0A;
const GET_ALL_CAPABILITIES: u8 = 0x0C;
const DELAY_REPORT: u8 = 0x0D;
// The message type (the low 2 bits of the first byte).
const COMMAND: u8 = 0;
const GENERAL_REJECT: u8 = 1;
const ACCEPT: u8 = 2;
const REJECT: u8 = 3;
// Endpoint capabilities.
const CAP_MEDIA_TRANSPORT: u8 = 0x01;
const CAP_MEDIA_CODEC: u8 = 0x07;
const MEDIA_AUDIO: u8 = 0x00;
const CODEC_SBC: u8 = 0x00;
const TSEP_SINK: u8 = 1;
const BAD_STATE: u8 = 0x31;
const NOT_SUPPORTED: u8 = 0x29;
/// Our endpoint: an audio source.
const OUR_SEID: u8 = 1;
// SBC: 48 kHz, stereo, 16 blocks, 8 subbands, "loudness" allocation.
const SBC_48K: u8 = 0x10;
const SBC_STEREO: u8 = 0x02;
const SBC_16_8_LOUDNESS: u8 = 0x10 | 0x04 | 0x01;
/// We do not take a bitpool larger than this: stereo 48 kHz "high quality" (~345 kbit/s).
const BITPOOL_MAX: u8 = 53;
/// Packet headers: RTP (12) and SBC (1).
const MEDIA_HEADER: usize = 13;
/// Audio frames per timer tick (48 kHz, HZ = 100).
const FRAMES_PER_TICK: u64 = audio::RATE as u64 / HZ;
/// We do not queue more ACL packets than this for the controller.
const MAX_BACKLOG: usize = 48;
/// How much to send right away at the start, so the headphones have a reserve (~60 ms).
const HEAD_START: u64 = audio::RATE as u64 * 60 / 1000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Idle,
    Discovering,
    Capabilities,
    Configuring,
    Opening,
    /// Open, waiting for the stream channel.
    Open,
    Starting,
    Streaming,
}

/// A2DP of one link.
pub(super) struct A2dp {
    signal: Option<u16>,
    media: Option<u16>,
    phase: Phase,
    /// We started (otherwise the headphones did).
    initiator: bool,
    label: u8,
    /// The device's sink endpoints (candidates) and the chosen one.
    sinks: Vec<u8>,
    remote: u8,
    /// The device's AVDTP is at least 1.3 — "get all capabilities" exists.
    v13: bool,
    bitpool: u8,
    stream: Option<StreamRef>,
    encoder: Option<sbc::Encoder>,
    started: u64,
    sent: u64,
    seq: u16,
    /// When the signalling opened: if the device stays silent — we start ourselves.
    signal_at: u64,
}

impl A2dp {
    /// The stream to the headphones (while music plays).
    pub(super) fn stream(&self) -> Option<&StreamRef> {
        self.stream.as_ref()
    }

    fn new(initiator: bool) -> A2dp {
        A2dp { signal: None, media: None, phase: Phase::Idle, initiator, label: 0, sinks: Vec::new(), remote: 0, v13: false, bitpool: BITPOOL_MAX, stream: None, encoder: None, started: 0, sent: 0, seq: 0, signal_at: 0 }
    }
}

/// Our capabilities: transport and SBC (what our encoder can do).
fn our_capabilities() -> Vec<u8> {
    vec![CAP_MEDIA_TRANSPORT, 0, CAP_MEDIA_CODEC, 6, MEDIA_AUDIO << 4, CODEC_SBC, SBC_48K | SBC_STEREO, SBC_16_8_LOUDNESS, 2, BITPOOL_MAX]
}

/// From the capability list — the SBC bytes (rates/modes, blocks/subbands/method, bitpool).
fn sbc_of(caps: &[u8]) -> Option<[u8; 4]> {
    let mut at = 0;
    while at + 2 <= caps.len() {
        let (cat, len) = (caps[at], caps[at + 1] as usize);
        let body = caps.get(at + 2..at + 2 + len)?;
        if cat == CAP_MEDIA_CODEC && len >= 6 && body[1] == CODEC_SBC {
            return Some([body[2], body[3], body[4], body[5]]);
        }
        at += 2 + len;
    }
    None
}

impl Host {
    /// The classic Bluetooth link is encrypted: does the device have an audio sink?
    pub(super) fn audio_connected(&mut self, handle: u16) {
        self.sdp_query(handle, UUID_AUDIO_SINK);
        self.sdp_query(handle, super::sdp::UUID_HANDSFREE);
    }

    /// An SDP answer: an audio sink was found — start A2DP (unless the device started by itself).
    pub(super) fn audio_sdp(&mut self, handle: u16, uuid: u16, services: &[Service]) {
        if uuid != UUID_AUDIO_SINK || services.is_empty() {
            return;
        }
        let Some(c) = self.conn_mut(handle) else { return };
        if c.a2dp.is_some() {
            return;
        }
        let mut a = A2dp::new(true);
        a.v13 = services.iter().any(|s| s.version >= 0x0103);
        c.a2dp = Some(a);
        let local = self.chan_open(handle, PSM_AVDTP);
        if let Some(a) = self.conn_mut(handle).and_then(|c| c.a2dp.as_mut()) {
            a.signal = local;
        }
    }

    fn a2dp(&mut self, handle: u16) -> Option<&mut A2dp> {
        self.conn_mut(handle).and_then(|c| c.a2dp.as_mut())
    }

    /// An AVDTP signal: label, type, id, parameters.
    fn avdtp_send(&mut self, handle: u16, label: u8, kind: u8, signal: u8, params: &[u8]) {
        let Some(local) = self.a2dp(handle).and_then(|a| a.signal) else { return };
        let mut p = vec![label << 4 | kind, signal];
        p.extend_from_slice(params);
        self.chan_send(handle, local, &p);
    }

    fn avdtp_command(&mut self, handle: u16, signal: u8, params: &[u8]) {
        let Some(a) = self.a2dp(handle) else { return };
        a.label = (a.label + 1) & 0x0F;
        let label = a.label;
        self.avdtp_send(handle, label, COMMAND, signal, params);
    }

    pub(super) fn avdtp_open(&mut self, handle: u16, local: u16) {
        let Some(c) = self.conn_mut(handle) else { return };
        // The first AVDTP channel is signalling, the second the stream.
        let a = c.a2dp.get_or_insert_with(|| A2dp::new(false));
        if a.signal.is_none() || a.signal == Some(local) {
            a.signal = Some(local);
            a.signal_at = interrupts::ticks();
            if a.initiator && a.phase == Phase::Idle {
                a.phase = Phase::Discovering;
                self.avdtp_command(handle, DISCOVER, &[]);
            }
            return;
        }
        // A stream channel only exists after OPEN; otherwise this is a second signalling channel
        // (opened at the same time as the device) — close the extra one.
        if a.phase != Phase::Open || a.media.is_some() {
            self.chan_close(handle, local);
            return;
        }
        a.media = Some(local);
        // The audio channel is open — the source (us) starts the stream, whoever led the setup.
        if a.phase == Phase::Open {
            a.phase = Phase::Starting;
            let seid = a.remote;
            self.avdtp_command(handle, START, &[seid << 2]);
        }
    }

    pub(super) fn avdtp_closed(&mut self, handle: u16, local: u16) {
        let Some(a) = self.a2dp(handle) else { return };
        if a.media == Some(local) {
            a.media = None;
            self.audio_stop(handle);
            if let Some(a) = self.a2dp(handle) {
                a.phase = Phase::Idle;
            }
        } else if a.signal == Some(local) {
            self.audio_stop(handle);
            if let Some(c) = self.conn_mut(handle) {
                c.a2dp = None;
            }
        }
    }

    pub(super) fn avdtp_data(&mut self, handle: u16, local: u16, p: &[u8]) {
        if self.a2dp(handle).and_then(|a| a.signal) != Some(local) || p.len() < 2 {
            return;
        }
        let (label, kind, signal) = (p[0] >> 4, p[0] & 3, p[1] & 0x3F);
        let params = &p[2..];
        match kind {
            COMMAND => self.avdtp_acceptor(handle, label, signal, params),
            ACCEPT => self.avdtp_accepted(handle, signal, params),
            REJECT | GENERAL_REJECT => {
                if let Some(addr) = self.addr_of(handle) {
                    kprintln!("bt: {} audio: the headphones refused step {signal:#04x} (error {:#04x})", addr_text(&addr), params.last().copied().unwrap_or(0));
                }
                self.avdtp_next_sink(handle, signal);
            }
            _ => {}
        }
    }

    /// The sink selection was refused — try the next one.
    fn avdtp_next_sink(&mut self, handle: u16, signal: u8) {
        if !matches!(signal, GET_CAPABILITIES | GET_ALL_CAPABILITIES | SET_CONFIGURATION) {
            return;
        }
        let Some(a) = self.a2dp(handle) else { return };
        if a.sinks.is_empty() {
            a.phase = Phase::Idle;
            return;
        }
        a.remote = a.sinks.remove(0);
        a.phase = Phase::Capabilities;
        let (seid, all) = (a.remote, a.v13);
        self.avdtp_command(handle, if all { GET_ALL_CAPABILITIES } else { GET_CAPABILITIES }, &[seid << 2]);
    }

    /// Answers to our commands (we started).
    fn avdtp_accepted(&mut self, handle: u16, signal: u8, params: &[u8]) {
        match signal {
            DISCOVER => {
                // 2 bytes each: id (6 bits), in use; type (4 bits), role (1 — sink).
                let sinks: Vec<u8> = params
                    .chunks_exact(2)
                    .filter(|e| e[0] & 0x02 == 0 && e[1] >> 4 == MEDIA_AUDIO && (e[1] >> 3) & 1 == TSEP_SINK)
                    .map(|e| e[0] >> 2)
                    .collect();
                if let Some(a) = self.a2dp(handle) {
                    a.sinks = sinks;
                }
                self.avdtp_next_sink(handle, GET_CAPABILITIES);
            }
            GET_CAPABILITIES | GET_ALL_CAPABILITIES => {
                let Some(theirs) = sbc_of(params) else {
                    self.avdtp_next_sink(handle, signal);
                    return;
                };
                // 48 kHz, stereo, 16 blocks, 8 subbands, "loudness" — mandatory for an SBC sink.
                if theirs[0] & SBC_48K == 0 || theirs[0] & SBC_STEREO == 0 || theirs[1] & SBC_16_8_LOUDNESS != SBC_16_8_LOUDNESS {
                    if let Some(addr) = self.addr_of(handle) {
                        kprintln!("bt: {} audio: SBC without 48 kHz stereo ({:02x} {:02x}) — not supported yet", addr_text(&addr), theirs[0], theirs[1]);
                    }
                    self.avdtp_next_sink(handle, signal);
                    return;
                }
                let (min, max) = (theirs[2].max(2), theirs[3].min(BITPOOL_MAX));
                let Some(a) = self.a2dp(handle) else { return };
                a.bitpool = max.max(min);
                a.phase = Phase::Configuring;
                let mut p = vec![a.remote << 2, OUR_SEID << 2, CAP_MEDIA_TRANSPORT, 0];
                p.extend_from_slice(&[CAP_MEDIA_CODEC, 6, MEDIA_AUDIO << 4, CODEC_SBC, SBC_48K | SBC_STEREO, SBC_16_8_LOUDNESS, min, max]);
                self.avdtp_command(handle, SET_CONFIGURATION, &p);
            }
            SET_CONFIGURATION => {
                let Some(a) = self.a2dp(handle) else { return };
                a.phase = Phase::Opening;
                let seid = a.remote;
                self.avdtp_command(handle, OPEN, &[seid << 2]);
            }
            OPEN => {
                if let Some(a) = self.a2dp(handle) {
                    a.phase = Phase::Open;
                }
                let local = self.chan_open(handle, PSM_AVDTP);
                if local.is_none() {
                    self.audio_stop(handle);
                }
            }
            START => self.audio_start(handle),
            SUSPEND | CLOSE | ABORT => self.audio_stop(handle),
            _ => {}
        }
    }

    /// Commands from the headphones (they started) — we answer as the source.
    fn avdtp_acceptor(&mut self, handle: u16, label: u8, signal: u8, params: &[u8]) {
        // The device leads by itself — we do not start our own setup.
        if let Some(a) = self.a2dp(handle).filter(|a| a.phase == Phase::Idle) {
            a.phase = Phase::Capabilities;
        }
        let accept = |h: &mut Host, data: &[u8]| h.avdtp_send(handle, label, ACCEPT, signal, data);
        match signal {
            DISCOVER => accept(self, &[OUR_SEID << 2, MEDIA_AUDIO << 4]),
            GET_CAPABILITIES | GET_ALL_CAPABILITIES => accept(self, &our_capabilities()),
            SET_CONFIGURATION if params.len() >= 2 => {
                let caps = &params[2..];
                let cfg = sbc_of(caps);
                let ok = cfg.is_some_and(|c| c[0] & SBC_48K != 0 && c[0] & SBC_STEREO != 0 && c[1] & SBC_16_8_LOUDNESS == SBC_16_8_LOUDNESS);
                if !ok {
                    kprintln!("bt: audio: refusing their SBC setting {cfg:02x?}");
                    // The faulty category, the code "not supported".
                    self.avdtp_send(handle, label, REJECT, signal, &[CAP_MEDIA_CODEC, NOT_SUPPORTED]);
                    return;
                }
                let bitpool = cfg.map_or(BITPOOL_MAX, |c| c[3].min(BITPOOL_MAX).max(c[2]));
                if let Some(a) = self.a2dp(handle) {
                    a.remote = params[1] >> 2;
                    a.bitpool = bitpool;
                    a.phase = Phase::Configuring;
                }
                accept(self, &[]);
            }
            GET_CONFIGURATION => {
                let bp = self.a2dp(handle).map_or(BITPOOL_MAX, |a| a.bitpool);
                accept(self, &[CAP_MEDIA_TRANSPORT, 0, CAP_MEDIA_CODEC, 6, MEDIA_AUDIO << 4, CODEC_SBC, SBC_48K | SBC_STEREO, SBC_16_8_LOUDNESS, 2, bp]);
            }
            OPEN => {
                if let Some(a) = self.a2dp(handle) {
                    a.phase = Phase::Open;
                }
                accept(self, &[]);
            }
            START => {
                if self.a2dp(handle).is_some_and(|a| a.media.is_some()) {
                    accept(self, &[]);
                    self.audio_start(handle);
                } else {
                    self.avdtp_send(handle, label, REJECT, signal, &[params.first().copied().unwrap_or(0), BAD_STATE]);
                }
            }
            SUSPEND | CLOSE | ABORT => {
                accept(self, &[]);
                self.audio_stop(handle);
                if signal != SUSPEND
                    && let Some(a) = self.a2dp(handle)
                {
                    a.phase = Phase::Idle;
                }
            }
            DELAY_REPORT => accept(self, &[]),
            _ => self.avdtp_send(handle, label, GENERAL_REJECT, signal, &[]),
        }
    }

    /// The stream started: the system sound goes to the headphones.
    fn audio_start(&mut self, handle: u16) {
        let Some(addr) = self.addr_of(handle) else { return };
        let name = name_of(&addr);
        let name = if name.trim().is_empty() { String::from("Bluetooth headphones") } else { String::from(name.trim()) };
        let Some(a) = self.a2dp(handle) else { return };
        if a.phase == Phase::Streaming {
            return;
        }
        a.phase = Phase::Streaming;
        a.encoder = Some(sbc::Encoder::new(sbc::Rate::Hz48000, a.bitpool));
        a.stream = Some(audio::open_stream(&name));
        a.started = interrupts::ticks();
        a.sent = 0;
        let bitpool = a.bitpool;
        kprintln!("bt: {} audio: streaming SBC 48 kHz stereo, bitpool {bitpool}", addr_text(&addr));
    }

    fn audio_stop(&mut self, handle: u16) {
        let Some(a) = self.a2dp(handle) else { return };
        if let Some(s) = a.stream.take() {
            audio::close_stream(&s);
        }
        a.encoder = None;
        if a.phase == Phase::Streaming {
            a.phase = Phase::Open;
        }
    }

    /// Voice opened (`pause`) — pause the music; closed — continue.
    pub(super) fn audio_pause(&mut self, handle: u16, pause: bool) {
        let Some(a) = self.a2dp(handle) else { return };
        let seid = a.remote;
        match (pause, a.phase) {
            (true, Phase::Streaming) => {
                self.avdtp_command(handle, SUSPEND, &[seid << 2]);
                self.audio_stop(handle);
            }
            (false, Phase::Open) if a.media.is_some() => {
                a.phase = Phase::Starting;
                self.avdtp_command(handle, START, &[seid << 2]);
            }
            _ => {}
        }
    }

    /// The link dropped: the sound goes back to the card.
    pub(super) fn audio_gone(a: &mut A2dp) {
        if let Some(s) = a.stream.take() {
            audio::close_stream(&s);
        }
    }

    /// Every loop pass: send the frames that are due by the clock.
    pub(super) fn audio_tick(&mut self, now: u64) {
        // The device opened signalling and stays silent (the headphones wait for a source) — we start ourselves.
        let quiet: Vec<u16> = self
            .conns
            .values()
            .filter(|c| c.a2dp.as_ref().is_some_and(|a| a.phase == Phase::Idle && a.signal.is_some() && !a.initiator && now >= a.signal_at + HZ))
            .filter_map(|c| c.handle)
            .collect();
        for handle in quiet {
            if let Some(a) = self.a2dp(handle) {
                a.initiator = true;
                a.phase = Phase::Discovering;
            }
            self.avdtp_command(handle, DISCOVER, &[]);
        }
        let handles: Vec<u16> = self.conns.values().filter(|c| c.a2dp.as_ref().is_some_and(|a| a.phase == Phase::Streaming)).filter_map(|c| c.handle).collect();
        for handle in handles {
            self.audio_pump(handle, now);
        }
    }

    fn audio_pump(&mut self, handle: u16, now: u64) {
        let Some(local) = self.a2dp(handle).and_then(|a| a.media) else { return };
        // The controller cannot keep up — wait (otherwise the packets would pile up endlessly).
        if self.acl_backlog() > MAX_BACKLOG {
            return;
        }
        let mtu = self.chan_mtu(handle, local) as usize;
        loop {
            let Some(a) = self.a2dp(handle) else { return };
            let (Some(enc), Some(stream)) = (a.encoder.as_mut(), a.stream.clone()) else { return };
            let per_packet = ((mtu.saturating_sub(MEDIA_HEADER)) / enc.frame_len()).clamp(1, 15);
            let frames = (per_packet * sbc::FRAME_SAMPLES) as u64;
            let due = now.saturating_sub(a.started) * FRAMES_PER_TICK + HEAD_START;
            if a.sent + frames > due {
                return;
            }
            let mut pcm = Vec::with_capacity(frames as usize * 2);
            stream.lock().take(frames as usize, &mut pcm);
            // RTP: version 2, type 96, sequence, time in samples, source 1; then the number of frames.
            let mut pkt = Vec::with_capacity(MEDIA_HEADER + per_packet * enc.frame_len());
            pkt.extend_from_slice(&[0x80, 0x60]);
            pkt.extend_from_slice(&a.seq.to_be_bytes());
            pkt.extend_from_slice(&(a.sent as u32).to_be_bytes());
            pkt.extend_from_slice(&1u32.to_be_bytes());
            pkt.push(per_packet as u8);
            let t0 = crate::arch::rdtsc();
            for chunk in pcm.chunks_exact(sbc::FRAME_SAMPLES * 2) {
                enc.encode(chunk, &mut pkt);
            }
            audio::diag::ENCODE.fetch_add(crate::arch::rdtsc() - t0, core::sync::atomic::Ordering::Relaxed);
            a.seq = a.seq.wrapping_add(1);
            a.sent += frames;
            self.chan_send(handle, local, &pkt);
        }
    }
}
