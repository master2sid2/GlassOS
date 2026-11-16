//! FAT32 and FAT16: volumes compatible with Windows, Linux and macOS.
//!
//! The whole FAT is kept in memory; data and directory sectors are cached.
//! Changes accumulate in the cache ("dirty" sectors) and go to disk on `take_dirty`
//! (called by the background saver) — so writing does not slow down the UI.
//! Names are long (LFN, UTF-16), lookup is case-insensitive, as in Windows.

use super::device_discard;
use super::span::{Run, Sectors, push_run, read_run, write_run};
use super::{DEVS, Device, Entry, FsError, Result};
use crate::ata::SECTOR;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

const EOC: u32 = 0x0FFF_FFFF;
const ATTR_DIR: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LFN: u8 = 0x0F;
const ATTR_VOLUME: u8 = 0x08;
const DELETED: u8 = 0xE5;
/// How many clean sectors to keep in the cache (extra ones are evicted).
const CACHE_LIMIT: usize = 16384;

type Sector = Box<[u8; SECTOR]>;

/// A directory entry together with its location (indices of 32-byte slots in the directory).
#[derive(Clone)]
pub struct DirEntry {
    pub name: String,
    pub attr: u8,
    pub cluster: u32,
    pub size: u32,
    /// Slot of the short entry and the first slot of the long name before it.
    slot: usize,
    first: usize,
    raw: [u8; 32],
}

impl DirEntry {
    pub fn is_dir(&self) -> bool {
        self.attr & ATTR_DIR != 0
    }
}

/// What a path resolved to: the volume root or an entry in directory `parent`.
pub enum Found {
    Root,
    Entry(u32, DirEntry),
}

pub struct Fat {
    /// Device index in the `DEVS` table.
    pub dev: usize,
    /// FAT16: 2-byte table entries, the root is a separate sector area.
    fat16: bool,
    root_start: u32,
    root_sectors: u32,
    /// Volume label (from the boot sector).
    pub label: String,
    spc: u32,
    fat_start: u32,
    fat_size: u32,
    num_fats: u32,
    data_start: u32,
    /// Data cluster numbers: 2..clusters + 2.
    clusters: u32,
    root: u32,
    fat: Vec<u32>,
    fat_dirty: BTreeSet<u32>,
    free: u32,
    next_free: u32,
    cache: BTreeMap<u32, Sector>,
    dirty: BTreeSet<u32>,
    /// Where we stopped in a chain: (file's first cluster, index, cluster). Otherwise writing
    /// a large file in pieces would walk the chain from the start for every piece.
    cursor: Option<(u32, u64, u32)>,
    /// A write to disk is in progress: do not evict clean sectors from the cache
    /// (the newest data of some of them has not reached the disk yet).
    pub writing: bool,
}

fn le16(b: &[u8], at: usize) -> u32 {
    u16::from_le_bytes([b[at], b[at + 1]]) as u32
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn put16(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 2].copy_from_slice(&(v as u16).to_le_bytes());
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// Date and time for directory entries (FAT format).
fn timestamp() -> (u32, u32) {
    let t = crate::time::now();
    let date = ((t.year.saturating_sub(1980) as u32) << 9) | (t.month as u32) << 5 | t.day as u32;
    let time = (t.hour as u32) << 11 | (t.min as u32) << 5 | (t.sec as u32 / 2);
    (date, time)
}

fn lfn_checksum(short: &[u8]) -> u8 {
    short[..11].iter().fold(0u8, |sum, &c| (sum >> 1 | sum << 7).wrapping_add(c))
}

/// Whether a file may have this name in FAT.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.encode_utf16().count() <= 255
        && !name.chars().any(|c| (c as u32) < 0x20 || "\\/:*?\"<>|".contains(c))
        && !name.ends_with(' ')
}

impl Sectors for Fat {
    fn read_sector(&mut self, lba: u32) -> Result<&[u8; SECTOR]> {
        self.load(lba, 1)?;
        Ok(&self.cache[&lba])
    }
    fn update_sector(&mut self, lba: u32) -> Result<&mut [u8; SECTOR]> {
        self.sector_mut(lba)
    }
    fn replace_sector(&mut self, lba: u32) -> &mut [u8; SECTOR] {
        self.sector_new(lba)
    }
    fn uncached(&self, lba: u32, n: u32) -> bool {
        self.cache.range(lba..lba + n).next().is_none()
    }
    fn flushing(&self) -> bool {
        self.writing
    }
    fn device(&self) -> usize {
        self.dev
    }
}

impl Fat {
    // ------------------------------------------------------------ sector cache

    fn cluster_bytes(&self) -> usize {
        self.spc as usize * SECTOR
    }

    fn cluster_lba(&self, c: u32) -> u32 {
        self.data_start + (c - 2) * self.spc
    }

    /// Loads sectors `lba..lba + n` into the cache (missing ones in batches).
    fn load(&mut self, lba: u32, n: u32) -> Result<()> {
        let mut i = 0;
        while i < n {
            if self.cache.contains_key(&(lba + i)) {
                i += 1;
                continue;
            }
            let start = i;
            while i < n && !self.cache.contains_key(&(lba + i)) && i - start < 255 {
                i += 1;
            }
            let count = (i - start) as usize;
            let mut buf = vec![0u8; count * SECTOR];
            let ok = DEVS.lock().get(self.dev).and_then(|d| d.as_ref()).is_some_and(|d| d.read(lba + start, &mut buf));
            if !ok {
                return Err(FsError::Io);
            }
            for (k, chunk) in buf.chunks(SECTOR).enumerate() {
                self.cache.insert(lba + start + k as u32, Box::new(chunk.try_into().unwrap()));
            }
        }
        // What was just read is needed by the caller: do not evict it from the cache.
        if self.cache.len().saturating_sub(self.dirty.len()) > 2 * CACHE_LIMIT && !self.writing {
            let dirty = &self.dirty;
            self.cache.retain(|l, _| dirty.contains(l) || (lba..lba + n).contains(l));
        }
        Ok(())
    }

    fn sector_mut(&mut self, lba: u32) -> Result<&mut [u8; SECTOR]> {
        self.load(lba, 1)?;
        self.dirty.insert(lba);
        Ok(self.cache.get_mut(&lba).unwrap())
    }

    /// A sector that will be overwritten entirely: no need to read it from disk.
    fn sector_new(&mut self, lba: u32) -> &mut [u8; SECTOR] {
        self.dirty.insert(lba);
        self.cache.entry(lba).or_insert_with(|| Box::new([0; SECTOR]))
    }

    /// Evicts clean sectors from the cache if there are too many.
    pub fn trim(&mut self) {
        if self.writing || self.cache.len() <= CACHE_LIMIT {
            return;
        }
        let dirty = &self.dirty;
        self.cache.retain(|lba, _| dirty.contains(lba));
    }

    /// Takes everything that must be written to disk: data sectors, both FAT copies and FSInfo.
    pub fn take_dirty(&mut self) -> Vec<(u32, Sector)> {
        let mut out: Vec<(u32, Sector)> = Vec::new();
        let per = self.per_sector();
        for idx in core::mem::take(&mut self.fat_dirty) {
            let mut s = Box::new([0u8; SECTOR]);
            for k in 0..per {
                let c = idx as usize * per + k;
                if let Some(&v) = self.fat.get(c) {
                    if self.fat16 {
                        put16(&mut s[..], k * 2, v & 0xFFFF);
                    } else {
                        put32(&mut s[..], k * 4, v);
                    }
                }
            }
            for copy in 0..self.num_fats {
                out.push((self.fat_start + copy * self.fat_size + idx, s.clone()));
            }
        }
        if !out.is_empty() && !self.fat16 {
            // FSInfo: free-space hints (optional, but other OSes read them).
            let (free, next) = (self.free, self.next_free);
            if let Ok(info) = self.sector_mut(1) {
                put32(&mut info[..], 488, free);
                put32(&mut info[..], 492, next);
            }
        }
        for lba in core::mem::take(&mut self.dirty) {
            if let Some(s) = self.cache.get(&lba) {
                out.push((lba, s.clone()));
            }
        }
        out.sort_by_key(|(lba, _)| *lba);
        out
    }

    pub fn has_changes(&self) -> bool {
        !self.dirty.is_empty() || !self.fat_dirty.is_empty()
    }

    // ------------------------------------------------------------ the FAT

    fn set_fat(&mut self, c: u32, v: u32) {
        let old = self.fat[c as usize];
        // Growing a file does not change the chain up to this point; everything else does.
        if old & 0x0FFF_FFFF != 0 && (v == 0 || v == EOC) {
            self.cursor = None;
        }
        if old == 0 && v != 0 {
            self.free -= 1;
        } else if old != 0 && v == 0 {
            self.free += 1;
        }
        self.fat[c as usize] = (old & 0xF000_0000) | (v & 0x0FFF_FFFF);
        self.fat_dirty.insert(c / self.per_sector() as u32);
    }

    /// FAT entries per sector.
    fn per_sector(&self) -> usize {
        if self.fat16 { SECTOR / 2 } else { SECTOR / 4 }
    }

    fn next(&self, c: u32) -> Option<u32> {
        let v = self.fat[c as usize] & 0x0FFF_FFFF;
        (2..self.clusters + 2).contains(&v).then_some(v)
    }

    /// A file's cluster chain (protected against loops on a corrupted volume).
    fn chain(&self, start: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut c = start;
        while (2..self.clusters + 2).contains(&c) && out.len() <= self.clusters as usize {
            out.push(c);
            match self.next(c) {
                Some(n) => c = n,
                None => break,
            }
        }
        out
    }

    /// Allocates `n` free clusters and links them into a chain.
    fn alloc(&mut self, n: usize) -> Result<Vec<u32>> {
        if n as u32 > self.free {
            return Err(FsError::NoSpace);
        }
        let mut out = Vec::with_capacity(n);
        let mut c = self.next_free.max(2);
        let total = self.clusters + 2;
        let mut scanned = 0;
        while out.len() < n && scanned < self.clusters {
            if c >= total {
                c = 2;
            }
            if self.fat[c as usize] & 0x0FFF_FFFF == 0 {
                out.push(c);
            }
            c += 1;
            scanned += 1;
        }
        if out.len() < n {
            return Err(FsError::NoSpace);
        }
        for i in 0..n {
            let v = out.get(i + 1).copied().unwrap_or(EOC);
            self.set_fat(out[i], v);
        }
        self.next_free = c;
        Ok(out)
    }

    /// Cluster number `index` of a file.
    fn cluster_at(&mut self, first: u32, index: u64) -> Result<u32> {
        let (mut at, mut c) = match self.cursor {
            Some((f, i, c)) if f == first && i <= index => (i, c),
            _ => (0, first),
        };
        while at < index {
            c = self.next(c).ok_or(FsError::Io)?;
            at += 1;
        }
        self.cursor = Some((first, index, c));
        Ok(c)
    }

    /// Where on disk `len` bytes of a file at `offset` lie.
    fn runs(&mut self, first: u32, offset: u64, len: usize) -> Result<Vec<Run>> {
        let cb = self.cluster_bytes() as u64;
        let (mut pos, end) = (offset, offset + len as u64);
        let mut out = Vec::new();
        while pos < end {
            let c = self.cluster_at(first, pos / cb)?;
            let inner = pos % cb;
            let n = (cb - inner).min(end - pos);
            push_run(&mut out, self.cluster_lba(c) + (inner / SECTOR as u64) as u32, (inner % SECTOR as u64) as usize, n as usize);
            pos += n;
        }
        Ok(out)
    }

    /// Writes a piece of an existing file, not beyond its end.
    pub fn write_at(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<()> {
        let Found::Entry(dir, mut e) = self.find(path)? else { return Err(FsError::IsADirectory) };
        if e.is_dir() {
            return Err(FsError::IsADirectory);
        }
        let end = offset + data.len() as u64;
        if end > u32::MAX as u64 {
            return Err(FsError::FileTooBig);
        }
        if data.is_empty() {
            return Ok(());
        }
        let cb = self.cluster_bytes() as u64;
        let (have, need) = ((e.size as u64).div_ceil(cb), end.div_ceil(cb));
        if need > have {
            let more = self.alloc((need - have) as usize)?;
            if have == 0 {
                e.cluster = more[0];
                put16(&mut e.raw, 20, more[0] >> 16);
                put16(&mut e.raw, 26, more[0] & 0xFFFF);
            } else {
                let last = self.cluster_at(e.cluster, have - 1)?;
                self.set_fat(last, more[0]);
            }
        }
        let mut at = 0;
        for run in self.runs(e.cluster, offset, data.len())? {
            write_run(self, &run, &data[at..at + run.bytes])?;
            at += run.bytes;
        }
        let (date, time) = timestamp();
        put32(&mut e.raw, 28, e.size.max(end as u32));
        put16(&mut e.raw, 22, time);
        put16(&mut e.raw, 24, date);
        put16(&mut e.raw, 18, date);
        e.raw[11] |= ATTR_ARCHIVE;
        self.update_entry(dir, &e)
    }

    /// Truncates a file to `len`.
    pub fn shrink(&mut self, path: &str, len: u64) -> Result<()> {
        let Found::Entry(dir, mut e) = self.find(path)? else { return Err(FsError::IsADirectory) };
        if e.is_dir() {
            return Err(FsError::IsADirectory);
        }
        if len >= e.size as u64 {
            return Ok(());
        }
        let keep = len.div_ceil(self.cluster_bytes() as u64);
        if keep == 0 {
            if e.cluster != 0 {
                self.free_chain(e.cluster);
            }
            e.cluster = 0;
            put16(&mut e.raw, 20, 0);
            put16(&mut e.raw, 26, 0);
        } else {
            let last = self.cluster_at(e.cluster, keep - 1)?;
            if let Some(tail) = self.next(last) {
                self.free_chain(tail);
            }
            self.set_fat(last, EOC);
        }
        put32(&mut e.raw, 28, len as u32);
        self.update_entry(dir, &e)
    }

    /// How many changes are waiting to be written to disk.
    pub fn pending_bytes(&self) -> usize {
        (self.dirty.len() + self.fat_dirty.len()) * SECTOR
    }

    fn free_chain(&mut self, start: u32) {
        let chain = self.chain(start);
        for &c in &chain {
            self.set_fat(c, 0);
        }
        let mut i = 0;
        while i < chain.len() {
            let mut j = i + 1;
            while j < chain.len() && chain[j] == chain[j - 1] + 1 {
                j += 1;
            }
            self.forget(self.cluster_lba(chain[i]), (j - i) as u32 * self.spc);
            i = j;
        }
    }

    /// Sectors of freed clusters: no need to write them, and the device may discard them.
    fn forget(&mut self, lba: u32, n: u32) {
        let cached: Vec<u32> = self.cache.range(lba..lba + n).map(|(l, _)| *l).collect();
        for l in cached {
            self.cache.remove(&l);
            self.dirty.remove(&l);
        }
        device_discard(self.dev, lba, n);
    }

    // ------------------------------------------------------------ contents

    fn read_clusters(&mut self, chain: &[u32], len: usize) -> Result<Vec<u8>> {
        let cb = self.cluster_bytes();
        let mut out = Vec::with_capacity(len);
        // Read consecutive clusters in one batch.
        let mut i = 0;
        while i < chain.len() && out.len() < len {
            let mut j = i + 1;
            while j < chain.len() && chain[j] == chain[j - 1] + 1 && (j - i) * cb < len - out.len() {
                j += 1;
            }
            let lba = self.cluster_lba(chain[i]);
            let sectors = ((j - i) as u32 * self.spc).min((len - out.len()).div_ceil(SECTOR) as u32);
            self.load(lba, sectors)?;
            for k in 0..sectors {
                let s = &self.cache[&(lba + k)];
                let n = SECTOR.min(len - out.len());
                out.extend_from_slice(&s[..n]);
            }
            i = j;
        }
        Ok(out)
    }

    fn write_clusters(&mut self, chain: &[u32], data: &[u8]) {
        let spc = self.spc as usize;
        for (k, chunk) in data.chunks(SECTOR).enumerate() {
            let lba = self.cluster_lba(chain[k / spc]) + (k % spc) as u32;
            let s = self.sector_new(lba);
            s[..chunk.len()].copy_from_slice(chunk);
            s[chunk.len()..].fill(0);
        }
    }

    fn zero_cluster(&mut self, c: u32) {
        let lba = self.cluster_lba(c);
        for k in 0..self.spc {
            self.sector_new(lba + k).fill(0);
        }
    }

    // ------------------------------------------------------------ directories

    /// The FAT16 root is a separate fixed-size area (not a cluster chain).
    fn fixed_root(&self, dir: u32) -> bool {
        dir == 0 && self.fat16
    }

    fn dir_chain(&self, dir: u32) -> Vec<u32> {
        self.chain(if dir == 0 { self.root } else { dir })
    }

    /// The directory's sectors in order.
    fn dir_sectors(&self, dir: u32) -> Vec<u32> {
        if self.fixed_root(dir) {
            return (self.root_start..self.root_start + self.root_sectors).collect();
        }
        self.cluster_sectors(&self.dir_chain(dir))
    }

    fn cluster_sectors(&self, chain: &[u32]) -> Vec<u32> {
        chain.iter().flat_map(|&c| (0..self.spc).map(move |k| self.cluster_lba(c) + k)).collect()
    }

    fn slot_place(sectors: &[u32], slot: usize) -> (u32, usize) {
        let byte = slot * 32;
        (sectors[byte / SECTOR], byte % SECTOR)
    }

    fn put_slot(&mut self, sectors: &[u32], slot: usize, raw: &[u8; 32]) -> Result<()> {
        let (lba, off) = Self::slot_place(sectors, slot);
        self.sector_mut(lba)?[off..off + 32].copy_from_slice(raw);
        Ok(())
    }

    /// The directory's raw 32-byte slots (and its sectors).
    fn dir_raw(&mut self, dir: u32) -> Result<(Vec<u32>, Vec<u8>)> {
        let sectors = self.dir_sectors(dir);
        let mut raw = Vec::with_capacity(sectors.len() * SECTOR);
        let mut i = 0;
        while i < sectors.len() {
            // Consecutive sectors in one batch.
            let mut j = i + 1;
            while j < sectors.len() && sectors[j] == sectors[j - 1] + 1 {
                j += 1;
            }
            self.load(sectors[i], (j - i) as u32)?;
            for &lba in &sectors[i..j] {
                raw.extend_from_slice(&self.cache[&lba][..]);
            }
            i = j;
        }
        Ok((sectors, raw))
    }

    /// Directory entries (`dir` = 0 is the root), without "." and "..".
    pub fn read_dir(&mut self, dir: u32) -> Result<Vec<DirEntry>> {
        let (_, raw) = self.dir_raw(dir)?;
        let mut out = Vec::new();
        let mut lfn: Vec<(u8, [u16; 13])> = Vec::new();
        let mut lfn_first = 0;
        let mut lfn_sum = 0u8;
        for (slot, e) in raw.chunks_exact(32).enumerate() {
            match e[0] {
                0x00 => break,
                DELETED => {
                    lfn.clear();
                    continue;
                }
                _ => {}
            }
            if e[11] == ATTR_LFN {
                if e[0] & 0x40 != 0 {
                    lfn.clear();
                    lfn_first = slot;
                    lfn_sum = e[13];
                }
                let mut chars = [0u16; 13];
                for (k, at) in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30].iter().enumerate() {
                    chars[k] = le16(e, *at) as u16;
                }
                lfn.push((e[0] & 0x1F, chars));
                continue;
            }
            if e[11] & ATTR_VOLUME != 0 || e[0] == b'.' {
                lfn.clear();
                continue;
            }
            let long = (!lfn.is_empty() && lfn_checksum(e) == lfn_sum).then(|| {
                lfn.sort_by_key(|(ord, _)| *ord);
                let units: Vec<u16> =
                    lfn.iter().flat_map(|(_, c)| c.iter().copied()).take_while(|&u| u != 0 && u != 0xFFFF).collect();
                String::from_utf16_lossy(&units)
            });
            let name = long.unwrap_or_else(|| short_to_string(e));
            let mut raw_entry = [0u8; 32];
            raw_entry.copy_from_slice(e);
            out.push(DirEntry {
                name,
                attr: e[11],
                cluster: le16(e, 20) << 16 | le16(e, 26),
                size: le32(e, 28),
                slot,
                first: if lfn.is_empty() { slot } else { lfn_first },
                raw: raw_entry,
            });
            lfn.clear();
        }
        Ok(out)
    }

    /// Finds an entry by path (case-insensitive name lookup).
    pub fn find(&mut self, path: &str) -> Result<Found> {
        let mut dir = 0u32;
        let mut found = Found::Root;
        for part in path.split('/').filter(|s| !s.is_empty()) {
            if let Found::Entry(_, e) = &found {
                if !e.is_dir() {
                    return Err(FsError::NotADirectory);
                }
                dir = e.cluster;
            }
            let e = self
                .read_dir(dir)?
                .into_iter()
                .find(|e| e.name.eq_ignore_ascii_case(part))
                .ok_or(FsError::NotFound)?;
            found = Found::Entry(dir, e);
        }
        Ok(found)
    }

    /// Directory cluster by path (0 — the root).
    pub fn dir_cluster(&mut self, path: &str) -> Result<u32> {
        match self.find(path)? {
            Found::Root => Ok(0),
            Found::Entry(_, e) if e.is_dir() => Ok(e.cluster),
            Found::Entry(..) => Err(FsError::NotADirectory),
        }
    }

    /// A name that already fits 8.3 (upper case, base up to 8 characters,
    /// extension up to 3): written as is and without a long entry, as all
    /// systems do. This way `\EFI\BOOT\BOOTX64.EFI` is found even by firmware that does
    /// not read long names.
    fn exact_short(name: &str) -> Option<[u8; 11]> {
        let ok = |s: &str, max: usize| s.len() <= max && s.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || b"$%'-_@~`!(){}^#&".contains(&c));
        let (stem, ext) = name.split_once('.').unwrap_or((name, ""));
        if stem.is_empty() || !ok(stem, 8) || !ok(ext, 3) || name.ends_with('.') {
            return None;
        }
        let mut short = [b' '; 11];
        short[..stem.len()].copy_from_slice(stem.as_bytes());
        short[8..8 + ext.len()].copy_from_slice(ext.as_bytes());
        Some(short)
    }

    /// A unique 8.3 short name for a long one: BASE~N.EXT.
    fn short_name(name: &str, taken: &[[u8; 11]]) -> [u8; 11] {
        let clean = |s: &str, n: usize| -> Vec<u8> {
            s.chars()
                .filter_map(|c| {
                    let c = c.to_ascii_uppercase();
                    (c.is_ascii_alphanumeric() || "$%'-_@~`!(){}^#&".contains(c)).then_some(c as u8)
                })
                .take(n)
                .collect()
        };
        let (stem, ext) = match name.rfind('.') {
            Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
            _ => (name, ""),
        };
        let (base, ext) = (clean(stem, 8), clean(ext, 3));
        let base = if base.is_empty() { b"FILE".to_vec() } else { base };
        for n in 1u32.. {
            let tail = alloc::format!("~{n}");
            let keep = (8 - tail.len()).min(base.len());
            let mut short = [b' '; 11];
            short[..keep].copy_from_slice(&base[..keep]);
            short[keep..keep + tail.len()].copy_from_slice(tail.as_bytes());
            short[8..8 + ext.len()].copy_from_slice(&ext);
            if !taken.contains(&short) {
                return short;
            }
        }
        unreachable!()
    }

    /// Adds entry `name` to a directory (long name + short one). `template` is
    /// the contents of the short entry (attributes, cluster, size, times).
    fn add_entry(&mut self, dir: u32, name: &str, mut template: [u8; 32]) -> Result<()> {
        if !valid_name(name) {
            return Err(FsError::InvalidName);
        }
        let (_, raw) = self.dir_raw(dir)?;
        let shorts: Vec<[u8; 11]> = raw
            .chunks_exact(32)
            .take_while(|e| e[0] != 0)
            .filter(|e| e[0] != DELETED && e[11] != ATTR_LFN)
            .map(|e| e[..11].try_into().unwrap())
            .collect();
        let exact = Self::exact_short(name).filter(|s| !shorts.contains(s));
        let short = exact.unwrap_or_else(|| Self::short_name(name, &shorts));
        template[..11].copy_from_slice(&short);
        let units: Vec<u16> = name.encode_utf16().collect();
        let parts = if exact.is_some() { 0 } else { units.len().div_ceil(13) };
        let need = parts + 1;

        // Look for `need` consecutive free slots; if there are not enough, grow the directory.
        let (mut sectors, mut raw) = (self.dir_sectors(dir), raw);
        let start = loop {
            let slots = raw.len() / 32;
            let mut run = 0;
            let mut found = None;
            for i in 0..slots {
                let b = raw[i * 32];
                if b == 0 || b == DELETED {
                    run += 1;
                    if run == need {
                        found = Some(i + 1 - need);
                        break;
                    }
                } else {
                    run = 0;
                }
            }
            if let Some(s) = found {
                break s;
            }
            if self.fixed_root(dir) {
                return Err(FsError::NoSpace); // the FAT16 root does not grow
            }
            let c = self.alloc(1)?[0];
            self.zero_cluster(c);
            let last = *self.dir_chain(dir).last().unwrap();
            self.set_fat(last, c);
            sectors.extend(self.cluster_sectors(&[c]));
            raw.resize(raw.len() + self.cluster_bytes(), 0);
        };

        let sum = lfn_checksum(&short);
        for p in 0..parts {
            let ord = (parts - p) as u8;
            let mut e = [0u8; 32];
            e[0] = ord | if p == 0 { 0x40 } else { 0 };
            e[11] = ATTR_LFN;
            e[13] = sum;
            for (k, at) in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30].iter().enumerate() {
                let idx = (ord as usize - 1) * 13 + k;
                let u = match idx.cmp(&units.len()) {
                    core::cmp::Ordering::Less => units[idx],
                    core::cmp::Ordering::Equal => 0,
                    core::cmp::Ordering::Greater => 0xFFFF,
                };
                put16(&mut e, *at, u as u32);
            }
            self.put_slot(&sectors, start + p, &e)?;
        }
        self.put_slot(&sectors, start + parts, &template)?;
        Ok(())
    }

    fn remove_entry(&mut self, dir: u32, e: &DirEntry) -> Result<()> {
        let sectors = self.dir_sectors(dir);
        for slot in e.first..=e.slot {
            let (lba, off) = Self::slot_place(&sectors, slot);
            self.sector_mut(lba)?[off] = DELETED;
        }
        Ok(())
    }

    fn update_entry(&mut self, dir: u32, e: &DirEntry) -> Result<()> {
        let sectors = self.dir_sectors(dir);
        self.put_slot(&sectors, e.slot, &e.raw)
    }

    fn new_template(attr: u8, cluster: u32, size: u32) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[11] = attr;
        let (date, time) = timestamp();
        put16(&mut e, 14, time);
        put16(&mut e, 16, date);
        put16(&mut e, 18, date);
        put16(&mut e, 22, time);
        put16(&mut e, 24, date);
        put16(&mut e, 20, cluster >> 16);
        put16(&mut e, 26, cluster & 0xFFFF);
        put32(&mut e, 28, size);
        e
    }

    // ------------------------------------------------------------ operations by path

    pub fn list(&mut self, path: &str) -> Result<Vec<Entry>> {
        let dir = self.dir_cluster(path)?;
        let entries = self.read_dir(dir)?;
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            let size = if e.is_dir() { self.read_dir(e.cluster).map_or(0, |c| c.len()) } else { e.size as usize };
            out.push(Entry { name: e.name, is_dir: e.attr & ATTR_DIR != 0, size });
        }
        out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(out)
    }

    pub fn read(&mut self, path: &str) -> Result<Vec<u8>> {
        match self.find(path)? {
            Found::Entry(_, e) if !e.is_dir() => {
                let chain = self.chain(e.cluster);
                self.read_clusters(&chain, e.size as usize)
            }
            _ => Err(FsError::IsADirectory),
        }
    }

    pub fn read_range(&mut self, path: &str, offset: u64, len: usize) -> Result<(Vec<u8>, u64)> {
        let Found::Entry(_, e) = self.find(path)? else { return Err(FsError::IsADirectory) };
        if e.is_dir() {
            return Err(FsError::IsADirectory);
        }
        let size = e.size as u64;
        let want = (len as u64).min(size.saturating_sub(offset)) as usize;
        let mut data = Vec::with_capacity(want);
        for run in self.runs(e.cluster, offset, want)? {
            read_run(self, &run, &mut data)?;
        }
        Ok((data, size))
    }

    pub fn is_dir(&mut self, path: &str) -> Option<bool> {
        match self.find(path).ok()? {
            Found::Root => Some(true),
            Found::Entry(_, e) => Some(e.is_dir()),
        }
    }

    fn split(path: &str) -> Result<(&str, &str)> {
        let path = path.trim_end_matches('/');
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        if !valid_name(name) {
            return Err(FsError::InvalidName);
        }
        Ok((parent, name))
    }

    /// Writes a whole file: clusters are reused where possible.
    pub fn write(&mut self, path: &str, data: &[u8]) -> Result<()> {
        let (parent, name) = Self::split(path)?;
        let dir = self.dir_cluster(parent)?;
        let existing = self.read_dir(dir)?.into_iter().find(|e| e.name.eq_ignore_ascii_case(name));
        if existing.as_ref().is_some_and(|e| e.is_dir()) {
            return Err(FsError::IsADirectory);
        }
        let cb = self.cluster_bytes();
        let need = data.len().div_ceil(cb);
        let mut chain = existing.as_ref().map_or(Vec::new(), |e| self.chain(e.cluster));
        if chain.len() > need {
            // Free the extra tail.
            for &c in &chain[need..] {
                self.set_fat(c, 0);
            }
            chain.truncate(need);
            if let Some(&last) = chain.last() {
                self.set_fat(last, EOC);
            }
        } else if chain.len() < need {
            let more = self.alloc(need - chain.len())?;
            if let Some(&last) = chain.last() {
                self.set_fat(last, more[0]);
            }
            chain.extend(more);
        }
        self.write_clusters(&chain, data);
        let first = chain.first().copied().unwrap_or(0);
        match existing {
            Some(mut e) => {
                let (date, time) = timestamp();
                put16(&mut e.raw, 20, first >> 16);
                put16(&mut e.raw, 26, first & 0xFFFF);
                put32(&mut e.raw, 28, data.len() as u32);
                put16(&mut e.raw, 22, time);
                put16(&mut e.raw, 24, date);
                put16(&mut e.raw, 18, date);
                e.raw[11] |= ATTR_ARCHIVE;
                self.update_entry(dir, &e)
            }
            None => self.add_entry(dir, name, Self::new_template(ATTR_ARCHIVE, first, data.len() as u32)),
        }
    }

    pub fn mkdir(&mut self, path: &str) -> Result<()> {
        let (parent, name) = Self::split(path)?;
        let dir = self.dir_cluster(parent)?;
        if self.read_dir(dir)?.iter().any(|e| e.name.eq_ignore_ascii_case(name)) {
            return Err(FsError::AlreadyExists);
        }
        let c = self.alloc(1)?[0];
        self.zero_cluster(c);
        let mut dot = Self::new_template(ATTR_DIR, c, 0);
        dot[..11].copy_from_slice(b".          ");
        let mut dotdot = Self::new_template(ATTR_DIR, dir, 0);
        dotdot[..11].copy_from_slice(b"..         ");
        let sectors = self.cluster_sectors(&[c]);
        self.put_slot(&sectors, 0, &dot)?;
        self.put_slot(&sectors, 1, &dotdot)?;
        self.add_entry(dir, name, Self::new_template(ATTR_DIR, c, 0))
    }

    fn remove_tree(&mut self, e: &DirEntry) -> Result<()> {
        if e.is_dir() {
            for child in self.read_dir(e.cluster)? {
                self.remove_tree(&child)?;
            }
        }
        if e.cluster != 0 {
            self.free_chain(e.cluster);
        }
        Ok(())
    }

    pub fn remove(&mut self, path: &str) -> Result<()> {
        match self.find(path)? {
            Found::Root => Err(FsError::InvalidName),
            Found::Entry(dir, e) => {
                self.remove_tree(&e)?;
                self.remove_entry(dir, &e)
            }
        }
    }

    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let Found::Entry(src_dir, e) = self.find(from)? else { return Err(FsError::InvalidName) };
        let (parent, name) = Self::split(to)?;
        let dst_dir = self.dir_cluster(parent)?;
        if let Some(other) = self.read_dir(dst_dir)?.into_iter().find(|x| x.name.eq_ignore_ascii_case(name)) {
            // The same file (for example, only the letter case changes) — allowed.
            if !(dst_dir == src_dir && other.slot == e.slot) {
                return Err(FsError::AlreadyExists);
            }
        }
        // Delete the old entry first (the name is freed), then write the new one.
        self.remove_entry(src_dir, &e)?;
        self.add_entry(dst_dir, name, e.raw)?;
        if e.is_dir() && src_dir != dst_dir {
            // ".." in a moved directory must point to the new parent.
            let sectors = self.cluster_sectors(&self.chain(e.cluster));
            if !sectors.is_empty() {
                let (lba, off) = Self::slot_place(&sectors, 1);
                let s = self.sector_mut(lba)?;
                put16(&mut s[off..], 20, dst_dir >> 16);
                put16(&mut s[off..], 26, dst_dir & 0xFFFF);
            }
        }
        Ok(())
    }

    /// Used and total bytes in the data area.
    pub fn usage(&self) -> (u64, u64) {
        let cb = self.cluster_bytes() as u64;
        ((self.clusters - self.free) as u64 * cb, self.clusters as u64 * cb)
    }

    // ------------------------------------------------------------ volume

    /// Reads the boot sector and the FAT. None — the device is not FAT32/FAT16.
    /// `dev` is the device index in `DEVS` (the volume later reads sectors through it).
    pub fn mount(device: &Device, dev: usize) -> Option<Fat> {
        let mut boot = [0u8; SECTOR];
        if !device.read(0, &mut boot) || boot[510..512] != [0x55, 0xAA] || le16(&boot, 11) != SECTOR as u32 {
            return None;
        }
        let spc = boot[13] as u32;
        let reserved = le16(&boot, 14);
        let num_fats = boot[16] as u32;
        let root_entries = le16(&boot, 17);
        let fat16 = le16(&boot, 22) != 0;
        let fat_size = if fat16 { le16(&boot, 22) } else { le32(&boot, 36) };
        let total = if le16(&boot, 19) != 0 { le16(&boot, 19) } else { le32(&boot, 32) };
        if spc == 0 || num_fats == 0 || fat_size == 0 || reserved == 0 || (fat16 && root_entries == 0) {
            return None;
        }
        let root_start = reserved + num_fats * fat_size;
        let root_sectors = (root_entries * 32).div_ceil(SECTOR as u32);
        let data_start = root_start + root_sectors;
        let clusters = total.checked_sub(data_start)? / spc;
        // Fewer than 4085 clusters means FAT12 (floppies); not supported.
        if fat16 && clusters < 4085 {
            return None;
        }
        let per = if fat16 { 256 } else { 128 };
        let entries = ((clusters + 2) as usize).min(fat_size as usize * per);
        let mut raw = vec![0u8; entries.div_ceil(per) * SECTOR];
        if !device.read(reserved, &mut raw) {
            return None;
        }
        let fat: Vec<u32> = if fat16 {
            // Special FAT16 values (end of chain, bad cluster) are converted to FAT32 form.
            (0..entries).map(|i| le16(&raw, i * 2)).map(|v| if v >= 0xFFF7 { v | 0x0FFF_0000 } else { v }).collect()
        } else {
            (0..entries).map(|i| le32(&raw, i * 4)).collect()
        };
        let clusters = entries as u32 - 2;
        let free = fat[2..].iter().filter(|&&v| v & 0x0FFF_FFFF == 0).count() as u32;
        let label_at = if fat16 { 43 } else { 71 };
        let label = core::str::from_utf8(&boot[label_at..label_at + 11]).unwrap_or("").trim().into();
        Some(Fat {
            dev,
            fat16,
            root_start,
            root_sectors: if fat16 { root_sectors } else { 0 },
            label,
            spc,
            fat_start: reserved,
            fat_size,
            num_fats,
            data_start,
            clusters,
            root: if fat16 { 0 } else { le32(&boot, 44) },
            fat,
            fat_dirty: BTreeSet::new(),
            free,
            next_free: 2,
            cache: BTreeMap::new(),
            dirty: BTreeSet::new(),
            cursor: None,
            writing: false,
        })
    }

    pub fn is_fat16(&self) -> bool {
        self.fat16
    }

    /// A new empty volume of `sectors` sectors (entries are in the cache and reach disk on save).
    pub fn format(sectors: u32, dev: usize) -> Fat {
        Self::format_labeled(sectors, dev, "GLASSOS")
    }

    /// The same with a volume label (up to 11 ASCII characters, upper case).
    pub fn format_labeled(sectors: u32, dev: usize, label: &str) -> Fat {
        let mut name = [b' '; 11];
        for (i, c) in label.bytes().filter(|c| c.is_ascii_graphic() || *c == b' ').take(11).enumerate() {
            name[i] = c.to_ascii_uppercase();
        }
        let reserved = 32;
        let num_fats = 2;
        // Clusters like other systems use: a bigger volume gets bigger clusters (and a smaller table in memory).
        let spc = match sectors {
            0..=532_480 => 1,
            532_481..=16_777_216 => 8,
            16_777_217..=33_554_432 => 16,
            33_554_433..=67_108_864 => 32,
            _ => 64,
        };
        let fat_size = ((sectors - reserved) / spc + 2).div_ceil(128);
        let data_start = reserved + num_fats * fat_size;
        let clusters = ((sectors - data_start) / spc).min(fat_size * 128 - 2);
        let mut fat = Fat {
            dev,
            fat16: false,
            root_start: 0,
            root_sectors: 0,
            label: String::from_utf8_lossy(&name).trim_end().into(),
            spc,
            fat_start: reserved,
            fat_size,
            num_fats,
            data_start,
            clusters,
            root: 2,
            fat: vec![0; (clusters + 2) as usize],
            fat_dirty: (0..fat_size).collect(),
            free: clusters,
            next_free: 3,
            cache: BTreeMap::new(),
            dirty: BTreeSet::new(),
            cursor: None,
            writing: false,
        };
        fat.fat[0] = 0x0FFF_FFF8;
        fat.fat[1] = EOC;
        fat.set_fat(2, EOC);
        fat.zero_cluster(2);
        // The volume label is the first root entry (fsck and other OSes expect it).
        let mut label = Self::new_template(ATTR_VOLUME, 0, 0);
        label[..11].copy_from_slice(&name);
        let root = fat.cluster_sectors(&[2]);
        let _ = fat.put_slot(&root, 0, &label);

        let mut boot = [0u8; SECTOR];
        boot[..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
        boot[3..11].copy_from_slice(b"GLASSOS ");
        put16(&mut boot, 11, SECTOR as u32);
        boot[13] = spc as u8;
        put16(&mut boot, 14, reserved);
        boot[16] = num_fats as u8;
        boot[21] = 0xF8;
        put16(&mut boot, 24, 63);
        put16(&mut boot, 26, 255);
        put32(&mut boot, 32, sectors);
        put32(&mut boot, 36, fat_size);
        put32(&mut boot, 44, 2);
        put16(&mut boot, 48, 1);
        put16(&mut boot, 50, 6);
        boot[64] = 0x80;
        boot[66] = 0x29;
        put32(&mut boot, 67, crate::arch::rdtsc() as u32);
        boot[71..82].copy_from_slice(&name);
        boot[82..90].copy_from_slice(b"FAT32   ");
        boot[510] = 0x55;
        boot[511] = 0xAA;
        let mut info = [0u8; SECTOR];
        put32(&mut info, 0, 0x4161_5252);
        put32(&mut info, 484, 0x6141_7272);
        put32(&mut info, 508, 0xAA55_0000);
        for (lba, s) in [(0, boot), (1, info), (6, boot), (7, info)] {
            *fat.sector_new(lba) = s;
        }
        for lba in [2, 3, 4, 5, 8] {
            fat.sector_new(lba).fill(0);
        }
        fat
    }
}

/// An 8.3 short name to a string; the NT flags (byte 12) say whether to write it in lower case.
fn short_to_string(e: &[u8]) -> String {
    let lower = |s: &[u8], flag: bool| -> String {
        let s = core::str::from_utf8(s).unwrap_or("?").trim_end();
        if flag { s.to_lowercase() } else { s.into() }
    };
    let mut base = e[..8].to_vec();
    if base[0] == 0x05 {
        base[0] = DELETED;
    }
    let base = lower(&base, e[12] & 0x08 != 0);
    let ext = lower(&e[8..11], e[12] & 0x10 != 0);
    if ext.is_empty() { base } else { alloc::format!("{base}.{ext}") }
}
