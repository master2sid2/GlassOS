"""Builds a GlassOS boot ISO (UEFI) — for Ventoy, virtual machines and burning to a disc.

python3 tools/mkiso.py [target/glassos.iso]

Inside the ISO is an El Torito boot image for EFI: a FAT16 volume with \\EFI\\BOOT\\BOOTX64.EFI,
\\apps and \\bin (the kernel reads them at startup). The same files also lie in the ISO itself —
for loaders that take them from there. Needs xorriso.
"""
import os, shutil, subprocess, sys, tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import fatimage

out = sys.argv[1] if len(sys.argv) > 1 else os.path.join(fatimage.ROOT, 'target', 'glassos.iso')
tree = fatimage.system_tree()

with tempfile.TemporaryDirectory() as tmp:
    # The boot volume: 16 MiB FAT16 (El Torito does not like images larger than 32 MiB).
    volume, used, clusters = fatimage.build(tree, 16 * 2048, 16)
    boot_dir = os.path.join(tmp, 'boot')
    os.makedirs(boot_dir)
    with open(os.path.join(boot_dir, 'efiboot.img'), 'wb') as f:
        f.write(volume)

    def copy(items, dst):
        for name, src in items.items():
            path = os.path.join(dst, name)
            if isinstance(src, dict):
                os.makedirs(path, exist_ok=True)
                copy(src, path)
            else:
                shutil.copy(src, path)
    copy(tree, tmp)

    subprocess.run(['xorriso', '-as', 'mkisofs', '-quiet', '-iso-level', '3', '-J', '-joliet-long', '-R',
                    '-V', 'GLASSOS', '-o', out,
                    '-e', 'boot/efiboot.img', '-no-emul-boot', '-isohybrid-gpt-basdat',
                    tmp], check=True)
print(f'{out}: {os.path.getsize(out) >> 20} MiB, UEFI boot (El Torito, FAT16 {used} of {clusters} clusters)')
