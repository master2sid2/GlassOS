//! The graphics accelerator: a common interface for any GPU.
//!
//! The window manager and frame output do not know which graphics card is installed: they ask
//! the accelerator to copy or fill a rectangle, get a completion fence and
//! wait for it only when they touch the same pixels themselves. A GPU driver
//! registers itself here once it passes its self-test. If there is no accelerator or it
//! fails — the old CPU path (SSE) works; it is always there.
//!
//! Drivers: `gpu` — the Intel Gen7 blitter (Bay Trail).

use crate::sync::IrqLock;

/// A completion fence: the command batch number. Larger means later.
pub type Fence = u32;

/// A memory rectangle as the accelerator sees it: the address in its space and the line stride in bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Surface {
    pub addr: u32,
    pub pitch: u32,
}

/// A picture as the accelerator sees it: address, size in pixels, line stride in bytes (32 bits per pixel).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Image {
    pub addr: u32,
    pub w: u32,
    pub h: u32,
    pub pitch: u32,
}

/// What the accelerator can do. What it cannot do, the CPU does.
pub trait Accel: Sync {
    fn name(&self) -> &'static str;

    /// Makes kernel memory `[addr, addr + bytes)` visible to the accelerator (remembers it: the same
    /// piece does not need to be mapped a second time). Returns the accelerator address.
    fn import(&self, addr: usize, bytes: usize) -> Option<u32>;

    /// Blurs piece `area` of picture `src` and puts it at the same place in `dst` (it may
    /// be the same picture); waits until done. The radius is as in `Canvas::blur`.
    /// false — not supported or it failed (then the CPU blurs).
    fn blur(&self, _dst: Image, _src: Image, _area: (u32, u32, u32, u32), _radius: u32) -> bool {
        false
    }

    /// Puts piece `from` (x, y, width, height) of picture `src` with premultiplied alpha
    /// over `dst` at point `to`; waits until done. false — not supported or it failed.
    fn over(&self, _dst: Image, _to: (u32, u32), _src: Image, _from: (u32, u32, u32, u32)) -> bool {
        false
    }

    /// Queues a copy of a `w`×`h` rectangle (32 bits per pixel). false — it failed,
    /// then the CPU does the whole batch.
    fn copy(&self, dst: Surface, dx: u32, dy: u32, src: Surface, sx: u32, sy: u32, w: u32, h: u32) -> bool;

    /// Queues a fill of a rectangle with a colour.
    #[allow(dead_code)] // for scene fills (the next steps)
    fn fill(&self, dst: Surface, x: u32, y: u32, w: u32, h: u32, color: u32) -> bool;

    /// Starts what was queued; the fence appears once everything before it is in memory.
    fn submit(&self) -> Option<Fence>;

    /// Whether everything up to the fence is done.
    fn done(&self, fence: Fence) -> bool;

    /// Waits for a fence. false — the accelerator hung (then it is not used anymore).
    fn wait(&self, fence: Fence) -> bool {
        let start = crate::arch::rdtsc();
        while !self.done(fence) {
            // Spin briefly (usually enough), then give the CPU to others.
            if crate::arch::rdtsc() - start < 100_000 {
                core::hint::spin_loop();
            } else if crate::arch::rdtsc() - start > 4_000_000_000 {
                return false;
            } else {
                crate::task::yield_now();
            }
        }
        true
    }
}

static ACTIVE: IrqLock<Option<&'static dyn Accel>> = IrqLock::new(None);

/// A GPU driver comes into service.
pub fn register(a: &'static dyn Accel) {
    crate::kprintln!("accel: {} takes over copying the frame to the screen", a.name());
    *ACTIVE.lock() = Some(a);
}

/// The accelerator hung or failed: from now on the CPU draws.
pub fn drop_active(why: &str) {
    if let Some(a) = ACTIVE.lock().take() {
        crate::kprintln!("accel: {} turned off ({why}); the CPU draws again", a.name());
    }
}

/// The active accelerator (None — the CPU draws).
pub fn get() -> Option<&'static dyn Accel> {
    *ACTIVE.lock()
}
