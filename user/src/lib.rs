//! The GlassOS program library: system calls, print!, arguments, memory.
//!
//! A program links it and declares an entry point:
//!
//! ```ignore
//! #![no_std]
//! #![no_main]
//! glass::entry!(main);
//! fn main(args: &[&str]) -> i32 { glass::println!("hi"); 0 }
//! ```

#![no_std]

extern crate alloc;

use core::arch::asm;
use core::fmt;

pub use alloc::{format, string::String, vec, vec::Vec};

mod nr {
    pub const EXIT: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const READ_FILE: u64 = 2;
    pub const WRITE_FILE: u64 = 3;
    pub const SLEEP: u64 = 4;
    pub const TICKS: u64 = 5;
    pub const YIELD: u64 = 6;
    pub const MAP: u64 = 7;
    pub const GETPID: u64 = 8;
    pub const LIST_DIR: u64 = 9;
    pub const LUA: u64 = 10;
    pub const LUA_FETCH: u64 = 11;
}

/// A system call: the number in rax, the arguments in rdi, rsi, rdx, r10.
fn syscall(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!("int 0x80", inlateout("rax") nr as i64 => ret, in("rdi") a0, in("rsi") a1, in("rdx") a2, in("r10") a3,
             options(nostack));
    }
    ret
}

pub fn exit(code: i32) -> ! {
    syscall(nr::EXIT, code as u64, 0, 0, 0);
    unreachable!()
}

pub fn write(bytes: &[u8]) {
    syscall(nr::WRITE, 1, bytes.as_ptr() as u64, bytes.len() as u64, 0);
}

/// Reads a whole file.
pub fn read_file(path: &str) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 4096];
    loop {
        let n = syscall(nr::READ_FILE, path.as_ptr() as u64, path.len() as u64, buf.as_mut_ptr() as u64, buf.len() as u64);
        if n < 0 {
            return None;
        }
        if n as usize <= buf.len() {
            buf.truncate(n as usize);
            return Some(buf);
        }
        buf.resize(n as usize, 0);
    }
}

pub fn write_file(path: &str, data: &[u8]) -> bool {
    syscall(nr::WRITE_FILE, path.as_ptr() as u64, path.len() as u64, data.as_ptr() as u64, data.len() as u64) == 0
}

/// The names in a folder (folders end with `/`).
pub fn list_dir(path: &str) -> Option<Vec<String>> {
    let mut buf = vec![0u8; 4096];
    loop {
        let n = syscall(nr::LIST_DIR, path.as_ptr() as u64, path.len() as u64, buf.as_mut_ptr() as u64, buf.len() as u64);
        if n < 0 {
            return None;
        }
        if n as usize <= buf.len() {
            let text = core::str::from_utf8(&buf[..n as usize]).ok()?;
            return Some(text.lines().map(String::from).collect());
        }
        buf.resize(n as usize, 0);
    }
}

/// A Lua API operation. The values in both directions are encoded (see lua/wire.rs).
/// `short` — how many bytes the kernel puts into the answer right away. false — the operation was rejected.
pub fn lua_op(op: u64, args: &[u8], reply: &mut Vec<u8>, short: usize) -> bool {
    reply.resize(short, 0);
    let n = syscall(nr::LUA, op, args.as_ptr() as u64, args.len() as u64, reply.as_mut_ptr() as u64);
    if n < 0 {
        return false;
    }
    let n = n as usize;
    if n <= short {
        reply.truncate(n);
        return true;
    }
    reply.resize(n, 0);
    syscall(nr::LUA_FETCH, reply.as_mut_ptr() as u64, n as u64, 0, 0) == n as i64
}

pub fn sleep(ms: u64) {
    syscall(nr::SLEEP, ms, 0, 0, 0);
}

/// Milliseconds since boot.
pub fn ticks() -> u64 {
    syscall(nr::TICKS, 0, 0, 0, 0) as u64
}

pub fn yield_now() {
    syscall(nr::YIELD, 0, 0, 0, 0);
}

pub fn pid() -> u64 {
    syscall(nr::GETPID, 0, 0, 0, 0) as u64
}

/// The privilege level the code runs at (3 for programs).
pub fn privilege_level() -> u16 {
    let cs: u16;
    unsafe { asm!("mov {0:x}, cs", out(reg) cs, options(nomem, nostack, preserves_flags)) };
    cs & 3
}

// ---------------------------------------------------------------- output

pub struct Stdout;

impl fmt::Write for Stdout {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write(s.as_bytes());
        Ok(())
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    // Build the whole line so it goes out in one system call.
    let s = alloc::fmt::format(args);
    write(s.as_bytes());
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::_print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::_print(format_args!("{}\n", format_args!($($arg)*))) };
}

// ---------------------------------------------------------------- memory

/// The allocator. Small blocks (and Lua asks almost only for those) live in bins
/// by size: both allocation and freeing are one list operation, however many
/// blocks there are. Large ones come from an ordinary heap with a free list, which
/// grows at the kernel's expense. The process is single-threaded, so no locks are needed.
struct Allocator;

const STEP: usize = 16;
const SMALL_MAX: usize = 2048;
const SLAB: usize = 64 * 1024;
const CHUNK: usize = 1 << 20;

struct Bins {
    /// Free blocks of each size: a block stores the address of the next one.
    free: [usize; SMALL_MAX / STEP + 1],
    /// The not yet cut remainder of a chunk.
    next: usize,
    end: usize,
}

static mut BINS: Bins = Bins { free: [0; SMALL_MAX / STEP + 1], next: 0, end: 0 };
static HEAP: linked_list_allocator::LockedHeap = linked_list_allocator::LockedHeap::empty();

fn large(layout: core::alloc::Layout) -> *mut u8 {
    let mut heap = HEAP.lock();
    loop {
        if let Ok(p) = heap.allocate_first_fit(layout) {
            return p.as_ptr();
        }
        let chunk = (layout.size() + layout.align() + 4096).max(CHUNK).next_multiple_of(4096);
        let base = syscall(nr::MAP, chunk as u64, 0, 0, 0) as usize;
        if base == 0 {
            return core::ptr::null_mut();
        }
        // The kernel hands out memory contiguously: the heap simply gets longer.
        unsafe {
            if heap.size() == 0 {
                heap.init(base as *mut u8, chunk);
            } else {
                heap.extend(chunk);
            }
        }
    }
}

unsafe impl core::alloc::GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        if layout.size() > SMALL_MAX || layout.align() > STEP {
            return large(layout);
        }
        let class = layout.size().div_ceil(STEP).max(1);
        let bins = unsafe { &mut *core::ptr::addr_of_mut!(BINS) };
        let head = bins.free[class];
        if head != 0 {
            bins.free[class] = unsafe { *(head as *const usize) };
            return head as *mut u8;
        }
        let size = class * STEP;
        if bins.next + size > bins.end {
            let slab = large(unsafe { core::alloc::Layout::from_size_align_unchecked(SLAB, STEP) });
            if slab.is_null() {
                return slab;
            }
            bins.next = slab as usize;
            bins.end = bins.next + SLAB;
        }
        let p = bins.next;
        bins.next += size;
        p as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        if layout.size() > SMALL_MAX || layout.align() > STEP {
            unsafe { HEAP.lock().deallocate(core::ptr::NonNull::new_unchecked(ptr), layout) };
            return;
        }
        let class = layout.size().div_ceil(STEP).max(1);
        let bins = unsafe { &mut *core::ptr::addr_of_mut!(BINS) };
        unsafe { *(ptr as *mut usize) = bins.free[class] };
        bins.free[class] = ptr as usize;
    }
}

#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

// ---------------------------------------------------------------- startup

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("panic: {}", info);
    exit(101)
}

/// The arguments as the kernel puts them: (address, length) pairs.
#[doc(hidden)]
pub fn _start(argc: usize, argv: *const [usize; 2], main: fn(&[&str]) -> i32) -> ! {
    let mut args = Vec::with_capacity(argc);
    for i in 0..argc {
        let [ptr, len] = unsafe { *argv.add(i) };
        let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
        args.push(core::str::from_utf8(bytes).unwrap_or(""));
    }
    exit(main(&args))
}

/// Declares the program entry point: `fn main(args: &[&str]) -> i32`.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn _start(argc: usize, argv: *const [usize; 2]) -> ! {
            $crate::_start(argc, argv, $main)
        }
    };
}
