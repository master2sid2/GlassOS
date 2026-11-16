//! Setting up Realtek controllers: loading the firmware patch. Follows FreeBSD's rtlbtfw
//! (BSD-2-Clause, Adrian Chadd and Vladimir Kondratyev): the ROM version, choosing the patch in the
//! "Realtech" file, pieces of 252 bytes with command 0xFC20. Independent of the transport (USB, UART).

use super::host::Host;
use crate::interrupts::{self, HZ};
use crate::task;
use alloc::format;
use alloc::string::String;

/// The Realtek manufacturer code in the controller version.
pub(super) const MAKER: u16 = 0x005D;
const LMP_8723B: u16 = 0x8723;
const HCI_REV_8723B: u16 = 0x000B;
const READ_ROM_VERSION: u16 = 0xFC6D;
const DOWNLOAD: u16 = 0xFC20;
const CHUNK: usize = 252;
const EXT_SIGNATURE: [u8; 4] = [0x51, 0x04, 0xFD, 0x77];
const SIGNATURE_V1: &[u8; 8] = b"Realtech";
/// The patch (linux-firmware rtl_bt/rtl8723b_fw.bin, Realtek license).
const FIRMWARE: &str = "/firmware/rtbt8723.fw";
/// How long to wait for the disk with the patch and for the Wi-Fi of the same card to start.
const WAIT_SECONDS: u64 = 90;

/// The patch, if the controller waits for it. Nothing is sent to an already patched one (after a warm
/// reboot).
pub(super) fn setup(h: &mut Host, rev: u16, lmp: u16) -> Result<String, String> {
    if lmp != LMP_8723B {
        return Ok(format!("Realtek, LMP subversion {lmp:#06x}: already patched"));
    }
    if rev != HCI_REV_8723B {
        return Err(format!("Realtek HCI revision {rev:#x} is not an RTL8723B"));
    }
    // A combo card (Wi-Fi + Bluetooth): the Wi-Fi driver turns its power off and on at
    // startup — wait until it finishes. The disk with the patch is mounted later than USB.
    let end = interrupts::ticks() + WAIT_SECONDS * HZ;
    while !crate::rtl8723be::settled() && crate::rtl8723be::expected() && interrupts::ticks() < end {
        task::sleep_until(interrupts::ticks() + HZ);
    }
    let file = loop {
        match task::as_system(|| crate::fs::read(FIRMWARE)) {
            Ok(f) => break f,
            Err(_) if interrupts::ticks() < end => task::sleep_until(interrupts::ticks() + HZ),
            Err(e) => return Err(format!("{FIRMWARE}: {e}")),
        }
    };
    load_patch(h, &file)
}

/// rtlbt_parse_fwfile_v1 + rtlbt_load_fwfile.
fn load_patch(h: &mut Host, file: &[u8]) -> Result<String, String> {
    let rom = h.ok(READ_ROM_VERSION, &[])?.get(1).copied().ok_or("no ROM version")?;
    if file.len() < 14 + 4 || &file[..8] != SIGNATURE_V1 || file[file.len() - 4..] != EXT_SIGNATURE {
        return Err(format!("{FIRMWARE}: not a Realtek patch file"));
    }
    let fw_version = &file[8..12];
    let patches = u16::from_le_bytes([file[12], file[13]]) as usize;
    let base = 14;
    if file.len() < base + 8 * patches {
        return Err(format!("{FIRMWARE}: truncated"));
    }
    let at = |k: usize| u16::from_le_bytes([file[base + 2 * k], file[base + 2 * k + 1]]) as usize;
    let pick = (0..patches).find(|&i| at(i) == rom as usize + 1).ok_or_else(|| format!("{FIRMWARE}: no patch for ROM version {rom}"))?;
    let len = at(patches + pick);
    let off_at = base + 4 * (patches + pick);
    let offset = u32::from_le_bytes(file[off_at..off_at + 4].try_into().unwrap()) as usize;
    if len < 4 || file.len() < offset + len {
        return Err(format!("{FIRMWARE}: patch {pick} does not fit"));
    }
    // The last 4 bytes of the patch are the file version.
    let mut patch = file[offset..offset + len - 4].to_vec();
    patch.extend_from_slice(fw_version);
    let chunks = patch.len() / CHUNK + 1;
    let mut params = [0u8; 1 + CHUNK];
    let mut index = 0u8;
    for (i, chunk) in (0..chunks).map(|i| &patch[(i * CHUNK).min(patch.len())..((i + 1) * CHUNK).min(patch.len())]).enumerate() {
        index = if index >= 0x7F { 1 } else if i == 0 { 0 } else { index + 1 };
        params[0] = if i == chunks - 1 { index | 0x80 } else { index };
        params[1..1 + chunk.len()].copy_from_slice(chunk);
        h.ok(DOWNLOAD, &params[..1 + chunk.len()]).map_err(|e| format!("patch chunk {i}/{chunks}: {e}"))?;
    }
    let (_, _, _, lmp) = h.version()?;
    Ok(format!("ROM version {rom}, patch {pick} ({} bytes), LMP subversion now {lmp:#06x}", patch.len()))
}
