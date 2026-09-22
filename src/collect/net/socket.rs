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
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if self.mode != Mode::Trace {
            return Err(format!(
                "`{}` mode cannot measure how long a send or a receive took: it is the gap \
                 between the two halves of one call, and the catalog declares `net.socket` in \
                 `trace` alone",
                self.mode
            ));
        }
        let narrowing = narrowing("net.socket", target)?;
        let load = Box::new(move || -> Result<Box<dyn Rows>, String> {
            let program = SocketProgram::load(narrowing)?;
            if !program.missing.is_empty() {
                // Said where it is decided rather than guessed at later: a
                // family whose send function is absent contributes receives
                // and no sends, and a reader comparing the two must know.
                eprintln!(
                    "note: net.socket: this kernel has no {} in its BTF, so sends on those \
                     sockets are not timed",
                    program.missing.join(", ")
                );
            }
            Ok(Box::new(program))
        });
        Ok(Box::new(HistogramCollector::new(
            Source {
                kind: "net.socket",
                collector: "net.socket/bpf:fentry",
                call: "a send or a receive",
                load,
                describe: describe(),
                notes: vec![
                    "receives are timed at sock_recvmsg, for every family; sends at \
                     inet_sendmsg, inet6_sendmsg, unix_stream_sendmsg and unix_dgram_sendmsg, \
                     because the syscalls inline the generic send path -- a send on any other \
                     family is not here, and neither are sendfile, splice or io_uring"
                        .to_owned(),
                ],
            },
            self.mode,
            self.filter,
        )))
    }
}

/// `AF_*`, the frozen numbering `socket(2)` takes.
fn family_name(family: u16) -> &'static str {
    match family {
        1 => "unix",
        2 => "inet",
        10 => "inet6",
        16 => "netlink",
        17 => "packet",
        _ => "other",
    }
}

fn describe() -> Describe {
    Box::new(|row: &Row| {
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
            Attr::text("family", family_name(family)),
            Attr::uint("dport", u64::from(dport)),
        ];
        let task = KeyValue::task(row.tid as i32);
        if row.object == 0 {
            // No socket behind the call: it was released while the call ran.
            // A cookie of zero is not a cookie, and keying on it would join
            // every such call to every other.
            return (vec![task], attrs);
        }
        attrs.push(Attr::uint("cookie", row.object));
        (vec![KeyValue::Sock { cookie: row.object }, task], attrs)
    })
}

impl Rows for SocketProgram {
    fn drain(&self) -> Result<(Vec<Row>, fentry::DrainLosses), String> {
        SocketProgram::drain(self)
    }

    fn still_pending(&self) -> usize {
        SocketProgram::still_pending(self)
    }

    fn dropped(&self) -> (u64, u64) {
        SocketProgram::dropped(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(object: u64) -> Row {
        Row {
            tid: 9,
            tgid: 9,
            op: OP_SEND,
            bucket: 12,
            object,
            dev: 0,
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
    fn every_filter_field_the_catalog_declares_is_on_an_event() {
        let catalog = crate::catalog::Catalog::embedded().expect("the built-in catalog");
        let mut describe = describe();
        let (_, attrs) = describe(&row(77));
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
