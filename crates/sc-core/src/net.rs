//! Open network endpoints and the process that owns each: TCP connections
//! and listeners, and UDP sockets. Addresses only; nothing is resolved to a
//! name, which would mean sending lookups for every address seen.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, GetExtendedUdpTable, TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
};

const AF_INET: u32 = 2;
const AF_INET6: u32 = 23;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    pub pid: u32,
    pub protocol: &'static str,
    pub local: SocketAddr,
    /// `None` for UDP, which has no connection to speak of.
    pub remote: Option<SocketAddr>,
    pub state: &'static str,
}

fn tcp_state(code: u32) -> &'static str {
    match code {
        1 => "Closed",
        2 => "Listening",
        3 => "Connecting",
        4 => "Accepting",
        5 => "Connected",
        6..=11 => "Closing",
        _ => "",
    }
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Ports sit in the low 16 bits in network byte order.
fn port_at(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn v4(b: &[u8], addr: usize, port: usize) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(b[addr], b[addr + 1], b[addr + 2], b[addr + 3])), port_at(b, port))
}

fn v6(b: &[u8], addr: usize, port: usize) -> SocketAddr {
    let mut octets = [0u8; 16];
    octets.copy_from_slice(&b[addr..addr + 16]);
    SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port_at(b, port))
}

/// Rows of a `MIB_*TABLE_OWNER_PID`: a count, then that many fixed-size rows.
/// A count that overruns the buffer is cut to what is really there.
fn rows(table: &[u8], size: usize) -> impl Iterator<Item = &[u8]> {
    let count = if table.len() >= 4 { u32_at(table, 0) as usize } else { 0 };
    table.get(4..).unwrap_or_default().chunks_exact(size).take(count)
}

pub fn parse_tcp4(table: &[u8]) -> Vec<Connection> {
    // state, local address, local port, remote address, remote port, pid
    rows(table, 24)
        .map(|r| {
            let state = tcp_state(u32_at(r, 0));
            let remote = (state != "Listening").then(|| v4(r, 12, 16));
            Connection { pid: u32_at(r, 20), protocol: "TCP", local: v4(r, 4, 8), remote, state }
        })
        .collect()
}

pub fn parse_tcp6(table: &[u8]) -> Vec<Connection> {
    // local address, scope, local port, remote address, scope, remote port, state, pid
    rows(table, 56)
        .map(|r| {
            let state = tcp_state(u32_at(r, 48));
            let remote = (state != "Listening").then(|| v6(r, 24, 44));
            Connection { pid: u32_at(r, 52), protocol: "TCP", local: v6(r, 0, 20), remote, state }
        })
        .collect()
}

pub fn parse_udp4(table: &[u8]) -> Vec<Connection> {
    // local address, local port, pid
    rows(table, 12)
        .map(|r| Connection { pid: u32_at(r, 8), protocol: "UDP", local: v4(r, 0, 4), remote: None, state: "" })
        .collect()
}

pub fn parse_udp6(table: &[u8]) -> Vec<Connection> {
    // local address, scope, local port, pid
    rows(table, 28)
        .map(|r| Connection { pid: u32_at(r, 24), protocol: "UDP", local: v6(r, 0, 20), remote: None, state: "" })
        .collect()
}

/// Calls one of the table functions until the buffer is big enough.
fn table(fetch: impl Fn(Option<*mut std::ffi::c_void>, &mut u32) -> u32) -> Vec<u8> {
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    let mut size = 0u32;
    fetch(None, &mut size);
    for _ in 0..4 {
        // u32 storage keeps the rows aligned for the system.
        let mut buf = vec![0u32; (size as usize).div_ceil(4) + 256];
        size = buf.len() as u32 * 4;
        match fetch(Some(buf.as_mut_ptr().cast()), &mut size) {
            0 => return buf.iter().flat_map(|w| w.to_ne_bytes()).collect(),
            ERROR_INSUFFICIENT_BUFFER => continue,
            _ => break,
        }
    }
    Vec::new()
}

/// Every TCP and UDP endpoint on the machine, IPv4 and IPv6.
pub fn connections() -> Vec<Connection> {
    let tcp = |af| table(|buf, size| unsafe { GetExtendedTcpTable(buf, size, false, af, TCP_TABLE_OWNER_PID_ALL, 0) });
    let udp = |af| table(|buf, size| unsafe { GetExtendedUdpTable(buf, size, false, af, UDP_TABLE_OWNER_PID, 0) });
    let mut out = parse_tcp4(&tcp(AF_INET));
    out.extend(parse_tcp6(&tcp(AF_INET6)));
    out.extend(parse_udp4(&udp(AF_INET)));
    out.extend(parse_udp6(&udp(AF_INET6)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn words(w: &[u32]) -> Vec<u8> {
        w.iter().flat_map(|w| w.to_ne_bytes()).collect()
    }

    #[test]
    fn tcp4_rows_decode_addresses_ports_and_state() {
        let addr = |a: [u8; 4]| u32::from_ne_bytes(a);
        let port = |p: u16| u32::from_ne_bytes([(p >> 8) as u8, p as u8, 0, 0]);
        let table = words(&[
            2,
            5, addr([192, 168, 1, 20]), port(50123), addr([93, 184, 216, 34]), port(443), 4321,
            2, addr([0, 0, 0, 0]), port(8080), 0, 0, 77,
        ]);
        let got = parse_tcp4(&table);
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].pid, got[0].state), (4321, "Connected"));
        assert_eq!(got[0].local.to_string(), "192.168.1.20:50123");
        assert_eq!(got[0].remote.unwrap().to_string(), "93.184.216.34:443");
        assert_eq!((got[1].pid, got[1].state, got[1].remote), (77, "Listening", None));
        assert_eq!(got[1].local.to_string(), "0.0.0.0:8080");
    }

    #[test]
    fn a_count_larger_than_the_buffer_is_not_trusted() {
        let mut table = words(&[1000, 5, 0, 0, 0, 0, 9]);
        assert_eq!(parse_tcp4(&table).len(), 1);
        table.truncate(10);
        assert!(parse_tcp4(&table).is_empty());
        assert!(parse_udp6(&[]).is_empty() && parse_tcp6(&[1, 0]).is_empty());
    }

    #[test]
    fn udp6_row_decodes() {
        let mut table = words(&[1]);
        table.extend([0u8; 15]);
        table.push(1); // ::1
        table.extend(words(&[0]));
        table.extend([0x14, 0xE9, 0, 0]); // port 5353
        table.extend(words(&[640]));
        let got = parse_udp6(&table);
        assert_eq!((got[0].pid, got[0].protocol, got[0].local.to_string().as_str()), (640, "UDP", "[::1]:5353"));
    }

    #[test]
    fn live_table_lists_a_listener_we_open() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let local = listener.local_addr().unwrap();
        let all = connections();
        let mine = all.iter().find(|c| c.local == local).expect("our listener is listed");
        assert_eq!((mine.pid, mine.protocol, mine.state, mine.remote), (std::process::id(), "TCP", "Listening", None));
    }
}
