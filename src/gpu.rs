//! Bay Trail GPU engines (Valleyview, Intel Gen7): the blitter and the render engine.
//!
//! The screen is driven by `baytrail`; this is what the GPU can compute by itself.
//! The blitter (BCS) fills and copies rectangles following commands from a ring in memory.
//! The ring and buffers live in ordinary kernel memory; the GPU sees them through the
//! GTT page table at its own addresses. The pages are marked "snoop the CPU cache"
//! (snoop): the CPU does not need to flush its cache before commands — this is verified by
//! the self-test at startup.
//!
//! Register numbers and the command format come from Intel's open GPU documentation for
//! Bay Trail (Programmer's Reference Manual) and Mesa (MIT).

use crate::accel::{Accel, Fence, Surface};
use crate::pci;
use crate::sync::IrqLock;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

const DEVICES: [u16; 4] = [0x0F30, 0x0F31, 0x0F32, 0x0F33];
/// Registers (2 MiB) followed by the GTT (2 MiB: 512 K entries — 2 GiB of GPU addresses).
const MMIO_SIZE: u64 = 4 << 20;
const GTT: usize = 2 << 20;
const GTT_ENTRIES: usize = 512 * 1024;
const GTT_FLUSH: usize = 0x10_1008;
const PAGE: usize = 4096;
/// Our buffers go into the GTT starting at 1 GiB: below are the firmware frame and our screen frame
/// (within the aperture); above the aperture the CPU cannot see them anyway, and we do not need it to.
const GTT_BASE: usize = 1 << 30;

// A Valleyview GTT entry: address [31:12] and [39:32] in bits 11:4, "writeable", "valid",
// "the GPU snoops the CPU cache".
const PTE_VALID: u32 = 1;
const PTE_WRITABLE: u32 = 1 << 1;
const PTE_SNOOPED: u32 = 1 << 2;

// GPU wake-up: until it is awake, the engine registers do not answer.
// On Valleyview the blitter is in the "media" power well, render in its own.
const FORCEWAKE_RENDER: usize = 0x13_00B0;
const FORCEWAKE_RENDER_ACK: usize = 0x13_00B4;
const FORCEWAKE_MEDIA: usize = 0x13_00B8;
const FORCEWAKE_MEDIA_ACK: usize = 0x13_00BC;
const FORCEWAKE_KERNEL: u32 = 1;

// Engine rings: the registers are the same, the addresses differ.
const BCS: usize = 0x2_2000;
const RCS: usize = 0x0_2000;
const RING_TAIL: usize = 0x30;
const RING_HEAD: usize = 0x34;
const RING_START: usize = 0x38;
const RING_CTL: usize = 0x3C;
const RING_MI_MODE: usize = 0x9C;
const RING_VALID: u32 = 1;
const HEAD_MASK: u32 = 0x001F_FFFC;
const MI_STOP_RING: u32 = 1 << 8;
const RING_PAGES: usize = 4;
const RING_BYTES: usize = RING_PAGES * PAGE;

// Commands.
const MI_NOOP: u32 = 0;
/// Wait until all previous commands of the engine have finished writing memory, then write
/// a number at a GTT address (a completion fence). 3 dwords: command, address, number.
const MI_FLUSH_DW: u32 = 0x26 << 23 | 1;
const MI_FLUSH_DW_STORE_DWORD: u32 = 1 << 14;
const MI_FLUSH_DW_USE_GTT: u32 = 1 << 2;
const XY_COLOR_BLT: u32 = 2 << 29 | 0x50 << 22 | 3 << 20 | 4;
const XY_SRC_COPY_BLT: u32 = 2 << 29 | 0x53 << 22 | 3 << 20 | 6;
const BLT_32BPP: u32 = 3 << 24;
const ROP_PATCOPY: u32 = 0xF0 << 16;
const ROP_SRCCOPY: u32 = 0xCC << 16;

/// PIPE_CONTROL (render engine): cache flushes, stalls and a fence write. 4 dwords.
/// Flags as in the i915 driver (MIT) for Gen7.
const PIPE_CONTROL: u32 = 3 << 29 | 3 << 27 | 2 << 24 | 2;
const PC_CS_STALL: u32 = 1 << 20;
const PC_GLOBAL_GTT: u32 = 1 << 24;
const PC_QW_WRITE: u32 = 1 << 14;
const PC_RT_FLUSH: u32 = 1 << 12;
const PC_INSTRUCTION_INVALIDATE: u32 = 1 << 11;
const PC_TEXTURE_INVALIDATE: u32 = 1 << 10;
const PC_FLUSH_ENABLE: u32 = 1 << 7;
const PC_DC_FLUSH: u32 = 1 << 5;
const PC_VF_INVALIDATE: u32 = 1 << 4;
const PC_CONST_INVALIDATE: u32 = 1 << 3;
const PC_STATE_INVALIDATE: u32 = 1 << 2;
const PC_STALL_AT_SCOREBOARD: u32 = 1 << 1;
const PC_DEPTH_FLUSH: u32 = 1;

/// A scratch slot in the fence page: commands that must write something by the rules write here.
const SCRATCH: u32 = 64;

/// How long to wait for an engine before considering it hung.
const WAIT_CYCLES: u64 = 2_000_000_000;

mod render;

/// The command ring of one engine (blitter or render) with a completion fence.
struct Ring {
    mmio: usize,
    /// Ring registers of this engine (BCS or RCS).
    base: usize,
    /// Render writes the fence with PIPE_CONTROL, the blitter with MI_FLUSH_DW.
    render: bool,
    /// The ring in kernel memory and where the next command goes.
    ring: *mut u32,
    tail: usize,
    /// The completion fence: the GPU writes the number of the last completed batch here.
    fence: *const u32,
    fence_gpu: u32,
    seqno: u32,
}

unsafe impl Send for Ring {}

static BLITTER: crate::sync::Mutex<Option<Ring>> = crate::sync::Mutex::new(None);
static RENDER: crate::sync::Mutex<Option<Ring>> = crate::sync::Mutex::new(None);
/// The next free GPU address for our buffers.
static NEXT_GTT: IrqLock<usize> = IrqLock::new(GTT_BASE);
static LAST: IrqLock<String> = IrqLock::new(String::new());

fn rd(mmio: usize, reg: usize) -> u32 {
    unsafe { ((mmio + reg) as *const u32).read_volatile() }
}

fn wr(mmio: usize, reg: usize, value: u32) {
    unsafe { ((mmio + reg) as *mut u32).write_volatile(value) }
}

/// A write to a "masked" register: the upper 16 bits say which lower bits to change.
fn masked(bits: u32, on: bool) -> u32 {
    bits << 16 | if on { bits } else { 0 }
}

fn wait(mmio: usize, reg: usize, mask: u32, value: u32) -> bool {
    let start = crate::arch::rdtsc();
    while rd(mmio, reg) & mask != value {
        if crate::arch::rdtsc() - start > WAIT_CYCLES / 10 {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

/// Wakes the GPU (both wells) and keeps it awake while we use it.
fn wake(mmio: usize) -> Result<(), String> {
    for (reg, ack, name) in [(FORCEWAKE_RENDER, FORCEWAKE_RENDER_ACK, "render"), (FORCEWAKE_MEDIA, FORCEWAKE_MEDIA_ACK, "media")] {
        wr(mmio, reg, masked(FORCEWAKE_KERNEL, true));
        if !wait(mmio, ack, FORCEWAKE_KERNEL, FORCEWAKE_KERNEL) {
            return Err(format!("the {name} part of the GPU does not wake up (ack {:#x})", rd(mmio, ack)));
        }
    }
    Ok(())
}

/// Maps kernel memory `[addr, addr + size)` into the GTT; returns its GPU address.
fn map(mmio: usize, addr: usize, size: usize) -> Option<u32> {
    let pages = size.div_ceil(PAGE);
    let at = {
        let mut next = NEXT_GTT.lock();
        let at = *next;
        if at / PAGE + pages > GTT_ENTRIES {
            return None;
        }
        *next += pages * PAGE;
        at
    };
    let table = (mmio + GTT) as *mut u32;
    for p in 0..pages {
        let phys = (addr & !(PAGE - 1)) as u64 + (p * PAGE) as u64;
        let pte = phys as u32 & 0xFFFF_F000 | (phys >> 28) as u32 & 0xFF0 | PTE_SNOOPED | PTE_WRITABLE | PTE_VALID;
        unsafe { table.add(at / PAGE + p).write_volatile(pte) };
    }
    wr(mmio, GTT_FLUSH, 1);
    let _ = rd(mmio, GTT_FLUSH);
    Some(at as u32)
}

/// The same as `map`, but without snooping the CPU cache: the GPU goes straight to memory,
/// and the CPU must flush the cache itself (clflush) before handing the data over.
fn map_raw(mmio: usize, addr: usize, size: usize) -> Option<u32> {
    let gpu = map(mmio, addr, size)?;
    let table = (mmio + GTT) as *mut u32;
    for p in 0..size.div_ceil(PAGE) {
        let slot = unsafe { table.add(gpu as usize / PAGE + p) };
        unsafe { slot.write_volatile(slot.read_volatile() & !PTE_SNOOPED) };
    }
    wr(mmio, GTT_FLUSH, 1);
    let _ = rd(mmio, GTT_FLUSH);
    Some(gpu)
}

/// Page-aligned memory (for the ring and GPU buffers).
fn alloc_pages(size: usize) -> Option<*mut u8> {
    let layout = core::alloc::Layout::from_size_align(size.next_multiple_of(PAGE), PAGE).ok()?;
    let p = unsafe { alloc::alloc::alloc_zeroed(layout) };
    (!p.is_null()).then_some(p)
}

impl Ring {
    fn new(mmio: usize, base: usize) -> Result<Self, String> {
        let ring = alloc_pages(RING_BYTES).ok_or("no memory for the command ring")? as *mut u32;
        let ring_gpu = map(mmio, ring as usize, RING_BYTES).ok_or("no room in the GTT")?;
        let b = base;
        let name = if base == RCS { "render" } else { "blitter" };
        wr(mmio, b + RING_MI_MODE, masked(MI_STOP_RING, false));
        wr(mmio, b + RING_CTL, 0);
        wr(mmio, b + RING_HEAD, 0);
        wr(mmio, b + RING_TAIL, 0);
        wr(mmio, b + RING_START, ring_gpu);
        wr(mmio, b + RING_CTL, ((RING_PAGES as u32 - 1) << 12) | RING_VALID);
        let ctl = rd(mmio, b + RING_CTL);
        let start = rd(mmio, b + RING_START);
        if ctl & RING_VALID == 0 || start != ring_gpu {
            return Err(format!("the {name} ring did not take (ctl {ctl:#x}, start {start:#x}, wanted {ring_gpu:#x})"));
        }
        let fence = alloc_pages(PAGE).ok_or("no memory for the fence")? as *const u32;
        let fence_gpu = map(mmio, fence as usize, PAGE).ok_or("no room in the GTT")?;
        Ok(Self { mmio, base, render: base == RCS, ring, tail: 0, fence, fence_gpu, seqno: 0 })
    }

    fn head(&self) -> usize {
        (rd(self.mmio, self.base + RING_HEAD) & HEAD_MASK) as usize
    }

    /// Puts commands into the ring and starts them. The ring is circular: if there is not enough room
    /// before the end, the rest is filled with no-ops and we start from the beginning.
    fn submit(&mut self, cmds: &[u32]) -> Result<(), String> {
        // The tail must be on an 8-byte boundary: with an odd number of dwords — a no-op.
        let words = cmds.len() + cmds.len() % 2;
        let bytes = words * 4;
        if bytes + 8 > RING_BYTES {
            return Err("too many commands at once".into());
        }
        if self.tail + bytes > RING_BYTES {
            self.wait_idle()?;
            while self.tail < RING_BYTES {
                unsafe { self.ring.add(self.tail / 4).write_volatile(MI_NOOP) };
                self.tail += 4;
            }
            self.tail = 0;
        }
        // Do not overrun commands not yet read.
        let start = crate::arch::rdtsc();
        loop {
            let head = self.head();
            let free = (head + RING_BYTES - self.tail - 8) % RING_BYTES;
            if head == self.tail || free >= bytes {
                break;
            }
            if crate::arch::rdtsc() - start > WAIT_CYCLES {
                return Err(self.hung());
            }
            core::hint::spin_loop();
        }
        for (i, &c) in cmds.iter().chain(core::iter::repeat_n(&MI_NOOP, words - cmds.len())).enumerate() {
            unsafe { self.ring.add(self.tail / 4 + i).write_volatile(c) };
        }
        self.tail = (self.tail + bytes) % RING_BYTES;
        // The commands are in memory before the engine learns about the new tail.
        core::sync::atomic::fence(Ordering::SeqCst);
        wr(self.mmio, self.base + RING_TAIL, self.tail as u32);
        Ok(())
    }

    /// A fence at the end of a batch: the number appears in memory once everything before it is done and written.
    /// "The ring has been read" does not mean that: the engine reads commands ahead.
    fn fence(&mut self) -> Result<u32, String> {
        self.seqno = self.seqno.wrapping_add(1).max(1);
        if self.render {
            // First wait until the pipeline finishes drawing, then flush the render cache
            // to memory and write the fence (Gen7 requires this: CS stall separately).
            let flush = PC_RT_FLUSH | PC_DEPTH_FLUSH | PC_DC_FLUSH | PC_FLUSH_ENABLE | PC_QW_WRITE | PC_GLOBAL_GTT | PC_CS_STALL;
            let cmds = [PIPE_CONTROL, PC_CS_STALL | PC_STALL_AT_SCOREBOARD, 0, 0, PIPE_CONTROL, flush, self.fence_gpu, self.seqno];
            self.submit(&cmds)?;
        } else {
            let cmds = [MI_FLUSH_DW | MI_FLUSH_DW_STORE_DWORD, self.fence_gpu | MI_FLUSH_DW_USE_GTT, self.seqno, MI_NOOP];
            self.submit(&cmds)?;
        }
        Ok(self.seqno)
    }

    /// Before a render batch: texture, state and command caches are reloaded from memory
    /// (the CPU may have rewritten both the images and the batch itself).
    fn invalidate(&mut self) -> Result<(), String> {
        // On Gen7 a CS_STALL must go together with a memory write
        // (or a render cache flush): without it the engine hangs. We write to the scratch slot.
        // The previous drawing goes from the cache to memory (the next one may read it as a texture).
        let inv = PC_TEXTURE_INVALIDATE | PC_STATE_INVALIDATE | PC_CONST_INVALIDATE | PC_VF_INVALIDATE | PC_INSTRUCTION_INVALIDATE | PC_CS_STALL | PC_QW_WRITE | PC_GLOBAL_GTT;
        self.submit(&[
            PIPE_CONTROL,
            PC_CS_STALL | PC_STALL_AT_SCOREBOARD,
            0,
            0,
            PIPE_CONTROL,
            PC_RT_FLUSH | PC_DEPTH_FLUSH | PC_CS_STALL,
            0,
            0,
            PIPE_CONTROL,
            inv,
            self.fence_gpu + SCRATCH,
            0,
        ])
    }

    /// Waits for fence `seqno`.
    fn wait(&self, seqno: u32) -> Result<(), String> {
        let start = crate::arch::rdtsc();
        while (unsafe { self.fence.read_volatile() }.wrapping_sub(seqno) as i32) < 0 {
            if crate::arch::rdtsc() - start > WAIT_CYCLES {
                return Err(self.hung());
            }
            core::hint::spin_loop();
        }
        Ok(())
    }

    /// Waits until the engine has read the ring to the end (only to rewrite its start).
    fn wait_idle(&self) -> Result<(), String> {
        let start = crate::arch::rdtsc();
        while self.head() != self.tail {
            if crate::arch::rdtsc() - start > WAIT_CYCLES {
                return Err(self.hung());
            }
            core::hint::spin_loop();
        }
        Ok(())
    }

    fn hung(&self) -> String {
        format!(
            "the {} stopped: head {:#x} tail {:#x} acthd {:#x} last command {:#010x} instdone {:#x} fence {}/{} ctl {:#x} mi_mode {:#x}",
            if self.render { "render engine" } else { "blitter" },
            self.head(),
            self.tail,
            rd(self.mmio, self.base + 0x74),
            rd(self.mmio, self.base + 0x68),
            rd(self.mmio, self.base + 0x6C),
            unsafe { self.fence.read_volatile() },
            self.seqno,
            rd(self.mmio, self.base + RING_CTL),
            rd(self.mmio, self.base + RING_MI_MODE)
        )
    }
}

fn fill_cmds(dst: Surface, x: u32, y: u32, w: u32, h: u32, color: u32) -> [u32; 6] {
    [XY_COLOR_BLT, BLT_32BPP | ROP_PATCOPY | dst.pitch, y << 16 | x, (y + h) << 16 | (x + w), dst.addr, color]
}

#[allow(clippy::too_many_arguments)]
fn copy_cmds(dst: Surface, dx: u32, dy: u32, src: Surface, sx: u32, sy: u32, w: u32, h: u32) -> [u32; 8] {
    [XY_SRC_COPY_BLT, BLT_32BPP | ROP_SRCCOPY | dst.pitch, dy << 16 | dx, (dy + h) << 16 | (dx + w), dst.addr, sy << 16 | sx, src.pitch, src.addr]
}

/// Coordinates and the line stride in blitter commands are signed 16-bit.
fn fits(s: Surface, x: u32, y: u32, w: u32, h: u32) -> bool {
    s.pitch < 32768 && x + w < 32768 && y + h < 32768 && w > 0 && h > 0
}

// ---------------------------------------------------------------- the shared interface

/// Memory brought from outside (scenes, backings) is mapped into the GTT on first
/// use and remembered by address and size (kernel memory is mapped 1:1, so
/// the same address means the same pages). The space is 1.5 to 2 GiB of GPU addresses;
/// when it runs out — wait for the engines and lay everything out again.
const IMPORT_BASE: usize = 3 << 29;
const IMPORT_END: usize = 2 << 30;
static IMPORTS: IrqLock<(usize, Vec<(usize, usize, u32)>)> = IrqLock::new((IMPORT_BASE, Vec::new()));
/// Where the GPU writes completion fences (for `done` without locking the rings).
static FENCE_AT: AtomicUsize = AtomicUsize::new(0);
static RENDER_FENCE_AT: AtomicUsize = AtomicUsize::new(0);
static MMIO: AtomicUsize = AtomicUsize::new(0);

fn passed(at: &AtomicUsize, fence: Fence) -> bool {
    let at = at.load(Ordering::Relaxed) as *const u32;
    !at.is_null() && (unsafe { at.read_volatile() }.wrapping_sub(fence) as i32) >= 0
}

/// Waits for a fence while yielding the CPU (not under a lock: the work may take milliseconds).
fn wait_passed(at: &AtomicUsize, fence: Fence) -> Result<(), String> {
    let start = crate::arch::rdtsc();
    while !passed(at, fence) {
        let spent = crate::arch::rdtsc() - start;
        if spent > WAIT_CYCLES {
            let ring = if core::ptr::eq(at, &RENDER_FENCE_AT) { &RENDER } else { &BLITTER };
            return Err(ring.lock().as_ref().map_or_else(|| "no engine".into(), Ring::hung));
        }
        if spent < 100_000 {
            core::hint::spin_loop();
        } else {
            crate::task::yield_now();
        }
    }
    Ok(())
}

/// The last fences of both engines: wait until they are idle.
fn wait_engines() {
    let last = |r: &crate::sync::Mutex<Option<Ring>>| r.lock().as_ref().map(|r| r.seqno);
    if let Some(f) = last(&BLITTER) {
        let _ = wait_passed(&FENCE_AT, f);
    }
    if let Some(f) = last(&RENDER) {
        let _ = wait_passed(&RENDER_FENCE_AT, f);
    }
}

fn import(addr: usize, bytes: usize) -> Option<u32> {
    let mmio = MMIO.load(Ordering::Relaxed);
    let first = addr & !(PAGE - 1);
    let pages = (addr + bytes - first).div_ceil(PAGE);
    if mmio == 0 || bytes == 0 || pages * PAGE > IMPORT_END - IMPORT_BASE {
        return None;
    }
    let mut imports = IMPORTS.lock();
    if let Some(&(_, _, gpu)) = imports.1.iter().find(|&&(a, b, _)| (a, b) == (addr, bytes)) {
        return Some(gpu);
    }
    if imports.0 + pages * PAGE > IMPORT_END {
        // Out of space: nobody reads the old mappings anymore — start over.
        drop(imports);
        wait_engines();
        imports = IMPORTS.lock();
        *imports = (IMPORT_BASE, Vec::new());
    }
    let at = imports.0;
    let table = (mmio + GTT) as *mut u32;
    for p in 0..pages {
        let phys = (first + p * PAGE) as u64;
        let pte = phys as u32 & 0xFFFF_F000 | (phys >> 28) as u32 & 0xFF0 | PTE_SNOOPED | PTE_WRITABLE | PTE_VALID;
        unsafe { table.add(at / PAGE + p).write_volatile(pte) };
    }
    wr(mmio, GTT_FLUSH, 1);
    let _ = rd(mmio, GTT_FLUSH);
    imports.0 += pages * PAGE;
    let gpu = (at + (addr - first)) as u32;
    imports.1.push((addr, bytes, gpu));
    Some(gpu)
}

// ---------------------------------------------------------------- blur on the render engine

/// Batches in a circle: each has its own fence and is reused once the fence has passed.
const BATCHES: usize = 32;
/// Intermediate blur levels (downscaled copies): enough for a 2560×1440 screen with a margin.
const SCRATCH_BYTES: usize = 12 << 20;

struct Blurrer {
    batches: Vec<(render::Batch, Fence)>,
    next: usize,
    scratch: u32,
}

/// One blur at a time (shared batches and intermediate memory). A lock
/// that yields: we wait milliseconds for the GPU, without disabling interrupts.
static BLURRER: crate::sync::YieldLock<Option<Blurrer>> = crate::sync::YieldLock::new(None);
/// When the GPU last drew (tick) and whether the frequency is raised.
static LAST_WORK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static FAST: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn blurrer_new(mmio: usize) -> Option<Blurrer> {
    let mut batches = Vec::with_capacity(BATCHES);
    for _ in 0..BATCHES {
        let cpu = alloc_pages(render::BATCH_BYTES)?;
        batches.push((render::Batch { cpu: cpu as *mut u32, gpu: map(mmio, cpu as usize, render::BATCH_BYTES)? }, 0));
    }
    let scratch = alloc_pages(SCRATCH_BYTES)?;
    Some(Blurrer { batches, next: 0, scratch: map(mmio, scratch as usize, SCRATCH_BYTES)? })
}

impl Blurrer {
    /// One draw (a bilinear copy) in a fresh batch; returns the fence.
    fn draw(&mut self, dst: render::Image, to: render::Area, src: render::Image, from: render::From) -> Result<Fence, String> {
        self.draw_with(dst, to, src, from, render::Blend::Copy, true)
    }

    fn draw_with(&mut self, dst: render::Image, to: render::Area, src: render::Image, from: render::From, blend: render::Blend, linear: bool) -> Result<Fence, String> {
        let slot = self.next;
        self.next = (self.next + 1) % BATCHES;
        if self.batches[slot].1 != 0 {
            wait_passed(&RENDER_FENCE_AT, self.batches[slot].1)?;
        }
        let mut guard = RENDER.lock();
        let ring = guard.as_mut().ok_or("no render engine")?;
        render::draw(ring, &self.batches[slot].0, dst, to, src, from, blend, linear)?;
        let fence = ring.fence()?;
        self.batches[slot].1 = fence;
        Ok(fence)
    }

    /// Blurs piece `r` of `src` into the same piece of `dst`: halve K times (bilinear
    /// sampling exactly between four pixels gives their average) and upscale as many times.
    ///
    /// Blur strength is a fractional level L = log₂(radius) + a correction: take cascades of depth
    /// ⌊L⌋ and ⌊L⌋+1 and mix them by the fractional part of L (on the way up, at level ⌊L⌋ —
    /// upscaling is linear, so this is the same as mixing two finished blurs). The level
    /// grid is powers of two from the screen corner and does not depend on the radius: turning frost
    /// only changes the mix share, and the glass changes smoothly, as on the CPU; when a window
    /// moves, the sampling of the background is the same and the glass does not "swim". The margin around is 2 radii, as on
    /// the CPU; beyond the screen edge — the edge pixels (sampling clamped to the edge).
    fn blur(&mut self, dst: render::Image, src: render::Image, r: render::Area, radius: u32) -> Result<(), String> {
        let radius = radius.max(1);
        let level = (log2(radius) + BLUR_OFFSET.load(Ordering::Relaxed) as f32 / 4.0).max(1.0);
        let k = (level as u32).min(MAX_LEVELS - 1);
        let share = (level - k as f32).min(1.0);
        let depth = k + 1;
        let deep = 1u32 << depth;
        let margin = 2 * radius as i32;
        let snap_down = |v: i32| v.div_euclid(deep as i32) * deep as i32;
        let x0 = snap_down(r.0 as i32 - margin);
        let y0 = snap_down(r.1 as i32 - margin);
        let n = ((r.0 + r.2) as i32 + margin - x0) as u32 / deep + 1;
        let m = ((r.1 + r.3) as i32 + margin - y0) as u32 / deep + 1;
        // Levels lie in the intermediate memory one after another: level j (1..=depth) is n·2^(depth−j) × m·2^(depth−j).
        let mut levels: Vec<render::Image> = Vec::with_capacity(depth as usize);
        let mut at = self.scratch;
        for j in 1..=depth {
            let (w, h) = (n << (depth - j), m << (depth - j));
            levels.push(render::Image { addr: at, w, h, pitch: w * 4 });
            at += (w * h * 4).next_multiple_of(64);
        }
        if (at - self.scratch) as usize > SCRATCH_BYTES || levels[0].w > 16384 || levels[0].h > 16384 {
            return Err("the area is too big for the scratch memory".into());
        }
        let whole = |i: render::Image| (0.0, 0.0, i.w as f32, i.h as f32);
        // Down: source → level 1 (a 2×2 pixel) → 2 (4×4) …
        let mut from = (x0 as f32, y0 as f32, (n * deep) as f32, (m * deep) as f32);
        let mut img = src;
        for level in &levels {
            self.draw(*level, (0, 0, level.w, level.h), img, from)?;
            img = *level;
            from = whole(*level);
        }
        // Up: the smallest level is laid onto the next one with share `share` (a mix of two
        // depths), then plain upscaling to level 1.
        let last = levels.len() - 1;
        for i in (0..last).rev() {
            let (small, big) = (levels[i + 1], levels[i]);
            let blend = match share {
                _ if i + 1 != last || share >= 0.999 => render::Blend::Copy,
                // Zero share: this level keeps its own downscale.
                _ if share <= 0.001 => continue,
                _ => render::Blend::Mix(share),
            };
            self.draw_with(big, (0, 0, big.w, big.h), small, whole(small), blend, true)?;
        }
        // And into the target: piece `r` is a piece of the first level (its pixel is 2 screen pixels).
        let from = ((r.0 as i32 - x0) as f32 / 2.0, (r.1 as i32 - y0) as f32 / 2.0, r.2 as f32 / 2.0, r.3 as f32 / 2.0);
        let fence = self.draw(dst, r, levels[0], from)?;
        wait_passed(&RENDER_FENCE_AT, fence)
    }
}

/// There are never more cascade levels (radius up to ~90 pixels with the ¾ correction).
const MAX_LEVELS: u32 = 7;

/// log₂ of an integer without libm: the integer part from the top bit, the fraction from the approximation
/// log₂(1 + x) ≈ x·(1.3466 − 0.3466·x) on [0, 1) (error < 0.005, smooth and monotonic).
fn log2(v: u32) -> f32 {
    let whole = 31 - v.leading_zeros();
    let x = v as f32 / (1u32 << whole) as f32 - 1.0;
    whole as f32 + x * (1.3466 - 0.3466 * x)
}

/// The GPU is working: raise the frequency (if not yet), remember the time of work.
fn busy() {
    LAST_WORK.store(crate::interrupts::ticks(), Ordering::Relaxed);
    if !FAST.swap(true, Ordering::Relaxed) {
        let mmio = MMIO.load(Ordering::Relaxed);
        if mmio != 0 {
            raise_frequency(mmio);
        }
    }
}

/// Every half second: a second without work — lower the frequency (saves power).
fn idle_watch(mmio: usize) {
    loop {
        crate::task::sleep_until(crate::interrupts::ticks() + crate::interrupts::HZ / 2);
        let quiet = crate::interrupts::ticks().saturating_sub(LAST_WORK.load(Ordering::Relaxed)) > crate::interrupts::HZ;
        if quiet && FAST.load(Ordering::Relaxed) {
            // Work may be starting right now: clear the flag before lowering the frequency,
            // so that the next `busy` raises it again.
            FAST.store(false, Ordering::Relaxed);
            lower_frequency(mmio);
        }
    }
}

/// Whether to blur on the GPU (the `gpu blur on|off` command — to compare with the CPU).
static BLUR_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

pub fn set_blur(on: bool) {
    BLUR_ON.store(on, Ordering::Relaxed);
}

/// Cascade level = log₂(radius) + a correction (in quarters of a level).
/// ¾ was fitted on a model against the CPU blur (box blur twice, σ ≈ 0.8 radius):
/// the same strength, a frost step changes the glass just as softly, without sharp bands.
static BLUR_OFFSET: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(3);

/// The level correction in quarters (the `gpu blur offset N` command).
pub fn set_blur_offset(quarters: u32) {
    BLUR_OFFSET.store(quarters.min(12), Ordering::Relaxed);
}

struct Gen7;
static GEN7: Gen7 = Gen7;

impl Accel for Gen7 {
    fn name(&self) -> &'static str {
        "Intel Gen7 (blitter and render engine)"
    }

    fn import(&self, addr: usize, bytes: usize) -> Option<u32> {
        import(addr, bytes)
    }

    fn blur(&self, dst: crate::accel::Image, src: crate::accel::Image, area: (u32, u32, u32, u32), radius: u32) -> bool {
        if !BLUR_ON.load(Ordering::Relaxed) {
            return false;
        }
        let Some(mut guard) = BLURRER.try_lock() else { return false };
        let Some(b) = guard.as_mut() else { return false };
        busy();
        let img = |i: crate::accel::Image| render::Image { addr: i.addr, w: i.w, h: i.h, pitch: i.pitch };
        match b.blur(img(dst), img(src), area, radius) {
            Ok(()) => true,
            Err(e) => {
                drop(guard);
                crate::kprintln!("gpu: blur failed: {e}");
                *BLURRER.lock() = None;
                false
            }
        }
    }

    fn over(&self, dst: crate::accel::Image, to: (u32, u32), src: crate::accel::Image, from: (u32, u32, u32, u32)) -> bool {
        if !BLUR_ON.load(Ordering::Relaxed) {
            return false;
        }
        let Some(mut guard) = BLURRER.try_lock() else { return false };
        let Some(b) = guard.as_mut() else { return false };
        busy();
        let img = |i: crate::accel::Image| render::Image { addr: i.addr, w: i.w, h: i.h, pitch: i.pitch };
        let result = b
            .draw_with(img(dst), (to.0, to.1, from.2, from.3), img(src), render::whole(from), render::Blend::Over, false)
            .and_then(|fence| wait_passed(&RENDER_FENCE_AT, fence));
        match result {
            Ok(()) => true,
            Err(e) => {
                drop(guard);
                crate::kprintln!("gpu: drawing a window failed: {e}");
                *BLURRER.lock() = None;
                false
            }
        }
    }

    fn copy(&self, dst: Surface, dx: u32, dy: u32, src: Surface, sx: u32, sy: u32, w: u32, h: u32) -> bool {
        if !fits(dst, dx, dy, w, h) || !fits(src, sx, sy, w, h) {
            return false;
        }
        BLITTER.lock().as_mut().is_some_and(|b| b.submit(&copy_cmds(dst, dx, dy, src, sx, sy, w, h)).is_ok())
    }

    fn fill(&self, dst: Surface, x: u32, y: u32, w: u32, h: u32, color: u32) -> bool {
        if !fits(dst, x, y, w, h) {
            return false;
        }
        BLITTER.lock().as_mut().is_some_and(|b| b.submit(&fill_cmds(dst, x, y, w, h, color)).is_ok())
    }

    fn submit(&self) -> Option<Fence> {
        BLITTER.lock().as_mut().and_then(|b| b.fence().ok())
    }

    fn done(&self, fence: Fence) -> bool {
        passed(&FENCE_AT, fence)
    }
}

/// What is up with the GPU — for the report and the log.
pub fn state() -> String {
    LAST.lock().clone()
}

/// The GPU found (PCI device id), 0 — none.
static DEVICE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// For "Settings → About": "name: value" lines about the GPU.
pub fn about() -> Vec<(&'static str, String)> {
    let device = DEVICE.load(Ordering::Relaxed);
    if device == 0 {
        return alloc::vec![("Graphics", "UEFI frame buffer (no GPU driver for this card)".into())];
    }
    let mut rows = alloc::vec![("Graphics", format!("Intel HD Graphics, Bay Trail (Gen7), PCI 8086:{device:04X}"))];
    let render = BLURRER.lock().is_some();
    let last = state();
    let engines = match (crate::accel::get().is_some(), render) {
        _ if last.is_empty() => "being checked (about 25 s after the start)".into(),
        (true, true) => "blitter and 3D engine, both passed their checks".into(),
        (true, false) => "blitter works; the 3D engine is off".into(),
        _ => format!("not in use: {last}"),
    };
    rows.push(("GPU engines", engines));
    let drawing = if !render {
        "by the processor"
    } else if BLUR_ON.load(Ordering::Relaxed) {
        "by the GPU (blur, large windows, frame output)"
    } else {
        "by the processor (switched with `gpu blur off`)"
    };
    rows.push(("Glass and windows drawn", drawing.into()));
    if render {
        let speed = if FAST.load(Ordering::Relaxed) { "high: it is drawing now" } else { "low: idle, saving power" };
        rows.push(("GPU clock", speed.into()));
    }
    let mapped = (*NEXT_GTT.lock() - GTT_BASE) as u64;
    if mapped > 0 {
        rows.push(("Memory shared with the GPU", crate::apps::transfer::size_text(mapped)));
    }
    rows
}

fn note(text: String) {
    crate::kprintln!("gpu: {text}");
    *LAST.lock() = text;
}

/// Brings up the blitter and tests it (in the background, after boot).
pub fn start(devices: &[pci::Device]) {
    let Some(dev) = devices.iter().find(|d| d.vendor == 0x8086 && DEVICES.contains(&d.device)).copied() else { return };
    DEVICE.store(dev.device as u32, Ordering::Relaxed);
    crate::task::spawn("gpu-check", None, move || {
        // After boot: while the rest comes up, the benchmark lies.
        crate::task::sleep_until(crate::interrupts::ticks() + 25 * crate::interrupts::HZ);
        dev.enable_bus_master();
        let mmio = dev.mmio(0, MMIO_SIZE);
        if let Err(e) = wake(mmio) {
            return note(format!("no GPU engines: {e}"));
        }
        MMIO.store(mmio, Ordering::Relaxed);
        match Ring::new(mmio, BCS) {
            Ok(b) => {
                FENCE_AT.store(b.fence as usize, Ordering::Relaxed);
                *BLITTER.lock() = Some(b);
                match self_check(mmio) {
                    Ok(text) => {
                        note(text);
                        crate::accel::register(&GEN7);
                    }
                    Err(e) => note(format!("the blitter failed its check: {e}")),
                }
            }
            Err(e) => note(format!("no blitter: {e}")),
        }
        // The GPU frequency: it does not rise by itself, the driver requests it from the power unit.
        crate::kprintln!("gpu: {}", raise_frequency(mmio));
        // The render engine: only a self-test for now (composition and blur are the next step).
        match Ring::new(mmio, RCS) {
            Ok(r) => {
                RENDER_FENCE_AT.store(r.fence as usize, Ordering::Relaxed);
                *RENDER.lock() = Some(r);
                match render_check(mmio) {
                    Ok(text) => {
                        crate::kprintln!("gpu: {text}");
                        // Glass blur on the render engine; the frequency is raised while it works.
                        *BLURRER.lock() = blurrer_new(mmio);
                    }
                    Err(e) => crate::kprintln!("gpu: the render engine failed its check: {e}"),
                }
                lower_frequency(mmio);
                idle_watch(mmio);
            }
            Err(e) => crate::kprintln!("gpu: no render engine: {e}"),
        }
    });
}

// The Valleyview GPU frequency: frequency codes, Punit and fuse registers —
// as in the i915 driver (MIT): PUNIT_REG_GPU_FREQ_REQ 0xD4, PUNIT_REG_GPU_FREQ_STS 0xD8,
// PUNIT_REG_GPU_LFM 0xD3, IOSF_NC_FB_GFX_FREQ_FUSE 0x1C (maximum in bits 10:3).
const PUNIT_FREQ_REQ: u32 = 0xD4;
const PUNIT_FREQ_STS: u32 = 0xD8;
const PUNIT_LFM: u32 = 0xD3;
const NC_FREQ_FUSE: u32 = 0x1C;
const FREQ_BUSY: u32 = 1;

/// Requests the highest GPU frequency from the power unit; the result as a line for the log.
fn raise_frequency(mmio: usize) -> String {
    use crate::baytrail::{nc_read, punit_read, punit_write};
    let (Some(sts), Some(lfm), Some(fuse)) = (punit_read(mmio, PUNIT_FREQ_STS), punit_read(mmio, PUNIT_LFM), nc_read(mmio, NC_FREQ_FUSE)) else {
        return "frequency: the power unit does not answer".into();
    };
    let now = sts >> 8 & 0xFF;
    let min = (lfm & 0xFF).max(0xC0);
    let max = ((fuse & 0x7F8) >> 3).min(0xEA);
    if max <= min {
        return format!("frequency: code {now:#x}, range {min:#x}..{max:#x} looks wrong, left alone");
    }
    if !punit_write(mmio, PUNIT_FREQ_REQ, max) {
        return format!("frequency: code {now:#x}, could not ask for {max:#x}");
    }
    let start = crate::arch::rdtsc();
    let mut sts = sts;
    while crate::arch::rdtsc() - start < 200_000_000 {
        sts = punit_read(mmio, PUNIT_FREQ_STS).unwrap_or(0);
        if sts & FREQ_BUSY == 0 && sts >> 8 & 0xFF == max {
            break;
        }
        core::hint::spin_loop();
    }
    format!("frequency code {now:#x} → {:#x} (range {min:#x}..{max:#x})", sts >> 8 & 0xFF)
}

/// Returns the GPU to the lowest frequency.
fn lower_frequency(mmio: usize) {
    use crate::baytrail::{punit_read, punit_write};
    if let Some(lfm) = punit_read(mmio, PUNIT_LFM) {
        punit_write(mmio, PUNIT_FREQ_REQ, (lfm & 0xFF).max(0xC0));
    }
}

/// An image in kernel memory visible to the GPU.
struct Pixels {
    cpu: *mut u32,
    img: render::Image,
    bytes: usize,
}

impl Pixels {
    fn new(mmio: usize, w: u32, h: u32) -> Option<Self> {
        let bytes = (w * h * 4) as usize;
        let cpu = alloc_pages(bytes)? as *mut u32;
        let addr = map(mmio, cpu as usize, bytes)?;
        Some(Self { cpu, img: render::Image { addr, w, h, pitch: w * 4 }, bytes })
    }

    fn px(&self) -> &mut [u32] {
        unsafe { core::slice::from_raw_parts_mut(self.cpu, (self.img.w * self.img.h) as usize) }
    }
}

impl Drop for Pixels {
    fn drop(&mut self) {
        if let Ok(layout) = core::alloc::Layout::from_size_align(self.bytes.next_multiple_of(PAGE), PAGE) {
            unsafe { alloc::alloc::dealloc(self.cpu as *mut u8, layout) };
        }
    }
}

/// Draws with the render engine and waits for the fence; cycles.
fn render_run(batch: &render::Batch, dst: &Pixels, to: render::Area, src: &Pixels, from: render::Area, blend: render::Blend, linear: bool) -> Result<u64, String> {
    let from = render::whole(from);
    let mut guard = RENDER.lock();
    let ring = guard.as_mut().ok_or("no render engine")?;
    let t = crate::arch::rdtsc();
    render::draw(ring, batch, dst.img, to, src.img, from, blend, linear)?;
    let seqno = ring.fence()?;
    ring.wait(seqno)?;
    Ok(crate::arch::rdtsc() - t)
}

/// Render self-test: a pixel-exact copy, composition with transparency against an exact
/// calculation, halving with bilinear sampling; a benchmark of compositing a whole screen.
fn render_check(mmio: usize) -> Result<String, String> {
    let batch_mem = alloc_pages(render::BATCH_BYTES).ok_or("no memory")?;
    let batch = render::Batch { cpu: batch_mem as *mut u32, gpu: map(mmio, batch_mem as usize, render::BATCH_BYTES).ok_or("no room in the GTT")? };
    let mut seed = 0x2545_F491u32;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    const N: u32 = 256;
    let a = Pixels::new(mmio, N, N).ok_or("no memory")?;
    let b = Pixels::new(mmio, N, N).ok_or("no memory")?;
    // 1. Copy.
    for p in a.px().iter_mut() {
        *p = next() | 0xFF00_0000;
    }
    b.px().fill(0);
    let full = (0, 0, N, N);
    render_run(&batch, &b, full, &a, full, render::Blend::Copy, false)?;
    if let Some(i) = (0..(N * N) as usize).find(|&i| b.px()[i] != a.px()[i]) {
        return Err(format!("copy: pixel {i} is {:#010x}, wanted {:#010x}", b.px()[i], a.px()[i]));
    }
    // 2. Composition: a window with premultiplied alpha over an opaque scene.
    for p in a.px().iter_mut() {
        let al = next() & 0xFF;
        let c = |v: u32| (v & 0xFF) * al / 255;
        let r = next();
        *p = al << 24 | c(r >> 16) << 16 | c(r >> 8) << 8 | c(r);
    }
    for p in b.px().iter_mut() {
        *p = next() | 0xFF00_0000;
    }
    let before: alloc::vec::Vec<u32> = b.px().to_vec();
    render_run(&batch, &b, full, &a, full, render::Blend::Over, false)?;
    let mut worst = 0i32;
    for i in 0..(N * N) as usize {
        let (s, d, got) = (a.px()[i], before[i], b.px()[i]);
        let al = (s >> 24) as f32 / 255.0;
        for sh in [0, 8, 16] {
            let want = ((s >> sh & 0xFF) as f32 + (d >> sh & 0xFF) as f32 * (1.0 - al) + 0.5) as i32;
            worst = worst.max((want - (got >> sh & 0xFF) as i32).abs());
        }
    }
    if worst > 2 {
        return Err(format!("over: a colour is off by {worst}"));
    }
    // 3. Halving: each pixel is the average of a 2×2 square.
    let c = Pixels::new(mmio, N / 2, N / 2).ok_or("no memory")?;
    render_run(&batch, &c, (0, 0, N / 2, N / 2), &b, full, render::Blend::Copy, true)?;
    let mut worst_half = 0i32;
    for y in 0..N / 2 {
        for x in 0..N / 2 {
            let at = |xx: u32, yy: u32| b.px()[(yy * N + xx) as usize];
            let quad = [at(2 * x, 2 * y), at(2 * x + 1, 2 * y), at(2 * x, 2 * y + 1), at(2 * x + 1, 2 * y + 1)];
            let got = c.px()[(y * N / 2 + x) as usize];
            for sh in [0, 8, 16] {
                let want = quad.iter().map(|p| (p >> sh & 0xFF) as i32).sum::<i32>() as f32 / 4.0;
                worst_half = worst_half.max(((want - (got >> sh & 0xFF) as f32).abs() + 0.5) as i32);
            }
        }
    }
    if worst_half > 2 {
        return Err(format!("half-size: a colour is off by {worst_half}"));
    }
    // 4. Mix: a quarter of the source and three quarters of the target (this is how the blur moves smoothly
    // from one level to the next).
    let before: alloc::vec::Vec<u32> = b.px().to_vec();
    render_run(&batch, &b, full, &a, full, render::Blend::Mix(0.25), false)?;
    let mut worst_mix = 0i32;
    for i in 0..(N * N) as usize {
        let (s, d, got) = (a.px()[i], before[i], b.px()[i]);
        for sh in [0, 8, 16, 24] {
            let want = (s >> sh & 0xFF) as f32 * 0.25 + (d >> sh & 0xFF) as f32 * 0.75;
            worst_mix = worst_mix.max(((want - (got >> sh & 0xFF) as f32).abs() + 0.5) as i32);
        }
    }
    if worst_mix > 2 {
        return Err(format!("mix: a colour is off by {worst_mix}"));
    }
    drop((a, b, c));
    // Benchmark: a full-screen 2560×1440 window over the scene — CPU (SSE) and GPU.
    let (w, h) = (2560, 1440);
    let win = Pixels::new(mmio, w, h).ok_or("no memory")?;
    let scene = Pixels::new(mmio, w, h).ok_or("no memory")?;
    for (i, p) in win.px().iter_mut().enumerate() {
        *p = if i % 7 == 0 { 0x8040_2010 } else { 0xFF33_6699 };
    }
    scene.px().fill(0xFF10_2030);
    let mut cpu = u64::MAX;
    for _ in 0..3 {
        let t = crate::arch::rdtsc();
        crate::gfx::simd::over_row(scene.px(), win.px());
        cpu = cpu.min(crate::arch::rdtsc() - t);
    }
    let all = (0, 0, w, h);
    let mut gpu = u64::MAX;
    for _ in 0..3 {
        gpu = gpu.min(render_run(&batch, &scene, all, &win, all, render::Blend::Over, false)?);
    }
    // The same without the GPU snooping the CPU cache (the data does not matter here, only the time).
    let raw = |p: &Pixels| render::Image { addr: map_raw(mmio, p.cpu as usize, p.bytes).unwrap_or(p.img.addr), ..p.img };
    let (scene_raw, win_raw) = (Pixels { img: raw(&scene), ..scene }, Pixels { img: raw(&win), ..win });
    let mut gpu_raw = u64::MAX;
    for _ in 0..3 {
        gpu_raw = gpu_raw.min(render_run(&batch, &scene_raw, all, &win_raw, all, render::Blend::Over, false)?);
    }
    core::mem::forget(scene_raw);
    core::mem::forget(win_raw);
    Ok(format!(
        "render engine ready (copy exact, over within {worst}, half-size within {worst_half}); a 2560x1440 window over the scene: CPU {} → GPU {} Mcycles ({} without snooping)",
        cpu / 1_000_000,
        gpu / 1_000_000,
        gpu_raw / 1_000_000
    ))
}

/// Fill and copy 8 MB with the blitter, check every pixel, benchmark against the CPU.
fn self_check(mmio: usize) -> Result<String, String> {
    // 8 MB is larger than the CPU cache (the N2807 has 1 MB L2), closer to a screen frame (14.7 MB).
    const W: usize = 2048;
    const H: usize = 1024;
    const BYTES: usize = W * H * 4;
    let a = alloc_pages(BYTES).ok_or("no memory")? as *mut u32;
    let b = alloc_pages(BYTES).ok_or("no memory")? as *mut u32;
    let sa = Surface { addr: map(mmio, a as usize, BYTES).ok_or("no room in the GTT")?, pitch: (W * 4) as u32 };
    let sb = Surface { addr: map(mmio, b as usize, BYTES).ok_or("no room in the GTT")?, pitch: (W * 4) as u32 };
    let pa = unsafe { core::slice::from_raw_parts_mut(a, W * H) };
    let pb = unsafe { core::slice::from_raw_parts_mut(b, W * H) };
    let run = |cmds: &[&[u32]]| -> Result<u64, String> {
        let mut guard = BLITTER.lock();
        let blt = guard.as_mut().ok_or("no blitter")?;
        let t = crate::arch::rdtsc();
        for c in cmds {
            blt.submit(c)?;
        }
        let seqno = blt.fence()?;
        blt.wait(seqno)?;
        Ok(crate::arch::rdtsc() - t)
    };
    // 1. Fill: the CPU writes cached, the GPU must overwrite everything.
    pa.fill(0xDEAD_BEEF);
    let gpu_fill = run(&[&fill_cmds(sa, 0, 0, W as u32, H as u32, 0x1122_3344)])?;
    if let Some(i) = pa.iter().position(|&p| p != 0x1122_3344) {
        return Err(format!("fill: pixel {i} is {:#010x} (the GPU does not see or write our memory coherently)", pa[i]));
    }
    // 2. Copy a pattern with part of the rectangle — with an offset.
    for (i, p) in pa.iter_mut().enumerate() {
        *p = (i as u32).wrapping_mul(2_654_435_761);
    }
    pb.fill(0);
    let gpu_copy = run(&[&copy_cmds(sb, 0, 0, sa, 0, 0, W as u32, H as u32)])?;
    if let Some(i) = (0..W * H).find(|&i| pb[i] != pa[i]) {
        return Err(format!("copy: pixel {i} is {:#010x}, wanted {:#010x}", pb[i], pa[i]));
    }
    let part = run(&[&copy_cmds(sb, 10, 20, sa, 100, 200, 64, 32)])?;
    let _ = part;
    for y in 0..32 {
        for x in 0..64 {
            if pb[(20 + y) * W + 10 + x] != pa[(200 + y) * W + 100 + x] {
                return Err(format!("copy of a part: wrong at {x},{y}"));
            }
        }
    }
    // The CPU for comparison.
    let t = crate::arch::rdtsc();
    pa.fill(core::hint::black_box(0x5566_7788));
    let cpu_fill = crate::arch::rdtsc() - t;
    let t = crate::arch::rdtsc();
    pb.copy_from_slice(core::hint::black_box(&*pa));
    let cpu_copy = crate::arch::rdtsc() - t;
    // The test memory is released; its GTT addresses remain, but the GPU no longer goes there.
    if let Ok(layout) = core::alloc::Layout::from_size_align(BYTES, PAGE) {
        unsafe {
            alloc::alloc::dealloc(a as *mut u8, layout);
            alloc::alloc::dealloc(b as *mut u8, layout);
        }
    }
    let k = |c: u64| c / 1000;
    Ok(format!(
        "blitter ready; 8 MB fill: CPU {} → GPU {} Kcycles, 8 MB copy: CPU {} → GPU {} Kcycles (snooped memory, checked pixel by pixel)",
        k(cpu_fill),
        k(gpu_fill),
        k(cpu_copy),
        k(gpu_copy)
    ))
}
