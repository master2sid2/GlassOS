//! A memory protection test: each of these attempts kills only the process itself.
//!   crash null   — a read at address zero
//!   crash kernel — a read of kernel memory
//!   crash code   — a write into its own code (it is read-only)
//!   crash cli    — a privileged instruction
//!   crash spin   — an endless loop (stop it with Ctrl+C)

#![no_std]
#![no_main]

use glass::println;

glass::entry!(main);

fn main(args: &[&str]) -> i32 {
    let mode = args.get(1).copied().unwrap_or("null");
    println!("crash: trying '{mode}'...");
    unsafe {
        match mode {
            "null" => println!("{}", core::ptr::read_volatile(core::ptr::null::<u64>())),
            // The kernel is mapped 1:1 in the lower gigabytes — for example, where the kernel image lies.
            "kernel" => println!("{}", core::ptr::read_volatile(0x10_0000 as *const u64)),
            "code" => core::ptr::write_volatile(main as *const () as *mut u8, 0xC3),
            "cli" => core::arch::asm!("cli"),
            "spin" => {
                let mut x = 0u64;
                loop {
                    x = core::hint::black_box(x + 1);
                }
            }
            _ => {
                println!("usage: crash null|kernel|code|cli|spin");
                return 2;
            }
        }
    }
    println!("crash: survived?!");
    1
}
