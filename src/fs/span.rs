//! Reading and writing large pieces of a file. Whole sectors go straight to the device,
//! bypassing the cache (in it each sector is a separate memory block), so the file size
//! is not limited by memory, and the speed is not limited by an allocation per 512 bytes.

use super::{DEVS, Result, SECTOR, FsError};
use alloc::vec;
use alloc::vec::Vec;

/// A piece on disk: the sector, the offset in it and the length in bytes.
pub struct Run {
    pub lba: u32,
    pub skip: usize,
    pub bytes: usize,
}

/// Adds a piece to the list; a continuation of the previous one is merged with it.
pub fn push_run(runs: &mut Vec<Run>, lba: u32, skip: usize, bytes: usize) {
    if let Some(r) = runs.last_mut()
        && r.lba as u64 * SECTOR as u64 + (r.skip + r.bytes) as u64 == lba as u64 * SECTOR as u64 + skip as u64
    {
        r.bytes += bytes;
        return;
    }
    runs.push(Run { lba, skip, bytes });
}

/// What is needed from the file system: its sector cache and the device number.
pub trait Sectors {
    fn read_sector(&mut self, lba: u32) -> Result<&[u8; SECTOR]>;
    fn update_sector(&mut self, lba: u32) -> Result<&mut [u8; SECTOR]>;
    fn replace_sector(&mut self, lba: u32) -> &mut [u8; SECTOR];
    /// None of the `n` sectors is in the cache.
    fn uncached(&self, lba: u32, n: u32) -> bool;
    /// A background cache write is in progress: writing past it is not allowed, the old copy would overwrite the new one.
    fn flushing(&self) -> bool;
    fn device(&self) -> usize;
}

/// How many sectors per command to the device.
const BURST: usize = 255;

fn device_read(dev: usize, lba: u32, buf: &mut [u8]) -> bool {
    let devs = DEVS.lock();
    let Some(Some(d)) = devs.get(dev) else { return false };
    buf.chunks_mut(BURST * SECTOR).enumerate().all(|(i, part)| d.read(lba + (i * BURST) as u32, part))
}

fn device_write(dev: usize, lba: u32, data: &[u8]) -> bool {
    let mut devs = DEVS.lock();
    let Some(Some(d)) = devs.get_mut(dev) else { return false };
    data.chunks(BURST * SECTOR).enumerate().all(|(i, part)| d.write_data(lba + (i * BURST) as u32, part))
}

pub fn write_run<S: Sectors>(s: &mut S, run: &Run, data: &[u8]) -> Result<()> {
    let (mut lba, mut rest) = (run.lba, data);
    if run.skip > 0 {
        let n = (SECTOR - run.skip).min(rest.len());
        s.update_sector(lba)?[run.skip..run.skip + n].copy_from_slice(&rest[..n]);
        rest = &rest[n..];
        lba += 1;
    }
    let whole = rest.len() / SECTOR;
    if whole > 0 {
        let (head, tail) = rest.split_at(whole * SECTOR);
        if !s.flushing() && s.uncached(lba, whole as u32) {
            if !device_write(s.device(), lba, head) {
                return Err(FsError::Io);
            }
        } else {
            for (k, part) in head.chunks_exact(SECTOR).enumerate() {
                s.replace_sector(lba + k as u32).copy_from_slice(part);
            }
        }
        lba += whole as u32;
        rest = tail;
    }
    if !rest.is_empty() {
        s.update_sector(lba)?[..rest.len()].copy_from_slice(rest);
    }
    Ok(())
}

pub fn read_run<S: Sectors>(s: &mut S, run: &Run, out: &mut Vec<u8>) -> Result<()> {
    let count = (run.skip + run.bytes).div_ceil(SECTOR);
    if s.uncached(run.lba, count as u32) {
        let mut buf = vec![0u8; count * SECTOR];
        if !device_read(s.device(), run.lba, &mut buf) {
            return Err(FsError::Io);
        }
        out.extend_from_slice(&buf[run.skip..run.skip + run.bytes]);
        return Ok(());
    }
    let mut left = run.bytes;
    let mut skip = run.skip;
    for k in 0..count {
        let n = (SECTOR - skip).min(left);
        out.extend_from_slice(&s.read_sector(run.lba + k as u32)?[skip..skip + n]);
        left -= n;
        skip = 0;
    }
    Ok(())
}
