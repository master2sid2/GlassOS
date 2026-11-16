# Third-party components

GlassOS itself is MIT-licensed (see `LICENSE`). These parts come from elsewhere and keep
their own licenses.

## Bundled source code

| Component | Where | License |
|---|---|---|
| Lua 5.4.7 | `third_party/lua-5.4.7` | MIT (see the end of `src/lua.h`) |
| lewton 0.10.2 (Ogg Vorbis decoder), converted to `no_std` | `third_party/lewton` | MIT OR Apache-2.0 (`third_party/lewton/LICENSE`) |

## Crates (fetched by Cargo)

fontdue, nanomp3, libm, uefi, linked_list_allocator, spinning_top — all MIT and/or Apache-2.0
(nanomp3 also Zlib). See each crate for its notice.

## Code derived from other projects

| What | Based on | License |
|---|---|---|
| RTL8723B register tables (`src/rtl8723be/tables.rs`) and IQ calibration (`src/rtl8723be/iqk.rs`) | FreeBSD `rtwn`, RTL8723B support by Ahmad Khalifa | BSD-2-Clause |
| RTL8723BE bring-up order (`src/rtl8723be/`) | FreeBSD and OpenBSD `rtwn` | BSD-2-Clause, ISC |
| Intel Bluetooth firmware loading (`src/bt/intel.rs`) | FreeBSD `iwmbtfw` (Vladimir Kondratyev, Future Crew LLC) | BSD-2-Clause |
| Realtek Bluetooth patch loading (`src/bt/realtek.rs`) | FreeBSD `rtlbtfw` (Adrian Chadd, Vladimir Kondratyev) | BSD-2-Clause |
| Intel AX201 Wi-Fi driver order of operations (`src/wifi.rs`, `src/wifi/`) | OpenBSD `iwx` | ISC |
| Gen7 render pipeline and shader binary (`src/gpu/render.rs`) | intel-gpu-tools `lib/rendercopy_gen7.c` (Intel Corporation and the intel-gpu-tools authors) | MIT |
| Bay Trail display and GPU register sequences (`src/baytrail.rs`, `src/gpu.rs`) | the Linux i915 driver (its MIT-licensed parts) and Intel's public documentation | MIT |

Linux GPL code was used only as a reference to cross-check behaviour; no GPL code is copied.

## Fonts

| Font | Files | License |
|---|---|---|
| Inter | `assets/fonts/Inter-*.ttf` | SIL Open Font License 1.1 (`assets/fonts/LICENSE-Inter.txt`) |
| JetBrains Mono | `assets/fonts/JetBrainsMono-*.ttf` | SIL Open Font License 1.1 (`assets/fonts/LICENSE-JetBrainsMono.txt`) |

The `*-Cyrillic-*` files are subsets of the same fonts.

## Firmware

Redistributed unmodified from [linux-firmware](https://git.kernel.org/pub/scm/linux/kernel/git/firmware/linux-firmware.git),
under the licences next to them in `assets/firmware/`:

| File | Device | Licence |
|---|---|---|
| `rtw8723b.fw` (`rtlwifi/rtl8723befw_36.bin`) | Realtek RTL8723BE Wi-Fi | `LICENCE.rtlwifi_firmware.txt` |
| `rtbt8723.fw` (`rtl_bt/rtl8723b_fw.bin`) | Realtek RTL8723B Bluetooth | `LICENCE.rtlwifi_firmware.txt` |
| `quz77.fw` (an `iwlwifi-QuZ-*-77.ucode` image) | Intel Wi-Fi 6 AX201 | `LICENCE.iwlwifi_firmware` |
| `ibt19.sfi`, `ibt19.ddc` (an `intel/ibt-19-*` pair) | Intel AX201 Bluetooth | `LICENCE.ibt_firmware` |
