# Known issues and simplifications

## Hardware

- **Realtek RTL8723BE Wi-Fi.** The transmit rate is set by hand (`wifi rate`, OFDM 36 Mbit/s by
  default) instead of by the card's firmware; no 802.11n aggregation (A-MPDU, Block Ack) yet.
  The receive DMA of this card can hang in a way that survives warm reboots; only removing power
  clears it. Heavy Wi-Fi traffic disturbs Bluetooth audio on the same chip (coexistence is basic).
- **Intel AX201 Wi-Fi** is experimental.
- **Bay Trail display.** Occasionally the firmware leaves the display plane off after a reboot
  (`baytrail: unexpected plane setup` in the log); the driver then keeps the firmware mode and the
  monitor may show coloured stripes. A reboot helps. The driver should set its own mode from that
  state too.
- **Realtek RTL8168.** Some firmwares leave the PHY without the 1000BASE-T advertisement; the driver
  turns it on. A bad cable makes the PHY fall back to 100 or 10 Mbit/s; `net phy` shows the
  negotiation.
- **MSI** is used by USB (xHCI), RTL8168 and RTL8723BE; HDA sound, Intel Wi-Fi and e1000 are polled.
- **Sleep and hibernation** are not implemented (closing the lid only turns off the screen).
- **Sound recording** only works from Bluetooth headsets: the microphone input of HDA sound cards
  (the headset jack, built-in microphones) is not used yet.
- **Touchpads** are polled; there are no settings for speed or scrolling direction, no three-finger
  gestures, no horizontal scrolling.

## System

- **The system disk is FAT32** for now (readable from other systems, convenient for debugging);
  GlassOS's own file system with owners and permissions is planned.
- **Remote access and the metrics exporter are not encrypted.** Remote requests and answers are
  signed (HMAC-SHA256), the metrics are only limited by address. Use them on a trusted network.
- **Time zones** are about a hundred cities plus bare UTC offsets with today's rules, no tz database
  and no history. NTP is a single SNTP request that steps the clock. The CMOS century register is
  not used (years 2000–2099).
- **Services** run with the rights of whoever added them; there is no separate "system" account.
  A service log is kept in memory only (the last 8 KiB).
- **Timers** have a 10 ms tick; short pauses use a fast timer. A tickless timer is planned.
- **The screen is composed in the main thread.** Lua windows draw in their own processes, but the
  built-in windows (Files, Terminal, Settings) draw in the desktop thread.
- **Time format** is 24-hour only.

## Lua API

See [docs/lua-api/08-limitations.md](lua-api/08-limitations.md).
