//! Builds Lua 5.4 and a minimal libc with clang and links them only into /bin/lua:
//! the kernel and the other programs know nothing about C.

use std::env;
use std::path::PathBuf;
use std::process::Command;

const LUA_DIR: &str = "../third_party/lua-5.4.7/src";
const LUA_FILES: &[&str] = &[
    "lapi", "lcode", "lctype", "ldebug", "ldo", "ldump", "lfunc", "lgc", "llex", "lmem", "lobject", "lopcodes",
    "lparser", "lstate", "lstring", "ltable", "ltm", "lundump", "lvm", "lzio", "lauxlib", "lbaselib", "lcorolib",
    "lmathlib", "lstrlib", "ltablib", "lutf8lib",
];
const LIBC_FILES: &[&str] = &["libc", "printf", "math", "glue"];

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let clang = env::var("CLANG").unwrap_or_else(|_| "clang".into());
    let sources = LUA_FILES
        .iter()
        .map(|f| (PathBuf::from(format!("{LUA_DIR}/{f}.c")), false))
        .chain(LIBC_FILES.iter().map(|f| (PathBuf::from(format!("../libc/src/{f}.c")), true)));
    for (src, libc) in sources {
        let obj = out.join(src.file_stem().unwrap()).with_extension("o");
        let mut cmd = Command::new(&clang);
        cmd.args([
            "--target=x86_64-unknown-none-elf",
            "-ffreestanding",
            "-nostdlibinc",
            "-isystem",
            "../libc/include",
            "-I",
            LUA_DIR,
            "-O2",
            "-fPIE",
            "-fvisibility=hidden",
            "-fno-stack-protector",
            "-fno-math-errno",
            "-Wno-everything",
            "-c",
        ]);
        if libc {
            // Otherwise clang may "recognize" a strlen loop in our strlen and call itself.
            cmd.arg("-fno-builtin");
        }
        cmd.arg(&src).arg("-o").arg(&obj);
        let status = cmd.status().unwrap_or_else(|e| panic!("cannot run {clang}: {e} (install clang)"));
        assert!(status.success(), "failed to compile {}", src.display());
        println!("cargo:rustc-link-arg-bin=lua={}", obj.display());
    }
    println!("cargo:rerun-if-changed=../libc");
    println!("cargo:rerun-if-changed={LUA_DIR}");
    println!("cargo:rerun-if-env-changed=CLANG");
}
