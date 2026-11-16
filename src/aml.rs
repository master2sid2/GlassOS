//! Reading the DSDT without an AML interpreter: only what the table states explicitly —
//! names with numbers, memory regions, the fields in them and packages of numbers (ACPI 6.4, chapter 20).
//! Methods are not executed: if a value is computed by a method, it cannot be obtained here.

use alloc::vec::Vec;

const NAME_OP: u8 = 0x08;
const PACKAGE_OP: u8 = 0x12;
const EXT_PREFIX: u8 = 0x5B;
const OP_REGION: u8 = 0x80;
const FIELD_OP: u8 = 0x81;
const ROOT: u8 = 0x5C;

/// The package length (PkgLength): (the value, where the next item starts).
fn pkglen(b: &[u8], i: usize) -> Option<(usize, usize)> {
    let lead = *b.get(i)?;
    let extra = (lead >> 6) as usize;
    if extra == 0 {
        return Some(((lead & 0x3F) as usize, i + 1));
    }
    let mut v = (lead & 0x0F) as usize;
    for k in 0..extra {
        v |= (*b.get(i + 1 + k)? as usize) << (4 + 8 * k);
    }
    Some((v, i + 1 + extra))
}

/// A number: Zero, One, Ones or a 1, 2, 4 or 8-byte constant.
fn integer(b: &[u8], i: usize) -> Option<(u64, usize)> {
    let read = |n: usize| -> Option<u64> { Some(b.get(i + 1..i + 1 + n)?.iter().rev().fold(0u64, |v, &x| v << 8 | x as u64)) };
    match *b.get(i)? {
        0x00 => Some((0, i + 1)),
        0x01 => Some((1, i + 1)),
        0xFF => Some((u64::MAX, i + 1)),
        0x0A => Some((read(1)?, i + 2)),
        0x0B => Some((read(2)?, i + 3)),
        0x0C => Some((read(4)?, i + 5)),
        0x0E => Some((read(8)?, i + 9)),
        _ => None,
    }
}

fn positions<'a>(d: &'a [u8], pattern: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
    d.windows(pattern.len()).enumerate().filter(move |(_, w)| *w == pattern).map(|(i, _)| i)
}

/// `Name (NAME, number)`.
pub fn name_int(d: &[u8], name: &[u8; 4]) -> Option<u64> {
    positions(d, &[&[NAME_OP][..], name].concat()).find_map(|at| integer(d, at + 5).map(|(v, _)| v))
}

/// A number or a name with a number (this is how region addresses and lengths are given).
fn term(d: &[u8], i: usize) -> Option<(u64, usize)> {
    if let Some(v) = integer(d, i) {
        return Some(v);
    }
    let name: [u8; 4] = d.get(i..i + 4)?.try_into().ok()?;
    name.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_').then_some(())?;
    Some((name_int(d, &name)?, i + 4))
}

/// `OperationRegion (NAME, SystemMemory, address, length)`: the address and length.
pub fn memory_region(d: &[u8], name: &[u8; 4]) -> Option<(u64, u64)> {
    positions(d, &[&[EXT_PREFIX, OP_REGION][..], name].concat()).find_map(|at| {
        // Space 0 is memory.
        (*d.get(at + 6)? == 0).then_some(())?;
        let (addr, next) = term(d, at + 7)?;
        let (len, _) = term(d, next)?;
        Some((addr, len))
    })
}

/// Field `field` in the `Field (region, …)` lists: the offset in bytes and the width in bits.
pub fn field(d: &[u8], region: &[u8; 4], field: &[u8; 4]) -> Option<(u64, u32)> {
    for at in positions(d, &[EXT_PREFIX, FIELD_OP]) {
        let Some((len, mut i)) = pkglen(d, at + 2) else { continue };
        let end = (at + 2 + len).min(d.len());
        if d.get(i) == Some(&ROOT) {
            i += 1;
        }
        if d.get(i..i + 4) != Some(&region[..]) {
            continue;
        }
        // The region name and the field flags.
        i += 5;
        let mut bit = 0u64;
        while i < end {
            match d[i] {
                // Skipped bits (Offset).
                0x00 => {
                    let Some((n, next)) = pkglen(d, i + 1) else { break };
                    bit += n as u64;
                    i = next;
                }
                // The access type: does not affect the offset.
                0x01 => i += 3,
                0x03 => i += 4,
                c if c.is_ascii_uppercase() || c == b'_' => {
                    let name = &d[i..(i + 4).min(end)];
                    let Some((n, next)) = pkglen(d, i + 4) else { break };
                    if name == field {
                        // A field inside a byte is not needed: whole bytes are read.
                        return (bit % 8 == 0).then_some((bit / 8, n as u32));
                    }
                    bit += n as u64;
                    i = next;
                }
                _ => break,
            }
        }
    }
    None
}

/// `Name (NAME, Package () { Package () { numbers… }, … })`.
pub fn packages(d: &[u8], name: &[u8; 4]) -> Option<Vec<Vec<u64>>> {
    positions(d, &[&[NAME_OP][..], name, &[PACKAGE_OP]].concat()).find_map(|at| {
        let (len, i) = pkglen(d, at + 6)?;
        let end = (at + 6 + len).min(d.len());
        let count = *d.get(i)? as usize;
        let mut i = i + 1;
        let mut out = Vec::new();
        while out.len() < count && i < end {
            (*d.get(i)? == PACKAGE_OP).then_some(())?;
            let (inner_len, j) = pkglen(d, i + 1)?;
            let inner_end = i + 1 + inner_len;
            let n = *d.get(j)? as usize;
            let mut k = j + 1;
            let mut row = Vec::new();
            while row.len() < n && k < inner_end {
                let (v, next) = integer(d, k)?;
                row.push(v);
                k = next;
            }
            out.push(row);
            i = inner_end;
        }
        (out.len() == count).then_some(out)
    })
}
