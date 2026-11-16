//! A sound file as a sample source: WAV (PCM 8/16/24/32-bit, float), MP3 (nanomp3,
//! MIT/Apache-2.0 — a port of minimp3) and OGG Vorbis (lewton, MIT/Apache-2.0, in third_party,
//! converted to no_std; the Ogg container is parsed here). The whole file is in memory; samples
//! are returned in pieces at its rate and channel count, seeking is supported.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

pub struct Track {
    pub rate: u32,
    pub channels: usize,
    /// The length in frames (samples per channel), if known.
    pub total: Option<u64>,
    pub format: &'static str,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Cover art from the tags (PNG/JPEG bytes as is).
    pub cover: Option<Vec<u8>>,
    inner: Inner,
}

enum Inner {
    Wav { data: Vec<u8>, start: usize, frames: u64, bits: u16, float: bool, pos: u64 },
    // The `reader` field refers to `_data`: it is declared earlier, so it is also destroyed earlier.
    Mp3 { reader: nanomp3::SliceReader<'static, i16>, _data: Vec<u8> },
    Vorbis(Vorbis),
}

impl Track {
    /// Detects the format by the contents.
    pub fn open(data: Vec<u8>) -> Result<Track, &'static str> {
        if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WAVE" {
            return wav(data);
        }
        if data.starts_with(b"OggS") {
            return Vorbis::open(data);
        }
        if nanomp3::detect(&data) {
            return mp3(data);
        }
        Err("unknown format (WAV, MP3 and OGG Vorbis are supported)")
    }

    /// Up to `frames` frames interleaved by channel; 0 — the end.
    pub fn read(&mut self, frames: usize, out: &mut Vec<i16>) -> usize {
        let ch = self.channels;
        match &mut self.inner {
            Inner::Wav { data, start, frames: total, bits, float, pos } => {
                let n = (frames as u64).min(total.saturating_sub(*pos)) as usize;
                let bytes = (*bits / 8) as usize;
                for f in 0..n {
                    for c in 0..ch {
                        let at = *start + ((*pos as usize + f) * ch + c) * bytes;
                        let s = match (*bits, *float) {
                            (8, _) => ((data[at] as i16) - 128) << 8,
                            (16, _) => i16::from_le_bytes([data[at], data[at + 1]]),
                            (24, _) => i16::from_le_bytes([data[at + 1], data[at + 2]]),
                            (32, true) => (f32::from_le_bytes(data[at..at + 4].try_into().unwrap()).clamp(-1.0, 1.0) * 32767.0) as i16,
                            (32, false) => i16::from_le_bytes([data[at + 2], data[at + 3]]),
                            _ => 0,
                        };
                        out.push(s);
                    }
                }
                *pos += n as u64;
                n
            }
            Inner::Mp3 { reader, .. } => {
                let start = out.len();
                out.resize(start + frames * ch, 0);
                let n = reader.read(&mut out[start..]).unwrap_or(0);
                out.truncate(start + n);
                n / ch
            }
            Inner::Vorbis(v) => v.read(frames, ch, out),
        }
    }

    /// Seek to frame `frame`.
    pub fn seek(&mut self, frame: u64) {
        let ch = self.channels;
        match &mut self.inner {
            Inner::Wav { frames, pos, .. } => *pos = frame.min(*frames),
            Inner::Mp3 { reader, .. } => {
                let _ = reader.seek(frame);
            }
            Inner::Vorbis(v) => v.seek(frame, ch),
        }
    }
}

fn wav(data: Vec<u8>) -> Result<Track, &'static str> {
    let le16 = |d: &[u8], at: usize| u16::from_le_bytes([d[at], d[at + 1]]);
    let le32 = |d: &[u8], at: usize| u32::from_le_bytes(d[at..at + 4].try_into().unwrap());
    let (mut fmt, mut body) = (None, None);
    let mut at = 12;
    while at + 8 <= data.len() {
        let (id, len) = (&data[at..at + 4], le32(&data, at + 4) as usize);
        let b = at + 8;
        match id {
            b"fmt " if len >= 16 && b + 16 <= data.len() => fmt = Some((le16(&data, b), le16(&data, b + 2), le32(&data, b + 4), le16(&data, b + 14))),
            b"data" => body = Some((b, len.min(data.len() - b))),
            _ => {}
        }
        at = b + len + (len & 1);
    }
    let (format, channels, rate, bits) = fmt.ok_or("WAV without format")?;
    let (start, len) = body.ok_or("WAV without data")?;
    // 1 — integers, 3 — float, 0xFFFE — extensible (taken as integers/float by bit depth).
    let float = format == 3;
    if !matches!(format, 1 | 3 | 0xFFFE) || !matches!(bits, 8 | 16 | 24 | 32) || channels == 0 || rate == 0 {
        return Err("this WAV encoding is not supported");
    }
    let frames = (len / (bits as usize / 8 * channels as usize)) as u64;
    Ok(Track { rate, channels: channels as usize, total: Some(frames), format: "WAV", title: String::new(), artist: String::new(), album: String::new(), cover: None, inner: Inner::Wav { data, start, frames, bits, float, pos: 0 } })
}

fn mp3(data: Vec<u8>) -> Result<Track, &'static str> {
    let (title, artist, album, cover) = id3(&data);
    // The bytes are on the heap and do not move while the Track lives; the reader is destroyed first.
    let slice: &'static [u8] = unsafe { core::slice::from_raw_parts(data.as_ptr(), data.len()) };
    let mut reader = nanomp3::SliceReader::<i16>::new(slice);
    // The first frame — to learn the rate and the channels.
    let mut probe = [0i16; 2];
    let _ = reader.read(&mut probe);
    let rate = reader.sample_rate();
    let channels = reader.channels().map_or(2, |c| c as usize);
    if rate == 0 {
        return Err("no MPEG audio frames found");
    }
    let _ = reader.seek(0);
    let total = reader.total_samples();
    Ok(Track { rate, channels, total, format: "MP3", title, artist, album, cover, inner: Inner::Mp3 { reader, _data: data } })
}

/// Title, artist, album and cover art from the ID3v2 tag (TIT2, TPE1, TALB, APIC).
fn id3(d: &[u8]) -> (String, String, String, Option<Vec<u8>>) {
    let (mut title, mut artist, mut album, mut cover) = (String::new(), String::new(), String::new(), None);
    if d.len() < 10 || &d[0..3] != b"ID3" {
        return (title, artist, album, cover);
    }
    let version = d[3];
    let syncsafe = |b: &[u8]| (b[0] as usize) << 21 | (b[1] as usize) << 14 | (b[2] as usize) << 7 | b[3] as usize;
    let size = syncsafe(&d[6..10]).min(d.len() - 10);
    let mut at = 10;
    while at + 10 <= 10 + size {
        let id = &d[at..at + 4];
        let len = if version >= 4 { syncsafe(&d[at + 4..at + 8]) } else { u32::from_be_bytes(d[at + 4..at + 8].try_into().unwrap()) as usize };
        if id[0] == 0 || len == 0 || at + 10 + len > d.len() {
            break;
        }
        let body = &d[at + 10..at + 10 + len];
        let text = || id3_text(body);
        match id {
            b"TIT2" => title = text(),
            b"TPE1" => artist = text(),
            b"TALB" => album = text(),
            // The front cover (type 3) beats any other picture.
            b"APIC" => {
                if let Some((kind, pic)) = apic(body)
                    && (cover.is_none() || kind == 3)
                {
                    cover = Some(pic.to_vec());
                }
            }
            _ => {}
        }
        at += 10 + len;
    }
    (title, artist, album, cover)
}

/// An APIC frame: encoding, MIME (a zero-terminated string), picture type, description (a string
/// terminated by zero in the frame's encoding), then the picture itself. → (type, bytes).
fn apic(b: &[u8]) -> Option<(u8, &[u8])> {
    let enc = *b.first()?;
    let mime_end = 1 + b.get(1..)?.iter().position(|&c| c == 0)?;
    let kind = *b.get(mime_end + 1)?;
    let desc = mime_end + 2;
    let rest = b.get(desc..)?;
    let skip = if matches!(enc, 1 | 2) {
        // UTF-16: the end is two zero bytes at an even position.
        rest.chunks_exact(2).position(|c| c == [0, 0])? * 2 + 2
    } else {
        rest.iter().position(|&c| c == 0)? + 1
    };
    let pic = rest.get(skip..)?;
    (pic.len() > 16).then_some((kind, pic))
}

/// A picture from the METADATA_BLOCK_PICTURE comment (Vorbis): base64 of a FLAC PICTURE
/// block — type, MIME, description, dimensions, then the data.
fn vorbis_picture(b64: &str) -> Option<(u32, Vec<u8>)> {
    let block = base64(b64)?;
    let be = |at: usize| block.get(at..at + 4).map(|s| u32::from_be_bytes(s.try_into().unwrap()) as usize);
    let kind = be(0)? as u32;
    let mime_len = be(4)?;
    let desc_at = 8 + mime_len;
    let desc_len = be(desc_at)?;
    let len_at = desc_at + 4 + desc_len + 16;
    let len = be(len_at)?;
    let data = block.get(len_at + 4..len_at + 4 + len)?;
    Some((kind, data.to_vec()))
}

fn base64(s: &str) -> Option<Vec<u8>> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    };
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        acc = acc << 6 | value(c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// The text of an ID3 frame: encoding (0 — Latin-1, 1/2 — UTF-16, 3 — UTF-8), then the string.
fn id3_text(b: &[u8]) -> String {
    let Some((&enc, s)) = b.split_first() else { return String::new() };
    let text = match enc {
        1 | 2 => {
            let (little, s) = match s {
                [0xFF, 0xFE, rest @ ..] => (true, rest),
                [0xFE, 0xFF, rest @ ..] => (false, rest),
                _ => (enc == 1, s),
            };
            let units = s.chunks_exact(2).map(|c| if little { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) });
            char::decode_utf16(units).map(|c| c.unwrap_or('?')).collect()
        }
        3 => String::from_utf8_lossy(s).into_owned(),
        _ => s.iter().map(|&c| c as char).collect(),
    };
    String::from(text.trim_end_matches('\0').trim())
}

/// An Ogg page: data start, end, position (granule), segment table.
struct Page {
    granule: i64,
    continued: bool,
    segments: Vec<u8>,
    body: usize,
    next: usize,
}

fn page_at(d: &[u8], at: usize) -> Option<Page> {
    let h = d.get(at..at + 27)?;
    if &h[0..4] != b"OggS" {
        return None;
    }
    let n = h[26] as usize;
    let segments = d.get(at + 27..at + 27 + n)?.to_vec();
    let body = at + 27 + n;
    let len: usize = segments.iter().map(|&s| s as usize).sum();
    if body + len > d.len() {
        return None;
    }
    Some(Page { granule: i64::from_le_bytes(h[6..14].try_into().unwrap()), continued: h[5] & 1 != 0, segments, body, next: body + len })
}

/// The packets of an Ogg stream: pages in order, packet pieces are joined.
struct Ogg {
    data: Vec<u8>,
    page: usize,
    partial: Vec<u8>,
    packets: VecDeque<Vec<u8>>,
}

impl Ogg {
    fn packet(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some(p) = self.packets.pop_front() {
                return Some(p);
            }
            let page = page_at(&self.data, self.page)?;
            self.page = page.next;
            if !page.continued {
                self.partial.clear();
            }
            let mut at = page.body;
            for &len in &page.segments {
                self.partial.extend_from_slice(&self.data[at..at + len as usize]);
                at += len as usize;
                if len < 255 {
                    self.packets.push_back(core::mem::take(&mut self.partial));
                }
            }
        }
    }

    /// Start from page `at` (after a seek): an unfinished packet is dropped.
    fn restart(&mut self, at: usize) {
        self.page = at;
        self.partial.clear();
        self.packets.clear();
    }
}

struct Vorbis {
    ogg: Ogg,
    ident: lewton::header::IdentHeader,
    setup: lewton::header::SetupHeader,
    pwr: lewton::audio::PreviousWindowRight,
    /// Where the audio pages start (after the headers).
    audio_start: usize,
    /// Decoded data not yet returned; how many frames to drop after a seek.
    pending: VecDeque<i16>,
    skip: u64,
}

impl Vorbis {
    fn open(data: Vec<u8>) -> Result<Track, &'static str> {
        let mut ogg = Ogg { data, page: 0, partial: Vec::new(), packets: VecDeque::new() };
        let p1 = ogg.packet().ok_or("not an Ogg Vorbis file")?;
        let ident = lewton::header::read_header_ident(&p1).map_err(|_| "not an Ogg Vorbis file")?;
        let p2 = ogg.packet().ok_or("Ogg Vorbis without comments")?;
        let comments = lewton::header::read_header_comment(&p2).ok();
        let p3 = ogg.packet().ok_or("Ogg Vorbis without setup")?;
        let setup = lewton::header::read_header_setup(&p3, ident.audio_channels, (ident.blocksize_0, ident.blocksize_1)).map_err(|_| "broken Ogg Vorbis setup")?;
        let audio_start = ogg.page;
        // The length is the position of the last page.
        let mut total = None;
        let mut at = audio_start;
        while let Some(p) = page_at(&ogg.data, at) {
            if p.granule >= 0 {
                total = Some(p.granule as u64);
            }
            at = p.next;
        }
        let (rate, channels) = (ident.audio_sample_rate, ident.audio_channels as usize);
        let tag = |key: &str| comments.as_ref().and_then(|c| c.comment_list.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.clone())).unwrap_or_default();
        let (title, artist, album) = (tag("TITLE"), tag("ARTIST"), tag("ALBUM"));
        let mut cover: Option<(u32, Vec<u8>)> = None;
        for (k, v) in comments.as_ref().map_or(&[][..], |c| &c.comment_list[..]) {
            if k.eq_ignore_ascii_case("METADATA_BLOCK_PICTURE")
                && let Some(p) = vorbis_picture(v)
                && (cover.is_none() || p.0 == 3)
            {
                cover = Some(p);
            }
        }
        let cover = cover.map(|c| c.1);
        let v = Vorbis { ogg, ident, setup, pwr: lewton::audio::PreviousWindowRight::new(), audio_start, pending: VecDeque::new(), skip: 0 };
        Ok(Track { rate, channels, total, format: "OGG Vorbis", title, artist, album, cover, inner: Inner::Vorbis(v) })
    }

    fn read(&mut self, frames: usize, ch: usize, out: &mut Vec<i16>) -> usize {
        while self.pending.len() < frames * ch {
            let Some(p) = self.ogg.packet() else { break };
            let Ok(s) = lewton::audio::read_audio_packet_generic::<lewton::samples::InterleavedSamples<i16>>(&self.ident, &self.setup, &p, &mut self.pwr) else { continue };
            let mut samples = s.samples.as_slice();
            // After a seek — drop up to the needed frame.
            let drop = (self.skip as usize * ch).min(samples.len());
            samples = &samples[drop..];
            self.skip -= (drop / ch.max(1)) as u64;
            self.pending.extend(samples.iter().copied());
        }
        let n = (frames * ch).min(self.pending.len());
        out.extend(self.pending.drain(..n));
        n / ch.max(1)
    }

    /// Seeking: the page where the position has not reached the target yet, then count up.
    fn seek(&mut self, frame: u64, _ch: usize) {
        let mut at = self.audio_start;
        let mut from = (self.audio_start, 0u64);
        while let Some(p) = page_at(&self.ogg.data, at) {
            if p.granule >= 0 && p.granule as u64 >= frame {
                break;
            }
            if p.granule >= 0 {
                from = (p.next, p.granule as u64);
            }
            at = p.next;
        }
        self.ogg.restart(from.0);
        self.pending.clear();
        self.pwr = lewton::audio::PreviousWindowRight::new();
        self.skip = frame.saturating_sub(from.1);
    }
}

/// File information from its start (and end — for OGG), without reading and parsing all of it:
/// length (ms, 0 — unknown), title, artist, album. `head` is the start of the file,
/// `tail` its end (for MP3 — the piece right after the ID3 tag), `size` the size.
pub fn probe(head: &[u8], tail: &[u8], size: u64) -> Option<(u64, String, String, String)> {
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WAVE" {
        return probe_wav(head);
    }
    if head.starts_with(b"OggS") {
        return probe_ogg(head, tail);
    }
    probe_mp3(head, tail, size)
}

/// Where the ID3v2 tag of an MP3 ends (the audio starts there); 0 — no tag.
pub fn id3_end(head: &[u8]) -> usize {
    if head.len() >= 10 && &head[0..3] == b"ID3" {
        let b = &head[6..10];
        10 + ((b[0] as usize) << 21 | (b[1] as usize) << 14 | (b[2] as usize) << 7 | b[3] as usize)
    } else {
        0
    }
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn probe_wav(b: &[u8]) -> Option<(u64, String, String, String)> {
    let (mut at, mut rate, mut align) = (12, 0u32, 0u32);
    while at + 8 <= b.len() {
        let (id, len) = (&b[at..at + 4], le32(b, at + 4)? as usize);
        if id == b"fmt " {
            rate = le32(b, at + 12)?;
            align = u16::from_le_bytes(b.get(at + 20..at + 22)?.try_into().ok()?) as u32;
        } else if id == b"data" {
            let ms = if rate > 0 && align > 0 { len as u64 / align as u64 * 1000 / rate as u64 } else { 0 };
            return Some((ms, String::new(), String::new(), String::new()));
        }
        at += 8 + len + (len & 1);
    }
    None
}

fn probe_ogg(head: &[u8], tail: &[u8]) -> Option<(u64, String, String, String)> {
    let find = |b: &[u8], pat: &[u8]| b.windows(pat.len()).position(|w| w == pat);
    // The header: the rate.
    let ident = find(head, b"\x01vorbis")?;
    let rate = le32(head, ident + 12)?;
    // Comments: vendor length, the vendor itself, the number of lines, "KEY=value" lines.
    let (mut title, mut artist, mut album) = (String::new(), String::new(), String::new());
    if let Some(c) = find(head, b"\x03vorbis") {
        let mut at = c + 7;
        let vendor = le32(head, at)? as usize;
        at += 4 + vendor;
        let count = le32(head, at).unwrap_or(0);
        at += 4;
        for _ in 0..count.min(256) {
            let Some(len) = le32(head, at) else { break };
            let Some(text) = head.get(at + 4..at + 4 + len as usize) else { break };
            at += 4 + len as usize;
            let text = String::from_utf8_lossy(text);
            if let Some((k, v)) = text.split_once('=') {
                match k.to_ascii_uppercase().as_str() {
                    "TITLE" => title = String::from(v),
                    "ARTIST" => artist = String::from(v),
                    "ALBUM" => album = String::from(v),
                    _ => {}
                }
            }
        }
    }
    // The length is the position (granule) of the last page.
    let mut granule = 0u64;
    let mut at = 0;
    while let Some(p) = find(&tail[at..], b"OggS") {
        let page = at + p;
        if let Some(g) = tail.get(page + 6..page + 14) {
            let g = i64::from_le_bytes(g.try_into().ok()?);
            if g > 0 {
                granule = g as u64;
            }
        }
        at = page + 4;
    }
    let ms = if rate > 0 { granule * 1000 / rate as u64 } else { 0 };
    Some((ms, title, artist, album))
}

fn probe_mp3(head: &[u8], audio: &[u8], size: u64) -> Option<(u64, String, String, String)> {
    let (title, artist, album, _) = id3(head);
    let tag = id3_end(head);
    // The audio is the piece right after the tag (`audio` starts with `tag`).
    let head = audio;
    // The first frame after the tag.
    let start = (0..head.len().saturating_sub(4)).find(|&i| head[i] == 0xFF && head[i + 1] & 0xE0 == 0xE0 && (head[i + 2] >> 4) != 0xF && (head[i + 2] >> 2) & 3 != 3);
    let Some(f) = start else { return Some((0, title, artist, album)) };
    let h = &head[f..f + 4];
    let version = (h[1] >> 3) & 3; // 3 — MPEG1, 2 — MPEG2, 0 — MPEG2.5
    let layer = (h[1] >> 1) & 3; // 1 — Layer III
    let mpeg1 = version == 3;
    const RATES: [[u32; 3]; 3] = [[44100, 48000, 32000], [22050, 24000, 16000], [11025, 12000, 8000]];
    let rate = RATES[match version { 3 => 0, 2 => 1, _ => 2 }][((h[2] >> 2) & 3) as usize];
    const L3_V1: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const L3_V2: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let index = (h[2] >> 4) as usize;
    let kbps = if mpeg1 { L3_V1[index] } else { L3_V2[index] };
    let per_frame: u64 = match (layer, mpeg1) {
        (3, _) => 384,
        (1, false) => 576,
        _ => 1152,
    };
    let mono = (h[3] >> 6) == 3;
    // Xing/Info or VBRI: the number of frames for a variable bitrate.
    let side = match (mpeg1, mono) {
        (true, false) => 32,
        (true, true) | (false, false) => 17,
        (false, true) => 9,
    };
    let frames = {
        let x = f + 4 + side;
        match head.get(x..x + 4) {
            Some(b"Xing") | Some(b"Info") => {
                let flags = u32::from_be_bytes(head.get(x + 4..x + 8)?.try_into().ok()?);
                if flags & 1 != 0 { Some(u32::from_be_bytes(head.get(x + 8..x + 12)?.try_into().ok()?) as u64) } else { None }
            }
            _ => match head.get(f + 36..f + 40) {
                Some(b"VBRI") => Some(u32::from_be_bytes(head.get(f + 50..f + 54)?.try_into().ok()?) as u64),
                _ => None,
            },
        }
    };
    let ms = match frames {
        Some(n) if rate > 0 => n * per_frame * 1000 / rate as u64,
        // A constant bitrate: the length from the audio size.
        _ if kbps > 0 => size.saturating_sub((tag + f) as u64) * 8 / kbps as u64,
        _ => 0,
    };
    Some((ms, title, artist, album))
}

