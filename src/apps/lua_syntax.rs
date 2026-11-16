//! Lua highlighting in Notepad: a line is split into coloured pieces. Long strings and
//! `[[ ]]` comments span several lines, so the parser keeps state.

use crate::gfx::Color;
use crate::wm::TEXT;
use alloc::vec::Vec;

const KEYWORD: Color = 0xC792EA;
const CONSTANT: Color = 0xF78C6C;
const BUILTIN: Color = 0x7DD3FC;
const STRING: Color = 0xA5D6A7;
const NUMBER: Color = 0xF9C97C;
const COMMENT: Color = 0x6B7490;

const KEYWORDS: [&str; 19] = ["and", "break", "do", "else", "elseif", "end", "for", "function", "goto", "if", "in", "local", "not", "or", "repeat", "return", "then", "until", "while"];
const CONSTANTS: [&str; 3] = ["false", "nil", "true"];
/// GlassOS libraries and the most common language functions.
const BUILTINS: [&str; 30] = [
    "assert", "coroutine", "error", "fs", "getmetatable", "gfx", "io", "ipairs", "load", "math", "net", "next", "os", "pairs", "pcall", "print", "rawget", "rawset", "require", "select", "setmetatable", "string", "sys", "table", "tonumber", "tostring", "type", "ui", "utf8", "xpcall",
];

/// An unclosed construct from previous lines: the level is the number of `=` in the brackets.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Open {
    Nothing,
    Comment(usize),
    Text(usize),
}

/// The level of a long bracket at the start of `s`: `[[` — 0, `[==[` — 2.
fn long_bracket(s: &[u8]) -> Option<usize> {
    let level = s.get(1..)?.iter().take_while(|&&b| b == b'=').count();
    (s.first() == Some(&b'[') && s.get(level + 1) == Some(&b'[')).then_some(level)
}

/// The end of a long construct of level `level` in `s`: the position after the closing bracket.
fn long_end(s: &[u8], level: usize) -> Option<usize> {
    let close = level + 2;
    (0..s.len().saturating_sub(close - 1)).find(|&i| s[i] == b']' && s[i + close - 1] == b']' && s[i + 1..i + close - 1].iter().all(|&b| b == b'=')).map(|i| i + close)
}

/// The pieces of a line (start, end, colour) in a row, boundaries at character boundaries:
/// everything special starts with ASCII, and the rest accumulates into a piece of plain text.
pub fn spans(line: &str, open: &mut Open) -> Vec<(usize, usize, Color)> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut plain = 0;
    let continued = match *open {
        Open::Comment(level) => Some((level, COMMENT, Open::Comment(level))),
        Open::Text(level) => Some((level, STRING, Open::Text(level))),
        Open::Nothing => None,
    };
    if let Some((level, color, _)) = continued {
        match long_end(b, level) {
            Some(end) => {
                out.push((0, end, color));
                (i, plain) = (end, end);
                *open = Open::Nothing;
            }
            None => {
                out.push((0, b.len(), color));
                return out;
            }
        }
    }
    let push = |out: &mut Vec<(usize, usize, Color)>, plain: usize, start: usize, end: usize, color: Color| {
        if plain < start {
            out.push((plain, start, TEXT));
        }
        out.push((start, end, color));
    };
    while i < b.len() {
        let c = b[i];
        let (end, color) = if c == b'-' && b.get(i + 1) == Some(&b'-') {
            match long_bracket(&b[i + 2..]) {
                Some(level) => match long_end(&b[i + 2..], level) {
                    Some(len) => (i + 2 + len, COMMENT),
                    None => {
                        *open = Open::Comment(level);
                        (b.len(), COMMENT)
                    }
                },
                None => (b.len(), COMMENT),
            }
        } else if c == b'[' && long_bracket(&b[i..]).is_some() {
            let level = long_bracket(&b[i..]).unwrap_or(0);
            match long_end(&b[i..], level) {
                Some(len) => (i + len, STRING),
                None => {
                    *open = Open::Text(level);
                    (b.len(), STRING)
                }
            }
        } else if c == b'"' || c == b'\'' {
            let mut j = i + 1;
            while j < b.len() && b[j] != c {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            ((j + 1).min(b.len()), STRING)
        } else if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'.' || (matches!(b[j], b'+' | b'-') && matches!(b[j - 1], b'e' | b'E' | b'p' | b'P'))) {
                j += 1;
            }
            (j, NUMBER)
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let len = b[i..].iter().take_while(|x| x.is_ascii_alphanumeric() || **x == b'_').count();
            let word = &line[i..i + len];
            // A table field (`t.print`) is not a built-in function.
            let field = i > 0 && b[i - 1] == b'.';
            let color = if KEYWORDS.contains(&word) {
                Some(KEYWORD)
            } else if CONSTANTS.contains(&word) {
                Some(CONSTANT)
            } else if !field && BUILTINS.contains(&word) {
                Some(BUILTIN)
            } else {
                None
            };
            match color {
                Some(color) => (i + len, color),
                None => {
                    i += len;
                    continue;
                }
            }
        } else {
            i += 1;
            continue;
        };
        push(&mut out, plain, i, end, color);
        (i, plain) = (end, end);
    }
    if plain < b.len() {
        out.push((plain, b.len(), TEXT));
    }
    out
}
