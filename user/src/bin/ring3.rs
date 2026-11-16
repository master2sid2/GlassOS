//! The first ring 3 program: shows who it is and where it runs.

#![no_std]
#![no_main]

use glass::println;

glass::entry!(main);

fn main(args: &[&str]) -> i32 {
    println!("Hello from user space!");
    println!("pid {}, running in ring {}", glass::pid(), glass::privilege_level());
    println!("args: {:?}", args);
    0
}
