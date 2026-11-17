# Contributing

GlassOS is an experiment, but reports and patches are welcome. The most useful thing you can
send is a **hardware report** from a machine GlassOS does not support yet: you do not need to
build anything for that.

## Hardware reports

GlassOS has a built-in hardware probe, `hwprobe`. It collects what a driver developer needs —
PCI and USB devices, the GPU and display state, the monitor EDID, Wi-Fi, I2C devices (touchpads),
SATA controllers, the firmware boot entries and the full kernel log — and saves it to the USB
stick GlassOS booted from.

1. **Get a boot image.** Download `glassos.img` (USB image) or `glassos.iso` from the Releases
   page, or build them yourself (see the README): `python3 tools/mkimage.py` /
   `python3 tools/mkiso.py`.
2. **Write it to a USB stick.** The `.img` with Rufus (DD mode), balenaEtcher or
   `dd if=glassos.img of=/dev/sdX bs=4M`; the `.iso` can also simply be copied onto a
   [Ventoy](https://www.ventoy.net) stick. Writing the image erases the stick.
3. **Boot it in UEFI mode.** Use the firmware boot menu (often F12, F11, F8 or Esc). Secure Boot
   must be **off**: the kernel is not signed. Legacy/CSM boot is not supported.
4. **Run the probe.** When the desktop appears, open Terminal from the dock and type `hwprobe`.
   It prints a short summary and writes the files to the `glassos-hw` folder on the stick.
   Without keyboard or mouse support you can try a USB keyboard; if nothing works, take photos of
   the screen instead.
5. **Check and send.** Shut down (menu → Shut down), take the stick to another computer and look at
   the `glassos-hw` folder. Then open an issue named "Hardware: <vendor and model>" and attach the
   folder (zipped), plus a few words: what works, what does not, what you expected. The machine
   then gets an entry in [docs/hardware.md](docs/hardware.md).

The report contains the network card MAC addresses, the names of Wi-Fi networks seen nearby,
the local IP addresses and the monitor's serial number (in the EDID). Remove what you do not want
to publish; the rest is enough.

If GlassOS hangs or panics while booting, the panic screen shows the reason; a photo of it is a
report too. After a hang or a panic on a machine with a disk, the next boot shows a crash report
window and keeps it in `/system/crashes`.

If you can also boot Linux on the same machine, the output of `lspci -nnk`, `lsusb` and
`sudo dmidecode -t system` helps a lot.

## Other debugging tools

| Command | What it shows |
|---|---|
| `dmesg [word]` | The kernel log (only lines containing `word`) |
| `pci`, `lsusb` | Devices; `pci` also shows a device's configuration and registers |
| `hwprobe` | The hardware report above |
| `power`, `cpu`, `irq` | Sleep states, sensors, CPU features, interrupts |
| `net`, `net phy`, `wifi`, `bt` | Network, cable negotiation, Wi-Fi and Bluetooth state |
| `crashtest hang\|panic\|stall` | Tests the crash reporting itself |

With [remote access](docs/remote-protocol.md) all of this works from another computer:
`tools/remote.py run hwprobe`, `tools/remote.py log -f --grep wifi`.

## Code

- Read [`AGENTS.md`](AGENTS.md): it describes the build, the test setup and the conventions for
  humans and AI agents alike.
- Test in QEMU (`./run.sh`) and, for drivers, on real hardware with a trial kernel
  (`tools/remote.py kernel`).
- Do not copy GPL code. BSD/ISC/MIT code may be ported with attribution.
- Keep comments and documentation in English.
