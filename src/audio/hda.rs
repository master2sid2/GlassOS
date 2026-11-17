//! Intel High Definition Audio — the sound of all modern PCs.
//!
//! The controller (on PCI) only moves data: the CORB/RIRB command rings for talking
//! to codecs and DMA streams with sound. Everything analog is in the codec (Realtek and the like):
//! it is a graph of nodes (DACs, mixers, selectors, output "pins"). We find the outputs
//! (speakers, headphones, line out), lay a path from each to a DAC,
//! remove "mute" along the way and feed our stream to the DAC: 48 kHz, 16-bit, stereo.
//!
//! Recording goes the other way: from a microphone pin (the headset jack, a built-in
//! microphone, line in) through selectors to an ADC, whose stream lands in a ring in memory.
//! The jack is watched like the headphone jack: plug a headset in and recording switches to it,
//! pull it out and the built-in microphone (if any) takes over.

use super::{BUFFER_BYTES, BUFFERS, Hw, InputRef, RATE};
use crate::interrupts::{self, HZ};
use crate::kprintln;
use crate::pci;
use alloc::alloc::{Layout, alloc_zeroed};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

// Controller registers.
const GCAP: usize = 0x00;
const GCTL: usize = 0x08;
const STATESTS: usize = 0x0E;
const INTCTL: usize = 0x20;
const CORBLBASE: usize = 0x40;
const CORBUBASE: usize = 0x44;
const CORBWP: usize = 0x48;
const CORBRP: usize = 0x4A;
const CORBCTL: usize = 0x4C;
const CORBSIZE: usize = 0x4E;
const RIRBLBASE: usize = 0x50;
const RIRBUBASE: usize = 0x54;
const RIRBWP: usize = 0x58;
const RINTCNT: usize = 0x5A;
const RIRBCTL: usize = 0x5C;
const RIRBSTS: usize = 0x5D;
const RIRBSIZE: usize = 0x5E;

// Stream registers (relative to its block).
const SD_CTL: usize = 0x00;
const SD_STS: usize = 0x03;
const SD_LPIB: usize = 0x04;
const SD_CBL: usize = 0x08;
const SD_LVI: usize = 0x0C;
const SD_FMT: usize = 0x12;
const SD_BDPL: usize = 0x18;
const SD_BDPU: usize = 0x1C;

// Codec commands.
const GET_PARAM: u32 = 0xF00;
const GET_CONN_LIST: u32 = 0xF02;
const SET_CONN_SELECT: u32 = 0x701;
const SET_POWER: u32 = 0x705;
const SET_STREAM: u32 = 0x706;
const SET_PIN_CTL: u32 = 0x707;
const SET_EAPD: u32 = 0x70C;
const GET_PIN_SENSE: u32 = 0xF09;
const EXEC_PIN_SENSE: u32 = 0x709;
const GET_CONFIG: u32 = 0xF1C;
// Four-bit commands (with a 16-bit argument).
const SET_FORMAT: u32 = 0x2;
const SET_AMP: u32 = 0x3;

// Parameters.
const P_VENDOR: u32 = 0x00;
const P_NODES: u32 = 0x04;
const P_FG_TYPE: u32 = 0x05;
const P_WIDGET_CAPS: u32 = 0x09;
const P_PIN_CAPS: u32 = 0x0C;
const P_IN_AMP: u32 = 0x0D;
const P_CONN_LEN: u32 = 0x0E;
const P_OUT_AMP: u32 = 0x12;

// Node types.
const W_OUTPUT: u32 = 0;
const W_INPUT: u32 = 1;
const W_MIXER: u32 = 2;
const W_SELECTOR: u32 = 3;
const W_PIN: u32 = 4;

/// 48 kHz, 16-bit, 2 channels.
const FORMAT: u16 = 0x0011;
/// Our stream number (the tag by which the DAC recognises its data).
const STREAM_TAG: u32 = 1;
/// The recording stream's tag (the ADC sends its data under it).
const CAPTURE_TAG: u32 = 2;
/// The recording ring: 16 pieces of 4 KiB, ~340 ms at 48 kHz stereo — the mixer thread
/// collects it every 10 ms, so a late tick loses nothing.
const CAPTURE_BUFS: usize = 16;
const CAPTURE_BYTES: usize = 4096;
/// Do not read the last bytes before the DMA position: the controller may not have written them yet.
const CAPTURE_GUARD: usize = 256;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Bdle {
    addr: u64,
    len: u32,
    flags: u32,
}

/// A codec output: a pin and its purpose (0 — line out, 1 — speaker, 2 — headphones).
struct OutPin {
    codec: u32,
    nid: u32,
    device: u32,
    /// The pin needs current for headphones (the HP bit in the pin control).
    hp_drive: bool,
    /// Can detect whether a plug is inserted; whether that needs a request.
    sense: bool,
    trigger: bool,
}

/// A codec input: a microphone or line-in pin and how to route it to an ADC.
struct InPin {
    codec: u32,
    nid: u32,
    /// The ADC at the start of the path.
    adc: u32,
    name: &'static str,
    /// A jack whose plug can be sensed (a built-in microphone is always there).
    jack: bool,
    trigger: bool,
    /// The commands that route this pin to its ADC and our capture stream.
    route: Vec<(u32, u32, u32)>,
}

pub struct Hda {
    pins: Vec<OutPin>,
    inputs: Vec<InPin>,
    /// The input recorded from now (index in `inputs`) and its entry in the system's input list.
    mic: Option<(usize, InputRef)>,
    mic_checked: u64,
    /// The register block of the capture stream (0 — the controller has none).
    cap_sd: usize,
    cap_bdl: *mut Bdle,
    cap_buf: *mut u8,
    /// How far into the ring we have read, in bytes; whether the stream runs.
    cap_read: usize,
    cap_running: bool,
    /// Headphones are plugged in (speakers off).
    headphones: bool,
    jack_checked: u64,
    base: usize,
    /// The register block of the output stream.
    sd: usize,
    corb: *mut u32,
    rirb: *mut u64,
    rirb_rp: u16,
    bdl: *mut Bdle,
}

unsafe impl Send for Hda {}

fn dma(bytes: usize) -> *mut u8 {
    unsafe { alloc_zeroed(Layout::from_size_align(bytes, 128).unwrap()) }
}

fn wait(ms: u64) {
    let t = interrupts::ticks();
    let ticks = (ms * HZ).div_ceil(1000) + 1;
    while interrupts::ticks() < t + ticks {
        core::hint::spin_loop();
    }
}

/// Waits for a condition for up to `ms` milliseconds. false — it did not happen.
fn until(ms: u64, mut f: impl FnMut() -> bool) -> bool {
    let t = interrupts::ticks();
    let ticks = (ms * HZ).div_ceil(1000) + 1;
    while interrupts::ticks() < t + ticks {
        if f() {
            return true;
        }
        core::hint::spin_loop();
    }
    f()
}

impl Hda {
    fn r8(&self, r: usize) -> u8 {
        unsafe { ((self.base + r) as *const u8).read_volatile() }
    }
    fn r16(&self, r: usize) -> u16 {
        unsafe { ((self.base + r) as *const u16).read_volatile() }
    }
    fn r32(&self, r: usize) -> u32 {
        unsafe { ((self.base + r) as *const u32).read_volatile() }
    }
    fn w8(&self, r: usize, v: u8) {
        unsafe { ((self.base + r) as *mut u8).write_volatile(v) }
    }
    fn w16(&self, r: usize, v: u16) {
        unsafe { ((self.base + r) as *mut u16).write_volatile(v) }
    }
    fn w32(&self, r: usize, v: u32) {
        unsafe { ((self.base + r) as *mut u32).write_volatile(v) }
    }

    /// Resets the controller; returns the mask of the codecs found.
    fn reset(&self) -> Option<u16> {
        self.w8(CORBCTL, 0);
        self.w8(RIRBCTL, 0);
        self.w32(GCTL, self.r32(GCTL) & !1);
        if !until(100, || self.r32(GCTL) & 1 == 0) {
            return None;
        }
        wait(1);
        self.w32(GCTL, self.r32(GCTL) | 1);
        if !until(100, || self.r32(GCTL) & 1 != 0) {
            return None;
        }
        // Codecs need time to announce themselves (no less than 521 µs).
        wait(5);
        Some(self.r16(STATESTS) & 0x7FFF)
    }

    /// Command rings: CORB (to the codecs) and RIRB (answers), 256 entries each.
    fn setup_rings(&mut self) {
        let corb = self.corb as u64;
        let rirb = self.rirb as u64;
        self.w32(CORBLBASE, corb as u32);
        self.w32(CORBUBASE, (corb >> 32) as u32);
        // Size 256 if supported (capability bit 6), otherwise whatever there is.
        let caps = self.r8(CORBSIZE);
        self.w8(CORBSIZE, if caps & 0x40 != 0 { 2 } else if caps & 0x20 != 0 { 1 } else { 0 });
        self.w16(CORBWP, 0);
        self.w16(CORBRP, 0x8000);
        // Not all controllers show the reset bit — wait only briefly.
        until(10, || self.r16(CORBRP) & 0x8000 != 0);
        self.w16(CORBRP, 0);
        until(10, || self.r16(CORBRP) & 0x8000 == 0);

        self.w32(RIRBLBASE, rirb as u32);
        self.w32(RIRBUBASE, (rirb >> 32) as u32);
        let caps = self.r8(RIRBSIZE);
        self.w8(RIRBSIZE, if caps & 0x40 != 0 { 2 } else if caps & 0x20 != 0 { 1 } else { 0 });
        self.w16(RIRBWP, 0x8000);
        self.w16(RINTCNT, 1);
        self.rirb_rp = 0;
        self.w8(CORBCTL, 2);
        // DMA + the "response arrived" flag (the interrupts themselves are off in INTCTL). Without
        // the flag some controllers (and QEMU) stop after RINTCNT responses.
        self.w8(RIRBCTL, 3);
    }

    /// A command to codec `codec`, node `nid`; the answer or None (the codec is silent).
    fn cmd(&mut self, codec: u32, nid: u32, verb: u32, payload: u32) -> Option<u32> {
        let word = if verb <= 0xF {
            codec << 28 | nid << 20 | verb << 16 | (payload & 0xFFFF)
        } else {
            codec << 28 | nid << 20 | verb << 8 | (payload & 0xFF)
        };
        let wp = (self.r16(CORBWP) as usize + 1) % 256;
        unsafe { self.corb.add(wp).write_volatile(word) };
        self.w16(CORBWP, wp as u16);
        loop {
            let ok = until(50, || self.r16(RIRBWP) & 0xFF != self.rirb_rp);
            if !ok {
                return None;
            }
            self.rirb_rp = (self.rirb_rp + 1) % 256;
            let e = unsafe { self.rirb.add(self.rirb_rp as usize).read_volatile() };
            self.w8(RIRBSTS, 5);
            // Unsolicited responses (for example, "headphones plugged in") are skipped.
            if (e >> 32) & 0x10 == 0 {
                return Some(e as u32);
            }
        }
    }

    fn param(&mut self, codec: u32, nid: u32, p: u32) -> u32 {
        self.cmd(codec, nid, GET_PARAM, p).unwrap_or(0)
    }

    /// The node's inputs (the connection list).
    fn connections(&mut self, codec: u32, nid: u32) -> Vec<u32> {
        let len = self.param(codec, nid, P_CONN_LEN);
        let (count, long) = (len & 0x7F, len & 0x80 != 0);
        let per = if long { 2 } else { 4 };
        let mut out: Vec<u32> = Vec::new();
        let mut i = 0;
        while i < count {
            let r = self.cmd(codec, nid, GET_CONN_LIST, i).unwrap_or(0);
            for k in 0..per {
                if i + k >= count {
                    break;
                }
                let (bits, mask) = if long { (16, 0xFFFF) } else { (8, 0xFF) };
                let e = (r >> (k * bits)) & mask;
                let range = e & (1 << (bits - 1)) != 0;
                let id = e & (mask >> 1);
                // A range "from the previous one to this one".
                if range && let Some(&prev) = out.last() {
                    out.extend(prev + 1..=id);
                } else {
                    out.push(id);
                }
            }
            i += per;
        }
        out
    }
}

/// A codec node — what is needed for path finding.
struct Widget {
    kind: u32,
    caps: u32,
    conns: Vec<u32>,
}

struct Codec<'a> {
    hda: &'a mut Hda,
    addr: u32,
    widgets: Vec<(u32, Widget)>,
    /// Default amplifiers (on the function group node).
    afg_out_amp: u32,
    afg_in_amp: u32,
}

impl Codec<'_> {
    fn widget(&self, nid: u32) -> Option<&Widget> {
        self.widgets.iter().find(|(n, _)| *n == nid).map(|(_, w)| w)
    }

    /// The path from a node to a DAC: [node, ..., DAC]; depth no more than 5.
    fn path_to_dac(&self, nid: u32, depth: usize) -> Option<Vec<u32>> {
        let w = self.widget(nid)?;
        if w.kind == W_OUTPUT {
            return Some(alloc::vec![nid]);
        }
        if depth == 0 || !matches!(w.kind, W_PIN | W_MIXER | W_SELECTOR) {
            return None;
        }
        for &c in &w.conns {
            if let Some(mut rest) = self.path_to_dac(c, depth - 1) {
                rest.insert(0, nid);
                return Some(rest);
            }
        }
        None
    }

    /// The path from an ADC down to pin `pin`: [ADC, ..., pin]; depth no more than 5.
    fn path_from(&self, nid: u32, pin: u32, depth: usize) -> Option<Vec<u32>> {
        if nid == pin {
            return Some(alloc::vec![nid]);
        }
        let w = self.widget(nid)?;
        if depth == 0 || !matches!(w.kind, W_INPUT | W_MIXER | W_SELECTOR) {
            return None;
        }
        for &c in &w.conns {
            if let Some(mut rest) = self.path_from(c, pin, depth - 1) {
                rest.insert(0, nid);
                return Some(rest);
            }
        }
        None
    }

    /// The commands that route an input path [ADC, ..., pin] to the capture stream: power,
    /// input selection, amplifiers at 0 dB, the microphone boost one step up, the pin as an input
    /// with microphone bias.
    fn input_route(&mut self, path: &[u32], vref: u32) -> Vec<(u32, u32, u32)> {
        let mut cmds = Vec::new();
        for (i, &nid) in path.iter().enumerate() {
            cmds.push((nid, SET_POWER, 0));
            let caps = self.widget(nid).map_or(0, |w| w.caps);
            let kind = self.widget(nid).map_or(0, |w| w.kind);
            if caps & 1 << 2 != 0 {
                cmds.push(self.amp_cmd(nid, true, 0, None));
            }
            if let Some(&next) = path.get(i + 1) {
                let index = self.widget(nid).and_then(|w| w.conns.iter().position(|&c| c == next)).unwrap_or(0) as u32;
                if kind != W_MIXER && self.widget(nid).is_some_and(|w| w.conns.len() > 1) {
                    cmds.push((nid, SET_CONN_SELECT, index));
                }
                if caps & 1 << 1 != 0 {
                    cmds.push(self.amp_cmd(nid, false, index, None));
                }
            } else if kind == W_PIN && caps & 1 << 1 != 0 {
                // The pin's own input amplifier is the microphone boost (often +10 dB a step).
                cmds.push(self.amp_cmd(nid, false, 0, Some(1)));
            }
            if kind == W_INPUT {
                cmds.push((nid, SET_FORMAT, FORMAT as u32));
                cmds.push((nid, SET_STREAM, CAPTURE_TAG << 4));
            }
        }
        if let Some(&pin) = path.last() {
            cmds.push((pin, SET_PIN_CTL, 0x20 | vref));
        }
        cmds
    }

    /// An amplifier command: unmuted, at 0 dB (or `steps` above the minimum, or the maximum if there is no 0 dB).
    fn amp_cmd(&mut self, nid: u32, output: bool, index: u32, steps: Option<u32>) -> (u32, u32, u32) {
        let caps = self.amp_caps(nid, output);
        let (offset, max) = (caps & 0x7F, (caps >> 8) & 0x7F);
        let gain = match steps {
            Some(n) => n.min(max),
            None if offset > 0 => offset,
            None => max,
        };
        let dir = if output { 1 << 15 } else { 1 << 14 };
        (nid, SET_AMP, dir | 0x3000 | index << 8 | gain.min(0x7F))
    }

    fn amp_caps(&mut self, nid: u32, output: bool) -> u32 {
        let w = self.widget(nid).map_or(0, |w| w.caps);
        // Bit 3: the node has its own amplifier parameters, otherwise the group's shared ones.
        if w & 1 << 3 != 0 {
            self.hda.param(self.addr, nid, if output { P_OUT_AMP } else { P_IN_AMP })
        } else if output {
            self.afg_out_amp
        } else {
            self.afg_in_amp
        }
    }

    /// Unmutes an amplifier and sets the gain to 0 dB (or the maximum if there is no 0 dB).
    fn unmute(&mut self, nid: u32, output: bool, index: u32) {
        let caps = self.amp_caps(nid, output);
        let (offset, steps) = (caps & 0x7F, (caps >> 8) & 0x7F);
        let gain = if offset > 0 { offset } else { steps };
        let dir = if output { 1 << 15 } else { 1 << 14 };
        let _ = self.hda.cmd(self.addr, nid, SET_AMP, dir | 0x3000 | index << 8 | gain.min(0x7F));
    }

    /// Enables a path: input selection, mute, power; the DAC gets our stream.
    fn enable_path(&mut self, path: &[u32]) {
        for (i, &nid) in path.iter().enumerate() {
            let _ = self.hda.cmd(self.addr, nid, SET_POWER, 0);
            let caps = self.widget(nid).map_or(0, |w| w.caps);
            let kind = self.widget(nid).map_or(0, |w| w.kind);
            if caps & 1 << 2 != 0 {
                self.unmute(nid, true, 0);
            }
            if let Some(&next) = path.get(i + 1) {
                let index = self.widget(nid).and_then(|w| w.conns.iter().position(|&c| c == next)).unwrap_or(0) as u32;
                if kind == W_MIXER {
                    // A mixer plays all inputs together — open the needed one.
                    if caps & 1 << 1 != 0 {
                        self.unmute(nid, false, index);
                    }
                } else {
                    let _ = self.hda.cmd(self.addr, nid, SET_CONN_SELECT, index);
                    if caps & 1 << 1 != 0 {
                        self.unmute(nid, false, index);
                    }
                }
            }
            if kind == W_OUTPUT {
                let _ = self.hda.cmd(self.addr, nid, SET_FORMAT, FORMAT as u32);
                let _ = self.hda.cmd(self.addr, nid, SET_STREAM, STREAM_TAG << 4);
            }
        }
    }
}

/// Sets up the codec: all outputs (speakers, headphones, line out) to our stream.
/// Returns a description (for "About"), or None if there are no outputs (HDMI and the like).
fn setup_codec(hda: &mut Hda, addr: u32) -> Option<String> {
    let vendor = hda.cmd(addr, 0, GET_PARAM, P_VENDOR)?;
    let nodes = hda.param(addr, 0, P_NODES);
    let (start, count) = ((nodes >> 16) & 0xFF, nodes & 0xFF);
    let afg = (start..start + count).find(|&n| hda.param(addr, n, P_FG_TYPE) & 0xFF == 1)?;
    let _ = hda.cmd(addr, afg, SET_POWER, 0);
    wait(10);
    let afg_out_amp = hda.param(addr, afg, P_OUT_AMP);
    let afg_in_amp = hda.param(addr, afg, P_IN_AMP);
    let nodes = hda.param(addr, afg, P_NODES);
    let (start, count) = ((nodes >> 16) & 0xFF, nodes & 0xFF);
    let mut widgets = Vec::new();
    for nid in start..start + count {
        let caps = hda.param(addr, nid, P_WIDGET_CAPS);
        let conns = if caps & 1 << 8 != 0 { hda.connections(addr, nid) } else { Vec::new() };
        widgets.push((nid, Widget { kind: (caps >> 20) & 0xF, caps, conns }));
    }
    let mut codec = Codec { hda, addr, widgets, afg_out_amp, afg_in_amp };
    // Every pin with its configuration default and capabilities: the starting point for
    // supporting a new codec from a log.
    for (nid, w) in codec.widgets.iter().filter(|(_, w)| w.kind == W_PIN).map(|(n, w)| (*n, w.conns.clone())).collect::<Vec<_>>() {
        let config = codec.hda.cmd(addr, nid, GET_CONFIG, 0).unwrap_or(0);
        let caps = codec.hda.param(addr, nid, P_PIN_CAPS);
        kprintln!("audio: codec {addr} pin {nid:#x}: config {config:08x} caps {caps:08x} conns {w:x?}");
    }
    let adcs: Vec<(u32, Vec<u32>)> = codec.widgets.iter().filter(|(_, w)| w.kind == W_INPUT).map(|(n, w)| (*n, w.conns.clone())).collect();
    for (nid, conns) in &adcs {
        let feeds: Vec<(u32, Vec<u32>)> =
            conns.iter().filter_map(|c| codec.widget(*c).filter(|w| w.kind != W_PIN).map(|w| (*c, w.conns.clone()))).collect();
        kprintln!("audio: codec {addr} ADC {nid:#x} <- {conns:x?} {feeds:x?}");
    }

    // Outputs: pins capable of outputting sound, physically connected,
    // with the purpose "line out", "speaker" or "headphones".
    let pins: Vec<u32> = codec.widgets.iter().filter(|(_, w)| w.kind == W_PIN).map(|(n, _)| *n).collect();
    let mut outputs = Vec::new();
    for nid in pins {
        let pin_caps = codec.hda.param(addr, nid, P_PIN_CAPS);
        let config = codec.hda.cmd(addr, nid, GET_CONFIG, 0).unwrap_or(0);
        let connected = config >> 30 != 1;
        let device = (config >> 20) & 0xF;
        if pin_caps & 1 << 4 == 0 || !connected || device > 2 {
            continue;
        }
        let Some(path) = codec.path_to_dac(nid, 5) else { continue };
        codec.enable_path(&path);
        let hp = pin_caps & 1 << 3 != 0;
        let _ = codec.hda.cmd(addr, nid, SET_PIN_CTL, 0x40 | if hp { 0x80 } else { 0 });
        if pin_caps & 1 << 16 != 0 {
            let _ = codec.hda.cmd(addr, nid, SET_EAPD, 0x02); // enable the external amplifier
        }
        let name = ["line out", "speaker", "headphones"][device as usize];
        let sense = pin_caps & 1 << 2 != 0 && config >> 30 != 2; // built-in speakers need no sensor
        let trigger = pin_caps & 1 << 1 != 0;
        codec.hda.pins.push(OutPin { codec: addr, nid, device, hp_drive: hp, sense, trigger });
        let jack = if sense { ", jack detect" } else { "" };
        kprintln!("audio: codec {addr} pin {nid:#x} ({name}{jack}) -> {:x?}", path);
        outputs.push(name);
    }
    // Inputs: microphone and line-in pins that an ADC can reach.
    let adcs: Vec<u32> = codec.widgets.iter().filter(|(_, w)| w.kind == W_INPUT).map(|(n, _)| *n).collect();
    let pins: Vec<u32> = codec.widgets.iter().filter(|(_, w)| w.kind == W_PIN).map(|(n, _)| *n).collect();
    // Whether the firmware describes a microphone in a jack (a built-in one does not count).
    let mut described_jack_mic = false;
    for &nid in &pins {
        let config = codec.hda.cmd(addr, nid, GET_CONFIG, 0).unwrap_or(0);
        described_jack_mic |= matches!(config >> 30, 0 | 3) && (config >> 20) & 0xF == 0xA;
    }
    for nid in pins {
        let pin_caps = codec.hda.param(addr, nid, P_PIN_CAPS);
        let mut config = codec.hda.cmd(addr, nid, GET_CONFIG, 0).unwrap_or(0);
        // Some firmwares (the BRIX, the Dell Latitude 7410) describe only the headphone pin of a
        // Realtek combo jack and mark its microphone "not connected". Realtek codecs wire the headset
        // microphone to pin 0x19: use it as a jack microphone when no jack microphone is described.
        if vendor >> 16 == 0x10EC && nid == 0x19 && config >> 30 == 1 && !described_jack_mic && codec.hda.pins.iter().any(|p| p.device == 2) {
            config = 0x0000_0000 | 0xA << 20;
        }
        let (connectivity, device) = (config >> 30, (config >> 20) & 0xF);
        if pin_caps & 1 << 5 == 0 || connectivity == 1 || !matches!(device, 0x8 | 0xA) {
            continue;
        }
        let Some(path) = adcs.iter().find_map(|&adc| codec.path_from(adc, nid, 5)) else { continue };
        // Microphone bias: 80 % if the pin offers it, otherwise 50 %.
        let vref = if device != 0xA {
            0
        } else if pin_caps & 1 << 12 != 0 {
            4
        } else if pin_caps & 1 << 9 != 0 {
            1
        } else {
            0
        };
        let route = codec.input_route(&path, vref);
        let jack = connectivity != 2 && pin_caps & 1 << 2 != 0;
        let name = match (device, connectivity) {
            (0x8, _) => "Line in",
            (_, 2) => "Internal microphone",
            _ => "Headset microphone",
        };
        kprintln!("audio: codec {addr} pin {nid:#x} ({name}{}) <- {:x?}", if jack { ", jack detect" } else { "" }, path);
        codec.hda.inputs.push(InPin { codec: addr, nid, adc: path[0], name, jack, trigger: pin_caps & 1 << 1 != 0, route });
    }
    if outputs.is_empty() {
        return None;
    }
    outputs.dedup();
    Some(format!("HDA codec {:04x}:{:04x} ({})", vendor >> 16, vendor & 0xFFFF, outputs.join(", ")))
}

pub fn probe(devices: &[pci::Device]) -> Option<(Hda, String)> {
    // Class 04.03 is HD Audio. (04.01 is DSP mode, we cannot handle that.)
    let dev = devices.iter().find(|d| d.class == 0x04 && d.subclass == 0x03)?;
    dev.enable_bus_master();
    if dev.vendor == 0x8086 {
        // Intel: traffic class TC0 and "snoop" for DMA (as Linux does).
        let v = dev.read(0x44);
        dev.write(0x44, v & !0x07);
        let v = dev.read(0x78);
        dev.write(0x78, v & !(1 << 11));
    }
    let base = dev.mmio(0, 0x4000);
    let mut hda = Hda {
        pins: Vec::new(),
        inputs: Vec::new(),
        mic: None,
        mic_checked: 0,
        cap_sd: 0,
        cap_bdl: dma(CAPTURE_BUFS * 16) as *mut Bdle,
        cap_buf: dma(CAPTURE_BUFS * CAPTURE_BYTES),
        cap_read: 0,
        cap_running: false,
        headphones: false,
        jack_checked: 0,
        base,
        sd: 0,
        corb: dma(1024) as *mut u32,
        rirb: dma(2048) as *mut u64,
        rirb_rp: 0,
        bdl: dma(BUFFERS * 16) as *mut Bdle,
    };
    let gcap = hda.r16(GCAP);
    let (iss, oss) = ((gcap >> 8) & 0xF, (gcap >> 12) & 0xF);
    if oss == 0 {
        kprintln!("audio: HDA {:04x}:{:04x} has no output streams", dev.vendor, dev.device);
        return None;
    }
    hda.sd = base + 0x80 + iss as usize * 0x20;
    if iss > 0 {
        hda.cap_sd = base + 0x80;
    }
    let Some(codecs) = hda.reset() else {
        kprintln!("audio: HDA controller does not come out of reset");
        return None;
    };
    hda.w32(INTCTL, 0);
    hda.setup_rings();
    let mut found = None;
    for addr in 0..15 {
        if codecs & 1 << addr == 0 {
            continue;
        }
        let info = setup_codec(&mut hda, addr);
        kprintln!("audio: HDA codec {addr}: {}", info.as_deref().unwrap_or("no analog outputs"));
        if found.is_none() {
            found = info;
        }
    }
    let Some(name) = found else {
        kprintln!("audio: HDA {:04x}:{:04x}: no usable codec (mask {codecs:#x})", dev.vendor, dev.device);
        return None;
    };
    kprintln!("audio: {name}, {RATE} Hz");
    // Headphones were plugged in before boot — turn off the speakers right away.
    if hda.pins.iter().any(|p| p.device == 2 && p.sense) {
        let hp = hda.headphones_present();
        if hp {
            hda.route(true);
        }
        super::OUTPUT.store(if hp { 2 } else { 1 }, core::sync::atomic::Ordering::Relaxed);
    }
    hda.update_mic();
    Some((hda, name))
}

impl Hda {
    fn sd_r32(&self, r: usize) -> u32 {
        unsafe { ((self.sd + r) as *const u32).read_volatile() }
    }
    fn sd_w32(&self, r: usize, v: u32) {
        unsafe { ((self.sd + r) as *mut u32).write_volatile(v) }
    }
    fn sd_w16(&self, r: usize, v: u16) {
        unsafe { ((self.sd + r) as *mut u16).write_volatile(v) }
    }
    fn sd_w8(&self, r: usize, v: u8) {
        unsafe { ((self.sd + r) as *mut u8).write_volatile(v) }
    }

    fn stream_reset(&self) {
        reset_stream(self.sd);
    }
}

/// The register at offset `r` of the stream block `sd`.
fn sd_reg(sd: usize, r: usize) -> *mut u32 {
    (sd + r) as *mut u32
}

/// Stops a stream and puts it through reset (its registers return to the defaults).
fn reset_stream(sd: usize) {
    let rd = || unsafe { sd_reg(sd, SD_CTL).read_volatile() };
    let wr = |v: u32| unsafe { sd_reg(sd, SD_CTL).write_volatile(v) };
    let ctl = rd() & !0b10;
    wr(ctl);
    until(10, || rd() & 0b10 == 0);
    wr(ctl | 1);
    until(10, || rd() & 1 != 0);
    wr(ctl & !1);
    until(10, || rd() & 1 == 0);
}

impl Hda {
    /// Whether a plug is in jack `nid` (a pin with presence detect).
    fn sensed(&mut self, codec: u32, nid: u32, trigger: bool) -> bool {
        if trigger {
            let _ = self.cmd(codec, nid, EXEC_PIN_SENSE, 0);
            wait(1);
        }
        self.cmd(codec, nid, GET_PIN_SENSE, 0).is_some_and(|v| v & 1 << 31 != 0)
    }

    /// The input to record from: a plugged-in jack first, then a built-in microphone, then
    /// an input whose plug cannot be sensed.
    fn pick_mic(&mut self) -> Option<usize> {
        let jacks: Vec<(usize, u32, u32, bool)> =
            self.inputs.iter().enumerate().filter(|(_, p)| p.jack).map(|(i, p)| (i, p.codec, p.nid, p.trigger)).collect();
        for (i, codec, nid, trigger) in jacks {
            if self.sensed(codec, nid, trigger) {
                return Some(i);
            }
        }
        self.inputs.iter().position(|p| p.name == "Internal microphone").or_else(|| self.inputs.iter().position(|p| !p.jack))
    }

    /// Follows the microphone jack: switches the routing and shows the system the right input.
    fn update_mic(&mut self) {
        let want = self.pick_mic();
        if want == self.mic.as_ref().map(|m| m.0) {
            return;
        }
        let running = self.cap_running;
        if running {
            self.capture_stop();
        }
        if let Some((old, input)) = self.mic.take() {
            super::remove_input(&input);
            let (codec, nid, adc) = (self.inputs[old].codec, self.inputs[old].nid, self.inputs[old].adc);
            let _ = self.cmd(codec, nid, SET_PIN_CTL, 0);
            let _ = self.cmd(codec, adc, SET_STREAM, 0);
        }
        let Some(i) = want else { return };
        let (codec, route) = (self.inputs[i].codec, self.inputs[i].route.clone());
        for (nid, verb, payload) in route {
            let _ = self.cmd(codec, nid, verb, payload);
        }
        let input = super::add_input(self.inputs[i].name, RATE);
        self.mic = Some((i, input));
        if running {
            self.capture_start();
        }
    }

    fn capture_start(&mut self) {
        let sd = self.cap_sd;
        reset_stream(sd);
        for i in 0..CAPTURE_BUFS {
            let addr = self.cap_buf as u64 + (i * CAPTURE_BYTES) as u64;
            unsafe { self.cap_bdl.add(i).write_volatile(Bdle { addr, len: CAPTURE_BYTES as u32, flags: 0 }) };
        }
        let bdl = self.cap_bdl as u64;
        unsafe {
            sd_reg(sd, SD_BDPL).write_volatile(bdl as u32);
            sd_reg(sd, SD_BDPU).write_volatile((bdl >> 32) as u32);
            sd_reg(sd, SD_CBL).write_volatile((CAPTURE_BUFS * CAPTURE_BYTES) as u32);
            ((sd + SD_LVI) as *mut u16).write_volatile((CAPTURE_BUFS - 1) as u16);
            ((sd + SD_FMT) as *mut u16).write_volatile(FORMAT);
            ((sd + SD_STS) as *mut u8).write_volatile(0x1C);
            let ctl = sd_reg(sd, SD_CTL).read_volatile() & !(0xF << 20);
            sd_reg(sd, SD_CTL).write_volatile(ctl | CAPTURE_TAG << 20 | 0b10);
        }
        self.cap_read = 0;
        self.cap_running = true;
    }

    fn capture_stop(&mut self) {
        unsafe {
            let ctl = sd_reg(self.cap_sd, SD_CTL).read_volatile();
            sd_reg(self.cap_sd, SD_CTL).write_volatile(ctl & !0b10);
        }
        self.cap_running = false;
    }

    /// Moves what the ADC has written since the last call to the system's input (stereo → mono).
    fn capture_read(&mut self) {
        let Some((_, input)) = &self.mic else { return };
        let size = CAPTURE_BUFS * CAPTURE_BYTES;
        let pos = unsafe { sd_reg(self.cap_sd, SD_LPIB).read_volatile() } as usize % size;
        let end = (pos + size - CAPTURE_GUARD) % size & !3;
        let mut mono = Vec::new();
        while self.cap_read != end {
            let frame = unsafe { (self.cap_buf.add(self.cap_read) as *const [i16; 2]).read_volatile() };
            mono.push(((frame[0] as i32 + frame[1] as i32) / 2) as i16);
            self.cap_read = (self.cap_read + 4) % size;
        }
        if !mono.is_empty() {
            input.lock().push(&mono);
        }
    }

    /// Whether a plug is inserted into at least one headphone jack.
    fn headphones_present(&mut self) -> bool {
        let jacks: Vec<(u32, u32, bool)> =
            self.pins.iter().filter(|p| p.device == 2 && p.sense).map(|p| (p.codec, p.nid, p.trigger)).collect();
        jacks.into_iter().any(|(codec, nid, trigger)| {
            if trigger {
                let _ = self.cmd(codec, nid, EXEC_PIN_SENSE, 0);
                wait(1);
            }
            self.cmd(codec, nid, GET_PIN_SENSE, 0).is_some_and(|v| v & 1 << 31 != 0)
        })
    }

    /// Headphones plugged in — sound only to them, unplugged — back to the speakers.
    fn route(&mut self, headphones: bool) {
        self.headphones = headphones;
        let pins: Vec<(u32, u32, u32, bool)> = self.pins.iter().map(|p| (p.codec, p.nid, p.device, p.hp_drive)).collect();
        for (codec, nid, device, hp) in pins {
            let ctl = match device {
                1 if headphones => 0, // speaker — turn off
                _ => 0x40 | if hp { 0x80 } else { 0 },
            };
            let _ = self.cmd(codec, nid, SET_PIN_CTL, ctl);
        }
        kprintln!("audio: output -> {}", if headphones { "headphones" } else { "speakers" });
    }
}

impl Hw for Hda {
    fn jack(&mut self) -> Option<bool> {
        if !self.pins.iter().any(|p| p.device == 2 && p.sense) {
            return None;
        }
        let now = interrupts::ticks();
        if now < self.jack_checked + HZ / 2 {
            return None;
        }
        self.jack_checked = now;
        let present = self.headphones_present();
        if present == self.headphones {
            return None;
        }
        self.route(present);
        Some(present)
    }

    fn start(&mut self, buffers: &[u64], _last: usize) {
        self.stream_reset();
        for (i, &addr) in buffers.iter().enumerate() {
            unsafe { self.bdl.add(i).write_volatile(Bdle { addr, len: BUFFER_BYTES as u32, flags: 0 }) };
        }
        let bdl = self.bdl as u64;
        self.sd_w32(SD_BDPL, bdl as u32);
        self.sd_w32(SD_BDPU, (bdl >> 32) as u32);
        self.sd_w32(SD_CBL, (BUFFERS * BUFFER_BYTES) as u32);
        self.sd_w16(SD_LVI, (BUFFERS - 1) as u16);
        self.sd_w16(SD_FMT, FORMAT);
        self.sd_w8(SD_STS, 0x1C);
        let ctl = self.sd_r32(SD_CTL) & !(0xF << 20);
        self.sd_w32(SD_CTL, ctl | STREAM_TAG << 20 | 0b10);
    }

    fn stop(&mut self) {
        let ctl = self.sd_r32(SD_CTL);
        self.sd_w32(SD_CTL, ctl & !0b10);
    }

    fn position(&self) -> usize {
        self.sd_r32(SD_LPIB) as usize / BUFFER_BYTES
    }

    fn queued(&mut self, _i: usize) {}

    fn underrun(&mut self) -> bool {
        false
    }

    fn capture(&mut self, recording: bool) {
        if self.inputs.is_empty() || self.cap_sd == 0 {
            return;
        }
        let now = interrupts::ticks();
        if now >= self.mic_checked + HZ / 2 {
            self.mic_checked = now;
            self.update_mic();
        }
        let recording = recording && self.mic.is_some();
        if recording && !self.cap_running {
            self.capture_start();
        } else if !recording && self.cap_running {
            self.capture_stop();
        }
        if self.cap_running {
            self.capture_read();
        }
    }
}
