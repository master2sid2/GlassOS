//! The headset (HFP 1.7), we are the gateway (like a phone). The headphones talk with AT commands over
//! an RFCOMM port: what they can do, what indicators we have; once agreed (SLC), the
//! voice channel can be opened — the microphone and the call audio (the next step: SCO).

use super::Host;
use super::rfcomm::CHANNEL_HFP_AG;
use super::sdp::{Service, UUID_HANDSFREE};
use crate::bt::addr_text;
use crate::kprintln;
use alloc::format;
use alloc::string::String;

/// The headset supports volume and gain set by the gateway (bit 4 of its +BRSF).
const HF_REMOTE_VOLUME: u32 = 1 << 4;
/// What we can do as the gateway (+BRSF): no calls; headset indicators (bit 10) — so that
/// it reports its battery (AT+BIEV).
const AG_FEATURES: u32 = 1 << 10;
/// The headset "battery level" indicator (HFP, Assigned Numbers).
const HF_BATTERY: u32 = 2;
/// The gateway indicators: name, range and the current value.
const INDICATORS: [(&str, &str, u8); 7] = [
    ("service", "(0,1)", 1),
    ("call", "(0,1)", 0),
    ("callsetup", "(0-3)", 0),
    ("callheld", "(0-2)", 0),
    ("signal", "(0-5)", 5),
    ("roam", "(0,1)", 0),
    ("battchg", "(0-5)", 5),
];

pub(super) struct Hfp {
    dlci: u8,
    line: String,
    /// The service level connection is ready.
    slc: bool,
    hf_features: u32,
}

impl Hfp {
    /// The service level connection is ready — voice may be opened.
    pub(super) fn ready(&self) -> bool {
        self.slc
    }
}

impl Host {
    /// SDP: the device has a headset — connect to its port.
    pub(super) fn hfp_sdp(&mut self, handle: u16, uuid: u16, services: &[Service]) {
        if uuid != UUID_HANDSFREE {
            return;
        }
        let Some(ch) = services.iter().find_map(|s| s.rfcomm) else { return };
        if self.conn_mut(handle).is_some_and(|c| c.hfp.is_some()) {
            return;
        }
        self.rfcomm_connect(handle, ch);
    }

    /// The RFCOMM port is open: the headset (our gateway channel or the device's headset channel).
    pub(super) fn port_open(&mut self, handle: u16, channel: u8, dlci: u8) {
        let Some(c) = self.conn_mut(handle) else { return };
        if c.hfp.is_none() {
            c.hfp = Some(Hfp { dlci, line: String::new(), slc: false, hf_features: 0 });
        }
        if let Some(addr) = self.addr_of(handle) {
            kprintln!("bt: {} headset: control channel open (RFCOMM {channel}{})", addr_text(&addr), if channel == CHANNEL_HFP_AG { ", ours" } else { "" });
        }
    }

    pub(super) fn port_closed(&mut self, handle: u16, _channel: u8) {
        if let Some(c) = self.conn_mut(handle) {
            c.hfp = None;
            if let Some(s) = c.sco.as_mut() {
                Host::sco_gone(s);
            }
        }
    }

    /// Port bytes: accumulate until the end of a line, each command goes to `at`.
    pub(super) fn port_data(&mut self, handle: u16, _channel: u8, dlci: u8, data: &[u8]) {
        let Some(h) = self.conn_mut(handle).and_then(|c| c.hfp.as_mut()).filter(|h| h.dlci == dlci) else { return };
        h.line.push_str(&String::from_utf8_lossy(data));
        let mut commands = alloc::vec::Vec::new();
        while let Some(end) = h.line.find(['\r', '\n']) {
            let cmd = String::from(h.line[..end].trim());
            h.line.drain(..=end);
            if !cmd.is_empty() {
                commands.push(cmd);
            }
        }
        for cmd in commands {
            self.at(handle, dlci, &cmd);
        }
    }

    fn at_reply(&mut self, handle: u16, dlci: u8, text: &str) {
        self.rfcomm_send(handle, dlci, format!("\r\n{text}\r\n").as_bytes());
    }

    /// A headset command.
    fn at(&mut self, handle: u16, dlci: u8, cmd: &str) {
        let upper = cmd.to_ascii_uppercase();
        if upper.starts_with("AT+BIEV") || upper.starts_with("AT+BIND") || upper.starts_with("AT+XAPL") || upper.starts_with("AT+IPHONEACCEV") || upper.starts_with("AT+XEVENT") || upper.starts_with("AT+BRSF") {
            kprintln!("bt: headset says {cmd}");
        }
        let ok = |h: &mut Host| h.at_reply(handle, dlci, "OK");
        if let Some(v) = upper.strip_prefix("AT+BRSF=") {
            let features = v.trim().parse().unwrap_or(0);
            if let Some(h) = self.conn_mut(handle).and_then(|c| c.hfp.as_mut()) {
                h.hf_features = features;
            }
            self.at_reply(handle, dlci, &format!("+BRSF: {AG_FEATURES}"));
            ok(self);
        } else if upper == "AT+CIND=?" {
            let list: alloc::vec::Vec<String> = INDICATORS.iter().map(|(n, r, _)| format!("(\"{n}\",{r})")).collect();
            self.at_reply(handle, dlci, &format!("+CIND: {}", list.join(",")));
            ok(self);
        } else if upper == "AT+CIND?" {
            let list: alloc::vec::Vec<String> = INDICATORS.iter().map(|i| format!("{}", i.2)).collect();
            self.at_reply(handle, dlci, &format!("+CIND: {}", list.join(",")));
            ok(self);
        } else if upper.starts_with("AT+CMER=") {
            ok(self);
            let first = self.conn_mut(handle).and_then(|c| c.hfp.as_mut()).map(|h| !core::mem::replace(&mut h.slc, true));
            if first == Some(true)
                && let Some(addr) = self.addr_of(handle)
            {
                kprintln!("bt: {} headset: ready (service level connection)", addr_text(&addr));
            }
            self.sco_headset_ready(handle);
            // Microphone gain and call volume to the maximum (0..15), if
            // the headset allows changing them from our side.
            let remote_volume = self.conn_mut(handle).and_then(|c| c.hfp.as_ref()).is_some_and(|h| h.hf_features & HF_REMOTE_VOLUME != 0);
            if remote_volume {
                self.at_reply(handle, dlci, "+VGM: 15");
                self.at_reply(handle, dlci, "+VGS: 15");
            }
        } else if upper == "AT+CHLD=?" {
            self.at_reply(handle, dlci, "+CHLD: (0,1,2)");
            ok(self);
        } else if upper.starts_with("AT+VGS=") || upper.starts_with("AT+VGM=") {
            // The headset speaker and microphone volume — noted.
            ok(self);
        } else if upper.starts_with("AT+CLIP") || upper.starts_with("AT+CCWA") || upper.starts_with("AT+CMEE") || upper.starts_with("AT+NREC") || upper.starts_with("AT+BIA") || upper.starts_with("AT+BAC") {
            ok(self);
        } else if upper == "AT+BIND=?" {
            self.at_reply(handle, dlci, "+BIND: (1,2)");
            ok(self);
        } else if upper == "AT+BIND?" {
            // Driver safety — not needed; battery — report it.
            self.at_reply(handle, dlci, "+BIND: 1,0");
            self.at_reply(handle, dlci, &format!("+BIND: {HF_BATTERY},1"));
            ok(self);
        } else if upper.starts_with("AT+BIND") {
            ok(self);
        } else if let Some(v) = upper.strip_prefix("AT+BIEV=") {
            // A headset indicator: "2,level" — the battery in percent.
            let mut parts = v.split(',').map(|p| p.trim().parse::<u32>().ok());
            if let (Some(Some(HF_BATTERY)), Some(Some(level))) = (parts.next(), parts.next()) {
                self.battery_level(handle, level.min(100) as u8);
            }
            ok(self);
        } else if upper.starts_with("AT+XAPL=") {
            // The Apple extension: answer that we accept battery messages (bit 1).
            self.at_reply(handle, dlci, "+XAPL=iPhone,2");
            ok(self);
        } else if let Some(v) = upper.strip_prefix("AT+IPHONEACCEV=") {
            // The number of pairs, then "key,value" pairs; key 1 is the battery 0..9.
            let nums: alloc::vec::Vec<u32> = v.split(',').filter_map(|p| p.trim().parse().ok()).collect();
            for pair in nums.get(1..).unwrap_or(&[]).chunks_exact(2) {
                if pair[0] == 1 {
                    self.battery_level(handle, ((pair[1].min(9) + 1) * 10) as u8);
                }
            }
            ok(self);
        } else if let Some(v) = upper.strip_prefix("AT+XEVENT=BATTERY,") {
            // Android/Plantronics: "level,maximum,…".
            let nums: alloc::vec::Vec<u32> = v.split(',').filter_map(|p| p.trim().parse().ok()).collect();
            if let [level, max, ..] = nums[..]
                && max > 0
            {
                self.battery_level(handle, (level * 100 / max).min(100) as u8);
            }
            ok(self);
        } else if upper.starts_with("AT+XEVENT") {
            ok(self);
        } else if upper == "AT+COPS?" {
            self.at_reply(handle, dlci, "+COPS: 0,0,\"GlassOS\"");
            ok(self);
        } else if upper.starts_with("AT+COPS") || upper == "AT+BTRH?" || upper == "AT+CLCC" {
            ok(self);
        } else if upper == "AT+CNUM" {
            ok(self);
        } else {
            if let Some(addr) = self.addr_of(handle) {
                kprintln!("bt: {} headset: {cmd:?} not supported", addr_text(&addr));
            }
            self.at_reply(handle, dlci, "ERROR");
        }
    }
}
