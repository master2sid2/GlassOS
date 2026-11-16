//! SDP — the classic Bluetooth service directory. The server: our records (what we can do —
//! the A2DP audio source, …); a device asks for them before opening a channel. The client:
//! we ask the device (headphones — whether there is an audio sink and a headset, their versions and
//! channels) and hand the answer to whoever asked (`sdp_found`).

use super::Host;
use super::chan::PSM_SDP;
use crate::bt::addr_text;
use crate::kprintln;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

const ERROR_RSP: u8 = 0x01;
const SEARCH_REQ: u8 = 0x02;
const SEARCH_RSP: u8 = 0x03;
const ATTR_REQ: u8 = 0x04;
const ATTR_RSP: u8 = 0x05;
const SEARCH_ATTR_REQ: u8 = 0x06;
const SEARCH_ATTR_RSP: u8 = 0x07;
const INVALID_HANDLE: u16 = 0x0002;
const INVALID_SYNTAX: u16 = 0x0003;

pub(super) const UUID_L2CAP: u16 = 0x0100;
pub(super) const UUID_RFCOMM: u16 = 0x0003;
pub(super) const UUID_AVDTP: u16 = 0x0019;
pub(super) const UUID_AUDIO_SOURCE: u16 = 0x110A;
pub(super) const UUID_AUDIO_SINK: u16 = 0x110B;
pub(super) const UUID_A2DP: u16 = 0x110D;
pub(super) const UUID_HANDSFREE: u16 = 0x111E;
pub(super) const UUID_HANDSFREE_AG: u16 = 0x111F;
pub(super) const UUID_AVCTP: u16 = 0x0017;
const UUID_REMOTE_TARGET: u16 = 0x110C;
const UUID_REMOTE: u16 = 0x110E;
const UUID_REMOTE_CONTROLLER: u16 = 0x110F;
const UUID_PUBLIC_BROWSE: u16 = 0x1002;
const UUID_GENERIC_AUDIO: u16 = 0x1203;
const ATTR_NETWORK: u16 = 0x0301;

const ATTR_HANDLE: u16 = 0x0000;
const ATTR_CLASSES: u16 = 0x0001;
const ATTR_PROTOCOLS: u16 = 0x0004;
const ATTR_BROWSE: u16 = 0x0005;
const ATTR_PROFILES: u16 = 0x0009;
const ATTR_NAME: u16 = 0x0100;
const ATTR_FEATURES: u16 = 0x0311;

/// An SDP data element.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum De {
    U8(u8),
    U16(u16),
    U32(u32),
    Uuid(u16),
    /// A 32- or 128-bit UUID — we do not need it, stored as is.
    LongUuid(Vec<u8>),
    Str(String),
    Bool(bool),
    Seq(Vec<De>),
    Other,
}

impl De {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            De::U8(v) => out.extend_from_slice(&[0x08, *v]),
            De::U16(v) => {
                out.push(0x09);
                out.extend_from_slice(&v.to_be_bytes());
            }
            De::U32(v) => {
                out.push(0x0A);
                out.extend_from_slice(&v.to_be_bytes());
            }
            De::Uuid(v) => {
                out.push(0x19);
                out.extend_from_slice(&v.to_be_bytes());
            }
            De::LongUuid(b) => {
                out.push(if b.len() == 4 { 0x1A } else { 0x1C });
                out.extend_from_slice(b);
            }
            De::Str(s) => {
                out.extend_from_slice(&[0x25, s.len() as u8]);
                out.extend_from_slice(s.as_bytes());
            }
            De::Bool(b) => out.extend_from_slice(&[0x28, *b as u8]),
            De::Seq(items) => {
                let mut body = Vec::new();
                for i in items {
                    i.encode(&mut body);
                }
                if body.len() < 256 {
                    out.extend_from_slice(&[0x35, body.len() as u8]);
                } else {
                    out.push(0x36);
                    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
                }
                out.extend_from_slice(&body);
            }
            De::Other => {}
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let mut v = Vec::new();
        self.encode(&mut v);
        v
    }

    /// Parsing one element: (the element, how many bytes it took).
    pub(super) fn decode(b: &[u8]) -> Option<(De, usize)> {
        let head = *b.first()?;
        let (kind, size) = (head >> 3, head & 7);
        let (len, at) = match size {
            0..=4 if kind != 0 => (1usize << size, 1),
            0 => (0, 1),
            5 => (*b.get(1)? as usize, 2),
            6 => (u16::from_be_bytes([*b.get(1)?, *b.get(2)?]) as usize, 3),
            7 => (u32::from_be_bytes(b.get(1..5)?.try_into().ok()?) as usize, 5),
            _ => return None,
        };
        let v = b.get(at..at + len)?;
        let uint = || v.iter().fold(0u64, |a, &x| a << 8 | x as u64);
        let de = match kind {
            0 => De::Other,
            1 | 2 => match len {
                1 => De::U8(v[0]),
                2 => De::U16(uint() as u16),
                4 => De::U32(uint() as u32),
                _ => De::Other,
            },
            3 if len == 2 => De::Uuid(uint() as u16),
            // A UUID based on the Bluetooth base (0000xxxx-0000-1000-8000-00805F9B34FB) — a short one.
            3 if len == 4 && v[0] == 0 && v[1] == 0 => De::Uuid(uint() as u16),
            3 if len == 16 && v[0] == 0 && v[1] == 0 && v[4..] == [0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0x80, 0x5F, 0x9B, 0x34, 0xFB] => {
                De::Uuid(u16::from_be_bytes([v[2], v[3]]))
            }
            3 => De::LongUuid(v.to_vec()),
            4 => De::Str(String::from_utf8_lossy(v).into_owned()),
            5 => De::Bool(v.first() == Some(&1)),
            6 | 7 => {
                let mut items = Vec::new();
                let mut rest = v;
                while !rest.is_empty() {
                    let (item, n) = De::decode(rest)?;
                    items.push(item);
                    rest = &rest[n..];
                }
                De::Seq(items)
            }
            _ => De::Other,
        };
        Some((de, at + len))
    }

    pub(super) fn uint(&self) -> Option<u32> {
        match self {
            De::U8(v) => Some(*v as u32),
            De::U16(v) => Some(*v as u32),
            De::U32(v) => Some(*v),
            _ => None,
        }
    }

    fn items(&self) -> &[De] {
        match self {
            De::Seq(v) => v,
            _ => &[],
        }
    }

    /// All short UUIDs inside (for finding a record by a pattern).
    fn uuids(&self, out: &mut Vec<u16>) {
        match self {
            De::Uuid(u) => out.push(*u),
            De::Seq(v) => v.iter().for_each(|d| d.uuids(out)),
            _ => {}
        }
    }
}

/// A service record: (attribute, value) pairs, in ascending attribute order.
pub(super) type Record = Vec<(u16, De)>;

/// What the device said about a service: the class, the profile version, the channel PSM or the
/// RFCOMM channel number.
#[derive(Clone, Debug, Default)]
pub(super) struct Service {
    pub class: u16,
    pub version: u16,
    pub psm: Option<u16>,
    pub rfcomm: Option<u8>,
    pub features: Option<u16>,
    pub name: String,
}

impl Service {
    fn from(r: &Record) -> Service {
        let get = |id: u16| r.iter().find(|a| a.0 == id).map(|a| &a.1);
        let mut s = Service::default();
        if let Some(De::Uuid(u)) = get(ATTR_CLASSES).and_then(|c| c.items().first()) {
            s.class = *u;
        }
        // Protocols: [[L2CAP, PSM], [RFCOMM, channel] | [AVDTP, version], …]
        for p in get(ATTR_PROTOCOLS).map_or(&[][..], De::items) {
            let items = p.items();
            match (items.first(), items.get(1).and_then(De::uint)) {
                (Some(De::Uuid(UUID_L2CAP)), Some(psm)) => s.psm = Some(psm as u16),
                (Some(De::Uuid(UUID_RFCOMM)), Some(ch)) => s.rfcomm = Some(ch as u8),
                _ => {}
            }
        }
        if let Some(v) = get(ATTR_PROFILES).and_then(|p| p.items().first()).and_then(|p| p.items().get(1)).and_then(De::uint) {
            s.version = v as u16;
        }
        s.features = get(ATTR_FEATURES).and_then(De::uint).map(|v| v as u16);
        if let Some(De::Str(n)) = get(ATTR_NAME) {
            s.name = n.trim_end_matches('\0').into();
        }
        s
    }
}

/// Client requests over one link: the UUID queue and the accumulating answer.
#[derive(Default)]
pub(super) struct SdpClient {
    chan: Option<u16>,
    open: bool,
    queue: Vec<u16>,
    current: Option<u16>,
    buf: Vec<u8>,
    tid: u16,
}

/// Our records (handles from 0x00010000).
fn records() -> Vec<Record> {
    let audio_source = vec![
        (ATTR_HANDLE, De::U32(0x0001_0001)),
        (ATTR_CLASSES, De::Seq(vec![De::Uuid(UUID_AUDIO_SOURCE)])),
        (
            ATTR_PROTOCOLS,
            De::Seq(vec![De::Seq(vec![De::Uuid(UUID_L2CAP), De::U16(super::chan::PSM_AVDTP)]), De::Seq(vec![De::Uuid(UUID_AVDTP), De::U16(0x0103)])]),
        ),
        (ATTR_BROWSE, De::Seq(vec![De::Uuid(UUID_PUBLIC_BROWSE)])),
        (ATTR_PROFILES, De::Seq(vec![De::Seq(vec![De::Uuid(UUID_A2DP), De::U16(0x0103)])])),
        (ATTR_NAME, De::Str("GlassOS Audio".into())),
        // Source: the player.
        (ATTR_FEATURES, De::U16(0x0001)),
    ];
    // The headset audio gateway (we act as a phone): an RFCOMM port, HFP 1.7.
    let headset_gateway = vec![
        (ATTR_HANDLE, De::U32(0x0001_0002)),
        (ATTR_CLASSES, De::Seq(vec![De::Uuid(UUID_HANDSFREE_AG), De::Uuid(UUID_GENERIC_AUDIO)])),
        (
            ATTR_PROTOCOLS,
            De::Seq(vec![De::Seq(vec![De::Uuid(UUID_L2CAP)]), De::Seq(vec![De::Uuid(UUID_RFCOMM), De::U8(super::rfcomm::CHANNEL_HFP_AG)])]),
        ),
        (ATTR_BROWSE, De::Seq(vec![De::Uuid(UUID_PUBLIC_BROWSE)])),
        (ATTR_PROFILES, De::Seq(vec![De::Seq(vec![De::Uuid(UUID_HANDSFREE), De::U16(0x0107)])])),
        (ATTR_NAME, De::Str("GlassOS Voice".into())),
        // Network: "ability to reject a call" — no.
        (ATTR_NETWORK, De::U8(0)),
        (ATTR_FEATURES, De::U16(0)),
    ];
    // The AVRCP 1.4 remote: we are the "target" (the headphone buttons control us) and
    // the "controller" (category 2: we set the headphones' absolute volume).
    let avctp = || {
        De::Seq(vec![De::Seq(vec![De::Uuid(UUID_L2CAP), De::U16(super::chan::PSM_AVCTP)]), De::Seq(vec![De::Uuid(UUID_AVCTP), De::U16(0x0104)])])
    };
    let remote_target = vec![
        (ATTR_HANDLE, De::U32(0x0001_0003)),
        (ATTR_CLASSES, De::Seq(vec![De::Uuid(UUID_REMOTE_TARGET)])),
        (ATTR_PROTOCOLS, avctp()),
        (ATTR_BROWSE, De::Seq(vec![De::Uuid(UUID_PUBLIC_BROWSE)])),
        (ATTR_PROFILES, De::Seq(vec![De::Seq(vec![De::Uuid(UUID_REMOTE), De::U16(0x0104)])])),
        (ATTR_NAME, De::Str("GlassOS Player".into())),
        (ATTR_FEATURES, De::U16(0x0001)),
    ];
    let remote_controller = vec![
        (ATTR_HANDLE, De::U32(0x0001_0004)),
        (ATTR_CLASSES, De::Seq(vec![De::Uuid(UUID_REMOTE), De::Uuid(UUID_REMOTE_CONTROLLER)])),
        (ATTR_PROTOCOLS, avctp()),
        (ATTR_BROWSE, De::Seq(vec![De::Uuid(UUID_PUBLIC_BROWSE)])),
        (ATTR_PROFILES, De::Seq(vec![De::Seq(vec![De::Uuid(UUID_REMOTE), De::U16(0x0104)])])),
        (ATTR_NAME, De::Str("GlassOS Remote".into())),
        (ATTR_FEATURES, De::U16(0x0002)),
    ];
    vec![audio_source, headset_gateway, remote_target, remote_controller]
}

/// A record's attributes from the requested list (ids and ranges).
fn pick(r: &Record, wanted: &[De]) -> De {
    let take = |id: u16| {
        wanted.iter().any(|w| match w {
            De::U16(a) => *a == id,
            De::U32(range) => (range >> 16) as u16 <= id && id <= *range as u16,
            _ => false,
        })
    };
    let mut out = Vec::new();
    for (id, v) in r.iter().filter(|(id, _)| take(*id)) {
        out.push(De::U16(*id));
        out.push(v.clone());
    }
    De::Seq(out)
}

/// Records that contain all the UUIDs of the pattern.
fn matching(pattern: &[De]) -> Vec<Record> {
    records()
        .into_iter()
        .filter(|r| {
            let mut all = Vec::new();
            r.iter().for_each(|(_, v)| v.uuids(&mut all));
            pattern.iter().all(|p| matches!(p, De::Uuid(u) if all.contains(u)))
        })
        .collect()
}

impl Host {
    /// Ask the device about service `uuid` (the answer goes to `sdp_found`).
    pub(super) fn sdp_query(&mut self, handle: u16, uuid: u16) {
        let Some(c) = self.conn_mut(handle) else { return };
        let client = c.sdp.get_or_insert_with(SdpClient::default);
        client.queue.push(uuid);
        let (open, chan) = (client.open, client.chan);
        if chan.is_none() {
            let local = self.chan_open(handle, PSM_SDP);
            if let Some(c) = self.conn_mut(handle).and_then(|c| c.sdp.as_mut()) {
                c.chan = local;
            }
        } else if open {
            self.sdp_next(handle);
        }
    }

    /// The next request from the queue, if we are not waiting for anything now.
    fn sdp_next(&mut self, handle: u16) {
        let Some(c) = self.conn_mut(handle).and_then(|c| c.sdp.as_mut()) else { return };
        if c.current.is_some() || !c.open {
            return;
        }
        let Some(uuid) = c.queue.first().copied() else {
            // Nothing more to ask — close the channel.
            if let Some(local) = c.chan.take() {
                c.open = false;
                self.chan_close(handle, local);
            }
            return;
        };
        c.queue.remove(0);
        c.current = Some(uuid);
        c.buf.clear();
        self.sdp_request(handle, &[]);
    }

    fn sdp_request(&mut self, handle: u16, cont: &[u8]) {
        let Some(c) = self.conn_mut(handle).and_then(|c| c.sdp.as_mut()) else { return };
        let (Some(uuid), Some(local)) = (c.current, c.chan) else { return };
        c.tid = c.tid.wrapping_add(1);
        let tid = c.tid;
        let mut params = De::Seq(vec![De::Uuid(uuid)]).bytes();
        params.extend_from_slice(&0x0400u16.to_be_bytes());
        params.extend_from_slice(&De::Seq(vec![De::U32(0x0000_FFFF)]).bytes());
        params.push(cont.len() as u8);
        params.extend_from_slice(cont);
        let mut pdu = vec![SEARCH_ATTR_REQ];
        pdu.extend_from_slice(&tid.to_be_bytes());
        pdu.extend_from_slice(&(params.len() as u16).to_be_bytes());
        pdu.extend_from_slice(&params);
        self.chan_send(handle, local, &pdu);
    }

    pub(super) fn sdp_open(&mut self, handle: u16, local: u16) {
        let ours = self.conn_mut(handle).and_then(|c| c.sdp.as_mut()).filter(|c| c.chan == Some(local));
        if let Some(c) = ours {
            c.open = true;
            self.sdp_next(handle);
        }
    }

    pub(super) fn sdp_closed(&mut self, handle: u16, local: u16) {
        let Some(c) = self.conn_mut(handle).and_then(|c| c.sdp.as_mut()).filter(|c| c.chan == Some(local)) else { return };
        c.chan = None;
        c.open = false;
        // What was not asked: there will be no answers.
        let lost: Vec<u16> = c.current.take().into_iter().chain(c.queue.drain(..)).collect();
        for uuid in lost {
            self.sdp_found(handle, uuid, Vec::new());
        }
    }

    pub(super) fn sdp_data(&mut self, handle: u16, local: u16, pdu: &[u8]) {
        if pdu.len() < 5 {
            return;
        }
        let (op, tid) = (pdu[0], u16::from_be_bytes([pdu[1], pdu[2]]));
        let body = &pdu[5..];
        let client = self.conn_mut(handle).and_then(|c| c.sdp.as_ref()).is_some_and(|c| c.chan == Some(local));
        if client {
            self.sdp_response(handle, op, body);
        } else {
            self.sdp_serve(handle, local, op, tid, body);
        }
    }

    /// The device's answer to our search: the pieces accumulate until the continuation ends.
    fn sdp_response(&mut self, handle: u16, op: u8, body: &[u8]) {
        let Some(c) = self.conn_mut(handle).and_then(|c| c.sdp.as_mut()) else { return };
        let Some(uuid) = c.current else { return };
        if op != SEARCH_ATTR_RSP || body.len() < 3 {
            c.current = None;
            self.sdp_found(handle, uuid, Vec::new());
            self.sdp_next(handle);
            return;
        }
        let n = u16::from_be_bytes([body[0], body[1]]) as usize;
        let Some(piece) = body.get(2..2 + n) else { return };
        c.buf.extend_from_slice(piece);
        let cont = body.get(2 + n..).unwrap_or(&[]);
        let cont_len = cont.first().copied().unwrap_or(0) as usize;
        if cont_len > 0 && cont.len() > cont_len {
            let state = cont[1..1 + cont_len].to_vec();
            self.sdp_request(handle, &state);
            return;
        }
        let all = core::mem::take(&mut c.buf);
        c.current = None;
        let found: Vec<Service> = De::decode(&all)
            .map(|(d, _)| {
                d.items()
                    .iter()
                    .map(|rec| {
                        let pairs = rec.items().chunks_exact(2).filter_map(|p| p[0].uint().map(|id| (id as u16, p[1].clone()))).collect::<Record>();
                        Service::from(&pairs)
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.sdp_found(handle, uuid, found);
        self.sdp_next(handle);
    }

    /// The client answer: whoever needs it (audio, the headset) parses it.
    fn sdp_found(&mut self, handle: u16, uuid: u16, services: Vec<Service>) {
        if let Some(addr) = self.addr_of(handle) {
            let list: Vec<String> = services
                .iter()
                .map(|s| alloc::format!("{:04x} v{:x}.{:02x}{}{}", s.class, s.version >> 8, s.version & 0xFF, s.psm.map_or(String::new(), |p| alloc::format!(" psm {p:#x}")), s.rfcomm.map_or(String::new(), |c| alloc::format!(" rfcomm {c}"))))
                .collect();
            kprintln!("bt: {} SDP {uuid:04x}: {}", addr_text(&addr), if list.is_empty() { "nothing".into() } else { list.join(", ") });
        }
        self.audio_sdp(handle, uuid, &services);
        self.hfp_sdp(handle, uuid, &services);
    }

    /// The server: the device asks about our services.
    fn sdp_serve(&mut self, handle: u16, local: u16, op: u8, tid: u16, body: &[u8]) {
        let fail = |code: u16| {
            let mut r = vec![ERROR_RSP];
            r.extend_from_slice(&tid.to_be_bytes());
            r.extend_from_slice(&2u16.to_be_bytes());
            r.extend_from_slice(&code.to_be_bytes());
            r
        };
        let reply = (|| -> Option<(u8, Vec<u8>)> {
            match op {
                SEARCH_REQ => {
                    let (pattern, n) = De::decode(body)?;
                    let max = u16::from_be_bytes([*body.get(n)?, *body.get(n + 1)?]) as usize;
                    let handles: Vec<u32> = matching(pattern.items()).iter().filter_map(|r| r[0].1.uint()).take(max).collect();
                    let mut out = Vec::new();
                    out.extend_from_slice(&(handles.len() as u16).to_be_bytes());
                    out.extend_from_slice(&(handles.len() as u16).to_be_bytes());
                    for h in handles {
                        out.extend_from_slice(&h.to_be_bytes());
                    }
                    out.push(0);
                    Some((SEARCH_RSP, out))
                }
                ATTR_REQ => {
                    let want = u32::from_be_bytes(body.get(0..4)?.try_into().ok()?);
                    let (ids, _) = De::decode(body.get(6..)?)?;
                    let Some(rec) = records().into_iter().find(|r| r[0].1.uint() == Some(want)) else {
                        return Some((ERROR_RSP, INVALID_HANDLE.to_be_bytes().to_vec()));
                    };
                    let list = pick(&rec, ids.items()).bytes();
                    let mut out = (list.len() as u16).to_be_bytes().to_vec();
                    out.extend_from_slice(&list);
                    out.push(0);
                    Some((ATTR_RSP, out))
                }
                SEARCH_ATTR_REQ => {
                    let (pattern, n) = De::decode(body)?;
                    let (ids, _) = De::decode(body.get(n + 2..)?)?;
                    let lists = De::Seq(matching(pattern.items()).iter().map(|r| pick(r, ids.items())).collect()).bytes();
                    let mut out = (lists.len() as u16).to_be_bytes().to_vec();
                    out.extend_from_slice(&lists);
                    out.push(0);
                    Some((SEARCH_ATTR_RSP, out))
                }
                _ => None,
            }
        })();
        let pdu = match reply {
            Some((ERROR_RSP, code)) => fail(u16::from_be_bytes([code[0], code[1]])),
            Some((code, params)) => {
                let mut r = vec![code];
                r.extend_from_slice(&tid.to_be_bytes());
                r.extend_from_slice(&(params.len() as u16).to_be_bytes());
                r.extend_from_slice(&params);
                r
            }
            None => fail(INVALID_SYNTAX),
        };
        self.chan_send(handle, local, &pdu);
    }
}
