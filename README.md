# GlassOS

**An operating system written from scratch by an AI.**

GlassOS is an experimental hobby OS for x86-64 PCs: its own kernel in Rust, its own drivers,
network stack, file systems, window manager and desktop, with applications written in Lua.
It is a *vibe-coded* project: practically all of the code was written by an AI coding agent
(Claude, through Claude Code) under the direction of one human, who set the goals, tested on
real hardware and reported what broke. The point of the project is to find out how far an AI can
go in building an independent operating system from nothing — no Linux kernel, no BSD, no POSIX
layer underneath.

It is not meant for daily use. Expect bugs, missing drivers and data you would not want to lose
kept somewhere else.

## Ideas behind it

- **Its own system, not another Unix.** No POSIX compatibility layer, no fork/exec. Built-in
  utilities borrow familiar names (`ls`, `cat`, `ping`) but the system is designed on its own terms.
- **Lua is the application language.** A program is a `.lua` file: no compiler, no build step.
  Each program runs in its own ring 3 process with a documented API ([docs/lua-api](docs/lua-api/00-getting-started.md))
  for widgets, drawing, files, network and sound. A hung or crashed program cannot stop the desktop.
- **An integrated desktop.** The compositor, window manager and the core applications (terminal,
  file manager, settings) live in the kernel, closer to Oberon or TempleOS than to X11.
- **Everything configurable in one place.** Each subsystem brings its own terminal commands and
  its own Settings page.
- **Real hardware first.** Drivers are developed on actual machines; the VM is a convenience.
- **Debuggable from outside.** Hardware reports (`hwprobe`), crash reports after hangs and panics,
  and a signed remote access API that an AI agent can use directly (MCP).

## What works

**Kernel.** UEFI boot, own page tables, SMP with a preemptive priority scheduler on all cores,
ring 3 processes (ELF), MSI interrupts, ACPI reset and power-off, a hang detector, the chipset
watchdog and crash reports.

**Storage.** SATA (AHCI), IDE (QEMU), USB mass storage; GPT and MBR partitioning; FAT16, FAT32 and
exFAT read/write; an installer that puts GlassOS on a SATA disk next to other systems and registers a
UEFI boot entry; live USB/ISO images that keep settings on the stick.

**Network.** Intel e1000, Realtek RTL8168, USB Ethernet (CDC-ECM, RNDIS), Wi-Fi (Realtek RTL8723BE
with WPA2, Intel AX201 experimental), cable and Wi-Fi at once. IPv4, ARP, ICMP, UDP, DHCP (with
lease renewal and link-local fallback), DNS, TCP (fast retransmit, RTT-based timers), HTTP client,
SMB2 client and server (shares visible from Windows, macOS and Linux), NetBIOS, mDNS, LLMNR,
WS-Discovery, NTP, a Prometheus metrics exporter and the remote access API.

**Bluetooth.** USB controllers (Realtek and Intel firmware loading), classic and LE, pairing,
keyboards and mice (HID over GATT), headphones (A2DP with an SBC encoder, AVRCP buttons and
absolute volume) and headset microphones (HFP + SCO).

**Graphics.** GOP framebuffer everywhere; native mode setting with a hardware cursor on Intel
Bay Trail; the Bay Trail GPU's blitter and render engine for composition and blur; HDMI on
Comet Lake laptops and their GPU's blitter for frame output; several monitors (extend or mirror), per-screen scaling.

**Input and sound.** PS/2, USB HID, I2C HID precision touchpads with gestures; input languages;
Intel HDA and AC97, a mixer, MP3/OGG/WAV playback, recording from built-in and headset microphones
(following the jack) and Bluetooth headsets.

**Desktop.** A compositing window manager with frosted glass, dock, top bar, notifications,
calendar, screenshots with annotation, multiple users with a login screen, per-program file
access rights and background services. Applications: Terminal, Files (two panes, network
neighbourhood), Notepad (Lua highlighting), Paint, an image viewer, Settings, Task Manager, Disks,
a sound recorder, and Lua programs such as a music player.

## Tested hardware

| Machine | Status |
|---|---|
| QEMU/KVM, q35 + OVMF | The development VM: everything except real-hardware drivers |
| GIGABYTE BRIX (Celeron N2807, Bay Trail) | The main test machine: display, SATA, Ethernet, Wi-Fi, Bluetooth, watchdog |
| Dell Latitude 7410 (Comet Lake) | Live from USB: HDMI, Wi-Fi, Bluetooth, battery; touchpad without gestures |

What works on each machine, part by part, is in [docs/hardware.md](docs/hardware.md). Other UEFI PCs
usually boot to the desktop with the firmware's framebuffer; drivers are a matter of luck. Please
send a [hardware report](CONTRIBUTING.md#hardware-reports) for your machine — it will be added there.

## Trying it

Download `glassos.img` (USB stick) or `glassos.iso` (Ventoy, VMs) from
[Releases](https://github.com/master2sid2/GlassOS/releases), write it to a USB stick and boot it in
UEFI mode with Secure Boot off. Details in [CONTRIBUTING.md](CONTRIBUTING.md#hardware-reports).

## Building and running

You need Linux with Rust nightly (`rust-toolchain.toml` selects it, with `rust-src`),
clang (for Lua and the small libc of `/bin/lua`), QEMU with OVMF (`/usr/share/OVMF/OVMF_CODE_4M.fd`)
and Python 3.

```sh
./run.sh                    # build and start QEMU; connect a VNC viewer to :1 (port 5901)
./run.sh --help             # options: --pulse HOST:PORT for sound, --hda, --bridge, --name ...
python3 tools/mkimage.py    # target/glassos.img — a bootable USB image (write it with dd/Rufus/Etcher)
python3 tools/mkiso.py      # target/glassos.iso — needs xorriso; works with Ventoy
tools/build-release.sh      # both images the way they are published in Releases
```

The kernel log goes to `target/serial.log`. On real hardware, boot the image in UEFI mode with
Secure Boot off. From the live system, `install` (or the menu) puts GlassOS on a SATA disk.

## Remote access and AI agents

A machine can be controlled from another computer over a signed HTTP API — run commands, read the
log and files, take screenshots, install and boot a trial kernel. `tools/remote.py` is the command
line client; `tools/glassos_mcp.py` exposes the same as an **MCP server**, so an AI assistant can
drive a GlassOS machine directly. See [docs/remote-protocol.md](docs/remote-protocol.md) and
[AGENTS.md](AGENTS.md).

## Documentation

- [Lua API](docs/lua-api/00-getting-started.md) — writing programs for GlassOS
- [Remote access protocol](docs/remote-protocol.md)
- [Instructions for AI agents](AGENTS.md)
- [Contributing and hardware reports](CONTRIBUTING.md)
- [Hardware compatibility](docs/hardware.md)
- [Known issues](docs/known-issues.md)

## Plans

- GlassOS's own file system (owners and permissions, crash safety) instead of FAT32 for the system disk.
- An application centre for Lua programs, signed with Ed25519.
- Composing the screen in its own thread; tickless timers.
- Wi-Fi: rate control by the firmware, 802.11n aggregation, better Bluetooth coexistence.
- TLS (HTTPS for programs, encrypted remote access with key exchange).
- A protocol for GlassOS machines to share keyboards, screens and disks with each other as native devices.
- Moving the built-in accessories (Notepad, Paint, viewer) to Lua.

## License

MIT, see [LICENSE](LICENSE). Bundled third-party code, fonts and firmware keep their own
licenses, listed in [THIRD_PARTY.md](THIRD_PARTY.md).
