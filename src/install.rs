//! Installing GlassOS to a SATA disk — as in other systems' installers, one of:
//! erase the whole disk (a new GPT with one partition), take free space (a new
//! partition, the others are untouched) or give the system one of the partitions.
//! `parts` reads and edits the partition tables.
//!
//! The system partition is FAT32 with the "EFI system partition" type; it holds the kernel at
//! `\EFI\BOOT\BOOTX64.EFI` and the programs from /apps, /bin and /firmware. The partition is no
//! larger than 32 GiB: the rest stays free (for a future GlassOS file
//! system), and the FAT of a large volume does not hold memory. A "GlassOS" boot entry is written
//! to the firmware NVRAM (`bootvars`) — many firmwares will not boot from an internal disk
//! otherwise. When booting from this partition the kernel makes it
//! the main volume (`fs::mount_sata`).

use crate::parts::{self, ALIGN, Drive, GLASSOS, Kind, SECTOR, Table, size_text};
use crate::sync::YieldLock;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// The kernel itself, read from the boot partition at startup.
pub static BOOT_IMAGE: YieldLock<Vec<u8>> = YieldLock::new(Vec::new());
/// Where the firmware loaded it from (the UEFI path on the partition), for example `\EFI\BOOT\BOOTX64.EFI`.
pub static BOOT_PATH: YieldLock<String> = YieldLock::new(String::new());

/// The largest system partition — 32 GiB.
const MAX_SECTORS: u64 = (32u64 << 30) / SECTOR as u64;
/// The smallest partition (and free gap) we install to.
const MIN_SECTORS: u64 = (256u64 << 20) / SECTOR as u64;
/// The path to the kernel on the partition.
const KERNEL_PATH: &str = "\\EFI\\BOOT\\BOOTX64.EFI";

/// Where to install the system.
#[derive(Clone, Copy, PartialEq)]
pub enum Place {
    /// The whole disk: a new partition table.
    Whole,
    /// A new partition in free space.
    Free { start: u64, sectors: u64 },
    /// Over an existing partition (the entry number).
    Part { slot: usize },
    /// Update GlassOS in its partition: replace the system files, keep accounts and files.
    Update { slot: usize },
}

/// An option for the install window.
pub struct Choice {
    pub place: Place,
    pub title: String,
    /// A question before installing.
    pub question: String,
    pub detail: String,
    /// A warning (for example, the partition is needed by another system).
    pub warning: Option<String>,
}

/// A disk the system can be installed to.
pub struct Target {
    pub disk: u32,
    pub sectors: u64,
    pub model: String,
    /// What is on it now: "empty", "GlassOS", "GPT, 3 partitions"…
    pub contents: String,
    /// This system runs from it or booted from it — installing there is not allowed.
    pub system: bool,
    pub table: Option<Table>,
}

/// Whether the system partition fits from this place (the volume is addressed with 32-bit sectors).
fn fits(start: u64, sectors: u64) -> bool {
    sectors >= MIN_SECTORS && start + sectors.min(MAX_SECTORS) <= u32::MAX as u64
}

impl Target {
    pub fn size(&self) -> String {
        size_text(self.sectors)
    }

    /// Where on this disk the system can be installed.
    pub fn choices(&self) -> Vec<Choice> {
        let mut out = vec![Choice {
            place: Place::Whole,
            title: "Erase the whole disk".into(),
            question: format!("Erase {} ({}) and install GlassOS on it?", self.model, self.size()),
            detail: format!("Everything on it is deleted. GlassOS gets {} at the start.", size_text(self.sectors.min(MAX_SECTORS))),
            warning: (self.contents != "empty").then(|| format!("Now on the disk: {}.", self.contents)),
        }];
        let Some(t) = &self.table else { return out };
        if t.slot_free {
            for &(start, sectors) in t.free.iter().filter(|&&(s, n)| fits(s, n)) {
                let used = sectors.min(MAX_SECTORS);
                let detail = if used < sectors {
                    format!("A new partition of {}; the rest stays free. Other partitions are kept.", size_text(used))
                } else {
                    "A new partition there. Other partitions are kept.".into()
                };
                out.push(Choice {
                    place: Place::Free { start, sectors },
                    title: format!("Free space, {}", size_text(sectors)),
                    question: format!("Install GlassOS into {} of free space on {}?", size_text(sectors), self.model),
                    detail,
                    warning: None,
                });
            }
        }
        for p in t.parts.iter().filter(|p| !p.extended && fits(p.start, p.sectors)) {
            if p.glassos() {
                out.push(Choice {
                    place: Place::Update { slot: p.slot },
                    title: format!("Update GlassOS in partition {}, {}", p.slot + 1, p.size()),
                    question: format!("Update GlassOS in partition {} on {}?", p.slot + 1, self.model),
                    detail: "The kernel and programs are replaced with this version; accounts, settings and files stay.".into(),
                    warning: None,
                });
                out.push(Choice {
                    place: Place::Part { slot: p.slot },
                    title: format!("Reinstall GlassOS in partition {}, {}", p.slot + 1, p.size()),
                    question: format!("Reinstall GlassOS in partition {} on {}?", p.slot + 1, self.model),
                    detail: "The GlassOS there is erased together with its accounts and files; other partitions are kept.".into(),
                    warning: None,
                });
                continue;
            }
            let used = p.sectors.min(MAX_SECTORS);
            let mut detail = String::from("Everything in this partition is deleted; other partitions are kept.");
            if used < p.sectors {
                detail = format!("Everything in it is deleted; GlassOS takes {}, the rest becomes free space.", size_text(used));
            }
            let warning = p.esp.then(|| String::from("Other systems on this disk start from this partition; they may stop starting."));
            out.push(Choice {
                place: Place::Part { slot: p.slot },
                title: format!("Replace partition {}: {}", p.slot + 1, p.describe()),
                question: format!("Erase partition {} ({}) on {} and install GlassOS there?", p.slot + 1, p.describe(), self.model),
                detail,
                warning,
            });
        }
        out
    }
}

pub fn targets() -> Vec<Target> {
    let system = crate::fs::system_sata_disk();
    crate::ahci::disks()
        .into_iter()
        .map(|(disk, sectors, model, _)| {
            let (contents, table) = parts::examine(Drive::Sata(disk), sectors);
            Target { disk, sectors, model, contents, system: system == Some(disk) || crate::fs::holds_boot_partition(disk), table }
        })
        .collect()
}

/// The system files: the kernel and the contents of /apps, /bin, /firmware.
fn system_files() -> Result<Vec<(String, Vec<u8>)>, String> {
    let kernel = BOOT_IMAGE.lock().clone();
    if kernel.is_empty() {
        return Err("the kernel image was not read at startup".into());
    }
    let mut files = vec![(KERNEL_PATH.replace('\\', "/"), kernel)];
    for dir in ["/apps", "/bin", "/firmware"] {
        for e in crate::fs::list(dir).unwrap_or_default().into_iter().filter(|e| !e.is_dir) {
            let path = format!("{dir}/{}", e.name);
            let data = crate::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
            files.push((path, data));
        }
    }
    Ok(files)
}

/// Updates the system files in the GlassOS partition: mounts it (if not mounted),
/// writes the kernel and the programs, removes from /bin what the new version lacks, saves.
fn update_files(drive: Drive, p: &parts::Part, files: &[(String, Vec<u8>)], progress: &mut dyn FnMut(&str, u32)) -> Result<(), String> {
    let was = crate::fs::partition_path(drive, p.start as u32);
    let root = match &was {
        Some(path) => path.clone(),
        None => crate::fs::mount_partition(drive, p.start as u32, p.sectors as u32)?,
    };
    if root == "/" {
        return Err("this system is running from that partition".into());
    }
    let result = crate::task::as_system(|| -> Result<(), String> {
        let fresh: Vec<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
        for e in crate::fs::list(&format!("{root}/bin")).unwrap_or_default().into_iter().filter(|e| !e.is_dir) {
            if !fresh.contains(&format!("/bin/{}", e.name).as_str()) {
                let _ = crate::fs::remove(&format!("{root}/bin/{}", e.name));
            }
        }
        let total = files.iter().map(|f| f.1.len()).sum::<usize>().max(1);
        let mut done = 0;
        for (path, data) in files {
            for (i, _) in path.match_indices('/').skip(1) {
                let _ = crate::fs::mkdir(&format!("{root}{}", &path[..i]));
            }
            crate::fs::write(&format!("{root}{path}"), data).map_err(|e| format!("{path}: {e}"))?;
            done += data.len();
            progress("Copying system files", (done * 95 / total) as u32);
        }
        Ok(())
    });
    // Save to disk; if we mounted it ourselves — unmount it too.
    let saved = if was.is_none() { crate::fs::eject(&root) } else { crate::fs::sync().map(|_| ()) };
    result?;
    saved.map_err(String::from)
}

/// How the installation ended.
pub struct Installed {
    /// The NVRAM boot entry number, or why it could not be written.
    pub boot_entry: Result<u16, String>,
    /// This was an update (accounts and files are in place).
    pub updated: bool,
}

/// Installs the system to disk `disk` at place `place`. `progress` gets the step and the percent.
pub fn install(disk: u32, place: Place, progress: &mut dyn FnMut(&str, u32)) -> Result<Installed, String> {
    let target = targets().into_iter().find(|t| t.disk == disk).ok_or("no such disk")?;
    if target.system {
        return Err("this system is running from that disk".into());
    }
    if target.sectors < MIN_SECTORS + 2 * ALIGN {
        return Err("the disk is smaller than 256 MB".into());
    }
    progress("Reading system files", 0);
    let files = system_files()?;
    let what = match place {
        Place::Whole => format!("whole disk, was {}", target.contents),
        Place::Free { start, sectors } => format!("free space at {start}, {sectors} sectors"),
        Place::Part { slot } => format!("over partition {}", slot + 1),
        Place::Update { slot } => format!("update in partition {}", slot + 1),
    };
    crate::kprintln!("install: {} ({}), {what}, {} files", target.model, target.size(), files.len());
    progress("Writing the partition table", 0);
    let drive = Drive::Sata(disk);
    let placed = match (place, &target.table) {
        (Place::Whole, _) => {
            let table = parts::create_table(drive, target.sectors)?;
            let &(start, sectors) = table.free.first().ok_or("no room on the new partition table")?;
            parts::add(drive, &table, start, sectors.min(MAX_SECTORS), Kind::Esp, GLASSOS)?
        }
        (Place::Free { start, sectors }, Some(table)) => parts::add(drive, table, start, sectors.min(MAX_SECTORS), Kind::Esp, GLASSOS)?,
        (Place::Part { slot }, Some(table)) => parts::replace(drive, table, slot, MAX_SECTORS, Kind::Esp, GLASSOS)?,
        (Place::Update { slot }, Some(table)) => {
            let p = table.parts.iter().find(|p| p.slot == slot && p.glassos()).ok_or("there is no GlassOS in that partition")?;
            progress("Copying system files", 0);
            update_files(drive, p, &files, progress)?;
            table.placed(p)
        }
        (_, None) => return Err("the partition table is not readable".into()),
    };
    let updated = matches!(place, Place::Update { .. });
    if !updated {
        progress("Copying system files", 0);
        let dev = drive.device(placed.start as u32, placed.sectors as u32);
        crate::fs::create_volume(dev, "GLASSOS", &files, &mut |p| progress("Copying system files", p))?;
    }
    crate::kprintln!("install: partition {} at {}, {} sectors", placed.number, placed.start, placed.sectors);
    progress("Adding GlassOS to the firmware boot list", 100);
    let hd = crate::bootvars::HardDrive { number: placed.number, start: placed.start, sectors: placed.sectors, signature: placed.signature, gpt: placed.gpt };
    let boot_entry = crate::bootvars::add(GLASSOS, &hd, KERNEL_PATH);
    if let Err(e) = &boot_entry {
        crate::kprintln!("install: boot option not written: {e}");
    }
    progress("Done", 100);
    Ok(Installed { boot_entry, updated })
}
