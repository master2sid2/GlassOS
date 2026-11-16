//! System time: universal time on top of CMOS, time zone, daylight saving time and
//! NTP synchronization.
//!
//! The CMOS clock may hold either universal or local time (Windows keeps it
//! local); the `rtc_utc` setting decides. The rest of the kernel gets from this
//! module either UTC seconds (`unix`) or local time (`now`).

mod zones;

pub use zones::Rule;
use crate::interrupts::{self, HZ};
use crate::kprintln;
use crate::rtc::{self, Time};
use crate::sync::IrqLock;
use crate::task;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, Ordering};
use zones::{Base, CITIES, Edge, FIXED};

pub const CONFIG: &str = "/system/time.cfg";
pub const DEFAULT_SERVER: &str = "pool.ntp.org";
/// How often to sync with the server when the last attempt succeeded and when it did not.
const SYNC_EVERY: u64 = 6 * 3600 * HZ;
const RETRY_SOON: u64 = 60 * HZ;
const RETRY_LATER: u64 = 30 * 60 * HZ;
/// How many failures in a row to retry often.
const SOON_TRIES: u32 = 3;
const DAY: i64 = 86_400;

pub const WEEKDAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
pub const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

#[derive(Clone, PartialEq, Eq)]
pub struct Settings {
    /// Zone name; None — not chosen yet, the offset is guessed from the CMOS clock.
    pub zone: Option<String>,
    /// Switch the clock to and from daylight saving time ourselves.
    pub auto_summer: bool,
    /// Daylight saving time is on manually (when we do not switch ourselves).
    pub summer: bool,
    pub ntp: bool,
    pub server: String,
    /// CMOS holds universal time, not local.
    pub rtc_utc: bool,
}

impl Settings {
    const fn new() -> Self {
        Self { zone: None, auto_summer: true, summer: false, ntp: true, server: String::new(), rtc_utc: false }
    }

    fn to_text(&self) -> String {
        format!(
            "zone={}\nauto_summer={}\nsummer={}\nntp={}\nserver={}\nrtc_utc={}\n",
            self.zone.as_deref().unwrap_or(""),
            self.auto_summer as u8,
            self.summer as u8,
            self.ntp as u8,
            self.server,
            self.rtc_utc as u8
        )
    }

    fn parse(text: &str) -> Self {
        let mut s = Self::new();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let (value, on) = (value.trim(), value.trim() == "1");
            match key.trim() {
                "zone" => s.zone = find_zone(value).map(|z| z.name),
                "auto_summer" => s.auto_summer = on,
                "summer" => s.summer = on,
                "ntp" => s.ntp = on,
                "server" => s.server = value.into(),
                "rtc_utc" => s.rtc_utc = on,
                _ => {}
            }
        }
        s
    }

    /// NTP server: empty — the default server.
    pub fn server(&self) -> &str {
        if self.server.is_empty() { DEFAULT_SERVER } else { &self.server }
    }
}

static SETTINGS: IrqLock<Settings> = IrqLock::new(Settings::new());
/// Offset in minutes guessed during sync while no zone is chosen.
static GUESS: AtomicI32 = AtomicI32::new(0);
/// Correction to the CMOS clock in seconds, if writing it failed.
static ADJUST: AtomicI64 = AtomicI64::new(0);
static VERSION: AtomicU64 = AtomicU64::new(0);
static SYNC_ASKED: AtomicBool = AtomicBool::new(false);
static SYNCING: AtomicBool = AtomicBool::new(false);
static LAST_SYNC: IrqLock<Option<SyncResult>> = IrqLock::new(None);

/// How the last sync ended.
#[derive(Clone)]
pub struct SyncResult {
    /// When (UTC).
    pub at: i64,
    /// How many milliseconds the clock was corrected by, or why it failed.
    pub result: Result<i64, String>,
}

/// Change counter: the settings window uses it to see that it should redraw.
pub fn version() -> u64 {
    VERSION.load(Ordering::Relaxed)
}

fn changed() {
    VERSION.fetch_add(1, Ordering::Relaxed);
}

pub fn settings() -> Settings {
    SETTINGS.lock().clone()
}

// ------------------------------------------------------------ zones

#[derive(Clone)]
pub struct Zone {
    pub name: String,
    /// Standard (winter) offset from UTC in minutes.
    pub offset: i32,
    pub rule: Option<&'static Rule>,
}

/// «UTC+02:00», «UTC-03:30».
pub fn offset_text(minutes: i32) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let m = minutes.unsigned_abs();
    format!("UTC{sign}{:02}:{:02}", m / 60, m % 60)
}

/// All zones: cities in alphabetical order first, then bare offsets.
pub fn zones() -> Vec<Zone> {
    let cities = CITIES.iter().map(|c| Zone { name: c.name.into(), offset: c.offset, rule: c.rule });
    cities.chain(FIXED.iter().map(|&m| Zone { name: offset_text(m), offset: m, rule: None })).collect()
}

/// Zone by name, case-insensitive; both "UTC+3" and "Kyiv" work.
pub fn find_zone(name: &str) -> Option<Zone> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    // Look up the exact name without allocating: the zone is resolved every time the clock is read.
    if let Some(c) = CITIES.iter().find(|c| c.name.eq_ignore_ascii_case(name)) {
        return Some(Zone { name: c.name.into(), offset: c.offset, rule: c.rule });
    }
    if let Some(minutes) = parse_offset(name) {
        return FIXED.contains(&minutes).then(|| Zone { name: offset_text(minutes), offset: minutes, rule: None });
    }
    let all = zones();
    let lower = name.to_ascii_lowercase().replace(' ', "_");
    let mut found = all.into_iter().filter(|z| z.name.to_ascii_lowercase().contains(&lower));
    let first = found.next()?;
    found.next().is_none().then_some(first)
}

/// "UTC+3", "+03:00", "utc-3:30" — offset in minutes.
fn parse_offset(text: &str) -> Option<i32> {
    let t = text.trim();
    let t = t.strip_prefix("UTC").or_else(|| t.strip_prefix("utc")).or_else(|| t.strip_prefix("GMT")).unwrap_or(t);
    if t.is_empty() {
        return Some(0);
    }
    let (sign, rest) = match t.as_bytes()[0] {
        b'+' => (1, &t[1..]),
        b'-' => (-1, &t[1..]),
        _ => return None,
    };
    let (h, m) = rest.split_once(':').unwrap_or((rest, "0"));
    let (h, m) = (h.parse::<i32>().ok()?, m.parse::<i32>().ok()?);
    (h <= 14 && m < 60).then_some(sign * (h * 60 + m))
}

/// The current zone. Not chosen — a bare offset guessed from the clock.
pub fn zone() -> Zone {
    let chosen = SETTINGS.lock().zone.clone();
    chosen.and_then(|n| find_zone(&n)).unwrap_or_else(|| {
        let m = GUESS.load(Ordering::Relaxed);
        Zone { name: offset_text(m), offset: m, rule: None }
    })
}

pub fn zone_chosen() -> bool {
    SETTINGS.lock().zone.is_some()
}

// ------------------------------------------------------------ calendar

/// Days since 1970-01-01 (Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let (y, m) = if m <= 2 { (y - 1, m as i64 + 9) } else { (y, m as i64 - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Year, month and day from days since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + (m <= 2) as i64, m, d)
}

/// Day of the week, 0 — Sunday.
pub fn weekday_of(days: i64) -> usize {
    (days + 4).rem_euclid(7) as usize
}

pub fn days_in_month(y: i64, m: u32) -> u32 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    (days_from_civil(ny, nm, 1) - days_from_civil(y, m, 1)) as u32
}

pub fn to_time(secs: i64) -> Time {
    let (y, m, d) = civil_from_days(secs.div_euclid(DAY));
    let s = secs.rem_euclid(DAY);
    Time { hour: (s / 3600) as u8, min: (s / 60 % 60) as u8, sec: (s % 60) as u8, day: d as u8, month: m as u8, year: y.clamp(0, 9999) as u16 }
}

pub fn from_time(t: &Time) -> i64 {
    days_from_civil(t.year as i64, t.month as u32, t.day as u32) * DAY + t.hour as i64 * 3600 + t.min as i64 * 60 + t.sec as i64
}

/// Day of the week of a date, 0 — Sunday.
pub fn weekday(t: &Time) -> usize {
    weekday_of(days_from_civil(t.year as i64, t.month as u32, t.day as u32))
}

// ------------------------------------------------------------ daylight saving time

/// The transition day in a year.
fn edge_day(e: &Edge, year: i64) -> i64 {
    let day = if e.nth > 0 {
        let first = days_from_civil(year, e.month as u32, 1);
        first + (e.weekday as i64 - weekday_of(first) as i64).rem_euclid(7) + 7 * (e.nth as i64 - 1)
    } else {
        let last = days_from_civil(year, e.month as u32, days_in_month(year, e.month as u32));
        last - (weekday_of(last) as i64 - e.weekday as i64).rem_euclid(7)
    };
    day + e.shift as i64
}

/// UTC seconds of a transition. On the wall, standard time runs until the start, summer time until the end.
fn edge_utc(e: &Edge, year: i64, offset: i32, save: i32, end: bool) -> i64 {
    let local = edge_day(e, year) * DAY + e.at as i64 * 60;
    let shift = match e.base {
        Base::Utc => 0,
        Base::Standard => offset,
        Base::Wall => offset + if end { save } else { 0 },
    };
    local - shift as i64 * 60
}

/// Whether the rule gives summer time at moment `utc`.
fn rule_summer(rule: &Rule, offset: i32, utc: i64) -> bool {
    let (year, _, _) = civil_from_days((utc + offset as i64 * 60).div_euclid(DAY));
    let start = edge_utc(&rule.start, year, offset, rule.save, false);
    let end = edge_utc(&rule.end, year, offset, rule.save, true);
    if start < end { utc >= start && utc < end } else { utc >= start || utc < end }
}

/// The zone's current offset by its own rule, and whether it is on summer time.
pub fn zone_now(z: &Zone) -> (i32, bool) {
    let summer = z.rule.is_some_and(|r| rule_summer(r, z.offset, unix()));
    (z.offset + if summer { z.rule.map_or(0, |r| r.save) } else { 0 }, summer)
}

/// Whether the zone is on summer time at moment `utc`, honouring the manual setting.
fn summer_at(z: &Zone, s: &Settings, utc: i64) -> bool {
    match z.rule {
        Some(rule) if s.auto_summer => rule_summer(rule, z.offset, utc),
        _ => !s.auto_summer && s.summer,
    }
}

fn offset_with(z: &Zone, s: &Settings, utc: i64) -> i32 {
    let save = z.rule.map_or(60, |r| r.save);
    z.offset + if summer_at(z, s, utc) { save } else { 0 }
}

/// Offset of local time from UTC in minutes at moment `utc`.
pub fn offset_at(utc: i64) -> i32 {
    offset_with(&zone(), &settings(), utc)
}

/// Summer time right now.
pub fn summer_now() -> bool {
    summer_at(&zone(), &settings(), unix())
}

/// The next transition: when (UTC) and the offset after it. Only if the clock
/// switches by itself.
pub fn next_change() -> Option<(i64, i32)> {
    let (z, s) = (zone(), settings());
    let rule = z.rule.filter(|_| s.auto_summer)?;
    let now = unix();
    let (year, _, _) = civil_from_days(now.div_euclid(DAY));
    let mut edges: Vec<i64> = (year..=year + 1)
        .flat_map(|y| [edge_utc(&rule.start, y, z.offset, rule.save, false), edge_utc(&rule.end, y, z.offset, rule.save, true)])
        .filter(|&t| t > now)
        .collect();
    edges.sort_unstable();
    let at = *edges.first()?;
    Some((at, offset_with(&z, &s, at)))
}

// ------------------------------------------------------------ clock

/// Seconds in the CMOS clock as if it were UTC.
fn hardware() -> i64 {
    from_time(&rtc::read())
}

/// Local seconds to UTC: find the offset by standard time and refine once.
fn local_to_utc(z: &Zone, s: &Settings, local: i64) -> i64 {
    let guess = local - z.offset as i64 * 60;
    local - offset_with(z, s, guess) as i64 * 60
}

fn unix_with(z: &Zone, s: &Settings) -> i64 {
    let hw = hardware();
    let utc = if s.rtc_utc { hw } else { local_to_utc(z, s, hw) };
    utc + ADJUST.load(Ordering::Relaxed)
}

/// UTC seconds since 1970-01-01.
pub fn unix() -> i64 {
    unix_with(&zone(), &settings())
}

/// Local time.
pub fn now() -> Time {
    let (z, s) = (zone(), settings());
    let utc = unix_with(&z, &s);
    to_time(utc + offset_with(&z, &s, utc) as i64 * 60)
}

/// Local time of moment `utc`.
pub fn local(utc: i64) -> Time {
    to_time(utc + offset_at(utc) as i64 * 60)
}

/// Sets the clock to `utc`. Best called in the middle of that second: after a write
/// the CMOS clock ticks to the next second half a second later.
fn set_unix(utc: i64) {
    let (z, s) = (zone(), settings());
    let hw = if s.rtc_utc { utc } else { utc + offset_with(&z, &s, utc) as i64 * 60 };
    rtc::write(to_time(hw));
    ADJUST.store(0, Ordering::Relaxed);
    // The clock did not accept the write (firmware sometimes holds it): keep a correction in memory.
    let miss = utc - unix_with(&z, &s);
    if miss.abs() > 1 {
        kprintln!("time: the hardware clock did not take the new time, {miss} s off");
        ADJUST.store(miss, Ordering::Relaxed);
    }
    changed();
}

/// Rewrites CMOS without changing the real time: needed when the clock holds local
/// time and the offset changed. Until the clock is rewritten, the correction keeps the time.
fn keep_time(before: i64) {
    let after = unix();
    ADJUST.fetch_add(before - after, Ordering::Relaxed);
    task::spawn("time-set", None, || {
        // Wait for the second to change plus half a second: then the write does not shift the clock.
        let start = rtc::read().sec;
        let deadline = interrupts::ticks() + 2 * HZ;
        while rtc::read().sec == start && interrupts::ticks() < deadline {
            task::sleep_until(interrupts::ticks() + 1);
        }
        task::sleep_until(interrupts::ticks() + HZ / 2);
        set_unix(unix());
    });
}

/// Sets local time manually.
pub fn set_local(t: &Time) -> Result<(), &'static str> {
    if !crate::access::is_admin() {
        return Err("Only an administrator can change the time");
    }
    if !(1..=12).contains(&t.month) || t.day == 0 || t.day as u32 > days_in_month(t.year as i64, t.month as u32) || t.hour > 23 || t.min > 59 || t.sec > 59 {
        return Err("There is no such date or time");
    }
    if !(2000..=2099).contains(&t.year) {
        return Err("The clock only knows the years 2000 to 2099");
    }
    let (z, s) = (zone(), settings());
    set_unix(local_to_utc(&z, &s, from_time(t)));
    kprintln!("time: set by hand to {}", stamp(unix()));
    Ok(())
}

/// Changes the settings, saves them and keeps the real time unchanged.
pub fn change(f: impl FnOnce(&mut Settings)) -> Result<(), &'static str> {
    if !crate::access::is_admin() {
        return Err("Only an administrator can change the time settings");
    }
    let before = unix();
    let old = settings();
    let mut new = old.clone();
    f(&mut new);
    if old == new {
        return Ok(());
    }
    *SETTINGS.lock() = new.clone();
    if new.zone.is_some() {
        GUESS.store(0, Ordering::Relaxed);
    }
    // CMOS holds local time and the offset is now different, or the clock changed meaning.
    let hw_moved = old.rtc_utc != new.rtc_utc || (!new.rtc_utc && unix() != before);
    if hw_moved {
        keep_time(before);
    }
    if new.ntp && !old.ntp {
        request_sync();
    }
    let _ = crate::fs::mkdir("/system");
    let saved = crate::fs::write(CONFIG, new.to_text().as_bytes()).is_ok();
    changed();
    if saved { Ok(()) } else { Err("The settings were not saved") }
}

/// At boot: settings from the file.
pub fn load() {
    if let Ok(data) = task::as_system(|| crate::fs::read(CONFIG)) {
        *SETTINGS.lock() = Settings::parse(&String::from_utf8_lossy(&data));
    }
    let (z, s) = (zone(), settings());
    let kind = if s.rtc_utc { "UTC" } else { "local time" };
    kprintln!("time: {} {} ({}), hardware clock keeps {kind}", stamp(unix()), z.name, offset_text(offset_at(unix())));
}

/// The next transition in words: "28 March 2027, 03:00 becomes 04:00".
pub fn change_text() -> Option<String> {
    let (at, after) = next_change()?;
    let before = to_time(at + offset_at(at - 1) as i64 * 60);
    let after = to_time(at + after as i64 * 60);
    let month = MONTHS[before.month as usize - 1];
    Some(format!("{} {month} {}, {:02}:{:02} becomes {:02}:{:02}", before.day, before.year, before.hour, before.min, after.hour, after.min))
}

/// "2026-11-02 11:08:51" in local time.
pub fn stamp(utc: i64) -> String {
    let t = local(utc);
    format!("{}-{:02}-{:02} {:02}:{:02}:{:02}", t.year, t.month, t.day, t.hour, t.min, t.sec)
}

/// «Monday, 2 November 2026».
pub fn long_date(t: &Time) -> String {
    format!("{}, {} {} {}", WEEKDAYS[weekday(t)], t.day, MONTHS[(t.month as usize).clamp(1, 12) - 1], t.year)
}

// ------------------------------------------------------------ NTP

pub fn request_sync() {
    SYNC_ASKED.store(true, Ordering::Relaxed);
}

pub fn syncing() -> bool {
    SYNCING.load(Ordering::Relaxed) || SYNC_ASKED.load(Ordering::Relaxed)
}

pub fn last_sync() -> Option<SyncResult> {
    LAST_SYNC.lock().clone()
}

/// One sync: the server answer, then a write to the clock in the middle of a second.
fn sync_once(server: &str) -> Result<i64, String> {
    let answer = crate::net::ntp::query(server)?;
    kprintln!("time: {server} answered, the way there and back took {} ms", answer.delay_ms);
    let true_ms = |tick: u64| answer.unix_ms + ((tick - answer.tick) * 1000 / HZ) as i64;
    let now = interrupts::ticks();
    if !zone_chosen() && !settings().rtc_utc {
        // No zone chosen and the clock holds local time (Windows sets it so): the offset
        // from UTC can be seen from it, to a quarter of an hour.
        const QUARTER: i64 = 15 * 60_000;
        let diff = hardware() * 1000 + 500 - true_ms(now);
        let quarter = ((diff + diff.signum() * QUARTER / 2) / QUARTER * 15) as i32;
        if quarter.abs() <= 14 * 60 {
            GUESS.store(quarter, Ordering::Relaxed);
        }
    }
    // We know our own time to a second: assume we are in the middle of it.
    let correction = true_ms(now) - (unix() * 1000 + 500);
    let wait = (1500 - true_ms(interrupts::ticks()).rem_euclid(1000)) % 1000;
    task::sleep_until(interrupts::ticks() + wait as u64 * HZ / 1000);
    set_unix(true_ms(interrupts::ticks()).div_euclid(1000));
    Ok(correction)
}

fn online() -> bool {
    crate::net::info().is_some_and(|i| i.ip != [0; 4])
}

/// Sync thread: with NTP on, as soon as the network appears, then every 6 hours.
pub fn start() {
    task::spawn("ntp", None, || {
        let mut next = 0;
        let mut failures = 0;
        loop {
            let asked = SYNC_ASKED.load(Ordering::Relaxed);
            let s = settings();
            if (asked || s.ntp && interrupts::ticks() >= next) && online() {
                SYNCING.store(true, Ordering::Relaxed);
                SYNC_ASKED.store(false, Ordering::Relaxed);
                changed();
                let server = s.server().to_string();
                let result = sync_once(&server);
                match &result {
                    Ok(ms) => {
                        kprintln!("time: synced with {server}, the clock was {} ms {}", ms.abs(), if *ms > 0 { "behind" } else { "ahead" });
                        failures = 0;
                        next = interrupts::ticks() + SYNC_EVERY;
                    }
                    Err(e) => {
                        kprintln!("time: {server}: {e}");
                        failures += 1;
                        next = interrupts::ticks() + if failures <= SOON_TRIES { RETRY_SOON } else { RETRY_LATER };
                    }
                }
                *LAST_SYNC.lock() = Some(SyncResult { at: unix(), result });
                SYNCING.store(false, Ordering::Relaxed);
                changed();
            } else if asked && !online() {
                SYNC_ASKED.store(false, Ordering::Relaxed);
                *LAST_SYNC.lock() = Some(SyncResult { at: unix(), result: Err("no network".into()) });
                changed();
            }
            task::sleep_until(interrupts::ticks() + HZ / 2);
        }
    });
}
