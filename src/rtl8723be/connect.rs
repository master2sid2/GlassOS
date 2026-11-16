//! Joining a network: authentication (open), association, the WPA2 handshake and writing the CCMP keys.
//! After the keys, the data path is checked: an encrypted DHCP DISCOVER and waiting for the answer;
//! then the interface is handed to the network stack. 802.11 frames are standard; the card
//! fills in the sequence number itself (HWSEQ).

use super::{Chip, Radio, park_if_stopping};
use crate::wifi::Network;
use crate::wifi::wpa::{Handshake, Step};
use crate::{interrupts, task};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// How long to wait for an answer to each attempt, and how many attempts.
const REPLY_TICKS: u64 = interrupts::HZ / 2;
const TRIES: usize = 4;

/// Station capabilities: ESS, privacy (an encrypted network), short preamble and slot.
const CAPABILITY: u16 = 0x0001 | 0x0010 | 0x0020 | 0x0400;
const LISTEN_INTERVAL: u16 = 10;
/// The security description in association: WPA2, CCMP (group and pairwise), PSK key. The same as
/// wifi::wpa::RSN — a standard RSN element, not tied to the card.
const RSN: [u8; 22] = [48, 20, 1, 0, 0, 0x0F, 0xAC, 4, 1, 0, 0, 0x0F, 0xAC, 4, 1, 0, 0, 0x0F, 0xAC, 2, 0, 0];

/// Management frame subtypes (the FC high byte): authentication request/response, association
/// request and response.
const FC_AUTH: u8 = 0xB0;
const FC_ASSOC_REQUEST: u8 = 0x00;
const FC_ASSOC_RESPONSE: u8 = 0x10;

/// A management frame header to the access point (24 bytes): FC, duration, three addresses,
/// the sequence field (the card fills it in).
fn header(fc: u8, mac: &[u8; 6], bssid: &[u8; 6]) -> [u8; 24] {
    let mut h = [0u8; 24];
    h[0] = fc;
    h[4..10].copy_from_slice(bssid); // addr1 — receiver (the access point)
    h[10..16].copy_from_slice(mac); // addr2 — transmitter (us)
    h[16..22].copy_from_slice(bssid); // addr3 — BSSID
    h
}

fn auth_request(mac: &[u8; 6], bssid: &[u8; 6]) -> Vec<u8> {
    let mut f = header(FC_AUTH, mac, bssid).to_vec();
    // Algorithm 0 (open), transaction 1, status 0.
    f.extend_from_slice(&[0, 0, 1, 0, 0, 0]);
    f
}

fn assoc_request(mac: &[u8; 6], bssid: &[u8; 6], ssid: &str) -> Vec<u8> {
    let mut f = header(FC_ASSOC_REQUEST, mac, bssid).to_vec();
    f.extend_from_slice(&CAPABILITY.to_le_bytes());
    f.extend_from_slice(&LISTEN_INTERVAL.to_le_bytes());
    f.extend_from_slice(&[0, ssid.len() as u8]); // SSID element
    f.extend_from_slice(ssid.as_bytes());
    // 802.11b/g rates (as in the probe request) — basic ones are marked with the top bit.
    f.extend_from_slice(&[1, 8, 0x82, 0x84, 0x8B, 0x96, 0x0C, 0x12, 0x18, 0x24]);
    f.extend_from_slice(&[50, 4, 0x30, 0x48, 0x60, 0x6C]);
    f.extend_from_slice(&RSN);
    f
}

/// Waits for a management frame of the given subtype from this access point (addr3 = BSSID). Everything
/// else from the air is simply drained from the ring.
fn wait_reply(radio: &mut Radio, subtype: u8, bssid: &[u8; 6]) -> Option<Vec<u8>> {
    let end = interrupts::ticks() + REPLY_TICKS;
    while interrupts::ticks() < end {
        park_if_stopping();
        let mut got = None;
        radio.rx.poll(|f| {
            if got.is_none() && f.data.len() >= 30 && f.data[0] == subtype && f.data.get(16..22) == Some(&bssid[..]) {
                got = Some(f.data.to_vec());
            }
        });
        if got.is_some() {
            return got;
        }
        task::sleep_until(interrupts::ticks() + 1);
    }
    None
}

fn authenticate(chip: &Chip, radio: &mut Radio, bssid: &[u8; 6]) -> Result<(), String> {
    for _ in 0..TRIES {
        radio.tx.send(chip, &auth_request(&radio.mac, bssid));
        if let Some(resp) = wait_reply(radio, FC_AUTH, bssid) {
            let status = u16::from_le_bytes([resp[28], resp[29]]);
            if status != 0 {
                return Err(format!("the access point refused authentication, status {status}"));
            }
            return Ok(());
        }
    }
    Err("no answer to authentication".into())
}

fn associate(chip: &Chip, radio: &mut Radio, bssid: &[u8; 6], ssid: &str) -> Result<u16, String> {
    for _ in 0..TRIES {
        radio.tx.send(chip, &assoc_request(&radio.mac, bssid, ssid));
        if let Some(resp) = wait_reply(radio, FC_ASSOC_RESPONSE, bssid) {
            let status = u16::from_le_bytes([resp[26], resp[27]]);
            if status != 0 {
                return Err(format!("the access point refused association, status {status}"));
            }
            return Ok(u16::from_le_bytes([resp[28], resp[29]]) & 0x3FFF);
        }
    }
    Err("no answer to association".into())
}

/// LLC header + the "EAPOL" type (0x888E) — the data frame payload starts with it.
const LLC_EAPOL: [u8; 8] = [0xAA, 0xAA, 0x03, 0x00, 0x00, 0x00, 0x88, 0x8E];

/// A data frame with EAPOL to the access point (TO_DS, unencrypted — there are no keys yet during
/// the handshake). addr1 = BSSID (where), addr2 = us, addr3 = BSSID (to whom, the access point itself).
fn eapol_frame(mac: &[u8; 6], bssid: &[u8; 6], payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(24 + LLC_EAPOL.len() + payload.len());
    f.extend_from_slice(&[0x08, 0x01, 0, 0]); // FC: data, TO_DS; duration 0
    f.extend_from_slice(bssid);
    f.extend_from_slice(mac);
    f.extend_from_slice(bssid);
    f.extend_from_slice(&[0, 0]); // sequence number — filled in by the card
    f.extend_from_slice(&LLC_EAPOL);
    f.extend_from_slice(payload);
    f
}

/// If this is an unencrypted data frame with EAPOL, returns the EAPOL payload.
fn eapol_payload(data: &[u8]) -> Option<&[u8]> {
    if data.len() < 24 || data[0] & 0x0C != 0x08 {
        return None; // not a data frame
    }
    let hdr = if data[0] & 0x80 != 0 { 26 } else { 24 }; // QoS data is 2 bytes longer
    if data.get(hdr..hdr + 8)? != LLC_EAPOL {
        return None; // another type, or the frame is encrypted (then the LLC does not match)
    }
    data.get(hdr + 8..)
}

/// The WPA2 four-way handshake: drive wpa::Handshake, send EAPOL frames as data.
/// Returns the pairwise key and the group key (with its id) — they are written to the card next.
/// The `Handshake` itself stays alive: it also answers group key renewals.
fn handshake(chip: &Chip, radio: &mut Radio, bssid: &[u8; 6], pmk: [u8; 32]) -> Result<([u8; 16], (u8, [u8; 16]), Handshake), String> {
    let mut hs = Handshake::new(pmk, radio.mac, *bssid);
    let end = interrupts::ticks() + 3 * interrupts::HZ;
    while interrupts::ticks() < end {
        park_if_stopping();
        let mut got: Vec<Vec<u8>> = Vec::new();
        radio.rx.poll(|f| {
            if f.data.get(10..16) == Some(&bssid[..])
                && let Some(p) = eapol_payload(f.data)
            {
                got.push(p.to_vec());
            }
        });
        for frame in got {
            crate::kprintln!("rtl8723be: handshake: EAPOL in, {} bytes", frame.len());
            match hs.key_frame(&frame).map_err(String::from)? {
                Step::Reply(reply) => {
                    crate::kprintln!("rtl8723be: handshake: sent reply (M2), {} bytes", reply.len());
                    radio.tx.send(chip, &eapol_frame(&radio.mac, bssid, &reply));
                }
                Step::Keys { reply, pairwise, group } => {
                    radio.tx.send(chip, &eapol_frame(&radio.mac, bssid, &reply));
                    return Ok((pairwise, group, hs));
                }
                Step::Group { reply, .. } => {
                    radio.tx.send(chip, &eapol_frame(&radio.mac, bssid, &reply));
                }
                Step::Ignore => {}
            }
        }
        task::sleep_until(interrupts::ticks() + 1);
    }
    Err("no answer to the WPA2 handshake (wrong password?)".into())
}

/// LLC header + the IP type (0x0800) — the start of a data frame payload with an IP packet.
const LLC_IP: [u8; 8] = [0xAA, 0xAA, 0x03, 0x00, 0x00, 0x00, 0x08, 0x00];

/// The 8-byte CCMP header: packet number (PN, 48 bits) + the extended IV flag and the key
/// id. The card encrypts the payload with this PN and appends the MIC; a PN must never repeat.
fn ccmp_header(pn: u64, keyid: u8) -> [u8; 8] {
    let b = pn.to_le_bytes();
    [b[0], b[1], 0, 0x20 | (keyid << 6), b[2], b[3], b[4], b[5]]
}

/// An encrypted data frame to the access point: an 802.11 header with the Protected bit, the CCMP header,
/// then the payload (LLC + IP…). addr1 = BSSID (the card finds the pairwise key in CAM by it), addr3 is
/// the real destination. The MIC and the encryption itself are added by the card.
fn data_encrypted(mac: &[u8; 6], bssid: &[u8; 6], dest: &[u8; 6], pn: u64, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(24 + 8 + payload.len());
    f.extend_from_slice(&[0x08, 0x41, 0, 0]); // data, TO_DS (0x01), Protected (0x40)
    f.extend_from_slice(bssid); // addr1 — to the access point
    f.extend_from_slice(mac); // addr2 — us
    f.extend_from_slice(dest); // addr3 — the real destination
    f.extend_from_slice(&[0, 0]); // sequence number — filled in by the card
    f.extend_from_slice(&ccmp_header(pn, 0));
    f.extend_from_slice(payload);
    f
}

/// Wraps an outgoing Ethernet frame from the stack into an encrypted 802.11 data frame to the access
/// point: addr3 = the Ethernet destination, payload = LLC/SNAP with the same type + data.
pub(super) fn wrap_eth(mac: &[u8; 6], bssid: &[u8; 6], pn: u64, eth: &[u8]) -> Option<Vec<u8>> {
    if eth.len() < 14 {
        return None;
    }
    let dst: [u8; 6] = eth[0..6].try_into().ok()?;
    let mut payload = Vec::with_capacity(8 + eth.len() - 14);
    payload.extend_from_slice(&[0xAA, 0xAA, 0x03, 0x00, 0x00, 0x00, eth[12], eth[13]]);
    payload.extend_from_slice(&eth[14..]);
    Some(data_encrypted(mac, bssid, &dst, pn, &payload))
}

/// Unwraps an incoming 802.11 data frame back into Ethernet. A frame from the access point comes
/// with FROM_DS: addr1 is the receiver (us or a group), addr3 the sender. The card may leave the CCMP
/// header after decryption — look for LLC/SNAP both right after the header and 8 bytes later.
pub(super) fn unwrap_eth(data: &[u8], mac: &[u8; 6]) -> Option<Vec<u8>> {
    if data.len() < 24 || data[0] & 0x0C != 0x08 {
        return None; // not a data frame
    }
    let ra = data.get(4..10)?; // addr1 — receiver
    if ra != &mac[..] && ra[0] & 1 == 0 {
        return None; // not for us and not for a group
    }
    let hdr = if data[0] & 0x80 != 0 { 26 } else { 24 }; // QoS data is 2 bytes longer
    const SNAP: [u8; 6] = [0xAA, 0xAA, 0x03, 0x00, 0x00, 0x00];
    let off = if data.get(hdr..hdr + 6) == Some(&SNAP) {
        hdr
    } else if data.get(hdr + 8..hdr + 14) == Some(&SNAP) {
        hdr + 8
    } else {
        return None; // another frame type, or still encrypted
    };
    let ethertype = data.get(off + 6..off + 8)?;
    let src = data.get(16..22)?; // addr3 — sender
    let payload = data.get(off + 8..)?;
    let mut eth = Vec::with_capacity(14 + payload.len());
    eth.extend_from_slice(ra);
    eth.extend_from_slice(src);
    eth.extend_from_slice(ethertype);
    eth.extend_from_slice(payload);
    Some(eth)
}

/// A DHCP DISCOVER body (BOOTP + magic cookie + options): ask for an address; the "broadcast reply" flag,
/// since we have no address yet.
fn dhcp_discover(mac: &[u8; 6], xid: u32) -> Vec<u8> {
    let mut d = alloc::vec![0u8; 240];
    d[0] = 1; // BOOTREQUEST
    d[1] = 1; // Ethernet
    d[2] = 6; // MAC length
    d[4..8].copy_from_slice(&xid.to_be_bytes());
    d[10] = 0x80; // broadcast reply
    d[28..34].copy_from_slice(mac);
    d[236..240].copy_from_slice(&[99, 130, 83, 99]); // magic cookie
    d.extend_from_slice(&[53, 1, 1]); // type: DISCOVER
    d.extend_from_slice(&[55, 3, 1, 3, 6]); // ask for mask, router, DNS
    d.push(255); // end of options
    d
}

/// A DHCP DISCOVER wrapped in UDP (68→67) and IP (0.0.0.0 → 255.255.255.255) — as on Ethernet.
fn dhcp_packet(mac: &[u8; 6], xid: u32) -> Vec<u8> {
    let dhcp = dhcp_discover(mac, xid);
    let mut udp = Vec::with_capacity(8 + dhcp.len());
    udp.extend_from_slice(&68u16.to_be_bytes());
    udp.extend_from_slice(&67u16.to_be_bytes());
    udp.extend_from_slice(&((8 + dhcp.len()) as u16).to_be_bytes());
    udp.extend_from_slice(&[0, 0]); // the UDP checksum is optional in IPv4
    udp.extend_from_slice(&dhcp);
    let mut ip = Vec::with_capacity(20 + udp.len());
    ip.extend_from_slice(&[0x45, 0]);
    ip.extend_from_slice(&((20 + udp.len()) as u16).to_be_bytes());
    ip.extend_from_slice(&[0, 0, 0x40, 0, 64, 17, 0, 0]); // id 0, don't fragment, ttl 64, UDP
    ip.extend_from_slice(&[0, 0, 0, 0]); // source 0.0.0.0
    ip.extend_from_slice(&[255, 255, 255, 255]); // destination — broadcast
    let sum = crate::net::checksum(&[&ip[..20]]);
    ip[10..12].copy_from_slice(&sum.to_be_bytes());
    ip.extend_from_slice(&udp);
    ip
}

/// If the frame is a DHCP reply to us (BOOTREPLY with our xid), return the offered address (yiaddr).
/// Search by the magic cookie: whether the card strips the CCMP header on decryption is not known in advance,
/// so BOOTP fields are read relative to the cookie, not from the start of the frame.
fn dhcp_reply(data: &[u8], xid: u32) -> Option<[u8; 4]> {
    let pos = data.windows(4).position(|w| w == [99, 130, 83, 99])?;
    let dhcp = data.get(pos.checked_sub(236)?..)?;
    if dhcp.first() != Some(&2) || dhcp.get(4..8)? != xid.to_be_bytes() {
        return None;
    }
    Some([dhcp[16], dhcp[17], dhcp[18], dhcp[19]])
}

/// Data path check after writing the keys: send an encrypted DHCP DISCOVER and wait for the answer.
/// Routes are not touched yet — this is a driver-level check (it confirms that the access point
/// accepts our encrypted frames).
fn dhcp_probe(chip: &Chip, radio: &mut Radio, bssid: &[u8; 6]) {
    let xid = crate::arch::rdtsc() as u32;
    let payload = {
        let mut p = LLC_IP.to_vec();
        p.extend_from_slice(&dhcp_packet(&radio.mac, xid));
        p
    };
    let end = interrupts::ticks() + 4 * interrupts::HZ;
    let (mut sent, mut next, mut offer) = (0u64, 0u64, None);
    while interrupts::ticks() < end && offer.is_none() {
        park_if_stopping();
        if interrupts::ticks() >= next {
            let pn = super::TX_PN.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
            let frame = data_encrypted(&radio.mac, bssid, &[0xFF; 6], pn, &payload);
            if radio.tx.send_encrypted(chip, &frame) {
                sent += 1;
            }
            next = interrupts::ticks() + interrupts::HZ; // retry once a second
        }
        let mut got = None;
        radio.rx.poll(|f| {
            if got.is_none()
                && let Some(ip) = dhcp_reply(f.data, xid)
            {
                got = Some(ip);
            }
        });
        offer = got;
        task::sleep_until(interrupts::ticks() + 1);
    }
    match offer {
        Some(ip) => crate::kprintln!("rtl8723be: data path OK \u{2014} DHCP reply {}.{}.{}.{} (sent {sent} encrypted discover)", ip[0], ip[1], ip[2], ip[3]),
        None => crate::kprintln!("rtl8723be: data path: no DHCP reply after {sent} encrypted discover(s)"),
    }
}

/// The full path: authentication, association, the WPA2 handshake, writing the CCMP keys. Then
/// the data path (sending encrypted frames) and DHCP.
pub(super) fn run(chip: &Chip, radio: &mut Radio, ssid: &str, pmk: [u8; 32]) -> Result<(), String> {
    let target: Network = super::found().into_iter().find(|n| n.ssid == ssid).ok_or_else(|| format!("{ssid} is not in the latest scan"))?;
    chip.set_channel(radio.chnlbw, target.channel);
    chip.join_mac(&target.bssid);
    radio.active = true;
    let steps = (|| {
        crate::wifi::set_rtl_status(format!("authenticating with {ssid}\u{2026}"));
        authenticate(chip, radio, &target.bssid)?;
        crate::wifi::set_rtl_status(format!("associating with {ssid}\u{2026}"));
        let aid = associate(chip, radio, &target.bssid, ssid)?;
        crate::kprintln!("rtl8723be: associated with {ssid}, id {aid}");
        crate::wifi::set_rtl_status(format!("signing in to {ssid} (WPA2)\u{2026}"));
        let (pairwise, group, hs) = handshake(chip, radio, &target.bssid, pmk)?;
        radio.hs = Some(hs);
        crate::kprintln!("rtl8723be: WPA2 handshake done; pairwise {:02x}{:02x}.., group key {} {:02x}{:02x}..", pairwise[0], pairwise[1], group.0, group.1[0], group.1[1]);
        chip.install_keys(&pairwise, group, &target.bssid);
        crate::kprintln!("rtl8723be: keys installed (CCMP; pairwise slot 4, group slot {})", group.0);
        // First check the encrypted path with our own DHCP request (the shared PN counter is continued by
        // the pump), then hand the interface to the stack — so its DHCP does not wait in the queue during the probe.
        *super::BSSID.lock() = target.bssid;
        super::TX_PN.store(0, core::sync::atomic::Ordering::Relaxed);
        crate::wifi::set_rtl_status(String::from("connected (encrypted) \u{2014} address (DHCP) next"));
        dhcp_probe(chip, radio, &target.bssid);
        // The link is up: start the pump and hand the interface to the stack (it will ask for an address over Wi-Fi).
        super::RX_RATE.store(0, core::sync::atomic::Ordering::Relaxed);
        super::HEARD.store(interrupts::ticks(), core::sync::atomic::Ordering::Relaxed);
        super::CONNECTED.store(true, core::sync::atomic::Ordering::Relaxed);
        crate::wifi::nic_up(radio.mac);
        Ok(())
    })();
    // After a failure, return the MAC to the scanning state so scanning works as before.
    if steps.is_err() {
        radio.hs = None;
        chip.leave_mac();
        super::CONNECTED.store(false, core::sync::atomic::Ordering::Relaxed);
        crate::wifi::nic_down();
    }
    steps
}
