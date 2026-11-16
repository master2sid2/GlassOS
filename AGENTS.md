# Instructions for AI agents

GlassOS was written almost entirely by an AI coding agent working with a human. This file is
for agents that continue that work or that drive a running GlassOS machine.

## Driving a GlassOS machine (MCP)

`tools/glassos_mcp.py` is an MCP server (stdio transport, Python standard library only) on top of
the [remote access protocol](docs/remote-protocol.md). Register it with your MCP client:

```json
{"mcpServers": {"glassos": {"command": "python3",
                            "args": ["/path/to/glassos/tools/glassos_mcp.py"],
                            "env": {"GLASSOS_HOST": "192.0.2.10"}}}}
```

The key comes from `GLASSOS_KEY` or from `~/.config/glassos/remote.cfg`, which
`tools/remote.py setup HOST KEYFILE` writes. On the machine, an administrator enables access
with `remote on` and exports the key with `remote key > /media/usb0/glassos.key`.

| Tool | Use it for |
|---|---|
| `glassos_info` | First call: which kernel runs, uptime, screens, the log position |
| `glassos_run` | Any terminal command; start with `help` (all commands, grouped) |
| `glassos_log` | The kernel log; pass the returned `from` to read only new lines |
| `glassos_list`, `glassos_read_file`, `glassos_write_file` | Files (absolute paths) |
| `glassos_screenshot` | See the screen (downscaled; the note says how to map coordinates) |
| `glassos_input` | Keyboard and mouse (needs the `debug-tools` build and `debug input on`) |
| `glassos_trial_kernel`, `glassos_keep_kernel`, `glassos_reboot` | Install and test a new kernel |

Good habits:

- **Look before acting.** Take a screenshot before clicking: a person may be using the machine.
- **Read the log** after anything that touches hardware (`glassos_log` with `grep`).
- **Prefer commands over clicks.** Most settings have a terminal command (`help` lists them).
- **Trial kernels are safe, kept kernels are not.** A trial kernel boots once; if it hangs, a power
  cycle (or the Bay Trail watchdog) returns the previous kernel. Only keep a kernel after checking
  `glassos_info` (`trial: yes`) and the log.
- **Never type secrets** (passwords, keys) through `glassos_input` into logs or files; the kernel
  does not log typed text, keep it that way.

## Working on the code

- Rust, `no_std`, nightly (see `rust-toolchain.toml`), a custom target `x86_64-glassos.json`
  (UEFI with hardware SSE) built with `build-std`. Build: `cargo build --release`; the ring 3
  programs: `cd user && cargo build --release`.
- Run in QEMU with `./run.sh` (see `./run.sh --help`); a second, scriptable instance with
  `./run.sh --name test --vnc :3`. `tools/qmp.py` and `tools/vmlib.py` drive it (screenshots, keys,
  clicks); the kernel log is in `target/NAME-serial.log`.
- Real hardware: build, then `tools/remote.py kernel` (upload and boot a trial kernel), check
  `remote.py log`, then `remote.py keep`. Hardware without remote access: `tools/mkimage.py` or
  `tools/mkiso.py` for a USB stick.
- Comments and documentation are in English. Explain *why* in comments, not *what*.
- No GPL code. BSD/ISC/MIT sources may be ported with attribution in the file header and in
  `THIRD_PARTY.md`; Linux may be read to cross-check behaviour only.
- Hardware drivers log what they find (`kprintln!`): a driver that cannot be tested in a VM must
  leave enough in the log (and in `hwprobe`) to debug it from a report.
- Lua is the application language: new user-facing features usually mean extending the Lua API
  (`src/lua/`, documented in `docs/lua-api/`), not adding a POSIX layer.
