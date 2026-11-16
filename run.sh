#!/usr/bin/env bash
# Build GlassOS and run it in QEMU/KVM.
#
#   ./run.sh [options]
#
# Options (each also has an environment variable, shown in brackets):
#   --name NAME          a separate instance with its own disk, sockets and log,
#                        for example a test VM next to the main one        [NAME]
#   --vnc :N             VNC display, :1 (port 5901) by default              [VNC]
#   --pulse HOST:PORT    play the VM's sound through a PulseAudio server that
#                        accepts TCP connections (module-native-protocol-tcp),
#                        for example on your desktop; without it the sound goes
#                        to the VNC client                                  [PULSE]
#   --hda                Intel HDA sound card instead of AC97          [SOUND=hda]
#   --bridge [BR]        join the host bridge BR (br0 by default) instead of
#                        QEMU's NAT: the VM gets an address from your router
#                        and its shares are visible on the LAN   [NET=bridge BRIDGE]
#   --mem SIZE           guest memory, 512M by default                       [MEM]
#   --extra "ARGS"       extra QEMU arguments (devices under test)         [EXTRA]
#
# Where things go:
#   screen        VNC (see --vnc)
#   kernel log    target/[NAME-]serial.log
#   QEMU monitor  target/[NAME-]qemu-monitor.sock
#   QMP           target/[NAME-]qmp.sock (screenshots and input for tests, see tools/qmp.py)
#   system disk   target/[NAME-]disk.img (FAT32, can be mounted on Linux with mount -o loop)
#
# apps/ is installed into /apps, the ring 3 programs built from user/ into /bin.
set -euo pipefail
cd "$(dirname "$0")"

while [ $# -gt 0 ]; do
    case "$1" in
        --name) NAME="$2"; shift 2 ;;
        --vnc) VNC="$2"; shift 2 ;;
        --pulse) PULSE="$2"; shift 2 ;;
        --hda) SOUND=hda; shift ;;
        --bridge)
            NET=bridge
            if [ $# -gt 1 ] && [ "${2#-}" = "$2" ]; then BRIDGE="$2"; shift; fi
            shift ;;
        --mem) MEM="$2"; shift 2 ;;
        --extra) EXTRA="$2"; shift 2 ;;
        -h|--help) sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "run.sh: unknown option $1 (see --help)" >&2; exit 2 ;;
    esac
done

NAME="${NAME:-}"
P="target/${NAME:+$NAME-}"

cargo build --release
(cd user && cargo build --release)
mkdir -p target/esp/EFI/BOOT target/esp/apps target/esp/bin
cp target/x86_64-glassos/release/kernel.efi target/esp/EFI/BOOT/BOOTX64.EFI
rm -f target/esp/apps/*
cp apps/* target/esp/apps/
rm -f target/esp/bin/*
find user/target/x86_64-unknown-none/release -maxdepth 1 -type f -executable -exec cp {} target/esp/bin/ \;
# The disk is a 64 MiB FAT32 volume (older 16 MiB images are grown; GlassOS
# moves the files over by itself).
[ -f "${P}disk.img" ] || truncate -s 64M "${P}disk.img"
[ "$(stat -c %s "${P}disk.img")" -ge $((64 << 20)) ] || truncate -s 64M "${P}disk.img"
[ -f "${P}OVMF_VARS.fd" ] || cp /usr/share/OVMF/OVMF_VARS_4M.fd "${P}OVMF_VARS.fd"

# Sound: a named instance writes it to target/NAME-audio.wav (tests check it there);
# otherwise PulseAudio if --pulse was given and answers, else the VNC client.
PULSE="${PULSE:-}"
if [ -n "$NAME" ]; then
    AUDIO="wav,id=snd0,path=${P}audio.wav"
elif [ -n "$PULSE" ] && timeout 2 bash -c "</dev/tcp/${PULSE%:*}/${PULSE##*:}" 2>/dev/null; then
    AUDIO="pa,id=snd0,server=tcp:$PULSE"
    echo "sound: PulseAudio at $PULSE"
else
    [ -n "$PULSE" ] && echo "sound: PulseAudio at $PULSE does not answer, sound goes over VNC"
    AUDIO="none,id=snd0"
fi

# Sound card: --hda — Intel HDA (as on modern PCs), otherwise AC97.
if [ "${SOUND:-}" = hda ]; then
    SOUNDCARD="-device ich9-intel-hda -device ${HDA_CODEC:-hda-output},audiodev=snd0"
else
    SOUNDCARD="-device AC97,audiodev=snd0"
fi

# Forwarded to the host: the guest SMB server (GlassOS shares) at 127.0.0.1:4450,
# Prometheus metrics at 127.0.0.1:19099, remote access (tools/remote.py) at
# 127.0.0.1:7070. A named instance uses 4451, 19100 and 7071.
if [ -n "$NAME" ]; then SMB_PORT=4451; METRICS_PORT=19100; REMOTE_PORT=7071; else SMB_PORT=4450; METRICS_PORT=19099; REMOTE_PORT=7070; fi

# Network: QEMU NAT with the ports above forwarded by default. With --bridge the VM
# joins the host bridge and gets its address from the router (Windows only talks SMB
# on port 445, so this is the way to see GlassOS shares from other computers).
if [ "${NET:-}" = bridge ]; then
    NIC="-netdev bridge,id=net0,br=${BRIDGE:-br0} -device e1000,netdev=net0,mac=${MAC:-52:54:00:47:4c:$([ -n "$NAME" ] && echo 02 || echo 01)}"
else
    NIC="-nic user,model=e1000,hostfwd=tcp:127.0.0.1:${SMB_PORT}-:445,hostfwd=tcp:127.0.0.1:${METRICS_PORT}-:9100,hostfwd=tcp:127.0.0.1:${REMOTE_PORT}-:7070"
fi

# USB (xHCI) with a keyboard and a tablet: input goes through the USB driver, as on
# real hardware (PS/2 and vmmouse remain as a fallback).
USB="-device qemu-xhci,id=xhci -device usb-kbd,bus=xhci.0 -device usb-tablet,bus=xhci.0"

exec qemu-system-x86_64 \
    -machine q35,accel=kvm -cpu host -smp 4 -m ${MEM:-512M} \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,file="${P}OVMF_VARS.fd" \
    -drive format=raw,file=fat:rw:target/esp \
    -device piix3-ide,id=ide \
    -drive if=none,id=disk,format=raw,cache=writeback,file="${P}disk.img" \
    -device ide-hd,drive=disk,bus=ide.0 \
    -vga std $NIC -rtc base=localtime \
    -audiodev "$AUDIO" $SOUNDCARD \
    $USB ${EXTRA:-} \
    -vnc "${VNC:-:1},audiodev=snd0" \
    -serial "file:${P}serial.log" \
    -monitor "unix:${P}qemu-monitor.sock,server,nowait" \
    -qmp "unix:${P}qmp.sock,server,nowait"
