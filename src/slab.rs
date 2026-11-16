//! Blocks of one size, allocated and freed in constant time. Blocks are cut from
//! chunks aligned to their size: the chunk header is found directly from a block's address,
//! and a completely free chunk goes back to the heap.

use core::ptr::null_mut;

/// The chunk and block sizes of a pool. `slab` is a power of two, blocks start at `first`.
#[derive(Clone, Copy)]
pub struct Shape {
    pub slab: usize,
    pub block: usize,
    pub first: usize,
}

/// How much room the chunk header takes.
pub const HEADER: usize = size_of::<Slab>();

#[repr(C)]
struct Slab {
    prev: *mut Slab,
    next: *mut Slab,
    /// Returned blocks: each stores the address of the next one.
    free: *mut u8,
    /// The start of the part of the chunk never handed out yet.
    fresh: usize,
    used: usize,
}

impl Slab {
    fn is_full(&self, shape: Shape) -> bool {
        self.free.is_null() && self.fresh + shape.block > shape.slab
    }
}

/// Chunks that have free blocks.
pub struct Pool(*mut Slab);

// The chunks are ordinary kernel memory, and the pool lives under a lock.
unsafe impl Send for Pool {}

impl Pool {
    pub const fn new() -> Self {
        Self(null_mut())
    }

    unsafe fn link(&mut self, slab: *mut Slab) {
        unsafe {
            (*slab).prev = null_mut();
            (*slab).next = self.0;
            if !self.0.is_null() {
                (*self.0).prev = slab;
            }
        }
        self.0 = slab;
    }

    unsafe fn unlink(&mut self, slab: *mut Slab) {
        let (prev, next) = unsafe { ((*slab).prev, (*slab).next) };
        if prev.is_null() {
            self.0 = next;
        } else {
            unsafe { (*prev).next = next };
        }
        if !next.is_null() {
            unsafe { (*next).prev = prev };
        }
    }

    /// A block, or null if `grow` did not provide a new chunk.
    pub fn take(&mut self, shape: Shape, grow: impl FnOnce() -> *mut u8) -> *mut u8 {
        if self.0.is_null() {
            let slab = grow() as *mut Slab;
            if slab.is_null() {
                return null_mut();
            }
            unsafe {
                slab.write(Slab { prev: null_mut(), next: null_mut(), free: null_mut(), fresh: shape.first, used: 0 });
                self.link(slab);
            }
        }
        let slab = self.0;
        unsafe {
            let s = &mut *slab;
            let block = if s.free.is_null() {
                let block = (slab as *mut u8).add(s.fresh);
                s.fresh += shape.block;
                block
            } else {
                let block = s.free;
                s.free = *(block as *mut *mut u8);
                block
            };
            s.used += 1;
            if s.is_full(shape) {
                self.unlink(slab);
            }
            block
        }
    }

    /// Returns a block; `release` gets a chunk that has no blocks left.
    ///
    /// # Safety
    /// `block` was handed out by this pool with this same `shape`.
    pub unsafe fn put(&mut self, block: *mut u8, shape: Shape, release: impl FnOnce(*mut u8)) {
        let slab = (block as usize & !(shape.slab - 1)) as *mut Slab;
        unsafe {
            let s = &mut *slab;
            let was_full = s.is_full(shape);
            *(block as *mut *mut u8) = s.free;
            s.free = block;
            s.used -= 1;
            if was_full {
                self.link(slab);
            }
            if s.used == 0 {
                self.unlink(slab);
                release(slab as *mut u8);
            }
        }
    }
}
