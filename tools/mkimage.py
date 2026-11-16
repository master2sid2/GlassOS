"""Builds a GlassOS boot image for a USB stick (and for QEMU without helper disks).

python3 tools/mkimage.py [target/glassos.img] [size in MiB, 64 by default]

The image is a GPT disk with one EFI partition (FAT32): \\EFI\\BOOT\\BOOTX64.EFI (the kernel),
\\apps (Lua programs) and \\bin (ring 3 programs). It can be written to a USB stick
(Rufus in DD mode, balenaEtcher, dd) and booted in UEFI mode.
No root rights are needed: FAT32 and GPT are built right here.
"""
import os, struct, sys, uuid, zlib

SECTOR = 512
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
out = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, 'target', 'glassos.img')
size_mib = int(sys.argv[2]) if len(sys.argv) > 2 else 64

total = size_mib * 2048
part_start = 2048
part_end = total - 34          # the last sector of the partition (the backup GPT follows it)
part_sectors = part_end - part_start + 1

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import fatimage

img = bytearray(total * SECTOR)
volume, used, clusters = fatimage.build(fatimage.system_tree(), part_sectors, 32, hidden=part_start)
img[part_start * SECTOR:part_start * SECTOR + len(volume)] = volume

# ---------------------------------------------------------------- GPT

ESP = uuid.UUID('C12A7328-F81F-11D2-BA4B-00A0C93EC93B')
disk_guid, part_guid = uuid.uuid4(), uuid.uuid4()
part = struct.pack('<16s16sQQQ72s', ESP.bytes_le, part_guid.bytes_le, part_start, part_end, 0,
                   'GlassOS'.encode('utf-16-le').ljust(72, b'\0'))
entries = part.ljust(128 * 128, b'\0')
entries_crc = zlib.crc32(entries) & 0xFFFFFFFF

def gpt_header(current, backup, entries_lba):
    h = struct.pack('<8sIIIIQQQQ16sQIII', b'EFI PART', 0x00010000, 92, 0, 0, current, backup,
                    34, total - 34, disk_guid.bytes_le, entries_lba, 128, 128, entries_crc)
    crc = zlib.crc32(h) & 0xFFFFFFFF
    return (h[:16] + struct.pack('<I', crc) + h[20:]).ljust(SECTOR, b'\0')

mbr = bytearray(SECTOR)
mbr[446:462] = struct.pack('<BBBBBBBBII', 0, 0, 2, 0, 0xEE, 0xFF, 0xFF, 0xFF, 1, min(total - 1, 0xFFFFFFFF))
mbr[510:512] = b'\x55\xAA'
img[0:SECTOR] = mbr
img[SECTOR:2 * SECTOR] = gpt_header(1, total - 1, 2)
img[2 * SECTOR:34 * SECTOR] = entries
img[(total - 33) * SECTOR:(total - 1) * SECTOR] = entries
img[(total - 1) * SECTOR:] = gpt_header(total - 1, 1, total - 33)

open(out, 'wb').write(img)
print(f'{out}: {size_mib} MiB, GPT + EFI partition (FAT32, {used} of {clusters} clusters used)')
