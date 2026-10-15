//! `net.socket` over fentry, bucketed in the kernel.
//!
//! `src/bpf/net_socket.rs` has the programs. This file is the event: the socket
//! is the series -- `sock:<cookie>`, the number `sock_diag` reports -- and the
//! task is the second key, because a socket is shared by whoever holds its
//! descriptor and a send is made by one of them.
//!
//! The collector is `io`'s histogram collector: the same row, the same drain,
//! the same loss accounting, and a different description of what a row's
//! object is.

use crate::bpf::attach::fentry::{self, Row};
use crate::bpf::attach::net_socket::Receives;
use crate::bpf::attach::net_socket::{family_and_port, SocketProgram, OP_RECV, OP_SEND};
use crate::collect::io::histogram::{Describe, HistogramCollector, Rows, Source};
use crate::collect::io::vfs::narrowing;
use crate::collect::{Collector, Prepared};
use crate::event::{Attr, KeyValue};
use crate::filter::FilterExpr;
use crate::request::{Mode, Target};

pub struct Prepare {
    pub mode: Mode,
    pub filter: Option<FilterExpr>,
    /// Where the receives are timed: the backend's choice.
    pub receives: Receives,
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if !matches!(self.mode, Mode::Trace | Mode::Count) {
            return Err(format!(
                "`{}` mode cannot measure how long a send or a receive took: it is the gap \
                 between the two halves of one call, and a sample that catches one without the \
                 other has no duration",
                self.mode
            ));
        }
        let narrowing = narrowing("net.socket", target)?;
        let receives = self.receives;
        let load = Box::new(move || -> Result<Box<dyn Rows>, String> {
            let program = SocketProgram::load(narrowing, receives)?;
            if !program.missing.is_empty() {
                // Said where it is decided rather than guessed at later: a
                // family whose send function is absent contributes receives
                // and no sends, and a reader comparing the two must know.
                eprintln!(
                    "note: net.socket: this kernel has no {} in its BTF, so calls on those \
                     sockets are not timed",
                    program.missing.join(", ")
                );
            }
            Ok(Box::new(program))
        });
        Ok(Box::new(HistogramCollector::new(
            Source {
                kind: "net.socket",
                collector: match receives {
                    Receives::Generic => "net.socket/bpf:fentry",
                    Receives::Families => "net.socket/bpf:fentry:families",
                },
                call: "a send or a receive",
                load,
                describe: describe(),
                notes: vec![
                    match receives {
                        Receives::Generic => {
                            "receives are timed at sock_recvmsg, for every \
                            family; sends at inet_sendmsg, inet6_sendmsg, unix_stream_sendmsg \
                            and unix_dgram_sendmsg, because the syscalls inline the generic send \
                            path -- a send on any other family is not here, and neither are \
                            sendfile, splice or io_uring"
                        }
                        Receives::Families => {
                            "receives and sends are both timed at the \
                            families' own functions -- inet, inet6 and unix stream and datagram \
                            sockets -- because this kernel inlined sock_recvmsg into the \
                            syscalls as it does the generic send path: a call on any other family \
                            is not here, and neither are sendfile, splice or io_uring"
                        }
                    }
                    .to_owned(),
                    "a row's `iface` is the interface the socket's cached route left by at the \
                     call; a socket with no route (unix, an unconnected datagram socket) or in \
                     another network namespace carries none"
                        .to_owned(),
                ],
            },
            self.mode,
            self.filter,
        )))
    }
}

fn describe() -> Describe {
    // An interface's name, asked for once per index a run sees rather than
    // once per row.
    let mut names: std::collections::BTreeMap<u32, String> = std::collections::BTreeMap::new();
    Box::new(move |row: &Row| {
        let (family, dport) = family_and_port(row.aux);
        let mut attrs = vec![
            Attr::text(
                "op",
                match row.op {
                    OP_SEND => "send",
                    OP_RECV => "recv",
                    _ => "unknown",
                },
            ),
            // The kernel's `AF_*` number, as `net.tcp.lifetime` carries it, so a
            // `family == 2` predicate means the same thing across the family.
            Attr::uint("family", u64::from(family)),
            Attr::uint("dport", u64::from(dport)),
        ];
        let task = KeyValue::task(row.tid as i32);
        // The interface the socket's route left by, when it had one in this
        // namespace (`src/bpf/net_socket.rs`): the key `net.bytes` and
        // `net.qdisc` carry, so a socket joins its interface.
        let route = (row.dev != 0).then(|| {
            let name = names
                .entry(row.dev)
                .or_insert_with(|| super::interfaces::name_of(row.dev).unwrap_or_default())
                .clone();
            attrs.push(Attr::text("iface", name.clone()));
            KeyValue::iface(row.dev, name)
        });
        if row.object == 0 {
            // No socket behind the call: it was released while the call ran.
            // A cookie of zero is not a cookie, and keying on it would join
            // every such call to every other.
            return (std::iter::once(task).chain(route).collect(), attrs);
        }
        attrs.push(Attr::uint("cookie", row.object));
        let keys = [KeyValue::Sock { cookie: row.object }, task]
            .into_iter()
            .chain(route)
            .collect();
        (keys, attrs)
    })
}

impl Rows for SocketProgram {
    fn drain(&self, last: bool) -> Result<fentry::Drained, String> {
        SocketProgram::drain(self, last)
    }

    fn still_pending(&self) -> (usize, usize) {
        SocketProgram::still_pending(self)
    }

    fn dropped(&self) -> (u64, u64, u64) {
        SocketProgram::dropped(self)
    }

    fn missed(&self) -> u64 {
        SocketProgram::missed(self)
    }

    fn unfollowed(&self) -> u64 {
        SocketProgram::unfollowed(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(object: u64) -> Row {
        routed(object, 0)
    }

    fn routed(object: u64, dev: u32) -> Row {
        Row {
            tid: 9,
            tgid: 9,
            op: OP_SEND,
            bucket: 12,
            object,
            dev,
            aux: (2 << 16) | u32::from(8080u16.to_be()),
            count: 3,
            total_ns: 9000,
            bytes: 12288,
            errors: 0,
            comm: "client".to_owned(),
        }
    }

    #[test]
    fn the_socket_is_the_series_and_a_zero_cookie_is_no_socket() {
        let mut describe = describe();
        let (keys, attrs) = describe(&row(77));
        assert_eq!(keys[0].to_string(), "sock:77");
        assert!(attrs
            .iter()
            .any(|attr| attr.name == "dport" && attr.value.to_string() == "8080"));
        let (keys, _) = describe(&row(0));
        assert_eq!(keys.len(), 1, "no sock key without a cookie");
    }

    #[test]
    fn a_routed_socket_carries_its_interface_and_an_unrouted_one_none() {
        let mut describe = describe();
        let (keys, attrs) = describe(&row(77));
        assert!(keys.iter().all(|key| key.iface_name().is_none()));
        assert!(attrs.iter().all(|attr| attr.name != "iface"));
        // Index 1 is loopback on every Linux machine.
        let (keys, attrs) = describe(&routed(77, 1));
        assert_eq!(
            keys[0].to_string(),
            "sock:77",
            "the socket stays the series"
        );
        let iface = keys
            .iter()
            .find_map(|key| key.iface_name())
            .expect("an iface key");
        assert_eq!(iface, "lo");
        assert!(attrs
            .iter()
            .any(|attr| attr.name == "iface" && attr.value.to_string() == "lo"));
    }

    #[test]
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let mut describe = describe();
        let (_, attrs) = describe(&routed(77, 1));
        // `comm` and `bytes` are the shared collector's; the rest are this
        // source's own.
        let names: Vec<&str> = attrs
            .iter()
            .map(|attr| attr.name.as_str())
            .chain(["comm", "bytes"])
            .collect();
        for field in &catalog
            .get(&"net.socket".parse().expect("a source id"))
            .expect("net.socket is in the catalog")
            .filter_fields
        {
            assert!(
                names.contains(&field.as_str()),
                "no event carries `{field}`"
            );
        }
    }
}
