"""Builds a FAT16/FAT32 volume from a host file tree — without root or external tools.

build(tree, sectors, fat_type) -> bytearray: tree = {name: path to a file | nested dict}.
Names are 8.3; the case (lowercase letters) is kept with flags, as Windows does.
"""
import os, struct, time, zlib

SECTOR = 512
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def system_tree():
    """What goes on the boot medium: the kernel, Lua programs and ring 3 programs."""
    def files(d, only_exec=False):
        return {n: os.path.join(d, n) for n in sorted(os.listdir(d))
                if os.path.isfile(os.path.join(d, n)) and not n.endswith('.d')
                and (not only_exec or os.access(os.path.join(d, n), os.X_OK))}
    return {
        'EFI': {'BOOT': {'BOOTX64.EFI': os.path.join(ROOT, 'target', 'x86_64-glassos', 'release', 'kernel.efi')}},
        'apps': files(os.path.join(ROOT, 'apps')),
        'bin': files(os.path.join(ROOT, 'user', 'target', 'x86_64-unknown-none', 'release'), only_exec=True),
        'firmware': {n: os.path.join(ROOT, 'assets', 'firmware', n) for n in ('quz77.fw', 'rtw8723b.fw', 'rtbt8723.fw', 'ibt19.sfi', 'ibt19.ddc')},
    }


def short_name(name):
    base, dot, ext = name.rpartition('.')
    if not dot or not base:
        base, ext = name, ''
    assert 1 <= len(base) <= 8 and len(ext) <= 3, f'{name}: not an 8.3 name'
    flags = (0x08 if base.islower() else 0) | (0x10 if ext.islower() else 0)
    return (base.upper().ljust(8) + ext.upper().ljust(3)).encode(), flags


def build(tree, sectors, fat_type=32, label=b'GLASSOS    ', hidden=0):
    spc, nfats = 1, 2
    if fat_type == 32:
        reserved, root_entries = 32, 0
        fat_size = ((sectors - reserved) // spc + 2 + 127) // 128
    else:
        reserved, root_entries = 1, 512
        fat_size = ((sectors - reserved - root_entries * 32 // SECTOR) // spc + 2 + 255) // 256
    root_sectors = root_entries * 32 // SECTOR
    data_start = reserved + nfats * fat_size + root_sectors
    clusters = (sectors - data_start) // spc
    if fat_type == 32:
        assert clusters >= 65525, 'too small for FAT32'
    else:
        assert 4085 <= clusters < 65525, 'wrong size for FAT16'

    img = bytearray(sectors * SECTOR)
    eoc = 0x0FFFFFFF if fat_type == 32 else 0xFFFF
    fat = [0] * (clusters + 2)
    fat[0], fat[1] = (0x0FFFFFF8 if fat_type == 32 else 0xFFF8), eoc
    state = {'next': 2}

    def alloc(n):
        chain = list(range(state['next'], state['next'] + n))
        state['next'] += n
        for i, c in enumerate(chain):
            fat[c] = chain[i + 1] if i + 1 < n else eoc
        return chain

    def write_chain(chain, data):
        cb = spc * SECTOR
        for i, c in enumerate(chain):
            at = (data_start + (c - 2) * spc) * SECTOR
            chunk = data[i * cb:(i + 1) * cb]
            img[at:at + len(chunk)] = chunk

    now = time.localtime()
    fdate = (now.tm_year - 1980) << 9 | now.tm_mon << 5 | now.tm_mday
    ftime = now.tm_hour << 11 | now.tm_min << 5 | now.tm_sec // 2

    def entry(name, attr, cluster, size):
        raw, flags = (name, 0) if isinstance(name, bytes) else short_name(name)
        return struct.pack('<11sBBBHHHHHHHI', raw, attr, flags, 0, ftime, fdate, fdate,
                           cluster >> 16, ftime, fdate, cluster & 0xFFFF, size)

    def clusters_for(n):
        return max(1, (n + SECTOR * spc - 1) // (SECTOR * spc))

    def make_dir(items, parent, me=None):
        """A directory; me — an already allocated cluster (for the FAT32 root), None — allocate one."""
        entries, subdirs = [], []
        for name, src in sorted(items.items()):
            if isinstance(src, dict):
                subdirs.append((name, src))
                continue
            data = open(src, 'rb').read()
            chain = alloc(clusters_for(len(data))) if data else []
            write_chain(chain, data)
            entries.append(entry(name, 0x20, chain[0] if chain else 0, len(data)))
        is_root = parent is None
        count = len(entries) + len(subdirs) + (1 if is_root else 2) + 1
        if is_root and fat_type != 32:
            chain = None
        elif me is not None:
            chain = [me]
        else:
            chain = alloc(clusters_for(count * 32))
        here = chain[0] if chain else 0
        head = [entry(label, 0x08, 0, 0)] if is_root else [
            entry(b'.          ', 0x10, here, 0), entry(b'..         ', 0x10, parent or 0, 0)]
        for name, sub in subdirs:
            c = make_dir(sub, 0 if is_root else here)  # ".." to the root is always 0
            entries.append(entry(name, 0x10, c, 0))
        blob = b''.join(head + entries)
        if chain is None:
            assert len(blob) <= root_entries * 32
            at = (reserved + nfats * fat_size) * SECTOR
            img[at:at + len(blob)] = blob
        else:
            write_chain(chain, blob)
        return here

    if fat_type == 32:
        root = alloc(1)[0]
        assert root == 2
        make_dir(tree, None, me=root)
    else:
        make_dir(tree, None)

    boot = bytearray(SECTOR)
    boot[0:3] = b'\xEB\x58\x90' if fat_type == 32 else b'\xEB\x3C\x90'
    boot[3:11] = b'GLASSOS '
    total16 = sectors if fat_type != 32 and sectors < 65536 else 0
    struct.pack_into('<HBHBHHBHHHII', boot, 11, SECTOR, spc, reserved, nfats, root_entries, total16, 0xF8,
                     0 if fat_type == 32 else fat_size, 63, 255, hidden, 0 if total16 else sectors)
    serial = zlib.crc32(repr(tree).encode()) & 0xFFFFFFFF
    if fat_type == 32:
        struct.pack_into('<IHHIHH', boot, 36, fat_size, 0, 0, 2, 1, 6)
        ext = 64
    else:
        ext = 36
    boot[ext] = 0x80
    boot[ext + 2] = 0x29
    struct.pack_into('<I', boot, ext + 3, serial)
    boot[ext + 7:ext + 18] = label
    boot[ext + 18:ext + 26] = b'FAT32   ' if fat_type == 32 else b'FAT16   '
    boot[510:512] = b'\x55\xAA'
    img[0:SECTOR] = boot
    if fat_type == 32:
        free = sum(1 for v in fat[2:] if v == 0)
        info = bytearray(SECTOR)
        struct.pack_into('<I', info, 0, 0x41615252)
        struct.pack_into('<III', info, 484, 0x61417272, free, state['next'])
        struct.pack_into('<I', info, 508, 0xAA550000)
        img[SECTOR:2 * SECTOR] = info
        img[6 * SECTOR:7 * SECTOR] = boot
        img[7 * SECTOR:8 * SECTOR] = info
    fmt = '<%dI' if fat_type == 32 else '<%dH'
    fat_bytes = struct.pack(fmt % len(fat), *fat)
    for k in range(nfats):
        at = (reserved + k * fat_size) * SECTOR
        img[at:at + len(fat_bytes)] = fat_bytes
    return img, state['next'] - 2, clusters
