//! As much of a frame as this tool is willing to look at.
//!
//! The list is short on purpose: addresses, ports, protocol, and where the
//! headers ended. That is what the catalog declares as filterable on
//! `net.capture`, and a capture that reads further than the request asked for
//! has crossed a boundary it cannot uncross -- the bytes are in this process's
//! memory whether or not anything prints them.
//!
//! In practice the boundary is enforced a layer down, by the kernel: the
//! classic BPF filter on the socket copies only the headers into the ring, so
//! the payload is never here to be read. This module is what decides how many
//! bytes that is.
//!
//! Extension headers are not walked. An IPv6 packet carrying them reports the
//! extension as its protocol and no ports, which is what it is: this reader
//! does not know where that packet's transport header starts, and inventing an
//! offset would put two arbitrary bytes in a field named `dport`.

/// The addresses and ports of one frame, as far as they could be read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Headers {
    /// 4 or 6, or `None` when the network header was not one this reader knows.
    pub ip_version: Option<u8>,
    /// The IP protocol number: 6 for TCP, 17 for UDP, and so on.
    pub protocol: Option<u8>,
    pub source: Option<String>,
    pub destination: Option<String>,
    pub source_port: Option<u16>,
    pub destination_port: Option<u16>,
}

const TCP: u8 = 6;
const UDP: u8 = 17;

/// Reads the headers of a frame, starting at the network layer.
///
/// `network_offset` is the kernel's own answer to where the link-layer header
/// ended, which is why this works on loopback, on Ethernet and on a tunnel
/// without a case for each.
pub fn read(bytes: &[u8], network_offset: usize) -> Headers {
    let Some(network) = bytes.get(network_offset..) else {
        return Headers::default();
    };
    match network.first().map(|first| first >> 4) {
        Some(4) => read_ipv4(network),
        Some(6) => read_ipv6(network),
        _ => Headers::default(),
    }
}

fn read_ipv4(network: &[u8]) -> Headers {
    let mut headers = Headers {
        ip_version: Some(4),
        ..Headers::default()
    };
    // The header length is in 32-bit words and is never less than five, so a
    // smaller one is a frame this reader will not guess at.
    let Some(header_len) = network
        .first()
        .map(|first| (first & 0x0f) as usize * 4)
        .filter(|length| *length >= 20)
    else {
        return headers;
    };
    if network.len() < 20 {
        return headers;
    }
    headers.protocol = Some(network[9]);
    headers.source = Some(ipv4(&network[12..16]));
    headers.destination = Some(ipv4(&network[16..20]));
    read_ports(&mut headers, network, header_len);
    headers
}

fn read_ipv6(network: &[u8]) -> Headers {
    let mut headers = Headers {
        ip_version: Some(6),
        ..Headers::default()
    };
    if network.len() < 40 {
        return headers;
    }
    headers.protocol = Some(network[6]);
    headers.source = Some(ipv6(&network[8..24]));
    headers.destination = Some(ipv6(&network[24..40]));
    read_ports(&mut headers, network, 40);
    headers
}

/// Ports, for the two protocols that put them in the same place.
///
/// Everything else -- ICMP, SCTP with its own shape, an IPv6 extension header
/// standing where the transport header was expected -- leaves them unset. An
/// absent port is a different answer from port zero.
fn read_ports(headers: &mut Headers, network: &[u8], transport_offset: usize) {
    if !matches!(headers.protocol, Some(TCP) | Some(UDP)) {
        return;
    }
    let Some(transport) = network.get(transport_offset..transport_offset + 4) else {
        // The copy stopped before the ports: the snaplen was reached, or the
        // frame really is that short.
        return;
    };
    headers.source_port = Some(u16::from_be_bytes([transport[0], transport[1]]));
    headers.destination_port = Some(u16::from_be_bytes([transport[2], transport[3]]));
}

fn ipv4(bytes: &[u8]) -> String {
    format!("{}.{}.{}.{}", bytes[0], bytes[1], bytes[2], bytes[3])
}

fn ipv6(bytes: &[u8]) -> String {
    let groups: Vec<String> = bytes
        .chunks(2)
        .map(|pair| format!("{:x}", u16::from_be_bytes([pair[0], pair[1]])))
        .collect();
    groups.join(":")
}

/// The protocol as a person names it, falling back to the number the header
/// carried rather than to a guess.
pub fn protocol_name(protocol: u8) -> String {
    match protocol {
        1 => "icmp".to_owned(),
        TCP => "tcp".to_owned(),
        17 => "udp".to_owned(),
        58 => "icmpv6".to_owned(),
        132 => "sctp".to_owned(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An IPv4 UDP datagram, written out field by field the way RFC 791 and
    /// RFC 768 lay them out.
    fn udp_over_ipv4() -> Vec<u8> {
        let mut frame = vec![0u8; 14]; // a link-layer header of the usual size
        frame.extend_from_slice(&[
            0x45, 0x00, 0x00, 0x24, // version+ihl, dscp, total length 36
            0x00, 0x00, 0x40, 0x00, // id, flags
            0x40, UDP, 0x00, 0x00, // ttl, protocol, checksum
            127, 0, 0, 1, // source
            127, 0, 0, 53, // destination
        ]);
        frame.extend_from_slice(&[
            0x1f, 0x90, // source port 8080
            0x00, 0x35, // destination port 53
            0x00, 0x10, 0x00, 0x00, // length, checksum
        ]);
        frame
    }

    #[test]
    fn an_ipv4_datagram_reads_the_addresses_and_ports_it_carries() {
        let headers = read(&udp_over_ipv4(), 14);
        assert_eq!(headers.ip_version, Some(4));
        assert_eq!(headers.protocol, Some(UDP));
        assert_eq!(headers.source.as_deref(), Some("127.0.0.1"));
        assert_eq!(headers.destination.as_deref(), Some("127.0.0.53"));
        assert_eq!(headers.source_port, Some(8080));
        assert_eq!(headers.destination_port, Some(53));
    }

    #[test]
    fn a_frame_cut_short_by_the_snaplen_reports_what_it_has_and_no_more() {
        let frame = udp_over_ipv4();
        // Everything through the IP header, and not one byte of the transport.
        let headers = read(&frame[..34], 14);
        assert_eq!(headers.destination.as_deref(), Some("127.0.0.53"));
        assert_eq!(
            headers.destination_port, None,
            "a port that was never copied is absent, not zero"
        );
    }

    #[test]
    fn an_ipv6_packet_with_an_extension_header_does_not_invent_ports() {
        let mut frame = vec![0u8; 14];
        let mut header = vec![0u8; 40];
        header[0] = 0x60;
        // Hop-by-hop options, which is not a transport header.
        header[6] = 0;
        frame.extend_from_slice(&header);
        frame.extend_from_slice(&[0u8; 8]);

        let headers = read(&frame, 14);
        assert_eq!(headers.ip_version, Some(6));
        assert_eq!(headers.protocol, Some(0));
        assert_eq!(headers.source_port, None);
        assert_eq!(headers.destination_port, None);
    }

    #[test]
    fn a_frame_this_reader_does_not_know_says_so_rather_than_guessing() {
        // ARP: a link-layer frame whose network header is not IP at all.
        let frame = vec![0u8; 14 + 28];
        let headers = read(&frame, 14);
        assert_eq!(headers.ip_version, None);
        assert_eq!(headers.protocol, None);
        assert_eq!(headers.source, None);
    }

    #[test]
    fn an_unnamed_protocol_reports_its_number_rather_than_a_guess() {
        assert_eq!(protocol_name(TCP), "tcp");
        assert_eq!(protocol_name(47), "47");
    }
}
