//! Debugging tools: switches, off by default; an administrator turns them on
//! (the `debug NAME on` command, Settings → Remote Access), the state is in /system/debug.cfg.
//!
//! All debugging code is behind the `debug-tools` build flag (Cargo.toml): without it there are no
//! tools at all. To remove it from the code — find everything by "debug-tools" and this file.

use crate::sync::IrqLock;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

/// Keyboard and mouse from another computer (remote access, POST /input).
pub const INPUT: &str = "input";

/// The tools: the name and what it does.
const TOOLS: &[(&str, &str)] = &[(INPUT, "keyboard and mouse from the remote computer (remote.py type/key/click)")];

const FILE: &str = "/system/debug.cfg";

/// The enabled tools.
static ON: IrqLock<Vec<&'static str>> = IrqLock::new(Vec::new());
/// The file was read (it is read once the system disk is mounted).
static LOADED: AtomicBool = AtomicBool::new(false);

fn load() {
    if LOADED.load(Ordering::Relaxed) || !crate::fs::has_disk() {
        return;
    }
    LOADED.store(true, Ordering::Relaxed);
    let Ok(data) = crate::task::as_system(|| crate::fs::read(FILE)) else { return };
    let text = String::from_utf8_lossy(&data);
    let mut on = ON.lock();
    for line in text.lines() {
        if let Some((name, "on")) = line.trim().split_once('=')
            && let Some(&(tool, _)) = TOOLS.iter().find(|(t, _)| *t == name)
            && !on.contains(&tool)
        {
            on.push(tool);
        }
    }
}

/// Whether a tool is on.
pub fn enabled(name: &str) -> bool {
    load();
    ON.lock().contains(&name)
}

/// Turn on or off (administrators only); remembered.
pub fn set(name: &str, enable: bool) -> Result<(), String> {
    if !crate::access::is_admin() {
        return Err("only an administrator can change debug tools".into());
    }
    let Some(&(tool, _)) = TOOLS.iter().find(|(t, _)| *t == name) else {
        return Err(format!("no debug tool {name:?}; there are: {}", TOOLS.iter().map(|(t, _)| *t).collect::<Vec<_>>().join(", ")));
    };
    load();
    {
        let mut on = ON.lock();
        on.retain(|t| *t != tool);
        if enable {
            on.push(tool);
        }
    }
    let text: String = TOOLS.iter().map(|(t, _)| format!("{t}={}\n", if enabled(t) { "on" } else { "off" })).collect();
    crate::task::as_system(|| {
        let _ = crate::fs::mkdir("/system");
        crate::fs::write(FILE, text.as_bytes())
    })
    .map_err(|e| format!("{FILE}: {e}"))?;
    crate::kprintln!("debug: {tool} {}", if enable { "on" } else { "off" });
    Ok(())
}

/// Lines for the `debug` command.
pub fn lines() -> Vec<String> {
    TOOLS.iter().map(|(t, about)| format!("{t:<8} {:<4} {about}", if enabled(t) { "on" } else { "off" })).collect()
}
