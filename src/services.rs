//! Services: Lua programs that run in the background without a window.
//!
//! A system service is added by an administrator, and it runs from boot, even while nobody
//! is logged in. A user service can be added by anyone, and it runs only while that
//! user is logged in. They run with the rights of whoever added them: their
//! home folder and the program's own folder. A crashed service is started again, less
//! and less often with each consecutive failure.

use crate::access;
use crate::fs;
use crate::interrupts::{self, HZ};
use crate::sync::IrqLock;
use crate::task;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub const CONFIG: &str = "/system/services.cfg";
/// How much of a service's recent output to keep.
const LOG_BYTES: usize = 8 * 1024;
/// The pause before a restart: 2 s, then twice as long for each consecutive failure, up to a minute.
const RETRY_FIRST: u64 = 2 * HZ;
const RETRY_MAX: u64 = 60 * HZ;
/// After running this long, the consecutive failures are forgotten.
const HEALTHY: u64 = 30 * HZ;
const LOOK_EVERY: u64 = HZ / 2;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    System,
    User,
}

impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Scope::System => "system",
            Scope::User => "user",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum State {
    Stopped,
    /// Waits for its user.
    Waiting,
    Running,
    /// Finished by itself with this code.
    Exited(i32),
    /// Did not start.
    Failed(String),
}

/// A service for the list: its settings and what is happening to it now.
#[derive(Clone)]
pub struct Service {
    pub name: String,
    pub path: String,
    pub scope: Scope,
    /// Whose it is and whose rights it runs with.
    pub user: String,
    /// Start by itself: from boot (system) or when the user logs in.
    pub autostart: bool,
    /// Start again if it finished.
    pub restart: bool,
    pub state: State,
    /// When it was started (tick) — while it runs.
    pub since: u64,
    pub restarts: u64,
    /// In how many ticks it will try again (if waiting).
    pub retry_at: Option<u64>,
}

impl Service {
    pub fn running(&self) -> bool {
        self.state == State::Running
    }
}

struct Entry {
    s: Service,
    /// It should be running: enabled manually or by autostart.
    wanted: bool,
    process: Option<crate::process::Process>,
    output: Arc<IrqLock<Vec<u8>>>,
    fails: u32,
}

static LIST: IrqLock<Vec<Entry>> = IrqLock::new(Vec::new());
static STARTED: AtomicBool = AtomicBool::new(false);
static VERSION: AtomicU64 = AtomicU64::new(0);

fn changed() {
    VERSION.fetch_add(1, Ordering::Relaxed);
}

/// Changes on any change in the list or in the state of services.
pub fn version() -> u64 {
    VERSION.load(Ordering::Relaxed)
}

pub fn list() -> Vec<Service> {
    LIST.lock().iter().map(|e| e.s.clone()).collect()
}

/// The last output lines of a service.
pub fn log(index: usize, lines: usize) -> Vec<String> {
    let bytes = match LIST.lock().get(index) {
        Some(e) => e.output.lock().clone(),
        None => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&bytes);
    let all: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    all[all.len().saturating_sub(lines)..].iter().map(|l| String::from(*l)).collect()
}

/// This service's thread.
pub fn is_service(thread: u64) -> bool {
    LIST.lock().iter().any(|e| e.process.as_ref().is_some_and(|p| p.handle.id == thread))
}

/// Threads of system services: they are left alone when a user logs out.
pub fn system_threads() -> Vec<u64> {
    LIST.lock().iter().filter(|e| e.s.scope == Scope::System).filter_map(|e| e.process.as_ref().map(|p| p.handle.id)).collect()
}

fn name_of(path: &str) -> String {
    fs::name(path).trim_end_matches(".lua").into()
}

fn parse(text: &str) -> Vec<Entry> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            let scope = match f.next()? {
                "system" => Scope::System,
                "user" => Scope::User,
                _ => return None,
            };
            let user = f.next()?.into();
            let autostart = f.next()? == "1";
            let restart = f.next()? == "1";
            let path: String = f.next()?.into();
            Some(entry(Service {
                name: name_of(&path),
                path,
                scope,
                user,
                autostart,
                restart,
                state: State::Stopped,
                since: 0,
                restarts: 0,
                retry_at: None,
            }))
        })
        .collect()
}

fn entry(s: Service) -> Entry {
    Entry { wanted: s.autostart && s.scope == Scope::System, s, process: None, output: Arc::new(IrqLock::new(Vec::new())), fails: 0 }
}

fn save(list: &[Entry]) -> Result<(), &'static str> {
    let text: String = list
        .iter()
        .map(|e| format!("{}\t{}\t{}\t{}\t{}\n", e.s.scope.name(), e.s.user, e.s.autostart as u8, e.s.restart as u8, e.s.path))
        .collect();
    task::as_system(|| {
        let _ = fs::mkdir("/system");
        fs::write(CONFIG, text.as_bytes()).map_err(|_| "cannot save the list of services")
    })
}

/// Reads the list and starts the supervisor. Call after the disks are mounted.
pub fn start() {
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let text = task::as_system(|| fs::read(CONFIG).ok()).unwrap_or_default();
    *LIST.lock() = parse(&String::from_utf8_lossy(&text));
    task::spawn("services", None, watch);
}

/// Whether the current user may touch this service.
fn may_touch(s: &Service) -> bool {
    match s.scope {
        Scope::System => access::is_admin(),
        Scope::User => access::user().is_some_and(|me| me == s.user) || access::is_admin(),
    }
}

/// Adds a service for the current user.
pub fn add(path: &str, scope: Scope) -> Result<(), &'static str> {
    let user = access::user().ok_or("nobody is signed in")?;
    if scope == Scope::System && !access::is_admin() {
        return Err("Only an administrator can add a system service");
    }
    if !crate::lua::is_program(path) || !fs::exists(path) {
        return Err("A service is a Lua program (.lua)");
    }
    let mut list = LIST.lock();
    if list.iter().any(|e| e.s.path == path && e.s.scope == scope && e.s.user == user) {
        return Err("This program is already a service");
    }
    let mut e = entry(Service {
        name: name_of(path),
        path: path.into(),
        scope,
        user,
        autostart: true,
        restart: true,
        state: State::Stopped,
        since: 0,
        restarts: 0,
        retry_at: None,
    });
    e.wanted = true;
    list.push(e);
    let result = save(&list);
    drop(list);
    changed();
    result
}

/// Changes service number `index`: `f` edits its settings.
fn change(index: usize, f: impl FnOnce(&mut Entry)) -> Result<(), &'static str> {
    let mut list = LIST.lock();
    let e = list.get_mut(index).ok_or("no such service")?;
    if !may_touch(&e.s) {
        return Err(if e.s.scope == Scope::System { "Only an administrator can change system services" } else { "This service belongs to another user" });
    }
    f(e);
    let result = save(&list);
    drop(list);
    changed();
    result
}

pub fn set_autostart(index: usize, on: bool) -> Result<(), &'static str> {
    change(index, |e| e.s.autostart = on)
}

pub fn set_restart(index: usize, on: bool) -> Result<(), &'static str> {
    change(index, |e| e.s.restart = on)
}

/// Start (or stop) now.
pub fn run(index: usize, on: bool) -> Result<(), &'static str> {
    change(index, |e| {
        e.wanted = on;
        e.fails = 0;
        e.s.retry_at = None;
    })
}

/// Restart: stop, and the supervisor starts it again right away.
pub fn restart(index: usize) -> Result<(), &'static str> {
    change(index, |e| {
        e.wanted = true;
        e.fails = 0;
        e.s.retry_at = None;
        if let Some(p) = &e.process {
            task::kill(p.handle.id);
        }
    })
}

pub fn remove(index: usize) -> Result<(), &'static str> {
    let mut list = LIST.lock();
    let e = list.get(index).ok_or("no such service")?;
    if !may_touch(&e.s) {
        return Err("You cannot remove this service");
    }
    let e = list.remove(index);
    if let Some(p) = &e.process {
        task::kill(p.handle.id);
    }
    let result = save(&list);
    drop(list);
    changed();
    result
}

/// The supervisor: starts whoever needs it and notices who finished.
fn watch() {
    let mut session: Option<String> = None;
    loop {
        let now = interrupts::ticks();
        let me = access::session_user();
        let mut list = LIST.lock();
        let mut dirty = false;
        // Another user logged in: their autostart services want to run.
        if me != session {
            for e in list.iter_mut().filter(|e| e.s.scope == Scope::User) {
                e.wanted = e.s.autostart && me.as_deref() == Some(e.s.user.as_str());
                e.fails = 0;
                e.s.retry_at = None;
            }
            session = me.clone();
            dirty = true;
        }
        let mut to_start = Vec::new();
        for (i, e) in list.iter_mut().enumerate() {
            let allowed = e.s.scope == Scope::System || me.as_deref() == Some(e.s.user.as_str());
            if let Some(p) = &e.process {
                let code = *p.exit_code.lock();
                if let Some(code) = code {
                    let lived = now - e.s.since;
                    e.process = None;
                    e.s.state = State::Exited(code);
                    if lived >= HEALTHY {
                        e.fails = 0;
                    }
                    e.fails += 1;
                    if e.wanted && e.s.restart && allowed {
                        let wait = (RETRY_FIRST << (e.fails - 1).min(5)).min(RETRY_MAX);
                        e.s.retry_at = Some(now + wait);
                    } else {
                        e.wanted = false;
                    }
                    log_line(&e.output, &format!("-- stopped with code {code}"));
                    dirty = true;
                } else if !(e.wanted && allowed) {
                    task::kill(p.handle.id);
                }
                continue;
            }
            if e.wanted && allowed {
                if e.s.retry_at.is_none_or(|at| now >= at) {
                    to_start.push(i);
                }
            } else {
                let state = if e.wanted { State::Waiting } else if matches!(e.s.state, State::Running | State::Waiting) { State::Stopped } else { e.s.state.clone() };
                if state != e.s.state {
                    e.s.state = state;
                    dirty = true;
                }
            }
        }
        // Starting reads files and yields the CPU: without the list lock.
        let jobs: Vec<(usize, Service, Arc<IrqLock<Vec<u8>>>)> = to_start.iter().map(|&i| (i, list[i].s.clone(), list[i].output.clone())).collect();
        drop(list);
        for (i, s, output) in jobs {
            let started = launch(&s, output.clone());
            let mut list = LIST.lock();
            // While we were starting, the list may have changed.
            let Some(e) = list.get_mut(i).filter(|e| e.s.path == s.path && e.s.user == s.user && e.s.scope == s.scope) else {
                if let Ok(p) = started {
                    task::kill(p.handle.id);
                }
                continue;
            };
            e.s.retry_at = None;
            match started {
                Ok(p) => {
                    if e.s.since != 0 {
                        e.s.restarts += 1;
                    }
                    e.s.since = interrupts::ticks();
                    e.s.state = State::Running;
                    e.process = Some(p);
                }
                Err(err) => {
                    log_line(&output, &format!("-- cannot start: {err}"));
                    e.s.state = State::Failed(err);
                    e.fails += 1;
                    e.s.retry_at = Some(interrupts::ticks() + (RETRY_FIRST << (e.fails - 1).min(5)).min(RETRY_MAX));
                }
            }
            dirty = true;
        }
        if dirty {
            changed();
        }
        task::sleep_until(interrupts::ticks() + LOOK_EVERY);
    }
}

fn launch(s: &Service, output: Arc<IrqLock<Vec<u8>>>) -> Result<crate::process::Process, String> {
    let admin = crate::users::find(&s.user).ok_or_else(|| format!("there is no user {}", s.user))?.admin;
    if !fs::exists(&s.path) {
        return Err(format!("{} is not found", s.path));
    }
    log_line(&output, "-- started");
    let cred = access::for_service(&s.user, admin, &s.path);
    let p = crate::lua::spawn_service(&s.path, output.clone(), cred)?;
    task::spawn("service-log", None, move || trim_log(output));
    Ok(p)
}

fn log_line(output: &Arc<IrqLock<Vec<u8>>>, line: &str) {
    let mut out = output.lock();
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(line.as_bytes());
    out.push(b'\n');
}

/// The output accumulates while the service runs: only the tail is kept.
fn trim_log(output: Arc<IrqLock<Vec<u8>>>) {
    // While someone other than us and the list refers to the output, the process is alive.
    while Arc::strong_count(&output) > 2 {
        {
            let mut out = output.lock();
            if out.len() > LOG_BYTES {
                let cut = out.len() - LOG_BYTES / 2;
                let cut = out[cut..].iter().position(|&b| b == b'\n').map_or(cut, |n| cut + n + 1);
                out.drain(..cut);
            }
        }
        task::sleep_until(interrupts::ticks() + HZ);
    }
}
