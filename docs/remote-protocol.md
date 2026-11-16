# Remote access protocol

GlassOS has a small signed REST API over HTTP. From another computer on the same network
you can run terminal commands, read the kernel log and files, take screenshots, install a
trial kernel and reboot — without touching the machine. This is a development interface; a
proper protocol with key exchange and encryption is planned to replace it. The actions are
plain functions in `src/net/remote.rs`; HTTP is only a wrapper around them.

Clients:

- `tools/remote.py` — a command line client (Python standard library only);
- `tools/glassos_mcp.py` — an MCP server for AI assistants, see [`AGENTS.md`](../AGENTS.md).

## Turning it on

On the machine, as an administrator:

- Settings → Remote Access → "Allow remote access", or `remote on` in the terminal;
- `remote` shows the state, `remote key` the key, `remote newkey` makes a new key (the old one
  stops working), `remote off` turns access off;
- hand the key to the other computer as a file, for example `remote key > /media/usb0/glassos.key`.

Commands run with the rights of the administrator who enabled access, even if nobody is
logged in. While requests come in (and for a minute after), the top bar shows a "Remote"
label; a new client address produces a notification. Access is on TCP port 7070 and only
from the machine's own subnets (any of its interfaces).

## Requests

| Request | What it does |
|---|---|
| `GET /info` | name, system version, kernel path, trial or not, uptime, user, address, screens, log position |
| `POST /run?cwd=&wait=` | the body is terminal lines, the answer is their output; `X-Glass-Cwd` is the folder after the commands; long commands are awaited up to `wait` seconds (30 by default, at most 600) and then stopped |
| `GET /log?from=N` | the kernel log after position N; `X-Glass-Log-End` is the new position |
| `GET /list?path=` | a folder: lines `d\t\tname` and `f\tsize\tname` |
| `GET /file?path=` | a file |
| `PUT /file?path=[&parents=1]` | write the body to a file (`parents=1` creates the folders) |
| `GET /shot?screen=N` | a screenshot, PNG (uncompressed: large) |
| `PUT /kernel` | a trial kernel into `\EFI\GLASSOS\TRIAL.EFI` (checked to be an x86-64 UEFI program) |
| `POST /kernel/keep` | the running trial kernel becomes `\EFI\BOOT\BOOTX64.EFI`, the previous one is kept as `OLD.EFI` |
| `POST /reboot?into=normal\|trial\|bios\|usb` | reboot; `trial` boots the trial kernel once through BootNext, `bios` opens the firmware setup, `usb` boots once from a USB stick |
| `POST /input` | keyboard and mouse input (only in builds with the `debug-tools` feature and after `debug input on`): body lines `text …`, `key NAME`, `click X Y`, `rclick X Y`, `move X Y`, `wheel X Y N`; coordinates are screen pixels as on `/shot`. Typed text is never logged. |

Paths are absolute. Errors are plain text with status 400 (bad request), 401 (signature, time
or replay), 403 (not from the local network), 404 (no such thing) or 409 (the action failed).
Request heads are limited to 16 KiB, bodies to 128 MiB.

## Signing

The key is 20 random bytes, shown to people in base32 in groups of four (`M4AU-XEPL-…`).
A request carries these headers:

- `X-Glass-Time` — Unix time in seconds;
- `X-Glass-Nonce` — a random tag, 16–64 hex characters;
- `X-Glass-Body` — the SHA-256 of the body (hex);
- `X-Glass-Sign` — HMAC-SHA256(key, `method\npath-with-query\ntime\nnonce\nbody-hash`), hex.

The server checks the signature before reading the body, then checks the body hash. It
refuses a request if the clocks differ by more than 2 minutes (the 401 answer then carries
`X-Glass-Now` so the client can correct itself and retry), if it has seen the nonce before, or
if the request time is earlier than the machine's boot.

The answer is signed too: `X-Glass-Sign` = HMAC-SHA256(key, `status\nrequest-nonce\n` +
the answer's own `X-Glass-…` headers, lower-cased, sorted by name, as `name:value` joined with `\n`,
+ `\n` + the SHA-256 of the answer body). A client must check it: an unsigned or wrongly signed
answer did not come from the machine. Answers to refused requests are not signed.

The traffic itself is **not encrypted**: use this on a network you trust.

## Trial kernels

1. `PUT /kernel` puts the kernel into `\EFI\GLASSOS\TRIAL.EFI` on the system partition (only for a
   system installed on a SATA disk).
2. `POST /reboot?into=trial` writes the boot entry "GlassOS: trial kernel" and sets BootNext to it:
   the firmware boots it once.
3. The kernel knows where it was loaded from, so `/info` says `trial: yes`. For its first three
   minutes a trial kernel writes its log to `/system/trial.log` once a second, so if it hangs, the
   main kernel can serve that file afterwards.
4. `POST /kernel/keep` (or `remote keep` on the machine) makes it the main kernel. Without that the
   next boot starts the previous kernel again. If a trial kernel hangs, power-cycle the machine; on
   Bay Trail the chipset watchdog also resets it after 30–60 s.

## Trying it in QEMU

`tools/sata-vm.sh` starts an "installed system" VM: GlassOS on a SATA disk (AHCI), VNC :4, remote
access forwarded to `127.0.0.1:7072`. After `remote on` inside the VM, `tools/sata-vm.sh key`
prints the key from the disk image.
