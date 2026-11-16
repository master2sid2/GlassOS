//! The old disk format (a snapshot of the whole tree in one of two slots) — read
//! only: on the first boot with FAT32 the files are moved out of it.

use super::Device;
use crate::ata::SECTOR;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

const MAGIC: &[u8; 8] = b"MYOSFS01";

pub enum Node {
    Dir(BTreeMap<String, Node>),
    File(Vec<u8>),
}

fn fnv1a(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf29ce484222325, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Some(head)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
}

fn deserialize(r: &mut Reader) -> Option<Node> {
    match r.take(1)?[0] {
        b'D' => {
            let mut children = BTreeMap::new();
            for _ in 0..r.u32()? {
                let len = r.u16()? as usize;
                let name = String::from_utf8(r.take(len)?.to_vec()).ok()?;
                children.insert(name, deserialize(r)?);
            }
            Some(Node::Dir(children))
        }
        b'F' => {
            let len = r.u32()? as usize;
            Some(Node::File(r.take(len)?.to_vec()))
        }
        _ => None,
    }
}

/// Looks for the newest complete snapshot. Slot B lay at half the disk; the disk may have
/// grown since then, so both half of the current size and the old 16 MiB are tried.
pub fn load(dev: &Device) -> Option<Node> {
    let mut best: Option<(u64, Node)> = None;
    let mut slots = vec![0, dev.sectors() / 2, 16 * 2048 / 2];
    slots.dedup();
    for lba in slots {
        let mut header = [0u8; SECTOR];
        if lba >= dev.sectors() || !dev.read(lba, &mut header) || &header[..8] != MAGIC {
            continue;
        }
        let field = |i: usize| u64::from_le_bytes(header[i..i + 8].try_into().unwrap());
        let (seq, len, hash) = (field(8), field(16) as usize, field(24));
        if len > 64 << 20 || best.as_ref().is_some_and(|(s, _)| *s > seq) {
            continue;
        }
        let mut data = vec![0u8; len.div_ceil(SECTOR) * SECTOR];
        if !dev.read(lba + 1, &mut data) || fnv1a(&data[..len]) != hash {
            continue;
        }
        if let Some(node @ Node::Dir(_)) = deserialize(&mut Reader(&data[..len])) {
            best = Some((seq, node));
        }
    }
    best.map(|(_, node)| node)
}
