//! How the computer is named on the network: name, workgroup, description and visibility.
//! Stored in /system/identity.cfg; without the file the name is built from the MAC address, so that
//! two GlassOS machines on one network are not named the same.

use crate::fs;
use crate::sync::IrqLock;
use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicU64, Ordering};

const FILE: &str = "/system/identity.cfg";
/// The NetBIOS limit: 15 characters.
pub const NAME_MAX: usize = 15;
pub const DESCRIPTION_MAX: usize = 48;

#[derive(Clone, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub workgroup: String,
    pub description: String,
    /// Answer network searches and announce ourselves.
    pub visible: bool,
}

static CURRENT: IrqLock<Option<Identity>> = IrqLock::new(None);
/// Grows when the settings change: the discovery service announces the new name.
static VERSION: AtomicU64 = AtomicU64::new(0);

pub fn version() -> u64 {
    VERSION.load(Ordering::Relaxed)
}

fn default_name() -> String {
    match super::mac() {
        Some(m) => format!("GLASSOS-{:02X}{:02X}", m[4], m[5]),
        None => "GLASSOS".into(),
    }
}

/// Whether a name is valid: letters, digits, "-" and "_", and for a workgroup also a space inside.
pub fn valid_name(s: &str, spaces: bool) -> bool {
    !s.is_empty()
        && s.len() <= NAME_MAX
        && !s.starts_with([' ', '-'])
        && !s.ends_with(' ')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || (spaces && b == b' '))
}

fn load() -> Identity {
    let mut id = Identity { name: default_name(), workgroup: "WORKGROUP".into(), description: "GlassOS".into(), visible: true };
    let text = crate::task::as_system(|| fs::read(FILE)).map(|d| String::from_utf8_lossy(&d).into_owned()).unwrap_or_default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim() {
            "name" if valid_name(v, false) => id.name = v.to_uppercase(),
            "workgroup" if valid_name(v, true) => id.workgroup = v.to_uppercase(),
            "description" => id.description = v.chars().take(DESCRIPTION_MAX).collect(),
            "visible" => id.visible = v != "no",
            _ => {}
        }
    }
    id
}

/// Without reading the file and without the network lock: for code that already holds it.
pub fn cached() -> Option<Identity> {
    CURRENT.lock().clone()
}

pub fn get() -> Identity {
    let mut slot = CURRENT.lock();
    if slot.is_none() {
        // The file is read without the lock: reading may yield the CPU.
        drop(slot);
        let id = load();
        slot = CURRENT.lock();
        *slot = Some(id);
    }
    slot.clone().unwrap()
}

/// Saves the settings; they take effect right away.
pub fn set(id: Identity) -> bool {
    if !crate::access::is_admin() {
        return false;
    }
    let text = format!(
        "name={}\nworkgroup={}\ndescription={}\nvisible={}\n",
        id.name,
        id.workgroup,
        id.description,
        if id.visible { "yes" } else { "no" }
    );
    *CURRENT.lock() = Some(id);
    VERSION.fetch_add(1, Ordering::Relaxed);
    let _ = fs::mkdir("/system");
    fs::write(FILE, text.as_bytes()).is_ok()
}
