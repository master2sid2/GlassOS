//! Lua programs. The interpreter lives not in the kernel but in /bin/lua: each program
//! is a separate ring 3 process. Here are the startup, the API operations and the program windows.

mod api;
pub mod ui;
pub mod wire;

use crate::fs;
use crate::interrupts::{self, HZ};
use crate::process::{self, Process};
use crate::sync::IrqLock;
use crate::task;
use crate::wm::{self, Request};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use wire::{OPS, Value};

pub use ui::LuaApp;

const INTERPRETER: &str = "/bin/lua";

/// Operation arguments. Indices start at one, as in Lua.
pub struct Args(Vec<Value>);

impl Args {
    fn get(&self, n: usize) -> Option<&Value> {
        self.0.get(n.wrapping_sub(1))
    }

    pub fn opt(&self, n: usize) -> Option<String> {
        match self.get(n) {
            Some(Value::Bytes(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            Some(Value::Int(v)) => Some(format!("{v}")),
            _ => None,
        }
    }

    pub fn text(&self, n: usize) -> String {
        self.opt(n).unwrap_or_default()
    }

    pub fn bytes(&self, n: usize) -> Option<&[u8]> {
        match self.get(n) {
            Some(Value::Bytes(b)) => Some(b),
            _ => None,
        }
    }

    pub fn int(&self, n: usize, default: i64) -> i64 {
        match self.get(n) {
            Some(Value::Int(v)) => *v,
            _ => default,
        }
    }

    /// A coordinate: large values are clamped so calculations do not overflow.
    pub fn pos(&self, n: usize) -> i64 {
        self.int(n, 0).clamp(-100_000, 100_000)
    }

    pub fn flag(&self, n: usize) -> bool {
        !matches!(self.get(n), None | Some(Value::Nil) | Some(Value::Bool(false)))
    }

    pub fn list(&self, n: usize) -> Vec<String> {
        match self.get(n) {
            Some(Value::List(l)) => l.clone(),
            _ => Vec::new(),
        }
    }
}

/// Executes operation `op` of a process. None — there is no such operation.
pub fn call(op: usize, args: Vec<Value>) -> Option<Vec<Value>> {
    let (lib, name) = *OPS.get(op)?;
    let args = Args(args);
    Some(match lib {
        "ui" | "gfx" => ui::call(lib, name, &args),
        _ => api::call(lib, name, &args),
    })
}

/// Where and at what size to open the window of a restored program.
#[derive(Clone, Copy)]
pub struct Placement {
    pub pos: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
    pub maximized: bool,
    pub minimized: bool,
}

pub struct LuaWindow {
    /// The program's script — the window is restored by it after a reboot.
    pub path: String,
    pub title: String,
    pub w: i32,
    pub h: i32,
    pub app: LuaApp,
    pub place: Option<Placement>,
}

/// Running programs: thread → (script, where to open the window).
static STARTED: IrqLock<BTreeMap<u64, (String, Option<Placement>)>> = IrqLock::new(BTreeMap::new());

/// How many programs are starting now: their thread may ask about itself before
/// `spawn` writes it to STARTED.
static SPAWNING: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// The record of a thread. While someone's start is not finished, wait while yielding the CPU:
/// holding the lock for the duration of a start is not allowed, another thread would stall on it.
fn with_started<R>(thread: u64, f: impl FnOnce(&mut BTreeMap<u64, (String, Option<Placement>)>) -> R) -> R {
    use core::sync::atomic::Ordering;
    let deadline = interrupts::ticks() + 2 * HZ;
    loop {
        {
            let mut started = STARTED.lock();
            if started.contains_key(&thread) || SPAWNING.load(Ordering::Acquire) == 0 || interrupts::ticks() >= deadline {
                return f(&mut started);
            }
        }
        task::yield_now();
    }
}

fn take_placement(thread: u64) -> Option<Placement> {
    with_started(thread, |s| s.get_mut(&thread).and_then(|s| s.1.take()))
}

fn script_of(thread: u64) -> String {
    with_started(thread, |s| s.remove(&thread).map_or_else(String::new, |s| s.0))
}

/// Finds a program the way the terminal does: a path with `/` is relative to `cwd`,
/// otherwise in `cwd` and then in `/apps`. The `.lua` extension may be omitted.
pub fn find_program(cwd: &str, name: &str) -> Option<String> {
    let bases: Vec<String> = if name.contains('/') {
        alloc::vec![fs::resolve(cwd, name)]
    } else {
        alloc::vec![fs::resolve(cwd, name), fs::resolve("/apps", name)]
    };
    for base in bases {
        for candidate in [base.clone(), format!("{base}.lua")] {
            if candidate.ends_with(".lua") && fs::exists(&candidate) && !fs::is_dir(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

pub fn is_program(path: &str) -> bool {
    path.ends_with(".lua")
}

/// Starts a program as a process. The output goes to `output`; if it opens a window,
/// the process counts as finished for the terminal, from then on it lives as a window.
pub fn spawn(path: &str, args: &[&str], output: Arc<IrqLock<Vec<u8>>>, place: Option<Placement>) -> Result<Process, String> {
    let mut argv = alloc::vec!["lua", path];
    argv.extend_from_slice(args);
    let name = fs::name(path).trim_end_matches(".lua");
    // The thread may open a window before we remember its script: then it waits.
    use core::sync::atomic::Ordering;
    SPAWNING.fetch_add(1, Ordering::AcqRel);
    let spawned = process::spawn_as(name, INTERPRETER, path, &argv, output);
    if let Ok(p) = &spawned {
        STARTED.lock().insert(p.handle.id, (path.into(), place));
    }
    SPAWNING.fetch_sub(1, Ordering::AcqRel);
    spawned
}

/// Starting a service: without a window, with the rights of whoever added it.
pub fn spawn_service(path: &str, output: Arc<IrqLock<Vec<u8>>>, cred: Arc<crate::access::Cred>) -> Result<Process, String> {
    let name = fs::name(path).trim_end_matches(".lua");
    process::spawn_with(name, INTERPRETER, &["lua", path], output, cred)
}

/// Starting from the desktop: a new terminal shows the output of a program without a window.
pub fn launch(path: &str, place: Option<Placement>) {
    launch_with(path, &[], place);
}

/// Starting a program with arguments (for example, the player with a file).
pub fn launch_with(path: &str, args: &[&str], place: Option<Placement>) {
    let output = Arc::new(IrqLock::new(Vec::new()));
    let p = match spawn(path, args, output.clone(), place) {
        Ok(p) => p,
        Err(e) => {
            wm::notify_with(Some(crate::audio::Sound::Error), "Can't start the program", &e);
            return;
        }
    };
    task::spawn("lua-wait", None, move || {
        while p.exit_code.lock().is_none() {
            task::sleep_until(interrupts::ticks() + HZ / 10);
        }
        if !ui::has_window(p.handle.id) {
            let bytes = core::mem::take(&mut *output.lock());
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<String> = text.trim_end_matches('\n').split('\n').filter(|_| !bytes.is_empty()).map(String::from).collect();
            if !lines.is_empty() {
                wm::request(Request::Output(lines));
            }
        }
    });
}

// ---------------------------------------------------------------- REPL

/// Lines for interactive Lua: thread → queue. None in the queue is the end of input.
static STDIN: IrqLock<BTreeMap<u64, Stdin>> = IrqLock::new(BTreeMap::new());

#[derive(Default)]
struct Stdin {
    lines: VecDeque<String>,
    /// The process waits for the next line.
    waiting: bool,
}

/// The next input line of a process (waits for it).
fn read_line() -> Option<String> {
    let me = task::current_id();
    loop {
        {
            let mut map = STDIN.lock();
            let q = map.get_mut(&me)?;
            if let Some(line) = q.lines.pop_front() {
                q.waiting = false;
                return Some(line);
            }
            q.waiting = true;
        }
        task::sleep_until(interrupts::ticks() + 1);
        if task::should_stop() {
            return None;
        }
    }
}

/// Interactive Lua in the terminal: one process for the whole session.
pub struct Repl {
    thread: u64,
    output: Arc<IrqLock<Vec<u8>>>,
}

impl Repl {
    pub fn new() -> Option<Repl> {
        let output = Arc::new(IrqLock::new(Vec::new()));
        let mut stdin = STDIN.lock();
        let p = process::spawn_as("lua", INTERPRETER, INTERPRETER, &["lua"], output.clone()).ok()?;
        stdin.insert(p.handle.id, Stdin::default());
        Some(Repl { thread: p.handle.id, output })
    }

    /// Finished output lines. While code runs, an unfinished line waits for its end.
    pub fn take_output(&self) -> Vec<String> {
        let busy = self.busy();
        let mut output = self.output.lock();
        let end = if busy { output.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1) } else { output.len() };
        let bytes: Vec<u8> = output.drain(..end).collect();
        String::from_utf8_lossy(&bytes).lines().map(String::from).collect()
    }

    /// Queues a line. `take_output` takes the answer: it must not be waited for here,
    /// the caller is the desktop thread.
    pub fn eval(&self, line: &str) {
        if let Some(q) = STDIN.lock().get_mut(&self.thread) {
            q.lines.push_back(line.into());
            q.waiting = false;
        }
    }

    /// The interpreter is busy, a new line will wait.
    pub fn busy(&self) -> bool {
        self.alive() && !STDIN.lock().get(&self.thread).is_some_and(|q| q.waiting && q.lines.is_empty())
    }

    pub fn alive(&self) -> bool {
        task::is_alive(self.thread)
    }
}

impl Drop for Repl {
    fn drop(&mut self) {
        STDIN.lock().remove(&self.thread);
        task::kill(self.thread);
    }
}
