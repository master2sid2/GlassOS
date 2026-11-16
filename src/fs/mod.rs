//! The file system: the main FAT32 volume on an ATA disk, the system USB stick or
//! in memory — and mounted USB sticks (FAT32/FAT16) in /media/usb0, /media/usb1...
//!
//! All operations go through the `fat` driver, which keeps changes in a cache;
//! the background saver (`sync_in_background`) carries them to disk — only
//! changed sectors are written. The disk can be opened in any other OS.
//!
//! If the disk has the old GlassOS format (a snapshot of the whole tree), the files are
//! moved to a new FAT32 volume on the first boot.

mod exfat;
pub mod span;
mod fat;
mod legacy;
mod ram;

use crate::access;
use crate::ata::{Disk, SECTOR};
use crate::sync::YieldLock;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use fat::Fat;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    NotADirectory,
    IsADirectory,
    AlreadyExists,
    InvalidName,
    NoSpace,
    /// The file will not fit in memory.
    TooLarge,
    /// Writing is not allowed here: the network neighbourhood; its folders must be mounted first.
    ReadOnly,
    /// Larger than the file system allows.
    FileTooBig,
    /// Closed to this user or program.
    Denied,
    Io,
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            FsError::NotFound => "no such file or directory",
            FsError::NotADirectory => "not a directory",
            FsError::IsADirectory => "is a directory",
            FsError::AlreadyExists => "already exists",
            FsError::InvalidName => "invalid name",
            FsError::NoSpace => "no space left on disk",
            FsError::TooLarge => "not enough memory",
            FsError::ReadOnly => "read-only location",
            FsError::FileTooBig => "file is too large for this disk (FAT32 limit is 4 GiB)",
            FsError::Denied => "access denied",
            FsError::Io => "disk error",
        })
    }
}

pub type Result<T> = core::result::Result<T, FsError>;

pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    /// The file size in bytes or the number of directory entries.
    pub size: usize,
}

/// The device under a volume: an ATA disk or a piece of memory.
pub enum Device {
    Ata(Disk),
    Ram(ram::RamDisk),
    /// A USB disk partition: the disk number (`usb::disks`), the first sector and the partition's sector count.
    Usb { disk: u32, start: u32, sectors: u32 },
    /// A SATA disk partition (`ahci::disks`) — the same.
    Sata { disk: u32, start: u32, sectors: u32 },
}

/// Transfer counters for disks of one kind (for metrics).
pub struct DiskIo {
    pub reads: AtomicU64,
    pub read_bytes: AtomicU64,
    pub writes: AtomicU64,
    pub written_bytes: AtomicU64,
    /// CPU cycles while the transfer was running.
    pub busy: AtomicU64,
}

impl DiskIo {
    const fn new() -> Self {
        Self { reads: AtomicU64::new(0), read_bytes: AtomicU64::new(0), writes: AtomicU64::new(0), written_bytes: AtomicU64::new(0), busy: AtomicU64::new(0) }
    }

    fn count(&self, write: bool, bytes: usize, started: u64) {
        let (ops, total) = if write { (&self.writes, &self.written_bytes) } else { (&self.reads, &self.read_bytes) };
        ops.fetch_add(1, Ordering::Relaxed);
        total.fetch_add(bytes as u64, Ordering::Relaxed);
        self.busy.fetch_add(crate::arch::rdtsc() - started, Ordering::Relaxed);
    }
}

/// Transfers by disk kind: ATA, USB, in memory, SATA (AHCI).
pub static DISK_IO: [(&str, DiskIo); 4] = [("ata", DiskIo::new()), ("usb", DiskIo::new()), ("ram", DiskIo::new()), ("sata", DiskIo::new())];

impl Device {
    fn io(&self) -> &'static DiskIo {
        match self {
            Device::Ata(_) => &DISK_IO[0].1,
            Device::Usb { .. } => &DISK_IO[1].1,
            Device::Ram(_) => &DISK_IO[2].1,
            Device::Sata { .. } => &DISK_IO[3].1,
        }
    }

    pub fn sectors(&self) -> u32 {
        match self {
            Device::Ata(d) => d.sectors,
            Device::Ram(r) => r.sectors(),
            Device::Usb { sectors, .. } | Device::Sata { sectors, .. } => *sectors,
        }
    }

    /// Another description of the same partition (ATA and in-memory volumes have none).
    fn twin(&self) -> Option<Device> {
        match self {
            Device::Usb { disk, start, sectors } => Some(Device::Usb { disk: *disk, start: *start, sectors: *sectors }),
            Device::Sata { disk, start, sectors } => Some(Device::Sata { disk: *disk, start: *start, sectors: *sectors }),
            _ => None,
        }
    }

    /// The partition's first sector on the disk.
    fn start(&self) -> u32 {
        match self {
            Device::Usb { start, .. } | Device::Sata { start, .. } => *start,
            _ => 0,
        }
    }

    /// Whether this is the partition of disk `drive` with first sector `start`.
    fn is(&self, drive: crate::parts::Drive, at: u32) -> bool {
        use crate::parts::Drive;
        match (self, drive) {
            (Device::Sata { disk, start, .. }, Drive::Sata(d)) | (Device::Usb { disk, start, .. }, Drive::Usb(d)) => *disk == d && *start == at,
            _ => false,
        }
    }

    pub fn read(&self, lba: u32, buf: &mut [u8]) -> bool {
        let started = crate::arch::rdtsc();
        let ok = self.read_raw(lba, buf);
        self.io().count(false, buf.len(), started);
        ok
    }

    fn read_raw(&self, lba: u32, buf: &mut [u8]) -> bool {
        match self {
            Device::Ata(d) => d.read(lba, buf),
            Device::Usb { disk, start, sectors } => {
                lba + (buf.len() / SECTOR) as u32 <= *sectors && crate::usb::disk_read(*disk, start + lba, buf)
            }
            Device::Ram(r) => r.read(lba, buf),
            Device::Sata { disk, start, sectors } => {
                lba + (buf.len() / SECTOR) as u32 <= *sectors && crate::ahci::disk_read(*disk, (*start + lba) as u64, buf)
            }
        }
    }

    /// A data stream write: without flushing the disk cache, if the device distinguishes this.
    pub fn write_data(&mut self, lba: u32, data: &[u8]) -> bool {
        let (started, io) = (crate::arch::rdtsc(), self.io());
        let ok = match self {
            Device::Ata(d) => d.write_data(lba, data),
            Device::Sata { disk, start, sectors } => {
                lba + data.len().div_ceil(SECTOR) as u32 <= *sectors && crate::ahci::disk_write_data(*disk, (*start + lba) as u64, data)
            }
            other => other.write_raw(lba, data),
        };
        io.count(true, data.len(), started);
        ok
    }

    /// The sectors' contents are no longer needed. Only frees an in-memory volume.
    pub fn discard(&mut self, lba: u32, sectors: u32) {
        if let Device::Ram(r) = self {
            r.discard(lba, sectors);
        }
    }

    pub fn write(&mut self, lba: u32, data: &[u8]) -> bool {
        let started = crate::arch::rdtsc();
        let ok = self.write_raw(lba, data);
        self.io().count(true, data.len(), started);
        ok
    }

    fn write_raw(&mut self, lba: u32, data: &[u8]) -> bool {
        match self {
            Device::Ata(d) => d.write(lba, data),
            Device::Usb { disk, start, sectors } => {
                lba + data.len().div_ceil(SECTOR) as u32 <= *sectors && crate::usb::disk_write(*disk, *start + lba, data)
            }
            Device::Ram(r) => r.write(lba, data),
            Device::Sata { disk, start, sectors } => {
                lba + data.len().div_ceil(SECTOR) as u32 <= *sectors && crate::ahci::disk_write(*disk, (*start + lba) as u64, data)
            }
        }
    }
}

fn device_discard(dev: usize, lba: u32, sectors: u32) {
    if let Some(Some(d)) = DEVS.lock().get_mut(dev) {
        d.discard(lba, sectors);
    }
}

/// Lock order: FS or MOUNTS first, then DEVS (the saver takes
/// only DEVS). All of them yield the CPU instead of disabling interrupts: the disk is slow.
static FS: YieldLock<Option<Fat>> = YieldLock::new(None);
/// Volume devices: 0 — the main one, then the mounted USB sticks.
pub(crate) static DEVS: YieldLock<Vec<Option<Device>>> = YieldLock::new(Vec::new());
static MOUNTS: YieldLock<Mounts> = YieldLock::new(Mounts { list: Vec::new(), tried: Vec::new() });
/// Network folders that are only being browsed: (resource path in the neighbourhood, volume).
static BROWSED: YieldLock<Vec<(String, Volumeh)>> = YieldLock::new(Vec::new());

/// The folder where USB sticks appear.
pub const MEDIA: &str = "/media";
/// The network neighbourhood: /network/workgroup/computer/resource (see net::discovery).
pub const NETWORK: &str = "/network";

/// A volume with files: FAT32/FAT16 or exFAT. Paths are inside the volume.
pub trait Filesystem: Send {
    fn list(&mut self, path: &str) -> Result<Vec<Entry>>;
    fn read(&mut self, path: &str) -> Result<Vec<u8>>;
    /// A piece of a file: up to `len` bytes from `offset`, and the full file size.
    fn read_range(&mut self, path: &str, offset: u64, len: usize) -> Result<(Vec<u8>, u64)> {
        let all = self.read(path)?;
        let total = all.len() as u64;
        let from = (offset as usize).min(all.len());
        Ok((all[from..(from + len).min(all.len())].to_vec(), total))
    }
    /// Some(true) — a directory, Some(false) — a file, None — no such thing.
    fn is_dir(&mut self, path: &str) -> Option<bool>;
    fn write(&mut self, path: &str, data: &[u8]) -> Result<()>;
    /// Writes a piece of an existing file at `offset`, not beyond its current end
    /// (`fs::write_at` fills the gap).
    fn write_at(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<()>;
    /// Truncates a file.
    fn shrink(&mut self, path: &str, len: u64) -> Result<()>;
    /// The largest file on this volume.
    fn max_file(&self) -> u64 {
        u64::MAX
    }
    /// How many changes are waiting to be written to the device.
    fn pending_bytes(&self) -> usize {
        0
    }
    fn mkdir(&mut self, path: &str) -> Result<()>;
    fn remove(&mut self, path: &str) -> Result<()>;
    fn rename(&mut self, from: &str, to: &str) -> Result<()>;
    /// Used and total bytes.
    fn usage(&self) -> (u64, u64);
    fn has_changes(&self) -> bool;
    /// Changed sectors to be written (the cache remembers that a write is in progress).
    fn take_dirty(&mut self) -> Vec<(u32, alloc::boxed::Box<[u8; SECTOR]>)>;
    fn set_writing(&mut self, on: bool);
    fn trim(&mut self);
    fn label(&self) -> &str;
    /// The device index in `DEVS`.
    fn dev(&self) -> usize;
    /// Whether the volume is online (a network folder may be temporarily unavailable).
    fn online(&self) -> bool {
        true
    }
    /// «FAT32», «FAT16», «exFAT».
    fn kind(&self) -> &'static str;
}

impl Filesystem for Fat {
    fn list(&mut self, path: &str) -> Result<Vec<Entry>> {
        Fat::list(self, path)
    }
    fn read(&mut self, path: &str) -> Result<Vec<u8>> {
        Fat::read(self, path)
    }
    fn read_range(&mut self, path: &str, offset: u64, len: usize) -> Result<(Vec<u8>, u64)> {
        Fat::read_range(self, path, offset, len)
    }
    fn is_dir(&mut self, path: &str) -> Option<bool> {
        Fat::is_dir(self, path)
    }
    fn write(&mut self, path: &str, data: &[u8]) -> Result<()> {
        Fat::write(self, path, data)
    }
    fn write_at(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<()> {
        Fat::write_at(self, path, offset, data)
    }
    fn shrink(&mut self, path: &str, len: u64) -> Result<()> {
        Fat::shrink(self, path, len)
    }
    fn max_file(&self) -> u64 {
        u32::MAX as u64
    }
    fn pending_bytes(&self) -> usize {
        Fat::pending_bytes(self)
    }
    fn mkdir(&mut self, path: &str) -> Result<()> {
        Fat::mkdir(self, path)
    }
    fn remove(&mut self, path: &str) -> Result<()> {
        Fat::remove(self, path)
    }
    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        Fat::rename(self, from, to)
    }
    fn usage(&self) -> (u64, u64) {
        Fat::usage(self)
    }
    fn has_changes(&self) -> bool {
        Fat::has_changes(self)
    }
    fn take_dirty(&mut self) -> Vec<(u32, alloc::boxed::Box<[u8; SECTOR]>)> {
        Fat::take_dirty(self)
    }
    fn set_writing(&mut self, on: bool) {
        self.writing = on;
    }
    fn trim(&mut self) {
        Fat::trim(self)
    }
    fn label(&self) -> &str {
        &self.label
    }
    fn dev(&self) -> usize {
        self.dev
    }
    fn kind(&self) -> &'static str {
        if self.is_fat16() { "FAT16" } else { "FAT32" }
    }
}

/// A volume with its own lock: a network folder may take seconds to answer, and all that
/// time the other volumes and the volume list must keep working.
type Volumeh = alloc::sync::Arc<YieldLock<alloc::boxed::Box<dyn Filesystem>>>;

/// Where a volume in /media comes from.
#[derive(Clone, Copy, PartialEq)]
enum Source {
    /// A USB disk (number in `usb::disks`).
    Usb(u32),
    /// A SATA disk partition: the disk number and the partition's first sector.
    Sata(u32, u32),
    /// A network folder.
    Network,
}

struct Mount {
    /// The name in /media ("usb0", "disk0").
    name: String,
    fat: Volumeh,
    source: Source,
    /// The volume's device index in `DEVS` (`usize::MAX` for a network folder).
    dev: usize,
    /// Label, type, used, total, online — the last values we could learn
    /// (a volume may be busy with a long operation, and the panel cannot wait).
    seen: (String, &'static str, u64, u64, bool),
}

impl Mount {
    fn new(name: String, fat: alloc::boxed::Box<dyn Filesystem>, source: Source) -> Self {
        let (used, total) = fat.usage();
        let seen = (String::from(fat.label()), fat.kind(), used, total, fat.online());
        let dev = fat.dev();
        Self { name, fat: alloc::sync::Arc::new(YieldLock::new(fat)), source, dev, seen }
    }
}

struct Mounts {
    list: Vec<Mount>,
    /// USB disks already looked at (mounted, unreadable or ejected).
    tried: Vec<u32>,
}

fn set_dev(id: usize, dev: Option<Device>) {
    if id == usize::MAX {
        return; // network folders have no device
    }
    let mut devs = DEVS.lock();
    if devs.len() <= id {
        devs.resize_with(id + 1, || None);
    }
    devs[id] = dev;
}

/// A free index in the device table (0 is taken by the main volume).
fn free_dev() -> usize {
    let devs = DEVS.lock();
    (1..).find(|&i| devs.get(i).is_none_or(|d| d.is_none())).unwrap()
}
/// A volume on a real disk (not in memory).
static ON_DISK: AtomicBool = AtomicBool::new(false);
/// A background save is running.
static SYNCING: AtomicBool = AtomicBool::new(false);
/// Files of the main volume being appended in pieces right now: (path, write tick).
static GROWING: YieldLock<Vec<(String, u64)>> = YieldLock::new(Vec::new());
/// After this much quiet the file counts as finished.
const SETTLE_TICKS: u64 = 10 * crate::interrupts::HZ;
/// Grows with every change (the WM and applications refresh their lists by it).
static VERSION: AtomicU64 = AtomicU64::new(0);
/// Grows when a save found changes on the main volume.
static ROOT_EPOCH: AtomicU64 = AtomicU64::new(0);

pub fn version() -> u64 {
    VERSION.load(Ordering::Relaxed)
}

pub fn root_epoch() -> u64 {
    ROOT_EPOCH.load(Ordering::Relaxed)
}

/// Something changed besides files (the network neighbourhood list, the connection to a server):
/// open folders re-read themselves.
pub fn touch() {
    VERSION.fetch_add(1, Ordering::Relaxed);
}

/// The file is still being written: too early to copy it.
pub fn settling(path: &str) -> bool {
    let now = crate::interrupts::ticks();
    GROWING.lock().iter().any(|(p, at)| p == path && now < at + SETTLE_TICKS)
}

fn note_growing(path: &str) {
    if media_path(path).is_none() {
        let now = crate::interrupts::ticks();
        let mut list = GROWING.lock();
        list.retain(|(p, at)| p != path && now < at + SETTLE_TICKS);
        list.push((path.into(), now));
    }
}

/// A folder on a network volume: an answer may take seconds.
pub fn is_remote(path: &str) -> bool {
    crate::net::discovery::share_of(path).is_some() || media_path(path).is_some_and(|(name, _)| MOUNTS.lock().list.iter().any(|m| m.name == name && m.source == Source::Network))
}

fn browsed(path: &str) -> Option<(Volumeh, String)> {
    let (key, inner) = crate::net::discovery::share_of(path)?;
    let volume = BROWSED.lock().iter().find(|(k, _)| k.eq_ignore_ascii_case(key))?.1.clone();
    Some((volume, alloc::format!("/{}", inner.trim_start_matches('/'))))
}

/// Whether the resource holding `path` is open for browsing.
pub fn is_browsing(path: &str) -> bool {
    browsed(path).is_some()
}

pub fn add_browsed(key: &str, fs: alloc::boxed::Box<dyn Filesystem>) {
    let mut list = BROWSED.lock();
    if !list.iter().any(|(k, _)| k.eq_ignore_ascii_case(key)) {
        list.push((key.into(), alloc::sync::Arc::new(YieldLock::new(fs))));
    }
}

/// Browsing a network folder: the connection comes up on first access. A lost connection is not
/// kept, the next access will try again.
fn with_browsed<R>(path: &str, f: impl FnOnce(&mut dyn Filesystem, &str) -> Result<R>) -> Result<R> {
    if browsed(path).is_none() {
        crate::net::smb::browse(path).map_err(|e| if crate::net::smb::is_login_error(&e) { FsError::Denied } else { FsError::Io })?;
    }
    let (volume, inner) = browsed(path).ok_or(FsError::Io)?;
    let mut guard = volume.lock();
    let r = f(&mut **guard, &inner);
    if !guard.online() {
        drop(guard);
        BROWSED.lock().retain(|(_, v)| !alloc::sync::Arc::ptr_eq(v, &volume));
    }
    r
}

/// A folder on a network volume that cannot be reached right now.
pub fn offline(path: &str) -> bool {
    media_path(path).is_some_and(|(name, _)| MOUNTS.lock().list.iter().any(|m| m.name == name && !m.seen.4))
}

fn readable(path: &str) -> Result<()> {
    if access::may_read(path) { Ok(()) } else { Err(FsError::Denied) }
}

/// The neighbourhood down to the resource level (groups, computers, the resources themselves) is only a list;
/// inside a resource the server writes, and it will say if that is not allowed.
fn network_listing(path: &str) -> bool {
    crate::net::discovery::depth(path).is_some_and(|d| d <= 3)
}

fn writable(path: &str) -> Result<()> {
    if network_listing(path) {
        return Err(FsError::ReadOnly);
    }
    if access::may_write(path) { Ok(()) } else { Err(FsError::Denied) }
}

fn removable(path: &str) -> Result<()> {
    if network_listing(path) {
        return Err(FsError::ReadOnly);
    }
    if access::may_remove(path) { Ok(()) } else { Err(FsError::Denied) }
}

fn changed<T>(r: Result<T>) -> Result<T> {
    if r.is_ok() {
        VERSION.fetch_add(1, Ordering::Relaxed);
    }
    r
}

/// A path inside /media: (stick name, path on it).
fn media_path(path: &str) -> Option<(&str, String)> {
    let rest = path.strip_prefix(MEDIA)?.strip_prefix('/')?;
    let (name, inner) = rest.split_once('/').unwrap_or((rest, ""));
    (!name.is_empty()).then(|| (name, alloc::format!("/{inner}")))
}

/// Runs `f` on the volume holding `path` (with the path inside the volume).
fn with_fs<R>(path: &str, f: impl FnOnce(&mut dyn Filesystem, &str) -> Result<R>) -> Result<R> {
    if let Some((name, inner)) = media_path(path) {
        let volume = MOUNTS.lock().list.iter().find(|m| m.name == name).ok_or(FsError::NotFound)?.fat.clone();
        let mut guard = volume.lock();
        return f(&mut **guard, &inner);
    }
    if crate::net::discovery::share_of(path).is_some() {
        return with_browsed(path, f);
    }
    if path.starts_with("/network/") {
        return Err(if crate::net::discovery::is_dir(path) { FsError::ReadOnly } else { FsError::NotFound });
    }
    match FS.lock().as_mut() {
        Some(fat) => f(fat, path),
        None => Err(FsError::Io),
    }
}

/// The volume of a path: a USB stick or network folder in /media, a resource in the neighbourhood or the main disk.
fn volume_of(path: &str) -> String {
    if let Some((name, _)) = media_path(path) {
        return alloc::format!("{MEDIA}/{name}").to_ascii_lowercase();
    }
    if let Some((key, _)) = crate::net::discovery::share_of(path) {
        return key.to_ascii_lowercase();
    }
    String::from("/")
}

/// Whether two paths are on the same volume (then a move is just a rename).
pub fn same_volume(a: &str, b: &str) -> bool {
    volume_of(a) == volume_of(b)
}

/// An absolute normalized path: `path` relative to `cwd`, honouring `.` and `..`.
pub fn resolve(cwd: &str, path: &str) -> String {
    let mut parts: Vec<&str> = if path.starts_with('/') { Vec::new() } else { components(cwd).collect() };
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    let mut out = String::from("/");
    out.push_str(&parts.join("/"));
    out
}

pub fn parent(path: &str) -> String {
    resolve(path, "..")
}

pub fn name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or("")
}

fn components(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|s| !s.is_empty())
}

pub fn list(path: &str) -> Result<Vec<Entry>> {
    readable(path)?;
    if let Some(entries) = crate::net::discovery::list(path) {
        return entries;
    }
    if path == MEDIA {
        // /media holds the mounted sticks (the folder itself on the main volume is empty).
        let mounts = MOUNTS.lock();
        return Ok(mounts.list.iter().map(|m| Entry { name: m.name.clone(), is_dir: true, size: 0 }).collect());
    }
    let mut entries = with_fs(path, |f, p| f.list(p))?;
    // In the homes folder only one's own home is visible.
    if resolve("/", path).eq_ignore_ascii_case("/home") {
        entries.retain(|e| access::may_read(&alloc::format!("/home/{}", e.name)));
    }
    Ok(entries)
}

/// How much unsaved data to keep in memory while writing a large file.
const STREAM_PENDING: usize = 8 << 20;

/// The file size.
pub fn file_len(path: &str) -> Result<u64> {
    read_range(path, 0, 0).map(|(_, size)| size)
}

/// Whether a file can be grown to `len` (or another `grow` bytes with the largest
/// file `len` put into folder `path`): whether there is room and the file system allows it.
pub fn can_hold(path: &str, grow: u64, len: u64) -> Result<()> {
    // A resource in the neighbourhood is only a list entry by itself, but things can be put into it.
    if crate::net::discovery::depth(path).is_some_and(|d| d < 3) {
        return Err(FsError::ReadOnly);
    }
    if !access::may_write(path) {
        return Err(FsError::Denied);
    }
    let (used, total) = usage_of(path).unwrap_or((0, 0));
    with_fs(path, |f, _| if len > f.max_file() { Err(FsError::FileTooBig) } else { Ok(()) })?;
    if total > 0 && grow > total.saturating_sub(used) {
        return Err(FsError::NoSpace);
    }
    Ok(())
}

/// Flushes what has accumulated to the device: a large file must not pile up in memory.
fn relieve(path: &str) {
    if with_fs(path, |f, _| Ok(f.pending_bytes())).unwrap_or(0) > STREAM_PENDING {
        while SYNCING.swap(true, Ordering::Acquire) {
            crate::task::yield_now();
        }
        if let Err(e) = sync() {
            crate::kprintln!("fs: save failed: {e}");
        }
        SYNCING.store(false, Ordering::Release);
    }
}

/// Writes a piece of a file at `offset`; the file is not kept in memory. The gap up to `offset`
/// is filled with zeros. `fs::touch` at the end tells open folders about the new size.
pub fn write_at(path: &str, offset: u64, data: &[u8]) -> Result<()> {
    const ZEROS: usize = 1 << 20;
    writable(path)?;
    let size = file_len(path)?;
    let end = offset.checked_add(data.len() as u64).ok_or(FsError::FileTooBig)?;
    if end > size {
        can_hold(path, end - size, end)?;
    }
    if offset > size {
        let zeros = alloc::vec![0u8; ZEROS];
        let mut at = size;
        while at < offset {
            let n = (offset - at).min(ZEROS as u64) as usize;
            with_fs(path, |f, p| f.write_at(p, at, &zeros[..n]))?;
            relieve(path);
            at += n as u64;
        }
    }
    with_fs(path, |f, p| f.write_at(p, offset, data))?;
    note_growing(path);
    relieve(path);
    Ok(())
}

/// Sets the file length: cuts off the excess, fills the missing part with zeros.
pub fn set_len(path: &str, len: u64) -> Result<()> {
    writable(path)?;
    let size = file_len(path)?;
    if len < size {
        changed(with_fs(path, |f, p| f.shrink(p, len)))
    } else if len > size {
        write_at(path, len, &[])
    } else {
        Ok(())
    }
}

/// A folder of the network neighbourhood (a workgroup, a computer or its resource).
fn is_network(path: &str) -> bool {
    crate::net::discovery::is_dir(path)
}

/// The heap reserve that reading a whole file does not use: files are read into shared
/// kernel memory, and without a reserve a large file would stop the whole system. Smaller
/// than the reserves for windows (32 MiB) and Lua (16 MiB): when those are exhausted, the system
/// must still read settings and small files.
pub const READ_RESERVE: usize = 8 << 20;

pub fn read(path: &str) -> Result<Vec<u8>> {
    readable(path)?;
    if let Ok((_, size)) = read_range(path, 0, 0)
        && crate::heap_stats().1 < (size as usize).saturating_add(READ_RESERVE)
    {
        return Err(FsError::TooLarge);
    }
    with_fs(path, |f, p| f.read(p))
}

/// A piece of a file (for copying in parts): the data and the full file size.
pub fn read_range(path: &str, offset: u64, len: usize) -> Result<(Vec<u8>, u64)> {
    readable(path)?;
    with_fs(path, |f, p| f.read_range(p, offset, len))
}

pub fn exists(path: &str) -> bool {
    access::may_read(path) && (is_network(path) || path == MEDIA || with_fs(path, |f, p| Ok(f.is_dir(p))).ok().flatten().is_some())
}

pub fn is_dir(path: &str) -> bool {
    access::may_read(path) && (is_network(path) || path == MEDIA || with_fs(path, |f, p| Ok(f.is_dir(p))).ok().flatten() == Some(true))
}

/// Creates a file or overwrites an existing one. The same contents
/// change nothing (and do not wake the disk saver).
pub fn write(path: &str, data: &[u8]) -> Result<()> {
    writable(path)?;
    // What was written stays in memory until the background saver sends it to disk.
    if crate::heap_stats().1 < (data.len() + data.len() / 8).saturating_add(READ_RESERVE) {
        return Err(FsError::TooLarge);
    }
    if read(path).is_ok_and(|old| old == data) {
        return Ok(());
    }
    changed(with_fs(path, |f, p| f.write(p, data)))
}

pub fn append(path: &str, data: &[u8]) -> Result<()> {
    match file_len(path) {
        Ok(size) => {
            write_at(path, size, data)?;
            touch();
            Ok(())
        }
        Err(FsError::NotFound) => write(path, data),
        Err(e) => Err(e),
    }
}

pub fn mkdir(path: &str) -> Result<()> {
    writable(path)?;
    changed(with_fs(path, |f, p| f.mkdir(p)))
}

/// Deletes a file or a directory with its contents.
pub fn remove(path: &str) -> Result<()> {
    removable(path)?;
    // A USB stick itself (its root) and /media cannot be deleted.
    if path == MEDIA || media_path(path).is_some_and(|(_, inner)| inner == "/") {
        return Err(FsError::InvalidName);
    }
    changed(with_fs(path, |f, p| f.remove(p)))
}

pub fn rename(from: &str, to: &str) -> Result<()> {
    removable(from)?;
    writable(to)?;
    if name(from).is_empty() || name(to).is_empty() || to.starts_with(&alloc::format!("{from}/")) {
        return Err(FsError::InvalidName);
    }
    if from == to {
        return Ok(());
    }
    if !same_volume(from, to) {
        // From volume to volume — a copy and a delete.
        copy(from, to)?;
        return remove(from);
    }
    if from == MEDIA || media_path(from).is_some_and(|(_, inner)| inner == "/") {
        return Err(FsError::InvalidName);
    }
    let inner = media_path(to).map_or(String::from(to), |(_, p)| p);
    changed(with_fs(from, |f, p| f.rename(p, &inner)))
}

/// Copies a file or a directory (with all its contents) to a new path.
pub fn copy(from: &str, to: &str) -> Result<()> {
    if to.starts_with(&alloc::format!("{from}/")) {
        return Err(FsError::InvalidName);
    }
    if exists(to) {
        return Err(FsError::AlreadyExists);
    }
    if is_dir(from) {
        mkdir(to)?;
        for e in list(from)? {
            copy(&resolve(from, &e.name), &resolve(to, &e.name))?;
        }
        Ok(())
    } else {
        copy_file(from, to, |_| crate::task::stop_reason(0) != 2)
    }
}

/// Copies a file in pieces: `step(bytes copied)` may interrupt the copy
/// (false). A failed copy is deleted.
pub fn copy_file(from: &str, to: &str, mut step: impl FnMut(u64) -> bool) -> Result<()> {
    const CHUNK: usize = 1 << 20;
    write(to, b"")?;
    let mut offset = 0;
    let result = loop {
        let (chunk, total) = match read_range(from, offset, CHUNK) {
            Ok(r) => r,
            Err(e) => break Err(e),
        };
        if chunk.is_empty() {
            break Ok(());
        }
        if let Err(e) = write_at(to, offset, &chunk) {
            break Err(e);
        }
        offset += chunk.len() as u64;
        if !step(offset) {
            break Err(FsError::Io);
        }
        if offset >= total {
            break Ok(());
        }
    };
    if result.is_err() {
        let _ = remove(to);
    }
    touch();
    result
}

/// A free name in directory `dir`: "base", "base 2", "base 3"… (the extension is kept).
pub fn unique_name(dir: &str, base: &str) -> String {
    let (stem, ext) = match base.rfind('.') {
        Some(i) if i > 0 => (&base[..i], &base[i..]),
        _ => (base, ""),
    };
    let mut candidate = base.to_string();
    let mut n = 2;
    while exists(&resolve(dir, &candidate)) {
        candidate = alloc::format!("{stem} {n}{ext}");
        n += 1;
    }
    candidate
}

// ---------------------------------------------------------------- volume

/// Moves an old-format tree to a fresh volume.
fn import(path: &str, node: legacy::Node) {
    match node {
        legacy::Node::File(data) => {
            if let Err(e) = write(path, &data) {
                crate::kprintln!("fs: migrate {path}: {e}");
            }
        }
        legacy::Node::Dir(children) => {
            if path != "/" {
                let _ = mkdir(path);
            }
            for (name, child) in children {
                import(&resolve(path, &name), child);
            }
        }
    }
}

/// What was found on the disk when mounting.
pub enum Mounted {
    /// A FAT32 volume with files.
    Fat32,
    /// The old GlassOS format: the files were moved to a new FAT32 volume.
    Migrated,
    /// The disk was empty (or foreign) — a new volume was created.
    Formatted,
}

/// Mounts the disk: FAT32 as is; the old format is moved to FAT32;
/// an empty disk is formatted. A disk with foreign data is left alone (`None`): booted from an ISO
/// on a real computer, that is its own disk with another system.
pub fn mount(disk: Disk) -> Option<Mounted> {
    let dev = Device::Ata(disk);
    if let Some(fat) = Fat::mount(&dev, 0).filter(|f| !f.is_fat16()) {
        ON_DISK.store(true, Ordering::Relaxed);
        set_dev(0, Some(dev));
        *FS.lock() = Some(fat);
        VERSION.fetch_add(1, Ordering::Relaxed);
        return Some(Mounted::Fat32);
    }
    let old = legacy::load(&dev);
    if old.is_none() && !blank(&dev) {
        return None;
    }
    ON_DISK.store(true, Ordering::Relaxed);
    let sectors = dev.sectors();
    set_dev(0, Some(dev));
    *FS.lock() = Some(Fat::format(sectors, 0));
    VERSION.fetch_add(1, Ordering::Relaxed);
    Some(match old {
        Some(tree) => {
            import("/", tree);
            Mounted::Migrated
        }
        None => Mounted::Formatted,
    })
}

/// Whether the disk is empty: the first 128 KiB are zeros. They hold the MBR, the GPT header and entries and
/// the service blocks of almost all file systems (NTFS, ext, XFS, Btrfs, LVM, ISO 9660).
fn blank(dev: &Device) -> bool {
    let mut buf = vec![0u8; 128 * 1024];
    dev.read(0, &mut buf) && buf.iter().all(|&b| b == 0)
}

/// FAT partitions on a disk: (first sector, sector count). GPT, MBR or the whole disk.
fn fat_partitions(read: &dyn Fn(u32, &mut [u8]) -> bool, total: u64) -> Vec<(u32, u32)> {
    let mut s0 = [0u8; SECTOR];
    if !read(0, &mut s0) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let le32 = |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
    let le64 = |b: &[u8], at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
    if s0[510..512] == [0x55, 0xAA] && s0[450] == 0xEE {
        // GPT: the partition entries (usually from sector 2).
        let mut h = [0u8; SECTOR];
        if read(1, &mut h) && &h[0..8] == b"EFI PART" {
            let (lba, count, size) = (le64(&h, 72) as u32, le32(&h, 80).min(128), le32(&h, 84) as usize);
            let mut table = vec![0u8; (count as usize * size).div_ceil(SECTOR) * SECTOR];
            if size >= 128 && read(lba, &mut table) {
                for e in table.chunks(size).take(count as usize) {
                    let (first, last) = (le64(e, 32), le64(e, 40));
                    if e[..16] != [0; 16] && last >= first && last < total {
                        out.push((first as u32, (last - first + 1) as u32));
                    }
                }
            }
        }
    } else if s0[510..512] == [0x55, 0xAA] && &s0[82..87] != b"FAT32" && &s0[54..59] != b"FAT16" && &s0[3..8] != b"EXFAT" {
        // MBR: FAT16 (0x04, 0x06, 0x0E), FAT32 (0x0B, 0x0C), exFAT/NTFS (0x07), EFI (0xEF) partitions.
        for e in s0[446..510].chunks(16) {
            if matches!(e[4], 0x04 | 0x06 | 0x0E | 0x0B | 0x0C | 0x07 | 0xEF) {
                out.push((le32(e, 8), le32(e, 12)));
            }
        }
    }
    if out.is_empty() {
        out.push((0, total.min(u32::MAX as u64) as u32)); // the whole disk — a "superfloppy"
    }
    out
}

/// Looks for the GlassOS boot stick (a FAT32 partition with \EFI\BOOT\BOOTX64.EFI and \apps)
/// and makes it the system disk: files will be stored right on it.
/// Other sticks are left alone — they hold someone else's files.
pub fn mount_usb() -> Option<String> {
    for (disk, blocks) in crate::usb::disks() {
        let read = |lba: u32, buf: &mut [u8]| crate::usb::disk_read(disk, lba, buf);
        for (start, sectors) in fat_partitions(&read, blocks) {
            let dev = Device::Usb { disk, start, sectors };
            let Some(fat) = Fat::mount(&dev, 0).filter(|f| !f.is_fat16()) else { continue };
            set_dev(0, Some(dev));
            *FS.lock() = Some(fat);
            if is_dir("/apps") && exists("/EFI/BOOT/BOOTX64.EFI") {
                ON_DISK.store(true, Ordering::Relaxed);
                VERSION.fetch_add(1, Ordering::Relaxed);
                MOUNTS.lock().tried.push(disk);
                return Some(alloc::format!("Disk: USB stick, {} MiB FAT32", sectors / 2048));
            }
            *FS.lock() = None;
            set_dev(0, None);
        }
    }
    None
}

/// The partition the firmware loaded the kernel from (from the UEFI device path).
#[derive(Clone, Copy)]
pub struct BootPart {
    /// The partition GUID in the GPT or the MBR disk signature.
    pub guid: Option<[u8; 16]>,
    pub mbr: Option<[u8; 4]>,
    pub start: u64,
    pub sectors: u64,
}

pub static BOOT_PART: YieldLock<Option<BootPart>> = YieldLock::new(None);

/// The partition on the disk we booted from: (first sector, sector count).
fn boot_partition_on(read: &dyn Fn(u32, &mut [u8]) -> bool, boot: &BootPart) -> Option<(u32, u32)> {
    let mut s0 = [0u8; SECTOR];
    if !read(0, &mut s0) || s0[510..512] != [0x55, 0xAA] {
        return None;
    }
    let fits = |start: u64, sectors: u64| {
        (start == boot.start && sectors == boot.sectors && start + sectors <= u32::MAX as u64).then_some((start as u32, sectors as u32))
    };
    let le = |b: &[u8], at: usize, n: usize| b[at..at + n].iter().rev().fold(0u64, |v, &x| v << 8 | x as u64);
    if let Some(guid) = boot.guid {
        let mut h = [0u8; SECTOR];
        if !read(1, &mut h) || &h[0..8] != b"EFI PART" {
            return None;
        }
        let (lba, count, size) = (le(&h, 72, 8) as u32, le(&h, 80, 4).min(128) as usize, le(&h, 84, 4) as usize);
        if size < 128 {
            return None;
        }
        let mut table = vec![0u8; (count * size).div_ceil(SECTOR) * SECTOR];
        if !read(lba, &mut table) {
            return None;
        }
        let e = table.chunks(size).take(count).find(|e| e[16..32] == guid)?;
        return fits(le(e, 32, 8), le(e, 40, 8) + 1 - le(e, 32, 8));
    }
    let sig = boot.mbr?;
    if s0[440..444] != sig {
        return None;
    }
    s0[446..510].chunks(16).find_map(|e| fits(le(e, 8, 4), le(e, 12, 4)))
}

/// The first sector of the boot partition, if it is on this SATA disk.
pub fn boot_partition_start(disk: u32) -> Option<u32> {
    let boot = (*BOOT_PART.lock())?;
    let read = |lba: u32, buf: &mut [u8]| crate::ahci::disk_read(disk, lba as u64, buf);
    boot_partition_on(&read, &boot).map(|(start, _)| start)
}

/// Whether the SATA disk holds the partition we booted from.
pub fn holds_boot_partition(disk: u32) -> bool {
    let Some(boot) = *BOOT_PART.lock() else { return false };
    let read = |lba: u32, buf: &mut [u8]| crate::ahci::disk_read(disk, lba as u64, buf);
    boot_partition_on(&read, &boot).is_some()
}

/// Looks on the SATA disks for the partition we booted from and makes it the main volume
/// (an installed system). Booted from an ISO or a stick — does nothing.
pub fn mount_sata() -> Option<String> {
    let boot = (*BOOT_PART.lock())?;
    for (disk, _, model, _) in crate::ahci::disks() {
        let read = |lba: u32, buf: &mut [u8]| crate::ahci::disk_read(disk, lba as u64, buf);
        let Some((start, sectors)) = boot_partition_on(&read, &boot) else { continue };
        let dev = Device::Sata { disk, start, sectors };
        let Some(fat) = Fat::mount(&dev, 0).filter(|f| !f.is_fat16()) else {
            crate::kprintln!("fs: the boot partition on {model} is not FAT32");
            continue;
        };
        set_dev(0, Some(dev));
        *FS.lock() = Some(fat);
        ON_DISK.store(true, Ordering::Relaxed);
        VERSION.fetch_add(1, Ordering::Relaxed);
        let size = crate::apps::transfer::size_text(sectors as u64 * SECTOR as u64);
        crate::kprintln!("fs: system partition on SATA disk {model}, {size}");
        return Some(alloc::format!("Disk: {model}, {size} FAT32"));
    }
    None
}

/// A new FAT32 volume labelled `label` on a disk partition (`dev` is Sata or Usb) with
/// files `files` (paths from the volume root): for installing the system and for "Disks".
/// Then reads everything back and verifies it. `progress` is 0..=100.
pub fn create_volume(dev: Device, label: &str, files: &[(String, Vec<u8>)], progress: &mut dyn FnMut(u32)) -> core::result::Result<(), &'static str> {
    let (start, sectors) = (dev.start(), dev.sectors());
    let mut raw = dev.twin().ok_or("not a disk partition")?;
    // Signatures of the previous file system at the start of the partition.
    let wipe = 2048u32.min(sectors) as usize;
    if !raw.write(0, &vec![0u8; wipe * SECTOR]) {
        return Err("disk write failed");
    }
    let id = free_dev();
    set_dev(id, Some(dev));
    let flush = |fat: &mut Fat| {
        fat.set_writing(true);
        let writes = fat.take_dirty();
        let ok = writes.is_empty() || write_out(id, &writes);
        fat.set_writing(false);
        fat.trim();
        ok
    };
    let result = (|| {
        let mut fat = Fat::format_labeled(sectors, id, label);
        if !flush(&mut fat) {
            return Err("disk write failed");
        }
        let total = files.iter().map(|f| f.1.len()).sum::<usize>().max(1);
        let mut done = 0;
        for (path, data) in files {
            for (i, _) in path.match_indices('/').skip(1) {
                if Filesystem::is_dir(&mut fat, &path[..i]).is_none() {
                    Filesystem::mkdir(&mut fat, &path[..i]).map_err(|_| "cannot create a folder")?;
                }
            }
            Filesystem::write(&mut fat, path, data).map_err(|_| "cannot write a file")?;
            if !flush(&mut fat) {
                return Err("disk write failed");
            }
            done += data.len();
            progress((done * 90 / total) as u32);
        }
        // Where the volume starts on the disk (the "hidden sectors" field) — in the boot
        // sector and its copy; Windows and some firmwares look at it.
        for lba in [0, 6] {
            let mut boot = [0u8; SECTOR];
            if !raw.read(lba, &mut boot) {
                return Err("the new volume does not read back");
            }
            if boot[510..512] == [0x55, 0xAA] {
                boot[28..32].copy_from_slice(&start.to_le_bytes());
                if !raw.write(lba, &boot) {
                    return Err("disk write failed");
                }
            }
        }
        // Verification: the volume again from disk, every file as written.
        let mut back = Fat::mount(&raw, id).ok_or("the new volume does not read back")?;
        for (path, data) in files {
            if Filesystem::read(&mut back, path).ok().as_deref() != Some(&data[..]) {
                return Err("a file reads back different");
            }
        }
        progress(100);
        Ok(())
    })();
    set_dev(id, None);
    result
}

/// Where a disk partition is mounted: "/" (the main volume), "/media/disk0" or None.
pub fn partition_path(drive: crate::parts::Drive, start: u32) -> Option<String> {
    let id = DEVS.lock().iter().position(|d| d.as_ref().is_some_and(|d| d.is(drive, start)))?;
    if id == 0 {
        return Some("/".into());
    }
    MOUNTS.lock().list.iter().find(|m| m.dev == id).map(|m| alloc::format!("{MEDIA}/{}", m.name))
}

/// Mounts a disk partition (FAT32, FAT16, exFAT) in /media: "disk0" for SATA,
/// "usb0" for USB. Returns the path.
pub fn mount_partition(drive: crate::parts::Drive, start: u32, sectors: u32) -> core::result::Result<String, &'static str> {
    use crate::parts::Drive;
    if let Some(path) = partition_path(drive, start) {
        return Ok(path);
    }
    let dev = drive.device(start, sectors);
    let id = free_dev();
    let fat: alloc::boxed::Box<dyn Filesystem> = match Fat::mount(&dev, id) {
        Some(f) => alloc::boxed::Box::new(f),
        None => match exfat::Exfat::mount(&dev, id) {
            Some(x) => alloc::boxed::Box::new(x),
            None => return Err("not FAT32, FAT16 or exFAT"),
        },
    };
    set_dev(id, Some(dev));
    let (prefix, source) = match drive {
        Drive::Sata(d) => ("disk", Source::Sata(d, start)),
        Drive::Usb(d) => ("usb", Source::Usb(d)),
    };
    let mut m = MOUNTS.lock();
    let name = (0..).map(|n| alloc::format!("{prefix}{n}")).find(|n| !m.list.iter().any(|x| x.name == *n)).unwrap();
    crate::kprintln!("fs: {drive:?} partition at {start}: {} \"{}\" at {MEDIA}/{name}", fat.kind(), fat.label());
    m.list.push(Mount::new(name.clone(), fat, source));
    drop(m);
    VERSION.fetch_add(1, Ordering::Relaxed);
    Ok(alloc::format!("{MEDIA}/{name}"))
}

/// Data partitions on SATA disks (FAT32, FAT16, exFAT) go to /media, like USB sticks.
/// EFI system partitions (ours and other loaders) and the partition we
/// booted from are not mounted.
pub fn mount_sata_data() {
    for (disk, total, model, _) in crate::ahci::disks() {
        let drive = crate::parts::Drive::Sata(disk);
        let (_, Some(table)) = crate::parts::examine(drive, total) else { continue };
        let boot = boot_partition_start(disk);
        for p in table.parts.iter().filter(|p| !p.esp && !p.extended && matches!(p.fs, "FAT32" | "FAT16" | "exFAT") && Some(p.start as u32) != boot) {
            if p.start + p.sectors > u32::MAX as u64 {
                continue;
            }
            if let Err(e) = mount_partition(drive, p.start as u32, p.sectors as u32) {
                crate::kprintln!("fs: {model} partition {}: {e}", p.slot + 1);
            }
        }
    }
}

/// No disk: a FAT32 volume in memory (the files will not survive a reboot).
pub fn mount_ram() {
    // Three quarters of memory: without a disk files have nowhere else to live. This is a ceiling, not
    // memory in use: the RamDisk takes it as data is written.
    let bytes = (crate::heap_stats().1 / 4 * 3).max(RAM_DISK_MIN);
    let sectors = bytes / SECTOR;
    set_dev(0, Some(Device::Ram(ram::RamDisk::new(sectors))));
    *FS.lock() = Some(Fat::format(sectors as u32, 0));
    VERSION.fetch_add(1, Ordering::Relaxed);
}

const RAM_DISK_MIN: usize = 32 << 20;

/// The SATA disk number holding the main volume (an installed system).
pub fn system_sata_disk() -> Option<u32> {
    match DEVS.lock().first() {
        Some(Some(Device::Sata { disk, .. })) => Some(*disk),
        _ => None,
    }
}

/// The USB disk number holding the main volume (if it is on a stick).
fn system_disk() -> Option<u32> {
    match DEVS.lock().first() {
        Some(Some(Device::Usb { disk, .. })) => Some(*disk),
        _ => None,
    }
}

/// Reconciles /media with the connected USB disks: mounts new ones, removes pulled ones.
/// Called when the USB disk list changes.
pub fn refresh_usb() {
    // The first pass — sticks inserted before boot (including the one we
    // booted from): mount them silently, without notifications or sound.
    static BOOTED: AtomicBool = AtomicBool::new(false);
    let quiet = !BOOTED.swap(true, Ordering::Relaxed);
    // We play the stick sound ourselves: one for everything that happened to disks in this pass.
    let notify = |title: &str, body: &str| {
        if !quiet {
            crate::wm::notify_with(None, title, body);
        }
    };
    let disks = crate::usb::disks();
    static DISKS: AtomicUsize = AtomicUsize::new(0);
    let disks_now = disks.len();
    let before = DISKS.swap(disks_now, Ordering::Relaxed);
    let mut lost_data = false;
    // Pulled without "eject": unsaved data is lost.
    let gone: Vec<Mount> = {
        let mut m = MOUNTS.lock();
        m.tried.retain(|d| disks.iter().any(|(id, _)| id == d));
        let (keep, gone) = core::mem::take(&mut m.list).into_iter().partition(|x| match x.source {
            Source::Usb(d) => disks.iter().any(|(id, _)| *id == d),
            _ => true,
        });
        m.list = keep;
        gone
    };
    for m in gone {
        let fat = m.fat.lock();
        set_dev(fat.dev(), None);
        let lost = if fat.has_changes() { " before all changes were saved" } else { "" };
        lost_data |= fat.has_changes();
        crate::kprintln!("fs: /media/{} removed{lost}", m.name);
        crate::wm::notify_with(None, "USB drive removed", &alloc::format!("{}{lost}", label_or(fat.label(), &m.name)));
        VERSION.fetch_add(1, Ordering::Relaxed);
    }
    for (disk, blocks) in disks {
        if Some(disk) == system_disk() || MOUNTS.lock().tried.contains(&disk) {
            continue;
        }
        MOUNTS.lock().tried.push(disk);
        let read = |lba: u32, buf: &mut [u8]| crate::usb::disk_read(disk, lba, buf);
        let mut mounted = 0;
        for (start, sectors) in fat_partitions(&read, blocks) {
            let dev = Device::Usb { disk, start, sectors };
            let id = free_dev();
            let fat: alloc::boxed::Box<dyn Filesystem> = match Fat::mount(&dev, id) {
                Some(f) => alloc::boxed::Box::new(f),
                None => match exfat::Exfat::mount(&dev, id) {
                    Some(x) => alloc::boxed::Box::new(x),
                    None => continue,
                },
            };
            set_dev(id, Some(dev));
            let mut m = MOUNTS.lock();
            let name = (0..).map(|n| alloc::format!("usb{n}")).find(|n| !m.list.iter().any(|x| x.name == *n)).unwrap();
            crate::kprintln!("fs: USB disk {disk}: {} \"{}\" at {MEDIA}/{name}", fat.kind(), fat.label());
            notify("USB drive connected", &alloc::format!("{} \u{2014} {MEDIA}/{name}", label_or(fat.label(), &name)));
            m.list.push(Mount::new(name, fat, Source::Usb(disk)));
            mounted += 1;
        }
        if mounted == 0 {
            // An ISO written to a stick (most likely the one we booted from) — no complaint.
            let mut pvd = [0u8; SECTOR];
            if read(64, &mut pvd) && &pvd[1..6] == b"CD001" {
                crate::kprintln!("fs: USB disk {disk}: ISO 9660 image (boot media), not mounted");
            } else {
                crate::kprintln!("fs: USB disk {disk}: no FAT32/FAT16/exFAT partition");
                notify("USB drive not readable", "Supported: FAT32, FAT16 and exFAT (not NTFS)");
            }
        }
        VERSION.fetch_add(1, Ordering::Relaxed);
    }
    if !quiet {
        use crate::audio::{self, Sound};
        match disks_now.cmp(&before) {
            _ if lost_data => audio::play(Sound::Critical),
            core::cmp::Ordering::Greater => audio::play(Sound::DriveIn),
            core::cmp::Ordering::Less => audio::play(Sound::DriveOut),
            core::cmp::Ordering::Equal => {}
        }
    }
}

/// `refresh_usb` in a separate thread. false — the previous one is still running.
pub fn refresh_usb_in_background() -> bool {
    static BUSY: AtomicBool = AtomicBool::new(false);
    if BUSY.swap(true, Ordering::Acquire) {
        return false;
    }
    crate::task::spawn("usb-mount", None, || {
        refresh_usb();
        BUSY.store(false, Ordering::Release);
    });
    true
}

fn label_or(label: &str, name: &str) -> String {
    if label.is_empty() || label == "NO NAME" { String::from(name) } else { String::from(label) }
}

/// Mounts a network folder at /media/`name` (if the name is taken, a number is added).
/// Returns the path.
pub fn mount_network(name: &str, fs: alloc::boxed::Box<dyn Filesystem>) -> String {
    let mut m = MOUNTS.lock();
    let mut unique = String::from(name);
    let mut n = 2;
    while m.list.iter().any(|x| x.name == unique) {
        unique = alloc::format!("{name}{n}");
        n += 1;
    }
    m.list.push(Mount::new(unique.clone(), fs, Source::Network));
    drop(m);
    VERSION.fetch_add(1, Ordering::Relaxed);
    alloc::format!("{MEDIA}/{unique}")
}

/// Safe removal: save everything to the stick and detach it from /media.
pub fn eject(path: &str) -> core::result::Result<(), &'static str> {
    let name = media_path(path).ok_or("not a removable drive")?.0;
    while SYNCING.swap(true, Ordering::Acquire) {
        crate::task::yield_now();
    }
    let r = sync_mounts();
    SYNCING.store(false, Ordering::Release);
    r?;
    let mut mounts = MOUNTS.lock();
    let i = mounts.list.iter().position(|m| m.name == name).ok_or("not mounted")?;
    let m = mounts.list.remove(i);
    drop(mounts);
    set_dev(m.fat.lock().dev(), None);
    if m.source == Source::Network {
        crate::net::smb::forget(path);
    }
    crate::kprintln!("fs: {MEDIA}/{name} ejected");
    VERSION.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Volumes for the file manager sidebar.
pub struct Volume {
    pub name: String,
    /// Where to go: "/" or "/media/usb0".
    pub path: String,
    pub removable: bool,
    /// The main volume is in memory (files are not saved).
    pub in_memory: bool,
    /// A network folder (SMB).
    pub network: bool,
    /// A network folder that cannot be reached right now.
    pub offline: bool,
    /// The file system: "FAT32", "FAT16", "exFAT".
    pub kind: &'static str,
    pub used: u64,
    pub total: u64,
}

pub fn volumes() -> Vec<Volume> {
    let mut out = Vec::new();
    if let Some(f) = FS.lock().as_ref() {
        let (used, total) = f.usage();
        let in_memory = !has_disk();
        let name = if in_memory { "System (RAM)" } else { "System" };
        let kind = Filesystem::kind(f);
        out.push(Volume { name: name.into(), path: "/".into(), removable: false, in_memory, network: false, offline: false, kind, used, total });
    }
    for m in &mut MOUNTS.lock().list {
        if let Some(fat) = m.fat.try_lock() {
            let (used, total) = fat.usage();
            m.seen = (String::from(fat.label()), fat.kind(), used, total, fat.online());
        }
        let (label, kind, used, total, online) = &m.seen;
        let network = m.source == Source::Network;
        out.push(Volume {
            name: label_or(label, &m.name),
            path: alloc::format!("{MEDIA}/{}", m.name),
            removable: !matches!(m.source, Source::Sata(..)),
            in_memory: false,
            network,
            offline: !online,
            kind,
            used: *used,
            total: *total,
        });
    }
    out
}

/// Writes sectors to device `dev` (consecutive ones in one command).
fn write_out(dev: usize, writes: &[(u32, alloc::boxed::Box<[u8; SECTOR]>)]) -> bool {
    let mut devs = DEVS.lock();
    let Some(Some(dev)) = devs.get_mut(dev) else { return false };
    let mut ok = true;
    let mut i = 0;
    while i < writes.len() {
        let mut j = i + 1;
        while j < writes.len() && writes[j].0 == writes[j - 1].0 + 1 && j - i < 255 {
            j += 1;
        }
        let mut buf = Vec::with_capacity((j - i) * SECTOR);
        for (_, s) in &writes[i..j] {
            buf.extend_from_slice(&s[..]);
        }
        ok &= dev.write(writes[i].0, &buf);
        i = j;
    }
    ok
}

/// Writes the accumulated changes of the main volume and the sticks. An in-memory volume goes here too:
/// otherwise its files would stay in the sector cache, one block per sector.
/// Returns the number of sectors written.
pub fn sync() -> core::result::Result<usize, &'static str> {
    let mut n = 0;
    if has_volume() {
        let writes = {
            let mut fs = FS.lock();
            let fat = fs.as_mut().ok_or("no filesystem")?;
            fat.writing = true;
            fat.take_dirty()
        };
        // Write without the volume lock: files can be read and changed while writing.
        let ok = writes.is_empty() || write_out(0, &writes);
        if let Some(fat) = FS.lock().as_mut() {
            fat.writing = false;
            fat.trim();
        }
        if !ok {
            return Err("disk write failed");
        }
        if !writes.is_empty() {
            ROOT_EPOCH.fetch_add(1, Ordering::Relaxed);
        }
        n += writes.len();
    }
    Ok(n + sync_mounts()?)
}

/// Saves changes on the USB sticks.
fn sync_mounts() -> core::result::Result<usize, &'static str> {
    let mut n = 0;
    // Network folders have no write cache: everything is already on the server.
    let volumes: Vec<Volumeh> = MOUNTS.lock().list.iter().filter(|m| m.source != Source::Network).map(|m| m.fat.clone()).collect();
    for volume in volumes {
        let (dev, writes) = {
            let mut fat = volume.lock();
            fat.set_writing(true);
            (fat.dev(), fat.take_dirty())
        };
        let ok = writes.is_empty() || write_out(dev, &writes);
        {
            let mut fat = volume.lock();
            fat.set_writing(false);
            fat.trim();
        }
        if !ok {
            return Err("USB drive write failed");
        }
        n += writes.len();
    }
    Ok(n)
}

/// Whether there is anything to save to disks (the main volume on a disk, or sticks mounted).
pub fn needs_saving() -> bool {
    has_volume() || !MOUNTS.lock().list.is_empty()
}

fn has_volume() -> bool {
    FS.lock().is_some()
}

pub fn has_disk() -> bool {
    ON_DISK.load(Ordering::Relaxed)
}

static READY: AtomicBool = AtomicBool::new(false);

/// System files are mounted and restored — from disk or (without a disk) in memory from
/// the stick. Before that it is too early to read settings: they do not exist yet.
pub fn ready() -> bool {
    READY.load(Ordering::Relaxed)
}

pub fn set_ready() {
    READY.store(true, Ordering::Relaxed);
}

/// Saves changes to disk in a background thread so the UI does not wait for the disk.
/// false — the previous save has not finished yet.
pub fn sync_in_background() -> bool {
    if SYNCING.swap(true, Ordering::Acquire) {
        return false;
    }
    crate::task::spawn("fs-sync", None, || {
        match sync() {
            Ok(0) => {}
            Ok(n) => crate::kprintln!("fs: saved ({n} sectors written)"),
            Err(e) => crate::kprintln!("fs: save failed: {e}"),
        }
        SYNCING.store(false, Ordering::Release);
    });
    true
}

/// Used and total on a volume (bytes).
pub fn disk_usage() -> Option<(u64, u64)> {
    FS.lock().as_ref().map(|f| f.usage())
}

/// Used and total on the volume holding `path`.
pub fn usage_of(path: &str) -> Option<(u64, u64)> {
    if crate::net::discovery::depth(path).is_some() {
        return None;
    }
    with_fs(path, |f, _| Ok(f.usage())).ok()
}

pub const README: &str = "Welcome to GlassOS!

Files are saved to the disk automatically and survive a restart.

Programs are Lua scripts (*.lua). Run them by double-clicking in Files,
or type the name in the terminal: try `widgets` or `sysinfo`.
Programs from /apps are found from any directory.

Native programs live in /bin and run in user mode (ring 3), each in its
own address space: try `ring3`, `count 5`, `cat readme.txt` or `crash`.
";

pub const OS_VERSION: &str = "GlassOS 0.2 x86_64 (UEFI)\n";

/// The initial contents for an empty disk.
pub fn create_defaults() {
    for dir in ["/home", "/system", "/tmp"] {
        let _ = mkdir(dir);
    }
    let _ = write("/system/version", OS_VERSION.as_bytes());
}
