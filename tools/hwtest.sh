#!/usr/bin/env bash
# Testing the USB stick image (tools/mkimage.py) in QEMU the way a real computer sees it:
# the image is attached as a USB stick, there is no IDE disk, vmmouse or network; input is USB.
# VNC :3 (port 5903), QMP — target/hw-qmp.sock, the log — target/hw-serial.log.
cd "$(dirname "$0")/.."
[ -f target/hw-OVMF_VARS.fd ] || cp /usr/share/OVMF/OVMF_VARS_4M.fd target/hw-OVMF_VARS.fd
exec qemu-system-x86_64 -machine q35,accel=kvm,vmport=off -cpu host -smp 2 -m 1G \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,file=target/hw-OVMF_VARS.fd \
    -device qemu-xhci,id=xhci \
    -drive if=none,id=stick,format=raw,file=target/glassos.img -device usb-storage,bus=xhci.0,drive=stick \
    -device usb-kbd,bus=xhci.0 -device usb-mouse,bus=xhci.0 \
    -vga std -nic none -vnc :3 -serial file:target/hw-serial.log \
    -qmp unix:target/hw-qmp.sock,server,nowait
