//! exFAT: USB sticks larger than 32 GB and memory cards (including the Ventoy data partition).
//!
//! Volume layout:
//! - cluster allocation is a bitmap (a file in the cluster heap); we keep it in memory;
//! - the FAT is only needed for files whose clusters are not contiguous (contiguous ones
//!   have the NoFatChain flag) — it is read and written through the sector cache;
//! - a directory entry is a set: "file" (attributes, times, checksum),
//!   "stream" (length, first cluster, name hash) and name pieces of 15 characters;
//! - names are compared case-insensitively using the volume's up-case table.
//!
//! New files are placed in one contiguous run if one is found, otherwise as a FAT chain.

use super::span::{Run, Sectors, push_run, read_run, write_run};
use super::{DEVS, Device, Entry, FsError, Filesystem, Result};
use crate::ata::SECTOR;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

type Sector = Box<[u8; SECTOR]>;

const ENTRY_FILE: u8 = 0x85;
const ENTRY_STREAM: u8 = 0xC0;
const ENTRY_NAME: u8 = 0xC1;
const ENTRY_BITMAP: u8 = 0x81;
const ENTRY_UPCASE: u8 = 0x82;
const ENTRY_LABEL: u8 = 0x83;
const IN_USE: u8 = 0x80;
const ATTR_DIR: u16 = 0x10;
const ATTR_ARCHIVE: u16 = 0x20;
const FLAG_ALLOC: u8 = 1;
const FLAG_NO_FAT_CHAIN: u8 = 2;
const EOC: u32 = 0xFFFF_FFFF;
const CACHE_LIMIT: usize = 16384;

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// Where the data of a file or directory lies.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Extent {
    first: u32,
    /// Contiguous clusters (NoFatChain) — otherwise a FAT chain.
    contiguous: bool,
    /// Length in bytes (DataLength).
    len: u64,
}

/// A set of directory entries (file or folder).
#[derive(Clone)]
struct Node {
    name: String,
    attr: u16,
    data: Extent,
    valid: u64,
    /// Index of the set's first entry in the directory.
    slot: usize,
    /// The set's raw entries (for rewriting and moving).
    raw: Vec<[u8; 32]>,
}

impl Node {
    fn is_dir(&self) -> bool {
        self.attr & ATTR_DIR != 0
    }
}

pub struct Exfat {
    dev: usize,
    label: String,
    fat_start: u32,
    heap_start: u32,
    spc_shift: u32,
    clusters: u32,
    root: u32,
    /// Allocation bitmap (one bit per cluster, cluster 2 is bit 0) and where it is on disk.
    bitmap: Vec<u8>,
    bitmap_at: Extent,
    bitmap_dirty: BTreeSet<usize>,
    free: u32,
    next_free: u32,
    upcase: Vec<u16>,
    cache: BTreeMap<u32, Sector>,
    dirty: BTreeSet<u32>,
    writing: bool,
    /// Where we stopped in a chain: (first cluster, index, cluster).
    cursor: Option<(u32, u64, u32)>,
}

impl Sectors for Exfat {
    fn read_sector(&mut self, lba: u32) -> Result<&[u8; SECTOR]> {
        self.sector(lba)
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

impl Exfat {
    // ------------------------------------------------------------ sector cache

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

    fn sector(&mut self, lba: u32) -> Result<&[u8; SECTOR]> {
        self.load(lba, 1)?;
        Ok(&self.cache[&lba])
    }

    fn sector_mut(&mut self, lba: u32) -> Result<&mut [u8; SECTOR]> {
        self.load(lba, 1)?;
        self.dirty.insert(lba);
        Ok(self.cache.get_mut(&lba).unwrap())
    }

    /// The sector will be overwritten entirely — do not read it from disk.
    fn sector_new(&mut self, lba: u32) -> &mut [u8; SECTOR] {
        self.dirty.insert(lba);
        self.cache.entry(lba).or_insert_with(|| Box::new([0; SECTOR]))
    }

    // ------------------------------------------------------------ clusters

    fn cluster_bytes(&self) -> u64 {
        (SECTOR as u64) << self.spc_shift
    }

    fn spc(&self) -> u32 {
        1 << self.spc_shift
    }

    fn cluster_lba(&self, c: u32) -> u32 {
        self.heap_start + ((c - 2) << self.spc_shift)
    }

    fn fat_get(&mut self, c: u32) -> Result<u32> {
        let at = c as usize * 4;
        let lba = self.fat_start + (at / SECTOR) as u32;
        Ok(le32(&self.sector(lba)?[..], at % SECTOR))
    }

    fn fat_set(&mut self, c: u32, v: u32) -> Result<()> {
        let at = c as usize * 4;
        let lba = self.fat_start + (at / SECTOR) as u32;
        self.sector_mut(lba)?[at % SECTOR..at % SECTOR + 4].copy_from_slice(&v.to_le_bytes());
        Ok(())
    }

    fn valid_cluster(&self, c: u32) -> bool {
        (2..self.clusters + 2).contains(&c)
    }

    /// The clusters of an extent in order.
    fn chain(&mut self, e: Extent) -> Result<Vec<u32>> {
        let need = e.len.div_ceil(self.cluster_bytes()) as usize;
        if e.first == 0 {
            return Ok(Vec::new());
        }
        if e.contiguous {
            return Ok((0..need as u32).map(|k| e.first + k).collect());
        }
        let mut out = Vec::new();
        let mut c = e.first;
        // The length of a directory in a FAT chain (the root) is not known in advance — walk to the end.
        while self.valid_cluster(c) && out.len() <= self.clusters as usize && (e.len == u64::MAX || out.len() < need) {
            out.push(c);
            c = self.fat_get(c)?;
        }
        Ok(out)
    }

    /// The sectors of an extent in order.
    fn sectors(&mut self, e: Extent) -> Result<Vec<u32>> {
        let spc = self.spc();
        let chain = self.chain(e)?;
        Ok(chain.iter().flat_map(|&c| (0..spc).map(move |k| (c, k))).map(|(c, k)| self.cluster_lba(c) + k).collect())
    }

    fn is_free(&self, c: u32) -> bool {
        let i = (c - 2) as usize;
        self.bitmap[i / 8] & (1 << (i % 8)) == 0
    }

    fn mark(&mut self, c: u32, used: bool) {
        let i = (c - 2) as usize;
        let was = self.bitmap[i / 8] & (1 << (i % 8)) != 0;
        if was == used {
            return;
        }
        if used {
            self.bitmap[i / 8] |= 1 << (i % 8);
            self.free -= 1;
        } else {
            self.bitmap[i / 8] &= !(1 << (i % 8));
            self.free += 1;
        }
        self.bitmap_dirty.insert(i / 8 / SECTOR);
    }

    /// Allocates `n` clusters: contiguous if such a run exists, otherwise scattered.
    /// Returns the clusters and a "contiguous" flag.
    fn alloc(&mut self, n: usize) -> Result<(Vec<u32>, bool)> {
        if n == 0 {
            return Ok((Vec::new(), true));
        }
        if n as u32 > self.free {
            return Err(FsError::NoSpace);
        }
        let total = self.clusters + 2;
        // A contiguous run: search from the hint, then from the start.
        for start in [self.next_free.max(2), 2] {
            let mut run = 0usize;
            let mut c = start;
            while c < total {
                if self.is_free(c) {
                    run += 1;
                    if run == n {
                        let first = c + 1 - n as u32;
                        let out: Vec<u32> = (first..=c).collect();
                        for &k in &out {
                            self.mark(k, true);
                        }
                        self.next_free = c + 1;
                        return Ok((out, true));
                    }
                } else {
                    run = 0;
                }
                c += 1;
            }
        }
        let mut out = Vec::with_capacity(n);
        let mut c = 2;
        while out.len() < n && c < total {
            if self.is_free(c) {
                out.push(c);
            }
            c += 1;
        }
        if out.len() < n {
            return Err(FsError::NoSpace);
        }
        for &k in &out {
            self.mark(k, true);
        }
        Ok((out, false))
    }

    /// Links clusters into a FAT chain.
    fn link(&mut self, chain: &[u32]) -> Result<()> {
        for (i, &c) in chain.iter().enumerate() {
            let next = chain.get(i + 1).copied().unwrap_or(EOC);
            self.fat_set(c, next)?;
        }
        Ok(())
    }

    fn release(&mut self, e: Extent) -> Result<()> {
        self.cursor = None;
        if e.first == 0 {
            return Ok(());
        }
        let need = e.len.div_ceil(self.cluster_bytes());
        let mut c = e.first;
        // No cluster list: for a file of tens of gigabytes it would take all memory.
        for i in 0..need {
            if e.contiguous {
                self.mark(e.first + i as u32, false);
                continue;
            }
            if !self.valid_cluster(c) {
                break;
            }
            let next = self.fat_get(c)?;
            self.mark(c, false);
            self.fat_set(c, 0)?;
            c = next;
        }
        Ok(())
    }

    /// Cluster number `index` of an extent.
    fn cluster_at(&mut self, e: Extent, index: u64) -> Result<u32> {
        if e.contiguous {
            return Ok(e.first + index as u32);
        }
        let (mut at, mut c) = match self.cursor {
            Some((f, i, c)) if f == e.first && i <= index => (i, c),
            _ => (0, e.first),
        };
        while at < index {
            c = self.fat_get(c)?;
            if !self.valid_cluster(c) {
                return Err(FsError::Io);
            }
            at += 1;
        }
        self.cursor = Some((e.first, index, c));
        Ok(c)
    }

    /// Where on disk `len` bytes of the extent at `offset` lie.
    fn runs(&mut self, e: Extent, offset: u64, len: usize) -> Result<Vec<Run>> {
        let cb = self.cluster_bytes();
        let (mut pos, end) = (offset, offset + len as u64);
        let mut out = Vec::new();
        while pos < end {
            let c = self.cluster_at(e, pos / cb)?;
            let inner = pos % cb;
            let n = (cb - inner).min(end - pos);
            push_run(&mut out, self.cluster_lba(c) + (inner / SECTOR as u64) as u32, (inner % SECTOR as u64) as usize, n as usize);
            pos += n;
        }
        Ok(out)
    }

    /// Grows an extent to `need` clusters. A contiguous file stays contiguous while
    /// there is free space right after it, otherwise it gets a FAT chain.
    fn extend(&mut self, e: &mut Extent, have: u64, need: u64) -> Result<()> {
        let more = (need - have) as usize;
        if have == 0 {
            let (chain, contiguous) = self.alloc(more)?;
            if !contiguous {
                self.link(&chain)?;
            }
            (e.first, e.contiguous) = (chain[0], contiguous);
            return Ok(());
        }
        if e.contiguous {
            let next = e.first as u64 + have;
            let fits = next + more as u64 <= self.clusters as u64 + 2 && (0..more as u64).all(|k| self.is_free((next + k) as u32));
            if fits {
                for k in 0..more as u64 {
                    self.mark((next + k) as u32, true);
                }
                self.next_free = (next + more as u64) as u32;
                return Ok(());
            }
            for k in 0..have - 1 {
                self.fat_set(e.first + k as u32, e.first + k as u32 + 1)?;
            }
            self.fat_set(e.first + have as u32 - 1, EOC)?;
            e.contiguous = false;
        }
        let last = self.cluster_at(*e, have - 1)?;
        let (chain, _) = self.alloc(more)?;
        self.link(&chain)?;
        self.fat_set(last, chain[0])
    }

    /// Updates the file's directory entry after its length changed.
    fn save_node(&mut self, dir: Extent, n: &mut Node) -> Result<()> {
        Self::set_stream(&mut n.raw[1], n.data, n.valid);
        n.raw[0][12..16].copy_from_slice(&timestamp().to_le_bytes());
        let attr = le16(&n.raw[0], 4) | ATTR_ARCHIVE;
        n.raw[0][4..6].copy_from_slice(&attr.to_le_bytes());
        Self::set_checksum(&mut n.raw);
        let sectors = self.sectors(dir)?;
        for (k, e) in n.raw.iter().enumerate() {
            self.put_entry(&sectors, n.slot + k, e)?;
        }
        Ok(())
    }

    fn read_extent(&mut self, e: Extent, len: u64) -> Result<Vec<u8>> {
        let sectors = self.sectors(e)?;
        let mut out = Vec::with_capacity(len as usize);
        let mut i = 0;
        while i < sectors.len() && (out.len() as u64) < len {
            let mut j = i + 1;
            while j < sectors.len() && sectors[j] == sectors[j - 1] + 1 && ((j - i) * SECTOR) < (len as usize - out.len()) {
                j += 1;
            }
            self.load(sectors[i], (j - i) as u32)?;
            for &lba in &sectors[i..j] {
                let n = SECTOR.min(len as usize - out.len());
                out.extend_from_slice(&self.cache[&lba][..n]);
            }
            i = j;
        }
        Ok(out)
    }

    // ------------------------------------------------------------ names

    fn up(&self, u: u16) -> u16 {
        self.upcase.get(u as usize).copied().unwrap_or(u)
    }

    fn name_hash(&self, name: &[u16]) -> u16 {
        let mut h: u16 = 0;
        for &u in name {
            let u = self.up(u);
            for b in u.to_le_bytes() {
                h = (if h & 1 != 0 { 0x8000 } else { 0 }) + (h >> 1) + b as u16;
            }
        }
        h
    }

    fn same_name(&self, a: &str, b: &str) -> bool {
        let (a, b): (Vec<u16>, Vec<u16>) = (a.encode_utf16().collect(), b.encode_utf16().collect());
        a.len() == b.len() && a.iter().zip(&b).all(|(&x, &y)| self.up(x) == self.up(y))
    }

    // ------------------------------------------------------------ directories

    fn root_extent(&self) -> Extent {
        Extent { first: self.root, contiguous: false, len: u64::MAX }
    }

    /// All 32-byte directory entries and their sectors.
    fn dir_raw(&mut self, dir: Extent) -> Result<(Vec<u32>, Vec<u8>)> {
        let sectors = self.sectors(dir)?;
        let mut raw = Vec::with_capacity(sectors.len() * SECTOR);
        let mut i = 0;
        while i < sectors.len() {
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

    fn read_dir(&mut self, dir: Extent) -> Result<Vec<Node>> {
        let (_, raw) = self.dir_raw(dir)?;
        let entries: Vec<[u8; 32]> = raw.chunks_exact(32).map(|e| e.try_into().unwrap()).collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < entries.len() {
            let e = &entries[i];
            if e[0] == 0 {
                break; // no more entries after this
            }
            if e[0] != ENTRY_FILE {
                i += 1;
                continue;
            }
            let count = e[1] as usize + 1;
            if count < 3 || i + count > entries.len() || entries[i + 1][0] != ENTRY_STREAM {
                i += 1;
                continue;
            }
            let s = &entries[i + 1];
            let name_len = s[3] as usize;
            let mut units = Vec::with_capacity(name_len);
            for n in &entries[i + 2..i + count] {
                if n[0] != ENTRY_NAME {
                    break;
                }
                units.extend((0..15).map(|k| le16(n, 2 + 2 * k)));
            }
            units.truncate(name_len);
            out.push(Node {
                name: String::from_utf16_lossy(&units),
                attr: le16(e, 4),
                data: Extent { first: le32(s, 20), contiguous: s[1] & FLAG_NO_FAT_CHAIN != 0, len: le64(s, 24) },
                valid: le64(s, 8),
                slot: i,
                raw: entries[i..i + count].to_vec(),
            });
            i += count;
        }
        Ok(out)
    }

    /// Resolves a path: (parent directory, node); the root is None.
    fn find(&mut self, path: &str) -> Result<Option<(Extent, Node)>> {
        let mut dir = self.root_extent();
        let mut found = None;
        for part in path.split('/').filter(|s| !s.is_empty()) {
            if let Some((_, n)) = &found {
                let n: &Node = n;
                if !n.is_dir() {
                    return Err(FsError::NotADirectory);
                }
                dir = n.data;
            }
            let nodes = self.read_dir(dir)?;
            let n = nodes.into_iter().find(|n| self.same_name(&n.name, part)).ok_or(FsError::NotFound)?;
            found = Some((dir, n));
        }
        Ok(found)
    }

    fn dir_of(&mut self, path: &str) -> Result<Extent> {
        match self.find(path)? {
            None => Ok(self.root_extent()),
            Some((_, n)) if n.is_dir() => Ok(n.data),
            Some(_) => Err(FsError::NotADirectory),
        }
    }

    fn split(path: &str) -> Result<(&str, &str)> {
        let path = path.trim_end_matches('/');
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        if !super::fat::valid_name(name) {
            return Err(FsError::InvalidName);
        }
        Ok((parent, name))
    }

    fn put_entry(&mut self, sectors: &[u32], slot: usize, raw: &[u8; 32]) -> Result<()> {
        let at = slot * 32;
        let lba = sectors[at / SECTOR];
        self.sector_mut(lba)?[at % SECTOR..at % SECTOR + 32].copy_from_slice(raw);
        Ok(())
    }

    /// Checksum of a set (bytes 2–3 of the first entry are not counted).
    fn set_checksum(set: &mut [[u8; 32]]) {
        let mut sum: u16 = 0;
        for (i, e) in set.iter().enumerate() {
            for (k, &b) in e.iter().enumerate() {
                if i == 0 && (k == 2 || k == 3) {
                    continue;
                }
                sum = (if sum & 1 != 0 { 0x8000 } else { 0 }) + (sum >> 1) + b as u16;
            }
        }
        set[0][2..4].copy_from_slice(&sum.to_le_bytes());
    }

    /// A new set of entries for name `name`.
    fn make_set(&self, name: &str, attr: u16, data: Extent, valid: u64) -> Vec<[u8; 32]> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let names = units.len().div_ceil(15);
        let mut set = vec![[0u8; 32]; 2 + names];
        let stamp = timestamp();
        set[0][0] = ENTRY_FILE;
        set[0][1] = (1 + names) as u8;
        set[0][4..6].copy_from_slice(&attr.to_le_bytes());
        for at in [8, 12, 16] {
            set[0][at..at + 4].copy_from_slice(&stamp.to_le_bytes());
        }
        set[1][0] = ENTRY_STREAM;
        Self::set_stream(&mut set[1], data, valid);
        set[1][3] = units.len() as u8;
        set[1][4..6].copy_from_slice(&self.name_hash(&units).to_le_bytes());
        for (k, chunk) in units.chunks(15).enumerate() {
            let e = &mut set[2 + k];
            e[0] = ENTRY_NAME;
            for (j, &u) in chunk.iter().enumerate() {
                e[2 + 2 * j..4 + 2 * j].copy_from_slice(&u.to_le_bytes());
            }
        }
        Self::set_checksum(&mut set);
        set
    }

    fn set_stream(s: &mut [u8; 32], data: Extent, valid: u64) {
        s[1] = if data.first != 0 { FLAG_ALLOC } else { 0 } | if data.contiguous && data.first != 0 { FLAG_NO_FAT_CHAIN } else { 0 };
        s[8..16].copy_from_slice(&valid.to_le_bytes());
        s[20..24].copy_from_slice(&data.first.to_le_bytes());
        s[24..32].copy_from_slice(&data.len.to_le_bytes());
    }

    /// Finds room for `n` consecutive entries; if there is none, grows directory `dir_path`.
    fn place(&mut self, dir_path: &str, n: usize) -> Result<(Vec<u32>, usize)> {
        loop {
            let dir = self.dir_of(dir_path)?;
            let (sectors, raw) = self.dir_raw(dir)?;
            let mut run = 0;
            for (i, e) in raw.chunks_exact(32).enumerate() {
                if e[0] & IN_USE == 0 {
                    run += 1;
                    if run == n {
                        return Ok((sectors, i + 1 - n));
                    }
                } else {
                    run = 0;
                }
            }
            self.grow_dir(dir_path)?;
        }
    }

    /// Adds one (zeroed) cluster to a directory.
    fn grow_dir(&mut self, dir_path: &str) -> Result<()> {
        let dir = self.dir_of(dir_path)?;
        let mut chain = self.chain(dir)?;
        let last = *chain.last().ok_or(FsError::Io)?;
        // Try the next cluster first — then the directory stays contiguous.
        let (c, contiguous) = if self.valid_cluster(last + 1) && self.is_free(last + 1) && dir.contiguous {
            self.mark(last + 1, true);
            (last + 1, true)
        } else {
            let (v, _) = self.alloc(1)?;
            (v[0], false)
        };
        let lba = self.cluster_lba(c);
        for k in 0..self.spc() {
            self.sector_new(lba + k).fill(0);
        }
        chain.push(c);
        let Some((parent, mut node)) = self.find(dir_path)? else {
            // The root is always a FAT chain.
            return self.link(&chain);
        };
        if !contiguous || !dir.contiguous {
            self.link(&chain)?;
        }
        let len = chain.len() as u64 * self.cluster_bytes();
        node.data = Extent { first: chain[0], contiguous: contiguous && dir.contiguous, len };
        Self::set_stream(&mut node.raw[1], node.data, len);
        Self::set_checksum(&mut node.raw);
        let sectors = self.sectors(parent)?;
        for (k, e) in node.raw.clone().iter().enumerate() {
            self.put_entry(&sectors, node.slot + k, e)?;
        }
        Ok(())
    }

    fn add_set(&mut self, dir_path: &str, set: &[[u8; 32]]) -> Result<()> {
        let (sectors, slot) = self.place(dir_path, set.len())?;
        for (k, e) in set.iter().enumerate() {
            self.put_entry(&sectors, slot + k, e)?;
        }
        Ok(())
    }

    fn drop_set(&mut self, parent: Extent, node: &Node) -> Result<()> {
        let sectors = self.sectors(parent)?;
        for (k, e) in node.raw.iter().enumerate() {
            let mut e = *e;
            e[0] &= !IN_USE;
            self.put_entry(&sectors, node.slot + k, &e)?;
        }
        Ok(())
    }

    /// Writes data into new clusters. Returns their extent.
    fn store(&mut self, data: &[u8]) -> Result<Extent> {
        let cb = self.cluster_bytes() as usize;
        let n = data.len().div_ceil(cb);
        let (chain, contiguous) = self.alloc(n)?;
        if !contiguous {
            self.link(&chain)?;
        }
        let spc = self.spc() as usize;
        for (k, chunk) in data.chunks(SECTOR).enumerate() {
            let lba = self.cluster_lba(chain[k / spc]) + (k % spc) as u32;
            let s = self.sector_new(lba);
            s[..chunk.len()].copy_from_slice(chunk);
            s[chunk.len()..].fill(0);
        }
        Ok(Extent { first: chain.first().copied().unwrap_or(0), contiguous, len: data.len() as u64 })
    }

    fn remove_tree(&mut self, node: &Node) -> Result<()> {
        if node.is_dir() {
            for child in self.read_dir(node.data)? {
                self.remove_tree(&child)?;
            }
        }
        self.release(node.data)
    }

    // ------------------------------------------------------------ mounting

    /// Reads an exFAT volume (512-byte sectors). None — not exFAT or unreadable.
    pub fn mount(device: &Device, dev: usize) -> Option<Exfat> {
        let mut boot = [0u8; SECTOR];
        if !device.read(0, &mut boot) || &boot[3..11] != b"EXFAT   " || boot[510..512] != [0x55, 0xAA] {
            return None;
        }
        if boot[108] != 9 {
            crate::kprintln!("exfat: {}-byte sectors are not supported", 1u32 << boot[108]);
            return None;
        }
        let mut x = Exfat {
            dev,
            label: String::new(),
            fat_start: le32(&boot, 80),
            heap_start: le32(&boot, 88),
            spc_shift: boot[109] as u32,
            clusters: le32(&boot, 92),
            root: le32(&boot, 96),
            bitmap: Vec::new(),
            bitmap_at: Extent { first: 0, contiguous: false, len: 0 },
            bitmap_dirty: BTreeSet::new(),
            free: 0,
            next_free: 2,
            upcase: Vec::new(),
            cache: BTreeMap::new(),
            dirty: BTreeSet::new(),
            writing: false,
            cursor: None,
        };
        if x.spc_shift > 16 || x.clusters == 0 || !x.valid_cluster(x.root) {
            return None;
        }
        // While the device is not in DEVS yet, read through a temporary copy of its description.
        let probe = match device {
            Device::Usb { disk, start, sectors } => Device::Usb { disk: *disk, start: *start, sectors: *sectors },
            Device::Sata { disk, start, sectors } => Device::Sata { disk: *disk, start: *start, sectors: *sectors },
            _ => return None,
        };
        super::set_dev(dev, Some(probe));
        let ok = x.load_meta();
        super::set_dev(dev, None);
        x.cache.clear();
        x.dirty.clear();
        if !ok {
            crate::kprintln!("exfat: bad volume (no bitmap or up-case table)");
            return None;
        }
        Some(x)
    }

    /// The root: bitmap, up-case table, volume label.
    fn load_meta(&mut self) -> bool {
        let Ok(entries) = self.dir_raw(self.root_extent()).map(|(_, r)| r) else { return false };
        let mut upcase = None;
        for e in entries.chunks_exact(32) {
            match e[0] {
                0 => break,
                ENTRY_BITMAP if e[1] & 1 == 0 => {
                    self.bitmap_at = Extent { first: le32(e, 20), contiguous: false, len: le64(e, 24) };
                }
                ENTRY_UPCASE => upcase = Some(Extent { first: le32(e, 20), contiguous: false, len: le64(e, 24) }),
                ENTRY_LABEL => {
                    let n = (e[1] as usize).min(11);
                    let units: Vec<u16> = (0..n).map(|k| le16(e, 2 + 2 * k)).collect();
                    self.label = String::from_utf16_lossy(&units);
                }
                _ => {}
            }
        }
        let (Some(up), true) = (upcase, self.bitmap_at.first != 0) else { return false };
        // The bitmap and up-case table use the FAT (mkfs marks their chains); if there is no chain,
        // it is a contiguous run (some formatters write them that way).
        let fix = |x: &mut Self, e: Extent| -> Extent {
            match x.fat_get(e.first) {
                Ok(next) if next == EOC || x.valid_cluster(next) => e,
                _ => Extent { contiguous: true, ..e },
            }
        };
        self.bitmap_at = fix(self, self.bitmap_at);
        let up = fix(self, up);
        let Ok(bitmap) = self.read_extent(self.bitmap_at, self.bitmap_at.len) else { return false };
        let Ok(raw) = self.read_extent(up, up.len) else { return false };
        self.bitmap = bitmap;
        if self.bitmap.len() < (self.clusters as usize).div_ceil(8) {
            return false;
        }
        self.free = (2..self.clusters + 2).filter(|&c| self.is_free(c)).count() as u32;
        // The up-case table is compressed: 0xFFFF, N means N characters unchanged.
        let words: Vec<u16> = raw.chunks_exact(2).map(|w| u16::from_le_bytes([w[0], w[1]])).collect();
        let mut table = Vec::with_capacity(0x10000);
        let mut i = 0;
        while i < words.len() && table.len() < 0x10000 {
            if words[i] == 0xFFFF && i + 1 < words.len() {
                let n = words[i + 1] as usize;
                let start = table.len();
                table.extend((start..start + n).map(|c| c as u16));
                i += 2;
            } else {
                table.push(words[i]);
                i += 1;
            }
        }
        self.upcase = table;
        true
    }
}

/// Date and time in exFAT format (as in FAT: the date in the upper 16 bits).
fn timestamp() -> u32 {
    let t = crate::time::now();
    let date = ((t.year.saturating_sub(1980) as u32) << 9) | (t.month as u32) << 5 | t.day as u32;
    let time = (t.hour as u32) << 11 | (t.min as u32) << 5 | (t.sec as u32 / 2);
    date << 16 | time
}

impl Filesystem for Exfat {
    fn list(&mut self, path: &str) -> Result<Vec<Entry>> {
        let dir = self.dir_of(path)?;
        let nodes = self.read_dir(dir)?;
        let mut out = Vec::with_capacity(nodes.len());
        for n in nodes {
            let size = if n.is_dir() { self.read_dir(n.data).map_or(0, |c| c.len()) } else { n.data.len as usize };
            out.push(Entry { name: n.name, is_dir: n.attr & ATTR_DIR != 0, size });
        }
        out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(out)
    }

    fn read(&mut self, path: &str) -> Result<Vec<u8>> {
        match self.find(path)? {
            Some((_, n)) if !n.is_dir() => {
                // Beyond ValidDataLength there are zeros (space allocated but not written).
                let mut data = self.read_extent(n.data, n.valid.min(n.data.len))?;
                data.resize(n.data.len as usize, 0);
                Ok(data)
            }
            _ => Err(FsError::IsADirectory),
        }
    }

    fn read_range(&mut self, path: &str, offset: u64, len: usize) -> Result<(Vec<u8>, u64)> {
        let Some((_, n)) = self.find(path)? else { return Err(FsError::IsADirectory) };
        if n.is_dir() {
            return Err(FsError::IsADirectory);
        }
        let size = n.data.len;
        let want = (len as u64).min(size.saturating_sub(offset)) as usize;
        let mut out = Vec::with_capacity(want);
        for run in self.runs(n.data, offset, want)? {
            read_run(self, &run, &mut out)?;
        }
        // Beyond ValidDataLength there are zeros.
        let valid = n.valid.saturating_sub(offset) as usize;
        if valid < out.len() {
            out[valid..].fill(0);
        }
        Ok((out, size))
    }

    fn write_at(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<()> {
        let Some((dir, mut n)) = self.find(path)? else { return Err(FsError::IsADirectory) };
        if n.is_dir() {
            return Err(FsError::IsADirectory);
        }
        if data.is_empty() {
            return Ok(());
        }
        let end = offset + data.len() as u64;
        let cb = self.cluster_bytes();
        let (have, need) = (n.data.len.div_ceil(cb), end.div_ceil(cb));
        if need > have {
            self.extend(&mut n.data, have, need)?;
        }
        let area = Extent { len: end.max(n.data.len), ..n.data };
        let mut at = 0;
        for run in self.runs(area, offset, data.len())? {
            write_run(self, &run, &data[at..at + run.bytes])?;
            at += run.bytes;
        }
        n.data = area;
        n.valid = n.valid.max(end);
        self.save_node(dir, &mut n)
    }

    fn shrink(&mut self, path: &str, len: u64) -> Result<()> {
        let Some((dir, mut n)) = self.find(path)? else { return Err(FsError::IsADirectory) };
        if n.is_dir() {
            return Err(FsError::IsADirectory);
        }
        if len >= n.data.len {
            return Ok(());
        }
        let cb = self.cluster_bytes();
        let keep = len.div_ceil(cb);
        if keep == 0 {
            self.release(n.data)?;
            n.data = Extent { first: 0, contiguous: false, len: 0 };
        } else {
            let last = self.cluster_at(n.data, keep - 1)?;
            let tail = if n.data.contiguous { last + 1 } else { self.fat_get(last)? };
            let tail_len = n.data.len - keep * cb;
            if tail_len > 0 {
                self.release(Extent { first: tail, contiguous: n.data.contiguous, len: tail_len })?;
            }
            if !n.data.contiguous {
                self.fat_set(last, EOC)?;
            }
            n.data.len = len;
        }
        n.valid = n.valid.min(len);
        self.save_node(dir, &mut n)
    }

    fn pending_bytes(&self) -> usize {
        (self.dirty.len() + self.bitmap_dirty.len()) * SECTOR
    }

    fn is_dir(&mut self, path: &str) -> Option<bool> {
        match self.find(path).ok()? {
            None => Some(true),
            Some((_, n)) => Some(n.is_dir()),
        }
    }

    fn write(&mut self, path: &str, data: &[u8]) -> Result<()> {
        let (parent, name) = Self::split(path)?;
        let dir = self.dir_of(parent)?;
        let existing = self.read_dir(dir)?.into_iter().find(|n| self.same_name(&n.name, name));
        if existing.as_ref().is_some_and(|n| n.is_dir()) {
            return Err(FsError::IsADirectory);
        }
        if let Some(old) = &existing {
            self.release(old.data)?;
        }
        let data_at = self.store(data)?;
        match existing {
            Some(mut n) => {
                // The same set of entries: new clusters, length and modification time.
                Self::set_stream(&mut n.raw[1], data_at, data.len() as u64);
                n.raw[0][12..16].copy_from_slice(&timestamp().to_le_bytes());
                let attr = le16(&n.raw[0], 4) | ATTR_ARCHIVE;
                n.raw[0][4..6].copy_from_slice(&attr.to_le_bytes());
                Self::set_checksum(&mut n.raw);
                let sectors = self.sectors(dir)?;
                for (k, e) in n.raw.iter().enumerate() {
                    self.put_entry(&sectors, n.slot + k, e)?;
                }
                Ok(())
            }
            None => {
                let set = self.make_set(name, ATTR_ARCHIVE, data_at, data.len() as u64);
                self.add_set(parent, &set)
            }
        }
    }

    fn mkdir(&mut self, path: &str) -> Result<()> {
        let (parent, name) = Self::split(path)?;
        let dir = self.dir_of(parent)?;
        if self.read_dir(dir)?.iter().any(|n| self.same_name(&n.name, name)) {
            return Err(FsError::AlreadyExists);
        }
        let (c, _) = self.alloc(1)?;
        let lba = self.cluster_lba(c[0]);
        for k in 0..self.spc() {
            self.sector_new(lba + k).fill(0);
        }
        let len = self.cluster_bytes();
        let set = self.make_set(name, ATTR_DIR, Extent { first: c[0], contiguous: true, len }, len);
        self.add_set(parent, &set)
    }

    fn remove(&mut self, path: &str) -> Result<()> {
        match self.find(path)? {
            None => Err(FsError::InvalidName),
            Some((parent, n)) => {
                self.remove_tree(&n)?;
                self.drop_set(parent, &n)
            }
        }
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let Some((src_dir, n)) = self.find(from)? else { return Err(FsError::InvalidName) };
        let (parent, name) = Self::split(to)?;
        let dst_dir = self.dir_of(parent)?;
        if let Some(other) = self.read_dir(dst_dir)?.into_iter().find(|x| self.same_name(&x.name, name))
            && !(dst_dir == src_dir && other.slot == n.slot)
        {
            return Err(FsError::AlreadyExists);
        }
        // A new set with the same contents: attributes and times from the old one.
        let mut set = self.make_set(name, n.attr, n.data, n.valid);
        set[0][4..24].copy_from_slice(&n.raw[0][4..24]);
        set[1][1] = n.raw[1][1];
        Self::set_checksum(&mut set);
        self.drop_set(src_dir, &n)?;
        self.add_set(parent, &set)
    }

    fn usage(&self) -> (u64, u64) {
        let cb = self.cluster_bytes();
        ((self.clusters - self.free) as u64 * cb, self.clusters as u64 * cb)
    }

    fn has_changes(&self) -> bool {
        !self.dirty.is_empty() || !self.bitmap_dirty.is_empty()
    }

    fn take_dirty(&mut self) -> Vec<(u32, Sector)> {
        // Changed pieces of the bitmap go to its sectors.
        if !self.bitmap_dirty.is_empty()
            && let Ok(sectors) = self.sectors(Extent { len: self.bitmap.len() as u64, ..self.bitmap_at })
        {
            for idx in core::mem::take(&mut self.bitmap_dirty) {
                let Some(&lba) = sectors.get(idx) else { continue };
                let from = idx * SECTOR;
                let to = (from + SECTOR).min(self.bitmap.len());
                let chunk = self.bitmap[from..to].to_vec();
                let s = self.sector_new(lba);
                s[..chunk.len()].copy_from_slice(&chunk);
            }
        }
        let mut out: Vec<(u32, Sector)> = Vec::new();
        for lba in core::mem::take(&mut self.dirty) {
            if let Some(s) = self.cache.get(&lba) {
                out.push((lba, s.clone()));
            }
        }
        out
    }

    fn set_writing(&mut self, on: bool) {
        self.writing = on;
    }

    fn trim(&mut self) {
        if self.writing || self.cache.len() <= CACHE_LIMIT {
            return;
        }
        let dirty = &self.dirty;
        self.cache.retain(|lba, _| dirty.contains(lba));
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn dev(&self) -> usize {
        self.dev
    }

    fn kind(&self) -> &'static str {
        "exFAT"
    }
}
