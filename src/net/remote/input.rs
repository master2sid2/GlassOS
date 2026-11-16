//! Debugging (the `debug-tools` build flag, the `debug input` switch): keyboard and mouse from
//! another computer through remote access — to test windows without a person at the screen.
//! The events go into the same queue as those of a USB keyboard and mouse.

use super::Response;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Input from another computer — into the same queue as a USB keyboard and mouse. Lines:
/// "text …" (type), "key NAME", "click X Y", "rclick X Y", "move X Y", "wheel X Y N"
/// (coordinates are screen pixels, as on the /shot screenshot). What is typed is not written to the log:
/// it may be a password.
pub(super) fn handle(body: &[u8]) -> Response {
    use crate::input::{self, Event};
    if !crate::debug::enabled(crate::debug::INPUT) {
        return Response::text(403, "remote keyboard and mouse are off: as administrator run `debug input on`, or Settings, Remote Access");
    }
    let text = String::from_utf8_lossy(body);
    let mut events = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        let (cmd, rest) = line.split_once(' ').unwrap_or((line.trim_end(), ""));
        match cmd {
            "" => {}
            "text" => {
                for ch in rest.chars() {
                    events.push(if ch.is_ascii() && !ch.is_ascii_control() { Event::Key(ch as u8) } else { Event::Text(ch) });
                }
            }
            "key" => match key_named(rest.trim()) {
                Some(k) => events.push(Event::Key(k)),
                None => {
                    return Response::text(400, format!("unknown key {:?}: enter, esc, tab, backspace, space, up, down, left, right, home, end, delete, pgup, pgdn, print, or one character", rest.trim()));
                }
            },
            "click" | "rclick" | "move" | "wheel" => {
                let n: Vec<i32> = rest.split_whitespace().filter_map(|v| v.parse().ok()).collect();
                let (Some(&x), Some(&y)) = (n.first(), n.get(1)) else {
                    return Response::text(400, format!("{cmd} needs X Y"));
                };
                let at = |buttons, wheel| Event::Mouse { x, y, buttons, wheel };
                match cmd {
                    "move" => events.push(at(0, 0)),
                    "wheel" => events.push(at(0, n.get(2).copied().unwrap_or(1))),
                    _ => {
                        let button = if cmd == "rclick" { input::RIGHT } else { input::LEFT };
                        events.extend([at(0, 0), at(button, 0), at(0, 0)]);
                    }
                }
            }
            other => return Response::text(400, format!("unknown input {other:?}: text, key, click, rclick, move, wheel")),
        }
    }
    let n = events.len();
    input::push(events);
    crate::kprintln!("remote: {n} input events");
    Response::text(200, format!("ok, {n} events"))
}

/// A key by name — the same codes a USB keyboard produces.
fn key_named(name: &str) -> Option<u8> {
    use crate::input::*;
    Some(match name {
        "enter" | "return" => b'\n',
        "esc" | "escape" => 0x1B,
        "tab" => b'\t',
        "backspace" | "bs" => 0x08,
        "space" => b' ',
        "up" => KEY_UP,
        "down" => KEY_DOWN,
        "left" => KEY_LEFT,
        "right" => KEY_RIGHT,
        "home" => KEY_HOME,
        "end" => KEY_END,
        "delete" | "del" => KEY_DELETE,
        "pgup" | "pageup" => KEY_PAGE_UP,
        "pgdn" | "pagedown" => KEY_PAGE_DOWN,
        "print" => KEY_PRINT,
        s if s.len() == 1 && s.is_ascii() => s.as_bytes()[0],
        _ => return None,
    })
}
