//! An HTTP client: GET at an `http://host[:port]/path` address (no HTTPS — there is no TLS).

use super::{TcpStream, resolve};
use crate::interrupts::HZ;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    /// The address the answer came from (after redirects).
    pub url: String,
}

struct Url {
    host: String,
    port: u16,
    path: String,
}

fn parse_url(url: &str) -> Result<Url, String> {
    let rest = if let Some(r) = url.strip_prefix("http://") {
        r
    } else if url.starts_with("https://") {
        return Err("https is not supported yet (no TLS), use http://".into());
    } else {
        url
    };
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().map_err(|_| format!("bad port in {url}"))?),
        None => (hostport, 80),
    };
    if host.is_empty() {
        return Err(format!("bad address: {url}"));
    }
    Ok(Url { host: host.into(), port, path: path.into() })
}

/// Parses a body with `Transfer-Encoding: chunked`.
fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(eol) = data.windows(2).position(|w| w == b"\r\n") {
        let size_str = core::str::from_utf8(&data[..eol]).unwrap_or("0");
        let size = usize::from_str_radix(size_str.split(';').next().unwrap_or("0").trim(), 16).unwrap_or(0);
        data = &data[eol + 2..];
        if size == 0 || data.len() < size {
            out.extend_from_slice(&data[..size.min(data.len())]);
            break;
        }
        out.extend_from_slice(&data[..size]);
        data = data.get(size + 2..).unwrap_or(&[]);
    }
    out
}

fn get_once(url: &str) -> Result<(u16, Vec<(String, String)>, Vec<u8>), String> {
    let u = parse_url(url)?;
    if !super::wait_online() {
        return Err("no network (no address from DHCP)".into());
    }
    let ip = resolve(&u.host).ok_or_else(|| format!("{}: host not found", u.host))?;
    let stream = TcpStream::connect(ip, u.port).map_err(|e| format!("{}: {e}", u.host))?;
    let host_header = if u.port == 80 { u.host.clone() } else { format!("{}:{}", u.host, u.port) };
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: GlassOS/0.2\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        u.path
    );
    stream.send(request.as_bytes()).map_err(String::from)?;
    let mut raw = Vec::new();
    loop {
        match stream.recv(15 * HZ) {
            Ok(chunk) if chunk.is_empty() => break,
            Ok(chunk) => raw.extend_from_slice(&chunk),
            Err(e) if !raw.is_empty() && e == "connection reset" => break,
            Err(e) => return Err(e.into()),
        }
        if raw.len() > 16 << 20 {
            return Err("response is too large".into());
        }
    }
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("bad response from server")?;
    let head = String::from_utf8_lossy(&raw[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines.next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse().ok()).ok_or("bad status line")?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let mut body = raw[end + 4..].to_vec();
    if headers.iter().any(|(k, v)| k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked")) {
        body = dechunk(&body);
    }
    Ok((status, headers, body))
}

/// GET following redirects (up to 5).
pub fn get(url: &str) -> Result<Response, String> {
    let mut url = if url.contains("://") { url.to_string() } else { format!("http://{url}") };
    for _ in 0..5 {
        let (status, headers, body) = get_once(&url)?;
        let location = headers.iter().find(|(k, _)| k == "location").map(|(_, v)| v.clone());
        match (status, location) {
            (301 | 302 | 303 | 307 | 308, Some(loc)) => {
                url = if loc.starts_with('/') {
                    let u = parse_url(&url)?;
                    format!("http://{}:{}{}", u.host, u.port, loc)
                } else {
                    loc
                };
            }
            _ => return Ok(Response { status, body, url }),
        }
    }
    Err("too many redirects".into())
}
