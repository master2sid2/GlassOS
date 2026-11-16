//! System metrics in the Prometheus text format. The exporter serves them
//! (`net::exporter`), and they are collected here from counters the kernel keeps anyway.
//! All names start with `glassos_`.

use crate::interrupts::{self, HZ};
use crate::sync::IrqLock;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

/// The CPU cycle count and the timer tick at boot: cycles are converted to seconds by them.
static BOOT: IrqLock<(u64, u64)> = IrqLock::new((0, 0));
/// The average queue of threads that need the CPU, over 1, 5 and 15 minutes.
static LOAD: IrqLock<[f64; 3]> = IrqLock::new([0.0; 3]);
/// Queue samples per second and the moving average multipliers for them:
/// e^(-0.2/60), e^(-0.2/300), e^(-0.2/900).
const LOAD_SAMPLES: u64 = 5;
const LOAD_DECAY: [f64; 3] = [0.996_672_213_253_674, 0.999_333_555_506_168, 0.999_777_802_464_651];
/// How many times the metrics were served and how many cycles that took in total.
pub static SCRAPES: AtomicU64 = AtomicU64::new(0);
static SCRAPE_CYCLES: AtomicU64 = AtomicU64::new(0);

/// Remembers the start of the count and computes the load average once a second.
pub fn init(devices: &[crate::pci::Device]) {
    *BOOT.lock() = (crate::arch::rdtsc(), interrupts::ticks());
    *PCI.lock() = Some(devices.to_vec());
    crate::task::spawn("load", None, || {
        crate::task::set_priority(crate::task::Prio::Background);
        let mut n = 0u64;
        loop {
            sample_load();
            if n % LOAD_SAMPLES == 0 {
                sample_usage();
            }
            n += 1;
            crate::task::sleep_until(interrupts::ticks() + HZ / LOAD_SAMPLES);
        }
    });
}

/// The load of all cores over the last second (a share) and the sample it was computed from.
static USAGE: IrqLock<(f64, u64, u64, u64)> = IrqLock::new((0.0, 0, 0, 0));

/// Once a second: the CPU load. The desktop itself computes it only for
/// a logged-in user, and metrics are needed before login too.
fn sample_usage() {
    let (idle, helpers) = crate::stats::totals();
    let now = crate::arch::rdtsc();
    {
        let mut u = USAGE.lock();
        let total = now.saturating_sub(u.1);
        if u.1 != 0 && total > 0 {
            let main = total.saturating_sub(idle - u.2) as f64;
            let all = total as f64 * crate::smp::cpu_count() as f64;
            u.0 = ((main + (helpers - u.3) as f64) / all).min(1.0);
        }
        (u.1, u.2, u.3) = (now, idle, helpers);
    }
}

/// The load average: how many threads wait for the CPU, like loadavg in Unix.
fn sample_load() {
    let ready = crate::task::runnable_count() as f64;
    let mut load = LOAD.lock();
    for (value, decay) in load.iter_mut().zip(LOAD_DECAY) {
        *value = *value * decay + ready * (1.0 - decay);
    }
}

/// CPU cycles per second, by the time since boot.
fn cycles_per_second() -> f64 {
    let (tsc, tick) = *BOOT.lock();
    let seconds = (interrupts::ticks() - tick) as f64 / HZ as f64;
    if seconds < 1.0 { 2.0e9 } else { (crate::arch::rdtsc() - tsc) as f64 / seconds }
}

/// Builds the text: a metric family is the HELP and TYPE lines, then the values.
struct Out {
    text: String,
}

impl Out {
    fn family(&mut self, name: &str, kind: &str, help: &str) {
        let _ = writeln!(self.text, "# HELP glassos_{name} {help}\n# TYPE glassos_{name} {kind}");
    }

    fn value(&mut self, name: &str, labels: &[(&str, &str)], value: f64) {
        self.text.push_str("glassos_");
        self.text.push_str(name);
        if !labels.is_empty() {
            self.text.push('{');
            for (i, (key, val)) in labels.iter().enumerate() {
                if i > 0 {
                    self.text.push(',');
                }
                let _ = write!(self.text, "{key}=\"{}\"", escape(val));
            }
            self.text.push('}');
        }
        let _ = writeln!(self.text, " {}", number(value));
    }

    /// A family of one value without labels.
    fn one(&mut self, name: &str, kind: &str, help: &str, value: f64) {
        self.family(name, kind, help);
        self.value(name, &[], value);
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// Integers without a fractional part, the rest as is.
fn number(v: f64) -> String {
    if v == (v as i64) as f64 && v.abs() < 1e15 { format!("{}", v as i64) } else { format!("{v}") }
}

/// The PCI device list does not change: the bus is enumerated once.
static PCI: IrqLock<Option<Vec<crate::pci::Device>>> = IrqLock::new(None);

pub fn render() -> String {
    let started = crate::arch::rdtsc();
    let mut o = Out { text: String::with_capacity(16 * 1024) };
    let hz = cycles_per_second();
    let uptime = interrupts::ticks() as f64 / HZ as f64;

    // The system.
    o.family("info", "gauge", "System name, version and processor.");
    let name = crate::net::identity::get().name;
    o.value("info", &[("os", crate::wm::OS_NAME), ("version", VERSION), ("hostname", &name), ("cpu", &crate::power::processor())], 1.0);
    o.one("uptime_seconds", "gauge", "Seconds since the system started.", uptime);
    let now = crate::time::unix();
    o.one("time_seconds", "gauge", "Current time, seconds since 1970 (UTC).", now as f64);
    o.one("boot_time_seconds", "gauge", "When the system started, seconds since 1970 (UTC).", libm::round(now as f64 - uptime));
    o.one("time_zone_offset_seconds", "gauge", "Local time minus UTC.", crate::time::offset_at(now) as f64 * 60.0);
    if let Some(sync) = crate::time::last_sync() {
        o.one("time_sync_timestamp_seconds", "gauge", "When the clock was last checked against a time server.", sync.at as f64);
        o.one("time_sync_ok", "gauge", "1 if the last check against a time server worked.", sync.result.is_ok() as u8 as f64);
        if let Ok(ms) = sync.result {
            o.one("time_sync_offset_seconds", "gauge", "How far the clock was off at the last check (positive: ahead).", ms as f64 / 1000.0);
        }
    }

    // The CPU.
    let (idle, helpers) = crate::stats::totals();
    let cpus = crate::smp::cpu_count();
    o.one("cpus", "gauge", "Processor cores.", cpus as f64);
    o.family("cpu_seconds_total", "counter", "Main core time by mode; helper cores count only their work.");
    o.value("cpu_seconds_total", &[("cpu", "main"), ("mode", "idle")], idle as f64 / hz);
    o.value("cpu_seconds_total", &[("cpu", "main"), ("mode", "busy")], (uptime - idle as f64 / hz).max(0.0));
    o.value("cpu_seconds_total", &[("cpu", "helpers"), ("mode", "busy")], helpers as f64 / hz);
    let (tasks, _) = crate::stats::tasks();
    let usage = USAGE.lock().0;
    o.one("cpu_usage_ratio", "gauge", "Share of all cores busy during the last second.", libm::round(usage * 1000.0) / 1000.0);
    let load = *LOAD.lock();
    o.family("load", "gauge", "Average number of threads waiting for the processor (main thread not counted).");
    for (minutes, value) in ["1", "5", "15"].iter().zip(load) {
        o.value("load", &[("minutes", minutes)], libm::round(value * 100.0) / 100.0);
    }
    o.one("context_switches_total", "counter", "Switches from one thread to another.", crate::task::switches() as f64);
    o.one("syscalls_total", "counter", "System calls made by programs.", crate::process::SYSCALLS.load(Ordering::Relaxed) as f64);
    let (threads, processes) = crate::task::counts();
    o.one("threads", "gauge", "Kernel threads and processes.", threads as f64);
    o.one("processes", "gauge", "Programs running in user mode.", processes as f64);

    // Sensors and power.
    let r = crate::power::reading();
    if let Some(c) = r.celsius {
        o.one("cpu_temperature_celsius", "gauge", "Processor package temperature.", c as f64);
    }
    if let Some(mhz) = r.mhz {
        o.one("cpu_frequency_hertz", "gauge", "Average speed of core 0 during the last second.", mhz as f64 * 1e6);
    }
    let parts = ["package", "cores", "graphics", "platform"];
    if r.milliwatts.iter().any(Option::is_some) {
        o.family("power_watts", "gauge", "Power use during the last second.");
        for (part, mw) in parts.iter().zip(r.milliwatts) {
            if let Some(mw) = mw {
                o.value("power_watts", &[("part", part)], mw as f64 / 1000.0);
            }
        }
    }
    let shares = r.sleep_shares();
    if !shares.is_empty() {
        o.family("cpu_sleep_ratio", "gauge", "Share of the last second spent in each sleep state.");
        for (level, state, tenths) in shares {
            o.value("cpu_sleep_ratio", &[("level", level), ("state", state)], tenths as f64 / 1000.0);
        }
    }
    if let Some(busy) = r.core_busy() {
        o.one("core0_busy_ratio", "gauge", "Share of the last second core 0 was awake.", busy as f64 / 1000.0);
    }
    if let Some(b) = crate::battery::get() {
        o.one("battery_charge_ratio", "gauge", "Battery charge.", b.percent as f64 / 100.0);
        o.one("battery_charging", "gauge", "1 while the battery charges.", b.charging as u8 as f64);
        o.one("power_on_ac", "gauge", "1 when the charger is plugged in.", b.on_ac as u8 as f64);
        if let Some(m) = b.minutes {
            o.one("battery_time_left_seconds", "gauge", "Estimated time on battery.", m as f64 * 60.0);
        }
    }
    if crate::output::lid_known() {
        o.one("lid_closed", "gauge", "1 while the laptop lid is closed.", crate::output::lid_closed() as u8 as f64);
    }

    // Memory.
    let (used, free) = crate::heap_stats();
    o.family("memory_bytes", "gauge", "Kernel memory.");
    o.value("memory_bytes", &[("state", "used")], used as f64);
    o.value("memory_bytes", &[("state", "free")], free as f64);

    // Tasks.
    o.family("task_cpu_ratio", "gauge", "Share of one core each window or thread used during the last second.");
    for t in &tasks {
        o.value("task_cpu_ratio", &[("name", &t.title), ("kind", t.kind)], t.cpu as f64 / 1000.0);
    }
    o.family("task_memory_bytes", "gauge", "Memory of each window or thread.");
    for t in &tasks {
        o.value("task_memory_bytes", &[("name", &t.title), ("kind", t.kind)], t.mem as f64);
    }

    // Disks.
    let get = |c: &AtomicU64| c.load(Ordering::Relaxed) as f64;
    o.family("disk_operations_total", "counter", "Disk reads and writes.");
    for (kind, io) in &crate::fs::DISK_IO {
        o.value("disk_operations_total", &[("disk", kind), ("op", "read")], get(&io.reads));
        o.value("disk_operations_total", &[("disk", kind), ("op", "write")], get(&io.writes));
    }
    o.family("disk_bytes_total", "counter", "Bytes read from and written to disks.");
    for (kind, io) in &crate::fs::DISK_IO {
        o.value("disk_bytes_total", &[("disk", kind), ("op", "read")], get(&io.read_bytes));
        o.value("disk_bytes_total", &[("disk", kind), ("op", "write")], get(&io.written_bytes));
    }
    o.family("disk_busy_seconds_total", "counter", "Time spent waiting for disks.");
    for (kind, io) in &crate::fs::DISK_IO {
        o.value("disk_busy_seconds_total", &[("disk", kind)], get(&io.busy) / hz);
    }
    let volumes = crate::fs::volumes();
    o.family("filesystem_size_bytes", "gauge", "Size of each drive.");
    for v in volumes.iter().filter(|v| !v.offline) {
        o.value("filesystem_size_bytes", &[("path", &v.path), ("name", &v.name), ("type", v.kind)], v.total as f64);
    }
    o.family("filesystem_used_bytes", "gauge", "Used space on each drive.");
    for v in volumes.iter().filter(|v| !v.offline) {
        o.value("filesystem_used_bytes", &[("path", &v.path), ("name", &v.name), ("type", v.kind)], v.used as f64);
    }

    // The network.
    if let Some((s, online, wireless)) = crate::net::counters() {
        let card = if wireless { "wifi" } else { "ethernet" };
        o.one("network_up", "gauge", "1 while there is a link and an address.", online as u8 as f64);
        o.family("network_bytes_total", "counter", "Bytes through the network card.");
        o.value("network_bytes_total", &[("card", card), ("direction", "receive")], s.rx_bytes as f64);
        o.value("network_bytes_total", &[("card", card), ("direction", "transmit")], s.tx_bytes as f64);
        o.family("network_packets_total", "counter", "Packets through the network card.");
        o.value("network_packets_total", &[("card", card), ("direction", "receive")], s.rx_packets as f64);
        o.value("network_packets_total", &[("card", card), ("direction", "transmit")], s.tx_packets as f64);
        o.one("tcp_resent_total", "counter", "TCP segments sent again.", s.resent as f64);
        o.one("tcp_out_of_order_total", "counter", "TCP segments that came out of order.", s.rx_disordered as f64);
    }
    if let Some(ssid) = crate::wifi::network() {
        let mode = crate::wifi::link_mode().unwrap_or_default();
        o.family("wifi_info", "gauge", "Wi-Fi network and link mode.");
        o.value("wifi_info", &[("ssid", &ssid), ("mode", &mode)], 1.0);
        o.one("wifi_rate_bits_per_second", "gauge", "Last receive rate.", crate::wifi::speed() as f64 * 1e6);
        if let Some((dbm, retried)) = crate::wifi::air() {
            o.one("wifi_signal_dbm", "gauge", "Signal on the better antenna.", dbm as f64);
            o.one("wifi_retried_ratio", "gauge", "Share of received frames that were repeats.", retried as f64 / 100.0);
        }
    }

    // Users.
    let accounts = crate::users::list();
    o.one("users", "gauge", "Accounts on this computer.", accounts.len() as f64);
    o.one("admins", "gauge", "Accounts with administrator rights.", accounts.iter().filter(|a| a.admin).count() as f64);
    o.family("user_signed_in", "gauge", "Who is signed in at the screen.");
    if let Some(user) = crate::access::session_user() {
        o.value("user_signed_in", &[("user", &user)], 1.0);
    }
    o.one("file_sharing_clients", "gauge", "Open connections to the shared folders of this computer.", crate::net::smbd::CLIENTS.load(Ordering::Relaxed) as f64);

    // Services.
    let services = crate::services::list();
    o.family("service_up", "gauge", "1 while the service runs.");
    for s in &services {
        o.value("service_up", &[("name", &s.name), ("scope", s.scope.name()), ("user", &s.user)], s.running() as u8 as f64);
    }
    o.family("service_restarts_total", "counter", "Times the service was started again after it stopped.");
    for s in &services {
        o.value("service_restarts_total", &[("name", &s.name), ("scope", s.scope.name()), ("user", &s.user)], s.restarts as f64);
    }

    // Devices.
    let devices = PCI.lock().clone();
    let devices = devices.unwrap_or_else(|| {
        let list = crate::pci::scan();
        *PCI.lock() = Some(list.clone());
        list
    });
    o.family("pci_device_info", "gauge", "PCI devices.");
    for d in &devices {
        let slot = format!("{:02x}:{:02x}.{}", d.bus, d.slot, d.func);
        let (vendor, device, class) = (format!("{:04x}", d.vendor), format!("{:04x}", d.device), format!("{:02x}{:02x}", d.class, d.subclass));
        o.value("pci_device_info", &[("slot", &slot), ("vendor", &vendor), ("device", &device), ("class", &class)], 1.0);
    }

    // The exporter itself.
    let scrapes = SCRAPES.fetch_add(1, Ordering::Relaxed) + 1;
    o.one("exporter_scrapes_total", "counter", "Times the metrics were read.", scrapes as f64);
    let took = crate::arch::rdtsc() - started;
    let spent = SCRAPE_CYCLES.fetch_add(took, Ordering::Relaxed) + took;
    o.one("exporter_scrape_seconds_total", "counter", "Time spent building the metrics.", spent as f64 / hz);
    o.text
}

/// The system version for `glassos_info`.
pub const VERSION: &str = "0.2";
