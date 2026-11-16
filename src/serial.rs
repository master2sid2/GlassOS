//! COM1 (a 16550 UART) — the debug output. In QEMU it goes to target/serial.log.
//! It also accumulates in memory — on a computer without a COM port the log can be seen
//! with the `dmesg` command. The start (boot) is always kept, after that the last 192 KiB:
//! frequent Wi-Fi lines must not push out what the kernel said at startup.

use crate::arch::{inb, outb};
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const HEAD_SIZE: usize = 64 * 1024;
const RING_SIZE: usize = 192 * 1024;

/// The start of the log and a ring for the rest. Written under the BUSY flag. If another
/// CPU holds it — wait (it will finish soon); if this same one does (output from an interrupt on top of
/// other output, a panic) — the line goes only to COM1, without waiting.
struct Log(UnsafeCell<([u8; HEAD_SIZE], [u8; RING_SIZE])>);
unsafe impl Sync for Log {}
static LOG: Log = Log(UnsafeCell::new(([0; HEAD_SIZE], [0; RING_SIZE])));
/// How many bytes were written in total.
static LOG_LEN: AtomicUsize = AtomicUsize::new(0);
static BUSY: AtomicBool = AtomicBool::new(false);
/// Who holds BUSY: the CPU number + 1.
static OWNER: AtomicUsize = AtomicUsize::new(0);

/// Takes BUSY; false — this same CPU holds it (waiting is useless) or it is held too long.
fn take_busy() -> bool {
    let me = crate::gdt::cpu() + 1;
    let start = crate::arch::rdtsc();
    while BUSY.swap(true, Ordering::Acquire) {
        if OWNER.load(Ordering::Relaxed) == me || crate::arch::rdtsc() - start > 200_000_000 {
            return false;
        }
        core::hint::spin_loop();
    }
    OWNER.store(me, Ordering::Relaxed);
    true
}

fn log(s: &str) {
    // While BUSY is ours, we must not be preempted: another CPU is waiting.
    crate::sync::without_interrupts(|| log_locked(s))
}

fn log_locked(s: &str) {
    if !take_busy() {
        return;
    }
    let (head, ring) = unsafe { &mut *LOG.0.get() };
    let mut at = LOG_LEN.load(Ordering::Relaxed);
    for b in s.bytes() {
        if at < HEAD_SIZE {
            head[at] = b;
        } else {
            ring[(at - HEAD_SIZE) % RING_SIZE] = b;
        }
        at += 1;
    }
    LOG_LEN.store(at, Ordering::Relaxed);
    OWNER.store(0, Ordering::Relaxed);
    BUSY.store(false, Ordering::Release);
}

/// The kernel log: the start of boot and the last lines (the middle may have been pushed out — in
/// its place there is a line about the gap).
pub fn log_text() -> String {
    // Copy with interrupts disabled: while BUSY is ours, we will not be preempted.
    let (len, mut start, mut rest) = crate::sync::without_interrupts(|| {
        while !take_busy() {
            core::hint::spin_loop();
        }
        let (head, ring) = unsafe { &*LOG.0.get() };
        let len = LOG_LEN.load(Ordering::Relaxed);
        let start = Vec::from(&head[..len.min(HEAD_SIZE)]);
        let mut rest = Vec::new();
        for i in HEAD_SIZE.max(len.saturating_sub(RING_SIZE))..len {
            rest.push(ring[(i - HEAD_SIZE) % RING_SIZE]);
        }
        OWNER.store(0, Ordering::Relaxed);
        BUSY.store(false, Ordering::Release);
        (len, start, rest)
    });
    let skipped = len.saturating_sub(HEAD_SIZE + RING_SIZE);
    if skipped > 0 {
        // Line fragments at the edges of the gap are removed.
        start.truncate(start.iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1));
        let from = rest.iter().position(|&b| b == b'\n').map_or(rest.len(), |p| p + 1);
        rest.drain(..from);
        start.extend_from_slice(alloc::format!("... {} KB of the log skipped ...\n", skipped.div_ceil(1024)).as_bytes());
    }
    start.extend_from_slice(&rest);
    String::from_utf8_lossy(&start).into_owned()
}

/// How many log bytes were written in total: the position for `log_since`.
pub fn log_end() -> usize {
    LOG_LEN.load(Ordering::Relaxed)
}

/// The log after position `from` and the new position: this way it can be followed. If
/// the start has already been pushed out of the ring, there is a line about the gap in its place.
pub fn log_since(from: usize) -> (String, usize) {
    // Copy with interrupts disabled: while BUSY is ours, we will not be preempted.
    let (out, len, skipped) = crate::sync::without_interrupts(|| {
        while !take_busy() {
            core::hint::spin_loop();
        }
        let (head, ring) = unsafe { &*LOG.0.get() };
        let len = LOG_LEN.load(Ordering::Relaxed);
        let oldest = HEAD_SIZE.max(len.saturating_sub(RING_SIZE));
        let mut out = Vec::new();
        let mut at = from.min(len);
        if at < HEAD_SIZE {
            out.extend_from_slice(&head[at..len.min(HEAD_SIZE)]);
            at = len.min(HEAD_SIZE);
        }
        // A gap only happens once the log is already going around the ring.
        let skipped = if at < len { oldest.saturating_sub(at) } else { 0 };
        for i in at.max(oldest)..len {
            out.push(ring[(i - HEAD_SIZE) % RING_SIZE]);
        }
        OWNER.store(0, Ordering::Relaxed);
        BUSY.store(false, Ordering::Release);
        (out, len, skipped)
    });
    let mut text = String::new();
    if skipped > 0 {
        text = alloc::format!("... {} KB of the log skipped ...\n", skipped.div_ceil(1024));
    }
    text.push_str(&String::from_utf8_lossy(&out));
    (text, len)
}

const COM1: u16 = 0x3F8;

pub fn init() {
    outb(COM1 + 1, 0x00); // no interrupts
    outb(COM1 + 3, 0x80); // DLAB
    outb(COM1, 0x01); // divisor 1 => 115200 baud
    outb(COM1 + 1, 0x00);
    outb(COM1 + 3, 0x03); // 8N1
    outb(COM1 + 2, 0xC7); // FIFO
    outb(COM1 + 4, 0x0B);
}

fn put(b: u8) {
    for _ in 0..100_000 {
        if inb(COM1 + 5) & 0x20 != 0 {
            break;
        }
    }
    outb(COM1, b);
}

pub struct Serial;

/// A log line is assembled whole and goes out in one piece: otherwise lines from different
/// CPUs mix in the middle. A long one goes out in 512-byte parts.
struct Line {
    buf: [u8; 512],
    len: usize,
}

impl Line {
    fn flush(&mut self) {
        // Cut only at a character boundary: the remainder moves to the start.
        let cut = (0..=self.len).rev().find(|&i| core::str::from_utf8(&self.buf[..i]).is_ok()).unwrap_or(0);
        if let Ok(text) = core::str::from_utf8(&self.buf[..cut]) {
            let _ = fmt::Write::write_str(&mut Serial, text);
        }
        self.buf.copy_within(cut..self.len, 0);
        self.len -= cut;
    }
}

impl fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if self.len == self.buf.len() {
                self.flush();
            }
            self.buf[self.len] = b;
            self.len += 1;
        }
        Ok(())
    }
}

/// Writes a line to the log and COM1 (for `kprintln!`).
pub fn print_line(args: fmt::Arguments) {
    let mut line = Line { buf: [0; 512], len: 0 };
    let _ = fmt::Write::write_fmt(&mut line, args);
    let _ = fmt::Write::write_str(&mut line, "\n");
    line.flush();
}

impl fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        log(s);
        for b in s.bytes() {
            if b == b'\n' {
                put(b'\r');
            }
            put(b);
        }
        Ok(())
    }
}

#[macro_export]
macro_rules! kprintln {
    ($($arg:tt)*) => {{
        $crate::serial::print_line(format_args!($($arg)*));
    }};
}
