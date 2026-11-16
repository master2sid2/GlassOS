#!/usr/bin/env bash
# The "installed system" VM: GlassOS on a SATA disk (AHCI on q35), without the ESP folder —
# this is how the installed system, the trial kernel and remote access are tested.
# The target/sata-disk.img image is built from the current build if it does not exist (NEW=1 — anew).
# VNC :4, QMP target/sata-qmp.sock, log target/sata-serial.log,
# GlassOS remote access — 127.0.0.1:7072.
#
#   (setsid nohup tools/sata-vm.sh > target/sata-run.out 2>&1 < /dev/null &)
#   tools/sata-vm.sh key    — the remote access key from the image (after remote on)
set -euo pipefail
cd "$(dirname "$0")/.."
IMG=target/sata-disk.img
VARS=target/sata-OVMF_VARS.fd

if [ "${1:-}" = key ]; then
    PART=target/sata-part.img
    dd if="$IMG" of="$PART" bs=512 skip=2048 status=none
    python3 tools/fsdump.py "$PART" /system/remote.key
    rm -f "$PART"
    exit
fi

if [ ! -f "$IMG" ] || [ -n "${NEW:-}" ]; then
    cargo build --release
    (cd user && cargo build --release)
    python3 tools/mkimage.py "$IMG" 256
    cp /usr/share/OVMF/OVMF_VARS_4M.fd "$VARS"
fi

exec qemu-system-x86_64 -machine q35,accel=kvm -cpu host -smp 2 -m 1G \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,file="$VARS" \
    -drive if=none,id=sd,format=raw,file="$IMG" -device ide-hd,drive=sd,bus=ide.0 \
    -vga std -nic user,model=e1000,hostfwd=tcp:127.0.0.1:7072-:7070 \
    -rtc base=localtime -device qemu-xhci,id=xhci -device usb-kbd,bus=xhci.0 -device usb-tablet,bus=xhci.0 \
    -vnc 127.0.0.1:4 -display none -serial file:target/sata-serial.log \
    -qmp unix:target/sata-qmp.sock,server,nowait
