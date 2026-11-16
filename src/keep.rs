//! A USB stick as the memory of a system without a disk. From an ISO the files live in memory and are lost on
//! power-off, so settings and home folders are also kept on the boot stick:
//! at startup they are read from there, and changes go there in the background.
//!
//! The boot stick is recognised by a `glassos-data` or `glassos-hw` folder, by an image file
//! in the root or by a Ventoy service partition next to the data partition.
//! Passwords do not go to it: a stick is easy to lose. The exceptions are Wi-Fi network keys
//! (without them a laptop would be left without a connection after every boot, and they contain no password) and
//! the remote access key (otherwise it would have to be copied to another computer after
//! every boot; it is a random key, not a password, and "New Key" replaces it).

use crate::sync::YieldLock;
use crate::{access, fs, interrupts, task};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const DIR: &str = "glassos-data";
const REPORTS: &str = "glassos-hw";
const VENTOY: &str = "VTOYEFI";
const TREES: [&str; 2] = ["/system", "/home"];
/// Files larger than this are not brought into memory at startup: the boot would wait for them.
const MAX_FILE: usize = 64 << 20;
/// No more often than this: the desktop saves its state often, and the stick deserves mercy.
const GAP_SECONDS: u64 = 30;
const CHUNK: usize = 1 << 20;
/// The mark of a directory: for files it is the hash of the contents.
const FOLDER: u64 = 0;
const FNV_START: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x100_0000_01B3;

/// The folder on the stick, while it is mounted.
static STICK: YieldLock<Option<String>> = YieldLock::new(None);
/// What is already on the stick: the path in the system and the mark.
static KEPT: YieldLock<BTreeMap<String, u64>> = YieldLock::new(BTreeMap::new());
static BUSY: AtomicBool = AtomicBool::new(false);
static SAVED_EPOCH: AtomicU64 = AtomicU64::new(0);
/// The file was being appended when its turn came: return to it later.
static POSTPONED: AtomicBool = AtomicBool::new(false);
static NEXT_SAVE: AtomicU64 = AtomicU64::new(0);

fn has_image(root: &str) -> bool {
    let is_image = |e: &fs::Entry| {
        let name = e.name.to_ascii_lowercase();
        !e.is_dir && name.contains("glass") && name.ends_with(".iso")
    };
    fs::list(root).is_ok_and(|list| list.iter().any(is_image))
}

/// The folder on the stick we booted from. The folder from last time is looked for first.
fn find() -> Option<String> {
    let volumes: Vec<fs::Volume> = fs::volumes().into_iter().filter(|v| v.removable && !v.network && !v.offline).collect();
    let ventoy = volumes.iter().any(|v| v.name == VENTOY);
    let sticks: Vec<String> = volumes.into_iter().filter(|v| v.name != VENTOY).map(|v| v.path).collect();
    let folder = |root: &String| format!("{root}/{DIR}");
    if let Some(root) = sticks.iter().find(|root| fs::is_dir(&folder(root))) {
        return Some(folder(root));
    }
    let root = sticks.iter().find(|root| has_image(root) || fs::is_dir(&format!("{root}/{REPORTS}"))).or(sticks.first().filter(|_| ventoy))?;
    fs::mkdir(&folder(root)).ok()?;
    Some(folder(root))
}

/// A file fingerprint: it is read in pieces, it may not fit in memory whole.
fn fingerprint(path: &str) -> Option<u64> {
    let mut hash = FNV_START;
    let mut offset = 0;
    loop {
        let (chunk, total) = fs::read_range(path, offset, CHUNK).ok()?;
        for &byte in &chunk {
            hash = (hash ^ byte as u64).wrapping_mul(FNV_PRIME);
        }
        offset += chunk.len() as u64;
        if chunk.is_empty() || offset >= total {
            // Zero is taken by directories, a collision with it would only copy the file once more.
            return Some(hash | 1);
        }
    }
}

/// What is not carried to the stick.
fn stays_home(path: &str) -> bool {
    access::is_secret(path) && path != crate::wifi::CONFIG && path != crate::net::remote::KEY
}

fn pull(from: &str, to: &str, kept: &mut BTreeMap<String, u64>) {
    let Ok(list) = fs::list(from) else { return };
    for e in list {
        let (source, target) = (format!("{from}/{}", e.name), format!("{to}/{}", e.name));
        if e.is_dir {
            let _ = fs::mkdir(&target);
            kept.insert(target.clone(), FOLDER);
            pull(&source, &target, kept);
        } else if e.size <= MAX_FILE && !stays_home(&target) && fs::copy_file(&source, &target, |_| true).is_ok() {
            if let Some(mark) = fingerprint(&target) {
                kept.insert(target, mark);
            }
        }
    }
}

/// Reads the settings and home folders from the stick. Call before login.
pub fn restore() {
    fs::refresh_usb();
    let Some(dir) = find() else { return };
    let started = interrupts::ticks();
    let mut kept = BTreeMap::new();
    for tree in TREES {
        let source = format!("{dir}{tree}");
        if fs::is_dir(&source) {
            kept.insert(tree.into(), FOLDER);
            pull(&source, tree, &mut kept);
        }
    }
    crate::kprintln!("keep: {} items from {dir} in {} ms", kept.len(), (interrupts::ticks() - started) * 1000 / interrupts::HZ);
    *KEPT.lock() = kept;
    *STICK.lock() = Some(dir);
}

pub fn active() -> bool {
    STICK.lock().is_some()
}

fn scan(dir: &str, seen: &mut BTreeMap<String, u64>) {
    seen.insert(dir.into(), FOLDER);
    let Ok(list) = fs::list(dir) else { return };
    for e in list {
        let path = format!("{dir}/{}", e.name);
        if e.is_dir {
            scan(&path, seen);
        } else if fs::settling(&path) {
            POSTPONED.store(true, Ordering::Relaxed);
        } else if e.size <= MAX_FILE && !stays_home(&path) {
            if let Some(mark) = fingerprint(&path) {
                seen.insert(path, mark);
            }
        }
    }
}

/// Brings the stick in line with what is in the system now. The stick may have been pulled and inserted under
/// another name, so the folder is looked for again.
fn save() {
    let stick = STICK.lock().clone();
    let Some(dir) = stick.filter(|d| fs::is_dir(d)).or_else(find) else { return };
    *STICK.lock() = Some(dir.clone());
    let mut seen = BTreeMap::new();
    POSTPONED.store(false, Ordering::Relaxed);
    for tree in TREES {
        scan(tree, &mut seen);
    }
    let mut kept = KEPT.lock().clone();
    // Names in order: a folder always comes before its files.
    for (path, &mark) in &seen {
        if kept.get(path) == Some(&mark) {
            continue;
        }
        let target = format!("{dir}{path}");
        let done = if mark == FOLDER { fs::is_dir(&target) || fs::mkdir(&target).is_ok() } else { fs::copy_file(path, &target, |_| true).is_ok() };
        if !done {
            crate::kprintln!("keep: cannot write {target}");
            break;
        }
        kept.insert(path.clone(), mark);
    }
    // From the end: files are deleted before their folders.
    let gone: Vec<String> = kept.keys().rev().filter(|path| !seen.contains_key(*path) && !fs::settling(path)).cloned().collect();
    for path in gone {
        let target = format!("{dir}{path}");
        if !fs::exists(&target) || fs::remove(&target).is_ok() {
            kept.remove(&path);
        }
    }
    *KEPT.lock() = kept;
}

/// Called from the main loop: saves changes, if there are any and it is time.
pub fn poll(now: u64) {
    let epoch = fs::root_epoch();
    let again = POSTPONED.load(Ordering::Relaxed);
    if (epoch == SAVED_EPOCH.load(Ordering::Relaxed) && !again) || now < NEXT_SAVE.load(Ordering::Relaxed) || !active() || BUSY.swap(true, Ordering::Acquire) {
        return;
    }
    SAVED_EPOCH.store(epoch, Ordering::Relaxed);
    NEXT_SAVE.store(now + GAP_SECONDS * interrupts::HZ, Ordering::Relaxed);
    task::spawn("keep", None, || {
        task::as_system(save);
        BUSY.store(false, Ordering::Release);
    });
}

/// The last save before power-off.
pub fn finish() {
    if !active() {
        return;
    }
    while BUSY.swap(true, Ordering::Acquire) {
        task::yield_now();
    }
    task::as_system(save);
}
