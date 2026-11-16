//! USB sticks and disks: the Mass Storage class, the Bulk-Only (BOT) protocol, SCSI commands.
//! Each command is a CBW block, data and a CSW answer over two bulk endpoints.

use super::xhci::{self, Ring, Xhci, dma};
use alloc::alloc::{Layout, alloc_zeroed};
use crate::interrupts::HZ;

/// The data buffer: 64 KiB, aligned to 64 KiB — one TRB does not cross a boundary.
const BOUNCE: usize = 64 * 1024;

pub struct Storage {
    /// The disk number for the FS (never reused).
    pub id: u32,
    pub slot: u8,
    pub loc: super::Loc,
    in_dci: u32,
    out_dci: u32,
    in_ring: Ring,
    out_ring: Ring,
    bounce: *mut u8,
    cmd: *mut u8,
    tag: u32,
    pub blocks: u64,
    pub block_size: u32,
}

unsafe impl Send for Storage {}

impl Storage {
    pub fn new(id: u32, slot: u8, loc: super::Loc, in_dci: u32, out_dci: u32, in_ring: Ring, out_ring: Ring) -> Storage {
        Storage {
            id,
            slot,
            loc,
            in_dci,
            out_dci,
            in_ring,
            out_ring,
            bounce: unsafe { alloc_zeroed(Layout::from_size_align(BOUNCE, BOUNCE).unwrap()) },
            cmd: dma(64),
            tag: 1,
            blocks: 0,
            block_size: 512,
        }
    }

    fn bulk(&mut self, hc: &mut Xhci, input: bool, buf: *mut u8, len: usize) -> Option<usize> {
        let (ring, dci) = if input { (&mut self.in_ring, self.in_dci) } else { (&mut self.out_ring, self.out_dci) };
        let addr = ring.push(buf as u64, len as u32, xhci::TRB_NORMAL << 10 | 1 << 5 | 1 << 2);
        hc.ring_doorbell(self.slot, dci);
        let slot = self.slot;
        let e = hc.wait_event_for(5 * HZ, |e| e.kind() == xhci::EV_TRANSFER && e.slot() == slot && e.pointer() == addr)?;
        match e.code() {
            xhci::SUCCESS | xhci::SHORT_PACKET => Some(len - (e.0[2] & 0xFF_FFFF) as usize),
            _ => {
                // STALL and other errors: the endpoint must be reset, otherwise it stays halted.
                let deq = ring.dequeue();
                hc.recover_endpoint(slot, dci, deq);
                None
            }
        }
    }

    /// A SCSI command: block `cb`, then `len` bytes of data (in the bounce buffer), then the status.
    fn scsi(&mut self, hc: &mut Xhci, cb: &[u8], input: bool, len: usize) -> bool {
        let tag = self.tag;
        self.tag = self.tag.wrapping_add(1);
        let cbw = unsafe { core::slice::from_raw_parts_mut(self.cmd, 31) };
        cbw.fill(0);
        cbw[0..4].copy_from_slice(b"USBC");
        cbw[4..8].copy_from_slice(&tag.to_le_bytes());
        cbw[8..12].copy_from_slice(&(len as u32).to_le_bytes());
        cbw[12] = if input { 0x80 } else { 0 };
        cbw[14] = cb.len() as u8;
        cbw[15..15 + cb.len()].copy_from_slice(cb);
        let cmd = self.cmd;
        if self.bulk(hc, false, cmd, 31).is_none() {
            return false;
        }
        if len > 0 {
            let bounce = self.bounce;
            let _ = self.bulk(hc, input, bounce, len);
        }
        let csw_buf = unsafe { self.cmd.add(32) };
        if self.bulk(hc, true, csw_buf, 13).is_none() {
            return false;
        }
        let csw = unsafe { core::slice::from_raw_parts(csw_buf, 13) };
        &csw[0..4] == b"USBS" && csw[4..8] == tag.to_le_bytes() && csw[12] == 0
    }

    /// Waits for the medium to be ready and learns its size. false — it failed.
    pub fn start(&mut self, hc: &mut Xhci) -> bool {
        let _ = self.scsi(hc, &[0x12, 0, 0, 0, 36, 0], true, 36); // INQUIRY
        for _ in 0..10 {
            if self.scsi(hc, &[0; 6], false, 0) {
                break; // TEST UNIT READY
            }
            let _ = self.scsi(hc, &[0x03, 0, 0, 0, 18, 0], true, 18); // REQUEST SENSE
        }
        if !self.scsi(hc, &[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], true, 8) {
            return false;
        }
        let b = unsafe { core::slice::from_raw_parts(self.bounce, 8) };
        self.blocks = u32::from_be_bytes(b[0..4].try_into().unwrap()) as u64 + 1;
        self.block_size = u32::from_be_bytes(b[4..8].try_into().unwrap());
        self.block_size == 512
    }

    /// Reads or writes 512-byte sectors, in pieces of up to 64 KiB.
    pub fn transfer(&mut self, hc: &mut Xhci, lba: u32, buf: &mut [u8], write: bool) -> bool {
        let per = BOUNCE / 512;
        for (k, chunk) in buf.chunks_mut(BOUNCE).enumerate() {
            let at = lba + (k * per) as u32;
            let count = chunk.len().div_ceil(512) as u16;
            let mut cb = [0u8; 10];
            cb[0] = if write { 0x2A } else { 0x28 };
            cb[2..6].copy_from_slice(&at.to_be_bytes());
            cb[7..9].copy_from_slice(&count.to_be_bytes());
            let bytes = count as usize * 512;
            let bounce = unsafe { core::slice::from_raw_parts_mut(self.bounce, bytes) };
            if write {
                bounce[..chunk.len()].copy_from_slice(chunk);
                bounce[chunk.len()..].fill(0);
            }
            if !self.scsi(hc, &cb, !write, bytes) {
                return false;
            }
            if !write {
                chunk.copy_from_slice(&bounce[..chunk.len()]);
            }
        }
        true
    }
}
