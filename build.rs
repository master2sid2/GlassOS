//! The build stamp for the kernel: the git commit (with "+" if there are uncommitted changes) and the build
//! time. The crash report uses it to say which kernel hung and which one it rolled back to.

use std::process::Command;

fn main() {
    let git = |args: &[&str]| {
        Command::new("git").args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    let time = Command::new("date").args(["+%Y-%m-%d %H:%M"]).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    println!("cargo:rustc-env=GLASSOS_BUILD={commit}{} {time}", if dirty { "+" } else { "" });
    // Rebuild the stamp on every source change and on a new commit.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
