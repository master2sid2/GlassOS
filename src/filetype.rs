//! The file type by contents — like the `file` utility in Linux: we look at the "magic"
//! header bytes and for known formats extract details (a program's bitness,
//! a picture's size, the audio parameters…).

use crate::fs;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// How many bytes of the start of the file to read (ISO 9660 has its signature at 32 KiB).
const HEAD: usize = 64 * 1024;

fn le16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2).map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}

fn le32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()))
}

fn le64(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8).map_or(0, |s| u64::from_le_bytes(s.try_into().unwrap()))
}

fn be16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2).map_or(0, |s| u16::from_be_bytes([s[0], s[1]]))
}

fn be32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |s| u32::from_be_bytes(s.try_into().unwrap()))
}

fn at(b: &[u8], off: usize, magic: &[u8]) -> bool {
    b.get(off..off + magic.len()) == Some(magic)
}

/// The file description and its MIME type.
pub struct Kind {
    pub text: String,
    pub mime: &'static str,
}

fn kind(text: impl Into<String>, mime: &'static str) -> Kind {
    Kind { text: text.into(), mime }
}

/// What file is at path `path`.
pub fn identify(path: &str) -> Result<Kind, fs::FsError> {
    if fs::is_dir(path) {
        return Ok(kind("directory", "inode/directory"));
    }
    let (head, size) = fs::read_range(path, 0, HEAD)?;
    if size == 0 {
        return Ok(kind("empty", "inode/x-empty"));
    }
    Ok(detect(&head, size))
}

/// The type by the first bytes `b` of a file of size `size`.
pub fn detect(b: &[u8], size: u64) -> Kind {
    if at(b, 0, b"\x7fELF") {
        return elf(b);
    }
    if at(b, 0, b"MZ") {
        return pe(b);
    }
    if at(b, 0, b"\x89PNG\r\n\x1a\n") {
        let color = match b.get(25) {
            Some(0) => "grayscale",
            Some(2) => "8-bit/color RGB",
            Some(3) => "colormap",
            Some(4) => "gray+alpha",
            Some(6) => "8-bit/color RGBA",
            _ => "unknown color",
        };
        let interlace = if b.get(28) == Some(&1) { "interlaced" } else { "non-interlaced" };
        return kind(format!("PNG image data, {} x {}, {color}, {interlace}", be32(b, 16), be32(b, 20)), "image/png");
    }
    if at(b, 0, b"\xff\xd8\xff") {
        return jpeg(b);
    }
    if at(b, 0, b"GIF87a") || at(b, 0, b"GIF89a") {
        let v = core::str::from_utf8(&b[3..6]).unwrap_or("");
        return kind(format!("GIF image data, version {v}, {} x {}", le16(b, 6), le16(b, 8)), "image/gif");
    }
    if at(b, 0, b"BM") && le32(b, 2) as u64 == size {
        let h = le32(b, 22) as i32;
        return kind(
            format!("PC bitmap, Windows 3.x format, {} x {} x {}", le32(b, 18) as i32, h.unsigned_abs(), le16(b, 28)),
            "image/bmp",
        );
    }
    if at(b, 0, b"\0\0\x01\0") && le16(b, 4) > 0 && le16(b, 4) < 64 {
        return kind(format!("MS Windows icon resource - {} icons", le16(b, 4)), "image/vnd.microsoft.icon");
    }
    if at(b, 0, b"RIFF") {
        return riff(b);
    }
    if at(b, 0, b"%PDF-") {
        let v: String = b[5..b.len().min(8)].iter().map(|&c| c as char).collect();
        return kind(format!("PDF document, version {v}"), "application/pdf");
    }
    if at(b, 0, b"PK\x03\x04") || at(b, 0, b"PK\x05\x06") {
        return zip(b);
    }
    if at(b, 0, b"\x1f\x8b") {
        let mut t = String::from("gzip compressed data");
        // The FNAME flag: the original file name after the 10-byte header.
        if b.get(3).is_some_and(|f| f & 0x08 != 0) {
            let name: String = b[10..].iter().take_while(|&&c| c != 0).take(100).map(|&c| c as char).collect();
            t += &format!(", was \"{name}\"");
        }
        return kind(t, "application/gzip");
    }
    if at(b, 0, b"BZh") {
        return kind("bzip2 compressed data", "application/x-bzip2");
    }
    if at(b, 0, b"\xfd7zXZ\0") {
        return kind("XZ compressed data", "application/x-xz");
    }
    if at(b, 0, b"\x28\xb5\x2f\xfd") {
        return kind("Zstandard compressed data", "application/zstd");
    }
    if at(b, 0, b"7z\xbc\xaf\x27\x1c") {
        return kind(format!("7-zip archive data, version {}.{}", b[6], b[7]), "application/x-7z-compressed");
    }
    if at(b, 0, b"Rar!\x1a\x07\x01\0") {
        return kind("RAR archive data, v5", "application/vnd.rar");
    }
    if at(b, 0, b"Rar!\x1a\x07\0") {
        return kind("RAR archive data, v4", "application/vnd.rar");
    }
    if at(b, 257, b"ustar") {
        return kind("POSIX tar archive", "application/x-tar");
    }
    if at(b, 0x8001, b"CD001") {
        let label: String = b[0x8028..0x8048].iter().map(|&c| c as char).collect();
        return kind(format!("ISO 9660 CD-ROM filesystem data '{}'", label.trim()), "application/x-iso9660-image");
    }
    if at(b, 0, b"SQLite format 3\0") {
        return kind("SQLite 3.x database", "application/vnd.sqlite3");
    }
    if at(b, 0, b"\x1bLua") {
        return kind(format!("Lua bytecode, version {}.{}", b[4] >> 4, b[4] & 0xF), "application/x-lua-bytecode");
    }
    if at(b, 0, b"fLaC") {
        return kind("FLAC audio bitstream data", "audio/flac");
    }
    if at(b, 0, b"OggS") {
        let codec = if at(b, 29, b"vorbis") { ", Vorbis audio" } else if at(b, 28, b"OpusHead") { ", Opus audio" } else { "" };
        return kind(format!("Ogg data{codec}"), "audio/ogg");
    }
    if at(b, 0, b"ID3") {
        return kind(format!("Audio file with ID3 version 2.{}, MPEG audio", b[3]), "audio/mpeg");
    }
    if b.len() > 2 && b[0] == 0xFF && b[1] & 0xE0 == 0xE0 && b[1] & 0x06 != 0 {
        return kind("MPEG ADTS, layer III audio", "audio/mpeg");
    }
    if at(b, 4, b"ftyp") {
        let brand: String = b[8..12].iter().map(|&c| c as char).collect();
        let (what, mime) = match brand.as_str() {
            "M4A " => ("MPEG v4 audio", "audio/mp4"),
            "qt  " => ("Apple QuickTime movie", "video/quicktime"),
            "heic" | "mif1" => ("HEIF image", "image/heic"),
            "avif" => ("AVIF image", "image/avif"),
            _ => ("ISO Media, MP4 video", "video/mp4"),
        };
        return kind(format!("{what} (brand {})", brand.trim()), mime);
    }
    if at(b, 0, b"\x1a\x45\xdf\xa3") {
        let webm = b[..b.len().min(64)].windows(4).any(|w| w == b"webm");
        return if webm { kind("WebM video", "video/webm") } else { kind("Matroska data", "video/x-matroska") };
    }
    if at(b, 0, b"\xca\xfe\xba\xbe") {
        return kind("compiled Java class data", "application/java-vm");
    }
    if at(b, 0, b"\0asm") {
        return kind(format!("WebAssembly (wasm) binary module version {}", le32(b, 4)), "application/wasm");
    }
    if let Some(k) = disk(b) {
        return k;
    }
    text(b, size)
}

/// An ELF program: bitness, byte order, type, CPU, interpreter.
fn elf(b: &[u8]) -> Kind {
    let bits64 = b.get(4) == Some(&2);
    let little = b.get(5) != Some(&2);
    let r16 = |o| if little { le16(b, o) } else { be16(b, o) };
    let r32 = |o| if little { le32(b, o) } else { be32(b, o) };
    let r64 = |o: usize| if little { le64(b, o) } else { (be32(b, o) as u64) << 32 | be32(b, o + 4) as u64 };
    let machine = match r16(18) {
        3 => "Intel 80386",
        0x3E => "x86-64",
        0x28 => "ARM",
        0xB7 => "ARM aarch64",
        0xF3 => "UCB RISC-V",
        8 => "MIPS",
        0x14 | 0x15 => "PowerPC",
        _ => "unknown arch",
    };
    let (phoff, phentsize, phnum) =
        if bits64 { (r64(32) as usize, r16(54) as usize, r16(56) as usize) } else { (r32(28) as usize, r16(42) as usize, r16(44) as usize) };
    // The interpreter (PT_INTERP) is a sign of dynamic linking.
    let interp = (0..phnum).find_map(|i| {
        let p = phoff + i * phentsize;
        (r32(p) == 3).then(|| {
            let (off, len) = if bits64 { (r64(p + 8) as usize, r64(p + 32) as usize) } else { (r32(p + 4) as usize, r32(p + 16) as usize) };
            b.get(off..off + len.saturating_sub(1)).map(|s| String::from_utf8_lossy(s).into_owned())
        })
    });
    // ET_DYN with an entry point is a program (PIE), without one — a library.
    let entry = if bits64 { r64(24) } else { r32(24) as u64 };
    let pie = r16(16) == 3 && (interp.is_some() || entry != 0);
    let ty = match r16(16) {
        1 => "relocatable",
        2 => "executable",
        3 if pie => "pie executable",
        3 => "shared object",
        4 => "core file",
        _ => "unknown type",
    };
    let mut t = format!("ELF {}-bit {} {ty}, {machine}, version {}", if bits64 { 64 } else { 32 }, if little { "LSB" } else { "MSB" }, b[6]);
    match interp {
        Some(Some(i)) => t += &format!(", dynamically linked, interpreter {i}"),
        Some(None) => t += ", dynamically linked",
        None if pie => t += ", static-pie linked",
        None if r16(16) == 2 => t += ", statically linked",
        None => {}
    }
    let mime = match r16(16) {
        3 if pie => "application/x-pie-executable",
        3 => "application/x-sharedlib",
        1 => "application/x-object",
        _ => "application/x-executable",
    };
    kind(t, mime)
}

/// A Windows/UEFI program (PE) or DOS (MZ).
fn pe(b: &[u8]) -> Kind {
    let pe = le32(b, 0x3C) as usize;
    if !at(b, pe, b"PE\0\0") {
        return kind("MS-DOS executable", "application/x-dosexec");
    }
    let machine = match le16(b, pe + 4) {
        0x14C => "Intel 80386",
        0x8664 => "x86-64",
        0xAA64 => "Aarch64",
        0x1C0 | 0x1C4 => "ARM",
        _ => "unknown arch",
    };
    let dll = le16(b, pe + 22) & 0x2000 != 0;
    let opt = pe + 24;
    let plus = le16(b, opt) == 0x20B;
    let subsystem = match le16(b, opt + 68) {
        1 => "(native)",
        2 => "(GUI)",
        3 => "(console)",
        10 => "(EFI application)",
        11 => "(EFI boot service driver)",
        12 => "(EFI runtime driver)",
        _ => "",
    };
    let what = if dll { "(DLL)" } else { "" };
    kind(
        format!("PE32{} executable {what}{subsystem} {machine}, for MS Windows", if plus { "+" } else { "" }).replace("  ", " "),
        if subsystem.contains("EFI") { "application/efi" } else { "application/vnd.microsoft.portable-executable" },
    )
}

/// JPEG: the standard (JFIF/Exif) and the size from the SOF marker.
fn jpeg(b: &[u8]) -> Kind {
    let mut t = String::from("JPEG image data");
    if at(b, 6, b"JFIF") {
        t += &format!(", JFIF standard {}.{:02}", b[11], b[12]);
    } else if at(b, 6, b"Exif") {
        t += ", Exif standard";
    }
    // Walk the markers up to SOFn: it holds the height, the width and the number of components.
    let mut i = 2;
    while i + 9 < b.len() && b[i] == 0xFF {
        let m = b[i + 1];
        let len = be16(b, i + 2) as usize;
        if (0xC0..=0xCF).contains(&m) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
            let kind_ = if m == 0xC2 { "progressive" } else { "baseline" };
            t += &format!(", {kind_}, precision {}, {}x{}, components {}", b[i + 4], be16(b, i + 7), be16(b, i + 5), b[i + 9]);
            break;
        }
        i += 2 + len;
    }
    kind(t, "image/jpeg")
}

/// RIFF: WAVE (audio), WebP, AVI.
fn riff(b: &[u8]) -> Kind {
    if at(b, 8, b"WAVE") {
        // Look for the "fmt " chunk.
        let mut i = 12;
        while i + 8 <= b.len() {
            let len = le32(b, i + 4) as usize;
            if at(b, i, b"fmt ") {
                let fmt = match le16(b, i + 8) {
                    1 => "Microsoft PCM",
                    3 => "IEEE float",
                    0x55 => "MPEG Layer 3",
                    0xFFFE => "WAVE_FORMAT_EXTENSIBLE",
                    _ => "unknown format",
                };
                let ch = match le16(b, i + 10) {
                    1 => String::from("mono"),
                    2 => String::from("stereo"),
                    n => format!("{n} channels"),
                };
                return kind(
                    format!("RIFF (little-endian) data, WAVE audio, {fmt}, {} bit, {ch} {} Hz", le16(b, i + 22), le32(b, i + 12)),
                    "audio/x-wav",
                );
            }
            i += 8 + len + (len & 1);
        }
        return kind("RIFF (little-endian) data, WAVE audio", "audio/x-wav");
    }
    if at(b, 8, b"WEBP") {
        return kind("RIFF (little-endian) data, Web/P image", "image/webp");
    }
    if at(b, 8, b"AVI ") {
        return kind("RIFF (little-endian) data, AVI", "video/x-msvideo");
    }
    kind("RIFF (little-endian) data", "application/octet-stream")
}

/// ZIP — and the formats built on it (Office documents, OpenDocument, JAR, APK).
fn zip(b: &[u8]) -> Kind {
    let name_len = le16(b, 26) as usize;
    let first = b.get(30..30 + name_len).map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default();
    let has = |s: &[u8]| b.windows(s.len()).any(|w| w == s);
    if first == "mimetype" {
        let mime: String = b[30 + name_len..].iter().take(60).take_while(|c| c.is_ascii_graphic()).map(|&c| c as char).collect();
        let what = if mime.contains("opendocument.text") {
            "OpenDocument Text"
        } else if mime.contains("opendocument.spreadsheet") {
            "OpenDocument Spreadsheet"
        } else if mime.contains("epub") {
            "EPUB document"
        } else {
            "OpenDocument"
        };
        return kind(what, "application/zip");
    }
    if has(b"word/") {
        return kind("Microsoft Word 2007+", "application/vnd.openxmlformats-officedocument.wordprocessingml.document");
    }
    if has(b"xl/") {
        return kind("Microsoft Excel 2007+", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet");
    }
    if has(b"ppt/") {
        return kind("Microsoft PowerPoint 2007+", "application/vnd.openxmlformats-officedocument.presentationml.presentation");
    }
    if has(b"AndroidManifest.xml") {
        return kind("Android package (APK)", "application/vnd.android.package-archive");
    }
    if has(b"META-INF/MANIFEST.MF") {
        return kind("Java archive data (JAR)", "application/java-archive");
    }
    kind(format!("Zip archive data, at least v{}.{} to extract", le16(b, 4) / 10, le16(b, 4) % 10), "application/zip")
}

/// Disk images: a partition table, a FAT/exFAT/NTFS boot sector.
fn disk(b: &[u8]) -> Option<Kind> {
    if b.len() < 512 || be16(b, 510) != 0x55AA {
        return None;
    }
    if at(b, 3, b"EXFAT   ") {
        return Some(kind("DOS/MBR boot sector, exFAT filesystem", "application/octet-stream"));
    }
    if at(b, 3, b"NTFS    ") {
        return Some(kind("DOS/MBR boot sector, NTFS filesystem", "application/octet-stream"));
    }
    if at(b, 82, b"FAT32") {
        let label: String = b[71..82].iter().map(|&c| c as char).collect();
        return Some(kind(format!("DOS/MBR boot sector, FAT (32 bit), label: \"{}\"", label.trim()), "application/octet-stream"));
    }
    if at(b, 54, b"FAT1") {
        let label: String = b[43..54].iter().map(|&c| c as char).collect();
        let bits = if at(b, 54, b"FAT12") { 12 } else { 16 };
        return Some(kind(format!("DOS/MBR boot sector, FAT ({bits} bit), label: \"{}\"", label.trim()), "application/octet-stream"));
    }
    let mut t = String::from("DOS/MBR boot sector");
    if at(b, 512, b"EFI PART") {
        t += "; GPT partition table";
    }
    for p in 0..4 {
        let e = 446 + p * 16;
        let ty = b[e + 4];
        if ty != 0 {
            t += &format!("; partition {}: ID=0x{ty:x}{}, start-CHS skipped, startsector {}, {} sectors", p + 1, if b[e] == 0x80 { ", active" } else { "" }, le32(b, e + 8), le32(b, e + 12));
        }
    }
    Some(kind(t, "application/octet-stream"))
}

/// Text: the encoding, line endings, scripts by "#!", markup.
fn text(b: &[u8], size: u64) -> Kind {
    if at(b, 0, b"\xff\xfe") || at(b, 0, b"\xfe\xff") {
        let order = if b[0] == 0xFF { "little-endian" } else { "big-endian" };
        return kind(format!("Unicode text, UTF-16, {order} text"), "text/plain");
    }
    let sample = &b[..b.len().min(HEAD)];
    // Binary data: zeros or many control characters.
    let control = sample.iter().filter(|&&c| c < 0x20 && !matches!(c, b'\n' | b'\r' | b'\t' | 0x0C | 0x1B)).count();
    // A multi-byte character cut at the read boundary is not an error.
    let utf8 = match core::str::from_utf8(sample) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && (size as usize) > sample.len(),
    };
    if sample.contains(&0) || control * 20 > sample.len() || !utf8 {
        return kind("data", "application/octet-stream");
    }
    let s = core::str::from_utf8(sample).unwrap_or_else(|e| core::str::from_utf8(&sample[..e.valid_up_to()]).unwrap());
    let ascii = s.is_ascii();
    let mut enc = String::from(if ascii { "ASCII text" } else if at(b, 0, b"\xef\xbb\xbf") { "Unicode text, UTF-8 (with BOM) text" } else { "Unicode text, UTF-8 text" });
    if s.lines().any(|l| l.len() > 300) {
        enc += ", with very long lines";
    }
    if s.contains("\r\n") {
        enc += ", with CRLF line terminators";
    } else if s.contains('\r') {
        enc += ", with CR line terminators";
    }
    if s.ends_with(|c| c != '\n') && (size as usize) == sample.len() {
        enc += ", with no line terminators";
    }
    let start = s.trim_start_matches('\u{feff}').trim_start();
    let lower: String = start.chars().take(64).collect::<String>().to_lowercase();
    if let Some(rest) = start.strip_prefix("#!") {
        let line = rest.lines().next().unwrap_or("").trim();
        let prog = line.split_whitespace().last().unwrap_or("").rsplit('/').next().unwrap_or("");
        let what = match prog {
            "sh" | "bash" | "dash" | "zsh" => "shell script",
            "python" | "python3" => "Python script",
            "lua" => "Lua script",
            "perl" => "Perl script",
            "node" => "Node.js script",
            _ => "script",
        };
        return kind(format!("{line} {what}, {enc} executable"), "text/x-script");
    }
    if lower.starts_with("<?xml") {
        let svg = s.contains("<svg");
        return if svg { kind(format!("SVG Scalable Vector Graphics image, {enc}"), "image/svg+xml") } else { kind(format!("XML document, {enc}"), "text/xml") };
    }
    if lower.starts_with("<!doctype html") || lower.starts_with("<html") {
        return kind(format!("HTML document, {enc}"), "text/html");
    }
    if lower.starts_with("<svg") {
        return kind(format!("SVG Scalable Vector Graphics image, {enc}"), "image/svg+xml");
    }
    if (start.starts_with('{') || start.starts_with('[')) && looks_json(start) {
        return kind("JSON text data", "application/json");
    }
    if start.starts_with("%!PS") {
        return kind("PostScript document text", "application/postscript");
    }
    kind(enc, "text/plain")
}

/// Looks like JSON: after the bracket — a quote, a digit, a bracket or a literal.
fn looks_json(s: &str) -> bool {
    let inner = s[1..].trim_start();
    inner.starts_with(['"', '{', '[', ']', '}', '-']) || inner.starts_with(|c: char| c.is_ascii_digit()) || ["true", "false", "null"].iter().any(|w| inner.starts_with(w))
}

/// Hex dump lines (like `xxd`): offset, bytes, characters.
pub fn hexdump(data: &[u8], base: u64) -> Vec<String> {
    data.chunks(16)
        .enumerate()
        .map(|(i, row)| {
            let mut line = format!("{:08x}: ", base + i as u64 * 16);
            for k in 0..16 {
                match row.get(k) {
                    Some(b) => line += &format!("{b:02x}"),
                    None => line += "  ",
                }
                if k % 2 == 1 {
                    line.push(' ');
                }
            }
            line.push(' ');
            line.extend(row.iter().map(|&b| if (0x20..0x7F).contains(&b) { b as char } else { '.' }));
            line
        })
        .collect()
}
