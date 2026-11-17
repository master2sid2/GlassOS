use crate::font::{self, FontId};
use crate::fs;
use crate::gfx::{Canvas, Color, Rect};
use crate::input::{KEY_DELETE, KEY_DOWN, KEY_END, KEY_HOME, KEY_LEFT, KEY_PAGE_DOWN, KEY_PAGE_UP, KEY_RIGHT, KEY_TAB_NEXT, KEY_TAB_PREV, KEY_UP, LEFT};
use super::transfer::size_text;
use super::ui::{SCROLL_W, ScrollBar, TAB_BAR_H, TabBar};
use crate::interrupts::HZ;
use crate::lua;
use crate::net;
use crate::process;
use crate::sync::IrqLock;
use crate::task;
use crate::theme;
use crate::wm::{self, ACCENT, App, Ctx, OS_NAME, Request, TEXT};
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

const FONT: f32 = 14.0;
const LINE: i32 = 20;
const PAD: i32 = 12;
const HISTORY_FILE: &str = "terminal_history";
const ADMIN_ONLY: &str = "only an administrator can change shared folders";
const HISTORY_MAX: usize = 100;
const INPUT_MAX: usize = 1024;
/// The continuation mark of a wrapped input line (in the output — without the prompt).
const CONT: &str = "\u{1}";
const CTRL_A: u8 = 0x01;
const CTRL_C: u8 = 0x03;
const CTRL_D: u8 = 0x04;
const CTRL_E: u8 = 0x05;
const CTRL_K: u8 = 0x0B;
const CTRL_T: u8 = 0x14;
const CTRL_W: u8 = 0x17;
const CTRL_L: u8 = 0x0C;
const CTRL_U: u8 = 0x15;
const CTRL_V: u8 = 0x16;
/// Shell commands: (section, name, usage). `help` sorts them itself.
const COMMANDS: [(&str, &str, &str); 72] = [
    ("Disks", "df", "df \u{2014} drives, their size and free space"),
    ("Disks", "eject", "eject <drive> \u{2014} safely remove a drive, for example /media/usb0"),
    ("Disks", "mount", "mount smb://[user[:password]@]server[:port]/share [name] \u{2014} connect a network folder"),
    ("Disks", "umount", "umount /media/<name> \u{2014} disconnect a network folder"),
    ("Files", "cat", "cat <file> \u{2014} show a file"),
    ("Files", "cd", "cd [folder] \u{2014} go to a folder, without a name to your home"),
    ("Files", "cp", "cp <from> <to> \u{2014} copy a file or folder"),
    ("Files", "edit", "edit <file> \u{2014} open a file in Notepad"),
    ("Files", "file", "file [-i] <files> \u{2014} what is inside, judging by the content"),
    ("Files", "files", "files [folder] \u{2014} open a folder in Files"),
    ("Files", "hexdump", "hexdump [-s offset] [-n bytes] <file> \u{2014} the same as xxd"),
    ("Files", "readtest", "readtest <file> [MB] \u{2014} read a file (up to 200 MB) and show the speed: disks and network shares"),
    ("Files", "ls", "ls [folder] \u{2014} list a folder; ls /network shows computers nearby"),
    ("Files", "mkdir", "mkdir <folder> \u{2014} create a folder"),
    ("Files", "mv", "mv <from> <to> \u{2014} move or rename"),
    ("Files", "pwd", "pwd \u{2014} the current folder"),
    ("Files", "rm", "rm <path> \u{2014} delete a file or folder, with everything inside"),
    ("Files", "touch", "touch <file> \u{2014} create an empty file"),
    ("Files", "open", "open <file> \u{2014} open as a double click in Files does (music \u{2192} Player)"),
    ("Files", "view", "view <file> \u{2014} open a picture: PNG, JPEG, GIF, BMP"),
    ("Files", "write", "write <file> <text> \u{2014} replace a file with this text"),
    ("Files", "xxd", "xxd [-s offset] [-n bytes] <file> \u{2014} hex dump"),
    ("Network", "fetch", "fetch <http://url> [file] \u{2014} download a page or file"),
    ("Network", "host", "host <name> \u{2014} find the address of a name"),
    ("Network", "hostname", "hostname [name] \u{2014} show or change the name of this computer"),
    ("Network", "net", "net [renew|phy [restart]] \u{2014} network card, address and traffic; renew asks for addresses again; phy shows cable negotiation"),
    ("Network", "route", "route [add DEST/LEN [GATEWAY] [-p] | del DEST/LEN [-p]] \u{2014} show, add or delete routes; -p keeps them after a reboot"),
    ("Network", "ping", "ping <host> \u{2014} check that a computer answers"),
    ("Network", "share", "share <folder> [name] \u{2014} share a folder over the network"),
    ("Network", "sharepass", "sharepass [password] \u{2014} set the password for shared folders"),
    ("Network", "shares", "shares \u{2014} list shared folders"),
    ("Network", "unshare", "unshare <name> \u{2014} stop sharing a folder"),
    ("Network", "workgroup", "workgroup [name] \u{2014} show or change the workgroup"),
    ("Programs", "jobs", "jobs \u{2014} programs running in the background"),
    ("Programs", "kill", "kill %N \u{2014} stop a background program"),
    ("Programs", "lua", "lua \u{2014} interactive Lua, exit or Ctrl+D to leave"),
    ("Sound", "beep", "beep [hz] [ms] \u{2014} play a tone"),
    ("Sound", "play", "play <file.wav> \u{2014} play a sound file"),
    ("Sound", "rec", "rec SECONDS FILE.wav \u{2014} record from the microphone (a Bluetooth headset)"),
    ("Sound", "sound", "sound [name] \u{2014} play a system sound: notification, error, critical, drive-in, drive-out"),
    ("Sound", "stop", "stop \u{2014} stop all sound"),
    ("System", "clear", "clear \u{2014} clear the terminal"),
    ("System", "dmesg", "dmesg [word] \u{2014} kernel log, only lines with the word"),
    ("System", "echo", "echo <text> \u{2014} print text"),
    ("System", "help", "help [command] \u{2014} all commands, or how to use one"),
    ("System", "hwprobe", "hwprobe \u{2014} save a hardware report to the USB stick"),
    ("System", "wifi", "wifi [status|connect|off|firmware|test] \u{2014} networks around; passwords go in the Network window"),
    ("System", "bt", "bt [on|off|scan|devices|connect A|disconnect A|forget A|yes A|no A] \u{2014} Bluetooth: turn on/off, find, pair and connect devices"),
    ("System", "monitor", "monitor off|on \u{2014} act as if the cable of screen 2 was pulled or plugged"),
    ("System", "lsusb", "lsusb \u{2014} USB devices"),
    ("System", "irq", "irq \u{2014} device interrupts (MSI): vector, device, count"),
    ("System", "touchpad", "touchpad [on|off|trace|slow|fast|dsdt] \u{2014} touchpad state; off returns the plain mouse after a restart; trace: the last reads and frames; slow/fast: I2C at 100/400 kHz; dsdt: save the ACPI table to /tmp/dsdt.aml"),
    ("System", "reboot", "reboot [now|bios|usb] \u{2014} restart; windows with unsaved work ask first, now does not wait; bios opens the firmware setup, usb starts once from the USB stick"),
    ("System", "shutdown", "shutdown [now] \u{2014} turn the computer off; windows with unsaved work ask first, now does not wait"),
    ("System", "mem", "mem \u{2014} kernel memory"),
    ("System", "cpu", "cpu [one|all] \u{2014} the processor, its special instructions and the load of each core; one/all: run threads on core 0 only or on all cores"),
    ("System", "lid", "lid [open|closed|auto] \u{2014} show the laptop lid; act as if it moved, auto listens to the sensor again"),
    ("System", "display", "display [WIDTHxHEIGHT[@HZ]|keep|revert|off] \u{2014} screen modes, off = blank now; a new mode shows at once and goes back by itself unless kept"),
    ("System", "pci", "pci [BUS:DEV.FN [read BAR OFFSET [COUNT] | write BAR OFFSET VALUE]] \u{2014} PCI devices, one's configuration, its registers in hex (administrator)"),
    ("System", "gpu", "gpu [on|off | blur on|off | read OFFSET [COUNT] | write OFFSET VALUE | dpio REG [VALUE]] \u{2014} the GPU engines; on/off: frame output by the GPU or the CPU; blur on/off: glass by the GPU or the CPU; Bay Trail display registers in hex, for debugging (administrator)"),
    ("System", "remote", "remote [on|off|key|newkey|keep] \u{2014} remote access over the network: status, turn it on or off, show or change the key; keep makes the running trial kernel the main one"),
    ("System", "power", "power [light|deep|wm on|wm off] \u{2014} temperature, speed, watts; how deeply the processor sleeps; wm lets memory sleep behind the monitor"),
    ("System", "screenshot", "screenshot \u{2014} choose an area of the screen"),
    ("System", "time", "time [zone NAME|zones [WORD]|set DATE TIME|sync|internet on|off|SERVER|summer auto|on|off|hardware local|utc] \u{2014} date, time zone, summer time, internet time"),
    ("System", "saver", "saver \u{2014} show the screensaver now, any key or mouse move closes it"),
    ("System", "disks", "disks \u{2014} SATA disks and what is on them"),
    ("System", "install", "install \u{2014} put GlassOS on a SATA disk (opens the installer)"),
    ("System", "app", "app NAME \u{2014} open an app window: netcfg, bluetooth, settings, files, terminal, taskmgr, clock, paint, disks, about"),
    ("System", "bootorder", "bootorder \u{2014} the firmware boot list, * marks what started now"),
    ("System", "uname", "uname \u{2014} system version"),
    ("System", "uptime", "uptime \u{2014} time since startup"),
    ("System", "whoami", "whoami \u{2014} who is signed in"),
];
/// The gap between command columns and table columns.
const COLUMN_GAP: usize = 2;
const TABLE_GAP: usize = 3;
/// The line width before the window has been drawn.
const DEFAULT_COLS: usize = 72;

/// How a job will finish.
enum JobEnd {
    /// A ring 3 process: exit code.
    Process(Arc<IrqLock<Option<i32>>>),
}

/// How a job finished.
enum Finished {
    Process(i32),
}

/// A program started in its own kernel thread (a script or a process).
struct Job {
    /// The job number in this terminal (like %1 in bash).
    num: usize,
    name: String,
    thread: u64,
    /// The thread writes its `print` output here.
    output: Arc<IrqLock<Vec<u8>>>,
    end: JobEnd,
    background: bool,
    /// An unfinished output line.
    partial: String,
    /// The file the output goes to (`>`); None — the screen.
    save_to: Option<String>,
}

struct Shell {
    /// (prompt, text); command output has an empty prompt.
    lines: VecDeque<(String, String)>,
    input: String,
    /// The cursor in the input line (byte); None — at the end of the line.
    caret: Option<usize>,
    /// Where the input line starts on screen (from the last draw): x, y.
    input_at: (i32, i32),
    /// The left edge of input lines after the first (without the prompt).
    input_left: i32,
    cwd: String,
    cursor_on: bool,
    /// Entered commands (shared by all terminals, stored in a file).
    history: Vec<String>,
    /// The position when browsing history with arrows; None — editing a new line.
    history_pos: Option<usize>,
    /// An unfinished line put aside while browsing history.
    draft: String,
    /// How many lines the output is scrolled back (0 — at the bottom).
    scroll: usize,
    /// Lines on screen and the area size (from the last draw).
    rows: usize,
    size: (i32, i32),
    /// The thumb is being dragged: how far below its top it was grabbed.
    thumb_grab: Option<i32>,
    /// Interactive Lua is open (the `lua` command).
    repl: Option<lua::Repl>,
    /// Lua is busy: no prompt (to notice the change).
    repl_busy: bool,
    jobs: Vec<Job>,
    next_job: usize,
    /// Lines entered while a program was running: they run after it.
    typeahead: Vec<String>,
    /// How many characters fit in a line (from the last draw).
    cols: usize,
    /// A command runs with `>`: its output accumulates here, not on screen.
    capture: Option<String>,
    /// Where it will go. A started program takes this path for itself.
    save_to: Option<String>,
    /// A shell without a window (remote access): commands do not go to the history.
    headless: bool,
}

impl Shell {
    pub fn new() -> Self {
        Self {
            lines: VecDeque::from([(String::new(), format!("{OS_NAME} shell. Type 'help'."))]),
            input: String::new(),
            caret: None,
            input_at: (0, 0),
            input_left: 0,
            cwd: crate::access::home(),
            cursor_on: true,
            history: load_history(),
            history_pos: None,
            draft: String::new(),
            scroll: 0,
            rows: 1,
            size: (0, 0),
            thumb_grab: None,
            repl: None,
            repl_busy: false,
            jobs: Vec::new(),
            next_job: 1,
            typeahead: Vec::new(),
            cols: DEFAULT_COLS,
            capture: None,
            save_to: None,
            headless: false,
        }
    }

    /// A terminal with ready output (for example, of a program started from Files).
    pub fn with_output(lines: Vec<String>) -> Self {
        let mut t = Self::new();
        t.lines.clear();
        for line in lines {
            t.print(line);
        }
        t
    }

    /// Starts a program in a separate thread: the terminal (and the whole system) does not wait for it.
    /// The output is printed as it appears, the WM opens the window (if any).
    fn run_program(&mut self, path: &str, args: &str, background: bool) {
        let output = Arc::new(IrqLock::new(Vec::new()));
        let name = fs::name(path).trim_end_matches(".lua").to_string();
        let words = split_args(args);
        let argv: Vec<&str> = words.iter().map(String::as_str).collect();
        match lua::spawn(path, &argv, output.clone(), None) {
            Ok(p) => self.add_job(name, p.handle.id, output, JobEnd::Process(p.exit_code), background),
            Err(e) => self.print(e),
        }
    }

    /// Starts a program (ELF) as a user process in ring 3.
    fn run_process(&mut self, path: &str, args: &str, background: bool) {
        let output = Arc::new(IrqLock::new(Vec::new()));
        let name = fs::name(path).to_string();
        let words = split_args(args);
        let mut argv = vec![name.as_str()];
        argv.extend(words.iter().map(String::as_str));
        match process::spawn(path, &argv, output.clone()) {
            Ok(p) => self.add_job(name.clone(), p.handle.id, output, JobEnd::Process(p.exit_code), background),
            Err(e) => self.print(e),
        }
    }

    /// A built-in command that waits a long time (network): in its own thread, like a program.
    /// `f` prints through `out` and returns the exit code.
    fn run_native(&mut self, name: &str, background: bool, f: impl FnOnce(&dyn Fn(&str)) -> i32 + Send + 'static) {
        let output = Arc::new(IrqLock::new(Vec::new()));
        let code = Arc::new(IrqLock::new(None));
        let slot = code.clone();
        let handle = task::spawn(name, Some(output.clone()), move || {
            let out = |s: &str| {
                task::write_output(s.as_bytes());
                task::write_output(b"\n");
            };
            let c = f(&out);
            // Ctrl+C interrupted the wait — say so.
            *slot.lock() = Some(if task::stop_reason(0) == 2 { task::KILLED } else { c });
        });
        self.add_job(name.into(), handle.id, output, JobEnd::Process(code), background);
    }

    fn add_job(&mut self, name: String, thread: u64, output: Arc<IrqLock<Vec<u8>>>, end: JobEnd, background: bool) {
        let num = self.next_job;
        self.next_job += 1;
        if background {
            self.print(format!("[{num}] {name}"));
        }
        let save_to = self.save_to.clone();
        self.jobs.push(Job { num, name, thread, output, end, background, partial: String::new(), save_to });
    }

    /// The foreground job: while it runs, there is no prompt.
    fn foreground(&self) -> Option<&Job> {
        self.jobs.iter().find(|j| !j.background)
    }

    /// A job's output: to its file or to the screen.
    fn job_output(&mut self, target: &Option<String>, text: String) {
        match target {
            Some(path) => {
                if let Err(e) = fs::append(path, format!("{text}\n").as_bytes()) {
                    self.print(format!("{path}: {e}"));
                }
            }
            None => self.print(text),
        }
    }

    /// Takes the jobs' output and the results of finished ones. true — something was printed.
    fn poll_jobs(&mut self) -> bool {
        let mut changed = false;
        let mut i = 0;
        while i < self.jobs.len() {
            let fresh = core::mem::take(&mut *self.jobs[i].output.lock());
            let finished = match &self.jobs[i].end {
                JobEnd::Process(r) => r.lock().take().map(Finished::Process),
            };
            if !fresh.is_empty() {
                let job = &mut self.jobs[i];
                job.partial.push_str(&String::from_utf8_lossy(&fresh));
                let text = core::mem::take(&mut job.partial);
                let (done, rest) = match text.rfind('\n') {
                    Some(n) => (text[..n].to_string(), text[n + 1..].to_string()),
                    None => (String::new(), text),
                };
                job.partial = rest;
                let target = job.save_to.clone();
                if !done.is_empty() || fresh.contains(&b'\n') {
                    self.job_output(&target, done);
                }
                changed = true;
            }
            let Some(finished) = finished else {
                i += 1;
                continue;
            };
            let job = self.jobs.remove(i);
            // The last output lines may have come together with the exit.
            let tail = core::mem::take(&mut *job.output.lock());
            let rest = format!("{}{}", job.partial, String::from_utf8_lossy(&tail));
            if !rest.is_empty() {
                self.job_output(&job.save_to, rest.trim_end_matches('\n').to_string());
            }
            let state = match finished {
                Finished::Process(task::KILLED) => "killed".into(),
                Finished::Process(task::CRASHED) => "crashed".into(),
                Finished::Process(0) => "done".into(),
                Finished::Process(code) => {
                    if !job.background {
                        self.print(format!("exit code {code}"));
                    }
                    format!("exit {code}")
                }
            };
            if job.background {
                self.print(format!("[{}] {state}  {}", job.num, job.name));
            }
            changed = true;
        }
        changed
    }

    /// The tab label: the running program or the current folder.
    fn label(&self) -> String {
        if let Some(job) = self.foreground() {
            return job.name.clone();
        }
        if self.repl.is_some() {
            return "lua".into();
        }
        match self.cwd.strip_prefix(crate::access::home().as_str()) {
            Some("") => "~".into(),
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => self.cwd.clone(),
        }
    }

    fn prompt(&self) -> String {
        if self.foreground().is_some() {
            return String::new();
        }
        if let Some(repl) = &self.repl {
            return if repl.busy() { String::new() } else { "lua> ".into() };
        }
        let dir = match self.cwd.strip_prefix(crate::access::home().as_str()) {
            Some("") => "~".to_string(),
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => self.cwd.clone(),
        };
        format!("{dir} $ ")
    }

    /// `display`: modes of the screen we set ourselves (Bay Trail, the laptop monitor,
    /// the VM's second screen). A new mode without `display keep` reverts by itself after 15 seconds.
    fn display(&mut self, arg: &str) {
        use crate::output;
        let name = |m: &crate::hdmi::Resolution| format!("{}x{}@{}", m.w, m.h, m.hz);
        if arg == "off" {
            // Like the "turn off the display now" button: wakes up from the mouse or keyboard.
            crate::saver::display_off();
            self.print("display: off until the mouse or keyboard is used");
            return;
        }
        if !output::own_primary() && output::count() < 2 {
            let (w, h) = crate::display::current();
            let modes: Vec<String> = crate::display::modes().iter().map(|(w, h)| format!("{w}x{h}")).collect();
            self.print(format!("display: {w}x{h}, set by the firmware; modes: {}", modes.join(" ")));
            self.print("display: choose one in Settings, it applies after a restart");
            return;
        }
        let modes = output::monitor_modes();
        match arg {
            "" => {
                let now = output::monitor_mode().map_or("unknown".into(), |m| name(&m));
                let first = if output::own_primary() { "the firmware mode" } else { "native" };
                let list: Vec<String> = modes.iter().map(name).collect();
                self.print(format!("display: now {now}"));
                self.print(format!("display: modes {} (the first is {first})", list.join(" ")));
                if let Some(left) = output::seconds_to_revert() {
                    self.print(format!("display: goes back in {left} s unless you type display keep"));
                }
            }
            "keep" => match output::seconds_to_revert() {
                Some(_) => {
                    output::keep();
                    self.print("display: kept, it stays after a restart too");
                }
                None => self.print("display: nothing to keep"),
            },
            "revert" => match output::seconds_to_revert() {
                Some(_) => output::revert(),
                None => self.print("display: nothing to revert"),
            },
            want => {
                let (size, hz) = want.split_once('@').map_or((want, None), |(s, hz)| (s, hz.parse::<i32>().ok()));
                let wanted = size.split_once('x').and_then(|(w, h)| Some((w.parse::<i32>().ok()?, h.parse::<i32>().ok()?)));
                let Some((w, h)) = wanted else {
                    self.print("usage: display [WIDTHxHEIGHT[@HZ]|keep|revert]");
                    return;
                };
                match modes.iter().position(|m| (m.w, m.h) == (w, h) && hz.is_none_or(|hz| m.hz == hz)) {
                    Some(i) => {
                        output::change(output::Layout { mode: (i != 0).then_some(modes[i]), ..output::current() });
                        self.print(format!("display: switching to {}; type display keep within 15 s, or it goes back", name(&modes[i])));
                    }
                    None => self.print(format!("display: no {want} here, see display")),
                }
            }
        }
    }

    /// `pci`: bus devices, the configuration of one and its registers through a BAR. For
    /// exploring new hardware, including over the network. Numbers are in hex.
    /// `route`: show, add or delete network routes. `-p` (or `permanent`)
    /// saves the route to a file so it survives a reboot.
    fn route(&mut self, arg: &str) {
        const USAGE: &str = "usage: route [add DEST/LEN [GATEWAY] [-p] | del DEST/LEN [-p]]";
        let mut words = arg.split_whitespace();
        match words.next() {
            None | Some("show" | "list") => {
                for line in net::route_lines() {
                    self.print(line);
                }
            }
            Some(op @ ("add" | "del" | "delete")) => {
                let cidr = words.next();
                let rest: Vec<&str> = words.collect();
                let permanent = rest.iter().any(|&x| x == "-p" || x == "permanent");
                let Some((dest, mask)) = cidr.and_then(net::parse_cidr) else {
                    return self.print(USAGE);
                };
                if op == "add" {
                    let gw = match rest.iter().copied().find(|&x| x != "-p" && x != "permanent") {
                        Some(g) => match net::parse_addr(g) {
                            Some(ip) => ip,
                            None => return self.print("route: bad gateway address"),
                        },
                        None => [0; 4],
                    };
                    match net::add_route(dest, mask, gw, permanent) {
                        Ok(()) => self.print(format!("route added{}", if permanent { " (permanent)" } else { "" })),
                        Err(e) => self.print(format!("route: {e}")),
                    }
                } else {
                    match net::del_route(dest, mask, permanent) {
                        Ok(()) => self.print(format!("route deleted{}", if permanent { " (from the saved list too)" } else { "" })),
                        Err(e) => self.print(format!("route: {e}")),
                    }
                }
            }
            _ => self.print(USAGE),
        }
    }

    fn pci(&mut self, arg: &str) {
        const USAGE: &str = "usage: pci [BUS:DEV.FN [read BAR OFFSET [COUNT] | write BAR OFFSET VALUE]]";
        let devices = crate::pci::scan();
        let mut words = arg.split_whitespace();
        let Some(id) = words.next() else {
            for d in &devices {
                self.print(format!("{:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}{:02x}", d.bus, d.slot, d.func, d.vendor, d.device, d.class, d.subclass));
            }
            return;
        };
        let parsed = id.split_once(':').and_then(|(b, rest)| {
            let (s, f) = rest.split_once('.')?;
            Some((u8::from_str_radix(b, 16).ok()?, u8::from_str_radix(s, 16).ok()?, f.parse::<u8>().ok()?))
        });
        let Some(d) = parsed.and_then(|(b, s, f)| devices.iter().find(|d| (d.bus, d.slot, d.func) == (b, s, f)).copied()) else {
            return self.print(format!("pci: no device {id}"));
        };
        let hex = |s: Option<&str>| s.and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok());
        match words.next() {
            None => {
                for row in (0..=255u8).step_by(16) {
                    let words: Vec<String> = (0..4).map(|i| format!("{:08x}", d.read(row + i * 4))).collect();
                    self.print(format!("{row:02x}: {}", words.join(" ")));
                }
                // The BAR size is not measured: for that the device stops answering for a moment, and
                // it may be busy (disk, network, video).
                for n in 0..6 {
                    let raw = d.read(0x10 + n * 4);
                    if raw != 0 {
                        self.print(format!("bar {n}: {:#x}{}", d.bar(n), if raw & 1 != 0 { ", I/O" } else { "" }));
                    }
                }
            }
            Some(op @ ("read" | "write")) => {
                if !crate::access::is_admin() {
                    return self.print("pci: only an administrator can touch device registers");
                }
                let (Some(bar), Some(offset)) = (words.next().and_then(|b| b.parse::<u8>().ok()), hex(words.next())) else {
                    return self.print(USAGE);
                };
                if bar > 5 || d.bar(bar) == 0 || d.read(0x10 + bar * 4) & 1 != 0 {
                    return self.print(format!("pci: bar {bar} is not memory"));
                }
                if offset % 4 != 0 || offset >= 16 << 20 {
                    return self.print("pci: the offset must be aligned and below 16 MiB");
                }
                let count = words.clone().nth(if op == "write" { 1 } else { 0 }).and_then(|c| c.parse::<u64>().ok()).unwrap_or(1).clamp(1, 256);
                let size = (offset + 4 * count).next_multiple_of(4096);
                let base = d.mmio(bar, size);
                if op == "write" {
                    let Some(value) = hex(words.next()) else { return self.print(USAGE) };
                    crate::hwprobe::wr(base, offset as usize, value as u32);
                    let now = crate::hwprobe::rd(base, offset as usize);
                    return self.print(format!("{offset:05x} <- {value:08x}, reads {now:08x}"));
                }
                for i in 0..count {
                    let at = offset + 4 * i;
                    self.print(format!("{at:05x} = {:08x}", crate::hwprobe::rd(base, at as usize)));
                }
            }
            _ => self.print(USAGE),
        }
    }

    /// `gpu`: Bay Trail display registers for debugging (including over the network). Numbers are in hex.
    fn gpu(&mut self, arg: &str) {
        const USAGE: &str = "usage: gpu [on|off | read OFFSET [COUNT] | write OFFSET VALUE | dpio REG [VALUE]]";
        if let Some(n) = arg.strip_prefix("blur offset ").and_then(|n| n.trim().parse::<u32>().ok()) {
            crate::gpu::set_blur_offset(n);
            crate::wm::repaint_all();
            self.print(format!("gpu: the blur cascade goes {n}/4 of a level deeper than log2(radius)"));
            return;
        }
        if let Some(how) = arg.strip_prefix("blur ").map(str::trim).filter(|h| *h == "on" || *h == "off") {
            crate::gpu::set_blur(how == "on");
            crate::wm::repaint_all();
            self.print(if how == "on" { "gpu: the glass is blurred by the GPU" } else { "gpu: the glass is blurred by the CPU" });
            return;
        }
        if arg == "off" || arg == "on" {
            let on = arg == "on";
            self.print(if crate::gpu::set_output(on) {
                if on { "gpu: the GPU copies frames to the screen" } else { "gpu: the CPU copies frames to the screen" }
            } else {
                "gpu: no working GPU engine"
            });
            crate::wm::repaint_all();
            return;
        }
        if arg.is_empty() {
            let state = crate::gpu::state();
            self.print(if state.is_empty() { "gpu: not started yet (it checks itself about 25 s after startup)".into() } else { format!("gpu: {state}") });
            return;
        }
        if !crate::access::is_admin() {
            self.print("gpu: only an administrator can touch the display registers");
            return;
        }
        let hex = |s: Option<&str>| s.and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok());
        let mut words = arg.split_whitespace();
        let (what, first, second) = (words.next(), hex(words.next()), words.next());
        match (what, first) {
            (Some("read"), Some(offset)) => {
                let count = second.and_then(|c| c.parse::<usize>().ok()).unwrap_or(1).clamp(1, 256);
                for i in 0..count {
                    let at = offset as usize + 4 * i;
                    match crate::baytrail::peek(at) {
                        Some(v) => self.print(format!("{at:06x} = {v:08x}")),
                        None => return self.print("gpu: no Bay Trail display here, or the offset is outside the registers"),
                    }
                }
            }
            (Some("write"), Some(offset)) => match hex(second) {
                Some(value) if crate::baytrail::poke(offset as usize, value) => {
                    let now = crate::baytrail::peek(offset as usize).unwrap_or(0);
                    self.print(format!("{offset:06x} <- {value:08x}, reads {now:08x}"));
                }
                Some(_) => self.print("gpu: no Bay Trail display here, or the offset is outside the registers"),
                None => self.print(USAGE),
            },
            (Some("dpio"), Some(reg)) => match hex(second) {
                Some(value) if crate::baytrail::dpio_poke(reg, value) => {
                    let now = crate::baytrail::dpio_peek(reg).unwrap_or(0);
                    self.print(format!("dpio {reg:04x} <- {value:08x}, reads {now:08x}"));
                }
                Some(_) => self.print("gpu: no Bay Trail display here"),
                None => match crate::baytrail::dpio_peek(reg) {
                    Some(v) => self.print(format!("dpio {reg:04x} = {v:08x}")),
                    None => self.print("gpu: no Bay Trail display here"),
                },
            },
            _ => self.print(USAGE),
        }
    }

    /// Prints text; whatever does not fit the window width wraps to the next line.
    fn print(&mut self, s: impl Into<String>) {
        let s = s.into();
        if let Some(file) = &mut self.capture {
            file.push_str(&s);
            file.push('\n');
            return;
        }
        let before = self.lines.len();
        for line in s.split('\n') {
            let chars: Vec<char> = line.replace('\t', "    ").chars().collect();
            let mut rest = &chars[..];
            while rest.len() > self.cols.max(1) {
                // Break at a space; a word longer than the line — where the room ran out.
                let cols = self.cols.max(1);
                let cut = rest[..=cols].iter().rposition(|&c| c == ' ').filter(|&i| i > 0).unwrap_or(cols);
                self.lines.push_back((String::new(), rest[..cut].iter().collect()));
                rest = &rest[cut..];
                if rest.first() == Some(&' ') {
                    rest = &rest[1..];
                }
            }
            self.lines.push_back((String::new(), rest.iter().collect()));
        }
        // For someone reading old output, new lines must not shift the text.
        if self.scroll > 0 {
            self.scroll += self.lines.len() - before;
        }
        let keep = theme::get().scrollback as usize;
        while self.lines.len() > keep {
            self.lines.pop_front();
        }
    }

    /// A table: the first row is headers centred in the columns, in the rows below the text
    /// is aligned to the right edge of the column.
    /// `left` — text columns: they are aligned left, numbers right.
    fn table<const N: usize>(&mut self, rows: &[[String; N]], left: &[usize]) {
        let widths: [usize; N] = core::array::from_fn(|n| rows.iter().map(|r| r[n].chars().count()).max().unwrap_or(0));
        for row in rows {
            let cells: Vec<String> = row
                .iter()
                .zip(widths)
                .enumerate()
                .map(|(n, (cell, w))| {
                    let free = w - cell.chars().count();
                    if left.contains(&n) { format!("{cell}{}", " ".repeat(free)) } else { format!("{}{cell}", " ".repeat(free)) }
                })
                .collect();
            self.print(cells.join(&" ".repeat(TABLE_GAP)).trim_end());
        }
    }

    /// Help: sections and commands in alphabetical order, in columns by window width. With a
    /// command name — how to use it.
    /// Date and time: without words — a summary, with a word — a setting.
    fn time(&mut self, arg: &str) {
        use crate::time;
        let (word, rest) = arg.split_once(' ').map_or((arg, ""), |(w, r)| (w, r.trim()));
        let done = |r: Result<(), &'static str>| r.err().map(String::from);
        let error = match word {
            "" => {
                let (now, zone, set) = (time::now(), time::zone(), time::settings());
                let utc = time::unix();
                self.print(format!("{}  {:02}:{:02}:{:02}", time::long_date(&now), now.hour, now.min, now.sec));
                let season = if time::summer_now() { "summer time" } else { "winter time" };
                let guessed = if time::zone_chosen() { "" } else { ", guessed from the clock" };
                self.print(format!("  zone      {}, {}, {season}{guessed}", zone.name, time::offset_text(time::offset_at(utc))));
                let summer = match (zone.rule.is_some(), set.auto_summer, time::change_text()) {
                    (true, true, Some(next)) => format!("changes by itself, next {next}"),
                    (_, false, _) => "set by hand".into(),
                    _ => "none in this zone".into(),
                };
                self.print(format!("  summer    {summer}"));
                let last = match time::last_sync() {
                    None => "no check yet".into(),
                    Some(r) => match r.result {
                        Ok(ms) => format!("last check {}, off by {} ms", time::stamp(r.at), ms.abs()),
                        Err(e) => format!("last check {}: {e}", time::stamp(r.at)),
                    },
                };
                self.print(format!("  internet  {}, {}; {last}", if set.ntp { "on" } else { "off" }, set.server()));
                self.print(format!("  hardware  keeps {}", if set.rtc_utc { "universal time" } else { "local time" }));
                None
            }
            "zones" => {
                let filter = rest.to_ascii_lowercase();
                for z in time::zones().iter().filter(|z| z.name.to_ascii_lowercase().contains(&filter)) {
                    let (minutes, summer) = time::zone_now(z);
                    self.print(format!("{:<24}{}{}", z.name, time::offset_text(minutes), if summer { "  summer" } else { "" }));
                }
                None
            }
            "zone" => match time::find_zone(rest) {
                Some(z) => done(time::change(|s| s.zone = Some(z.name))),
                None => Some(format!("no single zone matches \"{rest}\", see time zones")),
            },
            "set" => {
                let (date, clock) = rest.split_once(' ').unwrap_or((rest, ""));
                match super::datetime::parse_clock(date, clock) {
                    Some(t) if time::settings().ntp => Some(format!("internet time is on, turn it off first: time internet off ({:02}:{:02} not set)", t.hour, t.min)),
                    Some(t) => done(time::set_local(&t)),
                    None => Some("write it as time set 2026-11-02 14:05".into()),
                }
            }
            "sync" => {
                time::request_sync();
                self.print(format!("asking {}; see time in a few seconds", time::settings().server()));
                None
            }
            "internet" => match rest {
                "on" => done(time::change(|s| s.ntp = true)),
                "off" => done(time::change(|s| s.ntp = false)),
                "" => Some("usage: time internet on|off|SERVER".into()),
                server => {
                    let server = server.to_string();
                    done(time::change(|s| (s.ntp, s.server) = (true, server))).or_else(|| {
                        time::request_sync();
                        None
                    })
                }
            },
            "summer" => match rest {
                "auto" => done(time::change(|s| s.auto_summer = true)),
                "on" | "off" => done(time::change(|s| (s.auto_summer, s.summer) = (false, rest == "on"))),
                _ => Some("usage: time summer auto|on|off".into()),
            },
            "hardware" => match rest {
                "local" | "utc" => done(time::change(|s| s.rtc_utc = rest == "utc")),
                _ => Some("usage: time hardware local|utc".into()),
            },
            _ => Some("usage: time [zone NAME|zones [WORD]|set DATE TIME|sync|internet on|off|SERVER|summer auto|on|off|hardware local|utc]".into()),
        };
        if let Some(e) = error {
            self.print(e);
        }
    }

    fn help(&mut self, command: &str) {
        if !command.is_empty() {
            match COMMANDS.iter().find(|c| c.1 == command) {
                Some(c) => self.print(c.2),
                None => self.print(format!("help: no such command: {command}")),
            }
            return;
        }
        let mut sorted = COMMANDS;
        sorted.sort_by_key(|c| (c.0, c.1));
        let width = sorted.iter().map(|c| c.1.len()).max().unwrap_or(0) + COLUMN_GAP;
        let columns = (self.cols.saturating_sub(COLUMN_GAP) / width).max(1);
        for section in sorted.chunk_by(|a, b| a.0 == b.0) {
            match &mut self.capture {
                Some(file) => file.push_str(&format!("{}\n", section[0].0)),
                None => self.lines.push_back((section[0].0.into(), String::new())),
            }
            // Read top to bottom, like columns in a book.
            let rows = section.len().div_ceil(columns);
            for row in 0..rows {
                let names: String = section.iter().skip(row).step_by(rows).map(|c| format!("{:<width$}", c.1)).collect();
                self.print(format!("  {}", names.trim_end()));
            }
        }
        self.print("help <command> shows how to use it. Programs run by name, from here, /apps or /bin.");
        self.print("command > file saves the output, >> appends to it.");
        self.print("name & runs in the background, Ctrl+C stops. Ctrl+T new tab, Ctrl+W close, Ctrl+PgUp/PgDn switch, PgUp/PgDn scroll.");
    }

    fn report(&mut self, cmd: &str, result: fs::Result<()>) {
        if let Err(e) = result {
            self.print(format!("{cmd}: {e}"));
        }
    }

    fn path(&self, arg: &str) -> String {
        fs::resolve(&self.cwd, arg)
    }

    fn remember(&mut self, line: &str) {
        self.history_pos = None;
        self.draft.clear();
        let line = line.trim();
        if line.is_empty() || self.headless {
            return;
        }
        // Another terminal may have appended to the history — take the fresh one from the file.
        self.history = load_history();
        if self.history.last().map(String::as_str) != Some(line) {
            self.history.push(line.into());
        }
        if self.history.len() > HISTORY_MAX {
            self.history.drain(..self.history.len() - HISTORY_MAX);
        }
        let mut text = self.history.join("\n");
        text.push('\n');
        let _ = fs::write(&crate::access::config(HISTORY_FILE), text.as_bytes());
    }

    fn history_up(&mut self) {
        let pos = match self.history_pos {
            _ if self.history.is_empty() => return,
            None => {
                self.draft = core::mem::take(&mut self.input);
                self.history.len() - 1
            }
            Some(p) => p.saturating_sub(1),
        };
        self.history_pos = Some(pos);
        self.input = self.history[pos].clone();
        self.caret = None;
    }

    fn history_down(&mut self) {
        match self.history_pos {
            None => {}
            Some(p) if p + 1 < self.history.len() => {
                self.history_pos = Some(p + 1);
                self.input = self.history[p + 1].clone();
                self.caret = None;
            }
            Some(_) => {
                self.history_pos = None;
                self.input = core::mem::take(&mut self.draft);
                self.caret = None;
            }
        }
    }

    /// Completions for the word under the cursor: commands and programs for the first word,
    /// paths for the rest (and for the first one if it contains `/`).
    fn candidates(&self, word: &str, first: bool) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if first && !word.contains('/') {
            out.extend(COMMANDS.iter().filter(|c| c.1.starts_with(word)).map(|c| c.1.to_string()));
            for dir in ["/apps", self.cwd.as_str()] {
                for e in fs::list(dir).unwrap_or_default() {
                    if let Some(stem) = e.name.strip_suffix(".lua")
                        && !e.is_dir
                        && stem.starts_with(word)
                    {
                        out.push(stem.to_string());
                    }
                }
            }
            for e in fs::list("/bin").unwrap_or_default() {
                if !e.is_dir && e.name.starts_with(word) {
                    out.push(e.name);
                }
            }
        } else {
            let (dir_part, prefix) = word.rsplit_once('/').map_or(("", word), |(d, p)| (d, p));
            let dir_part = if word.contains('/') { format!("{dir_part}/") } else { String::new() };
            let dir = fs::resolve(&self.cwd, if dir_part.is_empty() { "." } else { &dir_part });
            for e in fs::list(&dir).unwrap_or_default() {
                if e.name.starts_with(prefix) {
                    out.push(format!("{dir_part}{}{}", e.name, if e.is_dir { "/" } else { "" }));
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Tab completion.
    fn complete(&mut self) {
        // Complete the word before the cursor; whatever is to the right stays in place.
        let caret = self.caret();
        let tail = self.input.split_off(caret);
        self.complete_end();
        self.caret = (!tail.is_empty()).then_some(self.input.len());
        self.input.push_str(&tail);
        self.clip_input();
    }

    /// The input line no longer than INPUT_MAX, truncated at a character boundary.
    /// The entered line goes to the output, wrapped at the window width: the first part with the prompt,
    /// the continuation marked `CONT` (drawn in the input colour, without the prompt).
    fn echo(&mut self, prompt: String, text: String) {
        let cols = self.cols.max(1);
        let chars: Vec<char> = text.chars().collect();
        let first = cols.saturating_sub(prompt.chars().count()).max(1);
        let (head, mut rest) = chars.split_at(first.min(chars.len()));
        self.lines.push_back((prompt, head.iter().collect()));
        while !rest.is_empty() {
            let (line, tail) = rest.split_at(cols.min(rest.len()));
            self.lines.push_back((String::from(CONT), line.iter().collect()));
            rest = tail;
        }
    }

    /// Wrapped input lines: (start byte, end byte) of each; the first has
    /// a prompt of `prompt_chars` characters before it. Room for the cursor at the end is accounted for.
    fn input_rows(&self, prompt_chars: usize) -> Vec<(usize, usize)> {
        let cols = self.cols.max(1);
        let mut rows = Vec::new();
        let (mut start, mut used) = (0, prompt_chars);
        for (i, _) in self.input.char_indices() {
            if used >= cols {
                rows.push((start, i));
                (start, used) = (i, 0);
            }
            used += 1;
        }
        rows.push((start, self.input.len()));
        // The cursor at the end of a full line moves to the next one.
        if used >= cols {
            rows.push((self.input.len(), self.input.len()));
        }
        rows
    }

    fn input_height(&self) -> usize {
        self.input_rows(self.prompt().chars().count()).len()
    }

    fn clip_input(&mut self) {
        let mut n = self.input.len().min(INPUT_MAX);
        while !self.input.is_char_boundary(n) {
            n -= 1;
        }
        self.input.truncate(n);
    }

    fn complete_end(&mut self) {
        let start = self.input.rfind(' ').map_or(0, |i| i + 1);
        let word = self.input[start..].to_string();
        let found = self.candidates(&word, start == 0);
        let Some(first) = found.first() else { return };
        let common = found.iter().fold(first.as_str(), |acc, c| {
            let same = acc.chars().zip(c.chars()).take_while(|(a, b)| a == b);
            &acc[..same.map(|(a, _)| a.len_utf8()).sum()]
        });
        if found.len() == 1 {
            let done = !first.ends_with('/');
            self.input.truncate(start);
            self.input.push_str(first);
            if done {
                self.input.push(' ');
            }
        } else if common.len() > word.len() {
            let common = common.to_string();
            self.input.truncate(start);
            self.input.push_str(&common);
        } else {
            // Nothing to append — show the options, like bash.
            let prompt = self.prompt();
            self.echo(prompt, self.input.clone());
            let names: Vec<&str> = found.iter().map(|c| c.trim_end_matches('/').rsplit('/').next().unwrap_or(c)).collect();
            self.print(names.join("   "));
        }
        self.clip_input();
    }

    /// Takes the Lua output; true — something changed on screen.
    fn poll_repl(&mut self) -> bool {
        let Some(repl) = &self.repl else { return false };
        let (output, busy) = (repl.take_output(), repl.busy());
        let ended = !repl.alive();
        let changed = !output.is_empty() || ended || busy != self.repl_busy;
        self.repl_busy = busy;
        if ended {
            self.repl = None;
        }
        for line in output {
            self.print(line);
        }
        changed
    }

    fn exec(&mut self, ctx: &Ctx) {
        let line = core::mem::take(&mut self.input);
        self.caret = None;
        self.remember(&line);
        let prompt = self.prompt();
        if self.repl.is_some() {
            self.echo(prompt, line.clone());
            if line.trim() == "exit" {
                self.repl = None;
            } else if !line.trim().is_empty()
                && let Some(repl) = &self.repl
            {
                repl.eval(&line);
            }
            return;
        }
        self.echo(prompt, line.clone());
        let line = line.trim();
        let (line, background) = match line.strip_suffix('&') {
            Some(rest) => (rest.trim_end(), true),
            None => (line, false),
        };
        let (line, redirect) = split_redirect(line);
        if let Some((file, append)) = redirect {
            let path = self.path(&file);
            // The file is created right away and the output is appended: the program writes it in pieces.
            if !append && let Err(e) = fs::write(&path, b"") {
                self.print(format!("{file}: {e}"));
                return;
            }
            self.capture = Some(String::new());
            self.save_to = Some(path);
        }
        self.command(line, background, ctx);
        // Output is only taken for a redirect: without a window (remote access) it
        // accumulates here too and is needed in full.
        if let Some(path) = self.save_to.take() {
            let text = self.capture.take().unwrap_or_default();
            if let Err(e) = fs::append(&path, text.as_bytes()) {
                self.print(format!("{path}: {e}"));
            }
        }
    }

    fn command(&mut self, line: &str, background: bool, ctx: &Ctx) {
        let (cmd, arg) = line.split_once(' ').map_or((line, ""), |(c, a)| (c, a.trim()));
        match cmd {
            "" => {}
            "help" => self.help(arg),
            "clear" => self.lines.clear(),
            "time" => self.time(arg),
            "uptime" => {
                let s = ctx.ticks / HZ;
                self.print(format!("up {}m {}s", s / 60, s % 60));
            }
            "mem" => {
                let (used, free) = crate::heap_stats();
                self.print(format!("memory: {} used, {} free of {}", size_text(used as u64), size_text(free as u64), size_text((used + free) as u64)));
            }
            "cpu" if arg == "one" || arg == "all" => {
                crate::task::set_spread(arg == "all");
                self.print(if arg == "all" { "threads run on all cores" } else { "threads run on core 0 only (helpers just help with graphics)" });
            }
            #[cfg(feature = "debug-tools")]
            "crashtest" => match arg {
                // A test of the crash report: hang while holding a lock (the detector leaves
                // a note, the watchdog resets the board), or crash with a panic.
                "hang" => {
                    self.print("crashtest: hanging with a lock held; the watchdog restarts the computer in 30-60 s");
                    static TRAP: crate::sync::IrqLock<()> = crate::sync::IrqLock::new(());
                    let _held = TRAP.lock();
                    let _again = TRAP.lock();
                }
                "panic" => panic!("crashtest: a test panic from the terminal"),
                // A slow frame: the desktop is busy for 3 s, interrupts keep going. The mouse cursor
                // must keep moving meanwhile.
                "stall" => {
                    let until = crate::interrupts::ticks() + 3 * crate::interrupts::HZ;
                    while crate::interrupts::ticks() < until {
                        core::hint::spin_loop();
                    }
                    self.print("crashtest: the desktop was busy for 3 s");
                }
                _ => self.print("usage: crashtest hang|panic|stall \u{2014} test the crash report (debug build), or a 3 s busy desktop"),
            },
            "cpu" => {
                for line in crate::cpu::report() {
                    self.print(line);
                }
            }
            "uname" => self.print(fs::OS_VERSION.trim_end()),
            "disks" => {
                let disks = crate::install::targets();
                if disks.is_empty() {
                    self.print("no SATA disks (AHCI)");
                }
                for t in disks {
                    let note = if t.system { String::from("system") } else { t.contents.clone() };
                    self.print(format!("sata{}  {:<28} {:>9}  {note}", t.disk, t.model, t.size()));
                }
            }
            "install" => crate::wm::request(crate::wm::Request::Launch("install")),
            "app" => {
                let (name, rest) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
                let kind = match name {
                    "netcfg" | "network" => "netcfg",
                    "bluetooth" | "bt" => "bluetooth",
                    "settings" => "settings",
                    "files" => "files",
                    "terminal" => "terminal",
                    "taskmgr" | "tasks" => "taskmgr",
                    "clock" => "clock",
                    "recorder" | "rec" => "recorder",
                    "paint" => "paint",
                    "disks" => "disks",
                    "about" => "about",
                    _ => {
                        self.print("usage: app NAME [ARG] \u{2014} netcfg|bluetooth|settings|files|terminal|taskmgr|clock|recorder|paint|disks|about");
                        return;
                    }
                };
                let rest = rest.trim();
                if rest.is_empty() {
                    crate::wm::request(crate::wm::Request::Launch(kind));
                } else {
                    crate::wm::request(crate::wm::Request::LaunchArg(kind, rest.into()));
                }
            }
            "bootorder" => {
                for line in crate::bootvars::list() {
                    self.print(line);
                }
            }
            "whoami" => {
                let name = crate::access::user().unwrap_or_default();
                let role = if crate::access::is_admin() { "administrator" } else { "user" };
                self.print(format!("{name} ({role}), home {}", crate::access::home()));
            }
            "hostname" | "workgroup" => {
                let mut id = net::identity::get();
                let hostname = cmd == "hostname";
                let value = arg.trim().to_uppercase();
                if value.is_empty() {
                    self.print(if hostname { id.name } else { id.workgroup });
                } else if !net::identity::valid_name(&value, !hostname) {
                    self.print("letters, digits, - and _ only, up to 15");
                } else {
                    if hostname {
                        id.name = value;
                    } else {
                        id.workgroup = value;
                    }
                    if !net::identity::set(id) {
                        self.print("only an administrator can change this");
                    }
                }
            }
            "df" => {
                let mut rows = vec![["Mounted on", "Type", "Size", "Used", "Free", "Label"].map(String::from)];
                for v in fs::volumes() {
                    let label = v.name;
                    rows.push([v.path, v.kind.into(), size_text(v.total), size_text(v.used), size_text(v.total - v.used), label]);
                }
                self.table(&rows, &[0, 1, 5]);
            }
            "screenshot" => wm::request(wm::Request::Screenshot),
            "saver" => crate::saver::request(),
            "share" => {
                let mut words = arg.split_whitespace();
                let Some(dir) = words.next().map(|d| self.path(d)) else {
                    self.print("usage: share <folder> [name]");
                    return;
                };
                if !fs::is_dir(&dir) {
                    self.print(format!("share: {dir}: not a folder"));
                    return;
                }
                let name = words.next().map(String::from).unwrap_or_else(|| String::from(if dir == "/" { "root" } else { fs::name(&dir) }));
                let mut c = net::smbd::config();
                c.shares.retain(|(n, _)| !n.eq_ignore_ascii_case(&name));
                c.shares.push((name.clone(), dir.clone()));
                if !net::smbd::save(&c) {
                    self.print(ADMIN_ONLY);
                    return;
                }
                net::smbd::start();
                let ip = net::info().map_or(String::from("this computer"), |i| net::ip_str(i.ip));
                self.print(format!("sharing {dir} as \\\\{ip}\\{name}"));
            }
            "unshare" => {
                let mut c = net::smbd::config();
                let before = c.shares.len();
                c.shares.retain(|(n, _)| !n.eq_ignore_ascii_case(arg.trim()));
                if !net::smbd::save(&c) {
                    self.print(ADMIN_ONLY);
                    return;
                }
                self.print(if c.shares.len() < before { "stopped sharing" } else { "unshare: no such share" });
            }
            "shares" => {
                let c = net::smbd::config();
                if c.shares.is_empty() {
                    self.print("no shared folders (share <folder> [name])");
                }
                for (name, path) in &c.shares {
                    self.print(format!("{name:<16} {path}"));
                }
                self.print(if c.password.is_empty() { "access: guest (no password)" } else { "access: any user name + the share password" });
            }
            "sharepass" => {
                let mut c = net::smbd::config();
                c.password = String::from(arg.trim());
                if !net::smbd::save(&c) {
                    self.print(ADMIN_ONLY);
                    return;
                }
                self.print(if c.password.is_empty() { "guest access, no password" } else { "password set" });
            }
            "mount" => {
                let mut words = arg.split_whitespace();
                let (Some(url), name) = (words.next().map(String::from), words.next().map(String::from)) else {
                    self.print("usage: mount smb://[user[:password]@]server[:port]/share [name]");
                    return;
                };
                self.run_native("mount", background, move |out| match crate::net::smb::mount(&url, name.as_deref()) {
                    Ok(path) => {
                        out(&format!("mounted at {path}"));
                        0
                    }
                    Err(e) => {
                        out(&format!("mount: {e}"));
                        1
                    }
                });
            }
            "eject" | "umount" => {
                let path = if arg.is_empty() { String::from("/media/usb0") } else { self.path(arg) };
                match fs::eject(&path) {
                    Ok(()) => self.print(format!("{path}: saved, safe to remove")),
                    Err(e) => self.print(format!("eject: {path}: {e}")),
                }
            }
            "wifi" if arg == "status" => self.print(crate::wifi::status()),
            "wifi" if arg.starts_with("connect ") => {
                let rest = arg["connect ".len()..].trim();
                let (ssid, pass) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                crate::wifi::connect(ssid, pass.trim());
                self.print(format!("wifi: connecting to {ssid}"));
            }
            "wifi" if arg == "connect" => crate::wifi::connect_known(),
            "wifi" if arg == "off" => crate::wifi::disconnect(),
            "wifi" if arg == "drop" => {
                crate::rtl8723be::drop_test();
                self.print("wifi: the radio leaves the channel; the link should be lost in ~10 s and come back");
            }
            "wifi" if arg == "test" => self.print(if crate::wifi::self_test() { "wifi ok" } else { "wifi FAILED" }),
            "wifi" if arg == "firmware" => match crate::wifi::firmware_info() {
                Ok(info) => self.print(info),
                Err(e) => self.print(format!("wifi: {e}")),
            },
            "wifi" if arg.starts_with("rate") => {
                const NAMES: [&str; 12] = ["1", "2", "5.5", "11", "6", "9", "12", "18", "24", "36", "48", "54"];
                let mut words = arg.split_whitespace().skip(1);
                if let Some(mbit) = words.next() {
                    match NAMES.iter().position(|n| *n == mbit) {
                        Some(i) => crate::rtl8723be::set_data_rate(i as u32, words.next() != Some("fixed")),
                        None => self.print("usage: wifi rate [1|2|5.5|11|6|9|12|18|24|36|48|54] [fixed]"),
                    }
                }
                let (rate, fallback) = crate::rtl8723be::data_rate();
                self.print(format!("Wi-Fi data goes at {} Mbit/s{}", NAMES[rate as usize], if fallback { ", lower on retries" } else { ", fixed" }));
            }
            "wifi" => {
                self.run_native("wifi", background, move |out| {
                    out("Starting the Wi-Fi module and listening, this takes a few seconds...");
                    let code = match crate::wifi::scan() {
                        Ok(list) => {
                            if list.is_empty() {
                                out("no networks heard");
                            }
                            for n in &list {
                                out(&crate::wifi::describe(n));
                            }
                            0
                        }
                        Err(e) => {
                            out(&format!("wifi: {e}"));
                            1
                        }
                    };
                    out(&crate::wifi::save_report());
                    code
                });
            }
            "bt" if arg.is_empty() => self.print(crate::bt::status()),
            "bt" if arg == "on" || arg == "off" => {
                crate::bt::set_enabled(arg == "on");
                self.print(format!("bt: turned {arg}"));
            }
            "bt" if arg == "scan" => {
                self.run_native("bt", background, move |out| {
                    out("Listening for Bluetooth devices (LE, then classic), about 12 seconds...");
                    match crate::bt::scan() {
                        Ok(list) => {
                            if list.is_empty() {
                                out("no devices heard");
                            }
                            for d in &list {
                                out(&crate::bt::describe(d));
                            }
                            0
                        }
                        Err(e) => {
                            out(&format!("bt: {e}"));
                            1
                        }
                    }
                });
            }
            "bt" if arg == "devices" => {
                let list = crate::bt::devices();
                if list.is_empty() {
                    self.print("no devices yet: run bt scan");
                }
                for e in &list {
                    let state = e.link.as_ref().map(crate::bt::link_text).unwrap_or_default();
                    let paired = if e.paired { "  [paired]" } else { "" };
                    self.print(format!("{}{paired}  {state}", crate::bt::describe(&e.dev)));
                }
            }
            "bt" if matches!(arg.split_whitespace().next(), Some("connect" | "disconnect" | "forget" | "yes" | "no")) => {
                let mut words = arg.split_whitespace();
                let cmd = words.next().unwrap_or("");
                match words.next().and_then(crate::bt::parse_addr) {
                    None => self.print("bt: give the address, like aa:bb:cc:dd:ee:ff (see bt devices)"),
                    Some(a) => {
                        match cmd {
                            "connect" => crate::bt::connect(a),
                            "disconnect" => crate::bt::disconnect(a),
                            "forget" => crate::bt::forget(a),
                            "yes" => crate::bt::confirm(a, true),
                            _ => crate::bt::confirm(a, false),
                        }
                        self.print(format!("bt: {cmd} {} \u{2014} see bt devices", crate::bt::addr_text(&a)));
                    }
                }
            }
            "bt" if arg.split_whitespace().next() == Some("sdp") => {
                let mut words = arg.split_whitespace().skip(1);
                match words.next().and_then(crate::bt::parse_addr) {
                    None => self.print("usage: bt sdp ADDRESS [UUID] \u{2014} the device's services (in the log, dmesg bt)"),
                    Some(a) => {
                        let uuid = words.next().and_then(|u| u16::from_str_radix(u.trim_start_matches("0x"), 16).ok()).unwrap_or(0x1002);
                        crate::bt::sdp(a, uuid);
                        self.print(format!("bt: asking {} about {uuid:04x} \u{2014} see dmesg bt", crate::bt::addr_text(&a)));
                    }
                }
            }
            "bt" => self.print("usage: bt [on|off|scan|devices|connect A|disconnect A|forget A|yes A|no A|sdp A [UUID]]"),
            // Debugging tools (the debug-tools build flag, see src/debug.rs).
            #[cfg(feature = "debug-tools")]
            "debug" => {
                let mut words = arg.split_whitespace();
                match (words.next(), words.next()) {
                    (None, _) => {
                        for line in crate::debug::lines() {
                            self.print(line);
                        }
                    }
                    (Some(name), Some(state @ ("on" | "off"))) => match crate::debug::set(name, state == "on") {
                        Ok(()) => self.print(format!("debug {name}: {state}")),
                        Err(e) => self.print(format!("debug: {e}")),
                    },
                    _ => self.print("usage: debug [NAME on|off]"),
                }
            }
            "monitor" => match arg {
                "off" => crate::output::set_detached(true),
                "on" => crate::output::set_detached(false),
                _ => self.print("usage: monitor off|on"),
            },
            "display" => self.display(arg),
            "gpu" => self.gpu(arg),
            "pci" => self.pci(arg),
            "remote" => {
                let result = match arg {
                    "" | "status" => Ok(String::new()),
                    "on" | "off" => net::remote::set_enabled(arg == "on").map(|()| String::new()),
                    "key" => net::remote::key_text(),
                    "newkey" => net::remote::new_key(),
                    "keep" if !crate::access::is_admin() => Err("only an administrator can change the kernel"),
                    "keep" => {
                        match net::remote::keep_trial() {
                            Ok(text) => self.print(text),
                            Err(e) => self.print(format!("remote: {e}")),
                        }
                        return;
                    }
                    _ => Err("usage: remote [on|off|key|newkey|keep]"),
                };
                match result {
                    Ok(key) if !key.is_empty() => self.print(key),
                    Ok(_) => {
                        for line in net::remote::describe() {
                            self.print(line);
                        }
                    }
                    Err(e) => self.print(format!("remote: {e}")),
                }
            }
            "lid" => match arg {
                "" => {
                    let state = match (crate::output::lid_known(), crate::output::lid_closed()) {
                        (false, _) => "lid: unknown",
                        (true, true) => "lid: closed",
                        (true, false) => "lid: open",
                    };
                    self.print(state);
                }
                "open" => crate::output::force_lid(Some(true)),
                "closed" => crate::output::force_lid(Some(false)),
                "auto" => crate::output::force_lid(None),
                _ => self.print("usage: lid [open|closed|auto]"),
            },
            "hwprobe" => {
                for line in crate::hwprobe::run() {
                    self.print(line);
                }
            }
            "touchpad" => match arg {
                "trace" => {
                    for line in crate::touchpad::trace() {
                        self.print(line);
                    }
                }
                "dsdt" => match crate::acpi::dsdt() {
                    // The firmware's ACPI table with the touchpad's bus speed and pins: for debugging.
                    Some(d) => match crate::fs::write("/tmp/dsdt.aml", d) {
                        Ok(()) => self.print(format!("touchpad: DSDT ({} bytes) saved to /tmp/dsdt.aml", d.len())),
                        Err(e) => self.print(format!("touchpad: {e}")),
                    },
                    None => self.print("touchpad: no DSDT"),
                },
                word @ ("slow" | "fast") => {
                    crate::i2c::force_slow(word == "slow");
                    self.print(format!("touchpad: I2C at {} kHz from now on", if word == "slow" { 100 } else { 400 }));
                }
                word @ ("on" | "off") => match crate::touchpad::set_enabled(word == "on") {
                    Ok(()) => self.print(format!("touchpad driver {word} after a restart")),
                    Err(e) => self.print(format!("touchpad: {e}")),
                },
                _ => {
                    for line in crate::touchpad::status() {
                        self.print(line);
                    }
                }
            },
            "reboot" | "shutdown" => {
                let restart = cmd == "reboot";
                match arg {
                    // Right away: we still save files and say goodbye to the network, but do not wait for windows.
                    "now" if restart => crate::reboot(),
                    "now" => crate::shutdown(),
                    // Firmware that does not hear the keyboard before the system will itself
                    // open its setup or boot from the USB stick.
                    "bios" | "setup" if restart => {
                        if crate::bootvars::setup_supported() {
                            self.print("restarting into the firmware setup\u{2026}");
                            crate::wm::request(crate::wm::Request::RestartInto(crate::bootvars::Next::Setup));
                        } else {
                            self.print("reboot: this firmware cannot open its setup on request; its key at power on is often F2 or Del");
                        }
                    }
                    "usb" if restart => match crate::bootvars::usb_stick() {
                        Ok((hd, what)) => {
                            self.print(format!("restarting from the USB stick, {what}\u{2026}"));
                            crate::wm::request(crate::wm::Request::RestartInto(crate::bootvars::Next::Usb(hd)));
                        }
                        Err(e) => self.print(format!("reboot: {e}")),
                    },
                    "" => {
                        self.print(if restart { "restarting\u{2026}" } else { "shutting down\u{2026}" });
                        crate::wm::request(if restart { crate::wm::Request::Restart } else { crate::wm::Request::Shutdown });
                    }
                    _ => self.print(if restart { "usage: reboot [now|bios|usb]" } else { "usage: shutdown [now]" }),
                }
            }
            "lsusb" => {
                for line in crate::usb::list() {
                    self.print(line);
                }
            }
            "irq" => {
                let lines = crate::irq::list();
                if lines.is_empty() {
                    self.print("no device uses MSI: drivers poll");
                }
                for line in lines {
                    self.print(line);
                }
            }
            "sound" => match crate::audio::SOUNDS.iter().find(|(name, _)| *name == arg) {
                Some(&(_, sound)) => crate::audio::play(sound),
                None => self.print("usage: sound notification|error|critical|drive-in|drive-out"),
            },
            "power" => match arg {
                "" => {
                    for (name, value) in crate::power::report() {
                        self.print(format!("{name:<16}{value}"));
                    }
                }
                "light" | "deep" => {
                    if crate::power::set_deep(arg == "deep") {
                        self.print(format!("sleep: {arg}"));
                    } else {
                        self.print("power: deep sleep is not available on this processor");
                    }
                }
                "wm on" | "wm off" => match crate::hdmi::deep_watermarks(arg == "wm on") {
                    Ok(0) => self.print("memory sleep: off"),
                    Ok(levels) => self.print(format!("memory sleep: on, {levels} levels; it turns itself off if the picture breaks")),
                    Err(e) => self.print(format!("power: {e}")),
                },
                _ => self.print("usage: power [light|deep|wm on|wm off]"),
            },
            "dmesg" => {
                for l in crate::serial::log_text().lines().filter(|l| l.contains(arg)) {
                    // Log lines come from foreign sources (firmware): strip control characters.
                    let clean: String = l.chars().filter(|c| !c.is_control()).collect();
                    self.print(clean);
                }
            }
            "lua" => match lua::Repl::new() {
                Some(repl) => {
                    self.repl = Some(repl);
                    self.print("Lua 5.4. Type exit or press Ctrl+D to leave.");
                }
                None => self.print("lua: out of memory"),
            },
            "beep" => {
                let mut it = arg.split_whitespace().map(|v| v.parse::<u32>().ok());
                let freq = it.next().flatten().unwrap_or(880);
                let ms = it.next().flatten().unwrap_or(200);
                if !crate::audio::available() {
                    self.print("beep: no sound card");
                }
                crate::audio::beep(freq, ms);
            }
            "play" => match fs::read(&self.path(arg)).map_err(|e| format!("{e}")).and_then(|d| crate::audio::play_wav(&d).map_err(String::from)) {
                Ok(()) => {}
                Err(e) => self.print(format!("play: {arg}: {e}")),
            },
            "stop" => crate::audio::stop_all(),
            "rec" => {
                // rec SECONDS FILE.wav — record from the microphone (headset) to a file.
                let mut words = arg.split_whitespace();
                let (secs, file) = (words.next().and_then(|s| s.parse::<u64>().ok()), words.next());
                let (Some(secs), Some(file)) = (secs, file) else {
                    self.print("usage: rec SECONDS FILE.wav \u{2014} record from the microphone");
                    return;
                };
                let path = self.path(file);
                self.run_native("rec", background, move |out| {
                    let mut cap = crate::audio::Capture::start();
                    let end = crate::interrupts::ticks() + secs.min(600) * crate::interrupts::HZ;
                    let mut samples = Vec::new();
                    // Wait until the input opens (the headset brings up its voice channel).
                    let mut announced = false;
                    while crate::interrupts::ticks() < end {
                        task::sleep_until(crate::interrupts::ticks() + crate::interrupts::HZ / 10);
                        cap.take(&mut samples);
                        if !announced && let Some((name, rate)) = cap.source() {
                            out(&format!("recording from {name} ({rate} Hz)..."));
                            announced = true;
                        }
                        if task::stop_reason(0) != 0 {
                            break;
                        }
                    }
                    cap.take(&mut samples);
                    let Some((_, rate)) = cap.source() else {
                        out("rec: no microphone (connect a Bluetooth headset)");
                        return 1;
                    };
                    drop(cap);
                    match fs::write(&path, &crate::audio::encode_wav(&samples, rate, 1)) {
                        Ok(()) => {
                            out(&format!("rec: {} samples ({:.1} s) saved to {path}", samples.len(), samples.len() as f32 / rate as f32));
                            0
                        }
                        Err(e) => {
                            out(&format!("rec: {path}: {e}"));
                            1
                        }
                    }
                });
            }
            "net" | "ifconfig" if arg == "renew" => {
                net::renew();
                self.print("asking for addresses again (DHCP)");
            }
            "net" | "ifconfig" if arg == "phy" || arg == "phy restart" => {
                let regs = net::phy_regs(arg == "phy restart");
                if regs.is_empty() {
                    self.print("net phy: the cable card does not report PHY registers");
                }
                let get = |r: u32| regs.iter().find(|x| x.0 == r).map_or(0, |x| x.1);
                for row in regs.chunks(8) {
                    self.print(row.iter().map(|(r, v)| format!("{r:2}:{v:04x}")).collect::<Vec<_>>().join(" "));
                }
                if !regs.is_empty() {
                    let ability = |a: u16, g: u16| {
                        let mut v = Vec::new();
                        if g & 0x800 != 0 { v.push("1000FD") }
                        if g & 0x400 != 0 { v.push("1000HD") }
                        if a & 0x100 != 0 { v.push("100FD") }
                        if a & 0x80 != 0 { v.push("100HD") }
                        if a & 0x40 != 0 { v.push("10FD") }
                        if a & 0x20 != 0 { v.push("10HD") }
                        v.join(" ")
                    };
                    let (bmsr, gbsr) = (get(1), get(10));
                    self.print(format!("link {}, negotiation {}", if bmsr & 4 != 0 { "up" } else { "down" }, if bmsr & 0x20 != 0 { "complete" } else { "not complete" }));
                    // Our own 1000 is GBCR (reg. 9) bits 9/8, shifted to bits 11/10 as in GBSR.
                    self.print(format!("we offer:     {}", ability(get(4), get(9) << 2)));
                    self.print(format!("partner says: {}", ability(get(5), gbsr)));
                    self.print(format!("1000BASE-T: local receiver {}, remote receiver {}, idle errors {}{}", if gbsr & 0x2000 != 0 { "ok" } else { "not ok" }, if gbsr & 0x1000 != 0 { "ok" } else { "not ok" }, gbsr & 0xFF, if gbsr & 0x8000 != 0 { ", master/slave fault" } else { "" }));
                }
            }
            "net" | "ifconfig" => {
                let ifaces = net::interfaces();
                if ifaces.is_empty() {
                    self.print("no network card");
                }
                for i in &ifaces {
                    let link = match i.speed {
                        Some((mbit, full)) => format!(", {mbit} Mbit/s {} duplex", if full { "full" } else { "half" }),
                        None if i.name == "wlan0" && i.link && crate::wifi::speed() != 0 => format!(", receiving at {} Mbit/s", crate::wifi::speed()),
                        None if !i.link => ", no link".into(),
                        None => String::new(),
                    };
                    self.print(format!("{:<5} mac {}{link}", i.name, i.mac));
                    self.print(format!("      ip {}  mask {}  ({})", net::ip_str(i.ip), net::ip_str(i.mask), i.dhcp));
                    self.print(format!("      gateway {}  dns {}", net::ip_str(i.gw), net::ip_str(i.dns)));
                    let s = i.stats;
                    self.print(format!("      rx {} packets {} KiB, tx {} packets {} KiB, resent {}, dropped {}", s.rx_packets, s.rx_bytes / 1024, s.tx_packets, s.tx_bytes / 1024, s.resent, s.tx_dropped));
                }
            }
            "route" => self.route(arg),
            "ping" => {
                // ping [-t] <host> [count]: -t — forever, until Ctrl+C (as in Windows).
                let forever = arg.split_whitespace().any(|w| w == "-t");
                let mut words = arg.split_whitespace().filter(|w| *w != "-t");
                let host = words.next().unwrap_or("").to_string();
                let count: u64 = words.next().and_then(|c| c.parse().ok()).unwrap_or(4);
                if host.is_empty() {
                    self.print("usage: ping [-t] <host> [count]   (-t: until Ctrl+C)");
                } else {
                    self.run_native("ping", background, move |out| {
                        if !net::wait_online() {
                            out("ping: no network");
                            return 1;
                        }
                        let Some(ip) = net::resolve(&host) else {
                            out(&format!("ping: {host}: host not found"));
                            return 1;
                        };
                        out(&format!("PING {host} ({}){}", net::ip_str(ip), if forever { ", Ctrl+C to stop" } else { "" }));
                        let (mut sent, mut ok) = (0u64, 0u64);
                        loop {
                            sent += 1;
                            let t = crate::interrupts::ticks();
                            // The interface — so the cable ↔ Wi-Fi switch is visible.
                            let via = net::egress(ip).unwrap_or("?");
                            match net::ping(ip, sent as u16) {
                                Some(us) => {
                                    ok += 1;
                                    let time = if us < 100 { String::from("<0.1") } else { format!("{}.{}", us / 1000, us % 1000 / 100) };
                                    out(&format!("reply from {}: seq={sent} time={time} ms via {via}", net::ip_str(ip)));
                                }
                                None => out(&format!("seq={sent}: no reply (via {via})")),
                            }
                            if task::stop_reason(0) == 2 || (!forever && sent >= count) {
                                break;
                            }
                            task::sleep_until(t + crate::interrupts::HZ);
                            if task::stop_reason(0) == 2 {
                                break;
                            }
                        }
                        let lost = (sent - ok) * 100 / sent;
                        out(&format!("{sent} sent, {ok} received, {lost}% lost"));
                        if ok > 0 { 0 } else { 1 }
                    });
                }
            }
            "host" | "nslookup" => {
                let host = arg.to_string();
                self.run_native("host", background, move |out| {
                    if !net::wait_online() {
                        out("host: no network");
                        return 1;
                    }
                    match net::resolve(&host) {
                        Some(ip) => {
                            out(&format!("{host} has address {}", net::ip_str(ip)));
                            0
                        }
                        None => {
                            out(&format!("host: {host}: not found"));
                            1
                        }
                    }
                });
            }
            "fetch" | "wget" => {
                let mut parts = arg.split_whitespace();
                let (url, save) = (parts.next().unwrap_or("").to_string(), parts.next().map(|f| self.path(f)));
                if url.is_empty() {
                    self.print("usage: fetch <http://url> [file]");
                } else {
                    self.run_native("fetch", background, move |out| match net::http::get(&url) {
                        Ok(r) => {
                            match &save {
                                Some(path) => match fs::write(path, &r.body) {
                                    Ok(()) => out(&format!("HTTP {}: saved {} bytes to {path}", r.status, r.body.len())),
                                    Err(e) => out(&format!("fetch: {path}: {e}")),
                                },
                                None => {
                                    let text = String::from_utf8_lossy(&r.body);
                                    for line in text.lines().take(400) {
                                        out(line);
                                    }
                                    out(&format!("-- HTTP {} from {}, {} bytes", r.status, r.url, r.body.len()));
                                }
                            }
                            if r.status < 400 { 0 } else { 1 }
                        }
                        Err(e) => {
                            out(&format!("fetch: {e}"));
                            1
                        }
                    });
                }
            }
            "jobs" => {
                if self.jobs.is_empty() {
                    self.print("no jobs");
                }
                let list: Vec<String> =
                    self.jobs.iter().map(|j| format!("[{}] running  {}  (thread {})", j.num, j.name, j.thread)).collect();
                for l in list {
                    self.print(l);
                }
            }
            "kill" => {
                let num = arg.trim_start_matches('%').parse::<usize>().ok();
                match self.jobs.iter().find(|j| Some(j.num) == num) {
                    Some(j) => task::kill(j.thread),
                    None => self.print("usage: kill %N   (see jobs)"),
                }
            }
            "pwd" => self.print(self.cwd.clone()),
            "ls" => {
                let path = self.path(if arg.is_empty() { "." } else { arg });
                match fs::list(&path) {
                    Ok(entries) if entries.is_empty() => self.print("(empty)"),
                    Ok(entries) => {
                        for e in entries {
                            if e.is_dir {
                                self.print(format!("{}/", e.name));
                            } else {
                                self.print(format!("{:<28}{} B", e.name, e.size));
                            }
                        }
                    }
                    Err(e) => self.print(format!("ls: {e}")),
                }
            }
            "cd" => {
                let path = if arg.is_empty() { crate::access::home() } else { self.path(arg) };
                if fs::is_dir(&path) {
                    self.cwd = path;
                } else {
                    self.print(format!("cd: {arg}: not a directory"));
                }
            }
            "file" => {
                let (mime, rest) = match arg.strip_prefix("-i") {
                    Some(r) if r.is_empty() || r.starts_with(' ') => (true, r.trim()),
                    _ => (false, arg),
                };
                // A whole name (may contain spaces) or several names separated by spaces.
                let names: Vec<&str> = if fs::exists(&self.path(rest)) || fs::is_dir(&self.path(rest)) {
                    vec![rest]
                } else {
                    rest.split_whitespace().collect()
                };
                if names.is_empty() {
                    self.print("usage: file [-i] <file>...");
                }
                for name in names {
                    let line = match crate::filetype::identify(&self.path(name)) {
                        Ok(k) if mime => format!("{name}: {}", k.mime),
                        Ok(k) => format!("{name}: {}", k.text),
                        Err(e) => format!("{name}: cannot open ({e})"),
                    };
                    self.print(line);
                }
            }
            "readtest" => {
                // The path may contain spaces: a number at the end is the size.
                let (file, mb) = match arg.rsplit_once(' ').map(|(f, n)| (f.trim(), n.parse::<u64>())) {
                    Some((f, Ok(n))) => (f, n),
                    _ => (arg.trim(), 200),
                };
                if file.is_empty() {
                    self.print("usage: readtest <file> [MB]");
                    return;
                }
                let path = self.path(file);
                let limit = mb << 20;
                self.run_native("readtest", background, move |out| {
                    const STEP: usize = 1 << 20;
                    let start = crate::interrupts::ticks();
                    let mut done = 0u64;
                    while done < limit {
                        match fs::read_range(&path, done, STEP) {
                            Ok((data, _)) if data.is_empty() => break,
                            Ok((data, _)) => done += data.len() as u64,
                            Err(e) => {
                                out(&format!("readtest: {path}: {e}"));
                                return 1;
                            }
                        }
                    }
                    let ms = (crate::interrupts::ticks() - start) * 1000 / crate::interrupts::HZ;
                    let speed = if ms > 0 { done * 1000 / ms / 1024 } else { 0 };
                    out(&format!("{} MB in {} ms: {}.{} MB/s", done >> 20, ms, speed / 1024, speed % 1024 * 10 / 1024));
                    0
                });
            }
            "xxd" | "hexdump" => {
                // xxd [-s offset] [-n bytes] file; by default the first 256 bytes.
                let mut words = arg.split_whitespace().peekable();
                let (mut offset, mut count) = (0u64, 256usize);
                let num = |w: Option<&str>| {
                    let w = w.unwrap_or("");
                    match w.strip_prefix("0x") {
                        Some(h) => u64::from_str_radix(h, 16).ok(),
                        None => w.parse().ok(),
                    }
                };
                let mut name = None;
                while let Some(w) = words.next() {
                    match w {
                        "-s" => offset = num(words.next()).unwrap_or(0),
                        "-n" | "-l" => count = num(words.next()).unwrap_or(256).min(64 * 1024) as usize,
                        _ => name = Some(w),
                    }
                }
                match name {
                    None => self.print("usage: xxd [-s offset] [-n bytes] <file>"),
                    Some(n) => match fs::read_range(&self.path(n), offset, count) {
                        Ok((data, _)) => {
                            for line in crate::filetype::hexdump(&data, offset) {
                                self.print(line);
                            }
                        }
                        Err(e) => self.print(format!("xxd: {n}: {e}")),
                    },
                }
            }
            "cat" => match fs::read(&self.path(arg)) {
                Ok(data) => {
                    let text = String::from_utf8_lossy(&data).into_owned();
                    self.print(text.trim_end_matches('\n'));
                }
                Err(e) => self.print(format!("cat: {e}")),
            },
            "mkdir" => {
                let r = fs::mkdir(&self.path(arg));
                self.report("mkdir", r);
            }
            "touch" => {
                let path = self.path(arg);
                if !fs::exists(&path) {
                    let r = fs::write(&path, b"");
                    self.report("touch", r);
                }
            }
            "rm" => {
                let r = fs::remove(&self.path(arg));
                self.report("rm", r);
            }
            "mv" => match first_arg(arg) {
                Some((from, to)) if !to.is_empty() => {
                    let (from, mut to) = (self.path(&from), self.path(unquote(to)));
                    if fs::is_dir(&to) {
                        to = fs::resolve(&to, fs::name(&from));
                    }
                    let r = fs::rename(&from, &to);
                    self.report("mv", r);
                }
                _ => self.print("usage: mv <from> <to>   (\"quote\" names with spaces)"),
            },
            "cp" => match first_arg(arg) {
                Some((from, to)) if !to.is_empty() => {
                    let (from, mut to) = (self.path(&from), self.path(unquote(to)));
                    if fs::is_dir(&to) {
                        to = fs::resolve(&to, fs::name(&from));
                    }
                    // A large file takes long to copy, and this is the desktop thread.
                    self.run_native("cp", background, move |out| match fs::copy(&from, &to) {
                        Ok(()) => 0,
                        Err(e) => {
                            out(&format!("cp: {e}"));
                            1
                        }
                    });
                }
                _ => self.print("usage: cp <from> <to>   (copies folders too)"),
            },
            "write" => match first_arg(arg) {
                Some((file, text)) => {
                    let r = fs::write(&self.path(&file), format!("{text}\n").as_bytes());
                    self.report("write", r);
                }
                None => self.print("usage: write <file> <text>   (\"quote\" names with spaces)"),
            },
            "echo" => self.print(arg),
            // Like a double click in Files: a picture goes to the viewer, music to the player…
            "open" => {
                let words = split_args(arg);
                let arg = words.first().map_or("", |w| w.as_str());
                let path = self.path(arg);
                if arg.is_empty() {
                    self.print("usage: open <file or folder>");
                } else if !fs::exists(&path) {
                    self.print(format!("open: {arg}: no such file"));
                } else if fs::is_dir(&path) {
                    wm::request(Request::OpenFolder(path));
                } else {
                    wm::request(Request::OpenFile(path));
                }
            }
            "view" => {
                let path = self.path(arg);
                if arg.is_empty() {
                    self.print("usage: view <file>");
                } else if !fs::exists(&path) {
                    self.print(format!("view: {arg}: no such file"));
                } else if !crate::image::is_picture(&path) {
                    self.print(format!("view: {arg}: not a picture"));
                } else {
                    wm::request(Request::OpenFile(path));
                }
            }
            "edit" => {
                if arg.is_empty() {
                    self.print("usage: edit <file>");
                } else {
                    let path = self.path(arg);
                    if !fs::exists(&path) {
                        let r = fs::write(&path, b"");
                        self.report("edit", r);
                    }
                    if !fs::is_dir(&path) {
                        wm::request(Request::OpenFile(path));
                    }
                }
            }
            "files" => {
                let path = self.path(if arg.is_empty() { "." } else { arg });
                wm::request(Request::OpenFolder(path));
            }
            other => {
                if let Some(path) = lua::find_program(&self.cwd, other) {
                    self.run_program(&path, arg, background);
                } else if let Some(path) = process::find(&self.cwd, other) {
                    self.run_process(&path, arg, background);
                } else {
                    self.print(format!("unknown command: {other}"));
                }
            }
        }
    }
}

impl Shell {
    /// The cursor in the input line.
    fn caret(&self) -> usize {
        self.caret.unwrap_or(self.input.len()).min(self.input.len())
    }

    fn set_caret(&mut self, i: usize) {
        self.caret = (i < self.input.len()).then_some(i);
    }

    fn insert(&mut self, text: &str) {
        let at = self.caret();
        let mut room = text.len().min(INPUT_MAX.saturating_sub(self.input.len()));
        while !text.is_char_boundary(room) {
            room -= 1;
        }
        let text = &text[..room];
        self.input.insert_str(at, text);
        self.set_caret(at + text.len());
    }

    fn prev_char(&self, i: usize) -> usize {
        self.input[..i].char_indices().next_back().map_or(0, |(k, _)| k)
    }

    /// The word boundary left / right of a position (Ctrl+arrows, Ctrl+Backspace).
    fn word_left(&self, i: usize) -> usize {
        self.input[..i].trim_end_matches(' ').rfind(' ').map_or(0, |k| k + 1)
    }

    fn word_right(&self, i: usize) -> usize {
        let rest = &self.input[i..];
        let skip = rest.len() - rest.trim_start_matches(' ').len();
        rest[skip..].find(' ').map_or(self.input.len(), |k| i + skip + k)
    }

    /// Editing the input line: cursor, deletion, insertion. true — the key was handled.
    fn edit(&mut self, ch: u8) -> bool {
        let ctrl = crate::input::mods() & crate::input::MOD_CTRL != 0;
        let at = self.caret();
        match ch {
            KEY_LEFT if ctrl => self.set_caret(self.word_left(at)),
            KEY_RIGHT if ctrl => self.set_caret(self.word_right(at)),
            KEY_LEFT => self.set_caret(self.prev_char(at)),
            KEY_RIGHT => self.set_caret(self.input[at..].chars().next().map_or(at, |c| at + c.len_utf8())),
            KEY_HOME | CTRL_A => self.set_caret(0),
            KEY_END | CTRL_E => self.caret = None,
            KEY_DELETE => {
                if at < self.input.len() {
                    self.input.remove(at);
                    self.set_caret(at);
                }
            }
            0x08 if at > 0 => {
                let from = if ctrl { self.word_left(at) } else { self.prev_char(at) };
                self.input.replace_range(from..at, "");
                self.set_caret(from);
            }
            0x08 => {}
            // As in bash: Ctrl+U erases up to the cursor, Ctrl+K after it.
            CTRL_U => {
                self.input.replace_range(..at, "");
                self.set_caret(0);
            }
            CTRL_K => self.input.truncate(at),
            CTRL_V => {
                let text: String = wm::clipboard()
                    .chars()
                    .map(|c| if c == '\n' { ' ' } else { c })
                    .filter(|c| !c.is_control())
                    .collect();
                self.insert(text.trim_end());
            }
            0x20..=0x7E => self.insert(core::str::from_utf8(&[ch]).unwrap()),
            _ => return false,
        }
        true
    }
}

impl Shell {
    /// The scroll bar: the last line of the list is the input. It sits in the right margin of the text.
    fn bar(&self) -> ScrollBar {
        let len = self.lines.len() + self.input_height();
        let track = Rect::new(self.size.0 - SCROLL_W - 2, PAD / 2, SCROLL_W, self.size.1 - PAD);
        ScrollBar { track, len, rows: self.rows, top: len.saturating_sub(self.rows + self.scroll) }
    }

    fn scroll_to(&mut self, top: usize) {
        self.scroll = (self.lines.len() + self.input_height()).saturating_sub(self.rows + top);
    }
}

impl App for Shell {
    fn background(&self) -> Color {
        theme::get().term_bg
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        let rows = ((a.h - 2 * PAD) / LINE).max(1) as usize;
        (self.rows, self.size) = (rows, (a.w, a.h));
        self.cols = ((a.w - 2 * PAD) * 5 / (font::measure(FontId::Mono, FONT, "MMMMM")).max(1)).max(1) as usize;
        let bar = self.bar();
        self.scroll = self.scroll.min(bar.len.saturating_sub(rows));
        let start = bar.top;
        let (x, mut y) = (a.x + PAD, a.y + PAD - 2);
        for (prompt, text) in self.lines.iter().skip(start).take(rows) {
            if prompt.is_empty() {
                c.text(x, y, text, 0xA9B0C6, FontId::Mono, FONT);
            } else if prompt == CONT {
                c.text(x, y, text, TEXT, FontId::Mono, FONT);
            } else {
                let pw = c.text(x, y, prompt, ACCENT, FontId::Mono, FONT);
                c.text(x + pw, y, text, TEXT, FontId::Mono, FONT);
            }
            y += LINE;
        }
        bar.draw(c, a, self.thumb_grab.is_some());
        // The input line is the last one in the scroll: it is not visible while reading old output.
        if self.foreground().is_some() || self.scroll > 0 {
            return;
        }
        // The input line wraps at the window width, like the output.
        let prompt = self.prompt();
        let pw = c.text(x, y, &prompt, ACCENT, FontId::Mono, FONT);
        let rows = self.input_rows(prompt.chars().count());
        for (i, &(from, to)) in rows.iter().enumerate() {
            let left = if i == 0 { x + pw } else { x };
            c.text(left, y + i as i32 * LINE, &self.input[from..to], TEXT, FontId::Mono, FONT);
        }
        self.input_at = (x + pw - a.x, y - a.y);
        self.input_left = x - a.x;
        if self.cursor_on {
            // A block cursor; the character under it is in the background colour so it stays visible.
            let at = self.caret();
            // The cursor's line: the last one that starts no later than the cursor (at the end of a full
            // line — already the next one).
            let row = rows.iter().rposition(|&(from, _)| from <= at).unwrap_or(0);
            let row = if row + 1 < rows.len() && rows[row].1 == at && at == self.input.len() { row + 1 } else { row };
            let (from, _) = rows[row];
            let left = if row == 0 { x + pw } else { x };
            let cy = y + row as i32 * LINE;
            let cx = left + font::measure(FontId::Mono, FONT, &self.input[from..at.max(from)]);
            let cw = font::measure(FontId::Mono, FONT, "M");
            c.fill_round(Rect::new(cx + 1, cy + 2, cw - 1, LINE - 3), 2, ACCENT, 230);
            if let Some(ch) = self.input[at..].chars().next() {
                c.text(cx, cy, ch.encode_utf8(&mut [0; 4]), 0x10121A, FontId::Mono, FONT);
            }
        }
    }

    /// The bar on the right scrolls the output, a click on the input line puts the cursor at the character under the mouse.
    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        if let Some(grab) = self.thumb_grab {
            if buttons & LEFT == 0 {
                self.thumb_grab = None;
            } else {
                self.scroll_to(self.bar().top_at(y - grab));
            }
            return true;
        }
        if pressed && buttons & LEFT != 0 && let Some(grab) = self.bar().grab(x, y) {
            self.thumb_grab = Some(grab);
            self.scroll_to(self.bar().top_at(y - grab));
            return true;
        }
        let (ix, iy) = self.input_at;
        let rows = self.input_rows(self.prompt().chars().count());
        let row = if y >= iy { ((y - iy) / LINE) as usize } else { usize::MAX };
        if !pressed || buttons & LEFT == 0 || self.scroll > 0 || row >= rows.len() || (row == 0 && x < ix - 4) {
            return false;
        }
        let (from, to) = rows[row];
        let left = if row == 0 { ix } else { self.input_left };
        // The character width is fractional: measure the text itself instead of multiplying by a rounded one.
        let ends = self.input[from..to].char_indices().map(|(i, _)| from + i).chain(core::iter::once(to));
        let at = ends.min_by_key(|&i| (font::measure(FontId::Mono, FONT, &self.input[from..i]) - (x - left)).abs());
        self.set_caret(at.unwrap_or(0));
        self.cursor_on = true;
        true
    }

    fn scroll(&mut self, lines: i32) -> bool {
        let max = (self.lines.len() + self.input_height()).saturating_sub(self.rows);
        let before = self.scroll;
        self.scroll = (self.scroll as i32 - lines).clamp(0, max as i32) as usize;
        self.scroll != before
    }

    fn memory(&self) -> usize {
        self.lines.iter().map(|(prompt, text)| size_of::<(String, String)>() + prompt.capacity() + text.capacity()).sum()
    }

    fn text(&mut self, ch: char, _ctx: &Ctx) -> bool {
        self.scroll = 0;
        self.insert(ch.encode_utf8(&mut [0; 4]));
        true
    }

    fn key(&mut self, ch: u8, ctx: &Ctx) -> bool {
        if matches!(ch, KEY_PAGE_UP | KEY_PAGE_DOWN) {
            let page = self.rows.saturating_sub(1).max(1) as i32;
            return self.scroll(if ch == KEY_PAGE_UP { -page } else { page });
        }
        self.scroll = 0;
        if let Some(job) = self.foreground() {
            // While a program runs, Ctrl+C stops it, and whatever is typed accumulates
            // and runs when it finishes (as in ordinary terminals).
            match ch {
                CTRL_C => {
                    task::kill(job.thread);
                    self.print("^C");
                    self.typeahead.clear();
                    self.input.clear();
                    self.caret = None;
                }
                b'\n' => {
                    self.typeahead.push(core::mem::take(&mut self.input));
                    self.caret = None;
                }
                _ => return self.edit(ch),
            }
            return true;
        }
        match ch {
            b'\n' => self.exec(ctx),
            b'\t' => self.complete(),
            CTRL_C => {
                // As in bash: show the interrupted line with ^C and start a new one.
                let prompt = self.prompt();
                let line = format!("{}^C", core::mem::take(&mut self.input));
                self.caret = None;
                self.echo(prompt, line);
                self.history_pos = None;
                self.draft.clear();
            }
            CTRL_L => self.lines.clear(),
            CTRL_D if self.repl.is_some() => {
                let prompt = self.prompt();
                self.echo(prompt, String::new());
                self.repl = None;
            }
            KEY_UP => self.history_up(),
            KEY_DOWN => self.history_down(),
            _ => return self.edit(ch),
        }
        true
    }

    fn tick(&mut self, ctx: &Ctx) -> bool {
        let on = ctx.caret_on();
        let changed = on != self.cursor_on;
        self.cursor_on = on;
        let mut changed = self.poll_jobs() | changed;
        changed |= self.poll_repl();
        while self.foreground().is_none() && !self.typeahead.is_empty() {
            let (pending, caret) = (self.input.clone(), self.caret);
            self.input = self.typeahead.remove(0);
            self.exec(ctx);
            (self.input, self.caret) = (pending, caret);
            changed = true;
        }
        changed
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        // The window was closed — nobody is left to show its jobs to.
        for job in &self.jobs {
            task::kill(job.thread);
        }
    }
}

fn load_history() -> Vec<String> {
    fs::read(&crate::access::config(HISTORY_FILE))
        .map(|data| String::from_utf8_lossy(&data).lines().map(String::from).collect())
        .unwrap_or_default()
}

/// Runs a terminal line without a window (remote access) in folder `cwd`. Long
/// commands (programs, network) are waited for up to `wait` ticks; if they do not finish — they are stopped.
/// Returns the output and the folder after the command (`cd` changes it).
pub fn run_headless(line: &str, cwd: &str, wait: u64) -> (String, String) {
    let mut shell = Shell::new();
    shell.headless = true;
    if fs::is_dir(cwd) {
        shell.cwd = cwd.into();
    }
    // The output accumulates in a string, as with `>`: no wrapping and no line limit.
    shell.capture = Some(String::new());
    shell.input = line.into();
    let ctx = Ctx { time: crate::time::now(), ticks: crate::interrupts::ticks(), focused: false };
    shell.exec(&ctx);
    let end = crate::interrupts::ticks() + wait;
    while !shell.jobs.is_empty() && crate::interrupts::ticks() < end {
        shell.poll_jobs();
        task::sleep_until(crate::interrupts::ticks() + HZ / 20);
    }
    shell.poll_jobs();
    let mut out = shell.capture.take().unwrap_or_default();
    if !shell.jobs.is_empty() {
        out.push_str(&format!("(still running after {} s, stopped)\n", wait / HZ));
    }
    (out, shell.cwd.clone())
}

/// The terminal window: several tabs, each with its own shell (folder, output
/// history, jobs). The tab strip is visible when there is more than one.
pub struct Terminal {
    tabs: Vec<Shell>,
    active: usize,
    /// The tab strip layout from the last draw (in client coordinates).
    bar: Option<TabBar>,
    /// Tab labels from the last draw — to notice that they changed.
    labels: Vec<String>,
}

impl Terminal {
    pub fn new() -> Self {
        Self { tabs: vec![Shell::new()], active: 0, bar: None, labels: Vec::new() }
    }

    /// A terminal with ready output (for example, of a program started from Files).
    pub fn with_output(lines: Vec<String>) -> Self {
        Self { tabs: vec![Shell::with_output(lines)], active: 0, bar: None, labels: Vec::new() }
    }

    fn new_tab(&mut self) {
        let cwd = self.tabs[self.active].cwd.clone();
        let mut shell = Shell::new();
        shell.cwd = cwd;
        self.tabs.insert(self.active + 1, shell);
        self.active += 1;
    }

    /// Closes a tab (its jobs are stopped); the last one is not closed.
    fn close_tab(&mut self, i: usize) {
        if self.tabs.len() > 1 {
            self.tabs.remove(i);
            if self.active > i || self.active == self.tabs.len() {
                self.active = self.active.saturating_sub(1);
            }
        }
    }

    /// The shell area inside the client area: below the tab strip, if there is one.
    fn shell_area(&self, a: Rect) -> Rect {
        if self.tabs.len() > 1 { Rect::new(a.x, a.y + TAB_BAR_H, a.w, a.h - TAB_BAR_H) } else { a }
    }
}

impl App for Terminal {
    fn background(&self) -> Color {
        theme::get().term_bg
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, ctx: &Ctx) {
        self.labels = self.tabs.iter().map(Shell::label).collect();
        if self.tabs.len() > 1 {
            let r = Rect::new(a.x, a.y, a.w, TAB_BAR_H);
            let bar = TabBar::layout(r, self.tabs.len());
            let labels: Vec<&str> = self.labels.iter().map(String::as_str).collect();
            bar.draw(c, r, &labels, self.active);
            self.bar = Some(TabBar::layout(Rect::new(0, 0, a.w, TAB_BAR_H), self.tabs.len()));
        } else {
            self.bar = None;
        }
        let area = self.shell_area(a);
        let saved = c.clip();
        c.set_clip(area.intersect(saved));
        self.tabs[self.active].draw(c, area, ctx);
        c.set_clip(saved);
    }

    fn scroll(&mut self, lines: i32) -> bool {
        self.tabs[self.active].scroll(lines)
    }

    fn text(&mut self, ch: char, ctx: &Ctx) -> bool {
        self.tabs[self.active].text(ch, ctx)
    }

    fn key(&mut self, ch: u8, ctx: &Ctx) -> bool {
        let n = self.tabs.len();
        match ch {
            CTRL_T => self.new_tab(),
            CTRL_W if n > 1 => self.close_tab(self.active),
            KEY_TAB_NEXT => self.active = (self.active + 1) % n,
            KEY_TAB_PREV => self.active = (self.active + n - 1) % n,
            _ => return self.tabs[self.active].key(ch, ctx),
        }
        true
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        if let Some(bar) = &self.bar
            && y < TAB_BAR_H
            && self.tabs[self.active].thumb_grab.is_none()
        {
            if !pressed {
                return false;
            }
            if bar.add.contains(x, y) {
                self.new_tab();
            } else if let Some((i, close)) = bar.hit(x, y) {
                if close {
                    self.close_tab(i);
                } else {
                    self.active = i;
                }
            }
            return true;
        }
        let dy = if self.bar.is_some() { TAB_BAR_H } else { 0 };
        self.tabs[self.active].mouse(x, y - dy, buttons, pressed)
    }

    fn tick(&mut self, ctx: &Ctx) -> bool {
        let mut changed = false;
        for (i, shell) in self.tabs.iter_mut().enumerate() {
            // Background tabs also take their jobs' output, but the cursor blinks only in the active one.
            let c = ctx.with_focus(ctx.focused && i == self.active);
            changed |= shell.tick(&c) && i == self.active;
        }
        // Some tab's label changed (a program started, the folder changed).
        changed || (self.tabs.len() > 1 && self.tabs.iter().map(Shell::label).ne(self.labels.iter().cloned()))
    }

    fn memory(&self) -> usize {
        self.tabs.iter().map(App::memory).sum()
    }
}

/// Program arguments: separated by spaces; in quotes — as one word ("My file.mp3").
fn split_args(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut word, mut quoted, mut any) = (String::new(), false, false);
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    out.push(core::mem::take(&mut word));
                    any = false;
                }
            }
            c => {
                word.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(word);
    }
    out
}

/// The first argument and the rest of the line. A name with spaces is put in quotes: "My file.txt".
/// Separates `> file` or `>> file` from the command: outside quotes, the file may be in quotes.
/// Returns the command and (file, whether to append).
fn split_redirect(line: &str) -> (&str, Option<(String, bool)>) {
    let mut quoted = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '>' if !quoted => {
                let rest = &line[i + 1..];
                let (file, append) = rest.strip_prefix('>').map_or((rest, false), |r| (r, true));
                let file = unquote(file);
                return if file.is_empty() { (line, None) } else { (line[..i].trim_end(), Some((file.to_string(), append))) };
            }
            _ => {}
        }
    }
    (line, None)
}

fn first_arg(arg: &str) -> Option<(String, &str)> {
    let arg = arg.trim_start();
    if let Some(rest) = arg.strip_prefix('"') {
        let end = rest.find('"')?;
        return Some((rest[..end].to_string(), rest[end + 1..].trim_start()));
    }
    let (first, rest) = arg.split_once(' ')?;
    Some((first.to_string(), rest.trim_start()))
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s)
}
