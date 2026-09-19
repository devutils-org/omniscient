//! The `ethtool` generic-netlink family, spoken directly.
//!
//! `src/capabilities.rs` says why this could not be probed until now: `ethtool`
//! is not a netlink protocol with a number, it is a *family* inside
//! `NETLINK_GENERIC` whose id the kernel assigns at boot and hands out by name
//! through `CTRL_CMD_GETFAMILY`. Resolving that name is the first thing this
//! module does, and it is also the honest answer to "does this kernel have
//! ethtool netlink": if the lookup fails, it does not.
//!
//! Only `ETHTOOL_MSG_TSINFO_GET` is implemented, because only `net.hwts` needs
//! it. The ioctl `SIOCETHTOOL` would answer the same question with fewer lines
//! and a fixed struct; the netlink form is used instead because it names its
//! own bits. A capability list that comes back as `hardware-receive` rather
//! than as bit 2 of a mask stays right when the kernel grows a bit this tool
//! has never heard of.

use std::io;
use std::os::fd::RawFd;

const NETLINK_GENERIC: i32 = 16;
/// The controller family, whose id is fixed because it is the one that hands
/// out the others.
const GENL_ID_CTRL: u16 = 16;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;

const ETHTOOL_FAMILY: &[u8] = b"ethtool\0";
const ETHTOOL_MSG_TSINFO_GET: u8 = 25;
const ETHTOOL_A_TSINFO_HEADER: u16 = 1;
const ETHTOOL_A_TSINFO_TIMESTAMPING: u16 = 2;
const ETHTOOL_A_TSINFO_TX_TYPES: u16 = 3;
const ETHTOOL_A_TSINFO_RX_FILTERS: u16 = 4;
const ETHTOOL_A_TSINFO_PHC_INDEX: u16 = 5;
const ETHTOOL_A_HEADER_DEV_NAME: u16 = 2;

/// A bitset the kernel sent in its verbose form: a list of the bits that are
/// set, each with the name the kernel knows it by.
const ETHTOOL_A_BITSET_BITS: u16 = 3;
const ETHTOOL_A_BITSET_BITS_BIT_NAME: u16 = 2;

const NLM_F_REQUEST: u16 = 0x001;
const NLM_F_ACK: u16 = 0x004;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLMSG_HDR_LEN: usize = 16;
const GENL_HDR_LEN: usize = 4;
const NLA_F_NESTED: u16 = 0x8000;

/// What an interface can do about timestamping, as the kernel describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimestampInfo {
    /// The `SOF_TIMESTAMPING_*` capabilities that are set, by the kernel's own
    /// names: `hardware-receive`, `software-system-clock`, and so on.
    pub capabilities: Vec<String>,
    /// The transmit timestamping modes the interface accepts.
    pub tx_types: Vec<String>,
    /// Which received packets it will stamp.
    pub rx_filters: Vec<String>,
    /// The PTP hardware clock this interface is wired to, when it has one.
    /// `None` is the ordinary answer on a machine whose NIC keeps no clock.
    pub phc_index: Option<u32>,
}

impl TimestampInfo {
    /// Whether the interface can stamp in hardware at all.
    ///
    /// This is the question `net.hwts` exists to answer: without it, a latency
    /// measured anywhere in this tool is the kernel's account of when it
    /// noticed a packet, not the NIC's account of when the packet arrived.
    pub fn stamps_in_hardware(&self) -> bool {
        self.capabilities
            .iter()
            .any(|name| name.starts_with("hardware-"))
    }

    pub fn has(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|name| name == capability)
    }
}

/// A generic-netlink socket with the `ethtool` family resolved.
pub struct Ethtool {
    fd: RawFd,
    family: u16,
    sequence: u32,
}

impl Ethtool {
    pub fn open() -> io::Result<Self> {
        // SAFETY: creating a socket; the arguments are constants.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                NETLINK_GENERIC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut ethtool = Self {
            fd,
            family: GENL_ID_CTRL,
            sequence: 0,
        };
        ethtool.family = ethtool.resolve_family()?;
        Ok(ethtool)
    }

    /// Asks the controller which id this kernel gave the `ethtool` family.
    fn resolve_family(&mut self) -> io::Result<u16> {
        let payload = attribute(CTRL_ATTR_FAMILY_NAME, ETHTOOL_FAMILY);
        let replies = self.round_trip(GENL_ID_CTRL, CTRL_CMD_GETFAMILY, &payload)?;
        for reply in &replies {
            for (kind, value) in attributes(reply) {
                if kind == CTRL_ATTR_FAMILY_ID && value.len() >= 2 {
                    return Ok(u16::from_ne_bytes([value[0], value[1]]));
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "this kernel has no `ethtool` generic-netlink family",
        ))
    }

    /// What one interface can do about timestamping.
    pub fn timestamping(&mut self, interface: &str) -> io::Result<TimestampInfo> {
        let mut name = interface.as_bytes().to_vec();
        name.push(0);
        let header = attribute(ETHTOOL_A_HEADER_DEV_NAME, &name);
        let payload = attribute(ETHTOOL_A_TSINFO_HEADER | NLA_F_NESTED, &header);
        let replies = self.round_trip(self.family, ETHTOOL_MSG_TSINFO_GET, &payload)?;

        let mut info = TimestampInfo::default();
        for reply in &replies {
            for (kind, value) in attributes(reply) {
                match kind {
                    ETHTOOL_A_TSINFO_TIMESTAMPING => info.capabilities = set_bits(value),
                    ETHTOOL_A_TSINFO_TX_TYPES => info.tx_types = set_bits(value),
                    ETHTOOL_A_TSINFO_RX_FILTERS => info.rx_filters = set_bits(value),
                    ETHTOOL_A_TSINFO_PHC_INDEX if value.len() >= 4 => {
                        let index =
                            i32::from_ne_bytes(value[0..4].try_into().expect("4 bytes checked"));
                        // The kernel writes -1 for an interface with no clock,
                        // and omits the attribute entirely on some drivers.
                        // Both mean the same thing and neither is clock zero.
                        info.phc_index = u32::try_from(index).ok();
                    }
                    _ => {}
                }
            }
        }
        Ok(info)
    }

    /// Sends one request and collects every reply that belongs to it.
    ///
    /// The sequence number is checked rather than assumed. A netlink socket
    /// delivers whatever is queued on it, so reading one datagram and calling
    /// it the answer gives the previous request's reply as often as not --
    /// which is how a first draft of this module reported one interface's
    /// hardware clock under another interface's name.
    fn round_trip(&mut self, family: u16, command: u8, payload: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        self.sequence += 1;
        let sequence = self.sequence;

        let mut message = Vec::with_capacity(NLMSG_HDR_LEN + GENL_HDR_LEN + payload.len());
        let total = (NLMSG_HDR_LEN + GENL_HDR_LEN + payload.len()) as u32;
        message.extend_from_slice(&total.to_ne_bytes());
        message.extend_from_slice(&family.to_ne_bytes());
        message.extend_from_slice(&(NLM_F_REQUEST | NLM_F_ACK).to_ne_bytes());
        message.extend_from_slice(&sequence.to_ne_bytes());
        message.extend_from_slice(&0u32.to_ne_bytes()); // port, the kernel fills it
        message.push(command);
        message.push(1); // the family's interface version
        message.extend_from_slice(&0u16.to_ne_bytes());
        message.extend_from_slice(payload);

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

        let mut replies = Vec::new();
        let mut buffer = vec![0u8; 32 * 1024];
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
                return Ok(replies);
            }

            let end = read as usize;
            let mut offset = 0usize;
            while offset + NLMSG_HDR_LEN <= end {
                let length =
                    u32::from_ne_bytes(buffer[offset..offset + 4].try_into().expect("4 bytes"))
                        as usize;
                let kind =
                    u16::from_ne_bytes(buffer[offset + 4..offset + 6].try_into().expect("2 bytes"));
                let seq = u32::from_ne_bytes(
                    buffer[offset + 8..offset + 12].try_into().expect("4 bytes"),
                );
                if length < NLMSG_HDR_LEN || offset + length > end {
                    break;
                }
                let body = &buffer[offset + NLMSG_HDR_LEN..offset + length];
                offset += (length + 3) & !3;

                // Somebody else's reply, still queued on this socket.
                if seq != sequence {
                    continue;
                }
                match kind {
                    NLMSG_ERROR => {
                        let code = i32::from_ne_bytes(body[0..4].try_into().expect("4 bytes"));
                        if code == 0 {
                            // An acknowledgement, which ends the exchange.
                            return Ok(replies);
                        }
                        return Err(io::Error::from_raw_os_error(-code));
                    }
                    NLMSG_DONE => return Ok(replies),
                    _ => replies.push(body[GENL_HDR_LEN..].to_vec()),
                }
            }
        }
    }
}

impl Drop for Ethtool {
    fn drop(&mut self) {
        // SAFETY: closing a descriptor this struct owns, once.
        unsafe { libc::close(self.fd) };
    }
}

/// One netlink attribute, padded to the four-byte alignment the protocol uses.
fn attribute(kind: u16, value: &[u8]) -> Vec<u8> {
    let length = 4 + value.len();
    let mut out = Vec::with_capacity((length + 3) & !3);
    out.extend_from_slice(&(length as u16).to_ne_bytes());
    out.extend_from_slice(&kind.to_ne_bytes());
    out.extend_from_slice(value);
    out.resize((length + 3) & !3, 0);
    out
}

/// Walks a run of netlink attributes.
fn attributes(body: &[u8]) -> Vec<(u16, &[u8])> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset + 4 <= body.len() {
        let length =
            u16::from_ne_bytes(body[offset..offset + 2].try_into().expect("2 bytes")) as usize;
        let kind = u16::from_ne_bytes(body[offset + 2..offset + 4].try_into().expect("2 bytes"));
        if length < 4 || offset + length > body.len() {
            break;
        }
        // The nesting bit is part of the type field, not of the type.
        out.push((kind & !NLA_F_NESTED, &body[offset + 4..offset + length]));
        offset += (length + 3) & !3;
    }
    out
}

/// The names of the bits that are set in an `ethtool` bitset.
///
/// The kernel sends these in one of two shapes. The verbose one lists each set
/// bit with its name, which is what is read here; the compact one is a raw mask
/// the caller would have to have its own table of names for, and asking for it
/// would trade the whole advantage of the netlink interface for a few bytes.
fn set_bits(bitset: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    for (kind, value) in attributes(bitset) {
        if kind != ETHTOOL_A_BITSET_BITS {
            continue;
        }
        for (_, bit) in attributes(value) {
            for (field, content) in attributes(bit) {
                if field == ETHTOOL_A_BITSET_BITS_BIT_NAME {
                    let name = content
                        .split(|byte| *byte == 0)
                        .next()
                        .unwrap_or_default()
                        .to_vec();
                    if let Ok(name) = String::from_utf8(name) {
                        names.push(name);
                    }
                }
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_attribute_is_padded_to_four_bytes_and_says_its_true_length() {
        // A five-byte value occupies twelve bytes and declares nine, which is
        // the rule every netlink parser on the other side relies on.
        let encoded = attribute(7, b"hello");
        assert_eq!(encoded.len(), 12);
        assert_eq!(u16::from_ne_bytes([encoded[0], encoded[1]]), 9);
        assert_eq!(u16::from_ne_bytes([encoded[2], encoded[3]]), 7);
        assert_eq!(&encoded[4..9], b"hello");

        let walked = attributes(&encoded);
        assert_eq!(walked.len(), 1);
        assert_eq!(walked[0].0, 7);
        assert_eq!(walked[0].1, b"hello");
    }

    #[test]
    fn the_nesting_bit_is_not_part_of_the_attribute_type() {
        let nested = attribute(ETHTOOL_A_TSINFO_HEADER | NLA_F_NESTED, b"\0\0\0\0");
        assert_eq!(attributes(&nested)[0].0, ETHTOOL_A_TSINFO_HEADER);
    }

    #[test]
    fn loopback_can_be_timestamped_in_software_and_never_in_hardware() {
        // Ground truth that owes nothing to this code: loopback has no network
        // interface card, so it cannot have a card's clock. `/sys/class/net/lo/
        // type` is 772, ARPHRD_LOOPBACK, which is where that fact is written
        // down independently of anything ethtool says.
        let kind = std::fs::read_to_string("/sys/class/net/lo/type")
            .expect("every machine has a loopback");
        assert_eq!(kind.trim(), "772", "ARPHRD_LOOPBACK");

        let Ok(mut ethtool) = Ethtool::open() else {
            panic!(
                "this kernel has no ethtool netlink family, which every kernel 5.6 and later has"
            )
        };
        let info = ethtool.timestamping("lo").expect("loopback answers");
        assert!(
            !info.stamps_in_hardware(),
            "loopback reported hardware timestamping: {:?}",
            info.capabilities
        );
        assert!(
            info.has("software-transmit") && info.has("software-receive"),
            "the kernel stamps loopback in software: {:?}",
            info.capabilities
        );
        assert_eq!(info.phc_index, None, "loopback has no hardware clock");
    }

    #[test]
    fn an_interface_that_does_not_exist_is_an_error_rather_than_an_empty_answer() {
        let Ok(mut ethtool) = Ethtool::open() else {
            panic!("this kernel has no ethtool netlink family")
        };
        let error = ethtool
            .timestamping("nosuchif0")
            .expect_err("there is no such interface");
        assert_eq!(error.raw_os_error(), Some(libc::ENODEV), "{error}");
    }

    #[test]
    fn two_requests_in_a_row_each_get_their_own_answer() {
        // The sequence check earns its place here: without it the second
        // request reads the first one's leftover reply off the socket.
        let Ok(mut ethtool) = Ethtool::open() else {
            panic!("this kernel has no ethtool netlink family")
        };
        let first = ethtool.timestamping("lo").expect("loopback answers");
        let _ = ethtool.timestamping("nosuchif0");
        let second = ethtool.timestamping("lo").expect("loopback answers again");
        assert_eq!(first, second, "the same interface answered differently");
    }
}
