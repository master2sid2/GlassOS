//! Who may do what on disk. `fs` checks the rules itself, so they cannot be bypassed from
//! Files, the terminal or a program.
//!
//! Every thread has rights (`task::cred`). Kernel services run without them and may do
//! anything. The desktop runs on behalf of the logged-in user, threads inherit
//! the rights of whoever started them. A program gets the user's rights narrowed down to
//! their home folder, its own folder and whatever the user chose for it.

use crate::fs;
use crate::sync::IrqLock;
use crate::task;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

/// Password files: only the kernel reads and writes them.
const SECRETS: [&str; 5] = [crate::net::smb::CONNECTIONS, crate::net::smbd::CONFIG, crate::users::FILE, crate::wifi::CONFIG, crate::net::remote::KEY];
/// The settings folder inside the home folder.
const CONFIG_DIR: &str = ".config";

pub struct Cred {
    pub user: String,
    pub home: String,
    pub admin: bool,
    /// The program's rights; None — the desktop itself.
    program: Option<Program>,
}

struct Program {
    /// The folder the program was started from.
    dir: String,
    /// What the user opened for it: the start arguments and the choice in a dialog.
    granted: IrqLock<Vec<String>>,
}

/// The user logged into the system.
static SESSION: IrqLock<Option<Arc<Cred>>> = IrqLock::new(None);

/// Starts a session: the main thread gets the user's rights.
pub fn sign_in(user: &str, admin: bool) {
    let cred = Arc::new(Cred { user: user.into(), home: home_of(user), admin, program: None });
    *SESSION.lock() = Some(cred.clone());
    task::set_cred(Some(cred));
}

/// Who is logged in now.
pub fn session_user() -> Option<String> {
    SESSION.lock().as_ref().map(|c| c.user.clone())
}

pub fn sign_out() {
    *SESSION.lock() = None;
    task::set_cred(None);
}

pub fn home_of(user: &str) -> String {
    format!("/home/{user}")
}

/// The rights of a remote session: like user `user` logged into the system.
pub fn for_remote(user: &str, admin: bool) -> Arc<Cred> {
    Arc::new(Cred { user: user.into(), home: home_of(user), admin, program: None })
}

/// Rights for program `path`: its path arguments are the user's choice. As with files
/// handed to an already open window, the file's folder is granted too (the player takes
/// neighbouring songs into the playlist).
pub fn for_program(path: &str, args: &[&str]) -> Option<Arc<Cred>> {
    let session = SESSION.lock().clone()?;
    let mut granted = Vec::new();
    for a in args.iter().filter(|a| a.starts_with('/')) {
        granted.push(normal(a));
        if *a != path && !fs::is_dir(a) {
            granted.push(normal(&fs::parent(a)));
        }
    }
    Some(Arc::new(Cred {
        user: session.user.clone(),
        home: session.home.clone(),
        admin: session.admin,
        program: Some(Program { dir: normal(&fs::parent(path)), granted: IrqLock::new(granted) }),
    }))
}

/// Rights for a service: like program `path` started by user `user`,
/// even if nobody is logged in now.
pub fn for_service(user: &str, admin: bool, path: &str) -> Arc<Cred> {
    Arc::new(Cred {
        user: user.into(),
        home: home_of(user),
        admin,
        program: Some(Program { dir: normal(&fs::parent(path)), granted: IrqLock::new(Vec::new()) }),
    })
}

/// The current program gets access to a file the user chose.
pub fn grant(path: &str) {
    if let Some(p) = task::cred().as_ref().and_then(|c| c.program.as_ref()) {
        p.granted.lock().push(normal(path));
    }
}

/// The home folder of whoever asks. Without a user — the shared homes folder.
pub fn home() -> String {
    task::cred().map_or_else(|| "/home".into(), |c| c.home.clone())
}

pub fn desktop() -> String {
    format!("{}/Desktop", home())
}

/// The user's settings file; the folder is created on first access.
pub fn config(name: &str) -> String {
    let dir = format!("{}/{CONFIG_DIR}", home());
    if !fs::is_dir(&dir) {
        let _ = fs::mkdir(&dir);
    }
    format!("{dir}/{name}")
}

pub fn user() -> Option<String> {
    task::cred().map(|c| c.user.clone())
}

pub fn is_admin() -> bool {
    task::cred().is_none_or(|c| c.admin)
}

/// Names on FAT are case-insensitive, so they are compared in one case.
fn normal(path: &str) -> String {
    fs::resolve("/", path).to_ascii_lowercase()
}

fn within(path: &str, dir: &str) -> bool {
    dir == "/" || path == dir || path.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'))
}

pub fn is_secret(path: &str) -> bool {
    SECRETS.contains(&normal(path).as_str())
}

/// A password file or the folder it is in: they cannot be moved or deleted over the network.
pub fn holds_secret(path: &str) -> bool {
    let path = normal(path);
    SECRETS.iter().any(|s| within(s, &path))
}

/// What this user's desktop is allowed.
fn desktop_may(c: &Cred, path: &str, write: bool) -> bool {
    if within(path, &c.home.to_ascii_lowercase()) || within(path, "/tmp") || within(path, fs::MEDIA) || within(path, fs::NETWORK) {
        return true;
    }
    // Someone else's home is closed to everyone, the homes folder itself is only a list of names.
    if within(path, "/home") {
        return path == "/home" && !write;
    }
    !write || c.admin
}

fn allowed(c: &Cred, path: &str, write: bool) -> bool {
    let path = normal(path);
    if SECRETS.contains(&path.as_str()) || !desktop_may(c, &path, write) {
        return false;
    }
    let Some(p) = &c.program else { return true };
    // The settings hold the list of what opens at login: otherwise a program could
    // start itself.
    let home = c.home.to_ascii_lowercase();
    if within(&path, &format!("{home}/{CONFIG_DIR}")) {
        return false;
    }
    if within(&path, &home) || within(&path, "/tmp") {
        return true;
    }
    if p.granted.lock().iter().any(|g| within(&path, g)) {
        return true;
    }
    let readable = path == "/" || path == "/home" || within(&path, &p.dir) || within(&path, "/apps") || within(&path, "/bin") || within(&path, fs::NETWORK);
    !write && readable
}

pub fn may_read(path: &str) -> bool {
    task::cred().is_none_or(|c| allowed(&c, path, false))
}

pub fn may_write(path: &str) -> bool {
    task::cred().is_none_or(|c| allowed(&c, path, true))
}

/// Whether the path itself can be removed: the home folder and the settings cannot be deleted or moved.
pub fn may_remove(path: &str) -> bool {
    task::cred().is_none_or(|c| {
        let (path, home) = (normal(path), c.home.to_ascii_lowercase());
        allowed(&c, &path, true) && path != home && path != "/tmp" && path != format!("{home}/{CONFIG_DIR}")
    })
}
