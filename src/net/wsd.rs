//! WS-Discovery (239.255.255.250:3702) and the device description over HTTP (port 5357).
//! The Windows 10 and 11 "Network" folder uses this: the old browser was removed there.
//! A Probe request finds neighbours, and the name and workgroup are in the metadata
//! (`<pub:Computer>NAME/Workgroup:GROUP</pub:Computer>`).

use super::discovery::{self, Found};
use super::identity::Identity;
use super::{Ip, TcpListener, TcpStream, UdpSocket, ip_str};
use crate::interrupts::{self, HZ};
use crate::task;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub const GROUP: Ip = [239, 255, 255, 250];
pub const PORT: u16 = 3702;
const HTTP_PORT: u16 = 5357;
const ENVELOPE: &str = "<soap:Envelope xmlns:soap=\"http://www.w3.org/2003/05/soap-envelope\" \
xmlns:wsa=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\" \
xmlns:wsd=\"http://schemas.xmlsoap.org/ws/2005/04/discovery\" \
xmlns:wsdp=\"http://schemas.xmlsoap.org/ws/2006/02/devprof\" \
xmlns:wsx=\"http://schemas.xmlsoap.org/ws/2004/09/mex\" \
xmlns:pnpx=\"http://schemas.microsoft.com/windows/pnpx/2005/10\" \
xmlns:pub=\"http://schemas.microsoft.com/windows/pub/2005/07\">";
const ANONYMOUS: &str = "http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous";
const DISCOVERY: &str = "http://schemas.xmlsoap.org/ws/2005/04/discovery";
const WELL_KNOWN: &str = "urn:schemas-xmlsoap-org:ws:2005:04:discovery";
const TYPES: &str = "wsdp:Device pub:Computer";

/// A pseudo-UUID from a string: the same one for the same computer.
fn uuid(seed: &[u8]) -> String {
    let h = crate::crypto::md5(seed);
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-3{:01x}{:02x}-a{:01x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        h[0], h[1], h[2], h[3], h[4], h[5], h[6] & 15, h[7], h[8] & 15, h[9], h[10], h[11], h[12], h[13], h[14], h[15]
    )
}

fn endpoint(id: &Identity) -> String {
    uuid(id.name.as_bytes())
}

fn message_id() -> String {
    uuid(&crate::arch::rdtsc().to_le_bytes())
}

fn envelope(action: &str, to: &str, relates: Option<&str>, body: &str, sequence: Option<u32>) -> String {
    let mut m = format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>{ENVELOPE}<soap:Header><wsa:To>{to}</wsa:To><wsa:Action>{action}</wsa:Action><wsa:MessageID>urn:uuid:{}</wsa:MessageID>", message_id());
    if let Some(r) = relates {
        m.push_str(&format!("<wsa:RelatesTo>{r}</wsa:RelatesTo>"));
    }
    if let Some(n) = sequence {
        m.push_str(&format!("<wsd:AppSequence InstanceId=\"{}\" SequenceId=\"urn:uuid:{}\" MessageNumber=\"{n}\"/>", (crate::time::unix().max(0) as u64), message_id()));
    }
    m.push_str(&format!("</soap:Header><soap:Body>{body}</soap:Body></soap:Envelope>"));
    m
}

/// The text between `<...name>` and `</...name>` (each system has its own namespace prefix).
fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = xml.find(&format!(":{name}>")).or_else(|| xml.find(&format!("<{name}>")))?;
    let start = open + xml[open..].find('>')? + 1;
    let end = start + xml[start..].find("</")?;
    Some(&xml[start..end])
}

fn xaddr(id: &Identity, me: Ip) -> String {
    format!("http://{}:{HTTP_PORT}/{}", ip_str(me), endpoint(id))
}

fn matched(id: &Identity, me: Ip, item: &str) -> String {
    format!(
        "<wsd:{item}><wsa:EndpointReference><wsa:Address>urn:uuid:{}</wsa:Address></wsa:EndpointReference>\
         <wsd:Types>{TYPES}</wsd:Types><wsd:XAddrs>{}</wsd:XAddrs><wsd:MetadataVersion>1</wsd:MetadataVersion></wsd:{item}>",
        endpoint(id),
        xaddr(id, me)
    )
}

/// The "I appeared" announcement (at boot and when the name changes).
pub fn hello(sock: &UdpSocket, id: &Identity, me: Ip) {
    let body = format!(
        "<wsd:Hello><wsa:EndpointReference><wsa:Address>urn:uuid:{}</wsa:Address></wsa:EndpointReference>\
         <wsd:Types>{TYPES}</wsd:Types><wsd:XAddrs>{}</wsd:XAddrs><wsd:MetadataVersion>1</wsd:MetadataVersion></wsd:Hello>",
        endpoint(id),
        xaddr(id, me)
    );
    sock.send_to(GROUP, PORT, envelope(&format!("{DISCOVERY}/Hello"), WELL_KNOWN, None, &body, Some(1)).as_bytes());
}

/// A Probe request: who is on the network.
pub fn probe(sock: &UdpSocket) {
    let body = "<wsd:Probe><wsd:Types>wsdp:Device</wsd:Types></wsd:Probe>";
    sock.send_to(GROUP, PORT, envelope(&format!("{DISCOVERY}/Probe"), WELL_KNOWN, None, body, None).as_bytes());
}

pub fn on_packet(sock: &UdpSocket, src: Ip, sport: u16, p: &[u8], id: &Identity, me: Ip) {
    let Ok(xml) = core::str::from_utf8(p) else { return };
    let Some(action) = tag(xml, "Action").map(str::trim) else { return };
    let message = tag(xml, "MessageID").unwrap_or("").to_string();
    let verb = action.rsplit('/').next().unwrap_or("");
    match verb {
        "Probe" if id.visible => {
            // An empty type list means "all"; otherwise we answer for device and computer.
            let types = tag(xml, "Types").unwrap_or("");
            if types.is_empty() || types.contains("Device") || types.contains("Computer") {
                let body = format!("<wsd:ProbeMatches>{}</wsd:ProbeMatches>", matched(id, me, "ProbeMatch"));
                let reply = envelope(&format!("{DISCOVERY}/ProbeMatches"), ANONYMOUS, Some(&message), &body, Some(1));
                sock.send_to(src, sport, reply.as_bytes());
            }
        }
        "Resolve" if id.visible && xml.contains(&endpoint(id)) => {
            let body = format!("<wsd:ResolveMatches>{}</wsd:ResolveMatches>", matched(id, me, "ResolveMatch"));
            let reply = envelope(&format!("{DISCOVERY}/ResolveMatches"), ANONYMOUS, Some(&message), &body, Some(1));
            sock.send_to(src, sport, reply.as_bytes());
        }
        "ProbeMatches" | "ResolveMatches" | "Hello" if src != me => {
            let Some(url) = tag(xml, "XAddrs").and_then(|x| x.split_whitespace().next()) else { return };
            if !xml.contains("pub:Computer") && !xml.contains("Computer") {
                return; // a printer or a camera
            }
            let url = String::from(url);
            let address = tag(xml, "Address").unwrap_or("").trim().to_string();
            task::spawn("wsd-meta", None, move || fetch_metadata(src, &url, &address));
        }
        _ => {}
    }
}

/// Asks a neighbour for its metadata (WS-Transfer Get): its name and workgroup are there.
fn fetch_metadata(ip: Ip, url: &str, address: &str) {
    let Some(rest) = url.strip_prefix("http://") else { return };
    let (host_port, path) = rest.split_once('/').unwrap_or((rest, ""));
    let port = host_port.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).unwrap_or(80);
    let Ok(stream) = TcpStream::connect(ip, port) else { return };
    let body = envelope("http://schemas.xmlsoap.org/ws/2004/09/transfer/Get", address, None, "", None);
    let request = format!(
        "POST /{path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if stream.send(request.as_bytes()).is_err() {
        return;
    }
    let mut reply = Vec::new();
    while reply.len() < 64 * 1024 {
        match stream.recv(3 * HZ) {
            Ok(d) if !d.is_empty() => reply.extend_from_slice(&d),
            _ => break,
        }
    }
    let text = String::from_utf8_lossy(&reply);
    let Some(computer) = tag(&text, "Computer") else { return };
    // "NAME/Workgroup:GROUP" or "NAME/Domain:DOMAIN".
    let (name, rest) = computer.split_once('/').unwrap_or((computer, ""));
    let workgroup = rest.split_once(':').map_or("", |(_, w)| w);
    discovery::found(Found { ip, name: name.trim().into(), workgroup: workgroup.trim().into(), smb: true, via: "WS-Discovery" });
}

/// Metadata about ourselves for a neighbour.
fn metadata(id: &Identity, message: &str) -> String {
    let e = endpoint(id);
    let body = format!(
        "<wsx:Metadata>\
         <wsx:MetadataSection Dialect=\"http://schemas.xmlsoap.org/ws/2006/02/devprof/ThisDevice\"><wsdp:ThisDevice>\
         <wsdp:FriendlyName>{name}</wsdp:FriendlyName><wsdp:FirmwareVersion>1.0</wsdp:FirmwareVersion><wsdp:SerialNumber>1</wsdp:SerialNumber>\
         </wsdp:ThisDevice></wsx:MetadataSection>\
         <wsx:MetadataSection Dialect=\"http://schemas.xmlsoap.org/ws/2006/02/devprof/ThisModel\"><wsdp:ThisModel>\
         <wsdp:Manufacturer>GlassOS</wsdp:Manufacturer><wsdp:ModelName>GlassOS</wsdp:ModelName><pnpx:DeviceCategory>Computers</pnpx:DeviceCategory>\
         </wsdp:ThisModel></wsx:MetadataSection>\
         <wsx:MetadataSection Dialect=\"http://schemas.xmlsoap.org/ws/2006/02/devprof/Relationship\">\
         <wsdp:Relationship Type=\"http://schemas.xmlsoap.org/ws/2006/02/devprof/host\"><wsdp:Host>\
         <wsa:EndpointReference><wsa:Address>urn:uuid:{e}</wsa:Address></wsa:EndpointReference>\
         <wsdp:Types>pub:Computer</wsdp:Types><wsdp:ServiceId>urn:uuid:{e}</wsdp:ServiceId>\
         <pub:Computer>{name}/Workgroup:{group}</pub:Computer>\
         </wsdp:Host></wsdp:Relationship></wsx:MetadataSection></wsx:Metadata>",
        name = id.name,
        group = id.workgroup
    );
    envelope("http://schemas.xmlsoap.org/ws/2004/09/transfer/GetResponse", ANONYMOUS, Some(message), &body, None)
}

fn serve_http(stream: TcpStream, id: Identity) {
    let mut req = Vec::new();
    let until = interrupts::ticks() + 5 * HZ;
    loop {
        let text = String::from_utf8_lossy(&req);
        if let Some(head) = text.find("\r\n\r\n") {
            let want = text[..head]
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if req.len() >= head + 4 + want {
                break;
            }
        }
        if interrupts::ticks() >= until || req.len() > 64 * 1024 {
            return;
        }
        match stream.recv(HZ) {
            Ok(d) if !d.is_empty() => req.extend_from_slice(&d),
            _ => return,
        }
    }
    let text = String::from_utf8_lossy(&req);
    let message = tag(&text, "MessageID").unwrap_or("").to_string();
    let body = metadata(&id, &message);
    let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let _ = stream.send(reply.as_bytes());
}

/// The device description server.
pub fn start_http() {
    task::spawn("wsd-http", None, || {
        let listener = loop {
            match TcpListener::bind(HTTP_PORT) {
                Ok(l) => break l,
                Err(_) => task::sleep_until(interrupts::ticks() + HZ),
            }
        };
        loop {
            if let Some(stream) = listener.accept(HZ) {
                let id = super::identity::get();
                if id.visible {
                    task::spawn("wsd-conn", None, move || serve_http(stream, id));
                }
            }
        }
    });
}
