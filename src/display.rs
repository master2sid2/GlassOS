//! Screen modes. The video mode is set by the firmware (UEFI GOP), and only before leaving
//! it — so the choice applies at the next boot: the kernel, still inside the firmware,
//! reads /system/display.cfg from any disk the firmware sees and sets the mode.

use crate::sync::IrqLock;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The file with the chosen mode ("mode=1920x1080").
pub const CONFIG: &str = "/system/display.cfg";

/// The modes the firmware offers (without repeats, by decreasing area).
static MODES: IrqLock<Vec<(u32, u32)>> = IrqLock::new(Vec::new());
static CURRENT: IrqLock<(u32, u32)> = IrqLock::new((0, 0));
/// The monitor EDID as the firmware gave it (empty — it did not).
static EDID: IrqLock<Vec<u8>> = IrqLock::new(Vec::new());

pub fn set_edid(edid: Vec<u8>) {
    *EDID.lock() = edid;
}

pub fn edid() -> Vec<u8> {
    EDID.lock().clone()
}

/// The monitor's native mode from the EDID (the first detailed timing): width, height, pixel clock in kHz.
pub fn native(edid: &[u8]) -> Option<(u32, u32, u32)> {
    if edid.len() < 128 || edid[..8] != [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0] {
        return None;
    }
    let d = &edid[54..72];
    let khz = u16::from_le_bytes([d[0], d[1]]) as u32 * 10;
    let w = d[2] as u32 | (d[4] as u32 >> 4) << 8;
    let h = d[5] as u32 | (d[7] as u32 >> 4) << 8;
    (khz > 0 && w > 0 && h > 0).then_some((w, h, khz))
}

/// «PHL 275E1 (PHL), native 2560x1440, 241.5 MHz».
pub fn describe_edid(edid: &[u8]) -> String {
    let Some((w, h, khz)) = native(edid) else { return "no EDID from the firmware".into() };
    let id = u16::from_be_bytes([edid[8], edid[9]]);
    let maker: String = [(id >> 10) & 31, (id >> 5) & 31, id & 31].iter().map(|&c| (b'@' + c as u8) as char).collect();
    format!("{} ({maker}), native {w}x{h}, {}.{} MHz", crate::hwprobe::monitor_name(edid), khz / 1000, khz % 1000 / 100)
}

pub fn set_modes(modes: Vec<(u32, u32)>, current: (u32, u32)) {
    *MODES.lock() = modes;
    *CURRENT.lock() = current;
}

pub fn modes() -> Vec<(u32, u32)> {
    MODES.lock().clone()
}

pub fn current() -> (u32, u32) {
    *CURRENT.lock()
}

pub fn parse(text: &str) -> Option<(u32, u32)> {
    let value = text.lines().find_map(|l| l.trim().strip_prefix("mode="))?;
    let (w, h) = value.trim().split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// The chosen mode (None — as the system decides).
pub fn saved() -> Option<(u32, u32)> {
    crate::task::as_system(|| crate::fs::read(CONFIG)).ok().and_then(|d| parse(&String::from_utf8_lossy(&d)))
}

pub fn save(mode: Option<(u32, u32)>) {
    crate::task::as_system(|| {
    let _ = crate::fs::mkdir("/system");
    match mode {
        Some((w, h)) => {
            let _ = crate::fs::write(CONFIG, format!("mode={w}x{h}\n").as_bytes());
        }
        None => {
            let _ = crate::fs::remove(CONFIG);
        }
    }
    });
}

/// While the firmware is alive: looks for /system/display.cfg on all its file systems.
pub fn read_preference_uefi() -> Option<(u32, u32)> {
    use uefi::boot;
    use uefi::proto::media::fs::SimpleFileSystem;
    let handles = boot::find_handles::<SimpleFileSystem>().ok()?;
    let path = uefi::CString16::try_from("\\system\\display.cfg").ok()?;
    for h in handles {
        let Ok(proto) = boot::open_protocol_exclusive::<SimpleFileSystem>(h) else { continue };
        let mut fs = uefi::fs::FileSystem::new(proto);
        if let Ok(data) = fs.read(path.as_ref())
            && let Some(mode) = parse(&String::from_utf8_lossy(&data))
        {
            return Some(mode);
        }
    }
    None
}
