//! Netlink, spoken directly.
//!
//! `sock_diag` answers a question nothing else can without privileges: what
//! every TCP connection on this machine is doing right now, including the
//! kernel's own estimate of its round-trip time. `ss` reads the same interface;
//! shelling out to it would mean parsing a human-facing table and inheriting
//! its clock.
//!
//! Only what this family needs is implemented: a dump request and a walk over
//! the replies. If a second family needs netlink, this moves up out of
//! `collect/net/`.

use std::io;
use std::os::fd::RawFd;

const NETLINK_SOCK_DIAG: i32 = 4;
const SOCK_DIAG_BY_FAMILY: u16 = 20;

const NLM_F_REQUEST: u16 = 0x001;
const NLM_F_DUMP: u16 = 0x300;

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;

/// Header sizes, fixed by the kernel's ABI.
const NLMSG_HDR_LEN: usize = 16;
const REQ_LEN: usize = 56;
/// `inet_diag_msg`: four bytes, then a 48-byte socket id, then five u32.
const DIAG_MSG_LEN: usize = 72;

/// The extension carrying `struct tcp_info`.
const INET_DIAG_INFO: u16 = 2;

/// One socket as `sock_diag` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Socket {
    pub family: u8,
    pub state: u8,
    pub source: Endpoint,
    pub destination: Endpoint,
    /// The kernel's socket cookie: the identity that survives softirq
    /// processing, and the typed key the rest of the tool joins sockets on.
    pub cookie: u64,
    pub inode: u32,
    pub uid: u32,
    /// Bytes still queued for the application to read, and to send.
    pub receive_queue: u32,
    pub send_queue: u32,
    /// From `tcp_info`, when the kernel supplied it.
    pub info: Option<TcpInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    pub address: [u8; 16],
    pub port: u16,
}

impl Endpoint {
    /// The address alone, as a person writes it -- for an IPv4 socket the four
    /// bytes the kernel put at the front of a sixteen-byte field -- and never
    /// the port, which travels as its own field.
    ///
    /// It carried the port once (`127.0.0.1:40123`), and an equality against
    /// an address could then never match: net 6 in `docs/scenarios/net.md`.
    /// The rendering is the standard library's, so `net.tcp.rtt`'s `daddr` is
    /// the same string `net.tcp.lifetime`'s is for the same peer.
    pub fn address(&self, family: u8) -> String {
        if family == libc::AF_INET as u8 {
            let octets: [u8; 4] = self.address[..4].try_into().expect("four bytes");
            std::net::Ipv4Addr::from(octets).to_string()
        } else {
            std::net::Ipv6Addr::from(self.address).to_string()
        }
    }
}

/// The fields of `struct tcp_info` this tool reads.
///
/// The struct grows with every kernel release, and reading it by offset is what
/// the ABI guarantees: fields are only ever appended. A short one is a kernel
/// older than the field wanted, which is why the length is checked rather than
/// assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpInfo {
    /// Smoothed round-trip time, in microseconds as the kernel reports it.
    pub rtt_us: u32,
    pub rtt_var_us: u32,
    pub send_congestion_window: u32,
    pub send_slow_start_threshold: u32,
    pub retransmits: u8,
    /// Congestion control state: 0 open, 1 disorder, 2 cwr, 3 recovery, 4 loss.
    pub congestion_state: u8,
}

impl TcpInfo {
    /// Offsets within `struct tcp_info`: eight single-byte fields, then u32s.
    const RTT: usize = 8 + 15 * 4;
    const RTT_VAR: usize = 8 + 16 * 4;
    const SSTHRESH: usize = 8 + 17 * 4;
    const CWND: usize = 8 + 18 * 4;

    fn parse(bytes: &[u8]) -> Option<Self> {
        let u32_at = |offset: usize| -> Option<u32> {
            bytes
                .get(offset..offset + 4)
                .map(|slice| u32::from_ne_bytes(slice.try_into().expect("4 bytes")))
        };
        Some(Self {
            congestion_state: *bytes.get(1)?,
            retransmits: *bytes.get(2)?,
            rtt_us: u32_at(Self::RTT)?,
            rtt_var_us: u32_at(Self::RTT_VAR)?,
            send_slow_start_threshold: u32_at(Self::SSTHRESH)?,
            send_congestion_window: u32_at(Self::CWND)?,
        })
    }
}

/// A netlink socket bound to `sock_diag`.
pub struct Diag {
    fd: RawFd,
}

impl Diag {
    pub fn open() -> io::Result<Self> {
        // SAFETY: creating a socket; the arguments are constants.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                NETLINK_SOCK_DIAG,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    /// Dumps every TCP socket of one address family.
    pub fn dump_tcp(&self, family: u8) -> io::Result<Vec<Socket>> {
        self.request(family)?;
        self.collect()
    }

    fn request(&self, family: u8) -> io::Result<()> {
        let mut message = [0u8; NLMSG_HDR_LEN + REQ_LEN];
        let total = message.len() as u32;

        message[0..4].copy_from_slice(&total.to_ne_bytes());
        message[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
        message[6..8].copy_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
        message[8..12].copy_from_slice(&1u32.to_ne_bytes()); // sequence
        message[12..16].copy_from_slice(&0u32.to_ne_bytes()); // port, kernel assigns

        // struct inet_diag_req_v2
        message[16] = family;
        message[17] = libc::IPPROTO_TCP as u8;
        // Ask for `tcp_info`, which is where the round-trip time lives. The
        // extension bitmask is one-based.
        message[18] = 1 << (INET_DIAG_INFO - 1);
        message[19] = 0;
        // Every state: a connection closing is still a connection that existed.
        message[20..24].copy_from_slice(&u32::MAX.to_ne_bytes());

        // SAFETY: sending a buffer we own on a socket we own.
        let sent = unsafe {
            libc::send(
                self.fd,
                message.as_ptr().cast::<libc::c_void>(),
                message.len(),
                0,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn collect(&self) -> io::Result<Vec<Socket>> {
        let mut sockets = Vec::new();
        let mut buffer = vec![0u8; 64 * 1024];

        loop {
            // SAFETY: receiving into a buffer we own.
            let read = unsafe {
                libc::recv(
                    self.fd,
                    buffer.as_mut_ptr().cast::<libc::c_void>(),
                    buffer.len(),
                    0,
                )
            };
            if read < 0 {
                return Err(io::Error::last_os_error());
            }
            if read == 0 {
                break;
            }

            let mut offset = 0usize;
            let end = read as usize;
            while offset + NLMSG_HDR_LEN <= end {
                let length =
                    u32::from_ne_bytes(buffer[offset..offset + 4].try_into().expect("4 bytes"))
                        as usize;
                let kind =
                    u16::from_ne_bytes(buffer[offset + 4..offset + 6].try_into().expect("2 bytes"));
                if length < NLMSG_HDR_LEN || offset + length > end {
                    break;
                }

                match kind {
                    NLMSG_DONE => return Ok(sockets),
                    NLMSG_ERROR => {
                        let code = i32::from_ne_bytes(
                            buffer[offset + NLMSG_HDR_LEN..offset + NLMSG_HDR_LEN + 4]
                                .try_into()
                                .expect("4 bytes"),
                        );
                        return Err(io::Error::from_raw_os_error(-code));
                    }
                    _ => {
                        if let Some(socket) =
                            parse_socket(&buffer[offset + NLMSG_HDR_LEN..offset + length])
                        {
                            sockets.push(socket);
                        }
                    }
                }
                // Netlink messages are four-byte aligned.
                offset += (length + 3) & !3;
            }
        }
        Ok(sockets)
    }
}

impl Drop for Diag {
    fn drop(&mut self) {
        // SAFETY: closing a descriptor this struct owns, once.
        unsafe { libc::close(self.fd) };
    }
}

fn parse_socket(body: &[u8]) -> Option<Socket> {
    if body.len() < DIAG_MSG_LEN {
        return None;
    }
    let be16 = |at: usize| u16::from_be_bytes(body[at..at + 2].try_into().expect("2 bytes"));
    let ne32 = |at: usize| u32::from_ne_bytes(body[at..at + 4].try_into().expect("4 bytes"));

    let mut source_address = [0u8; 16];
    source_address.copy_from_slice(&body[8..24]);
    let mut destination_address = [0u8; 16];
    destination_address.copy_from_slice(&body[24..40]);

    // `inet_diag_msg` is four bytes, then `inet_diag_sockid`, then five u32:
    //   0 family, state, timer, retrans
    //   4 sport, 6 dport, 8 src[4], 24 dst[4], 40 if, 44 cookie[2]
    //  52 expires, 56 rqueue, 60 wqueue, 64 uid, 68 inode
    // The cookie is two u32 halves, and it is the socket's identity.
    let cookie = (ne32(44) as u64) | ((ne32(48) as u64) << 32);

    let mut socket = Socket {
        family: body[0],
        state: body[1],
        source: Endpoint {
            address: source_address,
            port: be16(4),
        },
        destination: Endpoint {
            address: destination_address,
            port: be16(6),
        },
        cookie,
        inode: ne32(68),
        uid: ne32(64),
        receive_queue: ne32(56),
        send_queue: ne32(60),
        info: None,
    };

    // Attributes follow the fixed part.
    let mut offset = DIAG_MSG_LEN;
    while offset + 4 <= body.len() {
        let length =
            u16::from_ne_bytes(body[offset..offset + 2].try_into().expect("2 bytes")) as usize;
        let kind = u16::from_ne_bytes(body[offset + 2..offset + 4].try_into().expect("2 bytes"));
        if length < 4 || offset + length > body.len() {
            break;
        }
        if kind == INET_DIAG_INFO {
            socket.info = TcpInfo::parse(&body[offset + 4..offset + length]);
        }
        offset += (length + 3) & !3;
    }
    Some(socket)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn a_dump_finds_a_connection_this_test_just_opened() {
        let Ok(diag) = Diag::open() else {
            eprintln!("skipped: this machine refuses NETLINK_SOCK_DIAG");
            return;
        };

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let mut client = TcpStream::connect(address).expect("connect");
        let (mut server, _) = listener.accept().expect("accept");
        client.write_all(b"hello").expect("write");
        let _ = server.write_all(b"hi");

        let sockets = diag.dump_tcp(libc::AF_INET as u8).expect("dump");
        let port = address.port();
        let found = sockets
            .iter()
            .find(|socket| socket.source.port == port || socket.destination.port == port)
            .unwrap_or_else(|| panic!("the connection on port {port} was not in the dump"));

        assert_ne!(found.cookie, 0, "every socket has a cookie");
        let info = found
            .info
            .expect("tcp_info was requested and should be present");
        // A loopback round trip is measured in microseconds, never in seconds.
        assert!(
            info.rtt_us < 100_000,
            "loopback round-trip time reported as {} us",
            info.rtt_us
        );
        assert!(
            info.send_congestion_window > 0,
            "an established connection has a congestion window"
        );
    }

    #[test]
    fn an_ipv4_endpoint_renders_as_dotted_quad() {
        let endpoint = Endpoint {
            address: [127, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            port: 8080,
        };
        assert_eq!(endpoint.address(libc::AF_INET as u8), "127.0.0.1");
    }
}
