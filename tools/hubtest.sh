#!/usr/bin/env bash
# Testing hubs: a USB stick, a keyboard and a tablet behind a hub, a keyboard behind a hub in a hub. VNC :3.
cd "$(dirname "$0")/.."
# First: python3 tools/mkimage.py
cp /usr/share/OVMF/OVMF_VARS_4M.fd target/hub-OVMF_VARS.fd
exec qemu-system-x86_64 -machine q35,accel=kvm,vmport=off -cpu host -smp 2 -m 1G \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,file=target/hub-OVMF_VARS.fd \
    -device qemu-xhci,id=xhci \
    -device usb-hub,bus=xhci.0,port=1 \
    -drive if=none,id=stick,format=raw,file=target/glassos.img -device usb-storage,bus=xhci.0,port=1.1,drive=stick,bootindex=0 \
    -device usb-kbd,bus=xhci.0,port=1.2 -device usb-tablet,bus=xhci.0,port=1.3 \
    -device usb-hub,bus=xhci.0,port=1.4 -device usb-kbd,bus=xhci.0,port=1.4.1 \
    -device ich9-intel-hda -device hda-output,audiodev=snd0 -audiodev none,id=snd0 \
    -vga std -nic none -vnc :3 -serial file:target/hw-serial.log \
    -qmp unix:target/hw-qmp.sock,server,nowait
