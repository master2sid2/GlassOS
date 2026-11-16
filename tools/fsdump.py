"""Reads files from a GlassOS FAT32 volume in a disk image (without mounting).

python3 tools/fsdump.py target/disk.img                 — list all files
python3 tools/fsdump.py target/disk.img /path/to/file   — the file contents
--raw — output the file as bytes (for pictures and programs), not as text.
"""
import struct, sys

class Fat:
    def __init__(self, path):
        self.img = open(path, 'rb').read()
        b = self.img[:512]
        self.spc = b[13]
        reserved, = struct.unpack_from('<H', b, 14)
        nfats = b[16]
        fatsz, self.root = struct.unpack_from('<I', b, 36)[0], struct.unpack_from('<I', b, 44)[0]
        self.fat_off = reserved * 512
        self.data = (reserved + nfats * fatsz) * 512
        self.cb = self.spc * 512

    def chain(self, c):
        out = []
        while 2 <= c < 0x0FFFFFF8 and len(out) < 1 << 20:
            out.append(c)
            c = struct.unpack_from('<I', self.img, self.fat_off + c * 4)[0] & 0x0FFFFFFF
        return out

    def content(self, c, size=None):
        data = b''.join(self.img[self.data + (x - 2) * self.cb:self.data + (x - 1) * self.cb] for x in self.chain(c))
        return data if size is None else data[:size]

    def entries(self, c):
        raw, lfn = self.content(c), []
        for i in range(0, len(raw), 32):
            e = raw[i:i + 32]
            if e[0] == 0:
                break
            if e[0] == 0xE5:
                lfn = []
                continue
            if e[11] == 0x0F:
                chars = e[1:11] + e[14:26] + e[28:32]
                lfn.insert(0, chars.decode('utf-16-le'))
                continue
            if e[11] & 8 or e[0:1] == b'.':
                lfn = []
                continue
            name = ''.join(lfn).split('\0')[0] if lfn else (e[:8].decode().strip() + ('.' + e[8:11].decode().strip() if e[8:11].strip() else ''))
            lfn = []
            hi, lo, size = struct.unpack_from('<H', e, 20)[0], struct.unpack_from('<H', e, 26)[0], struct.unpack_from('<I', e, 28)[0]
            yield name, bool(e[11] & 0x10), hi << 16 | lo, size

    def walk(self, c=None, path=''):
        for name, is_dir, cl, size in self.entries(self.root if c is None else c):
            p = f'{path}/{name}'
            if is_dir:
                yield from self.walk(cl, p)
            else:
                yield p, cl, size

    def find(self, path):
        c, size = self.root, None
        for part in path.strip('/').split('/'):
            for name, is_dir, cl, sz in self.entries(c):
                if name.lower() == part.lower():
                    c, size = cl, sz
                    break
            else:
                raise SystemExit(f'{path}: not found')
        return self.content(c, size)

raw = '--raw' in sys.argv
args = [a for a in sys.argv if a != '--raw']
fat = Fat(args[1])
if len(args) > 2:
    data = fat.find(args[2])
    if raw:
        sys.stdout.buffer.write(data)
    else:
        sys.stdout.write(data.decode(errors='replace'))
else:
    for p, cl, size in fat.walk():
        print(f'{size:8d}  {p}')
