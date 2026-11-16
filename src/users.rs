//! Accounts. The /system/users.cfg file is closed to everyone except the kernel: it holds lines
//! `user=name|admin|iterations|salt|hash`, plus `auto=name` (login without the login screen)
//! and `last=name` (whom to offer at the next login). The password is stored as
//! PBKDF2-HMAC-SHA256; a record without a password has an empty salt and hash.

use crate::access;
use crate::crypto;
use crate::fs;
use crate::task;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub const FILE: &str = "/system/users.cfg";
/// This many iterations take a fraction of a second, so the check runs in a background thread.
const ITERATIONS: u32 = 60_000;
pub const MAX_NAME: usize = 20;
/// The user name of old versions and its home folder.
const OLD_HOME: &str = "/home/user";
/// Settings that used to be shared.
const OLD_CONFIG: [(&str, &str); 5] = [
    ("/system/session.cfg", "session.cfg"),
    ("/system/glass.cfg", "glass.cfg"),
    ("/system/settings.cfg", "settings.cfg"),
    ("/system/files.cfg", "files.cfg"),
    ("/system/terminal_history", "terminal_history"),
];

#[derive(Clone)]
pub struct Account {
    pub name: String,
    pub admin: bool,
    iterations: u32,
    salt: String,
    hash: String,
}

impl Account {
    pub fn has_password(&self) -> bool {
        !self.hash.is_empty()
    }
}

#[derive(Default)]
struct Book {
    accounts: Vec<Account>,
    auto: String,
    last: String,
}

fn load() -> Book {
    let text = task::as_system(|| fs::read(FILE)).map(|d| String::from_utf8_lossy(&d).into_owned()).unwrap_or_default();
    let mut book = Book::default();
    for line in text.lines() {
        match line.split_once('=') {
            Some(("auto", v)) => book.auto = v.into(),
            Some(("last", v)) => book.last = v.into(),
            Some(("user", v)) => {
                let f: Vec<&str> = v.split('|').collect();
                if let [name, admin, iterations, salt, hash] = f[..] {
                    book.accounts.push(Account { name: name.into(), admin: admin == "1", iterations: iterations.parse().unwrap_or(ITERATIONS), salt: salt.into(), hash: hash.into() });
                }
            }
            _ => {}
        }
    }
    book
}

fn save(book: &Book) -> bool {
    let mut text = format!("auto={}\nlast={}\n", book.auto, book.last);
    for a in &book.accounts {
        text.push_str(&format!("user={}|{}|{}|{}|{}\n", a.name, a.admin as u8, a.iterations, a.salt, a.hash));
    }
    task::as_system(|| {
        let _ = fs::mkdir("/system");
        fs::write(FILE, text.as_bytes()).is_ok()
    })
}

pub fn list() -> Vec<Account> {
    load().accounts
}

pub fn find(name: &str) -> Option<Account> {
    load().accounts.into_iter().find(|a| a.name == name)
}

/// Whom to let in without the login screen.
pub fn auto() -> Option<Account> {
    let book = load();
    book.accounts.iter().find(|a| a.name == book.auto).cloned()
}

pub fn set_auto(name: Option<&str>) -> bool {
    let mut book = load();
    book.auto = name.unwrap_or("").into();
    save(&book)
}

pub fn last() -> String {
    load().last
}

pub fn set_last(name: &str) {
    let mut book = load();
    if book.last != name {
        book.last = name.into();
        save(&book);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hash(password: &str, salt: &str, iterations: u32) -> String {
    hex(&crypto::pbkdf2_sha256(password.as_bytes(), salt.as_bytes(), iterations))
}

/// A new salt and password hash; without a password both are empty.
fn seal(password: &str) -> (String, String) {
    if password.is_empty() {
        return (String::new(), String::new());
    }
    let salt = hex(&crypto::random32());
    let h = hash(password, &salt, ITERATIONS);
    (salt, h)
}

/// Whether the password matches. Takes long: do not call from the main thread.
pub fn verify(name: &str, password: &str) -> bool {
    let Some(a) = find(name) else { return false };
    if !a.has_password() {
        return password.is_empty();
    }
    let got = hash(password, &a.salt, a.iterations);
    // No early exit: the comparison time must not depend on the matching prefix.
    got.len() == a.hash.len() && got.bytes().zip(a.hash.bytes()).fold(0, |d, (x, y)| d | (x ^ y)) == 0
}

/// The name becomes a folder name, so only lowercase Latin letters, digits, `-`, `_`.
pub fn check_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("Enter a name");
    }
    if !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
        return Err("Use lowercase letters, digits, - and _");
    }
    if !name.as_bytes()[0].is_ascii_lowercase() {
        return Err("The name must start with a letter");
    }
    Ok(())
}

/// The record's home folder: a new or a missing one. Files already there are left alone.
pub fn make_home(name: &str) {
    let home = access::home_of(name);
    let _ = fs::mkdir("/home");
    let _ = fs::mkdir(&home);
    for dir in ["Desktop", "Documents", "Pictures", ".config"] {
        let _ = fs::mkdir(&format!("{home}/{dir}"));
    }
    let readme = format!("{home}/readme.txt");
    if !fs::exists(&readme) {
        let _ = fs::write(&readme, fs::README.as_bytes());
        let _ = fs::write(&format!("{home}/Desktop/readme.txt"), fs::README.as_bytes());
        let _ = fs::copy("/apps/widgets.lua", &format!("{home}/Desktop/widgets.lua"));
    }
}

/// Replaces the old home folder at the start of paths inside the settings text.
fn repath(text: &str, home: &str) -> String {
    text.lines().map(|l| format!("{}\n", l.replace(OLD_HOME, home))).collect()
}

/// The first record on a disk from an old version: the /home/user folder and the shared settings
/// become its home, settings and shares.
fn adopt(name: &str) {
    let home = access::home_of(name);
    if name != "user" && fs::is_dir(OLD_HOME) && !fs::exists(&home) {
        let _ = fs::rename(OLD_HOME, &home);
    }
    let _ = fs::mkdir(&format!("{home}/.config"));
    for (old, new) in OLD_CONFIG {
        if let Ok(data) = fs::read(old) {
            let text = repath(&String::from_utf8_lossy(&data), &home);
            if fs::write(&format!("{home}/.config/{new}"), text.as_bytes()).is_ok() {
                let _ = fs::remove(old);
            }
        }
    }
    if let Ok(data) = fs::read(crate::net::smbd::CONFIG) {
        let _ = fs::write(crate::net::smbd::CONFIG, repath(&String::from_utf8_lossy(&data), &home).as_bytes());
    }
}

/// Creates a record. The first record is always an administrator and takes over the old version's files.
pub fn create(name: &str, password: &str, admin: bool) -> Result<(), &'static str> {
    check_name(name)?;
    let mut book = load();
    let first = book.accounts.is_empty();
    if !first && !access::is_admin() {
        return Err("Only an administrator can add users");
    }
    if book.accounts.iter().any(|a| a.name == name) {
        return Err("This name is taken");
    }
    let (salt, hash) = seal(password);
    book.accounts.push(Account { name: name.into(), admin: admin || first, iterations: ITERATIONS, salt, hash });
    if !save(&book) {
        return Err("Could not write to disk");
    }
    task::as_system(|| {
        if first {
            adopt(name);
        }
        make_home(name);
    });
    Ok(())
}

/// Starting without a disk: the files live only in memory anyway, there is no point asking.
pub fn create_live() {
    if create("user", "", true).is_ok() {
        set_auto(Some("user"));
    }
}

/// A new password. Everyone changes their own (the window checks the old password), someone else's — an administrator.
pub fn set_password(name: &str, password: &str) -> Result<(), &'static str> {
    if !access::is_admin() && access::user().as_deref() != Some(name) {
        return Err("Only an administrator can do this");
    }
    let mut book = load();
    let a = book.accounts.iter_mut().find(|a| a.name == name).ok_or("No such user")?;
    (a.salt, a.hash) = seal(password);
    a.iterations = ITERATIONS;
    if save(&book) { Ok(()) } else { Err("Could not write to disk") }
}

/// Deletes a record together with its home folder.
pub fn remove(name: &str) -> Result<(), &'static str> {
    if !access::is_admin() {
        return Err("Only an administrator can do this");
    }
    if access::user().as_deref() == Some(name) {
        return Err("You cannot remove yourself");
    }
    let mut book = load();
    let was = book.accounts.len();
    book.accounts.retain(|a| a.name != name);
    if book.accounts.len() == was {
        return Err("No such user");
    }
    if !book.accounts.iter().any(|a| a.admin) {
        return Err("The last administrator cannot be removed");
    }
    if book.auto == name {
        book.auto.clear();
    }
    if book.last == name {
        book.last.clear();
    }
    if !save(&book) {
        return Err("Could not write to disk");
    }
    let _ = task::as_system(|| fs::remove(&access::home_of(name)));
    Ok(())
}

/// The settings file of whoever logged in last: the login screen looks and types as for them.
pub fn greeting_file(file: &str) -> Option<String> {
    let last = last();
    let name = if last.is_empty() { list().first()?.name.to_string() } else { last };
    settings_file(&name, file)
}

/// The user's settings file: for the login screen, while nobody is logged in.
pub fn settings_file(name: &str, file: &str) -> Option<String> {
    let data = task::as_system(|| fs::read(&format!("{}/.config/{file}", access::home_of(name)))).ok()?;
    Some(String::from_utf8_lossy(&data).into_owned())
}
