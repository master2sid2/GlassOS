//! System APIs for Lua programs: `fs` (files), `sys` (time, launching) and `net`.
//!
//! The functions do not throw Lua errors: on failure they return `nil, "message"`, as
//! is customary in the Lua standard library. Each operation is a system call
//! of the process, all values in it come from the program and are checked here.

use super::Args;
use super::wire::Value;
use crate::apps::dialog::{self, Mode};
use crate::fs;
use crate::interrupts::{self, HZ};
use crate::wm::{self, Request};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI64, Ordering};

const NO_PATH: &str = "bad path";

static NEXT_DIALOG: AtomicI64 = AtomicI64::new(1);
static NEXT_FETCH: AtomicI64 = AtomicI64::new(1);

fn fail(msg: &str) -> Vec<Value> {
    vec![Value::Nil, Value::text(msg)]
}

/// A path from argument `n`, relative to the home folder. `fs` decides what is allowed.
fn path_arg(a: &Args, n: usize) -> Option<String> {
    a.opt(n).map(|p| fs::resolve(&crate::access::home(), &p))
}

/// The common form: `true` or `nil, error`.
fn status(r: fs::Result<()>) -> Vec<Value> {
    match r {
        Ok(()) => vec![Value::Bool(true)],
        Err(e) => fail(&format!("{e}")),
    }
}

fn fetch_reply(r: Result<crate::net::http::Response, String>) -> Vec<Value> {
    match r {
        Ok(resp) => vec![Value::Bytes(resp.body), Value::Int(resp.status as i64)],
        Err(e) => fail(&e),
    }
}

fn two_paths(a: &Args) -> Option<(String, String)> {
    Some((path_arg(a, 1)?, path_arg(a, 2)?))
}

pub fn call(lib: &str, name: &str, a: &Args) -> Vec<Value> {
    match (lib, name) {
        ("fs", "read") => {
            let Some(path) = path_arg(a, 1) else { return fail(NO_PATH) };
            match fs::read(&path) {
                Ok(data) => vec![Value::Bytes(data)],
                Err(e) => fail(&format!("{path}: {e}")),
            }
        }
        ("fs", "write") => match (path_arg(a, 1), a.bytes(2)) {
            (Some(path), Some(data)) => status(fs::write(&path, data)),
            _ => fail("usage: fs.write(path, text)"),
        },
        ("fs", "append") => match (path_arg(a, 1), a.bytes(2)) {
            (Some(path), Some(data)) => status(fs::append(&path, data)),
            _ => fail("usage: fs.append(path, text)"),
        },
        ("fs", "_list") => {
            let path = path_arg(a, 1).unwrap_or_else(crate::access::home);
            match fs::list(&path) {
                Ok(entries) => vec![
                    Value::List(entries.iter().map(|e| e.name.clone()).collect()),
                    Value::List(entries.iter().map(|e| String::from(if e.is_dir { "1" } else { "0" })).collect()),
                    Value::List(entries.iter().map(|e| e.size.to_string()).collect()),
                ],
                Err(e) => fail(&format!("{path}: {e}")),
            }
        }
        ("fs", "_volumes") => {
            let list = fs::volumes();
            let flag = |on: bool| String::from(if on { "1" } else { "0" });
            vec![
                Value::List(list.iter().map(|v| v.name.clone()).collect()),
                Value::List(list.iter().map(|v| v.path.clone()).collect()),
                Value::List(list.iter().map(|v| v.kind.into()).collect()),
                Value::List(list.iter().map(|v| v.used.to_string()).collect()),
                Value::List(list.iter().map(|v| v.total.to_string()).collect()),
                Value::List(list.iter().map(|v| flag(v.in_memory)).collect()),
                Value::List(list.iter().map(|v| flag(v.removable)).collect()),
                Value::List(list.iter().map(|v| flag(v.network)).collect()),
            ]
        }
        // File playback: the player number, controls, state.
        ("audio", "open") => {
            let Some(path) = path_arg(a, 1) else { return fail(NO_PATH) };
            let t0 = interrupts::ticks();
            let read = fs::read(&path);
            let t1 = interrupts::ticks();
            match read {
                Ok(data) => match crate::audio::player::open(data, Some(crate::task::current_id())) {
                    Ok(id) => {
                        let ms = 1000 / crate::interrupts::HZ;
                        crate::kprintln!("player {id}: read in {} ms, opened in {} ms", (t1 - t0) * ms, (interrupts::ticks() - t1) * ms);
                        vec![Value::Int(id as i64)]
                    }
                    Err(e) => fail(&format!("{path}: {e}")),
                },
                Err(e) => fail(&format!("{path}: {e}")),
            }
        }
        ("audio", "play") => vec![Value::Bool(crate::audio::player::play(a.int(1, 0) as u32))],
        ("audio", "pause") => vec![Value::Bool(crate::audio::player::pause(a.int(1, 0) as u32))],
        ("audio", "stop") => vec![Value::Bool(crate::audio::player::stop(a.int(1, 0) as u32))],
        ("audio", "seek") => vec![Value::Bool(crate::audio::player::seek(a.int(1, 0) as u32, a.int(2, 0).max(0) as u64))],
        ("audio", "volume") => vec![Value::Bool(crate::audio::player::set_volume(a.int(1, 0) as u32, a.int(2, 100).clamp(0, 100) as u32))],
        ("audio", "close") => {
            crate::audio::player::close(a.int(1, 0) as u32);
            vec![Value::Bool(true)]
        }
        ("audio", "_status") => match crate::audio::player::status(a.int(1, 0) as u32) {
            Some((state, pos, len)) => vec![Value::text(state.name()), Value::Int(pos as i64), Value::Int(len as i64)],
            None => fail("no such player"),
        },
        ("audio", "balance") => vec![Value::Bool(crate::audio::player::set_balance(a.int(1, 0) as u32, a.int(2, 0).clamp(-100, 100) as i32))],
        ("audio", "_spectrum") => match crate::audio::player::spectrum(a.int(1, 0) as u32, a.int(2, 16).clamp(1, 64) as usize) {
            Some(levels) => vec![Value::Bytes(levels)],
            None => vec![Value::Nil],
        },
        ("audio", "_probe") => {
            let Some(path) = path_arg(a, 1) else { return fail(NO_PATH) };
            match crate::audio::player::probe(&path).map_err(|e| format!("{path}: {e}")) {
                Ok((ms, title, artist, album)) => vec![Value::Int(ms as i64), Value::text(&title), Value::text(&artist), Value::text(&album)],
                Err(e) => fail(&e),
            }
        }
        ("audio", "cover") => match crate::audio::player::cover(a.int(1, 0) as u32) {
            Some(data) => vec![Value::Bytes(data.to_vec())],
            None => vec![Value::Nil],
        },
        ("audio", "_info") => match crate::audio::player::info(a.int(1, 0) as u32) {
            Some((format, title, artist, rate, channels, album)) => vec![Value::text(&format), Value::text(&title), Value::text(&artist), Value::Int(rate as i64), Value::Int(channels as i64), Value::text(&album)],
            None => fail("no such player"),
        },
        ("fs", "exists") => vec![Value::Bool(path_arg(a, 1).is_some_and(|p| fs::exists(&p)))],
        ("fs", "isdir") => vec![Value::Bool(path_arg(a, 1).is_some_and(|p| fs::is_dir(&p)))],
        ("fs", "mkdir") => path_arg(a, 1).map_or_else(|| fail(NO_PATH), |p| status(fs::mkdir(&p))),
        ("fs", "remove") => path_arg(a, 1).map_or_else(|| fail(NO_PATH), |p| status(fs::remove(&p))),
        ("fs", "rename") => two_paths(a).map_or_else(|| fail(NO_PATH), |(from, to)| status(fs::rename(&from, &to))),
        ("fs", "copy") => two_paths(a).map_or_else(|| fail(NO_PATH), |(from, to)| status(fs::copy(&from, &to))),
        ("sys", "_time") => {
            let utc = crate::time::unix();
            let t = crate::time::local(utc);
            let fields = [t.hour as i64, t.min as i64, t.sec as i64, t.day as i64, t.month as i64, t.year as i64, crate::time::weekday(&t) as i64 + 1, utc];
            let mut out: Vec<Value> = fields.map(Value::Int).to_vec();
            out.push(Value::Int(crate::time::offset_at(utc) as i64));
            out.push(Value::Bool(crate::time::summer_now()));
            out.push(Value::text(&crate::time::zone().name));
            out
        }
        ("sys", "ticks") => vec![Value::Int((interrupts::ticks() * 1000 / HZ) as i64)],
        ("sys", "mem") => {
            let (used, free) = crate::heap_stats();
            vec![Value::Int((used / 1024) as i64), Value::Int((free / 1024) as i64)]
        }
        ("sys", "user") => {
            let name = crate::access::user().unwrap_or_default();
            vec![Value::text(&name), Value::text(&crate::access::home()), Value::Bool(crate::access::is_admin())]
        }
        ("sys", "version") => vec![Value::text("0.2")],
        ("sys", "notify") => {
            wm::notify(a.opt(2).as_deref().unwrap_or("Notification"), &a.text(1));
            Vec::new()
        }
        ("sys", "open") => {
            let Some(path) = path_arg(a, 1) else { return fail(NO_PATH) };
            if !fs::exists(&path) {
                return fail(&format!("{path}: no such file or directory"));
            }
            wm::request(if fs::is_dir(&path) {
                Request::OpenFolder(path)
            } else if super::is_program(&path) {
                Request::Run(path)
            } else {
                Request::OpenFile(path)
            });
            vec![Value::Bool(true)]
        }
        ("sys", "_dialog") => {
            let mode = match a.text(1).as_str() {
                "save" => Mode::Save,
                "open_many" => Mode::OpenMany,
                _ => Mode::Open,
            };
            let id = NEXT_DIALOG.fetch_add(1, Ordering::Relaxed);
            // The answer comes to the window loop: without a window there is nobody to ask.
            if super::ui::has_window(crate::task::current_id()) {
                super::ui::watch_dialog(id, dialog::request(mode, &a.text(2), &a.text(3)));
            }
            vec![Value::Int(id)]
        }
        ("sys", "beep") => {
            crate::audio::beep(a.int(1, 880).clamp(20, 20_000) as u32, a.int(2, 120).clamp(1, 10_000) as u32);
            Vec::new()
        }
        ("sys", "play") => {
            let Some(path) = path_arg(a, 1) else { return fail(NO_PATH) };
            let r = fs::read(&path).map_err(|e| format!("{path}: {e}")).and_then(|d| crate::audio::play_wav(&d).map_err(|e| e.into()));
            match r {
                Ok(()) => vec![Value::Bool(true)],
                Err(e) => fail(&e),
            }
        }
        ("sys", "sleep") => {
            // Sleep in pieces: a request to stop (Ctrl+C, Stop) is noticed quickly.
            let ms = a.int(1, 0).clamp(0, 86_400_000) as u64;
            let end = interrupts::ticks() + (ms * HZ / 1000).max(1);
            while interrupts::ticks() < end && !crate::task::should_stop() {
                crate::task::sleep_until(end.min(interrupts::ticks() + HZ / 10));
            }
            Vec::new()
        }
        ("sys", "_readline") => super::read_line().map_or_else(Vec::new, |l| vec![Value::text(&l)]),
        ("net", "_get") => fetch_reply(crate::net::http::get(&a.text(1))),
        ("net", "_start") => {
            let url = a.text(1);
            let id = NEXT_FETCH.fetch_add(1, Ordering::Relaxed);
            if let Some(deliver) = super::ui::fetch_target() {
                crate::task::spawn("fetch", None, move || deliver(id, crate::net::http::get(&url)));
            }
            vec![Value::Int(id)]
        }
        ("net", "hostname") => {
            let id = crate::net::identity::get();
            vec![Value::text(&id.name), Value::text(&id.workgroup)]
        }
        _ => Vec::new(),
    }
}
