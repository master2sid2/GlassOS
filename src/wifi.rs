//! Intel AX201 Wi-Fi: the core in the Comet Lake chipset (CNVi) and the Hr radio module.
//!
//! Everything to do with the air is done by the module's firmware. The driver starts it, puts
//! commands into a queue in memory and reads answers from another queue. Interrupts are not needed:
//! we poll the answer queue ourselves.
//!
//! The order of operations was checked against OpenBSD's iwx driver (ISC license). A VM has no such
//! device, so every step writes to the log, and the result goes to `wifi.txt` on
//! the USB stick. One thread owns the module: the others ask it through `REQUEST`.

mod reorder;
mod station;
pub(crate) mod wpa;

use crate::hwprobe::{self, rd, wr};
use crate::sync::IrqLock;
use crate::{fs, interrupts, pci, task};
use alloc::alloc::{Layout, alloc_zeroed, dealloc};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

const DEVICE: (u16, u16) = (0x8086, 0x02F0);
const FIRMWARE: &str = "/firmware/quz77.fw";
const REGS_SIZE: u64 = 16 << 10;
const PAGE: usize = 4096;
/// TSC cycles per microsecond: with a margin, the laptop's frequency is lower.
const CYCLES_PER_US: u64 = 3000;

// PCI configuration.
const PCI_COMMAND: u8 = 0x04;
const INTX_OFF: u32 = 1 << 10;
const MSIX_CAP: u8 = 0x80;
const MSIX_ID: u32 = 0x11;
const MSIX_ENABLE: u32 = 1 << 31;
const PCIE_CONTROL_2: u8 = 0x68;
const LTR_ENABLED: u32 = 1 << 10;

// Device registers.
const HW_IF_CONFIG: usize = 0x000;
const INT_COALESCING: usize = 0x004;
const INT: usize = 0x008;
const INT_MASK: usize = 0x00C;
const FH_INT: usize = 0x010;
const RESET: usize = 0x020;
const GP_CNTRL: usize = 0x024;
const HW_REV: usize = 0x028;
const GIO: usize = 0x03C;
const CONTEXT_BASE: usize = 0x040;
const DRV_GP1: usize = 0x054;
const DRV_GP1_CLEAR: usize = 0x05C;
const MBOX_SET: usize = 0x088;
const SHADOW_CTRL: usize = 0x0A8;
const CHICKEN: usize = 0x100;
const HPET_MEM: usize = 0x240;
const LINK_POWER: usize = 0x250;
const MAC_BASE: usize = 0x380;
const MEM_ADDR: usize = 0x40C;
const MEM_DATA: usize = 0x41C;
const PRPH_WRITE_ADDR: usize = 0x444;
const PRPH_READ_ADDR: usize = 0x448;
const PRPH_WRITE_DATA: usize = 0x44C;
const PRPH_READ_DATA: usize = 0x450;
const TX_WRITE_INDEX: usize = 0x460;
const RX_WRITE_INDEX: usize = 0x1C80;

const NIC_READY: u32 = 0x0040_0000;
const CARD_PREPARE: u32 = 0x0800_0000;
const WAKE_L1A: u32 = 0x0008_0000;
const ENABLE_PME: u32 = 0x1000_0000;
const OS_ALIVE: u32 = 0x20;
const L0S_OFF: u32 = 2;
const L1A_NO_L0S: u32 = 0x0080_0000;
const HPET_MAX: u32 = 0xFFFF_0000;
const LINK_POWER_OFF: u32 = 0x8000_0000;
const CLOCK_READY: u32 = 1;
const INIT_DONE: u32 = 4;
const ACCESS_REQUEST: u32 = 8;
const GOING_TO_SLEEP: u32 = 0x10;
const RADIO_ON: u32 = 0x0800_0000;
const SW_RESET: u32 = 0x80;
const STOP_MASTER: u32 = 0x200;
const MASTER_STOPPED: u32 = 0x100;
const SW_RFKILL: u32 = 2;
const CMD_BLOCKED: u32 = 4;
const SHADOW_ALL: u32 = 0x800F_FFFF;
const COALESCING: u8 = 0x40;

const INT_ALIVE: u32 = 1;
const INT_WAKEUP: u32 = 1 << 1;
const INT_SW_RX: u32 = 1 << 3;
const INT_RF_KILL: u32 = 1 << 7;
const INT_SW_ERROR: u32 = 1 << 25;
const INT_FH_TX: u32 = 1 << 27;
const INT_HW_ERROR: u32 = 1 << 29;
const INT_FH_RX: u32 = 1 << 31;
const INT_ALL: u32 = INT_ALIVE | INT_WAKEUP | INT_SW_RX | INT_RF_KILL | INT_SW_ERROR | INT_FH_TX | INT_HW_ERROR | INT_FH_RX;

// Internal registers reachable through the PRPH window.
const PRPH_MASK: u32 = 0x000F_FFFF;
const PRPH_SIZE: u32 = 3 << 24;
const HPM_DEBUG: u32 = 0xA0_3440;
const PERSISTENCE: u32 = 1 << 12;
const WRITE_PROTECT: u32 = 0xA0_4D00;
const WFPM_ACCESS: u32 = 1 << 12;
const POWER_GATING: u32 = 0xA0_3458;
const GATING_ENABLE: u32 = 1;
const SLEEP_ENABLE: u32 = 2;
const FORCE_ACTIVE: u32 = 1 << 10;
const CHICK: u32 = 0xA0_5C00;
const CHICK_MSI: u32 = 1 << 24;
const MAC_LTR: u32 = 0xA0_348C;
const LTR_ENABLE_ALL: u32 = 0xF;
const UMAC_LTR: u32 = 0xA0_3480;
/// Bus delay at startup: 250 µs, as the integrated module asks.
const LTR_250_US: u32 = 0x82FA_82FA;
const CPU_INIT_RUN: u32 = 0xA0_5C44;
/// What to show if the firmware did not start.
const PRPH_REPORT: [(&str, u32); 5] = [
    ("cpu1 status", 0xA0_38C0),
    ("cpu2 status", 0xA0_38C4),
    ("umac pc", 0xA0_5C18),
    ("lmac1 pc", 0xA0_5C1C),
    ("lmac2 pc", 0xA0_5C20),
];

// The table for the firmware that it starts working from.
const CONTEXT_SIZE: usize = 1792;
const CONTEXT_CONTROL: usize = 8;
const CONTEXT_RX_FREE: usize = 24;
const CONTEXT_RX_USED: usize = 32;
const CONTEXT_RX_STATUS: usize = 40;
const CONTEXT_TX_QUEUE: usize = 48;
const CONTEXT_TX_SIZE: usize = 56;
const CONTEXT_UMAC: usize = 192;
const CONTEXT_LMAC: usize = 704;
const CONTEXT_PAGED: usize = 1216;
const CONTEXT_ENTRIES: usize = 64;
const TFD_LONG: u32 = 1 << 8;
const RX_CB_POS: u32 = 4;
const RB_SIZE_POS: u32 = 9;
const RB_SIZE_4K: u32 = 4;

// Queues.
const RX_COUNT: usize = 512;
const RX_BUFFER: usize = 4096;
const RX_ALIGN: usize = 256;
const RX_STATUS_SIZE: usize = 16;
const TX_COUNT: usize = 256;
const TFD_SIZE: usize = 256;
const TFD_ALIGN: usize = 256;
const COMMAND_QUEUE: u32 = 0;
const FIRST_TB: usize = 20;
const CMD_HEADER: usize = 8;
const CMD_SPACE: usize = 4096;
/// Each command has its own slot: the firmware does not read them right away, and a command sent without
/// waiting for an answer would otherwise be overwritten by the next one.
const CMD_SLOTS: usize = 16;
const PACKET_ALIGN: usize = 0x40;
const PACKET_HEADER: usize = 8;
const FRAME_INVALID: u32 = 0x5555_0000;
const FRAME_SIZE: u32 = 0x3FFF;
const FROM_FIRMWARE: u8 = 0x80;
const CMD_FAILED: u8 = 0x40;

// Command groups and ids.
const LONG: u8 = 1;
const SYSTEM: u8 = 2;
const DATA_PATH: u8 = 5;
const REGULATORY: u8 = 0xC;
const ALIVE: u8 = 0x01;
const TX_CMD: u8 = 0x1C;
const REPLY_ERROR: u8 = 0x02;
const INIT_COMPLETE: u8 = 0x04;
const SCAN_CONFIG: u8 = 0x0C;
const SCAN_REQUEST: u8 = 0x0D;
const SCAN_COMPLETE: u8 = 0x0F;
const POWER_TABLE: u8 = 0x77;
const TX_ANTENNAS: u8 = 0x98;
const BT_CONFIG: u8 = 0x9B;
const SCAN_ITERATION: u8 = 0xB5;
const RX_MPDU: u8 = 0xC1;
const BAR_RELEASE: u8 = 0xC2;
const FRAME_RELEASE: u8 = 0xC3;
const MCC_UPDATE: u8 = 0xC8;
const LTR_CONFIG: u8 = 0xEE;
const INIT_EXTENDED: u8 = 0x03;
const SOC_CONFIG: u8 = 0x01;
const DQA_ENABLE: u8 = 0x00;
const NVM_DONE: u8 = 0x00;
const NVM_INFO: u8 = 0x02;
const INIT_NVM: u32 = 1 << 1;
/// Coexistence with Bluetooth on a shared antenna: 1 — the firmware shares the air (as Linux does by
/// default); 3 — Wi-Fi always wins, and then Bluetooth keyboards are late with
/// reports (keys "stick" on auto-repeat).
const BT_COEX_SHARED: u32 = 1;
/// The integrated module: crystal delay in microseconds.
const XTAL_LATENCY: u32 = 500;
const MCC_WORLD: u16 = (b'Z' as u16) << 8 | b'Z' as u16;
const MCC_GET_CURRENT: u8 = 0x10;

const ALIVE_OK: u16 = 0xCAFE;
const ALIVE_LMAC_LOG: usize = 20;
const ALIVE_UMAC_LOG: usize = 108;
const ERROR_WORDS: usize = 20;

// The firmware container.
const FW_MAGIC: u32 = 0x0A4C_5749;
const FW_DATA: usize = 88;
const FW_NAME: core::ops::Range<usize> = 8..72;
const TLV_SEC_RT: u32 = 19;
const TLV_PHY_SKU: u32 = 23;
const TLV_API: u32 = 29;
const TLV_CAPA: u32 = 30;
const TLV_CMD_VERSIONS: u32 = 48;
const SEPARATOR_CPU: u32 = 0xFFFF_CCCC;
const SEPARATOR_PAGING: u32 = 0xAAAA_BBBB;
const PHY_TYPE_MASK: u32 = 0x3F;
const API_WORDS: usize = 4;
const API_REGULATORY_V4: usize = 48;
const API_REDUCED_SCAN_CONFIG: usize = 56;
const API_SCAN_EXT_CHAN: usize = 58;
const CAPA_LAR: usize = 1;
const CAPA_DQA: usize = 12;
const CAPA_BAID: usize = 63;

// The module answer (NVM).
const NVM_MAC_SKU: usize = 8;
const NVM_TX: usize = 12;
const NVM_RX: usize = 16;
const NVM_LAR: usize = 20;
const NVM_CHANNELS: usize = 28;
const BAND_5GHZ: u32 = 1 << 1;
const CHANNEL_VALID: u32 = 1;
const CHANNELS: [u8; 51] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 36, 40, 44, 48, 52, 56, 60, 64, 68, 72, 76, 80, 84, 88, 92, 96, 100, 104, 108, 112, 116,
    120, 124, 128, 132, 136, 140, 144, 149, 153, 157, 161, 165, 169, 173, 177, 181,
];
const CHANNELS_2GHZ: usize = 14;
const BAND_FLAG_2GHZ: u8 = 1;
const BAND_FLAG_5GHZ: u8 = 0;

// The scan command, versions 14 and 15.
const SCAN_SIZE: usize = 1940;
const SCAN_PRIORITY: u8 = 6;
const SCAN_GENERAL: usize = 8;
const SCAN_CHANNELS: usize = SCAN_GENERAL + 36;
const SCAN_CHANNEL_MAX: usize = 67;
const SCAN_PERIODIC: usize = SCAN_CHANNELS + 4 + SCAN_CHANNEL_MAX * 8;
const SCAN_PROBE: usize = SCAN_PERIODIC + 12;
const SCAN_PROBE_FRAME: usize = SCAN_PROBE + 20;
const SCAN_ORDER: u8 = 1 << 5;
const FLAG_PASS_ALL: u16 = 1 << 1;
const FLAG_NOTIFY_ITERATION: u16 = 1 << 2;
const FLAG_ADAPTIVE: u16 = 1 << 7;
const FLAG_PASSIVE: u16 = 1 << 11;
const DWELL_ACTIVE: u8 = 10;
const DWELL_PASSIVE: u8 = 110;
const ADWELL_2GHZ: u8 = 2;
const ADWELL_5GHZ: u8 = 8;
const ADWELL_SOCIAL: u8 = 10;
const ADWELL_BUDGET: u16 = 300;
const APS_FRIENDLY: u8 = 10;
const APS_SOCIAL: u8 = 2;
/// A probe request: a header and an empty network name, then the rates of the two bands.
const PROBE_HEADER: usize = 26;
const RATES_2GHZ: [u8; 16] = [1, 8, 0x82, 0x84, 0x8B, 0x96, 0x0C, 0x12, 0x18, 0x24, 50, 4, 0x30, 0x48, 0x60, 0x6C];
const RATES_5GHZ: [u8; 10] = [1, 8, 0x8C, 0x12, 0x98, 0x24, 0xB0, 0x48, 0x60, 0x6C];

// A received frame.
const RX_DESC: usize = 48;
const RX_STATUS_OK: u32 = 3;
const RX_FLAGS2: usize = 3;
const RX_PADDED: u8 = 0x20;
const RX_DECRYPTED: u32 = 1 << 11 | 1 << 6;
const RX_CIPHER_MASK: u32 = 7 << 8;
const RX_CIPHER_CCM: u32 = 2 << 8;
const RX_DUPLICATE: u32 = 1 << 22;
const RX_RATE: usize = 28;
const RX_TIME: usize = 36;
/// In the frame header: this is a retry, the first attempt did not reach us.
const FLAG_RETRY: u8 = 1 << 3;
const RX_ENERGY_A: usize = 32;
const RX_ENERGY_B: usize = 33;
const RX_CHANNEL: usize = 34;
const FRAME_BEACON: u8 = 0x80;
const FRAME_PROBE_RESPONSE: u8 = 0x50;
const FRAME_DEAUTH: u8 = 0xC0;
const FRAME_DISASSOC: u8 = 0xA0;
const FRAME_TYPE: u8 = 0x0C;
const TYPE_MANAGEMENT: u8 = 0;
const TYPE_DATA: u8 = 8;
const SUBTYPE_QOS: u8 = 0x80;
const SUBTYPE_NO_DATA: u8 = 0x40;
const FLAG_FROM_DS: u8 = 2;
const FLAG_PROTECTED: u8 = 0x40;
const FRAME_DESTINATION: usize = 4;
const FRAME_SENDER: usize = 10;
const FRAME_BSSID: usize = 16;
const FRAME_INTERVAL: usize = 32;
const FRAME_CAPABILITY: usize = 34;
const FRAME_ELEMENTS: usize = 36;
const FRAME_HEADER: usize = 24;
const QOS_FIELD: usize = 2;
const CCMP_HEADER: usize = 8;
const FRAME_PADDING: usize = 2;
const LLC_SIZE: usize = 8;
const CAP_PRIVACY: u16 = 1 << 4;
const ELEMENT_SSID: u8 = 0;
const ELEMENT_RATES: u8 = 1;
const ELEMENT_DS: u8 = 3;
const ELEMENT_TIM: u8 = 5;
const ELEMENT_RSN: u8 = 48;
const ELEMENT_HT_CAPS: u8 = 45;
const ELEMENT_HT_OPERATION: u8 = 61;
const ELEMENT_VHT_CAPS: u8 = 191;
const ELEMENT_VHT_OPERATION: u8 = 192;
const ELEMENT_MORE_RATES: u8 = 50;
const ELEMENT_VENDOR: u8 = 221;
const OUI: [u8; 3] = [0, 0x0F, 0xAC];
const CIPHER_CCMP: u8 = 4;
const AKM_ENTERPRISE: u8 = 1;
const AKM_PSK: u8 = 2;
const AKM_SAE: u8 = 8;
const RSN_MFP_REQUIRED: u16 = 1 << 6;
const ETHER_EAPOL: u16 = 0x888E;
const WPA_OUI: [u8; 4] = [0, 0x50, 0xF2, 1];
const WMM_OUI: [u8; 4] = [0, 0x50, 0xF2, 2];

const ALIVE_MS: u64 = 3000;
/// If the register never reported the start, we open the receive queue ourselves.
const ALIVE_KICK_MS: u64 = 500;
const COMMAND_MS: u64 = 2000;
const INIT_MS: u64 = 4000;
const SCAN_MS: u64 = 20_000;
const NOTES_MAX: usize = 40;
const QUEUE_MAX: usize = 512;
/// How many times to try connecting before giving up.
const ATTEMPTS: usize = 3;
const IDLE_TICKS: u64 = 5;
const SCAN_WAIT_SECONDS: u64 = 40;
const NOT_CONNECTED: &str = "not connected";
/// How much the 5 GHz band is preferred: wider channels there and no neighbours on 2.4.
const BONUS_5GHZ: i32 = 15;
/// How long to wait for the first data in the fast mode before stepping back.
const SILENCE_SECONDS: u64 = 20;
/// When to save the log of a working link: the mode and rate are known by then.
const LINK_REPORT_SECONDS: u64 = 30;
/// How often to update the log while data flows.
const TRAFFIC_REPORT_SECONDS: u64 = 30;
const RATE_MODE_POS: u32 = 8;
const RATE_MODE_MASK: u32 = 7;
const RATE_KINDS: usize = 5;
const RATE_OFDM: u32 = 1;
const RATE_HT: u32 = 2;
const RATE_VHT: u32 = 3;
const RATE_STREAMS_POS: u32 = 4;
const RATE_HT_MCS: u32 = 7;
const RATE_VHT_MCS: u32 = 0xF;
const RATE_STREAMS: u32 = 3;
const RATE_WIDTH_POS: u32 = 11;
const RATE_WIDTH: u32 = 7;
const RATE_SHORT_GUARD: u32 = 1 << 20;
const OFDM_MBIT: [u32; 8] = [6, 9, 12, 18, 24, 36, 48, 54];
/// The rate of one stream in hundreds of kbit/s: the row is the width, the column the MCS.
const STREAM_RATE: [[u32; 10]; 3] = [
    [65, 130, 195, 260, 390, 520, 585, 650, 780, 867],
    [135, 270, 405, 540, 810, 1080, 1215, 1350, 1620, 1800],
    [293, 585, 878, 1170, 1755, 2340, 2633, 2925, 3510, 3900],
];
/// Saved networks: the name and the network key, not the password.
pub const CONFIG: &str = "/system/wifi.cfg";

/// The access point's HT capabilities: what it can do and how it works now.
#[derive(Clone, Copy)]
struct Ht {
    caps: u16,
    ampdu: u8,
    mcs: [u8; 2],
    secondary: u8,
    wide: bool,
    protection: u8,
}

#[derive(Clone, Copy)]
struct Vht {
    caps: u32,
    mcs: u16,
    width: u8,
    center: u8,
}

/// The link mode. If the fast one did not work, the next attempt goes one step lower.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
enum Mode {
    #[default]
    Legacy,
    Ht,
    Vht,
}

/// An access point found on the air.
#[derive(Clone)]
pub struct Network {
    pub ssid: String,
    pub bssid: [u8; 6],
    pub channel: u8,
    /// Signal level in dBm.
    pub signal: i32,
    pub security: &'static str,
    /// Whether we can connect: WPA2-PSK with CCMP.
    pub usable: bool,
    capability: u16,
    interval: u16,
    dtim: u8,
    /// How many beacons until DTIM in the remembered frame.
    dtim_left: u8,
    tsf: u64,
    rx_time: u32,
    rates: Vec<u8>,
    ht: Option<Ht>,
    vht: Option<Vht>,
    wmm: bool,
}

/// An access point from a beacon or a probe response. `heard_on` is the channel the frame
/// was caught on (if it does not carry its own), `signal` is dBm, `rx_time` is the receive time by
/// the module's clock. Shared by all Wi-Fi drivers.
pub(crate) fn network_from(frame: &[u8], heard_on: u8, signal: i32, rx_time: u32) -> Option<Network> {
    if frame.len() < FRAME_ELEMENTS || !matches!(frame[0], FRAME_BEACON | FRAME_PROBE_RESPONSE) {
        return None;
    }
    let mut bssid = [0; 6];
    bssid.copy_from_slice(&frame[FRAME_BSSID..FRAME_BSSID + 6]);
    let privacy = u16_at(frame, FRAME_CAPABILITY) & CAP_PRIVACY != 0;
    let mut ssid = String::new();
    let mut channel = heard_on;
    let (mut rsn, mut wpa) = (None, false);
    let (mut rates, mut dtim, mut dtim_left) = (Vec::new(), 0, 0);
    let (mut ht_caps, mut ht_ampdu, mut ht_mcs, mut ht_operation) = (0, 0, [0; 2], None);
    let (mut vht_caps, mut vht_operation, mut wmm) = (None, (0, 0), false);
    let mut at = FRAME_ELEMENTS;
    while at + 2 <= frame.len() {
        let (id, size) = (frame[at], frame[at + 1] as usize);
        let Some(value) = frame.get(at + 2..at + 2 + size) else { break };
        match id {
            ELEMENT_SSID => ssid = String::from_utf8_lossy(value).into(),
            ELEMENT_DS if size == 1 => channel = value[0],
            ELEMENT_RATES | ELEMENT_MORE_RATES => rates.extend_from_slice(value),
            ELEMENT_TIM if size >= 2 => (dtim_left, dtim) = (value[0], value[1]),
            ELEMENT_RSN => rsn = Some(read_rsn(value)),
            ELEMENT_VENDOR if value.starts_with(&WPA_OUI) => wpa = true,
            ELEMENT_VENDOR if value.starts_with(&WMM_OUI) => wmm = true,
            ELEMENT_HT_CAPS if size >= 5 => (ht_caps, ht_ampdu, ht_mcs) = (u16_at(value, 0), value[2], [value[3], value[4]]),
            ELEMENT_HT_OPERATION if size >= 4 => ht_operation = Some((value[1] & 3, value[1] & 4 != 0, value[2] & 3)),
            ELEMENT_VHT_CAPS if size >= 6 => vht_caps = Some((u32_at(value, 0), u16_at(value, 4))),
            ELEMENT_VHT_OPERATION if size >= 2 => vht_operation = (value[0], value[1]),
            _ => {}
        }
        at += 2 + size;
    }
    let ht = ht_operation.filter(|_| ht_caps != 0).map(|(secondary, wide, protection)| Ht { caps: ht_caps, ampdu: ht_ampdu, mcs: ht_mcs, secondary, wide, protection });
    let vht = vht_caps.map(|(caps, mcs)| Vht { caps, mcs, width: vht_operation.0, center: vht_operation.1 });
    let (security, usable) = match (rsn, wpa, privacy) {
        (Some(found), ..) => found,
        (_, true, _) => ("WPA", false),
        (.., true) => ("WEP", false),
        _ => ("open", false),
    };
    let tsf = frame[FRAME_HEADER..FRAME_HEADER + 8].iter().rev().fold(0u64, |v, &b| v << 8 | b as u64);
    let (capability, interval) = (u16_at(frame, FRAME_CAPABILITY), u16_at(frame, FRAME_INTERVAL));
    Some(Network { ssid, bssid, channel, signal, security, usable, capability, interval, dtim, dtim_left, tsf, rx_time, rates, ht, vht, wmm })
}

/// What the module thread is asked for.
enum Request {
    Scan,
    Join(String, [u8; 32]),
    /// Connect to the strongest saved network.
    Known,
    Leave,
}

static REQUEST: IrqLock<Option<Request>> = IrqLock::new(None);
static STARTED: AtomicBool = AtomicBool::new(false);
/// The module is present in the computer (seen at startup).
static PRESENT: AtomicBool = AtomicBool::new(false);
/// The module thread is busy with a request: scanning, connecting or holding the link.
static WORKING: AtomicBool = AtomicBool::new(false);
/// The network we are connected to, and the link mode.
static JOINED: IrqLock<Option<(String, String)>> = IrqLock::new(None);
static LINK: AtomicBool = AtomicBool::new(false);
/// The highest mode to connect with (`Mode`).
static LIMIT: AtomicU8 = AtomicU8::new(Mode::Vht as u8);
/// The air rate of the last frame addressed to us, Mbit/s.
static SPEED: AtomicU32 = AtomicU32::new(0);
static WORKED: AtomicBool = AtomicBool::new(false);
static STATUS: IrqLock<String> = IrqLock::new(String::new());
/// The status line for the Realtek card (it has its own driver, not the Intel thread).
static RTL_STATUS: IrqLock<String> = IrqLock::new(String::new());
static SCANS: AtomicU64 = AtomicU64::new(0);
static FOUND: IrqLock<Vec<Network>> = IrqLock::new(Vec::new());
static NEW_LINK: IrqLock<Option<[u8; 6]>> = IrqLock::new(None);
static TX_FRAMES: IrqLock<VecDeque<Vec<u8>>> = IrqLock::new(VecDeque::new());
static RX_FRAMES: IrqLock<VecDeque<Vec<u8>>> = IrqLock::new(VecDeque::new());

/// A memory block for the device. Memory is mapped one-to-one, so the address for
/// the device is the same. The heap takes small blocks below 4 GiB.
struct Dma {
    ptr: *mut u8,
    layout: Layout,
}

unsafe impl Send for Dma {}

impl Dma {
    fn new(size: usize, align: usize) -> Result<Self, String> {
        let layout = Layout::from_size_align(size, align).map_err(|_| "bad memory block")?;
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            return Err(format!("no memory for {size} bytes"));
        }
        Ok(Self { ptr, layout })
    }

    fn addr(&self) -> u64 {
        self.ptr as u64
    }

    fn put(&self, at: usize, data: &[u8]) {
        assert!(at + data.len() <= self.layout.size());
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.add(at), data.len()) };
    }

    fn put_u64(&self, at: usize, value: u64) {
        self.put(at, &value.to_le_bytes());
    }

    fn get_u16(&self, at: usize) -> u16 {
        unsafe { (self.ptr.add(at) as *const u16).read_volatile() }
    }

    fn bytes(&self) -> Vec<u8> {
        let mut out = vec![0; self.layout.size()];
        unsafe { core::ptr::copy_nonoverlapping(self.ptr, out.as_mut_ptr(), out.len()) };
        out
    }
}

impl Drop for Dma {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

/// A parsed firmware file. Sections are pieces of `blob`: (device address, start, length).
struct Firmware {
    blob: Vec<u8>,
    sections: Vec<(u32, usize, usize)>,
    phy_config: u32,
    name: String,
    api: [u32; API_WORDS],
    capabilities: [u32; API_WORDS],
    /// (group, command, command version).
    versions: Vec<(u8, u8, u8)>,
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    data.get(at..at + 4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn u16_at(data: &[u8], at: usize) -> u16 {
    data.get(at..at + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
}

fn flag(words: &[u32; API_WORDS], bit: usize) -> bool {
    words.get(bit / 32).is_some_and(|w| w >> (bit % 32) & 1 != 0)
}

impl Firmware {
    fn parse(blob: Vec<u8>) -> Result<Self, String> {
        if u32_at(&blob, 0) != 0 || u32_at(&blob, 4) != FW_MAGIC {
            return Err("this is not a firmware file".into());
        }
        let name = blob.get(FW_NAME).map(|n| String::from_utf8_lossy(n).trim_end_matches('\0').to_string()).unwrap_or_default();
        let mut fw = Self { sections: Vec::new(), phy_config: 0, name, api: [0; API_WORDS], capabilities: [0; API_WORDS], versions: Vec::new(), blob: Vec::new() };
        let mut at = FW_DATA;
        while at + 8 <= blob.len() {
            let (kind, len) = (u32_at(&blob, at), u32_at(&blob, at + 4) as usize);
            let start = at + 8;
            let value = blob.get(start..start + len).ok_or("firmware file is cut short")?;
            match kind {
                TLV_SEC_RT if len >= 4 => fw.sections.push((u32_at(value, 0), start + 4, len - 4)),
                TLV_PHY_SKU => fw.phy_config = u32_at(value, 0),
                TLV_API | TLV_CAPA => {
                    let words = if kind == TLV_API { &mut fw.api } else { &mut fw.capabilities };
                    if let Some(word) = words.get_mut(u32_at(value, 0) as usize) {
                        *word = u32_at(value, 4);
                    }
                }
                TLV_CMD_VERSIONS => fw.versions = value.chunks_exact(4).map(|e| (e[1], e[0], e[2])).collect(),
                _ => {}
            }
            at = start + len.next_multiple_of(4);
        }
        fw.blob = blob;
        Ok(fw)
    }

    /// Firmware sections in three groups: two processors and the paged part.
    fn groups(&self) -> [&[(u32, usize, usize)]; 3] {
        let is_end = |s: &(u32, usize, usize)| s.0 == SEPARATOR_CPU || s.0 == SEPARATOR_PAGING;
        let first = self.sections.iter().position(is_end).unwrap_or(self.sections.len());
        let rest = self.sections.get(first + 1..).unwrap_or(&[]);
        let second = rest.iter().position(is_end).unwrap_or(rest.len());
        [&self.sections[..first], &rest[..second], rest.get(second + 1..).unwrap_or(&[])]
    }

    fn version_of(&self, group: u8, command: u8) -> Option<u8> {
        self.versions.iter().find(|v| (v.0, v.1) == (group, command)).map(|v| v.2)
    }
}

fn delay_us(us: u64) {
    let end = hwprobe::deadline(us * CYCLES_PER_US);
    while !hwprobe::expired(end) {
        core::hint::spin_loop();
    }
}

fn hex6(a: &[u8; 6]) -> String {
    a.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

struct Card {
    pci: pci::Device,
    mmio: usize,
    hw_rev: u32,
    fw: Firmware,
    context: Dma,
    /// Firmware sections: the paged ones are needed by the device while it runs.
    images: Vec<Dma>,
    rx_free: Dma,
    rx_used: Dma,
    rx_status: Dma,
    rx_buffers: Vec<Dma>,
    rx_next: usize,
    tx_ring: Dma,
    tx_next: usize,
    command: Dma,
    rx_open: bool,
    alive: Option<u16>,
    init_done: bool,
    scan_done: bool,
    reply: Option<(u8, Vec<u8>)>,
    lmac_log: u32,
    umac_log: u32,
    notes: usize,
    frames: usize,
    networks: BTreeMap<[u8; 6], Network>,
    mac: [u8; 6],
    tx_chains: u32,
    rx_chains: u32,
    lar: bool,
    channels: Vec<(u8, u8)>,
    /// The command id whose answer we wait for.
    expected: u8,
    bssid: [u8; 6],
    tx: Option<station::TxRing>,
    sequence: u16,
    qos_sequence: [u16; 8],
    link: station::Link,
    management_rate: u32,
    sessions: Vec<reorder::Session>,
    /// How many frames were sent to the network.
    delivered: u64,
    last_rate: u32,
    /// Frames to us since the last report: how many at which rate, access point retries,
    /// the sum of signal levels on two antennas.
    rate_frames: alloc::collections::BTreeMap<u32, u64>,
    rx_retried: u64,
    rx_energy: [u64; 2],
    /// Frames to us by modulation: CCK, OFDM, HT, VHT, HE.
    rx_kinds: [u64; RATE_KINDS],
    /// How many times frames waited for a lost one in vain.
    expired: u64,
    tx_failed: u64,
    management: Vec<Vec<u8>>,
    eapol: Vec<Vec<u8>>,
    handshake: Option<wpa::Handshake>,
    keys_ready: bool,
    lost: bool,
}

impl Card {
    fn new(pci: pci::Device, fw: Firmware) -> Result<Self, String> {
        let mmio = pci.mmio(0, REGS_SIZE);
        let rx_buffers = (0..RX_COUNT).map(|_| Dma::new(RX_BUFFER, PAGE)).collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            pci,
            mmio,
            hw_rev: 0,
            fw,
            context: Dma::new(CONTEXT_SIZE, PAGE)?,
            images: Vec::new(),
            rx_free: Dma::new(RX_COUNT * 8, RX_ALIGN)?,
            rx_used: Dma::new(RX_COUNT * 4, RX_ALIGN)?,
            rx_status: Dma::new(RX_STATUS_SIZE, RX_STATUS_SIZE)?,
            rx_buffers,
            rx_next: 0,
            tx_ring: Dma::new(TX_COUNT * TFD_SIZE, TFD_ALIGN)?,
            tx_next: 0,
            command: Dma::new(CMD_SLOTS * CMD_SPACE, PAGE)?,
            rx_open: false,
            alive: None,
            init_done: false,
            scan_done: false,
            reply: None,
            lmac_log: 0,
            umac_log: 0,
            notes: 0,
            frames: 0,
            networks: BTreeMap::new(),
            mac: [0; 6],
            tx_chains: 0,
            rx_chains: 0,
            lar: false,
            channels: Vec::new(),
            expected: 0,
            bssid: [0; 6],
            tx: None,
            sequence: 0,
            qos_sequence: [0; 8],
            link: station::Link::default(),
            management_rate: 0,
            sessions: Vec::new(),
            delivered: 0,
            last_rate: 0,
            rate_frames: alloc::collections::BTreeMap::new(),
            rx_retried: 0,
            rx_energy: [0; 2],
            rx_kinds: [0; RATE_KINDS],
            expired: 0,
            tx_failed: 0,
            management: Vec::new(),
            eapol: Vec::new(),
            handshake: None,
            keys_ready: false,
            lost: false,
        })
    }

    fn rd(&self, reg: usize) -> u32 {
        rd(self.mmio, reg)
    }

    fn wr(&self, reg: usize, value: u32) {
        wr(self.mmio, reg, value)
    }

    fn set(&self, reg: usize, bits: u32) {
        self.wr(reg, self.rd(reg) | bits);
    }

    fn clear(&self, reg: usize, bits: u32) {
        self.wr(reg, self.rd(reg) & !bits);
    }

    fn poll(&self, reg: usize, value: u32, mask: u32, us: u64) -> bool {
        let end = hwprobe::deadline(us * CYCLES_PER_US);
        loop {
            if self.rd(reg) & mask == value & mask {
                return true;
            }
            if hwprobe::expired(end) {
                return false;
            }
            delay_us(10);
        }
    }

    fn prph_read(&self, addr: u32) -> u32 {
        self.wr(PRPH_READ_ADDR, addr & PRPH_MASK | PRPH_SIZE);
        self.rd(PRPH_READ_DATA)
    }

    fn prph_write(&self, addr: u32, value: u32) {
        self.wr(PRPH_WRITE_ADDR, addr & PRPH_MASK | PRPH_SIZE);
        self.wr(PRPH_WRITE_DATA, value);
    }

    /// Keeps the device awake: otherwise the internal registers do not answer.
    fn lock(&self) -> Result<(), String> {
        self.set(GP_CNTRL, ACCESS_REQUEST);
        delay_us(2);
        if self.poll(GP_CNTRL, CLOCK_READY, CLOCK_READY | GOING_TO_SLEEP, 150_000) {
            Ok(())
        } else {
            Err(format!("the card does not wake up, GP_CNTRL {:08x}", self.rd(GP_CNTRL)))
        }
    }

    fn unlock(&self) {
        self.clear(GP_CNTRL, ACCESS_REQUEST);
    }

    fn prph_change(&self, addr: u32, set: u32, clear: u32) -> Result<(), String> {
        self.lock()?;
        self.prph_write(addr, self.prph_read(addr) & !clear | set);
        self.unlock();
        Ok(())
    }

    fn hw_ready(&self) -> bool {
        const READY_US: u64 = 50;
        self.set(HW_IF_CONFIG, NIC_READY);
        let ready = self.poll(HW_IF_CONFIG, NIC_READY, NIC_READY, READY_US);
        if ready {
            self.set(MBOX_SET, OS_ALIVE);
        }
        ready
    }

    fn prepare(&self) -> Result<(), String> {
        const TRIES: usize = 10;
        const ROUND_US: u64 = 150_000;
        const STEP_US: u64 = 200;
        if self.hw_ready() {
            return Ok(());
        }
        self.set(LINK_POWER, LINK_POWER_OFF);
        delay_us(1000);
        for _ in 0..TRIES {
            self.set(HW_IF_CONFIG, CARD_PREPARE);
            for _ in 0..ROUND_US / STEP_US {
                if self.hw_ready() {
                    return Ok(());
                }
                delay_us(STEP_US);
            }
            delay_us(25_000);
        }
        Err(format!("the card is not ready, config {:08x}", self.rd(HW_IF_CONFIG)))
    }

    fn reset(&self) {
        self.set(RESET, SW_RESET);
        delay_us(5000);
    }

    fn apm_init(&self) -> Result<(), String> {
        self.set(CHICKEN, L1A_NO_L0S);
        self.set(HPET_MEM, HPET_MAX);
        self.set(HW_IF_CONFIG, WAKE_L1A);
        self.set(GIO, L0S_OFF);
        self.set(GP_CNTRL, INIT_DONE);
        if self.poll(GP_CNTRL, CLOCK_READY, CLOCK_READY, 25_000) {
            Ok(())
        } else {
            Err(format!("the clock did not settle, GP_CNTRL {:08x}", self.rd(GP_CNTRL)))
        }
    }

    fn start_hardware(&self) -> Result<(), String> {
        self.prepare()?;
        let debug = self.prph_read(HPM_DEBUG);
        if debug != 0xA5A5_A5A0 && debug & PERSISTENCE != 0 {
            if self.prph_read(WRITE_PROTECT) & WFPM_ACCESS != 0 {
                return Err("cannot clear the persistence bit".into());
            }
            self.prph_write(HPM_DEBUG, debug & !PERSISTENCE);
        }
        self.reset();
        // The integrated module: put it through a power-off once.
        self.set(GP_CNTRL, INIT_DONE);
        delay_us(20);
        if !self.poll(GP_CNTRL, CLOCK_READY, CLOCK_READY, 25_000) {
            return Err("the clock did not settle before power gating".into());
        }
        self.prph_change(POWER_GATING, FORCE_ACTIVE, 0)?;
        delay_us(20);
        self.prph_change(POWER_GATING, GATING_ENABLE | SLEEP_ENABLE, 0)?;
        delay_us(20);
        self.prph_change(POWER_GATING, 0, FORCE_ACTIVE)?;
        self.reset();
        self.apm_init()?;
        // By default the device sends MSI-X interrupts, and we need the ordinary register.
        self.lock()?;
        self.prph_write(CHICK, CHICK_MSI);
        self.unlock();
        crate::kprintln!("wifi: hardware ready, GP_CNTRL {:08x}", self.rd(GP_CNTRL));
        if self.rd(GP_CNTRL) & RADIO_ON == 0 {
            return Err("the radio is switched off (airplane switch or BIOS)".into());
        }
        Ok(())
    }

    fn configure_nic(&self) {
        const MASK: u32 = 0x3 | 0xC | 0xC00 | 0xC000 | 0x3000 | 0x200 | 0x100;
        let phy = self.fw.phy_config & PHY_TYPE_MASK;
        let (radio_type, radio_step, radio_dash) = (phy & 3, phy >> 2 & 3, phy >> 4 & 3);
        let (step, dash) = (self.hw_rev >> 2 & 3, self.hw_rev & 3);
        let value = dash | step << 2 | radio_type << 10 | radio_dash << 12 | radio_step << 14;
        self.wr(HW_IF_CONFIG, self.rd(HW_IF_CONFIG) & !MASK | value);
    }

    fn fill_context(&mut self) -> Result<(), String> {
        const RX_ORDER: u32 = RX_COUNT.trailing_zeros();
        const TX_ORDER: u8 = TX_COUNT.trailing_zeros() as u8 - 3;
        let raw_rev = self.rd(HW_REV);
        let mut head = [0u8; 8];
        head[..2].copy_from_slice(&(raw_rev as u16).to_le_bytes());
        head[4..6].copy_from_slice(&((CONTEXT_SIZE / 4) as u16).to_le_bytes());
        self.context.put(0, &head);
        let control = TFD_LONG | RX_ORDER << RX_CB_POS | RB_SIZE_4K << RB_SIZE_POS;
        self.context.put(CONTEXT_CONTROL, &control.to_le_bytes());
        self.context.put_u64(CONTEXT_RX_FREE, self.rx_free.addr());
        self.context.put_u64(CONTEXT_RX_USED, self.rx_used.addr());
        self.context.put_u64(CONTEXT_RX_STATUS, self.rx_status.addr());
        self.context.put_u64(CONTEXT_TX_QUEUE, self.tx_ring.addr());
        self.context.put(CONTEXT_TX_SIZE, &[TX_ORDER]);
        let groups = self.fw.groups();
        let [lmac, umac, paged] = groups;
        crate::kprintln!("wifi: firmware sections: {} + {} + {} paged", lmac.len(), umac.len(), paged.len());
        if groups.iter().any(|g| g.len() > CONTEXT_ENTRIES) {
            return Err("too many firmware sections".into());
        }
        let mut images = Vec::new();
        for (group, table) in groups.iter().zip([CONTEXT_LMAC, CONTEXT_UMAC, CONTEXT_PAGED]) {
            for (i, &(_, start, len)) in group.iter().enumerate() {
                let image = Dma::new(len, PAGE)?;
                image.put(0, &self.fw.blob[start..start + len]);
                self.context.put_u64(table + i * 8, image.addr());
                images.push(image);
            }
        }
        self.images = images;
        Ok(())
    }

    fn fill_rx(&self) {
        for (i, buffer) in self.rx_buffers.iter().enumerate() {
            self.rx_free.put_u64(i * 8, buffer.addr() | i as u64);
        }
    }

    /// Starts the firmware: it reads its sections from memory itself.
    fn start_firmware(&mut self) -> Result<(), String> {
        self.wr(INT, !0);
        self.wr(INT_MASK, 0);
        self.wr(DRV_GP1_CLEAR, SW_RFKILL);
        self.wr(DRV_GP1_CLEAR, CMD_BLOCKED);
        self.wr(INT, !0);
        self.apm_init()?;
        self.configure_nic();
        unsafe { ((self.mmio + INT_COALESCING) as *mut u8).write_volatile(COALESCING) };
        self.set(SHADOW_CTRL, SHADOW_ALL);
        self.wr(INT_MASK, INT_ALL);
        self.fill_rx();
        self.fill_context()?;
        let context = self.context.addr();
        self.wr(CONTEXT_BASE, context as u32);
        self.wr(CONTEXT_BASE + 4, (context >> 32) as u32);
        self.lock()?;
        self.prph_write(MAC_LTR, LTR_ENABLE_ALL);
        self.prph_write(UMAC_LTR, LTR_250_US);
        self.prph_write(CPU_INIT_RUN, 1);
        self.unlock();
        crate::kprintln!("wifi: firmware started, context at {context:#x}");
        Ok(())
    }

    /// The firmware set up receive: give it buffers.
    fn open_rx(&mut self) {
        const FIRST_BUFFERS: u32 = 8;
        self.fill_rx();
        self.wr(RX_WRITE_INDEX, FIRST_BUFFERS);
        self.rx_open = true;
    }

    fn sram(&self, addr: u32, words: usize) -> Vec<u32> {
        self.wr(MEM_ADDR, addr);
        (0..words).map(|_| self.rd(MEM_DATA)).collect()
    }

    fn state(&self) {
        crate::kprintln!(
            "wifi: GP_CNTRL {:08x} INT {:08x} FH {:08x} DRV_GP1 {:08x} RESET {:08x}",
            self.rd(GP_CNTRL),
            self.rd(INT),
            self.rd(FH_INT),
            self.rd(DRV_GP1),
            self.rd(RESET)
        );
        if self.lock().is_ok() {
            for (name, reg) in PRPH_REPORT {
                crate::kprintln!("wifi: {name} {:08x}", self.prph_read(reg));
            }
            for (name, addr) in [("lmac", self.lmac_log), ("umac", self.umac_log)] {
                if addr != 0 {
                    crate::kprintln!("wifi: {name} error log at {addr:#x}: {:08x?}", self.sram(addr, ERROR_WORDS));
                }
            }
            self.unlock();
        }
    }

    /// Handles everything that came from the device.
    fn service(&mut self) -> Result<(), String> {
        let int = self.rd(INT);
        if int == !0 {
            return Err("the card stopped answering".into());
        }
        if int != 0 {
            self.wr(INT, int);
            self.wr(FH_INT, self.rd(FH_INT));
        }
        if int & INT_SW_ERROR != 0 {
            return Err("firmware error".into());
        }
        if int & INT_HW_ERROR != 0 {
            return Err("hardware error".into());
        }
        if int & INT_RF_KILL != 0 && self.rd(GP_CNTRL) & RADIO_ON == 0 {
            return Err("the radio was switched off".into());
        }
        if int & INT_ALIVE != 0 && !self.rx_open {
            crate::kprintln!("wifi: alive interrupt");
            self.open_rx();
        }
        self.receive();
        Ok(())
    }

    fn receive(&mut self) {
        let closed = self.rx_status.get_u16(0) as usize & (RX_COUNT - 1);
        if closed == self.rx_next {
            return;
        }
        while self.rx_next != closed {
            let page = self.rx_buffers[self.rx_next].bytes();
            self.packets(&page);
            self.rx_next = (self.rx_next + 1) % RX_COUNT;
        }
        let last = closed.checked_sub(1).unwrap_or(RX_COUNT - 1);
        self.wr(RX_WRITE_INDEX, last as u32 & !7);
    }

    /// Parses a receive buffer: it may hold several packets in a row.
    fn packets(&mut self, page: &[u8]) {
        let mut at = 0;
        while at + PACKET_HEADER < RX_BUFFER {
            let word = u32_at(page, at);
            let (code, group, idx, qid) = (page[at + 4], page[at + 5], page[at + 6], page[at + 7]);
            if word == FRAME_INVALID || (qid & !FROM_FIRMWARE == 0 && idx == 0 && code == 0 && group == 0) {
                break;
            }
            let len = 4 + (word & FRAME_SIZE) as usize;
            if len < PACKET_HEADER || len > RX_BUFFER - at {
                break;
            }
            self.packet(code, group, idx, qid, &page[at + PACKET_HEADER..at + len]);
            at += len.next_multiple_of(PACKET_ALIGN);
        }
    }

    fn packet(&mut self, code: u8, group: u8, idx: u8, qid: u8, data: &[u8]) {
        let legacy = group & !CMD_FAILED <= LONG;
        match code {
            ALIVE if legacy => {
                let status = u16_at(data, 0);
                self.lmac_log = u32_at(data, ALIVE_LMAC_LOG);
                self.umac_log = u32_at(data, ALIVE_UMAC_LOG);
                crate::kprintln!("wifi: alive status {status:#x}, {} bytes", data.len());
                self.alive = Some(status);
            }
            INIT_COMPLETE if legacy => self.init_done = true,
            SCAN_COMPLETE | SCAN_ITERATION if legacy => self.scan_done = true,
            RX_MPDU if legacy => self.frame(data),
            BAR_RELEASE if legacy && data.len() >= 8 => self.release_frames((u32_at(data, 4) >> 24 & 0x3F) as u8, u32_at(data, 4) as u16 & 0xFFF),
            FRAME_RELEASE if legacy && data.len() >= 4 => self.release_frames(data[0], u16_at(data, 2)),
            TX_CMD if legacy => self.transmitted(data),
            REPLY_ERROR if legacy => crate::kprintln!("wifi: firmware refused command {:#x}, error {:#x}", data.get(4).copied().unwrap_or(0), u32_at(data, 0)),
            _ if qid & FROM_FIRMWARE != 0 && self.notes < NOTES_MAX => {
                self.notes += 1;
                crate::kprintln!("wifi: note {group:02x}.{code:02x}, {} bytes", data.len());
            }
            _ => {}
        }
        if qid == COMMAND_QUEUE as u8 && idx == self.expected {
            self.reply = Some((group, data.to_vec()));
        }
    }

    /// The firmware sent a frame: move the queue tail.
    fn transmitted(&mut self, data: &[u8]) {
        const STATUS: usize = 40;
        const STATUS_MASK: u16 = 0xFF;
        const STATUS_OK: u16 = 1;
        const STATUS_DONE: u16 = 2;
        let count = data.first().copied().unwrap_or(0) as usize;
        let status = u16_at(data, STATUS) & STATUS_MASK;
        if !matches!(status, STATUS_OK | STATUS_DONE) {
            self.tx_failed += 1;
            if self.notes < NOTES_MAX {
                self.notes += 1;
                crate::kprintln!("wifi: frame not delivered, status {status:#x}");
            }
        }
        if let Some(tx) = &mut self.tx {
            let position = u32_at(data, STATUS + count * 4) as usize;
            if position < TX_COUNT {
                tx.finished(position);
            }
        }
    }

    /// A data frame from the access point: a handshake or a frame for the network.
    fn data_frame(&mut self, desc: &[u8], frame: &[u8]) {
        let status = u32_at(desc, 12);
        if frame.len() < FRAME_HEADER || frame[1] & FLAG_FROM_DS == 0 || status & RX_DUPLICATE != 0 || frame[0] & SUBTYPE_NO_DATA != 0 {
            return;
        }
        let mut at = FRAME_HEADER + if frame[0] & SUBTYPE_QOS != 0 { QOS_FIELD } else { 0 };
        let protected = frame[1] & FLAG_PROTECTED != 0;
        if protected {
            if status & RX_CIPHER_MASK != RX_CIPHER_CCM || status & RX_DECRYPTED != RX_DECRYPTED {
                return;
            }
            at += CCMP_HEADER;
        }
        if desc[RX_FLAGS2] & RX_PADDED != 0 {
            at += FRAME_PADDING;
        }
        let Some(payload) = frame.get(at + LLC_SIZE..) else { return };
        let ether_type = u16::from_be_bytes([frame[at + 6], frame[at + 7]]);
        // Broadcast frames go at a low rate and say nothing about the link.
        if frame[FRAME_DESTINATION] & 1 == 0 {
            self.last_rate = u32_at(desc, RX_RATE);
            SPEED.store(rate_mbit(self.last_rate), Ordering::Relaxed);
            let kind = (self.last_rate >> RATE_MODE_POS & RATE_MODE_MASK) as usize;
            self.rx_kinds[kind.min(RATE_KINDS - 1)] += 1;
            *self.rate_frames.entry(rate_mbit(self.last_rate)).or_insert(0) += 1;
            self.rx_retried += (frame[1] & FLAG_RETRY != 0) as u64;
            self.rx_energy[0] += desc[RX_ENERGY_A] as u64;
            self.rx_energy[1] += desc[RX_ENERGY_B] as u64;
        }
        // The frame number is needed by the queue anyway, so the handshake also goes through it.
        let sender = &frame[FRAME_BSSID..FRAME_BSSID + 6];
        let carried = if ether_type == ETHER_EAPOL {
            self.eapol.push(payload.to_vec());
            None
        } else if !protected || sender == self.mac {
            // Unencrypted frames on a protected network are the easiest to forge.
            None
        } else {
            let mut ethernet = Vec::with_capacity(payload.len() + 14);
            ethernet.extend_from_slice(&frame[FRAME_DESTINATION..FRAME_DESTINATION + 6]);
            ethernet.extend_from_slice(sender);
            ethernet.extend_from_slice(&ether_type.to_be_bytes());
            ethernet.extend_from_slice(payload);
            Some(ethernet)
        };
        self.sort_frame(desc, frame, carried);
    }

    /// A frame from the air.
    fn frame(&mut self, desc: &[u8]) {
        if desc.len() < RX_DESC || u32_at(desc, 12) & RX_STATUS_OK != RX_STATUS_OK {
            return;
        }
        let len = u16_at(desc, 0) as usize;
        let Some(frame) = desc.get(RX_DESC..RX_DESC + len) else { return };
        self.frames += 1;
        if frame.len() < FRAME_HEADER {
            return;
        }
        match frame[0] & FRAME_TYPE {
            TYPE_DATA => return self.data_frame(desc, frame),
            TYPE_MANAGEMENT => {}
            _ => return,
        }
        if !matches!(frame[0], FRAME_BEACON | FRAME_PROBE_RESPONSE) {
            let ours = frame[FRAME_DESTINATION..FRAME_DESTINATION + 6] == self.mac && frame[FRAME_SENDER..FRAME_SENDER + 6] == self.bssid;
            if ours {
                if matches!(frame[0], FRAME_DEAUTH | FRAME_DISASSOC) {
                    crate::kprintln!("wifi: the access point dropped us, reason {}", u16_at(frame, FRAME_HEADER));
                    self.lost = true;
                } else {
                    self.management.push(frame.to_vec());
                }
            }
            return;
        }
        let energy = match (desc[RX_ENERGY_A], desc[RX_ENERGY_B]) {
            (0, b) => b,
            (a, 0) => a,
            (a, b) => a.min(b),
        };
        let Some(network) = network_from(frame, desc[RX_CHANNEL], -(energy as i32), u32_at(desc, RX_TIME)) else { return };
        let bssid = network.bssid;
        self.networks
            .entry(bssid)
            .and_modify(|n| {
                n.signal = n.signal.max(network.signal);
                (n.tsf, n.rx_time) = (network.tsf, network.rx_time);
                n.ht = n.ht.or(network.ht);
                n.vht = n.vht.or(network.vht);
                n.wmm |= network.wmm;
                if network.dtim != 0 {
                    (n.dtim, n.dtim_left) = (network.dtim, network.dtim_left);
                }
            })
            .or_insert(network);
    }

    /// Waits for a condition, handling everything that comes from the device.
    fn wait(&mut self, what: &str, ms: u64, done: impl Fn(&Self) -> bool) -> Result<(), String> {
        let end = interrupts::ticks() + (ms * interrupts::HZ / 1000).max(1);
        loop {
            self.service()?;
            self.drain_eapol()?;
            if done(self) {
                return Ok(());
            }
            let now = interrupts::ticks();
            if now >= end {
                return Err(format!("{what}: no answer"));
            }
            task::sleep_until(now + 1);
        }
    }

    fn wait_alive(&mut self) -> Result<(), String> {
        let end = interrupts::ticks() + ALIVE_MS * interrupts::HZ / 1000;
        let kick = interrupts::ticks() + ALIVE_KICK_MS * interrupts::HZ / 1000;
        while self.alive.is_none() {
            self.service()?;
            let now = interrupts::ticks();
            if !self.rx_open && now >= kick {
                crate::kprintln!("wifi: no alive interrupt, opening the receive queue anyway");
                self.open_rx();
            }
            if now >= end {
                return Err("the firmware did not start".into());
            }
            task::sleep_until(now + 1);
        }
        match self.alive {
            Some(ALIVE_OK) => Ok(()),
            status => Err(format!("firmware reported failure {status:x?}")),
        }
    }

    /// A command to the queue and waiting for the answer. The firmware rejects group 0, so
    /// such commands go in group 1.
    fn send(&mut self, group: u8, id: u8, data: &[u8]) -> Result<Vec<u8>, String> {
        self.send_versioned(group, id, 0, data)
    }

    fn send_versioned(&mut self, group: u8, id: u8, version: u8, data: &[u8]) -> Result<Vec<u8>, String> {
        self.post(group, id, version, data)?;
        self.wait(&format!("command {group:x}.{id:02x}"), COMMAND_MS, |c| c.reply.is_some())?;
        let (flags, reply) = self.reply.take().unwrap_or_default();
        if flags & CMD_FAILED != 0 {
            return Err(format!("command {group:x}.{id:02x} failed"));
        }
        Ok(reply)
    }

    /// Puts a command into the queue and does not wait for an answer.
    fn post(&mut self, group: u8, id: u8, version: u8, data: &[u8]) -> Result<(), String> {
        let idx = self.tx_next;
        let len = CMD_HEADER + data.len();
        if len > CMD_SPACE {
            return Err("command is too long".into());
        }
        let size = (data.len() as u16).to_le_bytes();
        let slot = idx % CMD_SLOTS * CMD_SPACE;
        self.command.put(slot, &[id, group, idx as u8, COMMAND_QUEUE as u8, size[0], size[1], 0, version]);
        self.command.put(slot + CMD_HEADER, data);
        let tfd = idx * TFD_SIZE;
        self.tx_ring.put(tfd, &[0; TFD_SIZE]);
        let first = len.min(FIRST_TB);
        self.tx_ring.put(tfd, &(1 + (len > FIRST_TB) as u16).to_le_bytes());
        self.tx_ring.put(tfd + 2, &(first as u16).to_le_bytes());
        self.tx_ring.put_u64(tfd + 4, self.command.addr() + slot as u64);
        if len > FIRST_TB {
            self.tx_ring.put(tfd + 12, &((len - FIRST_TB) as u16).to_le_bytes());
            self.tx_ring.put_u64(tfd + 14, self.command.addr() + (slot + FIRST_TB) as u64);
        }
        self.tx_next = (idx + 1) % TX_COUNT;
        self.reply = None;
        self.expected = idx as u8;
        self.wr(TX_WRITE_INDEX, COMMAND_QUEUE << 16 | self.tx_next as u32);
        Ok(())
    }

    fn send_words(&mut self, group: u8, id: u8, words: &[u32]) -> Result<Vec<u8>, String> {
        let data: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        self.send(group, id, &data)
    }

    fn read_mac(&mut self) -> Result<(), String> {
        const STRAP: usize = MAC_BASE + 8;
        const OTP: usize = MAC_BASE;
        self.lock()?;
        let read = |at: usize| {
            let (a, b) = (self.rd(at).to_le_bytes(), self.rd(at + 4).to_le_bytes());
            [a[3], a[2], a[1], a[0], b[1], b[0]]
        };
        let good = |m: &[u8; 6]| *m != [0; 6] && *m != [0xFF; 6] && *m != [0x02, 0xCC, 0xAA, 0xFF, 0xEE, 0] && m[0] & 1 == 0;
        let strap = read(STRAP);
        self.mac = if good(&strap) { strap } else { read(OTP) };
        self.unlock();
        if good(&self.mac) { Ok(()) } else { Err(format!("no valid address, read {}", hex6(&self.mac))) }
    }

    /// What this module can do: antennas and allowed channels.
    fn read_nvm(&mut self) -> Result<(), String> {
        let info = self.send(REGULATORY, NVM_INFO, &[0; 4])?;
        crate::kprintln!("wifi: module description: {} bytes", info.len());
        let wide = flag(&self.fw.api, API_REGULATORY_V4);
        let mac_sku = u32_at(&info, NVM_MAC_SKU);
        let chains = |config: u32, pos: u32| config >> pos & 0xF;
        self.tx_chains = u32_at(&info, NVM_TX) & chains(self.fw.phy_config, 16);
        self.rx_chains = u32_at(&info, NVM_RX) & chains(self.fw.phy_config, 20);
        self.lar = u32_at(&info, NVM_LAR) != 0 && flag(&self.fw.capabilities, CAPA_LAR);
        let profile = |i: usize| {
            if wide { u32_at(&info, NVM_CHANNELS + i * 4) } else { u16_at(&info, NVM_CHANNELS + i * 2) as u32 }
        };
        self.channels = CHANNELS
            .iter()
            .enumerate()
            .filter(|&(i, _)| profile(i) & CHANNEL_VALID != 0 && (i < CHANNELS_2GHZ || mac_sku & BAND_5GHZ != 0))
            .map(|(i, &number)| (number, if i < CHANNELS_2GHZ { BAND_FLAG_2GHZ } else { BAND_FLAG_5GHZ }))
            .take(SCAN_CHANNEL_MAX)
            .collect();
        crate::kprintln!("wifi: {} channels, antennas tx {:x} rx {:x}, regulatory by firmware: {}", self.channels.len(), self.tx_chains, self.rx_chains, self.lar);
        Ok(())
    }

    /// The first stage after startup: the firmware is ready to accept settings.
    fn initialize(&mut self) -> Result<(), String> {
        self.send_words(SYSTEM, INIT_EXTENDED, &[INIT_NVM])?;
        self.send_words(REGULATORY, NVM_DONE, &[0])?;
        self.wait("init complete", INIT_MS, |c| c.init_done)?;
        crate::kprintln!("wifi: firmware initialized");
        self.read_mac()?;
        crate::kprintln!("wifi: address {}", hex6(&self.mac));
        self.read_nvm()
    }

    fn configure(&mut self) -> Result<(), String> {
        self.lock()?;
        self.send_words(LONG, TX_ANTENNAS, &[self.tx_chains])?;
        self.send_words(LONG, BT_CONFIG, &[BT_COEX_SHARED, 0])?;
        self.send_words(SYSTEM, SOC_CONFIG, &[0, XTAL_LATENCY])?;
        // Without this capability the firmware crashes on the command itself.
        if flag(&self.fw.capabilities, CAPA_DQA) {
            self.send_words(DATA_PATH, DQA_ENABLE, &[COMMAND_QUEUE])?;
        }
        if self.pci.read(PCIE_CONTROL_2) & LTR_ENABLED != 0 {
            self.send_words(LONG, LTR_CONFIG, &[1, 0, 0, 0, 0, 0, 0, 0])?;
        }
        self.send_words(LONG, POWER_TABLE, &[0])?;
        if self.lar {
            let mut mcc = [0u8; 28];
            mcc[..2].copy_from_slice(&MCC_WORLD.to_le_bytes());
            mcc[2] = MCC_GET_CURRENT;
            self.send(LONG, MCC_UPDATE, &mcc)?;
        }
        if !flag(&self.fw.api, API_REDUCED_SCAN_CONFIG) {
            return Err("this firmware needs the old scan setup".into());
        }
        let mut config = [0u8; 12];
        config[4..8].copy_from_slice(&self.tx_chains.to_le_bytes());
        config[8..12].copy_from_slice(&self.rx_chains.to_le_bytes());
        self.send(LONG, SCAN_CONFIG, &config)?;
        self.unlock();
        crate::kprintln!("wifi: ready to scan");
        Ok(())
    }

    /// Passive scanning of all allowed channels: we do not transmit on the air ourselves.
    fn scan_command(&self) -> Vec<u8> {
        let mut cmd = vec![0u8; SCAN_SIZE];
        cmd[4] = SCAN_PRIORITY;
        let g = SCAN_GENERAL;
        let flags = FLAG_PASS_ALL | FLAG_NOTIFY_ITERATION | FLAG_ADAPTIVE | FLAG_PASSIVE;
        cmd[g..g + 2].copy_from_slice(&flags.to_le_bytes());
        cmd[g + 4] = DWELL_ACTIVE;
        cmd[g + 5] = DWELL_ACTIVE;
        cmd[g + 6] = ADWELL_2GHZ;
        cmd[g + 7] = ADWELL_5GHZ;
        cmd[g + 8] = ADWELL_SOCIAL;
        cmd[g + 10..g + 12].copy_from_slice(&ADWELL_BUDGET.to_le_bytes());
        cmd[g + 28] = SCAN_PRIORITY;
        cmd[g + 32] = DWELL_PASSIVE;
        cmd[g + 33] = DWELL_PASSIVE;
        let c = SCAN_CHANNELS;
        cmd[c] = SCAN_ORDER;
        cmd[c + 1] = self.channels.len() as u8;
        cmd[c + 2] = APS_FRIENDLY;
        cmd[c + 3] = APS_SOCIAL;
        let extended = flag(&self.fw.api, API_SCAN_EXT_CHAN);
        for (i, &(number, band)) in self.channels.iter().enumerate() {
            let at = c + 4 + i * 8 + 4;
            cmd[at] = number;
            if extended {
                cmd[at + 1] = band;
                cmd[at + 2] = 1;
            } else {
                cmd[at + 1] = 1;
            }
        }
        cmd[SCAN_PERIODIC + 2] = 1;
        let p = SCAN_PROBE_FRAME;
        cmd[p] = 0x40;
        cmd[p + 4..p + 10].fill(0xFF);
        cmd[p + 10..p + 16].copy_from_slice(&self.mac);
        cmd[p + 16..p + 22].fill(0xFF);
        let segment = |cmd: &mut [u8], index: usize, offset: u16, len: u16| {
            cmd[SCAN_PROBE + index * 4..SCAN_PROBE + index * 4 + 2].copy_from_slice(&offset.to_le_bytes());
            cmd[SCAN_PROBE + index * 4 + 2..SCAN_PROBE + index * 4 + 4].copy_from_slice(&len.to_le_bytes());
        };
        segment(&mut cmd, 0, 0, PROBE_HEADER as u16);
        let (rates_2, rates_5) = (PROBE_HEADER, PROBE_HEADER + RATES_2GHZ.len());
        cmd[p + rates_2..p + rates_5].copy_from_slice(&RATES_2GHZ);
        segment(&mut cmd, 1, rates_2 as u16, RATES_2GHZ.len() as u16);
        let end = rates_5 + RATES_5GHZ.len();
        cmd[p + rates_5..p + end].copy_from_slice(&RATES_5GHZ);
        segment(&mut cmd, 2, rates_5 as u16, RATES_5GHZ.len() as u16);
        segment(&mut cmd, 4, end as u16, 0);
        cmd
    }

    fn scan(&mut self) -> Result<(), String> {
        let version = self.fw.version_of(LONG, SCAN_REQUEST).unwrap_or(0);
        crate::kprintln!("wifi: scan command version {version}");
        if !matches!(version, 14 | 15) {
            return Err(format!("scan command version {version} is not supported"));
        }
        let cmd = self.scan_command();
        self.send(LONG, SCAN_REQUEST, &cmd)?;
        self.wait("scan", SCAN_MS, |c| c.scan_done)?;
        crate::kprintln!("wifi: scan finished, {} frames, {} access points", self.frames, self.networks.len());
        Ok(())
    }

    /// Stops the device: after this it does not touch our memory.
    fn stop(&self) {
        self.wr(INT_MASK, 0);
        self.set(LINK_POWER, LINK_POWER_OFF);
        self.set(HW_IF_CONFIG, CARD_PREPARE | ENABLE_PME);
        delay_us(1000);
        self.clear(LINK_POWER, LINK_POWER_OFF);
        delay_us(5000);
        self.set(RESET, STOP_MASTER);
        if !self.poll(RESET, MASTER_STOPPED, MASTER_STOPPED, 100) {
            crate::kprintln!("wifi: bus master did not stop");
        }
        self.clear(GP_CNTRL, INIT_DONE | ACCESS_REQUEST);
        self.reset();
        self.pci.write(PCI_COMMAND, self.pci.read(PCI_COMMAND) & !(1 << 2));
    }

    /// Starts the module until it is ready to listen to the air.
    fn bring_up(&mut self) -> Result<(), String> {
        let raw = self.rd(HW_REV);
        self.hw_rev = raw & 0xFFF0 | (raw << 2 & 0xC);
        crate::kprintln!("wifi: hw rev {raw:#x}, read as {:#x}, firmware {}", self.hw_rev, self.fw.name);
        self.start_hardware()?;
        self.start_firmware()?;
        self.wait_alive()?;
        self.initialize()?;
        self.configure()
    }
}

/// Network security from its RSN element: how to name it and whether we can connect.
fn read_rsn(value: &[u8]) -> (&'static str, bool) {
    let suite = |at: usize| value.get(at..at + 4).filter(|s| s[..3] == OUI).map(|s| s[3]);
    let count = |at: usize| u16_at(value, at) as usize;
    let pairwise = count(6);
    let akm_at = 8 + pairwise * 4;
    let akms = count(akm_at);
    let has = |first: usize, n: usize, kind: u8| (0..n).any(|i| suite(first + i * 4) == Some(kind));
    let psk = has(akm_at + 2, akms, AKM_PSK);
    let caps = u16_at(value, akm_at + 2 + akms * 4);
    let ciphers = suite(2) == Some(CIPHER_CCMP) && has(8, pairwise, CIPHER_CCMP);
    let label = if psk {
        "WPA2"
    } else if has(akm_at + 2, akms, AKM_SAE) {
        "WPA3"
    } else if has(akm_at + 2, akms, AKM_ENTERPRISE) {
        "802.1X"
    } else {
        "RSN"
    };
    (label, psk && ciphers && caps & RSN_MFP_REQUIRED == 0)
}

/// The device must answer only to us: without interrupts and with memory access.
fn claim(d: &pci::Device) {
    d.enable_bus_master();
    d.write(PCI_COMMAND, d.read(PCI_COMMAND) | INTX_OFF);
    let msix = d.read(MSIX_CAP);
    if msix & 0xFF == MSIX_ID && msix & MSIX_ENABLE != 0 {
        crate::kprintln!("wifi: turning off MSI-X left by the firmware");
        d.write(MSIX_CAP, msix & !MSIX_ENABLE);
    }
}

/// Checks the firmware file without the device: works in a VM too.
pub fn self_test() -> bool {
    wpa::self_test() && station::self_test()
}

/// The frame rate from the word with its parameters, Mbit/s.
fn rate_mbit(rate: u32) -> u32 {
    let streams = (rate >> RATE_STREAMS_POS & RATE_STREAMS) + 1;
    let width = (rate >> RATE_WIDTH_POS & RATE_WIDTH) as usize;
    let mcs = match rate >> RATE_MODE_POS & RATE_MODE_MASK {
        RATE_OFDM => return OFDM_MBIT[(rate & 7) as usize],
        RATE_HT => rate & RATE_HT_MCS,
        RATE_VHT => (rate & RATE_VHT_MCS).min(9),
        _ => return 0,
    };
    let Some(table) = STREAM_RATE.get(width) else { return 0 };
    let tenths = table[mcs as usize] * streams;
    if rate & RATE_SHORT_GUARD != 0 { tenths / 9 } else { tenths / 10 }
}

pub fn firmware_info() -> Result<String, String> {
    let blob = crate::task::as_system(|| fs::read(FIRMWARE)).map_err(|e| format!("{FIRMWARE}: {e}"))?;
    let size = blob.len();
    let fw = Firmware::parse(blob)?;
    let [lmac, umac, paged] = fw.groups();
    Ok(format!(
        "{}: {} KiB, {}, sections {} + {} + {}, scan command v{}",
        FIRMWARE,
        size >> 10,
        fw.name,
        lmac.len(),
        umac.len(),
        paged.len(),
        fw.version_of(LONG, SCAN_REQUEST).unwrap_or(0)
    ))
}

fn device() -> Option<pci::Device> {
    pci::scan().into_iter().find(|d| (d.vendor, d.device) == DEVICE)
}

/// A module ready to listen to the air.
fn open_card() -> Result<Card, String> {
    let d = device().ok_or("no supported Wi-Fi module on this computer")?;
    let blob = task::as_system(|| fs::read(FIRMWARE)).map_err(|e| format!("{FIRMWARE}: {e}"))?;
    let fw = Firmware::parse(blob)?;
    claim(&d);
    let mut card = Card::new(d, fw)?;
    if let Err(e) = card.bring_up() {
        crate::kprintln!("wifi: stopped: {e}");
        card.state();
        card.stop();
        return Err(e);
    }
    Ok(card)
}

fn set_status(text: &str) {
    crate::kprintln!("wifi: {text}");
    *STATUS.lock() = text.into();
}

fn publish(card: &Card) {
    let mut list: Vec<Network> = card.networks.values().cloned().collect();
    list.sort_by_key(|n| -n.signal);
    for n in &list {
        crate::kprintln!("wifi: {:>4} dBm ch {:>3} {:6} {} {}", n.signal, n.channel, n.security, hex6(&n.bssid), n.ssid);
    }
    *FOUND.lock() = list;
}

fn scan_only() {
    set_status("looking for networks");
    let result = open_card().and_then(|mut card| {
        let result = card.scan();
        publish(&card);
        card.stop();
        result
    });
    match result {
        Ok(()) => set_status(NOT_CONNECTED),
        Err(e) => set_status(&e),
    }
}

/// Picks the best access point among the suitable networks: 5 GHz beats a couple of decibels, but only
/// while we are trying the fast modes.
fn choose(found: &[Network], known: &[(String, [u8; 32])], limit: Mode) -> Option<(Network, [u8; 32])> {
    let pick = |n: &&Network| n.usable && known.iter().any(|(ssid, _)| *ssid == n.ssid);
    let prefer_5ghz = limit == Mode::Vht;
    let score = |n: &&Network| n.signal + if prefer_5ghz && n.channel > CHANNELS[CHANNELS_2GHZ - 1] { BONUS_5GHZ } else { 0 };
    let best = found.iter().filter(pick).max_by_key(score)?;
    let pmk = known.iter().find(|(ssid, _)| *ssid == best.ssid)?.1;
    Some((best.clone(), pmk))
}

fn stay_connected(card: &mut Card, ssid: &str) -> Result<(), String> {
    let text = card.link_text();
    *JOINED.lock() = Some((ssid.into(), text.clone()));
    LINK.store(true, Ordering::Relaxed);
    WORKED.store(true, Ordering::Relaxed);
    *NEW_LINK.lock() = Some(card.mac);
    set_status(&format!("connected to {ssid}, {text}"));
    let start = interrupts::ticks();
    let silence_end = start + SILENCE_SECONDS * interrupts::HZ;
    let mut report_at = start + LINK_REPORT_SECONDS * interrupts::HZ;
    let mut reported = 0;
    let mut received = crate::net::traffic().0;
    let mut result = Ok(());
    loop {
        if matches!(*REQUEST.lock(), Some(Request::Leave | Request::Join(..))) {
            break;
        }
        REQUEST.lock().take_if(|r| matches!(r, Request::Scan));
        let mut worked = false;
        result = card.service().and_then(|()| card.drain_eapol()).and_then(|()| card.drain_actions()).and_then(|()| {
            // Frames are handled right here: the acknowledgement goes on the air in the same pass,
            // not after a round through all threads.
            worked = crate::net::poll() > 0;
            card.forward().map(|busy| worked |= busy)
        });
        if result.is_err() || card.lost {
            break;
        }
        card.expire_frames();
        let now = interrupts::ticks();
        if card.delivered == 0 && now >= silence_end && card.fast() {
            result = Err("no data in this mode".into());
            break;
        }
        if now >= report_at {
            report_at = now + TRAFFIC_REPORT_SECONDS * interrupts::HZ;
            if card.delivered != reported {
                reported = card.delivered;
                let (bytes, sent, disordered, resent) = crate::net::traffic();
                let rate = (bytes - received) / TRAFFIC_REPORT_SECONDS / 1024;
                received = bytes;
                crate::kprintln!("wifi: {text}, {rate} KB/s, {}, sent {sent}, out of order {disordered}, resent {resent}", card.counters(SPEED.load(Ordering::Relaxed)));
                crate::kprintln!("wifi: {}", card.air_report());
                report();
            }
        }
        if TX_FRAMES.lock().is_empty() {
            crate::net::pause(worked);
        } else {
            task::yield_now();
        }
    }
    LINK.store(false, Ordering::Relaxed);
    SPEED.store(0, Ordering::Relaxed);
    *JOINED.lock() = None;
    TX_FRAMES.lock().clear();
    RX_FRAMES.lock().clear();
    if card.lost {
        return Err("the connection was lost".into());
    }
    result
}

/// One attempt: start, find the network, connect and work until the link drops.
fn connect_once(known: &[(String, [u8; 32])], save: bool) -> Result<(), String> {
    set_status("looking for the network");
    let mut card = open_card()?;
    let result = (|| {
        card.scan()?;
        publish(&card);
        let list = FOUND.lock().clone();
        let limit = limit();
        let (bss, pmk) = choose(&list, known, limit).ok_or("the network is not in range or not WPA2-PSK")?;
        set_status(&format!("connecting to {}", bss.ssid));
        card.join(&bss, pmk, limit)?;
        if save {
            remember(&bss.ssid, &pmk);
        }
        report();
        stay_connected(&mut card, &bss.ssid)
    })();
    if let Err(e) = &result {
        crate::kprintln!("wifi: stopped: {e}");
        card.state();
        // Data did not flow: maybe the mode is to blame, not the network.
        if card.delivered == 0 {
            lower_limit();
        }
    }
    card.stop();
    result
}

fn limit() -> Mode {
    match LIMIT.load(Ordering::Relaxed) {
        0 => Mode::Legacy,
        1 => Mode::Ht,
        _ => Mode::Vht,
    }
}

fn lower_limit() {
    let now = limit() as u8;
    LIMIT.store(now.saturating_sub(1), Ordering::Relaxed);
}

fn session(known: &[(String, [u8; 32])], save: bool) {
    LIMIT.store(Mode::Vht as u8, Ordering::Relaxed);
    let mut attempt = 0;
    while attempt < ATTEMPTS {
        attempt += 1;
        match connect_once(known, save) {
            Ok(()) => {
                set_status(NOT_CONNECTED);
                return;
            }
            Err(e) => set_status(&format!("attempt {attempt}: {e}")),
        }
        report();
        if REQUEST.lock().is_some() {
            return;
        }
        // The link worked and dropped: this is not a failed attempt.
        if WORKED.swap(false, Ordering::Relaxed) {
            attempt = 0;
        }
    }
}

/// The module thread.
fn driver() {
    loop {
        WORKING.store(true, Ordering::Relaxed);
        let request = REQUEST.lock().take();
        WORKING.store(request.is_some(), Ordering::Relaxed);
        match request {
            Some(Request::Scan) => {
                scan_only();
                SCANS.fetch_add(1, Ordering::Relaxed);
                report();
            }
            Some(Request::Join(ssid, pmk)) => session(&[(ssid, pmk)], true),
            Some(Request::Known) => session(&known_networks(), false),
            Some(Request::Leave) | None => task::sleep_until(interrupts::ticks() + IDLE_TICKS),
        }
    }
}

fn ask(request: Request) {
    *REQUEST.lock() = Some(request);
    if !STARTED.swap(true, Ordering::Relaxed) {
        task::spawn("wifi", None, driver);
    }
}

/// Brings the link up at startup if networks are already saved.
pub fn start() {
    PRESENT.store(device().is_some(), Ordering::Relaxed);
    if !known_networks().is_empty() && available() {
        ask(Request::Known);
    }
}

/// Whether the computer has a Wi-Fi module. Besides Intel (PRESENT) we count the Realtek
/// RTL8723BE card: it is not a `device()` but a separate driver — without this the Wi-Fi
/// tab did not appear in the settings even though the card was up and catching networks.
pub fn available() -> bool {
    PRESENT.load(Ordering::Relaxed) || crate::rtl8723be::present()
}

/// The module is busy: scanning, connecting or holding the link.
pub fn busy() -> bool {
    WORKING.load(Ordering::Relaxed) || REQUEST.lock().is_some()
}

/// The last air report: the signal of the best antenna (dBm, negative) in the upper
/// bits, the share of retries in percent in the low byte. 0 — no report yet.
pub(crate) static AIR: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Signal (dBm) and the share of retries (%) of the last report, while there is a link.
pub fn air() -> Option<(i32, u32)> {
    let v = AIR.load(Ordering::Relaxed);
    (LINK.load(Ordering::Relaxed) && v != 0).then(|| (-((v >> 8) as i32), (v & 0xFF) as u32))
}

/// The receive rate, Mbit/s. 0 — no data yet.
pub fn speed() -> u32 {
    if device().is_none() && crate::rtl8723be::present() {
        return crate::rtl8723be::rx_speed();
    }
    SPEED.load(Ordering::Relaxed)
}

/// The name of the network we are connected to.
pub fn network() -> Option<String> {
    if device().is_none() && crate::rtl8723be::present() {
        return crate::rtl8723be::network();
    }
    JOINED.lock().as_ref().map(|j| j.0.clone())
}

/// The link mode: standard, channel width and number of streams.
pub fn link_mode() -> Option<String> {
    JOINED.lock().as_ref().map(|j| j.1.clone())
}

/// Scans for networks and returns them. While connected, returns the list from the moment of connection.
pub fn scan() -> Result<Vec<Network>, String> {
    if device().is_none() && crate::rtl8723be::present() {
        return crate::rtl8723be::scan();
    }
    if device().is_none() {
        set_status("no supported Wi-Fi module on this computer");
        return Err(status());
    }
    if LINK.load(Ordering::Relaxed) {
        return Ok(FOUND.lock().clone());
    }
    let seen = SCANS.load(Ordering::Relaxed);
    ask(Request::Scan);
    let end = interrupts::ticks() + SCAN_WAIT_SECONDS * interrupts::HZ;
    while SCANS.load(Ordering::Relaxed) == seen && interrupts::ticks() < end {
        task::sleep_until(interrupts::ticks() + interrupts::HZ / 4);
    }
    let found = FOUND.lock().clone();
    if found.is_empty() { Err(status()) } else { Ok(found) }
}

/// Connects to a network with this password. The key is saved once the link works.
pub fn connect(ssid: &str, password: &str) {
    let pmk = crate::crypto::wpa_psk(password.as_bytes(), ssid.as_bytes());
    // The Realtek card comes up with its own driver and does not go through the Intel thread (Request::Join) —
    // pass the request to its driver.
    if device().is_none() && crate::rtl8723be::present() {
        crate::rtl8723be::start_connect(ssid.into(), pmk);
        return;
    }
    ask(Request::Join(ssid.into(), pmk));
}

/// The join state for the Realtek card (its driver sets it, the Network window shows it).
pub fn set_rtl_status(s: String) {
    *RTL_STATUS.lock() = s;
}

/// Connects to one of the saved networks.
pub fn connect_known() {
    ask(Request::Known);
}

pub fn disconnect() {
    ask(Request::Leave);
}

pub fn status() -> String {
    // The Realtek card is a separate driver: its state is not in STATUS (a "no module" from the Intel
    // attempt may remain there). Connecting to a saved network is handled by its own driver.
    let text = if device().is_none() && crate::rtl8723be::present() {
        RTL_STATUS.lock().clone()
    } else {
        STATUS.lock().clone()
    };
    let mut chars = if text.is_empty() { NOT_CONNECTED.chars() } else { text.chars() };
    chars.next().map_or(String::new(), |first| first.to_uppercase().chain(chars).collect())
}

/// The last list found.
pub fn found() -> Vec<Network> {
    if device().is_none() && crate::rtl8723be::present() {
        return crate::rtl8723be::found();
    }
    FOUND.lock().clone()
}

/// A link appeared: an address for the network stack.
pub fn take_new_link() -> Option<[u8; 6]> {
    NEW_LINK.lock().take()
}

// --- The "card driver ↔ network stack" bridge. The queues and the link flag are shared: there is
// always one active card, so any driver (Intel or rtl8723be) can use them. ---

/// The driver reports: there is a link. The flag is raised, and the interface goes to the stack (with
/// the cable connected, the WifiNic becomes a second interface; without it, the primary one).
pub(crate) fn nic_up(mac: [u8; 6]) {
    LINK.store(true, Ordering::Relaxed);
    *NEW_LINK.lock() = Some(mac);
}

/// The link is gone: the stack no longer sends to this card, the queues are cleared.
pub(crate) fn nic_down() {
    LINK.store(false, Ordering::Relaxed);
    TX_FRAMES.lock().clear();
    RX_FRAMES.lock().clear();
}

/// The next outgoing Ethernet frame from the stack (None — the queue is empty). The driver wraps it
/// into an 802.11 data frame and sends it.
pub(crate) fn nic_take_tx() -> Option<Vec<u8>> {
    TX_FRAMES.lock().pop_front()
}

/// An incoming frame (already unwrapped into Ethernet by the driver) — to the network stack.
pub(crate) fn nic_deliver_rx(frame: Vec<u8>) {
    let mut q = RX_FRAMES.lock();
    if q.len() < QUEUE_MAX {
        q.push_back(frame);
    }
}

/// A Wi-Fi network for the network stack: Ethernet frames, the module thread does the rest.
pub struct WifiNic {
    pub mac: [u8; 6],
}

impl crate::net::Nic for WifiNic {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }
    fn send(&mut self, frame: &[u8]) -> bool {
        let mut queue = TX_FRAMES.lock();
        if !LINK.load(Ordering::Relaxed) || queue.len() >= QUEUE_MAX {
            return false;
        }
        queue.push_back(frame.to_vec());
        drop(queue);
        crate::rtl8723be::wake();
        true
    }
    fn recv(&mut self) -> Option<Vec<u8>> {
        RX_FRAMES.lock().pop_front()
    }
    fn link_up(&self) -> bool {
        LINK.load(Ordering::Relaxed)
    }
    fn wireless(&self) -> bool {
        true
    }
}

fn known_networks() -> Vec<(String, [u8; 32])> {
    let Ok(data) = task::as_system(|| fs::read(CONFIG)) else { return Vec::new() };
    let text = String::from_utf8_lossy(&data);
    let parse = |line: &str| {
        let (ssid, hex) = line.rsplit_once('\t')?;
        let mut key = [0u8; 32];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        Some((ssid.to_string(), key))
    };
    text.lines().filter_map(parse).collect()
}

/// Remembers a network (the PMK key, not the password) once connecting to it succeeded.
pub(crate) fn remember(ssid: &str, pmk: &[u8; 32]) {
    let mut list = known_networks();
    if list.iter().any(|(name, key)| name == ssid && key == pmk) {
        return;
    }
    list.retain(|(name, _)| name != ssid);
    list.push((ssid.into(), *pmk));
    save_known(&list);
}

fn save_known(list: &[(String, [u8; 32])]) {
    let text: String = list.iter().map(|(name, key)| format!("{name}\t{}\n", key.iter().map(|b| format!("{b:02x}")).collect::<String>())).collect();
    task::as_system(|| {
        let _ = if list.is_empty() { fs::remove(CONFIG) } else { fs::write(CONFIG, text.as_bytes()) };
        // Straight to disk: the network must survive even a sudden reboot.
        let _ = fs::sync();
    });
}

/// Saved networks (names) — for the settings.
pub fn saved_networks() -> Vec<String> {
    known_networks().into_iter().map(|(name, _)| name).collect()
}

/// The key of a saved network (for a driver that connects by itself).
pub(crate) fn saved_key(ssid: &str) -> Option<[u8; 32]> {
    known_networks().into_iter().find(|(name, _)| name == ssid).map(|(_, key)| key)
}

/// Forget a network: do not connect to it automatically anymore.
pub fn forget(ssid: &str) {
    let mut list = known_networks();
    list.retain(|(name, _)| name != ssid);
    save_known(&list);
}

/// Connect to a saved network without a password. false — no such network is saved.
pub fn connect_saved(ssid: &str) -> bool {
    let Some(pmk) = saved_key(ssid) else { return false };
    if device().is_none() && crate::rtl8723be::present() {
        crate::rtl8723be::start_connect(ssid.into(), pmk);
    } else {
        ask(Request::Join(ssid.into(), pmk));
    }
    true
}

/// A network list line for the terminal.
pub fn describe(n: &Network) -> String {
    format!("{:>4} dBm  ch {:>3}  {:5}  {}", n.signal, n.channel, n.security, if n.ssid.is_empty() { "(hidden)" } else { &n.ssid })
}

/// The startup log: put on the USB stick, it is needed to understand where everything stopped.
fn report() {
    save_report();
}

pub fn save_report() -> String {
    // The previous boot's report is not overwritten: the copy may have been there, and then a reboot happened.
    static FIRST: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
    if FIRST.swap(false, Ordering::Relaxed) {
        hwprobe::keep_previous("wifi.txt", "wifi-prev.txt");
    }
    let log = crate::serial::log_text();
    let lines: Vec<&str> = log.lines().filter(|l| l.contains("wifi:")).collect();
    hwprobe::save_file("wifi.txt", (lines.join("\n") + "\n").as_bytes())
}
