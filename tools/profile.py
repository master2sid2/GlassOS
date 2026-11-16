#!/usr/bin/env python3
"""Where the CPU spends its time: samples RIP through the QEMU monitor and groups them by kernel function.

The kernel must be built with a symbol map (the UEFI target only, not the ring 3 programs):
    CARGO_TARGET_X86_64_UNKNOWN_UEFI_RUSTFLAGS="-C link-arg=/MAP:$PWD/target/kernel.map" NAME=test VNC=:3 ./run.sh

    python3 tools/profile.py [--qmp target/test-qmp.sock] [--log target/test-serial.log]
                             [--samples 100] [--interval 0.05] [--cpu 0]

The image base is taken from the log ("kernel image base 0x…"), the map addresses are rebased.
"""
import argparse
import bisect
import collections
import re
import sys
import time

sys.path.insert(0, 'tools')
from qmp import Qmp  # noqa: E402

PREFERRED = 0x140000000


def load_map(path):
    syms = []
    for line in open(path, errors='replace'):
        m = re.match(r'\s+\w{4}:\w{8}\s+(\S+)\s+([0-9a-f]{16})\s', line)
        if m:
            syms.append((int(m.group(2), 16), m.group(1)))
    syms.sort()
    return syms


def short(name):
    """The readable parts of a v0 name (_R…): modules and the function."""
    parts = re.findall(r'\d+([A-Za-z_][A-Za-z0-9_]*)', name)
    parts = [p for p in parts if not re.fullmatch(r'Cs[0-9A-Za-z_]+', p) and len(p) > 1]
    return '::'.join(parts[-4:]) if parts else name[:80]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--qmp', default='target/test-qmp.sock')
    ap.add_argument('--log', default='target/test-serial.log')
    ap.add_argument('--map', default='target/kernel.map')
    ap.add_argument('--samples', type=int, default=100)
    ap.add_argument('--interval', type=float, default=0.05)
    ap.add_argument('--cpu', type=int, default=0)
    a = ap.parse_args()
    base = None
    for line in open(a.log, errors='replace'):
        m = re.search(r'kernel image base (0x[0-9a-f]+)', line)
        if m:
            base = int(m.group(1), 16)
    if base is None:
        sys.exit('no "kernel image base" in the log')
    syms = load_map(a.map)
    addrs = [s[0] for s in syms]
    q = Qmp(a.qmp)
    hits = collections.Counter()
    for _ in range(a.samples):
        r = q.cmd('human-monitor-command', **{'command-line': 'info registers -a'})
        text = r['return'] if isinstance(r, dict) else r
        blocks = re.split(r'\nCPU#', '\n' + text)
        block = next((b for b in blocks if b.startswith(f'{a.cpu}')), '')
        m = re.search(r'RIP=([0-9a-f]+)', block)
        if m:
            rip = int(m.group(1), 16)
            at = rip - base + PREFERRED
            i = bisect.bisect_right(addrs, at) - 1
            name = short(syms[i][1]) if 0 <= i < len(syms) and at - addrs[i] < 0x100000 else f'outside kernel {rip:#x}'
            hits[name] += 1
        time.sleep(a.interval)
    total = sum(hits.values())
    for name, n in hits.most_common(25):
        print(f'{n * 100 // max(total, 1):3d}%  {n:4d}  {name}')


if __name__ == '__main__':
    main()
