//! Classic Bluetooth L2CAP channels (basic mode): the profiles live on them — SDP,
//! audio (AVDTP), the remote control (AVCTP), the headset (RFCOMM). We open them ourselves (`chan_open`) and
//! accept from the device the ones we support; the configuration is the packet size (MTU) in both directions.
//! An open channel, data and closing go to the protocol by PSM (`proto_*`).

use super::Host;
use super::l2cap::{CID_SIGNAL, CONFIGURE_REQUEST, CONFIGURE_RESPONSE, CONNECTION_REQUEST, CONNECTION_RESPONSE, DISCONNECTION_REQUEST, DISCONNECTION_RESPONSE};
use crate::bt::addr_text;
use crate::kprintln;
use alloc::vec::Vec;

pub(super) const PSM_SDP: u16 = 0x0001;
pub(super) const PSM_RFCOMM: u16 = 0x0003;
pub(super) const PSM_AVCTP: u16 = 0x0017;
pub(super) const PSM_AVDTP: u16 = 0x0019;
/// The channels we accept from the device.
const ACCEPTED: &[u16] = &[PSM_SDP, PSM_RFCOMM, PSM_AVCTP, PSM_AVDTP];
/// Our packet size: enough for SBC audio frames.
pub(super) const OUR_MTU: u16 = 1021;
/// The first dynamic channel id.
const FIRST_CID: u16 = 0x0040;
const OPTION_MTU: u8 = 0x01;
const RESULT_OK: u16 = 0;
const RESULT_PENDING: u16 = 1;
const RESULT_NO_PSM: u16 = 2;
/// The default packet size until the device states its own.
const DEFAULT_MTU: u16 = 672;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum State {
    /// Waiting for the answer to our connection request.
    Connecting,
    /// Negotiating the parameters.
    Config,
    Open,
}

#[derive(Debug)]
pub(super) struct Chan {
    pub psm: u16,
    pub local: u16,
    pub remote: u16,
    pub state: State,
    /// How much the device accepts at once.
    pub mtu: u16,
    /// The id of our request (connection or configuration) waiting for an answer.
    ident: u8,
    /// The device accepted our parameters / we accepted its parameters.
    ours_done: bool,
    theirs_done: bool,
}

impl Host {
    fn next_ident(&mut self, handle: u16) -> u8 {
        let Some(c) = self.conn_mut(handle) else { return 1 };
        c.ident = c.ident.wrapping_add(1).max(1);
        c.ident
    }

    fn new_cid(&mut self, handle: u16) -> u16 {
        let Some(c) = self.conn_mut(handle) else { return FIRST_CID };
        let mut cid = c.next_cid.max(FIRST_CID);
        while c.chans.iter().any(|ch| ch.local == cid) {
            cid = cid.wrapping_add(1).max(FIRST_CID);
        }
        c.next_cid = cid.wrapping_add(1).max(FIRST_CID);
        cid
    }

    fn signal_out(&mut self, handle: u16, code: u8, ident: u8, words: &[u16], extra: &[u8]) {
        let mut d = Vec::with_capacity(words.len() * 2 + extra.len());
        for w in words {
            d.extend_from_slice(&w.to_le_bytes());
        }
        d.extend_from_slice(extra);
        self.reply(handle, CID_SIGNAL, code, ident, &d);
    }

    /// Open a channel to service `psm` of the device. Our side's id or None.
    pub(super) fn chan_open(&mut self, handle: u16, psm: u16) -> Option<u16> {
        let local = self.new_cid(handle);
        let ident = self.next_ident(handle);
        let c = self.conn_mut(handle)?;
        c.chans.push(Chan { psm, local, remote: 0, state: State::Connecting, mtu: DEFAULT_MTU, ident, ours_done: false, theirs_done: false });
        self.signal_out(handle, CONNECTION_REQUEST, ident, &[psm, local], &[]);
        Some(local)
    }

    /// Close the channel.
    pub(super) fn chan_close(&mut self, handle: u16, local: u16) {
        let Some(ch) = self.conn_mut(handle).and_then(|c| c.chans.iter().find(|ch| ch.local == local)).map(|ch| (ch.remote, ch.psm)) else { return };
        let ident = self.next_ident(handle);
        self.signal_out(handle, DISCONNECTION_REQUEST, ident, &[ch.0, local], &[]);
        if let Some(c) = self.conn_mut(handle) {
            c.chans.retain(|x| x.local != local);
        }
        self.proto_closed(handle, local, ch.1);
    }

    /// Data to the channel (one L2CAP packet each; nothing longer than the device's MTU is sent).
    pub(super) fn chan_send(&mut self, handle: u16, local: u16, data: &[u8]) -> bool {
        let Some((remote, mtu)) = self.conn_mut(handle).and_then(|c| c.chans.iter().find(|ch| ch.local == local && ch.state == State::Open)).map(|ch| (ch.remote, ch.mtu)) else {
            return false;
        };
        if data.len() > mtu as usize {
            kprintln!("bt: l2cap: {} bytes do not fit the channel (MTU {mtu})", data.len());
            return false;
        }
        self.send_l2cap(handle, remote, data);
        true
    }

    /// The packet size the device accepts on the channel.
    pub(super) fn chan_mtu(&mut self, handle: u16, local: u16) -> u16 {
        self.conn_mut(handle).and_then(|c| c.chans.iter().find(|ch| ch.local == local)).map_or(DEFAULT_MTU, |ch| ch.mtu)
    }

    /// Our parameters: the packet size.
    fn send_config(&mut self, handle: u16, local: u16) {
        let Some(remote) = self.conn_mut(handle).and_then(|c| c.chans.iter().find(|ch| ch.local == local)).map(|ch| ch.remote) else { return };
        let ident = self.next_ident(handle);
        if let Some(ch) = self.conn_mut(handle).and_then(|c| c.chans.iter_mut().find(|ch| ch.local == local)) {
            ch.ident = ident;
        }
        let mut opt = alloc::vec![OPTION_MTU, 2];
        opt.extend_from_slice(&OUR_MTU.to_le_bytes());
        self.signal_out(handle, CONFIGURE_REQUEST, ident, &[remote, 0], &opt);
    }

    /// Both sides agreed — the channel is open.
    fn maybe_open(&mut self, handle: u16, local: u16) {
        let Some(ch) = self.conn_mut(handle).and_then(|c| c.chans.iter_mut().find(|ch| ch.local == local)) else { return };
        if ch.state == State::Config && ch.ours_done && ch.theirs_done {
            ch.state = State::Open;
            let psm = ch.psm;
            self.proto_open(handle, local, psm);
        }
    }

    /// Channel signalling (classic channel 1). true — the command is ours.
    pub(super) fn chan_signal(&mut self, handle: u16, code: u8, id: u8, d: &[u8]) -> bool {
        let at = |i: usize| d.get(i..i + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
        match code {
            CONNECTION_REQUEST => {
                let (psm, remote) = (at(0), at(2));
                if !ACCEPTED.contains(&psm) {
                    kprintln!("bt: the device asks for channel PSM {psm:#06x}: not supported yet");
                    self.signal_out(handle, CONNECTION_RESPONSE, id, &[0, remote, RESULT_NO_PSM, 0], &[]);
                    return true;
                }
                let local = self.new_cid(handle);
                if let Some(c) = self.conn_mut(handle) {
                    c.chans.push(Chan { psm, local, remote, state: State::Config, mtu: DEFAULT_MTU, ident: 0, ours_done: false, theirs_done: false });
                }
                self.signal_out(handle, CONNECTION_RESPONSE, id, &[local, remote, RESULT_OK, 0], &[]);
                self.send_config(handle, local);
            }
            CONNECTION_RESPONSE => {
                let (remote, local, result) = (at(0), at(2), at(4));
                let Some(ch) = self.conn_mut(handle).and_then(|c| c.chans.iter_mut().find(|ch| ch.local == local && ch.state == State::Connecting)) else { return true };
                match result {
                    RESULT_OK => {
                        ch.remote = remote;
                        ch.state = State::Config;
                        self.send_config(handle, local);
                    }
                    RESULT_PENDING => {
                        let psm = ch.psm;
                        kprintln!("bt: l2cap: channel PSM {psm:#06x}: the device says «pending» (status {})", at(6));
                    }
                    _ => {
                        let psm = ch.psm;
                        if let Some(c) = self.conn_mut(handle) {
                            c.chans.retain(|x| x.local != local);
                        }
                        if let Some(addr) = self.addr_of(handle) {
                            kprintln!("bt: {} refused channel PSM {psm:#06x} (result {result})", addr_text(&addr));
                        }
                        self.proto_closed(handle, local, psm);
                    }
                }
            }
            CONFIGURE_REQUEST => {
                let (local, flags) = (at(0), at(2));
                let Some(ch) = self.conn_mut(handle).and_then(|c| c.chans.iter_mut().find(|ch| ch.local == local)) else { return true };
                // Parameters: type, length, value; we take the packet size, accept the rest.
                let mut opts = d.get(4..).unwrap_or(&[]);
                while opts.len() >= 2 {
                    let (kind, len) = (opts[0] & 0x7F, opts[1] as usize);
                    if kind == OPTION_MTU && len == 2 && opts.len() >= 4 {
                        ch.mtu = u16::from_le_bytes([opts[2], opts[3]]);
                    }
                    opts = opts.get(2 + len..).unwrap_or(&[]);
                }
                // A continuation (flag 1) — not everything yet.
                let remote = ch.remote;
                if flags & 1 == 0 {
                    ch.theirs_done = true;
                }
                self.signal_out(handle, CONFIGURE_RESPONSE, id, &[remote, flags & 1, RESULT_OK], &[]);
                self.maybe_open(handle, local);
            }
            CONFIGURE_RESPONSE => {
                let (local, result) = (at(0), at(4));
                let Some(ch) = self.conn_mut(handle).and_then(|c| c.chans.iter_mut().find(|ch| ch.local == local)) else { return true };
                if result == RESULT_OK {
                    ch.ours_done = true;
                    self.maybe_open(handle, local);
                } else {
                    kprintln!("bt: l2cap: our channel settings were refused (result {result})");
                }
            }
            DISCONNECTION_REQUEST => {
                let (local, remote) = (at(0), at(2));
                self.signal_out(handle, DISCONNECTION_RESPONSE, id, &[local, remote], &[]);
                let psm = self.conn_mut(handle).and_then(|c| c.chans.iter().find(|ch| ch.local == local)).map(|ch| ch.psm);
                if let Some(c) = self.conn_mut(handle) {
                    c.chans.retain(|x| x.local != local);
                }
                if let Some(psm) = psm {
                    self.proto_closed(handle, local, psm);
                }
            }
            DISCONNECTION_RESPONSE => {}
            _ => return false,
        }
        true
    }

    /// Data on a dynamic channel — to its protocol.
    pub(super) fn chan_data(&mut self, handle: u16, local: u16, data: &[u8]) {
        let Some(psm) = self.conn_mut(handle).and_then(|c| c.chans.iter().find(|ch| ch.local == local && ch.state == State::Open)).map(|ch| ch.psm) else { return };
        self.proto_data(handle, local, psm, data);
    }

    /// The channel opened — to the protocol.
    fn proto_open(&mut self, handle: u16, local: u16, psm: u16) {
        match psm {
            PSM_SDP => self.sdp_open(handle, local),
            PSM_AVDTP => self.avdtp_open(handle, local),
            PSM_RFCOMM => self.rfcomm_open(handle, local),
            PSM_AVCTP => self.avrcp_open(handle, local),
            _ => {}
        }
    }

    fn proto_data(&mut self, handle: u16, local: u16, psm: u16, data: &[u8]) {
        match psm {
            PSM_SDP => self.sdp_data(handle, local, data),
            PSM_AVDTP => self.avdtp_data(handle, local, data),
            PSM_RFCOMM => self.rfcomm_data(handle, local, data),
            PSM_AVCTP => self.avrcp_data(handle, local, data),
            _ => {}
        }
    }

    fn proto_closed(&mut self, handle: u16, local: u16, psm: u16) {
        match psm {
            PSM_SDP => self.sdp_closed(handle, local),
            PSM_AVDTP => self.avdtp_closed(handle, local),
            PSM_RFCOMM => self.rfcomm_closed(handle, local),
            PSM_AVCTP => self.avrcp_closed(handle, local),
            _ => {}
        }
    }
}
