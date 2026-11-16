//! An SNTP client (RFC 4330): one question to a time server over UDP.

use super::{UdpSocket, resolve};
use crate::interrupts::{self, HZ};
use crate::task;
use alloc::format;
use alloc::string::String;

const PORT: u16 = 123;
const PACKET: usize = 48;
/// Version 4, "client" mode.
const ASK: u8 = 4 << 3 | 3;
const MODE_SERVER: u8 = 4;
/// How many times to ask and how long to wait for an answer.
const TRIES: usize = 3;
const WAIT: u64 = 2 * HZ;
/// Seconds from 1900 at which Unix seconds start.
const UNIX_FROM_1900: i64 = 2_208_988_800;

pub struct Answer {
    /// The UTC time in milliseconds at the moment of tick `tick`.
    pub unix_ms: i64,
    pub tick: u64,
    /// The round trip without the server's processing time.
    pub delay_ms: i64,
}

/// An NTP timestamp to Unix milliseconds. After 2036 the seconds start counting anew.
fn to_unix_ms(stamp: &[u8]) -> i64 {
    let secs = u32::from_be_bytes([stamp[0], stamp[1], stamp[2], stamp[3]]) as i64;
    let frac = u32::from_be_bytes([stamp[4], stamp[5], stamp[6], stamp[7]]) as i64;
    let secs = if secs < 1 << 31 { secs + (1 << 32) } else { secs };
    (secs - UNIX_FROM_1900) * 1000 + (frac * 1000 >> 32)
}

pub fn query(server: &str) -> Result<Answer, String> {
    let ip = resolve(server).ok_or_else(|| format!("{server} is not found"))?;
    let socket = UdpSocket::bind(0).ok_or("the network is off")?;
    for _ in 0..TRIES {
        // The transmit time field holds a random tag: the answer must return it.
        let mut ask = [0u8; PACKET];
        ask[0] = ASK;
        let nonce = crate::arch::rdtsc().to_be_bytes();
        ask[40..48].copy_from_slice(&nonce);
        let sent = interrupts::ticks();
        socket.send_to(ip, PORT, &ask);
        while interrupts::ticks() < sent + WAIT {
            let Some((from, port, data)) = socket.try_recv() else {
                task::sleep_until(interrupts::ticks() + 1);
                continue;
            };
            if from != ip || port != PORT || data.len() < PACKET || data[0] & 7 != MODE_SERVER || data[24..32] != nonce {
                continue;
            }
            let got = interrupts::ticks();
            let (stratum, leap) = (data[1], data[0] >> 6);
            if stratum == 0 || leap == 3 {
                return Err(format!("{server} has no good time now"));
            }
            let (received, replied) = (to_unix_ms(&data[32..40]), to_unix_ms(&data[40..48]));
            let round = ((got - sent) * 1000 / HZ) as i64;
            let delay_ms = (round - (replied - received)).max(0);
            return Ok(Answer { unix_ms: replied + delay_ms / 2, tick: got, delay_ms });
        }
    }
    Err(format!("{server} does not answer"))
}
