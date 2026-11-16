//! Connecting to an access point: firmware contexts, authentication, association,
//! WPA2 keys and frame transmission. The link mode is chosen by what the access point can do:
//! 802.11ac, 802.11n, or a/g if the fast modes did not work (see `Mode`).

use super::reorder::{Mark, Session};
use super::wpa::{Handshake, RSN, Step};
use super::*;

const ACTION_ADD: u32 = 1;
const ACTION_MODIFY: u32 = 2;
const MAC_TYPE_STA: u32 = 5;
const INVALID: u32 = !0;

const PHY_CONTEXT: u8 = 0x08;
const SESSION_PROTECTION: u8 = 0x05;
const MAC_CONF: u8 = 3;
const RLC_CONFIG: u8 = 0x08;
const RLC_VERSION: u8 = 2;
const SCD_QUEUE_CONFIG: u8 = 0x17;
const TLC_CONFIG: u8 = 0x0F;
const SEC_KEY: u8 = 0x18;
const ADD_STA: u8 = 0x18;
const MAC_CONTEXT: u8 = 0x28;
const BINDING: u8 = 0x2B;
const MCAST_FILTER: u8 = 0xD0;

const RX_CHAIN_VALID_POS: u32 = 1;
const RX_CHAIN_COUNT_POS: u32 = 10;
const RX_CHAIN_MIMO_POS: u32 = 12;
const ADD_STA_OK: u32 = 1;
const STA_ID: u8 = 0;
const STA_MASK: u32 = 1 << STA_ID;
const MGMT_TID: u8 = 15;
const STA_FLAGS_MASK: u32 = 3 << 26 | 3 << 28;
const STA_HT_MASK: u32 = 0xF << 19 | 7 << 23;
const STA_AGG_POS: u32 = 19;
const STA_DENSITY_POS: u32 = 23;
const STA_WIDTH_POS: u32 = 26;
const STA_MIMO2: u32 = 1 << 28;
const STA_MODIFY_ADD_BA: u8 = 1 << 3;
const STA_MODIFY_REMOVE_BA: u8 = 1 << 4;
const STA_BAID_VALID: u32 = 0x8000;
const STA_BAID_POS: u32 = 8;
const STA_BAID_MASK: u32 = 0x7F;
const MAC_PROTECT_HT: u32 = 1 << 23 | 1 << 24;
const MAC_QOS: u32 = 1 | 1 << 1;
const AGG_SIZE_MAX: u32 = 7;
const WIDTH_20: u8 = 0;
const WIDTH_40: u8 = 1;
const WIDTH_80: u8 = 2;
const CONTROL_BELOW: u8 = 0;
const CONTROL_SECOND_BELOW: u8 = 1;
const CONTROL_ABOVE: u8 = 4;
const CONTROL_SECOND_ABOVE: u8 = 5;
const SECONDARY_ABOVE: u8 = 1;
const SECONDARY_BELOW: u8 = 3;
const HT_PROTECT_NON_MEMBER: u8 = 1;
const HT_PROTECT_20MHZ: u8 = 2;
const HT_PROTECT_MIXED: u8 = 3;
const HT_CAP_40: u16 = 1 << 1;
const HT_CAP_SGI20: u16 = 1 << 5;
const HT_CAP_SGI40: u16 = 1 << 6;
const HT_CAP_SM_OFF: u16 = 3 << 2;
const HT_CAP_TX_STBC: u16 = 1 << 7;
const HT_CAP_RX_STBC: u16 = 1 << 8;
const HT_CAP_RX_STBC_MASK: u16 = 3 << 8;
const HT_AMPDU: u8 = 5 << 2 | 3;
const HT_AMPDU_SIZE: u8 = 3;
const HT_DENSITY_POS: u32 = 2;
const HT_DENSITY_MASK: u8 = 7;
const HT_DENSITY_MIN: u8 = 4;
const HT_ELEMENT: usize = 26;
const HT_TX_MCS_DEFINED: usize = 15;
const ELEMENT_HT: u8 = 45;
const ELEMENT_VHT: u8 = 191;
const VHT_SGI80: u32 = 1 << 5;
const VHT_TX_STBC: u32 = 1 << 7;
const VHT_RX_STBC: u32 = 1 << 8;
const VHT_RX_STBC_MASK: u32 = 7 << 8;
const VHT_AMPDU_POS: u32 = 23;
const VHT_AMPDU: u32 = 7 << VHT_AMPDU_POS;
const VHT_MCS_ONE_STREAM: u16 = 0xFFFE;
const VHT_MCS_TWO_STREAMS: u16 = 0xFFFA;
const VHT_MCS_BITS: u32 = 2;
const VHT_MCS_MASK: u16 = 3;
const VHT_MCS_NONE: u16 = 3;
const VHT_WIDTH_80: u8 = 1;
const WMM_ELEMENT: [u8; 9] = [ELEMENT_VENDOR, 7, 0x00, 0x50, 0xF2, 2, 0, 1, 0];
const TLC_MODE_HT: u8 = 1;
const TLC_MODE_VHT: u8 = 2;
const TLC_CHAINS_AB: u8 = 3;
const TLC_STBC: u16 = 1;
const TLC_RATES: usize = 12;
const TLC_RATES_PER_STREAM: usize = 6;
const TLC_MPDU_HT: u16 = 3839;
const TLC_MPDU_VHT: u16 = 3895;
const SF_CONFIG: u8 = 0xD1;
const SF_FULL_ON: u32 = 1;
const SF_MARK_SINGLE: u32 = 4096;
const SF_MARK_MIMO2: u32 = 8192;
const SF_LONG_DELAY: u32 = 1_000_000;
/// The "all enabled" state: (ageing, idle) for each kind of traffic.
const SF_FULL_TIMEOUTS: [(u32, u32); 5] = [(2016, 320), (2016, 320), (10016, 2016), (2016, 320), (2016, 320)];
const BAID_CONFIG: u8 = 0x16;
const BAID_ADD: u32 = 0;
const BAID_REMOVE: u32 = 2;
const BAID_MAX: u32 = 32;
const SESSIONS_MAX: usize = 8;
const BA_WINDOW: u16 = 64;
const REORDER_WAIT_TICKS: u64 = interrupts::HZ / 10;
const CATEGORY_BLOCK_ACK: u8 = 3;
const ACTION_ADDBA_REQUEST: u8 = 0;
const ACTION_ADDBA_RESPONSE: u8 = 1;
const ACTION_DELBA: u8 = 2;
const BA_IMMEDIATE: u16 = 1 << 1;
const BA_TID_POS: u32 = 2;
const BA_TID_MASK: u16 = 0xF;
const BA_WINDOW_POS: u32 = 6;
const BA_SEQUENCE_POS: u32 = 4;
const DELBA_INITIATOR: u16 = 1 << 11;
const DELBA_TID_POS: u32 = 12;
const STATUS_SUCCESS: u16 = 0;
const STATUS_DECLINED: u16 = 37;
const TID_MAX: u8 = 7;
const TID_BEST_EFFORT: usize = 0;
const TID_VOICE: usize = 7;
const REORDER_BAID_POS: u32 = 24;
const REORDER_NUMBER_POS: u32 = 12;
const REORDER_MASK: u32 = 0xFFF;
const REORDER_BAID_MASK: u32 = 0x7F;
const REORDER_OLD: u32 = 1 << 31;
const REORDER_NO_SESSION: u8 = 0x7F;
const RX_REORDER: usize = 16;
const RX_FLAGS_AMSDU: u8 = 0x40;
const RX_AMSDU_INFO: usize = 4;
const RX_AMSDU_LAST: u8 = 0x80;
const QOS_TID_MASK: u8 = 0xF;
const MULTICAST: u8 = 1;
const OFFLOAD_PAD: u16 = 1 << 13;
const FILTER_GROUP: u32 = 1 << 2;
const FILTER_BEACON: u32 = 1 << 6;
const SHORT_PREAMBLE: u32 = 1 << 5;
const SHORT_SLOT: u32 = 1 << 4;
const CAP_SHORT_PREAMBLE: u16 = 1 << 5;
const CAP_SHORT_SLOT: u16 = 1 << 10;
const CAP_REQUEST: u16 = 1 | 1 << 4 | CAP_SHORT_PREAMBLE | CAP_SHORT_SLOT;
/// MAC queues: (queue, cw_min, cw_max, aifsn, txop) as for an ordinary station.
const EDCA: [(usize, u16, u16, u8, u16); 4] = [(2, 15, 1023, 3, 0), (1, 15, 1023, 7, 0), (3, 7, 15, 2, 94 * 32), (4, 3, 7, 2, 47 * 32)];
const LISTEN_INTERVAL: u16 = 10;
const TU_US: u32 = 1024;
const SESSION_TU: u32 = 900;
const TLC_MAX_MPDU: u16 = 2346;
const KEY_CCMP: u32 = 2;
const KEY_GROUP: u32 = 0x40;
/// Rates in units of 500 kbit/s: CCK first, then OFDM.
const RATES: [u8; 12] = [2, 4, 11, 22, 12, 18, 24, 36, 48, 72, 96, 108];
const FIRST_OFDM: usize = 4;
const RATE_BASIC: u8 = 0x80;
const RATE_VALUE: u8 = 0x7F;
const RATE_ANT_A: u32 = 1 << 14;
const RATE_OFDM: u32 = 1 << 8;
const TX_CMD_RATE: u32 = 1;
const TX_NO_ENCRYPT: u32 = 1 << 1;
const TX_HIGH_PRIORITY: u32 = 1 << 2;

const FC_AUTH: u8 = 0xB0;
const FC_ASSOC_REQUEST: u8 = 0x00;
const FC_ASSOC_RESPONSE: u8 = 0x10;
const FC_ACTION: u8 = 0xD0;
const FC_DATA: u8 = 0x08;
const FC_QOS_DATA: u8 = 0x88;
const FC_TO_DS: u8 = 1;
const FC_PROTECTED: u8 = 0x40;
const MAC_HEADER: usize = 24;
const TX_COMMAND: usize = 24;
const CMD_NARROW: usize = 4;
const LLC: [u8; 6] = [0xAA, 0xAA, 3, 0, 0, 0];
const ETHER_HEADER: usize = 14;
const ETHER_EAPOL: u16 = 0x888E;
const STREAM_LIMIT: usize = 200;
const REPLY_MS: u64 = 400;
const TRIES: usize = 4;
const HANDSHAKE_MS: u64 = 6000;
const AUTH_OK: u16 = 0;

/// A transmit queue: the ring, the length table and the frame memory.
pub(super) struct TxRing {
    ring: Dma,
    counts: Dma,
    slots: Vec<Dma>,
    qid: u32,
    next: usize,
    done: usize,
}

impl TxRing {
    pub(super) fn finished(&mut self, position: usize) {
        self.done = position;
    }
}

/// What was chosen for this access point. Until association — plain a/g.
#[derive(Clone, Copy, Default)]
pub(super) struct Link {
    pub(super) mode: Mode,
    width: u8,
    /// Where the primary channel is inside the wide one (`CONTROL_*`).
    control: u8,
    streams: u8,
}

impl Link {
    fn describe(&self) -> String {
        match self.mode {
            Mode::Legacy => "802.11a/g".into(),
            mode => format!("802.11{} {} MHz {}x{}", if mode == Mode::Vht { "ac" } else { "n" }, 20 << self.width, self.streams, self.streams),
        }
    }
}

/// The best that is available with this access point, no higher than `limit`.
fn pick_link(bss: &Network, limit: Mode, tx_chains: u32, rx_chains: u32) -> Link {
    let Some(ht) = bss.ht.filter(|_| limit >= Mode::Ht && bss.wmm) else { return Link::default() };
    let five_ghz = bss.channel > CHANNELS[CHANNELS_2GHZ - 1];
    let forty = five_ghz && ht.wide && ht.caps & HT_CAP_40 != 0 && matches!(ht.secondary, SECONDARY_ABOVE | SECONDARY_BELOW);
    let vht = bss.vht.filter(|_| five_ghz && limit == Mode::Vht);
    let position80 = vht.filter(|v| forty && v.width == VHT_WIDTH_80).and_then(|v| match bss.channel as i32 - v.center as i32 {
        -6 => Some(CONTROL_SECOND_BELOW),
        -2 => Some(CONTROL_BELOW),
        2 => Some(CONTROL_ABOVE),
        6 => Some(CONTROL_SECOND_ABOVE),
        _ => None,
    });
    let (width, control) = match (position80, forty, ht.secondary) {
        (Some(position), ..) => (WIDTH_80, position),
        (None, true, SECONDARY_ABOVE) => (WIDTH_40, CONTROL_BELOW),
        (None, true, _) => (WIDTH_40, CONTROL_ABOVE),
        _ => (WIDTH_20, CONTROL_BELOW),
    };
    let two_ways = (tx_chains & rx_chains).count_ones() >= 2;
    let peer_two = match vht {
        Some(v) => v.mcs >> VHT_MCS_BITS & VHT_MCS_MASK != VHT_MCS_NONE,
        None => ht.mcs[1] != 0,
    };
    let mode = if vht.is_some() { Mode::Vht } else { Mode::Ht };
    Link { mode, width, control, streams: if two_ways && peer_two { 2 } else { 1 } }
}

/// What we can do: the HT and VHT elements for the association request.
fn capability_elements(link: &Link, five_ghz: bool) -> Vec<u8> {
    let two = link.streams == 2;
    let mut ht = [0u8; HT_ELEMENT];
    let wide = if five_ghz { HT_CAP_40 | HT_CAP_SGI40 } else { 0 };
    let caps = HT_CAP_SM_OFF | HT_CAP_SGI20 | HT_CAP_RX_STBC | wide | if two { HT_CAP_TX_STBC } else { 0 };
    ht[..2].copy_from_slice(&caps.to_le_bytes());
    ht[2] = HT_AMPDU;
    ht[3] = 0xFF;
    ht[4] = if two { 0xFF } else { 0 };
    ht[HT_TX_MCS_DEFINED] = 1;
    let mut out = vec![ELEMENT_HT, HT_ELEMENT as u8];
    out.extend_from_slice(&ht);
    if link.mode == Mode::Vht {
        let caps = VHT_SGI80 | VHT_RX_STBC | VHT_AMPDU | if two { VHT_TX_STBC } else { 0 };
        let mcs = if two { VHT_MCS_TWO_STREAMS } else { VHT_MCS_ONE_STREAM };
        out.extend_from_slice(&[ELEMENT_VHT, 12]);
        out.extend_from_slice(&caps.to_le_bytes());
        for value in [mcs, 0, mcs, 0] {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    out
}

/// One-stream VHT rates: MCS bits from the access point's map entry.
fn vht_rates(map: u16, stream: u32, forty: bool) -> u16 {
    match map >> (stream * VHT_MCS_BITS) & VHT_MCS_MASK {
        0 => 0xFF,
        1 => 0x1FF,
        2 if forty => 0x3FF,
        2 => 0x1FF,
        _ => 0,
    }
}

pub(super) fn self_test() -> bool {
    let bss = |channel, ht, vht, wmm| Network {
        ssid: String::new(),
        bssid: [0; 6],
        channel,
        signal: 0,
        security: "",
        usable: true,
        capability: 0,
        interval: 0,
        dtim: 0,
        dtim_left: 0,
        tsf: 0,
        rx_time: 0,
        rates: Vec::new(),
        ht,
        vht,
        wmm,
    };
    let wide = |secondary| Some(Ht { caps: HT_CAP_40, ampdu: 0, mcs: [0xFF, 0xFF], secondary, wide: true, protection: 0 });
    let eighty = |center| Some(Vht { caps: 0, mcs: VHT_MCS_TWO_STREAMS, width: VHT_WIDTH_80, center });
    let chains = |n: &Network, limit| pick_link(n, limit, 3, 3);
    let ac = chains(&bss(48, wide(SECONDARY_BELOW), eighty(42), true), Mode::Vht);
    let first = chains(&bss(36, wide(SECONDARY_ABOVE), eighty(42), true), Mode::Vht);
    let n40 = chains(&bss(36, wide(SECONDARY_ABOVE), eighty(42), true), Mode::Ht);
    let narrow = chains(&bss(6, wide(SECONDARY_ABOVE), None, true), Mode::Vht);
    let no_qos = chains(&bss(6, wide(0), None, false), Mode::Vht);
    let plain = chains(&bss(48, wide(SECONDARY_BELOW), eighty(42), true), Mode::Legacy);
    let elements = capability_elements(&ac, true);
    ac.mode == Mode::Vht
        && (ac.width, ac.control, ac.streams) == (WIDTH_80, CONTROL_SECOND_ABOVE, 2)
        && (first.width, first.control) == (WIDTH_80, CONTROL_SECOND_BELOW)
        && n40.mode == Mode::Ht
        && (n40.width, n40.control) == (WIDTH_40, CONTROL_BELOW)
        && (narrow.mode, narrow.width) == (Mode::Ht, WIDTH_20)
        && no_qos.mode == Mode::Legacy
        && plain.mode == Mode::Legacy
        && elements.len() == 2 + HT_ELEMENT + 2 + 12
        && vht_rates(VHT_MCS_TWO_STREAMS, 1, true) == 0x3FF
        && vht_rates(VHT_MCS_ONE_STREAM, 1, true) == 0
        && super::reorder::self_test()
}

fn rate_index(value: u8) -> Option<usize> {
    RATES.iter().position(|&r| r == value & RATE_VALUE)
}

fn basic_indexes(bss: &Network) -> impl Iterator<Item = usize> + '_ {
    bss.rates.iter().filter(|r| **r & RATE_BASIC != 0).filter_map(|&r| rate_index(r))
}

/// Acknowledgement rates: the basic ones and all mandatory ones below them.
fn ack_rates(bss: &Network) -> (u32, u32) {
    const MANDATORY_CCK: [usize; 4] = [0, 1, 2, 3];
    const MANDATORY_OFDM: [usize; 3] = [FIRST_OFDM, FIRST_OFDM + 2, FIRST_OFDM + 4];
    let (mut cck, mut ofdm) = (0u32, 0u32);
    let is_2ghz = bss.channel <= CHANNELS[CHANNELS_2GHZ - 1];
    for i in basic_indexes(bss) {
        if i >= FIRST_OFDM {
            ofdm |= 1 << (i - FIRST_OFDM);
        } else if is_2ghz {
            cck |= 1 << i;
        }
    }
    let lowest = |bits: u32| if bits == 0 { 0 } else { bits.trailing_zeros() as usize };
    for m in MANDATORY_OFDM {
        if m - FIRST_OFDM < lowest(ofdm) {
            ofdm |= 1 << (m - FIRST_OFDM);
        }
    }
    for m in MANDATORY_CCK {
        if m < lowest(cck) {
            cck |= 1 << m;
        }
    }
    (cck, ofdm | 1)
}

/// The management frame rate: the lowest of the basic ones.
fn management_rate(bss: &Network) -> u32 {
    let index = basic_indexes(bss).min().or_else(|| bss.rates.iter().filter_map(|&r| rate_index(r)).min()).unwrap_or(FIRST_OFDM);
    RATE_ANT_A | if index >= FIRST_OFDM { RATE_OFDM | (index - FIRST_OFDM) as u32 } else { index as u32 }
}

fn word(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

impl Card {
    fn phy_context(&mut self, bss: &Network, action: u32) -> Result<(), String> {
        let mut cmd = [0u8; 32];
        word(&mut cmd, 4, action);
        word(&mut cmd, 8, bss.channel as u32);
        cmd[12] = if bss.channel <= CHANNELS[CHANNELS_2GHZ - 1] { BAND_FLAG_2GHZ } else { BAND_FLAG_5GHZ };
        cmd[13] = self.link.width;
        cmd[14] = self.link.control;
        self.send(LONG, PHY_CONTEXT, &cmd)?;
        let mut rlc = [0u8; 32];
        let chains = self.link.streams.max(1) as u32;
        word(&mut rlc, 4, self.rx_chains << RX_CHAIN_VALID_POS | chains << RX_CHAIN_COUNT_POS | chains << RX_CHAIN_MIMO_POS);
        self.send_versioned(DATA_PATH, RLC_CONFIG, RLC_VERSION, &rlc).map(|_| ())
    }

    fn mac_context(&mut self, bss: &Network, action: u32, aid: u16) -> Result<(), String> {
        const AC: usize = 60;
        const STA: usize = 100;
        let mut cmd = [0u8; 148];
        word(&mut cmd, 4, action);
        word(&mut cmd, 8, MAC_TYPE_STA);
        cmd[16..22].copy_from_slice(&self.mac);
        cmd[24..30].copy_from_slice(&bss.bssid);
        let (cck, ofdm) = ack_rates(bss);
        word(&mut cmd, 32, cck);
        word(&mut cmd, 36, ofdm);
        let is_5ghz = bss.channel > CHANNELS[CHANNELS_2GHZ - 1];
        word(&mut cmd, 44, if bss.capability & CAP_SHORT_PREAMBLE != 0 { SHORT_PREAMBLE } else { 0 });
        word(&mut cmd, 48, if is_5ghz || bss.capability & CAP_SHORT_SLOT != 0 { SHORT_SLOT } else { 0 });
        if self.link.mode != Mode::Legacy {
            word(&mut cmd, 56, MAC_QOS);
            let protection = bss.ht.map_or(0, |ht| ht.protection);
            if matches!(protection, HT_PROTECT_NON_MEMBER | HT_PROTECT_MIXED) || (protection == HT_PROTECT_20MHZ && self.link.width != WIDTH_20) {
                word(&mut cmd, 40, MAC_PROTECT_HT);
            }
        }
        let dtim = bss.dtim.max(1) as u32;
        word(&mut cmd, 52, FILTER_GROUP | if aid == 0 || bss.dtim == 0 { FILTER_BEACON } else { 0 });
        for (fifo, cw_min, cw_max, aifsn, txop) in EDCA {
            let at = AC + fifo * 8;
            cmd[at..at + 2].copy_from_slice(&cw_min.to_le_bytes());
            cmd[at + 2..at + 4].copy_from_slice(&cw_max.to_le_bytes());
            cmd[at + 4] = aifsn;
            cmd[at + 5] = 1 << fifo;
            cmd[at + 6..at + 8].copy_from_slice(&txop.to_le_bytes());
        }
        let interval = bss.interval as u32;
        if aid != 0 {
            let to_dtim = bss.dtim_left as u32 * interval * TU_US;
            word(&mut cmd, STA, 1);
            word(&mut cmd, STA + 4, bss.rx_time + to_dtim);
            cmd[STA + 8..STA + 16].copy_from_slice(&(bss.tsf + to_dtim as u64).to_le_bytes());
            word(&mut cmd, STA + 40, bss.rx_time);
        }
        word(&mut cmd, STA + 16, interval);
        word(&mut cmd, STA + 24, interval * dtim);
        word(&mut cmd, STA + 32, LISTEN_INTERVAL as u32);
        word(&mut cmd, STA + 36, aid as u32);
        self.send(LONG, MAC_CONTEXT, &cmd).map(|_| ())
    }

    fn binding(&mut self) -> Result<(), String> {
        let mut cmd = [0u8; 28];
        word(&mut cmd, 4, ACTION_ADD);
        word(&mut cmd, 12, INVALID);
        word(&mut cmd, 16, INVALID);
        let reply = self.send(LONG, BINDING, &cmd)?;
        if u32_at(&reply, 0) != 0 {
            return Err(format!("binding refused, status {:#x}", u32_at(&reply, 0)));
        }
        Ok(())
    }

    fn station(&mut self, bss: &Network, update: bool) -> Result<(), String> {
        let mut cmd = [0u8; 48];
        cmd[0] = update as u8;
        if !update {
            cmd[8..14].copy_from_slice(&bss.bssid);
        }
        cmd[16] = STA_ID;
        word(&mut cmd, 24, STA_FLAGS_MASK);
        if let (Mode::Ht | Mode::Vht, Some(ht)) = (self.link.mode, bss.ht) {
            word(&mut cmd, 24, STA_FLAGS_MASK | STA_HT_MASK);
            let size = match bss.vht {
                Some(vht) if self.link.mode == Mode::Vht => vht.caps >> VHT_AMPDU_POS & AGG_SIZE_MAX,
                _ => (ht.ampdu & HT_AMPDU_SIZE) as u32,
            };
            let density = ht.ampdu >> HT_DENSITY_POS & HT_DENSITY_MASK;
            let density = if density >= HT_DENSITY_MIN { density as u32 } else { 0 };
            let streams = if self.link.streams == 2 { STA_MIMO2 } else { 0 };
            word(&mut cmd, 20, streams | (self.link.width as u32) << STA_WIDTH_POS | size << STA_AGG_POS | density << STA_DENSITY_POS);
        }
        let reply = self.send(LONG, ADD_STA, &cmd)?;
        let status = u32_at(&reply, 0) & 0xFF;
        if status != ADD_STA_OK {
            return Err(format!("station refused, status {status:#x}"));
        }
        Ok(())
    }

    fn tx_queue(&mut self) -> Result<(), String> {
        const COUNTS_SIZE: usize = 640;
        const COUNTS_ALIGN: usize = 64;
        const QUEUE_ORDER: u32 = TX_COUNT.trailing_zeros() - 3;
        let ring = Dma::new(TX_COUNT * TFD_SIZE, TFD_ALIGN)?;
        let counts = Dma::new(COUNTS_SIZE, COUNTS_ALIGN)?;
        let slots = (0..TX_COUNT).map(|_| Dma::new(PAGE, PAGE)).collect::<Result<Vec<_>, _>>()?;
        let mut cmd = [0u8; 36];
        word(&mut cmd, 4, STA_MASK);
        cmd[8] = MGMT_TID;
        word(&mut cmd, 16, QUEUE_ORDER);
        cmd[20..28].copy_from_slice(&counts.addr().to_le_bytes());
        cmd[28..36].copy_from_slice(&ring.addr().to_le_bytes());
        let reply = self.send(DATA_PATH, SCD_QUEUE_CONFIG, &cmd)?;
        let (qid, position) = (u16_at(&reply, 0) as u32, u16_at(&reply, 4) as usize);
        crate::kprintln!("wifi: transmit queue {qid}, position {position}");
        self.tx = Some(TxRing { ring, counts, slots, qid, next: position % TX_COUNT, done: position % TX_COUNT });
        Ok(())
    }

    fn protect_session(&mut self) -> Result<(), String> {
        let mut cmd = [0u8; 24];
        word(&mut cmd, 4, ACTION_ADD);
        word(&mut cmd, 12, SESSION_TU);
        self.send(MAC_CONF, SESSION_PROTECTION, &cmd).map(|_| ())
    }

    fn rate_control(&mut self, bss: &Network) -> Result<(), String> {
        let link = self.link;
        let mut cmd = [0u8; 28];
        cmd[0] = STA_ID;
        cmd[4] = link.width;
        cmd[6] = if link.streams == 2 { TLC_CHAINS_AB } else { 1 };
        let rates = bss.rates.iter().filter_map(|&r| rate_index(r)).fold(0u16, |bits, i| bits | 1 << i);
        cmd[10..12].copy_from_slice(&rates.to_le_bytes());
        let mut mpdu = TLC_MAX_MPDU;
        if let (Mode::Ht | Mode::Vht, Some(ht)) = (link.mode, bss.ht) {
            let mut sgi = 0;
            if ht.caps & HT_CAP_SGI20 != 0 {
                sgi |= 1 << WIDTH_20;
            }
            if ht.caps & HT_CAP_SGI40 != 0 {
                sgi |= 1 << WIDTH_40;
            }
            let mut stbc = ht.caps & HT_CAP_RX_STBC_MASK != 0;
            let mut rates = [ht.mcs[0] as u16, ht.mcs[1] as u16];
            cmd[5] = TLC_MODE_HT;
            mpdu = TLC_MPDU_HT;
            if let (Mode::Vht, Some(vht)) = (link.mode, bss.vht) {
                cmd[5] = TLC_MODE_VHT;
                mpdu = TLC_MPDU_VHT;
                let forty = link.width != WIDTH_20;
                rates = [vht_rates(vht.mcs, 0, forty), vht_rates(vht.mcs, 1, forty)];
                stbc = vht.caps & VHT_RX_STBC_MASK != 0;
                if vht.caps & VHT_SGI80 != 0 {
                    sgi |= 1 << WIDTH_80;
                }
            }
            cmd[7] = sgi;
            if stbc && link.streams == 2 {
                cmd[8..10].copy_from_slice(&TLC_STBC.to_le_bytes());
            }
            for (stream, value) in rates.iter().take(link.streams as usize).enumerate() {
                let at = TLC_RATES + stream * TLC_RATES_PER_STREAM;
                cmd[at..at + 2].copy_from_slice(&value.to_le_bytes());
            }
        }
        cmd[24..26].copy_from_slice(&mpdu.to_le_bytes());
        // The firmware does not have to answer this command.
        self.post(DATA_PATH, TLC_CONFIG, 0, &cmd)
    }

    fn pass_multicast(&mut self, bss: &Network) -> Result<(), String> {
        let mut cmd = [0u8; 12];
        cmd[0] = 1;
        cmd[3] = 1;
        cmd[4..10].copy_from_slice(&bss.bssid);
        self.send(LONG, MCAST_FILTER, &cmd).map(|_| ())
    }

    fn install_key(&mut self, id: u8, key: &[u8; 16], group: bool) -> Result<(), String> {
        let mut cmd = [0u8; 80];
        word(&mut cmd, 0, ACTION_ADD);
        word(&mut cmd, 4, STA_MASK);
        word(&mut cmd, 8, id as u32);
        word(&mut cmd, 12, KEY_CCMP | if group { KEY_GROUP } else { 0 });
        cmd[16..32].copy_from_slice(key);
        self.send(DATA_PATH, SEC_KEY, &cmd).map(|_| ())
    }

    /// Fast receive: the firmware needs a large buffer, otherwise bursts of frames do not fit.
    fn full_fifo(&mut self, bss: &Network) -> Result<(), String> {
        let mut cmd = [0u8; 92];
        word(&mut cmd, 0, SF_FULL_ON);
        word(&mut cmd, 4, SF_MARK_SINGLE);
        let mimo = bss.ht.is_some_and(|ht| ht.mcs[1] != 0);
        word(&mut cmd, 8, if mimo { SF_MARK_MIMO2 } else { SF_MARK_SINGLE });
        for (i, (aging, idle)) in SF_FULL_TIMEOUTS.iter().enumerate() {
            word(&mut cmd, 12 + i * 8, SF_LONG_DELAY);
            word(&mut cmd, 16 + i * 8, SF_LONG_DELAY);
            word(&mut cmd, 52 + i * 8, *aging);
            word(&mut cmd, 56 + i * 8, *idle);
        }
        self.post(LONG, SF_CONFIG, 0, &cmd)
    }

    /// Negotiates block receive with the firmware. Returns its agreement number.
    fn open_session(&mut self, tid: u8, first: u16, window: u16) -> Result<Option<u8>, String> {
        let baid = if flag(&self.fw.capabilities, CAPA_BAID) {
            let mut cmd = [0u8; 16];
            word(&mut cmd, 0, BAID_ADD);
            word(&mut cmd, 4, STA_MASK);
            cmd[8] = tid;
            cmd[12..14].copy_from_slice(&first.to_le_bytes());
            cmd[14..16].copy_from_slice(&window.to_le_bytes());
            let reply = self.send(DATA_PATH, BAID_CONFIG, &cmd)?;
            Some(u32_at(&reply, 0)).filter(|baid| *baid < BAID_MAX)
        } else {
            let mut cmd = [0u8; 48];
            cmd[0] = 1;
            cmd[16] = STA_ID;
            cmd[17] = STA_MODIFY_ADD_BA;
            cmd[28] = tid;
            cmd[30..32].copy_from_slice(&first.to_le_bytes());
            cmd[44..46].copy_from_slice(&window.to_le_bytes());
            let reply = self.send(LONG, ADD_STA, &cmd)?;
            let status = u32_at(&reply, 0);
            let accepted = status & 0xFF == ADD_STA_OK && status & STA_BAID_VALID != 0;
            Some(status >> STA_BAID_POS & STA_BAID_MASK).filter(|baid| accepted && *baid < BAID_MAX)
        };
        Ok(baid.map(|b| b as u8))
    }

    fn close_session(&mut self, session: &Session) -> Result<(), String> {
        if flag(&self.fw.capabilities, CAPA_BAID) {
            let mut cmd = [0u8; 16];
            word(&mut cmd, 0, BAID_REMOVE);
            if self.fw.version_of(DATA_PATH, BAID_CONFIG) == Some(1) {
                word(&mut cmd, 4, session.baid as u32);
            } else {
                word(&mut cmd, 4, STA_MASK);
                word(&mut cmd, 8, session.tid as u32);
            }
            self.send(DATA_PATH, BAID_CONFIG, &cmd).map(|_| ())
        } else {
            let mut cmd = [0u8; 48];
            cmd[0] = 1;
            cmd[16] = STA_ID;
            cmd[17] = STA_MODIFY_REMOVE_BA;
            cmd[29] = session.tid;
            self.send(LONG, ADD_STA, &cmd).map(|_| ())
        }
    }

    /// The access point offers to send in blocks.
    fn block_ack_request(&mut self, body: &[u8]) -> Result<(), String> {
        const REQUEST_SIZE: usize = 9;
        if body.len() < REQUEST_SIZE {
            return Ok(());
        }
        let token = body[2];
        let (params, sequence) = (u16_at(body, 3), u16_at(body, 7));
        let tid = (params >> BA_TID_POS & BA_TID_MASK) as u8;
        let asked = params >> BA_WINDOW_POS;
        let window = if asked == 0 { BA_WINDOW } else { asked.min(BA_WINDOW) };
        let first = sequence >> BA_SEQUENCE_POS;
        let known = self.sessions.iter().any(|s| s.tid == tid);
        let allowed = self.link.mode != Mode::Legacy && tid <= TID_MAX && params & BA_IMMEDIATE != 0 && (known || self.sessions.len() < SESSIONS_MAX);
        let mut accepted = known && allowed;
        if allowed && !known && let Some(baid) = self.open_session(tid, first, window)? {
            self.sessions.push(Session::new(tid, baid, first, window as usize));
            accepted = true;
            crate::kprintln!("wifi: receiving in blocks, tid {tid}, window {window}, from {first}");
        }
        // A-MSDU parts inside a block are not accepted: the bit in the parameters stays zero.
        let answer = BA_IMMEDIATE | (tid as u16) << BA_TID_POS | window << BA_WINDOW_POS;
        let status = if accepted { STATUS_SUCCESS } else { STATUS_DECLINED };
        let mut reply = vec![CATEGORY_BLOCK_ACK, ACTION_ADDBA_RESPONSE, token];
        reply.extend_from_slice(&status.to_le_bytes());
        reply.extend_from_slice(&answer.to_le_bytes());
        reply.extend_from_slice(&0u16.to_le_bytes());
        let bssid = self.bssid;
        let h = self.header([FC_ACTION, 0], &bssid, &bssid);
        self.transmit(&h, &reply, TX_CMD_RATE | TX_NO_ENCRYPT, self.management_rate);
        Ok(())
    }

    /// The access point stopped sending in blocks.
    fn block_ack_end(&mut self, body: &[u8]) -> Result<(), String> {
        const END_SIZE: usize = 6;
        if body.len() < END_SIZE {
            return Ok(());
        }
        let params = u16_at(body, 2);
        let tid = (params >> DELBA_TID_POS) as u8;
        if params & DELBA_INITIATOR == 0 {
            return Ok(());
        }
        if let Some(i) = self.sessions.iter().position(|s| s.tid == tid) {
            let mut session = self.sessions.remove(i);
            let mut out = Vec::new();
            session.expire(u64::MAX, 0, &mut out);
            self.deliver(out);
            self.close_session(&session)?;
            crate::kprintln!("wifi: block session ended, tid {tid}");
        }
        Ok(())
    }

    /// Management frames from the access point that arrived during operation.
    pub(super) fn drain_actions(&mut self) -> Result<(), String> {
        for frame in core::mem::take(&mut self.management) {
            if frame[0] != FC_ACTION || frame.len() < MAC_HEADER + 2 {
                continue;
            }
            let body = &frame[MAC_HEADER..];
            match (body[0], body[1]) {
                (CATEGORY_BLOCK_ACK, ACTION_ADDBA_REQUEST) => self.block_ack_request(body)?,
                (CATEGORY_BLOCK_ACK, ACTION_DELBA) => self.block_ack_end(body)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn deliver(&mut self, frames: Vec<Vec<u8>>) {
        self.delivered += frames.len() as u64;
        let mut queue = RX_FRAMES.lock();
        for frame in frames {
            if queue.len() < QUEUE_MAX {
                queue.push_back(frame);
            }
        }
    }

    /// A network frame goes through the reorder buffer if there is an agreement for it.
    pub(super) fn sort_frame(&mut self, desc: &[u8], frame: &[u8], carried: Option<Vec<u8>>) {
        let order = u32_at(desc, RX_REORDER);
        let baid = (order >> REORDER_BAID_POS & REORDER_BAID_MASK) as u8;
        let mut out = Vec::new();
        let qos = frame[0] & SUBTYPE_QOS != 0;
        let unicast = frame[FRAME_DESTINATION] & MULTICAST == 0;
        let session = self.sessions.iter_mut().find(|s| s.baid == baid && qos && unicast && baid != REORDER_NO_SESSION && s.tid == frame[FRAME_HEADER] & QOS_TID_MASK);
        match session {
            Some(session) => {
                let mark = Mark {
                    number: (order >> REORDER_NUMBER_POS & REORDER_MASK) as u16,
                    nssn: (order & REORDER_MASK) as u16,
                    old: order & REORDER_OLD != 0,
                    partial: desc[RX_FLAGS2] & RX_FLAGS_AMSDU != 0 && desc[RX_AMSDU_INFO] & RX_AMSDU_LAST == 0,
                };
                session.receive(carried, &mark, interrupts::ticks(), &mut out);
            }
            None => out.extend(carried),
        }
        self.deliver(out);
    }

    /// The firmware released frames up to this number.
    pub(super) fn release_frames(&mut self, baid: u8, nssn: u16) {
        let mut out = Vec::new();
        if let Some(session) = self.sessions.iter_mut().find(|s| s.baid == baid) {
            session.release(nssn & REORDER_MASK as u16, interrupts::ticks(), &mut out);
        }
        self.deliver(out);
    }

    /// Releases frames that have waited for a lost one longer than reasonable.
    pub(super) fn expire_frames(&mut self) {
        let now = interrupts::ticks();
        let mut out = Vec::new();
        for session in &mut self.sessions {
            session.expire(now.saturating_sub(REORDER_WAIT_TICKS), now, &mut out);
        }
        if !out.is_empty() {
            self.expired += 1;
            self.deliver(out);
        }
    }

    /// Counters for the log: they show where the throughput is lost.
    pub(super) fn counters(&self, speed: u32) -> String {
        let [cck, ofdm, ht, vht, he] = self.rx_kinds;
        format!(
            "{} frames to the network, to us cck {cck} ofdm {ofdm} ht {ht} vht {vht} he {he}, last {speed} Mbit/s rate {:#x}, block sessions {}, waits expired {}, not delivered {}",
            self.delivered,
            self.last_rate,
            self.sessions.len(),
            self.expired,
            self.tx_failed
        )
    }

    /// What was on the air since the last report: at which rates the frames went (share in percent),
    /// how many of them were retries and the signal on each antenna. An empty counter resets the report.
    pub(super) fn air_report(&mut self) -> String {
        let total: u64 = self.rate_frames.values().sum();
        if total == 0 {
            return "air: no frames to us".into();
        }
        let mut rates: Vec<(u32, u64)> = self.rate_frames.iter().map(|(&r, &n)| (r, n)).collect();
        rates.sort_by_key(|&(_, n)| core::cmp::Reverse(n));
        let list: Vec<String> = rates.iter().take(6).map(|(r, n)| format!("{r}:{}%", n * 100 / total)).collect();
        super::AIR.store(
            (self.rx_energy[0].max(self.rx_energy[1]) / total) << 8 | (self.rx_retried * 100 / total).min(100),
            core::sync::atomic::Ordering::Relaxed,
        );
        let report = format!(
            "air: {total} frames, Mbit/s {}, retried {}%, signal -{} and -{} dBm",
            list.join(" "),
            self.rx_retried * 100 / total,
            self.rx_energy[0] / total,
            self.rx_energy[1] / total
        );
        self.rate_frames.clear();
        self.rx_retried = 0;
        self.rx_energy = [0; 2];
        report
    }

    /// A frame to the queue. false — the queue is full.
    fn transmit(&mut self, header: &[u8], body: &[u8], flags: u32, rate: u32) -> bool {
        let Some(tx) = &mut self.tx else { return false };
        // The first piece is a multiple of 4: the QoS header needs padding.
        let padded = header.len().next_multiple_of(4);
        let waiting = (tx.next + TX_COUNT - tx.done) % TX_COUNT;
        if waiting >= STREAM_LIMIT || TX_COMMAND + padded + body.len() > PAGE {
            return false;
        }
        let idx = tx.next;
        let total = (header.len() + body.len()) as u16;
        let slot = &tx.slots[idx];
        slot.put(0, &[TX_CMD, 0, idx as u8, tx.qid as u8]);
        slot.put(CMD_NARROW, &total.to_le_bytes());
        let pad = if padded != header.len() { OFFLOAD_PAD } else { 0 };
        slot.put(CMD_NARROW + 2, &(((header.len() / 2) as u16) << 8 | pad).to_le_bytes());
        slot.put(CMD_NARROW + 4, &flags.to_le_bytes());
        slot.put(CMD_NARROW + 8, &[0; 8]);
        slot.put(CMD_NARROW + 16, &rate.to_le_bytes());
        slot.put(TX_COMMAND, header);
        slot.put(TX_COMMAND + padded, body);
        let tfd = idx * TFD_SIZE;
        let base = slot.addr();
        tx.ring.put(tfd, &[0; TFD_SIZE]);
        tx.ring.put(tfd, &3u16.to_le_bytes());
        tx.ring.put(tfd + 2, &(FIRST_TB as u16).to_le_bytes());
        tx.ring.put_u64(tfd + 4, base);
        tx.ring.put(tfd + 12, &((TX_COMMAND + padded - FIRST_TB) as u16).to_le_bytes());
        tx.ring.put_u64(tfd + 14, base + FIRST_TB as u64);
        tx.ring.put(tfd + 22, &(body.len() as u16).to_le_bytes());
        tx.ring.put_u64(tfd + 24, base + (TX_COMMAND + padded) as u64);
        tx.counts.put(idx * 2, &(total.div_ceil(4)).to_le_bytes());
        tx.next = (idx + 1) % TX_COUNT;
        let (qid, next) = (tx.qid, tx.next as u32);
        self.wr(TX_WRITE_INDEX, qid << 16 | next);
        true
    }

    fn header(&mut self, control: [u8; 2], bssid: &[u8; 6], third: &[u8; 6]) -> [u8; MAC_HEADER] {
        let mut h = [0u8; MAC_HEADER];
        h[..2].copy_from_slice(&control);
        h[4..10].copy_from_slice(bssid);
        h[10..16].copy_from_slice(&self.mac);
        h[16..22].copy_from_slice(third);
        h[22..24].copy_from_slice(&(self.sequence << 4).to_le_bytes());
        self.sequence = (self.sequence + 1) & 0xFFF;
        h
    }

    fn send_management(&mut self, bss: &Network, kind: u8, body: &[u8]) -> bool {
        let h = self.header([kind, 0], &bss.bssid, &bss.bssid);
        self.transmit(&h, body, TX_CMD_RATE | TX_NO_ENCRYPT, management_rate(bss))
    }

    /// Waits for a management frame of this kind from the access point.
    fn wait_management(&mut self, kind: u8) -> Result<Option<Vec<u8>>, String> {
        let end = interrupts::ticks() + REPLY_MS * interrupts::HZ / 1000;
        while interrupts::ticks() < end {
            self.service()?;
            if let Some(i) = self.management.iter().position(|f| f[0] == kind) {
                return Ok(Some(self.management.remove(i)));
            }
            task::sleep_until(interrupts::ticks() + 1);
        }
        Ok(None)
    }

    fn authenticate(&mut self, bss: &Network) -> Result<(), String> {
        const OPEN_REQUEST: [u8; 6] = [0, 0, 1, 0, 0, 0];
        for _ in 0..TRIES {
            self.send_management(bss, FC_AUTH, &OPEN_REQUEST);
            if let Some(frame) = self.wait_management(FC_AUTH)? {
                let status = u16_at(&frame, MAC_HEADER + 4);
                if status != AUTH_OK {
                    return Err(format!("the access point refused authentication, status {status}"));
                }
                crate::kprintln!("wifi: authenticated");
                return Ok(());
            }
        }
        Err("no answer to authentication".into())
    }

    fn associate(&mut self, bss: &Network, link: &Link) -> Result<u16, String> {
        const AID_MASK: u16 = 0x3FFF;
        const BASIC_RATES_MAX: usize = 8;
        let mut body = Vec::new();
        body.extend_from_slice(&(bss.capability & CAP_REQUEST).to_le_bytes());
        body.extend_from_slice(&LISTEN_INTERVAL.to_le_bytes());
        body.extend_from_slice(&[ELEMENT_SSID, bss.ssid.len() as u8]);
        body.extend_from_slice(bss.ssid.as_bytes());
        let known: Vec<u8> = bss.rates.iter().copied().filter(|&r| rate_index(r).is_some()).collect();
        let (first, rest) = known.split_at(known.len().min(BASIC_RATES_MAX));
        body.extend_from_slice(&[ELEMENT_RATES, first.len() as u8]);
        body.extend_from_slice(first);
        if !rest.is_empty() {
            body.extend_from_slice(&[ELEMENT_MORE_RATES, rest.len() as u8]);
            body.extend_from_slice(rest);
        }
        body.extend_from_slice(&RSN);
        if link.mode != Mode::Legacy {
            body.extend_from_slice(&capability_elements(link, bss.channel > CHANNELS[CHANNELS_2GHZ - 1]));
            body.extend_from_slice(&WMM_ELEMENT);
        }
        for _ in 0..TRIES {
            self.send_management(bss, FC_ASSOC_REQUEST, &body);
            if let Some(frame) = self.wait_management(FC_ASSOC_RESPONSE)? {
                let status = u16_at(&frame, MAC_HEADER + 2);
                if status != 0 {
                    return Err(format!("the access point refused association, status {status}"));
                }
                let aid = u16_at(&frame, MAC_HEADER + 4) & AID_MASK;
                crate::kprintln!("wifi: associated, id {aid}");
                return Ok(aid);
            }
        }
        Err("no answer to association".into())
    }

    /// A data frame from us to the access point.
    fn send_data(&mut self, bssid: &[u8; 6], destination: &[u8; 6], ether_type: u16, payload: &[u8], urgent: bool) -> bool {
        let protect = self.keys_ready;
        let mut body = Vec::with_capacity(LLC.len() + 2 + payload.len());
        body.extend_from_slice(&LLC);
        body.extend_from_slice(&ether_type.to_be_bytes());
        body.extend_from_slice(payload);
        let flags = if protect { FC_TO_DS | FC_PROTECTED } else { FC_TO_DS };
        let tx = if protect { 0 } else { TX_NO_ENCRYPT } | if urgent { TX_HIGH_PRIORITY } else { 0 };
        if self.link.mode == Mode::Legacy {
            let h = self.header([FC_DATA, flags], bssid, destination);
            return self.transmit(&h, &body, tx, 0);
        }
        // With QoS each TID has its own sequence numbers.
        let tid = if urgent { TID_VOICE } else { TID_BEST_EFFORT };
        let mut h = [0u8; MAC_HEADER + QOS_FIELD];
        h[..MAC_HEADER].copy_from_slice(&self.header([FC_QOS_DATA, flags], bssid, destination));
        h[22..24].copy_from_slice(&(self.qos_sequence[tid] << 4).to_le_bytes());
        h[MAC_HEADER] = tid as u8;
        self.qos_sequence[tid] = (self.qos_sequence[tid] + 1) & 0xFFF;
        self.transmit(&h, &body, tx, 0)
    }

    /// Handles the handshake frames accumulated during receive.
    pub(super) fn drain_eapol(&mut self) -> Result<(), String> {
        while self.handshake.is_some() && !self.eapol.is_empty() {
            let frame = self.eapol.remove(0);
            let Some(handshake) = &mut self.handshake else { return Ok(()) };
            let bssid = self.bssid;
            match handshake.key_frame(&frame).map_err(String::from)? {
                Step::Reply(reply) => {
                    crate::kprintln!("wifi: handshake step 1 of 2");
                    self.send_data(&bssid, &bssid, ETHER_EAPOL, &reply, true);
                }
                Step::Keys { reply, pairwise, group } => {
                    crate::kprintln!("wifi: handshake step 2 of 2");
                    self.send_data(&bssid, &bssid, ETHER_EAPOL, &reply, true);
                    self.install_key(0, &pairwise, false)?;
                    self.install_key(group.0, &group.1, true)?;
                    self.keys_ready = true;
                }
                Step::Group { reply, group } => {
                    self.send_data(&bssid, &bssid, ETHER_EAPOL, &reply, true);
                    self.install_key(group.0, &group.1, true)?;
                }
                Step::Ignore => {}
            }
        }
        Ok(())
    }

    /// The whole path to a working link.
    pub(super) fn join(&mut self, bss: &Network, pmk: [u8; 32], limit: Mode) -> Result<(), String> {
        self.bssid = bss.bssid;
        self.management_rate = management_rate(bss);
        crate::kprintln!("wifi: joining channel {}, signal {} dBm, rates {:02x?}", bss.channel, bss.signal, bss.rates);
        // The firmware finishes scanning after its last message about it.
        task::sleep_until(interrupts::ticks() + interrupts::HZ / 10);
        self.service()?;
        self.lock()?;
        self.phy_context(bss, ACTION_ADD)?;
        self.mac_context(bss, ACTION_ADD, 0)?;
        self.binding()?;
        self.station(bss, false)?;
        self.tx_queue()?;
        self.protect_session()?;
        task::sleep_until(interrupts::ticks() + interrupts::HZ / 10);
        self.authenticate(bss)?;
        let link = pick_link(bss, limit, self.tx_chains, self.rx_chains);
        let aid = self.associate(bss, &link)?;
        self.link = link;
        crate::kprintln!("wifi: mode {}, primary channel {}", link.describe(), bss.channel);
        if link.mode != Mode::Legacy {
            self.phy_context(bss, ACTION_MODIFY)?;
        }
        crate::kprintln!("wifi: channel settings applied");
        self.station(bss, true)?;
        crate::kprintln!("wifi: station updated");
        let latest = self.networks.get(&bss.bssid).cloned().unwrap_or_else(|| bss.clone());
        self.mac_context(&latest, ACTION_MODIFY, aid)?;
        crate::kprintln!("wifi: address settings applied");
        if link.mode != Mode::Legacy {
            self.full_fifo(bss)?;
        }
        self.pass_multicast(bss)?;
        crate::kprintln!("wifi: receive filter set");
        self.rate_control(bss)?;
        // The handshake only now: its first frame may already have arrived, and we may answer
        // once the firmware picks the rate itself.
        self.handshake = Some(Handshake::new(pmk, self.mac, bss.bssid));
        self.wait("the handshake", HANDSHAKE_MS, |c| c.keys_ready).map_err(|_| String::from("no handshake, the password is probably wrong"))?;
        crate::kprintln!("wifi: keys installed, link is up");
        Ok(())
    }

    /// Connected faster than a/g.
    pub(super) fn fast(&self) -> bool {
        self.link.mode != Mode::Legacy
    }

    /// The chosen mode for people.
    pub(super) fn link_text(&self) -> String {
        self.link.describe()
    }

    /// The work loop of one pass: frames from the network to the air.
    pub(super) fn forward(&mut self) -> Result<bool, String> {
        let mut busy = false;
        let bssid = self.bssid;
        loop {
            let Some(frame) = TX_FRAMES.lock().pop_front() else { break };
            busy = true;
            if frame.len() < ETHER_HEADER {
                continue;
            }
            let mut destination = [0u8; 6];
            destination.copy_from_slice(&frame[..6]);
            let ether_type = u16::from_be_bytes([frame[12], frame[13]]);
            self.send_data(&bssid, &destination, ether_type, &frame[ETHER_HEADER..], false);
        }
        Ok(busy)
    }
}
