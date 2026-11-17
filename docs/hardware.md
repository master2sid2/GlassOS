# Hardware compatibility

What has been tried on real machines, part by part. This list grows with every new machine:
send a [hardware report](../CONTRIBUTING.md#hardware-reports) and it gets an entry here.

| Mark | Meaning |
|---|---|
| ✅ | Works |
| ⚠️ | Works partly or with known problems (see the note) |
| ❌ | Does not work / no driver yet |
| ❔ | Not tested yet |

Machines: [GIGABYTE BRIX GB-BXBT-2807](#gigabyte-brix-gb-bxbt-2807) ·
[Dell Latitude 7410](#dell-latitude-7410) · [QEMU/KVM](#qemukvm)

## GIGABYTE BRIX GB-BXBT-2807

Mini PC, Intel Celeron N2807 (Bay Trail, 2 cores), the main test machine. GlassOS is installed on
its SATA disk and updated over the network with trial kernels.

| Part | Device | Status | Notes |
|---|---|---|---|
| Boot | AMI UEFI | ✅ | Installed system boots from its own UEFI entry; trial kernels via BootNext |
| Display | Intel HD Graphics (Valleyview), HDMI | ⚠️ | Own mode setting (up to 2560×1440@50 instead of the firmware's 1280×1024), hardware cursor, monitor sleep. Rarely after a reboot the firmware leaves the plane off and the screen shows stripes — reboot |
| GPU acceleration | Gen7 blitter and render engine | ✅ | Window composition and glass blur on the GPU |
| Storage | SATA (AHCI) | ✅ | Installer, partitions, FAT32/exFAT |
| Ethernet | Realtek RTL8168G | ✅ | Gigabit (the driver turns on the 1000BASE-T advertisement the firmware leaves off); `net phy` shows negotiation |
| Wi-Fi | Realtek RTL8723BE | ⚠️ | WPA2-PSK, reconnects after a drop, group key renewal; ~1.5 MB/s upload with a fixed rate. The firmware gives the card no PCI address (the driver assigns one). Its receive DMA can hang until a full power-off |
| Bluetooth | Realtek RTL8723B (USB, same card) | ✅ | Keyboards and mice (LE, HID over GATT), headphones (A2DP, buttons, absolute volume), headset microphone (HFP). Heavy Wi-Fi traffic can disturb audio |
| USB | xHCI | ✅ | Keyboards, mice, sticks, hubs, USB Ethernet |
| Sound | Intel HDA | ⚠️ | Headphone jack works. A microphone on the jack is not used: recording only supports Bluetooth headsets so far |
| Watchdog | TCO | ✅ | Resets the board 30–60 s after a hard hang |
| Power | | ⚠️ | Deep C-states and sensors work; no sleep/hibernate |

## Dell Latitude 7410

Laptop, Intel Comet Lake. Runs as a live system from a USB stick (its NVMe disk is not supported yet).

| Part | Device | Status | Notes |
|---|---|---|---|
| Boot | Dell UEFI | ✅ | From a USB stick (ISO on Ventoy or the USB image); Secure Boot off |
| Display | Built-in panel (GOP) | ✅ | Firmware mode, brightness from Settings |
| External monitor | HDMI (DDI B through a DP→HDMI converter) | ✅ | Own driver, modes from the monitor EDID |
| Brightness keys | Fn brightness keys | ❌ | The firmware sends them through an SMI path GlassOS does not handle yet |
| Keyboard | PS/2 | ✅ | Keyboard backlight (Fn+F5) is handled by the firmware |
| Touchpad | I2C HID (0488:120a) | ⚠️ | Cursor and clicks work; **multi-touch gestures and two-finger scrolling do not** |
| Storage | NVMe | ❌ | No NVMe driver yet; files live on the boot stick |
| Wi-Fi | Intel Wi-Fi 6 AX201 | ⚠️ | Experimental driver; copying over Wi-Fi measured at 8–10 MB/s |
| Bluetooth | Intel AX201 (USB 8087:0026) | ⚠️ | Firmware loads, keyboards pair; key auto-repeat may stick while Wi-Fi is busy |
| Battery, lid | Embedded controller | ✅ | Charge, time left, lid closes the screen |
| Sound | Intel HDA | ⚠️ | Speakers and headphone jack work. A microphone on the jack is not used: recording only supports Bluetooth headsets so far |
| Power | | ⚠️ | No sleep/hibernate; deep C-states not verified |

## QEMU/KVM

The development VM: `./run.sh` (q35 machine, OVMF firmware).

| Part | Device | Status |
|---|---|---|
| Display | std VGA (GOP), a second screen through `secondary-vga` | ✅ |
| Storage | IDE (piix3), SATA (AHCI), USB storage | ✅ |
| Network | e1000 | ✅ |
| Sound | AC97, Intel HDA | ✅ |
| Input | PS/2, vmmouse, USB keyboard/tablet | ✅ |

## Adding a machine

Copy this block, fill it in from a `hwprobe` report and what the person saw, and add the machine to
the list at the top. Keep notes short and factual: what works, what does not, and any workaround.

```markdown
## Vendor Model

Type (laptop, mini PC, desktop), CPU and chipset generation. How it was booted.

| Part | Device | Status | Notes |
|---|---|---|---|
| Boot | | ❔ | |
| Display | | ❔ | |
| Storage | | ❔ | |
| Ethernet | | ❔ | |
| Wi-Fi | | ❔ | |
| Bluetooth | | ❔ | |
| USB | | ❔ | |
| Keyboard, touchpad | | ❔ | |
| Sound | | ❔ | |
| Battery, lid | | ❔ | |
```
