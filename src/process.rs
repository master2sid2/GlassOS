//! User processes: loading ELF, running in ring 3 and system calls.
//!
//! A program is a static ELF for x86_64 (usually PIE, built from the `user/` crate).
//! It is loaded into its own address space at `USER_BASE`, gets a stack and
//! arguments, and talks to the kernel only through `int 0x80`:
//! the call number in rax, the arguments in rdi, rsi, rdx, r10, the result in rax.

use crate::access;
use crate::fs;
use crate::interrupts::{self, HZ};
use crate::kprintln;
use crate::lua::wire;
use crate::paging::{AddressSpace, PAGE, USER_BASE, USER_END};
use crate::sync::IrqLock;
use crate::task::{self, CRASHED, Handle};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

/// The process stack: Lua parses code recursively, hence a megabyte.
const STACK_PAGES: u64 = 256;
const STACK_TOP: u64 = USER_END - PAGE;
/// How much memory a process may ask the kernel for through map.
const HEAP_LIMIT: u64 = 256 << 30;

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_READ_FILE: u64 = 2;
const SYS_WRITE_FILE: u64 = 3;
const SYS_SLEEP: u64 = 4;
const SYS_TICKS: u64 = 5;
const SYS_YIELD: u64 = 6;
const SYS_MAP: u64 = 7;
const SYS_GETPID: u64 = 8;
const SYS_LIST_DIR: u64 = 9;
const SYS_LUA: u64 = 10;
const SYS_LUA_FETCH: u64 = 11;

/// The largest arguments of a Lua operation (a file to write).
const LUA_ARGS_MAX: u64 = 48 << 20;
/// How much of the kernel heap processes may not take.
const HEAP_RESERVE: usize = 16 << 20;

/// Long answers of Lua operations the process has not taken yet.
static REPLIES: IrqLock<BTreeMap<u64, Vec<u8>>> = IrqLock::new(BTreeMap::new());

/// Whether the contents look like a program (ELF).
pub fn is_elf(data: &[u8]) -> bool {
    data.starts_with(b"\x7fELF")
}

/// Finds a program the same way the terminal finds scripts: a path with `/` is relative
/// to `cwd`, otherwise in `cwd` and then in `/bin`.
pub fn find(cwd: &str, name: &str) -> Option<String> {
    let candidates =
        if name.contains('/') { alloc::vec![fs::resolve(cwd, name)] } else { alloc::vec![fs::resolve(cwd, name), fs::resolve("/bin", name)] };
    candidates.into_iter().find(|p| !fs::is_dir(p) && fs::read(p).is_ok_and(|d| is_elf(&d)))
}

fn u16_at(d: &[u8], at: usize) -> Option<u64> {
    Some(u16::from_le_bytes(d.get(at..at + 2)?.try_into().ok()?) as u64)
}

fn u32_at(d: &[u8], at: usize) -> Option<u64> {
    Some(u32::from_le_bytes(d.get(at..at + 4)?.try_into().ok()?) as u64)
}

fn u64_at(d: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(d.get(at..at + 8)?.try_into().ok()?))
}

/// Loads an ELF into a new address space; returns it and the entry point.
fn load(elf: &[u8]) -> Result<(AddressSpace, u64), String> {
    let bad = || String::from("not a valid x86_64 program");
    if !is_elf(elf) || elf.get(4) != Some(&2) || u16_at(elf, 18) != Some(0x3E) {
        return Err(bad());
    }
    let base = match u16_at(elf, 16) {
        Some(2) => 0,         // ET_EXEC: the addresses are already absolute
        Some(3) => USER_BASE, // ET_DYN (PIE): put at the start of user memory
        _ => return Err(bad()),
    };
    let (phoff, phsize, phnum) = (u64_at(elf, 32).ok_or_else(bad)?, u16_at(elf, 54).ok_or_else(bad)?, u16_at(elf, 56).ok_or_else(bad)?);
    let mut segments = Vec::new();
    let mut dynamic = None;
    for i in 0..phnum {
        let ph = (phoff + i * phsize) as usize;
        let field = |off| u64_at(elf, ph + off).ok_or_else(bad);
        let kind = u32_at(elf, ph).ok_or_else(bad)?;
        let (offset, vaddr, filesz, memsz) = (field(8)?, field(16)?, field(32)?, field(40)?);
        match kind {
            1 => {
                let writable = u32_at(elf, ph + 4).ok_or_else(bad)? & 2 != 0;
                let start = base.checked_add(vaddr).ok_or_else(bad)?;
                let end = start.checked_add(memsz).ok_or_else(bad)?;
                if start < USER_BASE || end > STACK_TOP - STACK_PAGES * PAGE || filesz > memsz {
                    return Err(bad());
                }
                let data = elf.get(offset as usize..(offset + filesz) as usize).ok_or_else(bad)?;
                segments.push((start, end, writable, data));
            }
            2 => dynamic = Some((offset, filesz)),
            _ => {}
        }
    }

    // A page may belong to two segments — then it is writable if at least one of them requires it.
    let mut pages: BTreeMap<u64, bool> = BTreeMap::new();
    for &(start, end, writable, _) in &segments {
        let mut p = start & !(PAGE - 1);
        while p < end {
            *pages.entry(p).or_default() |= writable;
            p += PAGE;
        }
    }
    if pages.len() > 4096 {
        return Err("program is too large".into());
    }
    let mut space = AddressSpace::new();
    for (&p, &writable) in &pages {
        space.map(p, writable).ok_or_else(|| String::from("out of memory"))?;
    }
    for &(start, _, _, data) in &segments {
        space.poke(start, data);
    }

    // PIE: adjust absolute addresses (R_X86_64_RELATIVE) to the load base.
    if let Some((offset, size)) = dynamic {
        let (mut rela, mut rela_size) = (0, 0);
        for i in (0..size).step_by(16) {
            let at = (offset + i) as usize;
            match u64_at(elf, at) {
                Some(7) => rela = u64_at(elf, at + 8).ok_or_else(bad)?,
                Some(8) => rela_size = u64_at(elf, at + 8).ok_or_else(bad)?,
                Some(0) | None => break,
                _ => {}
            }
        }
        let table = if rela_size > 0 { space.read(base + rela, rela_size as usize).ok_or_else(bad)? } else { Vec::new() };
        for r in table.chunks_exact(24) {
            let (off, info, addend) = (u64_at(r, 0).unwrap(), u64_at(r, 8).unwrap(), u64_at(r, 16).unwrap());
            if info & 0xFFFF_FFFF != 8 {
                return Err(format!("unsupported relocation type {}", info & 0xFFFF_FFFF));
            }
            space.poke(base + off, &(base.wrapping_add(addend)).to_le_bytes());
        }
    }

    for i in 1..=STACK_PAGES {
        space.map(STACK_TOP - i * PAGE, true).ok_or_else(|| String::from("out of memory"))?;
    }
    let entry = base + u64_at(elf, 24).ok_or_else(bad)?;
    Ok((space, entry))
}

/// Puts the arguments on the process stack: the strings, below them an array of (address, length) pairs.
/// Returns (rsp, argc, argv).
fn push_args(space: &mut AddressSpace, args: &[&str]) -> (u64, u64, u64) {
    let mut sp = STACK_TOP;
    let mut pairs = Vec::new();
    for a in args.iter().rev() {
        let bytes = &a.as_bytes()[..a.len().min(4096)];
        sp -= bytes.len() as u64;
        space.poke(sp, bytes);
        pairs.push((sp, bytes.len() as u64));
    }
    pairs.reverse();
    sp = (sp - pairs.len() as u64 * 16) & !15;
    let argv = sp;
    for (i, (ptr, len)) in pairs.iter().enumerate() {
        space.poke(argv + i as u64 * 16, &ptr.to_le_bytes());
        space.poke(argv + i as u64 * 16 + 8, &len.to_le_bytes());
    }
    // As if we were called with call: rsp ≡ 8 (mod 16), a zero return address on top.
    sp = argv - 8;
    space.poke(sp, &0u64.to_le_bytes());
    (sp, pairs.len() as u64, argv)
}

/// System calls since boot.
pub static SYSCALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub struct Process {
    pub handle: Handle,
    /// The exit code: appears when the process ends.
    pub exit_code: Arc<IrqLock<Option<i32>>>,
}

/// Runs program `path`; `args[0]` is its name by tradition.
pub fn spawn(path: &str, args: &[&str], output: Arc<IrqLock<Vec<u8>>>) -> Result<Process, String> {
    spawn_as(fs::name(path), path, path, args, output)
}

/// The same under another name for the task list: a Lua program is /bin/lua,
/// and its rights are computed from the script `program` itself.
pub fn spawn_as(name: &str, path: &str, program: &str, args: &[&str], output: Arc<IrqLock<Vec<u8>>>) -> Result<Process, String> {
    let cred = access::for_program(program, args).ok_or("nobody is signed in")?;
    spawn_with(name, path, args, output, cred)
}

/// The same with ready rights (for services: they run even without a logged-in user).
pub fn spawn_with(name: &str, path: &str, args: &[&str], output: Arc<IrqLock<Vec<u8>>>, cred: Arc<access::Cred>) -> Result<Process, String> {
    let elf = fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let (mut space, entry) = load(&elf).map_err(|e| format!("{path}: {e}"))?;
    let (rsp, argc, argv) = push_args(&mut space, args);
    let exit_code = Arc::new(IrqLock::new(None));
    let handle = task::spawn_user(name, Some(output), space, entry, rsp, (argc, argv), exit_code.clone(), cred);
    kprintln!("process: started {path} as thread {}", handle.id);
    Ok(Process { handle, exit_code })
}

/// The process crashed (an exception in ring 3): report it and end only it.
pub fn crashed(name: &str, vector: usize, error: u64, rip: u64) -> ! {
    let detail = if vector == 14 {
        let addr = crate::paging::fault_address();
        let what = if error & 2 != 0 { "write to" } else { "read from" };
        format!("{what} {addr:#x}")
    } else {
        format!("error code {error:#x}")
    };
    let msg = format!("Segmentation fault: {name}, {detail} at {rip:#x}\n");
    kprintln!("process: thread {}: {}", task::current_id(), msg.trim_end());
    task::write_output(msg.as_bytes());
    task::exit_with(CRASHED)
}

/// The `int 0x80` handler. `frame` is the saved registers (r15 first, rax last).
pub extern "sysv64" fn syscall_entry(frame: *mut u64) -> *mut u64 {
    let regs = unsafe { core::slice::from_raw_parts_mut(frame, 15) };
    let (nr, a0, a1, a2, a3) = (regs[14], regs[9], regs[10], regs[11], regs[5]);
    if nr == SYS_EXIT {
        task::exit_with(a0 as i32);
    }
    SYSCALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    task::set_in_syscall(true);
    // While the call runs, let other threads preempt us as usual.
    unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
    regs[14] = syscall(nr, a0, a1, a2, a3) as u64;
    task::set_in_syscall(false);
    // Nothing is left on the kernel stack here: the process can be stopped.
    if task::should_stop() {
        task::exit_with(task::KILLED);
    }
    unsafe { core::arch::asm!("cli", options(nomem, nostack)) };
    frame
}

/// Writes to process memory in portions: interrupts are disabled under the address space lock,
/// and a gigabyte at once would stop the whole computer.
fn write_in_parts(space: &IrqLock<AddressSpace>, va: u64, data: &[u8]) -> bool {
    data.chunks(1 << 20).enumerate().all(|(i, part)| space.lock().write(va + (i << 20) as u64, part))
}

fn syscall(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let Some(space) = task::current_space() else { return -1 };
    let read_str = |ptr: u64, len: u64| -> Option<String> {
        if len > 4096 {
            return None;
        }
        let bytes = space.lock().read(ptr, len as usize)?;
        String::from_utf8(bytes).ok()
    };
    // Relative paths are from the home folder; fs decides what is allowed.
    let read_path = |ptr: u64, len: u64| read_str(ptr, len).map(|p| fs::resolve(&access::home(), &p));
    match nr {
        SYS_WRITE => {
            let Some(bytes) = space.lock().read(a1, a2.min(1 << 20) as usize) else { return -1 };
            if !task::write_output(&bytes) {
                use core::fmt::Write;
                let _ = crate::serial::Serial.write_str(&String::from_utf8_lossy(&bytes));
            }
            bytes.len() as i64
        }
        SYS_READ_FILE => {
            let Some(path) = read_path(a0, a1) else { return -1 };
            let Ok(data) = fs::read(&path) else { return -1 };
            let n = data.len().min(a3 as usize);
            if !write_in_parts(&space, a2, &data[..n]) {
                return -1;
            }
            data.len() as i64
        }
        SYS_WRITE_FILE => {
            let Some(path) = read_path(a0, a1) else { return -1 };
            let Some(data) = space.lock().read(a2, a3.min(16 << 20) as usize) else { return -1 };
            if fs::write(&path, &data).is_ok() { 0 } else { -1 }
        }
        SYS_LIST_DIR => {
            let Some(path) = read_path(a0, a1) else { return -1 };
            let Ok(entries) = fs::list(&path) else { return -1 };
            let mut text = String::new();
            for e in entries {
                text.push_str(&e.name);
                text.push_str(if e.is_dir { "/\n" } else { "\n" });
            }
            let n = text.len().min(a3 as usize);
            if !space.lock().write(a2, &text.as_bytes()[..n]) {
                return -1;
            }
            text.len() as i64
        }
        SYS_SLEEP => {
            task::sleep_until(interrupts::ticks() + (a0 * HZ).div_ceil(1000));
            0
        }
        SYS_TICKS => (interrupts::ticks() * 1000 / HZ) as i64,
        SYS_YIELD => {
            task::yield_now();
            0
        }
        SYS_MAP => {
            let mut sp = space.lock();
            let len = a0.div_ceil(PAGE) * PAGE;
            let start = sp.heap_next;
            // Process pages come from the shared heap: the kernel needs a reserve.
            if len == 0 || start + len > USER_BASE + (64 << 30) + HEAP_LIMIT || crate::heap_stats().1 < len as usize + HEAP_RESERVE {
                return 0;
            }
            for p in (start..start + len).step_by(PAGE as usize) {
                if sp.map(p, true).is_none() {
                    return 0;
                }
            }
            sp.heap_next += len;
            start as i64
        }
        SYS_GETPID => task::current_id() as i64,
        SYS_LUA => {
            if a2 > LUA_ARGS_MAX {
                return -1;
            }
            let Some(args) = space.lock().read(a1, a2 as usize).and_then(|b| wire::decode(&b)) else { return -1 };
            let Some(values) = crate::lua::call(a0 as usize, args) else { return -1 };
            let reply = wire::encode(&values);
            let n = reply.len();
            if n <= wire::SHORT_REPLY {
                if !space.lock().write(a3, &reply) {
                    return -1;
                }
            } else {
                REPLIES.lock().insert(task::current_id(), reply);
            }
            n as i64
        }
        SYS_LUA_FETCH => {
            let Some(reply) = REPLIES.lock().remove(&task::current_id()) else { return -1 };
            let n = reply.len().min(a1 as usize);
            if !write_in_parts(&space, a0, &reply[..n]) {
                return -1;
            }
            n as i64
        }
        _ => -1,
    }
}
