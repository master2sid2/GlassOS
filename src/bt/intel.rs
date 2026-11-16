//! Setting up Intel controllers (USB 8087:…): after power-on they are in the boot loader and wait
//! for firmware. Follows FreeBSD's iwmbtfw (BSD-2-Clause, Vladimir Kondratyev, Future Crew
//! LLC): the version, the boot parameters, the RSA header and the command buffer in pieces with command
//! 0xFC09 (in the boot loader it goes over the bulk channel), waiting for "loaded", Intel Reset into
//! the operational firmware, DDC settings, the event mask. The firmware is linux-firmware
//! intel/ibt-*.sfi/.ddc (license LICENCE.ibt_firmware, redistributed unmodified).
//!
//! Done for the 8260 family (variants 0x0b, 0x0c, 0x11–0x14, the old version format) —
//! this includes the AX201 (variant 0x13). Newer ones (the version in TLV, 0x17 and later) are refused for now with
//! details in the log.

use super::Kind;
use super::host::Host;
use crate::interrupts::{self, HZ};
use crate::kprintln;
use crate::task;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The Intel USB vendor id.
pub(super) const USB_VENDOR: u16 = 0x8087;
const READ_VERSION: u16 = 0xFC05;
const READ_BOOT_PARAMS: u16 = 0xFC0D;
const SECURE_SEND: u16 = 0xFC09;
const INTEL_RESET: u16 = 0xFC01;
const WRITE_BOOT_PARAMS: u16 = 0xFC0E;
const WRITE_DDC: u16 = 0xFC8B;
const SET_EVENT_MASK: u16 = 0xFC52;
const PLATFORM: u8 = 0x37;
const FW_BOOTLOADER: u8 = 0x06;
const FW_OPERATIONAL: u8 = 0x23;
/// The TLV version: the entry with the controller variant and the "what is running" entry (3 — firmware).
const TLV_CNVI_BT: u8 = 0x12;
const TLV_IMAGE_TYPE: u8 = 0x1C;
const IMAGE_OPERATIONAL: u8 = 0x03;
/// The .sfi header: CSS (128), the public key (2×128), 4 bytes skipped, the signature (2×128).
const RSA_HEADER: usize = 644;
const CSS_VERSION_AT: usize = 8;
const CSS_RSA: u32 = 0x0001_0000;
/// The largest command buffer piece in one 0xFC09.
const CHUNK: usize = 0xFC;
const VENDOR_EVENT: u8 = 0xFF;
const EV_DOWNLOADED: u8 = 0x06;
const EV_BOOTED: u8 = 0x02;
const EV_COMMAND_COMPLETE: u8 = 0x0E;
/// How long to wait for the disk with the firmware.
const WAIT_SECONDS: u64 = 90;

/// The firmware, if the controller is in the boot loader. Does nothing if it is already running.
pub(super) fn setup(h: &mut Host) -> Result<String, String> {
    let v = h.command(READ_VERSION, &[0xFF])?;
    // The old format: status, platform 0x37, variant, revision, firmware kind, revision…
    if v.len() != 10 || v[1] != PLATFORM {
        // The version in TLV: this is how newer models answer, and the already running firmware of older ones (after
        // a warm reboot the firmware stays in the controller).
        let image = tlv(&v, TLV_IMAGE_TYPE).and_then(|x| x.first().copied());
        let variant = tlv(&v, TLV_CNVI_BT).filter(|x| x.len() >= 4).map(|x| (u32::from_le_bytes([x[0], x[1], x[2], x[3]]) >> 16) & 0x3F);
        return match image {
            Some(IMAGE_OPERATIONAL) => Ok(format!("Intel variant {:#04x}: firmware already running", variant.unwrap_or(0))),
            _ => {
                let head: Vec<String> = v.iter().take(24).map(|b| format!("{b:02x}")).collect();
                Err(format!("Intel with a TLV version, image {image:?} ({} bytes: {}…): loading this model is not supported yet", v.len(), head.join(" ")))
            }
        };
    }
    let (variant, hw_rev, fw_variant, fw_rev) = (v[2], v[3], v[4], v[5]);
    let linux_name = format!("ibt-{variant}-{hw_rev}-{fw_rev}");
    kprintln!("bt: Intel variant {variant:#04x} revision {hw_rev}, firmware {fw_variant:#04x} revision {fw_rev} (build {}/{}/{})", v[6], v[7], v[8]);
    match fw_variant {
        FW_OPERATIONAL => return Ok(format!("Intel {linux_name}: firmware already running")),
        FW_BOOTLOADER => {}
        other => return Err(format!("Intel: unknown firmware kind {other:#04x}")),
    }
    if !matches!(variant, 0x0b | 0x0c | 0x11..=0x14) {
        return Err(format!("Intel variant {variant:#04x} is not supported yet"));
    }
    let p = h.ok(READ_BOOT_PARAMS, &[])?;
    // Status (0), OTP (1–3), revision (4–5), security (6–11), address (12–17), the minimum
    // build (18–20), limited_cce (21): pieces must be confirmed with an event.
    if p.get(21).copied().unwrap_or(0) != 0 {
        return Err("Intel: this firmware loading method is not supported".into());
    }
    let (sfi, ddc) = (format!("/firmware/ibt{variant}.sfi"), format!("/firmware/ibt{variant}.ddc"));
    let fw = read_waiting(&sfi).map_err(|e| format!("{sfi} (linux-firmware intel/{linux_name}.sfi): {e}"))?;
    if fw.len() < RSA_HEADER || u32::from_le_bytes(fw[CSS_VERSION_AT..CSS_VERSION_AT + 4].try_into().unwrap()) != CSS_RSA {
        return Err(format!("{sfi}: not an RSA-signed Intel firmware"));
    }
    kprintln!("bt: Intel: loading {sfi} ({} KiB)", fw.len() / 1024);
    // The header: the boot loader checks the signature.
    for (kind, from, to) in [(0x00u8, 0, 128), (0x03, 128, 256), (0x03, 256, 384), (0x02, 388, 516), (0x02, 516, 644)] {
        fragment(h, kind, &fw[from..to])?;
    }
    let boot = commands(h, &fw[RSA_HEADER..])?;
    wait_vendor(h, EV_DOWNLOADED, 5 * HZ).map_err(|e| format!("Intel: the firmware was not accepted: {e}"))?;
    // Into the operational firmware: reset type, patch, DDC, boot at an address.
    let mut reset = alloc::vec![0x00, 0x00, 0x00, 0x01];
    reset.extend_from_slice(&boot.to_le_bytes());
    send_command(h, INTEL_RESET, &reset)?;
    wait_vendor(h, EV_BOOTED, 5 * HZ).map_err(|e| format!("Intel: the firmware did not start: {e}"))?;
    if let Ok(v) = h.command(READ_VERSION, &[]) {
        kprintln!("bt: Intel: firmware running, kind {:#04x} revision {}", v.get(4).copied().unwrap_or(0), v.get(5).copied().unwrap_or(0));
    }
    // Board settings (DDC): "length, data" entries.
    let mut text = format!("Intel {linux_name}, firmware loaded");
    match task::as_system(|| crate::fs::read(&ddc)) {
        Ok(d) => {
            let mut at = 0;
            while at < d.len() {
                let len = d[at] as usize + 1;
                let Some(rec) = d.get(at..at + len) else { break };
                if let Err(e) = h.ok(WRITE_DDC, rec) {
                    kprintln!("bt: Intel DDC: {e}");
                    break;
                }
                at += len;
            }
            text.push_str(", DDC applied");
        }
        Err(e) => kprintln!("bt: Intel: {ddc}: {e} (works without it)"),
    }
    if let Err(e) = h.ok(SET_EVENT_MASK, &[0x87, 0x0C, 0, 0, 0, 0, 0, 0]) {
        kprintln!("bt: Intel event mask: {e}");
    }
    Ok(text)
}

/// The value of entry `kind` in the TLV version (after the status byte: type, length, value).
fn tlv(v: &[u8], kind: u8) -> Option<&[u8]> {
    let mut at = 1;
    while at + 2 <= v.len() {
        let (t, len) = (v[at], v[at + 1] as usize);
        let value = v.get(at + 2..at + 2 + len)?;
        if t == kind {
            return Some(value);
        }
        at += 2 + len;
    }
    None
}

/// The firmware file; the disk with it may be mounted later than USB.
fn read_waiting(path: &str) -> Result<Vec<u8>, String> {
    let end = interrupts::ticks() + WAIT_SECONDS * HZ;
    loop {
        match task::as_system(|| crate::fs::read(path)) {
            Ok(f) => return Ok(f),
            Err(_) if interrupts::ticks() < end => task::sleep_until(interrupts::ticks() + HZ),
            Err(e) => return Err(format!("{e}")),
        }
    }
}

/// The firmware command buffer: whole commands, in pieces of up to 252 bytes, the piece length a multiple of 4.
/// Also finds the boot address (the parameter of command 0xFC0E).
fn commands(h: &mut Host, buf: &[u8]) -> Result<u32, String> {
    let (mut sent, mut ready, mut boot) = (0usize, 0usize, 0u32);
    while buf.len() - sent - ready >= 3 {
        let at = sent + ready;
        let op = u16::from_le_bytes([buf[at], buf[at + 1]]);
        let len = 3 + buf[at + 2] as usize;
        if op == WRITE_BOOT_PARAMS && let Some(b) = buf.get(at + 3..at + 7) {
            boot = u32::from_le_bytes(b.try_into().unwrap());
        }
        ready += len;
        while ready >= CHUNK {
            fragment(h, 0x01, &buf[sent..(sent + CHUNK).min(buf.len())])?;
            sent += CHUNK;
            ready -= CHUNK;
        }
        if ready > 0 && ready % 4 == 0 {
            fragment(h, 0x01, &buf[sent..(sent + ready).min(buf.len())])?;
            sent += ready;
            ready = 0;
        }
    }
    Ok(boot)
}

/// A secure boot piece: command 0xFC09 over the bulk channel, the answer from there too.
fn fragment(h: &mut Host, kind: u8, data: &[u8]) -> Result<(), String> {
    let mut p = Vec::with_capacity(4 + data.len());
    p.extend_from_slice(&SECURE_SEND.to_le_bytes());
    p.push(data.len() as u8 + 1);
    p.push(kind);
    p.extend_from_slice(data);
    if !h.raw_send(Kind::Acl, p) {
        return Err("Intel: the firmware piece was not accepted".into());
    }
    let start = interrupts::ticks();
    let end = start + 2 * HZ;
    loop {
        while let Some((k, packet)) = h.raw_recv() {
            // The answer to a piece is Command Complete 0xFC09 (over bulk or as an event).
            let done = packet.first() == Some(&EV_COMMAND_COMPLETE) && packet.get(3..5) == Some(&SECURE_SEND.to_le_bytes()[..]);
            if done {
                return match packet.get(5) {
                    Some(0) | None => Ok(()),
                    Some(&s) => Err(format!("Intel: firmware piece refused, status {s:#04x}")),
                };
            }
            if k == Kind::Event && packet.first() == Some(&VENDOR_EVENT) {
                kprintln!("bt: Intel: vendor event {:02x?} during loading", &packet[..packet.len().min(8)]);
            }
        }
        let now = interrupts::ticks();
        if now >= end {
            return Err("Intel: no answer to a firmware piece".into());
        }
        // The answer comes within a fraction of a millisecond: thousands of pieces must not wait a tick each.
        if now <= start + 1 {
            task::yield_now();
        } else {
            task::sleep_until(now + 1);
        }
    }
}

/// A command without waiting for Command Complete (after Intel Reset there may be none).
fn send_command(h: &mut Host, op: u16, params: &[u8]) -> Result<(), String> {
    let mut p = Vec::with_capacity(3 + params.len());
    p.extend_from_slice(&op.to_le_bytes());
    p.push(params.len() as u8);
    p.extend_from_slice(params);
    if h.raw_send(Kind::Command, p) { Ok(()) } else { Err(format!("command {op:04x} was not accepted")) }
}

/// Waits for vendor event 0xFF with first byte `code` (over the interrupt endpoint or bulk).
fn wait_vendor(h: &mut Host, code: u8, ticks: u64) -> Result<(), String> {
    let end = interrupts::ticks() + ticks;
    loop {
        while let Some((_, packet)) = h.raw_recv() {
            if packet.first() == Some(&VENDOR_EVENT) && packet.get(2) == Some(&code) {
                return Ok(());
            }
        }
        if interrupts::ticks() >= end {
            return Err(format!("no vendor event {code:#04x}"));
        }
        task::sleep_until(interrupts::ticks() + 1);
    }
}
