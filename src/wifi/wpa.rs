//! The WPA2-PSK four-way handshake: the encryption keys are derived from the network key and two random
//! numbers. Only the computation and the parsing of EAPOL frames live here.

use crate::crypto::{aes_unwrap, hmac_sha1, random32};
use alloc::vec;
use alloc::vec::Vec;

const HEADER: usize = 4;
const INFO: usize = 5;
const REPLAY: usize = 9;
const NONCE: usize = 17;
const MIC: usize = 81;
const DATA_LEN: usize = 97;
const DATA: usize = 99;
const MIC_LEN: usize = 16;
const KEY_TYPE_RSN: u8 = 2;
const EAPOL_VERSION: u8 = 1;
const EAPOL_KEY: u8 = 3;

const VERSION_AES: u16 = 2;
const PAIRWISE: u16 = 1 << 3;
const ACK: u16 = 1 << 7;
const HAS_MIC: u16 = 1 << 8;
const SECURE: u16 = 1 << 9;
const ENCRYPTED: u16 = 1 << 12;

const KDE: u8 = 0xDD;
const KDE_GTK: [u8; 4] = [0x00, 0x0F, 0xAC, 1];
const KEY_ID_MASK: u8 = 3;

/// The security description we send to the access point: WPA2, CCMP, a pre-shared key.
pub const RSN: [u8; 22] = [48, 20, 1, 0, 0, 0x0F, 0xAC, 4, 1, 0, 0, 0x0F, 0xAC, 4, 1, 0, 0, 0x0F, 0xAC, 2, 0, 0];

/// What to do with a handshake frame.
pub enum Step {
    /// Answer with this EAPOL frame.
    Reply(Vec<u8>),
    /// Answer and install the keys: the pairwise one and the group one with its id.
    Keys { reply: Vec<u8>, pairwise: [u8; 16], group: (u8, [u8; 16]) },
    /// The access point changed the group key.
    Group { reply: Vec<u8>, group: (u8, [u8; 16]) },
    Ignore,
}

pub struct Handshake {
    pmk: [u8; 32],
    mac: [u8; 6],
    bssid: [u8; 6],
    snonce: [u8; 32],
    /// KCK, KEK and the data key.
    ptk: Option<[u8; 48]>,
}

/// The PRF function from the 802.11i standard.
fn prf(key: &[u8], label: &[u8], data: &[u8], out: &mut [u8]) {
    for (counter, chunk) in out.chunks_mut(20).enumerate() {
        let mut msg = label.to_vec();
        msg.push(0);
        msg.extend_from_slice(data);
        msg.push(counter as u8);
        let hash = hmac_sha1(key, &msg);
        chunk.copy_from_slice(&hash[..chunk.len()]);
    }
}

/// The pairwise keys from the network key, the addresses and the random numbers.
pub fn derive_ptk(pmk: &[u8; 32], a: &[u8; 6], b: &[u8; 6], anonce: &[u8; 32], snonce: &[u8; 32]) -> [u8; 48] {
    let mut data = Vec::with_capacity(76);
    data.extend_from_slice(a.min(b));
    data.extend_from_slice(a.max(b));
    data.extend_from_slice(anonce.min(snonce));
    data.extend_from_slice(anonce.max(snonce));
    let mut ptk = [0u8; 48];
    prf(pmk, b"Pairwise key expansion", &data, &mut ptk);
    ptk
}

fn mic(kck: &[u8], frame: &[u8]) -> [u8; MIC_LEN] {
    let mut copy = frame.to_vec();
    copy[MIC..MIC + MIC_LEN].fill(0);
    let hash = hmac_sha1(kck, &copy);
    let mut out = [0u8; MIC_LEN];
    out.copy_from_slice(&hash[..MIC_LEN]);
    out
}

fn group_key(data: &[u8]) -> Option<(u8, [u8; 16])> {
    let mut at = 0;
    while at + 2 <= data.len() {
        let (kind, len) = (data[at], data[at + 1] as usize);
        let body = data.get(at + 2..at + 2 + len)?;
        if kind == KDE && body.starts_with(&KDE_GTK) && body.len() >= 6 + 16 {
            let mut key = [0u8; 16];
            key.copy_from_slice(&body[6..22]);
            return Some((body[4] & KEY_ID_MASK, key));
        }
        at += 2 + len;
    }
    None
}

impl Handshake {
    pub fn new(pmk: [u8; 32], mac: [u8; 6], bssid: [u8; 6]) -> Self {
        Self { pmk, mac, bssid, snonce: random32(), ptk: None }
    }

    /// Our frame: the header, the fields and the MIC over the whole frame.
    fn frame(&self, info: u16, replay: &[u8], nonce: &[u8; 32], data: &[u8], kck: &[u8]) -> Vec<u8> {
        let mut frame = vec![0u8; DATA + data.len()];
        frame[0] = EAPOL_VERSION;
        frame[1] = EAPOL_KEY;
        let body = (frame.len() - HEADER) as u16;
        frame[2..4].copy_from_slice(&body.to_be_bytes());
        frame[HEADER] = KEY_TYPE_RSN;
        frame[INFO..INFO + 2].copy_from_slice(&info.to_be_bytes());
        frame[REPLAY..REPLAY + 8].copy_from_slice(replay);
        frame[NONCE..NONCE + 32].copy_from_slice(nonce);
        frame[DATA_LEN..DATA].copy_from_slice(&(data.len() as u16).to_be_bytes());
        frame[DATA..].copy_from_slice(data);
        let check = mic(kck, &frame);
        frame[MIC..MIC + MIC_LEN].copy_from_slice(&check);
        frame
    }

    /// Parses a frame from the access point. Err — the frame is forged or the password is wrong.
    pub fn key_frame(&mut self, frame: &[u8]) -> Result<Step, &'static str> {
        if frame.len() < DATA || frame[1] != EAPOL_KEY || frame[HEADER] != KEY_TYPE_RSN {
            return Ok(Step::Ignore);
        }
        // An encryption tail may follow: the MIC is computed over the frame itself only.
        let total = HEADER + u16::from_be_bytes([frame[2], frame[3]]) as usize;
        let frame = frame.get(..total).ok_or("broken key frame")?;
        let info = u16::from_be_bytes([frame[INFO], frame[INFO + 1]]);
        let size = u16::from_be_bytes([frame[DATA_LEN], frame[DATA_LEN + 1]]) as usize;
        let data = frame.get(DATA..DATA + size).ok_or("broken key frame")?;
        let replay = &frame[REPLAY..REPLAY + 8];
        let mut anonce = [0u8; 32];
        anonce.copy_from_slice(&frame[NONCE..NONCE + 32]);
        if info & ACK != 0 && info & HAS_MIC == 0 {
            let ptk = derive_ptk(&self.pmk, &self.mac, &self.bssid, &anonce, &self.snonce);
            self.ptk = Some(ptk);
            return Ok(Step::Reply(self.frame(VERSION_AES | PAIRWISE | HAS_MIC, replay, &self.snonce, &RSN, &ptk[..16])));
        }
        let Some(ptk) = self.ptk else { return Ok(Step::Ignore) };
        let (kck, kek) = (&ptk[..16], <&[u8; 16]>::try_from(&ptk[16..32]).map_err(|_| "bad key")?);
        if frame[MIC..MIC + MIC_LEN] != mic(kck, frame) {
            return Err("wrong password");
        }
        let plain = if info & ENCRYPTED != 0 { aes_unwrap(kek, data).ok_or("cannot unwrap the keys")? } else { data.to_vec() };
        let group = group_key(&plain);
        if info & PAIRWISE != 0 {
            let reply = self.frame(VERSION_AES | PAIRWISE | HAS_MIC | SECURE, replay, &[0; 32], &[], kck);
            let mut pairwise = [0u8; 16];
            pairwise.copy_from_slice(&ptk[32..48]);
            return Ok(Step::Keys { reply, pairwise, group: group.ok_or("no group key in the handshake")? });
        }
        // In a group key renewal it lies right there, without a KDE only on old access points.
        let reply = self.frame(VERSION_AES | HAS_MIC | SECURE, replay, &[0; 32], &[], kck);
        Ok(group.map_or(Step::Ignore, |group| Step::Group { reply, group }))
    }
}

/// A check against known values: the network key from 802.11i and the example from RFC 3394.
pub fn self_test() -> bool {
    use crate::crypto::{sha1, wpa_psk};
    const PSK: [u8; 32] = [
        0xF4, 0x2C, 0x6F, 0xC5, 0x2D, 0xF0, 0xEB, 0xEF, 0x9E, 0xBB, 0x4B, 0x90, 0xB3, 0x8A, 0x5F, 0x90, 0x2E, 0x83, 0xFE, 0x1B, 0x13, 0x5A, 0x70, 0xE2, 0x3A,
        0xED, 0x76, 0x2E, 0x97, 0x10, 0xA1, 0x2E,
    ];
    const SHA1_ABC: [u8; 4] = [0xA9, 0x99, 0x3E, 0x36];
    const WRAPPED: [u8; 24] = [
        0x1F, 0xA6, 0x8B, 0x0A, 0x81, 0x12, 0xB4, 0x47, 0xAE, 0xF3, 0x4B, 0xD8, 0xFB, 0x5A, 0x7B, 0x82, 0x9D, 0x3E, 0x86, 0x23, 0x71, 0xD2, 0xCF, 0xE5,
    ];
    const PLAIN: [u8; 16] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
    // The handshake is computed separately, not by this code.
    const PTK_START: [u8; 4] = [0xE4, 0xCF, 0x08, 0x08];
    const PTK_END: [u8; 4] = [0xF5, 0xA3, 0x7D, 0x35];
    const REPLY_MIC: [u8; 16] = [0xAD, 0x88, 0x30, 0xF1, 0xF1, 0x2F, 0xC0, 0x99, 0xF6, 0x8D, 0x60, 0xB7, 0x2D, 0x52, 0xBF, 0x63];
    let kek: [u8; 16] = core::array::from_fn(|i| i as u8);
    let basics = sha1(b"abc")[..4] == SHA1_ABC && wpa_psk(b"password", b"IEEE") == PSK && aes_unwrap(&kek, &WRAPPED).is_some_and(|p| p == PLAIN);
    let pmk: [u8; 32] = core::array::from_fn(|i| i as u8);
    let (mac, bssid) = ([2, 0, 0, 0, 0, 1], [2, 0, 0, 0, 0, 2]);
    let ptk = derive_ptk(&pmk, &mac, &bssid, &[0x11; 32], &[0x22; 32]);
    let mut first = vec![0u8; DATA];
    first[1] = EAPOL_KEY;
    first[2..4].copy_from_slice(&((DATA - HEADER) as u16).to_be_bytes());
    first[HEADER] = KEY_TYPE_RSN;
    first[INFO..INFO + 2].copy_from_slice(&(VERSION_AES | PAIRWISE | ACK).to_be_bytes());
    first[REPLAY + 7] = 7;
    first[NONCE..NONCE + 32].fill(0x11);
    let mut handshake = Handshake::new(pmk, mac, bssid);
    handshake.snonce = [0x22; 32];
    let reply = match handshake.key_frame(&first) {
        Ok(Step::Reply(reply)) => reply,
        _ => return false,
    };
    basics && ptk[..4] == PTK_START && ptk[44..48] == PTK_END && reply[MIC..MIC + MIC_LEN] == REPLY_MIC
}
