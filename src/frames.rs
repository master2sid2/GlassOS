//! Process memory pages. The kernel heap finds room by walking the hole list, and pages
//! mixed with small blocks create holes: with gigabytes of memory each next
//! page would walk hundreds of thousands of entries with interrupts disabled.
//!
//! So pages are cut from 2 MiB blocks (see `slab`). Page tables and
//! device buffers live in the heap: they need low memory.

use crate::paging::Page;
use crate::slab::{Pool, Shape};
use crate::sync::IrqLock;
use alloc::alloc::{Layout, alloc, dealloc};
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;

const PAGE: usize = size_of::<Page>();
const SLAB: usize = 2 << 20;
const SLAB_LAYOUT: Layout = unsafe { Layout::from_size_align_unchecked(SLAB, SLAB) };
/// The pages are aligned, so the first one is given entirely to the chunk header.
const SHAPE: Shape = Shape { slab: SLAB, block: PAGE, first: PAGE };

static POOL: IrqLock<Pool> = IrqLock::new(Pool::new());

/// A zeroed process page.
pub struct UserPage(NonNull<Page>);

// A page belongs to an address space, and that belongs to one process.
unsafe impl Send for UserPage {}

impl UserPage {
    /// None — no more memory.
    pub fn new() -> Option<Self> {
        let page = POOL.lock().take(SHAPE, || unsafe { alloc(SLAB_LAYOUT) }) as *mut Page;
        let page = NonNull::new(page)?;
        unsafe { page.as_ptr().write_bytes(0, 1) };
        Some(Self(page))
    }
}

impl Deref for UserPage {
    type Target = Page;

    fn deref(&self) -> &Page {
        unsafe { self.0.as_ref() }
    }
}

impl DerefMut for UserPage {
    fn deref_mut(&mut self) -> &mut Page {
        unsafe { self.0.as_mut() }
    }
}

impl Drop for UserPage {
    fn drop(&mut self) {
        unsafe { POOL.lock().put(self.0.as_ptr() as *mut u8, SHAPE, |slab| dealloc(slab, SLAB_LAYOUT)) };
    }
}
