//! Counts to N once a second: the sleep system call does not spend CPU.

#![no_std]
#![no_main]

use glass::println;

glass::entry!(main);

fn main(args: &[&str]) -> i32 {
    let n: u32 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(5);
    let start = glass::ticks();
    for i in 1..=n {
        glass::sleep(1000);
        println!("{i}  ({} ms)", glass::ticks() - start);
    }
    0
}
