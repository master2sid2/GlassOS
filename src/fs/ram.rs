//! An in-memory volume. Memory is taken as data is written, so an unfilled disk
//! costs nothing, and sectors that were never written read as zeros.

use super::{READ_RESERVE, SECTOR};
use alloc::vec::Vec;

/// The heap looks for blocks from this size up in high memory: files will not take the low memory.
const CHUNK: usize = crate::BIG_BLOCK;

pub struct RamDisk {
    sectors: u32,
    /// An empty element is a piece that was never written.
    chunks: Vec<Vec<u8>>,
}

impl RamDisk {
    pub fn new(sectors: usize) -> Self {
        let count = (sectors * SECTOR).div_ceil(CHUNK);
        Self { sectors: sectors as u32, chunks: (0..count).map(|_| Vec::new()).collect() }
    }

    pub fn sectors(&self) -> u32 {
        self.sectors
    }

    fn fits(&self, at: usize, len: usize) -> bool {
        at + len <= self.sectors as usize * SECTOR
    }

    pub fn read(&self, lba: u32, buf: &mut [u8]) -> bool {
        let at = lba as usize * SECTOR;
        if !self.fits(at, buf.len()) {
            return false;
        }
        let mut done = 0;
        while done < buf.len() {
            let (index, offset) = ((at + done) / CHUNK, (at + done) % CHUNK);
            let n = (CHUNK - offset).min(buf.len() - done);
            match self.chunks[index].get(offset..offset + n) {
                Some(src) => buf[done..done + n].copy_from_slice(src),
                None => buf[done..done + n].fill(0),
            }
            done += n;
        }
        true
    }

    /// Gives the heap back the pieces lying entirely in these sectors.
    pub fn discard(&mut self, lba: u32, sectors: u32) {
        let (start, end) = (lba as usize * SECTOR, (lba + sectors) as usize * SECTOR);
        for chunk in &mut self.chunks[start.div_ceil(CHUNK)..(end / CHUNK).max(start.div_ceil(CHUNK))] {
            *chunk = Vec::new();
        }
    }

    /// false — the disk is full or there is no room left in the heap.
    pub fn write(&mut self, lba: u32, data: &[u8]) -> bool {
        let at = lba as usize * SECTOR;
        if !self.fits(at, data.len()) {
            return false;
        }
        let mut done = 0;
        while done < data.len() {
            let (index, offset) = ((at + done) / CHUNK, (at + done) % CHUNK);
            let n = (CHUNK - offset).min(data.len() - done);
            let part = &data[done..done + n];
            done += n;
            let chunk = &mut self.chunks[index];
            if chunk.is_empty() {
                // Zeros are read there anyway: formatting takes no memory.
                if part.iter().all(|&b| b == 0) {
                    continue;
                }
                if crate::heap_stats().1 < CHUNK + READ_RESERVE || chunk.try_reserve_exact(CHUNK).is_err() {
                    return false;
                }
                chunk.resize(CHUNK, 0);
            }
            chunk[offset..offset + n].copy_from_slice(part);
        }
        true
    }
}
