#![no_std]
#![no_main]

extern crate alloc;

/// The system version: the one place to change it (the About window, Settings, /system/version,
/// `sys.version()` in Lua and the metrics exporter take it from here).
macro_rules! os_version {
    () => {
        "0.3"
    };
}
pub(crate) use os_version;

mod access;
mod acpi;
mod ahci;
mod irq;
mod aml;
mod apps;
mod appmenu;
mod arch;
mod audio;
mod cpu;
mod crash;
mod crypto;
#[cfg(feature = "debug-tools")]
mod debug;
mod backlight;
mod bootvars;
mod bt;
mod battery;
mod baytrail;
mod display;
mod filetype;
mod ata;
mod font;
mod frames;
mod fs;
mod gdt;
mod accel;
mod gfx;
mod gpu;
mod hdmi;
mod hid;
mod hwprobe;
mod i2c;
mod image;
mod input;
mod install;
mod keep;
mod keyboard;
mod interrupts;
mod lua;
mod net;
mod output;
mod paging;
mod parts;
mod png;
mod power;
mod pci;
mod process;
mod rtc;
mod rtl8723be;
mod watchdog;
mod saver;
mod touchpad;
mod metrics;
mod services;
mod serial;
mod slab;
mod smp;
mod splash;
mod stats;
mod sync;
mod task;
mod theme;
mod time;
mod usb;
mod users;
mod wallpaper;
mod wifi;
mod wm;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use gfx::Framebuffer;
use input::{Event, Input};
use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};
use linked_list_allocator::LockedHeap;
use uefi::boot::{self, AllocateType, MemoryType};
use uefi::mem::memory_map::MemoryMap;
use uefi::prelude::*;
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};
use uefi::runtime::ResetType;
use apps::login::{Login, Outcome};
use wm::{Ctx, Leave, Wm};

/// A heap memory region. The firmware cuts memory into pieces, so there are several regions.
struct Region {
    start: AtomicUsize,
    end: AtomicUsize,
    heap: LockedHeap,
}

/// Memory returned by the firmware is heavily fragmented, so there are spare slots.
const MAX_REGIONS: usize = 64;
/// The address limit reachable by 32-bit devices.
const LOW_END: u64 = 1 << 32;
/// Blocks from this size up are taken from high memory: low memory is needed by devices.
const BIG_BLOCK: usize = 1 << 20;

/// How many times a second to check the monitor connector.
const PORT_CHECKS_PER_SECOND: u64 = 4;

/// The kernel heap: interrupts are disabled during an operation, otherwise a preempted thread
/// holding the heap lock would block everyone else forever.
///
/// Regions below 4 GiB fill the array from the start, the rest from the end. Many devices
/// write to memory only at 32-bit addresses, and their buffers are small blocks
/// allocated at startup: such blocks are taken from below. Large ones (windows, files) first
/// look for room in the upper regions.
///
/// Small blocks live in size-class lists: in `LockedHeap` allocation and free walk the
/// hole list, and small blocks are exactly what creates those holes, so the file system
/// sector cache would bring the whole system to a halt. Their regions are taken from below.
struct KernelHeap([Region; MAX_REGIONS]);

const SMALL_MAX: usize = 2048;
const SMALL_STEP: usize = 16;
const SMALL_SLAB: Layout = unsafe { Layout::from_size_align_unchecked(64 << 10, 64 << 10) };
const SMALL_CLASSES: usize = SMALL_MAX / SMALL_STEP + 1;

static SMALL: sync::IrqLock<[slab::Pool; SMALL_CLASSES]> = sync::IrqLock::new([const { slab::Pool::new() }; SMALL_CLASSES]);

/// Self-check of the fast paths after boot.
fn self_check() {
    gfx::bench();
    crypto::check();
}

/// List index for a block, or None if the block goes directly to the heap.
fn small_class(layout: Layout) -> Option<usize> {
    (layout.size() <= SMALL_MAX && layout.align() <= SMALL_STEP).then(|| layout.size().div_ceil(SMALL_STEP).max(1))
}

fn small_shape(class: usize) -> slab::Shape {
    slab::Shape { slab: SMALL_SLAB.size(), block: class * SMALL_STEP, first: slab::HEADER.next_multiple_of(SMALL_STEP) }
}

impl KernelHeap {
    fn try_alloc(&self, regions: impl DoubleEndedIterator<Item = usize>, layout: Layout) -> Option<*mut u8> {
        regions.map(|i| unsafe { self.0[i].heap.alloc(layout) }).find(|p| !p.is_null())
    }

    /// Returns a block to the region of the area it was taken from.
    unsafe fn release(&self, ptr: *mut u8, layout: Layout) {
        let at = ptr as usize;
        if let Some(r) = self.0.iter().find(|r| (r.start.load(Ordering::Relaxed)..r.end.load(Ordering::Relaxed)).contains(&at)) {
            unsafe { r.heap.dealloc(ptr, layout) }
        }
    }
}

unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if let Some(class) = small_class(layout) {
            return SMALL.lock()[class].take(small_shape(class), || self.try_alloc(0..MAX_REGIONS, SMALL_SLAB).unwrap_or(core::ptr::null_mut()));
        }
        sync::without_interrupts(|| {
            let first = if layout.size() >= BIG_BLOCK { self.try_alloc((0..MAX_REGIONS).rev(), layout) } else { None };
            first.or_else(|| self.try_alloc(0..MAX_REGIONS, layout)).unwrap_or(core::ptr::null_mut())
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if let Some(class) = small_class(layout) {
            unsafe { SMALL.lock()[class].put(ptr, small_shape(class), |slab| self.release(slab, SMALL_SLAB)) };
            return;
        }
        sync::without_interrupts(|| unsafe { self.release(ptr, layout) })
    }
}

#[global_allocator]
static ALLOCATOR: KernelHeap = KernelHeap([const { Region { start: AtomicUsize::new(0), end: AtomicUsize::new(0), heap: LockedHeap::empty() } }; MAX_REGIONS]);

/// A region smaller than this is not worth adding to the heap while memory is plentiful.
const REGION_MIN: u64 = 8 << 20;
/// The same for memory returned by the firmware: regions there are smaller, and there are enough slots.
const RECLAIM_MIN: u64 = 1 << 20;
/// How much memory to leave to the firmware until ExitBootServices: an eighth,
/// but no less than this. After the exit it returns to the heap.
const FIRMWARE_MIN: u64 = 64 << 20;

/// Gives the heap a memory region. false — no more slots for regions.
fn add_region(start: *mut u8, size: usize) -> bool {
    let is_free = |&i: &usize| ALLOCATOR.0[i].end.load(Ordering::Relaxed) == 0;
    let slot = if (start as u64) < LOW_END { (0..MAX_REGIONS).find(is_free) } else { (0..MAX_REGIONS).rev().find(is_free) };
    let Some(slot) = slot else { return false };
    let r = &ALLOCATOR.0[slot];
    r.start.store(start as usize, Ordering::Relaxed);
    r.end.store(start as usize + size, Ordering::Relaxed);
    unsafe { r.heap.lock().init(start, size) };
    true
}

pub fn heap_stats() -> (usize, usize) {
    sync::without_interrupts(|| {
        ALLOCATOR.0.iter().fold((0, 0), |(used, free), r| {
            let heap = r.heap.lock();
            (used + heap.used(), free + heap.free())
        })
    })
}

/// Flushes unsaved FS changes before shutdown.
fn flush_storage() {
    keep::finish();
    output::settled();
    if fs::needs_saving()
        && let Err(e) = fs::sync()
    {
        kprintln!("fs: final sync failed: {e}");
    }
}

/// A reboot after which the firmware starts with `next`: its own setup or a USB stick.
pub fn reboot_into(next: bootvars::Next) -> ! {
    if let Err(e) = bootvars::prepare(next) {
        kprintln!("reboot: {e}");
    }
    reboot()
}

pub fn reboot() -> ! {
    kprintln!("reboot requested");
    output::announce("Restarting...");
    net::say_goodbye();
    flush_storage();
    crash::mark_clean();
    rtl8723be::stop();
    hdmi::stop();
    watchdog::stop();
    arch::cli();
    acpi::reboot();
    kprintln!("reboot: hardware reset failed, asking the firmware");
    uefi::runtime::reset(ResetType::COLD, Status::SUCCESS, None)
}

pub fn shutdown() -> ! {
    kprintln!("shutdown requested");
    output::announce("Shutting down...");
    net::say_goodbye();
    flush_storage();
    crash::mark_clean();
    rtl8723be::stop();
    hdmi::stop();
    watchdog::stop();
    arch::cli();
    acpi::shutdown();
    kprintln!("shutdown: ACPI S5 failed, asking the firmware");
    uefi::runtime::reset(ResetType::SHUTDOWN, Status::SUCCESS, None)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    arch::cli();
    kprintln!("KERNEL PANIC: {info}");
    let location = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
    let message = format!("{}", info.message());
    // Stop the other cores; leave a note for the report after the watchdog restart.
    crash::fatal(&format!("panic: {message} at {location}"));
    gfx::crash_screen(&message, &location);
    audio::crash_alarm();
    arch::halt_forever()
}

/// Choose the video mode through GOP while Boot Services are still alive.
fn setup_display() -> Framebuffer {
    let handle = boot::get_handle_for_protocol::<GraphicsOutput>().expect("GOP not found");
    let mut gop = boot::open_protocol_exclusive::<GraphicsOutput>(handle).expect("cannot open GOP");
    // In a VM — 1280×800; a real monitor may not have such a mode,
    // then the largest one up to 1920×1200 (larger is too expensive to draw).
    let modes: Vec<_> = gop.modes().filter(|m| matches!(m.info().pixel_format(), PixelFormat::Rgb | PixelFormat::Bgr)).collect();
    // The monitor's EDID as the firmware read it (it lies next to GOP).
    let edid = {
        let params = boot::OpenProtocolParams { handle, agent: boot::image_handle(), controller: None };
        let discovered = unsafe { boot::open_protocol::<uefi::proto::console::gop::EdidDiscovered>(params, boot::OpenProtocolAttributes::GetProtocol) };
        discovered.ok().and_then(|p| p.edid().map(|e| e.to_vec())).unwrap_or_default()
    };
    let list: Vec<String> = modes.iter().map(|m| format!("{}x{}", m.info().resolution().0, m.info().resolution().1)).collect();
    kprintln!("display: firmware modes {}; monitor: {}", list.join(" "), display::describe_edid(&edid));
    let native = display::native(&edid).map(|(w, h, _)| (w as usize, h as usize));
    // The mode chosen in Settings (if this screen has it), then the monitor's native mode
    // (if the firmware supports it and it is not larger than 1920×1200).
    let preferred = display::read_preference_uefi();
    let chosen = preferred.and_then(|p| modes.iter().find(|m| m.info().resolution() == (p.0 as usize, p.1 as usize)));
    let fits = |(w, h): (usize, usize)| w <= 1920 && h <= 1200;
    let chosen = chosen.or_else(|| native.filter(|&n| fits(n)).and_then(|n| modes.iter().find(|m| m.info().resolution() == n)));
    display::set_edid(edid);
    let best = chosen.or_else(|| modes.iter().find(|m| m.info().resolution() == (1280, 800))).or_else(|| {
        modes
            .iter()
            .filter(|m| {
                let (w, h) = m.info().resolution();
                (800..=1920).contains(&w) && (600..=1200).contains(&h)
            })
            .max_by_key(|m| {
                let (w, h) = m.info().resolution();
                w * h
            })
    });
    if let Some(mode) = best {
        let _ = gop.set_mode(&mode);
    }
    let info = gop.current_mode_info();
    let (w, h) = info.resolution();
    let mut list: Vec<(u32, u32)> = modes
        .iter()
        .map(|m| (m.info().resolution().0 as u32, m.info().resolution().1 as u32))
        .filter(|&(mw, mh)| mw >= 640 && mh >= 480)
        .collect();
    list.sort_by_key(|&(mw, mh)| core::cmp::Reverse((mw * mh, mw)));
    list.dedup();
    display::set_modes(list, (w as u32, h as u32));
    Framebuffer {
        base: gop.frame_buffer().as_mut_ptr().cast(),
        w: w as i32,
        h: h as i32,
        stride: info.stride() as i32,
        bgr: info.pixel_format() == PixelFormat::Bgr,
    }
}

/// Ranges below and above 4 GiB: pairs (start, length), a length may be zero.
fn split_at_low_end(start: u64, end: u64) -> [(u64, u64); 2] {
    [(start, end.min(LOW_END).saturating_sub(start)), (start.max(LOW_END), end.saturating_sub(start.max(LOW_END)))]
}

/// Free memory regions (start, length): below 4 GiB first, inside each from larger to
/// smaller. A region crossing the 4 GiB boundary is split in two; the first megabyte is left alone: it
/// holds the startup code of the other cores. There is no heap yet, so the result is in an array.
/// Returns all free memory and the number of regions.
fn free_chunks(out: &mut [(u64, u64); MAX_REGIONS]) -> (u64, usize) {
    let order = |c: (u64, u64)| (c.0 >= LOW_END, core::cmp::Reverse(c.1));
    let Ok(map) = boot::memory_map(MemoryType::LOADER_DATA) else { return (0, 0) };
    let (mut total, mut count) = (0, 0);
    for d in map.entries().filter(|d| d.ty == MemoryType::CONVENTIONAL) {
        let (start, end) = (d.phys_start.max(1 << 20), d.phys_start + d.page_count * 4096);
        total += end.saturating_sub(start);
        for chunk in split_at_low_end(start, end) {
            if chunk.1 < REGION_MIN {
                continue;
            }
            let at = out[..count].iter().position(|&c| order(chunk) < order(c)).unwrap_or(count);
            if at < MAX_REGIONS {
                count = (count + 1).min(MAX_REGIONS);
                out[at..count].rotate_right(1);
                out[at] = chunk;
            }
        }
    }
    (total, count)
}

/// The kernel heap is pages allocated from UEFI. This is done before ExitBootServices,
/// so that the heap already works while reading files from the ESP. The firmware does not
/// touch the LOADER_DATA type even after the exit. Paging is left as is:
/// UEFI already mapped all memory 1:1.
fn init_heap() {
    let mut chunks = [(0, 0); MAX_REGIONS];
    let (total, count) = free_chunks(&mut chunks);
    let mut budget = total.saturating_sub((total / 8).max(FIRMWARE_MIN));
    let mut region = 0;
    for &(start, len) in &chunks[..count] {
        let size = len.min(budget) & !0xFFF;
        if size < REGION_MIN {
            continue;
        }
        let Ok(heap) = boot::allocate_pages(AllocateType::Address(start), MemoryType::LOADER_DATA, size as usize / 4096) else { continue };
        add_region(heap.as_ptr(), size as usize);
        kprintln!("heap: {:p}, {} MiB", heap.as_ptr(), size >> 20);
        budget -= size;
        region += 1;
    }
    if region == 0 {
        panic!("no memory for kernel heap");
    }
}

/// Gives the heap everything that was left to the firmware: its temporary memory and the reserve not taken
/// by `init_heap`. Call once we have left its tables (pages, GDT, IDT). The main thread's stack
/// still belongs to it, so the region containing it stays in place.
fn reclaim_boot_memory(map: &impl MemoryMap) {
    let on_stack = 0u8;
    let stack = &raw const on_stack as u64;
    let usable = |ty| matches!(ty, MemoryType::CONVENTIONAL | MemoryType::BOOT_SERVICES_CODE | MemoryType::BOOT_SERVICES_DATA);
    let mut free: Vec<(u64, u64)> = map
        .entries()
        .filter(|d| usable(d.ty))
        .map(|d| (d.phys_start.max(1 << 20), d.phys_start + d.page_count * 4096))
        .filter(|&(start, end)| start < end && !(start..end).contains(&stack))
        .collect();
    free.sort_unstable();
    // Adjacent firmware descriptors are one region: otherwise there would not be enough slots.
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (start, end) in free {
        match merged.last_mut() {
            Some(last) if last.1 == start => last.1 = end,
            _ => merged.push((start, end)),
        }
    }
    let mut chunks: Vec<(u64, u64)> = merged.iter().flat_map(|&(start, end)| split_at_low_end(start, end)).filter(|c| c.1 >= RECLAIM_MIN).collect();
    chunks.sort_unstable_by_key(|c| core::cmp::Reverse(c.1));
    let (mut added, mut regions) = (0, 0);
    for (start, len) in chunks {
        if !add_region(start as *mut u8, len as usize) {
            break;
        }
        added += len;
        regions += 1;
    }
    let total: u64 = merged.iter().map(|c| c.1 - c.0).sum();
    kprintln!("heap: reclaimed {} MiB in {regions} regions, {} MiB left unused", added >> 20, (total - added) >> 20);
}

/// The partition we booted from: the "hard drive" node in the image's device path.
/// A CD (ISO) has no such node — which means we did not boot from an installed system.
fn find_boot_partition() -> Option<fs::BootPart> {
    use uefi::proto::device_path::media::PartitionSignature;
    use uefi::proto::device_path::{DevicePath, DevicePathNodeEnum};
    let device = boot::open_protocol_exclusive::<uefi::proto::loaded_image::LoadedImage>(boot::image_handle()).ok()?.device()?;
    // Only look: an exclusive open would disconnect the disk drivers.
    let params = boot::OpenProtocolParams { handle: device, agent: boot::image_handle(), controller: None };
    let path = unsafe { boot::open_protocol::<DevicePath>(params, boot::OpenProtocolAttributes::GetProtocol) }.ok()?;
    for node in path.node_iter() {
        let Ok(DevicePathNodeEnum::MediaHardDrive(hd)) = node.as_enum() else { continue };
        let (guid, mbr) = match hd.partition_signature() {
            PartitionSignature::Guid(g) => (Some(g.to_bytes()), None),
            PartitionSignature::Mbr(m) => (None, Some(m)),
            _ => (None, None),
        };
        kprintln!("boot partition: start {}, {} sectors, {}", hd.partition_start(), hd.partition_size(), if guid.is_some() { "GPT" } else { "MBR" });
        return Some(fs::BootPart { guid, mbr, start: hd.partition_start(), sectors: hd.partition_size() });
    }
    kprintln!("boot partition: none (booted from CD/ISO or a whole-disk image)");
    None
}

/// Programs from the \apps (scripts) and \bin (ELF) folders on the boot partition (ESP).
/// This makes them easy to write on the host: run.sh puts the project's apps/ and the built user/ there.
/// Returns pairs (path in the OS file system, contents).
fn load_esp_apps() -> Vec<(String, Vec<u8>)> {
    let mut apps = Vec::new();
    let Ok(proto) = boot::get_image_file_system(boot::image_handle()) else { return apps };
    let mut esp = uefi::fs::FileSystem::new(proto);
    for dir in ["apps", "bin", "firmware"] {
        let Ok(path) = uefi::CString16::try_from(format!("\\{dir}").as_str()) else { continue };
        let Ok(list) = esp.read_dir(path.as_ref()) else { continue };
        let entries: Vec<_> = list.filter_map(|e| e.ok()).filter(|e| !e.is_directory()).collect();
        for info in entries {
            let name = info.file_name().to_string();
            let Ok(path) = uefi::CString16::try_from(format!("\\{dir}\\{name}").as_str()) else { continue };
            if let Ok(data) = esp.read(path.as_ref()) {
                apps.push((format!("/{dir}/{name}"), data));
            }
            splash::step();
        }
    }
    // The kernel itself — for installing the system to disk and replacing the kernel over the network: the file
    // the firmware loaded (a trial kernel reads itself, not the previous one).
    let image = image_path().unwrap_or_else(|| String::from("\\EFI\\BOOT\\BOOTX64.EFI"));
    if let Ok(path) = uefi::CString16::try_from(image.as_str())
        && let Ok(data) = esp.read(path.as_ref())
    {
        *install::BOOT_IMAGE.lock() = data;
    }
    kprintln!("boot: kernel from {image}");
    *install::BOOT_PATH.lock() = image;
    apps
}

/// The path of the kernel file on the partition, as the firmware loaded it.
fn image_path() -> Option<String> {
    use uefi::proto::device_path::DevicePathNodeEnum;
    let loaded = boot::open_protocol_exclusive::<uefi::proto::loaded_image::LoadedImage>(boot::image_handle()).ok()?;
    let mut out = String::new();
    for node in loaded.file_path()?.node_iter() {
        let Ok(DevicePathNodeEnum::MediaFilePath(f)) = node.as_enum() else { continue };
        let part = String::from_utf16_lossy(&f.path_name().to_vec()).trim_end_matches('\0').to_string();
        // The path may span several nodes: join them with backslashes.
        if !out.is_empty() && !out.ends_with('\\') && !part.starts_with('\\') {
            out.push('\\');
        }
        out.push_str(&part);
    }
    (!out.is_empty()).then_some(out)
}

/// Mounts the disk and restores the FS from it; installs programs from the ESP into /apps and /bin.
fn mount_storage(esp_apps: Vec<(String, Vec<u8>)>) -> String {
    let disk = ata::Disk::detect().map(|disk| (apps::transfer::size_text(disk.sectors as u64 * 512), fs::mount(disk)));
    if let Some((size, None)) = &disk {
        kprintln!("disk: the {size} ATA disk holds other data (not FAT32, not empty), left untouched");
    }
    let disk_info = match disk {
        Some((size, Some(mounted))) => match mounted {
            fs::Mounted::Fat32 => format!("Disk: {size}, FAT32"),
            fs::Mounted::Migrated => format!("Disk: {size}, FAT32 (files moved from the old format)"),
            fs::Mounted::Formatted => {
                fs::create_defaults();
                format!("Disk: {size}, FAT32 (new)")
            }
        },
        _ => match fs::mount_sata().or_else(fs::mount_usb) {
            Some(info) => {
                if !fs::is_dir("/home") {
                    fs::create_defaults();
                }
                info
            }
            None => {
                fs::mount_ram();
                fs::create_defaults();
                keep::restore();
                let place = if keep::active() { "memory, settings and home also on the USB drive" } else { "memory" };
                format!("Disk: none (files are kept in {place})")
            }
        },
    };
    let _ = fs::mkdir("/apps");
    let _ = fs::mkdir("/bin");
    let _ = fs::mkdir("/firmware");
    let _ = fs::mkdir(fs::MEDIA);
    let _ = fs::mkdir(fs::NETWORK);
    for (path, data) in &esp_apps {
        let _ = fs::write(path, data);
    }
    // /bin is a system folder: programs no longer on the ESP are removed.
    if !esp_apps.is_empty() {
        for e in fs::list("/bin").unwrap_or_default() {
            let path = format!("/bin/{}", e.name);
            if !esp_apps.iter().any(|(p, _)| *p == path) {
                let _ = fs::remove(&path);
            }
        }
    }
    let _ = fs::write("/system/version", fs::OS_VERSION.as_bytes());
    // System files are in place (from disk, or without a disk — in memory, from the USB stick).
    fs::set_ready();
    // A new volume, moved files, new programs — saved in the background without delaying boot
    // (if there is nothing to change, the save writes nothing).
    if fs::has_disk() {
        fs::sync_in_background();
    }
    kprintln!("{disk_info}; {} apps from ESP", esp_apps.len());
    disk_info
}

/// Starts a session: its settings, then the desktop with the windows that were open.
fn enter(account: &users::Account, w: i32, h: i32, sysinfo: Vec<String>) -> Wm {
    task::as_system(|| users::make_home(&account.name));
    access::sign_in(&account.name, account.admin);
    users::set_last(&account.name);
    let saved = fs::read(&access::config(wm::SETTINGS_FILE)).map_or(theme::DEFAULT, |cfg| theme::from_config(&String::from_utf8_lossy(&cfg)));
    theme::update(|t| *t = saved);
    audio::set_volume(saved.sound as i32);
    audio::set_input_level(saved.mic_volume, saved.mic_boost);
    if saved.brightness > 0 {
        backlight::set(saved.brightness);
    }
    keyboard::load(fs::read(&access::config(keyboard::CONFIG)).ok().map(|d| String::from_utf8_lossy(&d).into_owned()).as_deref());
    kprintln!("session: {} signed in", account.name);
    let wm = Wm::new(w, h, sysinfo);
    // The previous boot ended with a hang or a panic — say so with a window.
    crash::arm();
    wm
}

#[entry]
fn main() -> Status {
    serial::init();
    kprintln!("{} booting...", wm::OS_NAME);
    // The image base — to find a function by the address on the panic screen (llvm-symbolizer + kernel.pdb).
    if let Ok(img) = boot::open_protocol_exclusive::<uefi::proto::loaded_image::LoadedImage>(boot::image_handle()) {
        kprintln!("kernel image base {:#x}", img.info().0 as u64);
    }
    init_heap();

    let fb = setup_display();
    kprintln!("display: {}x{} stride {} bgr={}", fb.w, fb.h, fb.stride, fb.bgr);
    splash::show(fb);
    splash::mark("splash");
    let esp_apps = load_esp_apps();
    *fs::BOOT_PART.lock() = find_boot_partition();
    splash::mark("files from the boot drive");
    acpi::find_rsdp();

    // From this moment the firmware is gone: all the hardware is ours.
    let mmap = unsafe { boot::exit_boot_services(None) };
    arch::cli();
    kprintln!("exited boot services");
    // All the memory the computer has (without devices and what the board itself occupies).
    let not_ram = [MemoryType::RESERVED, MemoryType::MMIO, MemoryType::MMIO_PORT_SPACE, MemoryType::UNUSABLE, MemoryType::PAL_CODE];
    let ram: u64 = mmap.entries().filter(|d| !not_ram.contains(&d.ty)).map(|d| d.page_count * 4096).sum();
    let max_phys = mmap.entries().map(|d| d.phys_start + d.page_count * 4096).max().unwrap_or(0);
    // A free page below 1 MiB — for the startup code of the other cores (SIPI can only address that).
    let low_page = mmap
        .entries()
        .filter(|d| d.ty == MemoryType::CONVENTIONAL)
        .flat_map(|d| (0..d.page_count).map(move |i| d.phys_start + i * 4096))
        .find(|&p| (0x1000..0x9F000).contains(&p))
        .unwrap_or(0);
    paging::init(max_phys);
    acpi::init();
    // The frame buffer may lie above ordinary memory (on graphics cards — even above 4 GiB).
    paging::map_frame(fb.base as u64, (fb.stride * fb.h) as u64 * 4);
    fb.register_for_crash();
    gdt::init();

    task::init();
    interrupts::init();
    reclaim_boot_memory(&mmap);
    task::start_reaper();
    let timer_ok = interrupts::enable_and_check();
    kprintln!("interrupts: timer ok = {timer_ok}");
    if timer_ok {
        power::init();
        smp::init(low_page);
        splash::animate();
    }
    // Parsing fonts takes noticeable time: by now the splash is already animating.
    font::init();
    splash::mark("fonts");

    // PS/2 initialization reads the controller's answers itself — without the IRQ handler, otherwise it would intercept them.
    let pci_devices = pci::scan();
    power::attach_gpu(&pci_devices);
    let (w, h) = (fb.w, fb.h);
    let display_info = format!("Display: {w}x{h} via UEFI GOP");
    let mut screens = output::Screens::new(fb);
    output::detect(&pci_devices, &mut screens);
    splash::screens(screens.shown());
    splash::mark("screens");
    let desk = output::bounds();
    let mut input = sync::without_interrupts(|| Input::new(desk.w, desk.h));
    for d in &pci_devices {
        kprintln!("pci: {:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}.{:02x}", d.bus, d.slot, d.func, d.vendor, d.device, d.class, d.subclass);
    }
    // USB goes before the file system: on a real computer the boot USB stick serves as the disk.
    // The trial kernel watchdog — as early as possible: a hang can happen while starting devices too.
    watchdog::start(&pci_devices);
    // Realtek Wi-Fi before USB: the Bluetooth of the same card waits until the card is up.
    rtl8723be::start(&pci_devices);
    let usb_info = usb::init(&pci_devices, (desk.w, desk.h));
    splash::mark("usb");
    for disk in ahci::init(&pci_devices) {
        kprintln!("sata: {disk}");
    }
    let disk_info = mount_storage(esp_apps);
    fs::mount_sata_data();
    splash::mark("files");
    if let Some(layout) = output::saved_for_boot() {
        screens.apply(layout);
        splash::screens(screens.shown());
    }
    let home = output::primary();
    let (mut w, mut h) = (home.w, home.h);
    time::load();
    // What happened to the previous boot (hang, panic, watchdog) — before the network: the report is on disk.
    crash::check();
    let net_info = net::init(&pci_devices);
    time::start();
    metrics::init(&pci_devices);
    services::start();
    net::exporter::start();
    net::remote::start();
    net::smbd::start();
    net::discovery::start();
    net::smb::restore();
    let sound_info = audio::init(&pci_devices);
    gpu::start(&pci_devices);
    // Fast versions (SSE graphics, AES-NI, CRC): compare with the simple ones and benchmark (once, in the background).
    task::spawn("self-check", None, self_check);
    let brightness_info = backlight::init(&pci_devices);
    battery::start();
    touchpad::start(&pci_devices);
    wifi::start();
    // Without a disk files live until shutdown: an account is useless there.
    if !fs::has_disk() && users::list().is_empty() {
        users::create_live();
    }
    let sysinfo = vec![
        display_info,
        format!("CPU: {} cores", smp::cpu_count()),
        format!("RAM: {} usable by the system", apps::transfer::size_text(ram)),
        format!("Timer: PIT {} Hz ({})", interrupts::HZ, if timer_ok { "ok" } else { "polling" }),
        format!("Mouse: {}", if input.has_vmmouse() { "vmmouse (absolute)" } else { "PS/2" }),
        disk_info,
        net_info,
        sound_info,
        brightness_info,
        usb_info,
        "Programs: Lua 5.4 and native ELF, each in its own ring 3 process".into(),
        "Written in Rust, no_std.".into(),
    ];
    splash::mark("devices");
    let mut login = None;
    let mut desktop = match users::auto() {
        Some(account) => Some(enter(&account, w, h, sysinfo.clone())),
        None => {
            login = Some(Login::new(w, h));
            None
        }
    };
    let mut events = Vec::new();
    let mut ctx = Ctx { time: time::now(), ticks: 0, focused: true };
    let mut last_rtc = 0;
    let (mut mx, mut my) = (home.x + w / 2, home.y + h / 2);
    input.move_pointer(mx, my);
    let mut drawn_cursor = (wm::Cursor::Arrow, mx, my);
    let mut startup_sound = desktop.is_some();
    let mut frames = 0u64;
    let mut presented = u64::MAX;
    // FS autosave: half a second after the last change.
    let (mut fs_seen, mut fs_saved, mut fs_changed_at) = (fs::version(), fs::version(), 0);
    let mut usb_disks = u64::MAX;
    let mut port_checked = 0;
    let mut adapter_checked = 0;
    let mut screensaver: Option<saver::Saver> = None;
    // The display was turned off with a button: it stays off until the first input, not until a timer.
    let mut dark_until_input = false;
    let mut dark_asked_at = 0;
    let mut lid_was_shut = false;
    let mut last_mods = 0;
    let mut last_input = 0;

    // The welcome sound — once the first (heaviest) frame is drawn, otherwise
    // the sound thread cannot keep the buffers filled and the melody stutters.
    let mut welcome_at = u64::MAX;
    kprintln!("entering main loop");
    let mut frame_at = interrupts::ticks();
    // Where the previous loop iteration spent its time (cycles): input and events, programs,
    // other, drawing, output to the screen, waiting (sleep or other threads).
    let mut marks = [0u64; 7];
    let mut yields = 0u32;
    loop {
        ctx.ticks = if timer_ok { interrupts::ticks() } else { arch::rdtsc() / 2_000_000 };
        watchdog::alive();
        if desktop.is_some() {
            crash::poll();
        }
        // The desktop did not respond for a noticeable time: log it to find the cause.
        if timer_ok && ctx.ticks - frame_at >= interrupts::HZ / 2 {
            let part = |a: usize, b: usize| marks[b].saturating_sub(marks[a]) / 1_000_000;
            kprintln!(
                "input: desktop stalled for {} ms (Mcycles: input {}, apps {}, other {}, compose {}, present {}, waiting {} with {yields} yields)",
                (ctx.ticks - frame_at) * 1000 / interrupts::HZ,
                part(0, 1),
                part(1, 2),
                part(2, 3),
                part(3, 4),
                part(4, 5),
                part(5, 6)
            );
        }
        marks[0] = arch::rdtsc();
        yields = 0;
        frame_at = ctx.ticks;
        if ctx.ticks - last_rtc >= interrupts::HZ / 10 {
            ctx.time = time::now();
            last_rtc = ctx.ticks;
        }

        input.poll(&mut events);
        // Shift and Ctrl alone produce no events, but they reveal a person at the keyboard.
        let mods = input::mods();
        let active = events.iter().any(|e| saver::activity(e, (mx, my))) || mods != last_mods;
        last_mods = mods;
        if active {
            last_input = ctx.ticks;
        }
        // A turned-off display is woken by any movement, which does not reach programs. Without
        // a second screen a closed lid keeps it off, an open lid wakes it.
        let shut = output::lid_closed() && output::count() < 2;
        if lid_was_shut && !shut {
            last_input = ctx.ticks;
            dark_until_input = false;
        }
        lid_was_shut = shut;
        let idle_off = saver::display_timeout().is_some_and(|wait| timer_ok && ctx.ticks >= last_input + wait);
        let asked_off = saver::take_display_off();
        if asked_off {
            dark_until_input = true;
            dark_asked_at = ctx.ticks;
        }
        // The hand is still on the mouse after the press: that does not mean the display should wake.
        if active && ctx.ticks >= dark_asked_at + interrupts::HZ {
            dark_until_input = false;
        }
        let want_dark = shut || idle_off || dark_until_input;
        if want_dark != output::dark() {
            if want_dark {
                screens.darken();
            } else {
                screens.brighten();
                if let Some(wm) = &mut desktop {
                    wm.redraw_all();
                }
                if let Some(l) = &mut login {
                    l.redraw();
                }
                screensaver = None;
                for event in &events {
                    if let Event::Mouse { x, y, .. } = *event {
                        (mx, my) = (x, y);
                    }
                }
                drawn_cursor = (drawn_cursor.0, mx, my);
                events.clear();
            }
        }
        if output::dark() {
            events.clear();
        }
        // The screensaver takes all input: the press that dismisses it does not reach programs.
        if let Some(s) = &screensaver {
            if active && s.settled() {
                screensaver = None;
                if let Some(wm) = &mut desktop {
                    wm.redraw_all();
                }
                if let Some(l) = &mut login {
                    l.redraw();
                }
                for event in &events {
                    if let Event::Mouse { x, y, .. } = *event {
                        (mx, my) = (x, y);
                    }
                }
            }
            events.clear();
        }
        for event in events.drain(..) {
            match (event, &mut login, &mut desktop) {
                (Event::Key(ch), Some(l), _) => l.key(ch),
                (Event::Key(ch), None, Some(wm)) => wm.key(ch, &ctx),
                (Event::Text(ch), Some(l), _) => l.text(ch),
                (Event::Text(ch), None, Some(wm)) => wm.text(ch, &ctx),
                (Event::Mouse { x, y, buttons, .. }, Some(l), _) => {
                    (mx, my) = (x, y);
                    l.mouse(x, y, buttons);
                }
                (Event::Mouse { x, y, buttons, wheel }, None, Some(wm)) => {
                    (mx, my) = (x, y);
                    wm.mouse(x, y, buttons);
                    if wheel != 0 {
                        wm.wheel(x, y, wheel);
                    }
                }
                _ => {} // MouseMove: Input::poll turns these into Mouse
            }
        }
        if let Some(l) = &mut login {
            l.tick(&ctx);
            match l.take_outcome() {
                Some(Outcome::Enter(name)) => {
                    login = None;
                    if let Some(account) = users::find(&name) {
                        desktop = Some(enter(&account, w, h, sysinfo.clone()));
                        startup_sound = true;
                    }
                }
                Some(Outcome::Unlock) => {
                    login = None;
                    if let Some(wm) = &mut desktop {
                        wm.redraw_all();
                    }
                }
                None => {}
            }
        }
        marks[1] = arch::rdtsc();
        if let Some(wm) = &mut desktop {
            wm.tick(&ctx);
            marks[2] = arch::rdtsc();
            match wm.take_leave() {
                Some(Leave::Lock) => {
                    let power = !wm.has_unsaved();
                    login = access::user().map(|name| Login::locked(w, h, &name, power));
                }
                Some(Leave::LogOut) => {
                    desktop = None;
                    // System services keep running, user services are stopped by their supervisor.
                    task::kill_processes(&services::system_threads());
                    wm::forget_session();
                    access::sign_out();
                    login = Some(Login::new(w, h));
                }
                None => {}
            }
        }
        if core::mem::take(&mut startup_sound) && theme::get().startup_sound {
            welcome_at = 0;
        }
        if welcome_at == 0 {
            welcome_at = ctx.ticks + interrupts::HZ;
        } else if ctx.ticks >= welcome_at {
            welcome_at = u64::MAX;
            audio::welcome();
        }

        if fs::version() != fs_seen {
            fs_seen = fs::version();
            fs_changed_at = ctx.ticks;
        }
        // Save to disk in the background after 2 s of quiet (a series of changes — one write);
        // if the previous save is still running — try later.
        if fs_seen != fs_saved && ctx.ticks - fs_changed_at >= 2 * interrupts::HZ && fs::needs_saving() && fs::sync_in_background() {
            fs_saved = fs_seen;
        }
        keep::poll(ctx.ticks);
        // A USB Ethernet adapter was connected — hand it to the network stack (not from the USB thread: the stack reads settings from disk).
        if let Some(mac) = usb::net::take_new() {
            task::as_system(|| net::attach(alloc::boxed::Box::new(usb::net::UsbNic::new(mac)), "USB Ethernet"));
        }
        // Wi-Fi started working: to the network stack as well.
        if let Some(mac) = wifi::take_new_link() {
            task::as_system(|| net::attach(alloc::boxed::Box::new(wifi::WifiNic { mac }), "Wi-Fi"));
        }
        if ctx.ticks >= adapter_checked + interrupts::HZ / 2 {
            adapter_checked = ctx.ticks;
            task::as_system(net::pick_adapter);
        }
        // A USB stick was inserted or removed — refresh /media (in the background: reading the FAT
        // of a large stick takes time).
        if usb::disks_version() != usb_disks && task::as_system(fs::refresh_usb_in_background) {
            usb_disks = usb::disks_version();
        }

        // Screens were rearranged or connected: scene, cursor and desktop get the new sizes.
        let request = output::take_request();
        let check_port = ctx.ticks >= port_checked + interrupts::HZ / PORT_CHECKS_PER_SECOND;
        let before = if request.is_some() || check_port { output::placed() } else { Vec::new() };
        let moved = match request {
            Some(layout) => {
                screens.apply(layout);
                true
            }
            // A monitor without signal often releases the connector: that does not yet mean it was unplugged.
            None if check_port && !output::dark() => {
                port_checked = ctx.ticks;
                screens.watch()
            }
            None => false,
        };
        if moved {
            let home = output::primary();
            (w, h) = (home.w, home.h);
            (mx, my) = output::clamp(mx, my);
            input.move_pointer(mx, my);
            if let Some(wm) = &mut desktop {
                wm.relayout(w, h, &before);
            }
            if let Some(l) = &mut login {
                l.relayout(w, h);
            }
            drawn_cursor = (drawn_cursor.0, mx, my);
            if screensaver.take().is_some()
                && let Some(wm) = &mut desktop
            {
                wm.redraw_all();
            }
        }
        let idle = saver::timeout().is_some_and(|wait| timer_ok && ctx.ticks >= last_input + wait);
        if screensaver.is_none() && !output::dark() && (saver::take_request() || idle) {
            screensaver = Some(saver::Saver::new(screens.scenes(), ctx.ticks));
        }

        let t0 = arch::rdtsc();
        if marks[2] < marks[1] {
            marks[2] = marks[1];
        }
        marks[3] = t0;
        let damage = match (&mut login, &mut desktop) {
            _ if output::dark() => {
                if let Some(wm) = &mut desktop {
                    wm.skip_frame();
                }
                Vec::new()
            }
            _ if screensaver.is_some() => {
                if let Some(wm) = &mut desktop {
                    wm.skip_frame();
                }
                screensaver.as_mut().map_or_else(Vec::new, |s| s.render(screens.scenes(), &ctx))
            }
            (Some(l), wm) => {
                if let Some(wm) = wm {
                    wm.skip_frame();
                }
                l.render(screens.scenes(), &ctx)
            }
            (None, Some(wm)) => wm.render(screens.scenes(), &ctx),
            (None, None) => Vec::new(),
        };
        marks[4] = arch::rdtsc();
        if !damage.is_empty() {
            let t1 = arch::rdtsc();
            if frames == 0 {
                splash::finish();
                splash::mark("first frame");
            }
            for &r in &damage {
                screens.present(r);
            }
            frames += 1;
            let area: i32 = damage.iter().map(|r| r.w * r.h).sum();
            // Full redraws (more than 3 million pixels) — always: they are used to compare drawing methods.
            if screensaver.is_none() && (frames <= 3 || frames % 50 == 0 || area > 3_000_000) {
                kprintln!(
                    "frame {frames}: {} rects, {} kpx damage, compose {} Mcycles, present {} Mcycles",
                    damage.len(),
                    area / 1000,
                    (t1 - t0) / 1_000_000,
                    (arch::rdtsc() - t1) / 1_000_000
                );
                let c: alloc::vec::Vec<u64> = wm::COMPOSE_COST.iter().map(|a| a.swap(0, core::sync::atomic::Ordering::Relaxed)).collect();
                kprintln!(
                    "  since last: background {} M, shadows {} M, glass {} M ({} full blurs), windows {} M, panels {} M",
                    c[0] / 1_000_000,
                    c[1] / 1_000_000,
                    c[2] / 1_000_000,
                    c[5],
                    c[3] / 1_000_000,
                    c[4] / 1_000_000
                );
            }
        }
        let cursor = (desktop.as_ref().filter(|_| login.is_none()).map_or(wm::Cursor::Arrow, Wm::cursor), mx, my);
        if screensaver.is_some() || output::dark() {
            // The hardware cursor does not leave the screen by itself.
            screens.hide_cursor();
            presented = presented.wrapping_sub(1);
        } else if drawn_cursor != cursor || frames != presented {
            // Restore the piece of the scene under the old cursor and draw the new one.
            let (kind, x, y) = drawn_cursor;
            screens.present_small(output::cursor_area(kind, x, y));
            screens.draw_cursor(cursor.0, mx, my);
            drawn_cursor = cursor;
            presented = frames;
            // Writes to the frame accumulate in the CPU buffer: let them reach the screen.
            arch::sfence();
        }

        marks[5] = arch::rdtsc();
        if timer_ok {
            let tick = interrupts::ticks();
            loop {
                // Input or a ready window frame: the next iteration right away, without waiting for a tick.
                if task::take_kick() || interrupts::ticks() != tick {
                    break;
                }
                if task::others_ready() {
                    // The main thread has nothing to do — the CPU goes to background work until a tick or an event.
                    yields += 1;
                    task::park(tick + 1);
                } else {
                    let t = arch::rdtsc();
                    // The flag is checked with interrupts disabled: `sti; hlt` will not miss
                    // a wake-up that arrives between the check and the sleep.
                    sync::without_interrupts(|| {
                        if !task::kicked() {
                            power::idle();
                        }
                    });
                    stats::add_idle(arch::rdtsc() - t);
                }
                // The fast timer wakes us for the network thread, not for the whole loop.
                if !interrupts::fast() {
                    break;
                }
            }
        } else {
            core::hint::spin_loop();
        }
        marks[6] = arch::rdtsc();
    }
}
