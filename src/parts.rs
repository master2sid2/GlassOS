//! Partition tables (GPT and MBR) on SATA and USB disks: read them, find free
//! space, add, replace and delete a partition, create a new table. Used by the
//! installer (`install`) and the "Disks" window.
//!
//! GPT is written properly: entries and header first to the backup at the end of the disk,
//! then to the primary, with checksums. Partitions are aligned to 1 MiB.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

pub const SECTOR: usize = 512;
/// Partition alignment — 1 MiB.
pub const ALIGN: u64 = 2048;
/// Free space smaller than this is not shown.
pub const MIN_FREE: u64 = (64u64 << 20) / SECTOR as u64;
/// Entries in a new GPT and their size — the usual ones (32 sectors).
const ENTRIES: usize = 128;
const ENTRY: usize = 128;
/// The "EFI system partition" type C12A7328-F81F-11D2-BA4B-00A0C93EC93B, as on disk.
pub const ESP_TYPE: [u8; 16] = [0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B];
/// The "basic data" type EBD0A0A2-B9E5-4433-87C0-68B6B72699C7 (seen by Windows, macOS and Linux).
pub const DATA_TYPE: [u8; 16] = [0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7];
/// Partition name (and volume label) of the installed system.
pub const GLASSOS: &str = "GlassOS";

/// A disk: SATA (`ahci::disks`) or USB (`usb::disks`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Drive {
    Sata(u32),
    Usb(u32),
}

impl Drive {
    pub fn read(self, lba: u64, buf: &mut [u8]) -> bool {
        match self {
            Drive::Sata(d) => crate::ahci::disk_read(d, lba, buf),
            Drive::Usb(d) => lba <= u32::MAX as u64 && crate::usb::disk_read(d, lba as u32, buf),
        }
    }

    /// Write and flush the disk cache.
    pub fn write(self, lba: u64, data: &[u8]) -> bool {
        match self {
            Drive::Sata(d) => crate::ahci::disk_write(d, lba, data),
            Drive::Usb(d) => lba <= u32::MAX as u64 && crate::usb::disk_write(d, lba as u32, data),
        }
    }

    /// A disk partition as a volume device.
    pub fn device(self, start: u32, sectors: u32) -> crate::fs::Device {
        match self {
            Drive::Sata(disk) => crate::fs::Device::Sata { disk, start, sectors },
            Drive::Usb(disk) => crate::fs::Device::Usb { disk, start, sectors },
        }
    }

    fn at(self, lba: u64, n: usize) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; n * SECTOR];
        self.read(lba, &mut buf).then_some(buf)
    }

    fn put(self, lba: u64, data: &[u8]) -> Result<(), String> {
        if self.write(lba, data) { Ok(()) } else { Err(format!("cannot write sector {lba}")) }
    }
}

/// A partition on a disk.
#[derive(Clone)]
pub struct Part {
    /// Entry number in the table (from 0).
    pub slot: usize,
    pub start: u64,
    pub sectors: u64,
    /// Name (GPT) or partition kind.
    pub name: String,
    /// File system inside: "NTFS", "FAT32", "ext4"… or empty.
    pub fs: &'static str,
    /// Volume label (FAT and exFAT), if any.
    pub label: String,
    /// EFI system partition (holds system boot loaders).
    pub esp: bool,
    /// MBR extended partition (a container of logical ones).
    pub extended: bool,
    /// Partition GUID (GPT) or the disk signature in the first 4 bytes (MBR) — for the firmware.
    pub signature: [u8; 16],
}

impl Part {
    pub fn size(&self) -> String {
        size_text(self.sectors)
    }

    pub fn end(&self) -> u64 {
        self.start + self.sectors
    }

    /// «Basic data partition · NTFS · 100 GB».
    pub fn describe(&self) -> String {
        let mut parts = vec![self.name.clone()];
        if !self.fs.is_empty() && !self.name.contains(self.fs) {
            parts.push(self.fs.into());
        }
        parts.push(self.size());
        parts.join(" \u{b7} ")
    }

    /// The GlassOS partition.
    pub fn glassos(&self) -> bool {
        self.name == GLASSOS
    }
}

/// A disk's partition table and everything needed to modify it.
#[derive(Clone)]
pub struct Table {
    pub gpt: bool,
    pub parts: Vec<Part>,
    /// Free gaps (first sector, sector count) no smaller than `MIN_FREE`.
    pub free: Vec<(u64, u64)>,
    /// Whether there is an empty entry for a new partition.
    pub slot_free: bool,
    /// Sector 1 (GPT header) or sector 0 (MBR).
    head: Vec<u8>,
    /// All GPT entries.
    entries: Vec<u8>,
    entry_size: usize,
}

impl Table {
    /// How the firmware will find an existing partition.
    pub fn placed(&self, p: &Part) -> Placed {
        Placed { number: p.slot as u32 + 1, start: p.start, sectors: p.sectors, signature: p.signature, gpt: self.gpt }
    }
}

/// Where the system partition is and how the firmware will find it (the "hard drive" node).
pub struct Placed {
    /// Partition number in the table (from 1).
    pub number: u32,
    pub start: u64,
    pub sectors: u64,
    /// Partition GUID (GPT) or the disk signature in the first 4 bytes (MBR).
    pub signature: [u8; 16],
    pub gpt: bool,
}

/// Kind of a new partition.
#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    /// EFI system partition (GlassOS).
    Esp,
    /// Data (FAT32).
    Data,
}

pub fn size_text(sectors: u64) -> String {
    crate::apps::transfer::size_text(sectors * SECTOR as u64)
}

fn le(b: &[u8], at: usize, n: usize) -> u64 {
    b[at..at + n].iter().rev().fold(0u64, |v, &x| v << 8 | x as u64)
}

fn put(b: &mut [u8], at: usize, v: u64, n: usize) {
    b[at..at + n].copy_from_slice(&v.to_le_bytes()[..n]);
}

/// What is on the disk and its partition table (if understood): ("GPT, 3 partitions", table).
pub fn examine(drive: Drive, total: u64) -> (String, Option<Table>) {
    let Some(head) = drive.at(0, 256) else { return ("not readable".into(), None) };
    if head.iter().all(|&b| b == 0) {
        return ("empty".into(), None);
    }
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let table = match read_gpt(drive, total) {
        Some(t) => Some(t),
        None if &head[SECTOR..SECTOR + 8] == b"EFI PART" => return ("a damaged GPT".into(), None),
        None => read_mbr(drive, total, &head[..SECTOR]),
    };
    if let Some(t) = table {
        let n = t.parts.len();
        let text = if t.parts.iter().any(|p| p.glassos()) {
            if n == 1 { GLASSOS.into() } else { format!("GlassOS and {} other partition{}", n - 1, plural(n - 1)) }
        } else if n == 0 {
            format!("{}, no partitions", if t.gpt { "GPT" } else { "MBR" })
        } else {
            format!("{}, {n} partition{}", if t.gpt { "GPT" } else { "MBR" }, plural(n))
        };
        return (text, Some(t));
    }
    let fs = filesystem(&head[..4096]);
    if !fs.is_empty() {
        return (format!("{fs} without partitions"), None);
    }
    ("unknown data".into(), None)
}

/// File system from the first 4 KiB of a partition.
pub fn filesystem(b: &[u8]) -> &'static str {
    if b.len() < 4096 {
        return "";
    }
    match () {
        _ if &b[3..11] == b"NTFS    " => "NTFS",
        _ if &b[3..11] == b"EXFAT   " => "exFAT",
        _ if &b[3..11] == b"-FVE-FS-" => "BitLocker",
        _ if &b[82..90] == b"FAT32   " => "FAT32",
        _ if &b[54..59] == b"FAT16" || &b[54..59] == b"FAT12" => "FAT16",
        _ if b[1024 + 56..1024 + 58] == [0x53, 0xEF] => "ext4",
        _ if &b[..4] == b"LUKS" => "LUKS",
        _ if &b[4086..4096] == b"SWAPSPACE2" => "swap",
        _ => "",
    }
}

/// File system and volume label of a partition.
fn probe(drive: Drive, start: u64) -> (&'static str, String) {
    let Some(b) = drive.at(start, 8) else { return ("", String::new()) };
    let fs = filesystem(&b);
    let label = match fs {
        "FAT32" => String::from_utf8_lossy(&b[71..82]).trim().into(),
        "FAT16" => String::from_utf8_lossy(&b[43..54]).trim().into(),
        _ => String::new(),
    };
    let label = if label == "NO NAME" { String::new() } else { label };
    (fs, label)
}

/// Partition name from a GPT entry (UTF-16 from byte 56).
fn name_of(e: &[u8]) -> String {
    let units: Vec<u16> = e[56..128].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
    String::from_utf16_lossy(&units)
}

/// GPT partition kind by its type (if there is no name).
fn gpt_kind(t: &[u8]) -> &'static str {
    const KINDS: [([u8; 4], &str); 7] = [
        ([0x28, 0x73, 0x2A, 0xC1], "EFI system partition"),
        ([0x16, 0xE3, 0xC9, 0xE3], "Microsoft reserved"),
        ([0xA2, 0xA0, 0xD0, 0xEB], "Basic data"),
        ([0xA4, 0xBB, 0x94, 0xDE], "Windows recovery"),
        ([0xAF, 0x3D, 0xC6, 0x0F], "Linux"),
        ([0x6D, 0xFD, 0x57, 0x06], "Linux swap"),
        ([0x48, 0x61, 0x68, 0x21], "BIOS boot"),
    ];
    KINDS.iter().find(|(k, _)| t[..4] == *k).map_or("partition", |(_, n)| n)
}

/// Free gaps between partitions (aligned) from `first` to `last`.
fn gaps(parts: &[Part], first: u64, last: u64) -> Vec<(u64, u64)> {
    let mut used: Vec<(u64, u64)> = parts.iter().map(|p| (p.start, p.end() - 1)).collect();
    used.sort();
    let mut out = Vec::new();
    let mut at = first.next_multiple_of(ALIGN);
    for (s, e) in used.into_iter().chain([(last + 1, last + 1)]) {
        if s > at && s - at >= MIN_FREE {
            out.push((at, s - at));
        }
        at = at.max((e + 1).next_multiple_of(ALIGN));
    }
    out
}

fn read_gpt(drive: Drive, total: u64) -> Option<Table> {
    let head = drive.at(1, 1)?;
    let hsize = le(&head, 12, 4) as usize;
    if &head[..8] != b"EFI PART" || !(92..=SECTOR).contains(&hsize) || le(&head, 24, 8) != 1 {
        return None;
    }
    let mut check = head[..hsize].to_vec();
    check[16..20].fill(0);
    if crate::png::crc32(&check) as u64 != le(&head, 16, 4) {
        return None;
    }
    let (first, last) = (le(&head, 40, 8), le(&head, 48, 8));
    let (at, count, size) = (le(&head, 72, 8), le(&head, 80, 4) as usize, le(&head, 84, 4) as usize);
    if size < ENTRY || size % 8 != 0 || count == 0 || count > 1024 || last >= total || first > last {
        return None;
    }
    let raw = drive.at(at, (count * size).div_ceil(SECTOR))?;
    let entries = raw[..count * size].to_vec();
    if crate::png::crc32(&entries) as u64 != le(&head, 88, 4) {
        return None;
    }
    let mut parts = Vec::new();
    let mut slot_free = false;
    for (slot, e) in entries.chunks(size).enumerate() {
        if e[..16] == [0; 16] {
            slot_free = true;
            continue;
        }
        let (start, end) = (le(e, 32, 8), le(e, 40, 8));
        if end < start || end > last {
            continue;
        }
        let name = name_of(e);
        let (fs, label) = probe(drive, start);
        let name = if name.is_empty() { gpt_kind(e).into() } else { name };
        let signature = e[16..32].try_into().unwrap();
        parts.push(Part { slot, start, sectors: end - start + 1, name, fs, label, esp: e[..16] == ESP_TYPE, extended: false, signature });
    }
    let free = gaps(&parts, first, last);
    Some(Table { gpt: true, parts, free, slot_free, head, entries, entry_size: size })
}

/// MBR partition kind by type.
fn mbr_kind(t: u8) -> &'static str {
    match t {
        0x07 => "NTFS or exFAT",
        0x0B | 0x0C => "FAT32",
        0x04 | 0x06 | 0x0E => "FAT16",
        0x05 | 0x0F | 0x85 => "extended",
        0x27 => "Windows recovery",
        0x82 => "Linux swap",
        0x83 => "Linux",
        0xEF => "EFI system partition",
        0x12 => "vendor recovery",
        0x17 => "hidden NTFS",
        _ => "partition",
    }
}

fn read_mbr(drive: Drive, total: u64, s0: &[u8]) -> Option<Table> {
    if s0[510..512] != [0x55, 0xAA] {
        return None;
    }
    // A volume boot sector without a table (a "superfloppy" stick) is not an MBR.
    if &s0[3..11] == b"NTFS    " || &s0[3..11] == b"EXFAT   " || &s0[82..87] == b"FAT32" || &s0[54..59] == b"FAT16" {
        return None;
    }
    let mut parts = Vec::new();
    let mut slot_free = false;
    for (slot, e) in s0[446..510].chunks(16).enumerate() {
        let (ty, start, sectors) = (e[4], le(e, 8, 4), le(e, 12, 4));
        if ty == 0 || sectors == 0 {
            slot_free = true;
            continue;
        }
        if ty == 0xEE || start + sectors > total || e[0] & 0x7F != 0 {
            return None; // protective MBR without GPT, or garbage
        }
        let extended = matches!(ty, 0x05 | 0x0F | 0x85);
        let (fs, label) = if extended { ("", String::new()) } else { probe(drive, start) };
        // MBR has no names: recognise the GlassOS partition by its volume label.
        let name = if ty == 0xEF && label == "GLASSOS" { GLASSOS.into() } else { mbr_kind(ty).into() };
        let mut signature = [0u8; 16];
        signature[..4].copy_from_slice(&s0[440..444]);
        parts.push(Part { slot, start, sectors, name, fs, label, esp: ty == 0xEF, extended, signature });
    }
    let last = (total - 1).min(u32::MAX as u64 - 1);
    let free = gaps(&parts, 1, last);
    Some(Table { gpt: false, parts, free, slot_free, head: s0.to_vec(), entries: Vec::new(), entry_size: 0 })
}

/// A random GUID (version 4) in on-disk byte order.
fn guid() -> [u8; 16] {
    let mut g = [0u8; 16];
    g.copy_from_slice(&crate::crypto::random32()[..16]);
    g[7] = g[7] & 0x0F | 0x40;
    g[8] = g[8] & 0x3F | 0x80;
    g
}

/// GPT entry of a new partition; returns its GUID.
fn gpt_entry(e: &mut [u8], start: u64, sectors: u64, kind: Kind, name: &str) -> [u8; 16] {
    e.fill(0);
    e[0..16].copy_from_slice(if kind == Kind::Esp { &ESP_TYPE } else { &DATA_TYPE });
    let unique = guid();
    e[16..32].copy_from_slice(&unique);
    put(e, 32, start, 8);
    put(e, 40, start + sectors - 1, 8);
    for (i, u) in name.encode_utf16().take(36).enumerate() {
        put(e, 56 + i * 2, u as u64, 2);
    }
    unique
}

/// Recomputes the GPT header checksum.
fn seal(h: &mut [u8]) {
    let hsize = le(h, 12, 4) as usize;
    h[16..20].fill(0);
    let crc = crate::png::crc32(&h[..hsize]);
    put(h, 16, crc as u64, 4);
}

/// Writes the GPT entries and both headers: the backup at the end of the disk first, then the primary.
fn write_gpt(drive: Drive, head: &[u8], entries: &[u8]) -> Result<(), String> {
    let crc = crate::png::crc32(entries) as u64;
    let mut h = head.to_vec();
    put(&mut h, 88, crc, 4);
    seal(&mut h);
    let alt = le(&h, 32, 8);
    let entry_sectors = entries.len().div_ceil(SECTOR) as u64;
    let mut b = drive.at(alt, 1).filter(|b| &b[..8] == b"EFI PART" && le(b, 24, 8) == alt).unwrap_or_else(|| {
        let mut b = h.clone();
        put(&mut b, 24, alt, 8);
        put(&mut b, 32, 1, 8);
        put(&mut b, 72, alt - entry_sectors, 8);
        b
    });
    put(&mut b, 88, crc, 4);
    seal(&mut b);
    let mut padded = entries.to_vec();
    padded.resize(entry_sectors as usize * SECTOR, 0);
    drive.put(le(&b, 72, 8), &padded)?;
    drive.put(alt, &b)?;
    drive.put(le(&h, 72, 8), &padded)?;
    drive.put(1, &h)
}

/// A new empty GPT over the whole disk (everything there is lost) — and the same, read back.
pub fn create_table(drive: Drive, total: u64) -> Result<Table, String> {
    let entry_sectors = (ENTRIES * ENTRY / SECTOR) as u64;
    let last_usable = total - 2 - entry_sectors;
    let mut h = vec![0u8; SECTOR];
    h[0..8].copy_from_slice(b"EFI PART");
    put(&mut h, 8, 0x0001_0000, 4);
    put(&mut h, 12, 92, 4);
    put(&mut h, 24, 1, 8);
    put(&mut h, 32, total - 1, 8);
    put(&mut h, 40, 2 + entry_sectors, 8);
    put(&mut h, 48, last_usable, 8);
    h[56..72].copy_from_slice(&guid());
    put(&mut h, 72, 2, 8);
    put(&mut h, 80, ENTRIES as u64, 4);
    put(&mut h, 84, ENTRY as u64, 4);

    let mut mbr = vec![0u8; SECTOR];
    mbr[446 + 1..446 + 4].copy_from_slice(&[0x00, 0x02, 0x00]);
    mbr[446 + 4] = 0xEE;
    mbr[446 + 5..446 + 8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    put(&mut mbr, 446 + 8, 1, 4);
    put(&mut mbr, 446 + 12, (total - 1).min(u32::MAX as u64), 4);
    mbr[510..512].copy_from_slice(&[0x55, 0xAA]);

    // Wipe the old first: the start of the disk (tables, boot loaders) and the end (GPT backup).
    drive.put(0, &vec![0u8; ALIGN as usize * SECTOR])?;
    drive.put(total - 1 - entry_sectors, &vec![0u8; (entry_sectors as usize + 1) * SECTOR])?;
    write_gpt(drive, &h, &vec![0u8; ENTRIES * ENTRY])?;
    drive.put(0, &mbr)?;
    crate::kprintln!("parts: {drive:?}: new GPT");
    read_gpt(drive, total).ok_or_else(|| String::from("the new partition table does not read back"))
}

/// A new partition in free space (start `start`, length `sectors`).
pub fn add(drive: Drive, table: &Table, start: u64, sectors: u64, kind: Kind, name: &str) -> Result<Placed, String> {
    if !table.free.iter().any(|&(s, n)| start >= s && start + sectors <= s + n) {
        return Err("there is no such free space".into());
    }
    if table.gpt {
        let slot = table.entries.chunks(table.entry_size).position(|e| e[..16] == [0; 16]).ok_or("the partition table is full")?;
        write_slot(drive, table, slot, start, sectors, kind, name)
    } else {
        let slot = table.head[446..510].chunks(16).position(|e| e[4] == 0 || le(e, 12, 4) == 0).ok_or("all four MBR entries are taken")?;
        if start + sectors > u32::MAX as u64 {
            return Err("MBR cannot address past 2 TB".into());
        }
        write_slot(drive, table, slot, start, sectors, kind, name)
    }
}

/// Partition `slot` anew: same start, length `sectors` (no more than before).
pub fn replace(drive: Drive, table: &Table, slot: usize, sectors: u64, kind: Kind, name: &str) -> Result<Placed, String> {
    let p = table.parts.iter().find(|p| p.slot == slot && !p.extended).ok_or("no such partition")?;
    write_slot(drive, table, slot, p.start, sectors.min(p.sectors), kind, name)
}

/// Deletes partition `slot` from the table (the data stays on disk until overwritten).
pub fn delete(drive: Drive, table: &Table, slot: usize) -> Result<(), String> {
    if !table.parts.iter().any(|p| p.slot == slot) {
        return Err("no such partition".into());
    }
    if table.gpt {
        let mut entries = table.entries.clone();
        entries[slot * table.entry_size..(slot + 1) * table.entry_size].fill(0);
        write_gpt(drive, &table.head, &entries)?;
    } else {
        let mut s0 = fresh_mbr(drive, table)?;
        s0[446 + slot * 16..462 + slot * 16].fill(0);
        drive.put(0, &s0)?;
    }
    crate::kprintln!("parts: {drive:?}: partition {} deleted", slot + 1);
    Ok(())
}

/// Sector 0 of the disk — if its table has not changed since.
fn fresh_mbr(drive: Drive, table: &Table) -> Result<Vec<u8>, String> {
    let s0 = drive.at(0, 1).ok_or("cannot read the partition table")?;
    if s0[446..510] != table.head[446..510] {
        return Err("the partition table changed; look again".into());
    }
    Ok(s0)
}

fn write_slot(drive: Drive, table: &Table, slot: usize, start: u64, sectors: u64, kind: Kind, name: &str) -> Result<Placed, String> {
    if table.gpt {
        let mut entries = table.entries.clone();
        let size = table.entry_size;
        let unique = gpt_entry(&mut entries[slot * size..slot * size + ENTRY], start, sectors, kind, name);
        // The entry is longer than 128 bytes: the tail is zeroed too (`gpt_entry` only clears 128).
        entries[slot * size + ENTRY..(slot + 1) * size].fill(0);
        write_gpt(drive, &table.head, &entries)?;
        crate::kprintln!("parts: {drive:?}: GPT partition {} at {start}, {sectors} sectors", slot + 1);
        Ok(Placed { number: slot as u32 + 1, start, sectors, signature: unique, gpt: true })
    } else {
        let mut s0 = fresh_mbr(drive, table)?;
        // The disk signature is needed so the firmware and the kernel recognise the partition.
        if s0[440..444] == [0; 4] {
            s0[440..444].copy_from_slice(&crate::crypto::random32()[..4]);
        }
        let ty = if kind == Kind::Esp { 0xEF } else { 0x0C };
        let e = &mut s0[446 + slot * 16..462 + slot * 16];
        e.copy_from_slice(&[0, 0xFE, 0xFF, 0xFF, ty, 0xFE, 0xFF, 0xFF, 0, 0, 0, 0, 0, 0, 0, 0]);
        put(e, 8, start, 4);
        put(e, 12, sectors, 4);
        let mut signature = [0u8; 16];
        signature[..4].copy_from_slice(&s0[440..444]);
        drive.put(0, &s0)?;
        crate::kprintln!("parts: {drive:?}: MBR partition {} at {start}, {sectors} sectors", slot + 1);
        Ok(Placed { number: slot as u32 + 1, start, sectors, signature, gpt: false })
    }
}
