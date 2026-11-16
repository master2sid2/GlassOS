//! The shared language of a Lua program and the kernel: the operation list and the value format.
//! The same file is compiled into the kernel and into /bin/lua, so they cannot drift apart.

use alloc::string::String;
use alloc::vec::Vec;

/// Operations: (Lua table, function). The operation number is the index in this list.
/// Functions starting with `_` are internal, the Lua code in /bin/lua wraps them.
pub const OPS: &[(&str, &str)] = &[
    ("fs", "read"),
    ("fs", "write"),
    ("fs", "append"),
    ("fs", "_list"),
    ("fs", "exists"),
    ("fs", "isdir"),
    ("fs", "mkdir"),
    ("fs", "remove"),
    ("fs", "rename"),
    ("fs", "copy"),
    ("fs", "_volumes"),
    ("sys", "_time"),
    ("sys", "ticks"),
    ("sys", "user"),
    ("sys", "mem"),
    ("sys", "version"),
    ("sys", "open"),
    ("sys", "notify"),
    ("sys", "_dialog"),
    ("sys", "beep"),
    ("sys", "play"),
    ("sys", "_readline"),
    ("sys", "sleep"),
    ("net", "_get"),
    ("net", "_start"),
    ("net", "hostname"),
    ("ui", "heading"),
    ("ui", "label"),
    ("ui", "separator"),
    ("ui", "space"),
    ("ui", "sameline"),
    ("ui", "button"),
    ("ui", "input"),
    ("ui", "checkbox"),
    ("ui", "radio"),
    ("ui", "dropdown"),
    ("ui", "slider"),
    ("ui", "color"),
    ("ui", "progress"),
    ("ui", "panel"),
    ("ui", "tabs"),
    ("ui", "list"),
    ("ui", "textarea"),
    ("ui", "_canvas"),
    ("ui", "_canvas_end"),
    ("ui", "_open"),
    ("ui", "_begin"),
    ("ui", "_end"),
    ("ui", "_wait"),
    ("ui", "_fail"),
    ("gfx", "rect"),
    ("gfx", "circle"),
    ("gfx", "line"),
    ("gfx", "text"),
    ("gfx", "clear"),
    // New operations go only at the end: the numbers of already built programs do not shift.
    ("audio", "open"),
    ("audio", "play"),
    ("audio", "pause"),
    ("audio", "stop"),
    ("audio", "seek"),
    ("audio", "volume"),
    ("audio", "close"),
    ("audio", "_status"),
    ("audio", "_info"),
    ("audio", "cover"),
    ("ui", "image"),
    ("audio", "balance"),
    ("audio", "_spectrum"),
    ("audio", "_probe"),
    ("gfx", "gradient"),
    ("gfx", "image"),
    ("gfx", "measure"),
    ("gfx", "clip"),
    ("gfx", "stroke"),
];

/// How many answer bytes the kernel puts right away; a long answer is taken with a second call.
pub const SHORT_REPLY: usize = 4096;

/// A value at the process boundary. There are no fractional numbers: all API arguments are integers.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Nil,
    Bool(bool),
    Int(i64),
    Bytes(Vec<u8>),
    /// An array table of strings.
    List(Vec<String>),
}

impl Value {
    #[allow(dead_code)]
    pub fn text(s: &str) -> Value {
        Value::Bytes(s.as_bytes().to_vec())
    }
}

fn put(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

pub fn encode(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for v in values {
        match v {
            Value::Nil => out.push(0),
            Value::Bool(b) => out.push(1 + *b as u8),
            Value::Int(n) => {
                out.push(3);
                out.extend_from_slice(&n.to_le_bytes());
            }
            Value::Bytes(b) => {
                out.push(4);
                put(&mut out, b);
            }
            Value::List(l) => {
                out.push(5);
                out.extend_from_slice(&(l.len() as u32).to_le_bytes());
                for s in l {
                    put(&mut out, s.as_bytes());
                }
            }
        }
    }
    out
}

/// Parses values. The data may come from someone else's program, so any
/// mismatch is None, not a panic.
pub fn decode(mut b: &[u8]) -> Option<Vec<Value>> {
    fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        let (head, tail) = b.split_at_checked(n)?;
        *b = tail;
        Some(head)
    }
    fn len(b: &mut &[u8]) -> Option<usize> {
        Some(u32::from_le_bytes(take(b, 4)?.try_into().ok()?) as usize)
    }
    let mut out = Vec::new();
    while let Some((&tag, rest)) = b.split_first() {
        b = rest;
        out.push(match tag {
            0 => Value::Nil,
            1 => Value::Bool(false),
            2 => Value::Bool(true),
            3 => Value::Int(i64::from_le_bytes(take(&mut b, 8)?.try_into().ok()?)),
            4 => {
                let n = len(&mut b)?;
                Value::Bytes(take(&mut b, n)?.to_vec())
            }
            5 => {
                let n = len(&mut b)?;
                let mut list = Vec::new();
                for _ in 0..n {
                    let k = len(&mut b)?;
                    list.push(String::from_utf8_lossy(take(&mut b, k)?).into_owned());
                }
                Value::List(list)
            }
            _ => return None,
        });
    }
    Some(out)
}
