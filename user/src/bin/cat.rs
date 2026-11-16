//! Prints files (without arguments — the home folder listing).

#![no_std]
#![no_main]

use glass::{print, println};

glass::entry!(main);

fn main(args: &[&str]) -> i32 {
    if args.len() < 2 {
        for name in glass::list_dir(".").unwrap_or_default() {
            println!("{name}");
        }
        return 0;
    }
    let mut status = 0;
    for path in &args[1..] {
        match glass::read_file(path) {
            Some(data) => print!("{}", core::str::from_utf8(&data).unwrap_or("(binary file)")),
            None => {
                println!("cat: {path}: no such file");
                status = 1;
            }
        }
    }
    status
}
