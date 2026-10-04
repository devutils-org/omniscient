// SPDX-License-Identifier: MIT OR GPL-2.0
//! `net.socket`: how long a send or a receive took, per socket.
//!
//! Both halves of each function, counted in the kernel, and **the two
//! directions are timed at different depths because the kernel left no other
//! choice.**
//!
//! Receives are timed at `sock_recvmsg`, which every `recv`, `recvfrom`,
//! `recvmsg` and `read` on a socket calls out of line, for every family.
//!
//! Sends are not timed at `sock_sendmsg`, which is the catalog's name and the
//! receive side's mirror, because **userspace sends do not reach it**. Measured
//! here: 2000 sends over a loopback socket, zero calls. Every syscall path --
//! `sendto`, `sendmsg`, `write` -- calls `__sock_sendmsg`, which the compiler
//! inlined everywhere; `sock_sendmsg` is the out-of-line copy the kernel's own
//! callers use. What each of those paths does call out of line is the family's
//! `sock->ops->sendmsg`, and the four that carry nearly all traffic share one
//! prototype: `inet_sendmsg`, `inet6_sendmsg`, `unix_stream_sendmsg` and
//! `unix_dgram_sendmsg`. A send on a netlink or a packet socket is therefore
//! not timed, and a receive on one is; the run says so.
//!
//! Every function here has the `struct socket *` as its first argument, which
//! is the point: from there `sock->sk` is one typed load, and the socket's
//! cookie is one helper call on it.
//!
//! **The cookie is generated here, not found.** `bpf_get_socket_cookie` on a
//! `struct sock *` assigns one if nothing has yet, which is the opposite of
//! what the raw-tracepoint sources in this family can do: they read
//! `skc_cookie` and find zero on any socket nothing has dumped, and
//! `docs/scenarios/net.md` item 11 is that finding. A cookie this source asks
//! for is the same number `sock_diag` reports for the socket afterwards, so
//! `net.socket` joins `net.tcp.rtt` and `net.tcp.lifetime` whether or not
//! either was running first.
//!
//! **It fires on everything, so it counts in the kernel.** Loopback traffic
//! alone is a few hundred thousand sends a second on this machine, and a ring
//! record per call would make the measurement's cost grow with the thing being
//! measured. What the kernel keeps is `fentry::HistogramProgram`'s row per
//! `(task, op, socket, log2 bucket)`, with the family and the peer port in the
//! row's key beside the cookie -- functionally dependent on it, so they cost no
//! cardinality, and present so a `dport` predicate has something to match.
//!
//! **The interface is the route's.** The row's `dev` is the index of the
//! interface the socket's cached route leaves by (`sk->sk_dst_cache->dev`),
//! which is what joins a socket to `net.bytes` and `net.qdisc` on the same
//! interface. It is the route the kernel holds at the moment of the call: a
//! socket with none -- a unix socket, an unconnected datagram socket -- has
//! zero there, and so does a socket in another network namespace, whose index
//! would name some other interface here. Functionally dependent on the socket
//! until its route changes, so it costs no cardinality either.

use crate::bpf::attach::fentry::{
    self, HistogramProgram, HistogramSpec, Narrowing, Timed, BPF_H, FILL_AUX, FILL_DEV,
    FILL_OBJECT, GET_SOCKET_COOKIE,
};
use crate::bpf::btf::Btf;
use crate::bpf::insn::{read_narrow, read_pointer, Insn, Program, BPF_DW, BPF_W, R10};

/// What a row's `op` says the call was.
pub const OP_SEND: u32 = 0;
pub const OP_RECV: u32 = 1;

const SEND_SIGNATURE: [&str; 3] = ["socket *", "msghdr *", "size_t"];
const RECV_SIGNATURE: [&str; 3] = ["socket *", "msghdr *", "int"];

/// The families' send functions. `inet_sendmsg` is required; the others are
/// timed where the kernel has them -- IPv6 and unix sockets can be modules, and
/// a module's functions are in its own BTF rather than in `vmlinux`.
const SENDS: [(&str, bool); 4] = [
    ("inet_sendmsg", true),
    ("inet6_sendmsg", false),
    ("unix_stream_sendmsg", false),
    ("unix_dgram_sendmsg", false),
];

const MAX_PENDING: u32 = 16384;
pub const MAX_ROWS: u32 = 32768;

/// The loaded programs.
pub struct SocketProgram {
    inner: HistogramProgram,
    /// The send functions this kernel did not have, by name.
    pub missing: Vec<String>,
}

struct Offsets {
    sk: i32,
    family: i32,
    dport: i32,
    route: Route,
}

/// The walk from a socket to the interface its route leaves by:
/// `sk->sk_dst_cache->dev->ifindex`, and the socket's network namespace, which
/// is what an interface index means anything in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Route {
    dst_cache: i32,
    dst_dev: i32,
    ifindex: i32,
    /// `sk->__sk_common.skc_net.net` and `net->ns.inum`, with the inode of the
    /// namespace this process reads interface names in. `None` on a kernel
    /// with no network namespaces, where there is one and nothing to compare.
    netns: Option<(i32, i32, u64)>,
}

impl Route {
    fn read(btf: &Btf, common: i32) -> Result<Self, String> {
        let own = crate::event::own_netns();
        let netns = match btf.member_offset("possible_net_t", "net") {
            Ok(pointer) if own != 0 => Some((
                common + btf.member_offset("sock_common", "skc_net")? + pointer,
                btf.member_offset("net", "ns")? + btf.member_offset("ns_common", "inum")?,
                own,
            )),
            _ => None,
        };
        Ok(Self {
            dst_cache: btf.member_offset("sock", "sk_dst_cache")?,
            dst_dev: btf.member_offset("dst_entry", "dev")?,
            ifindex: btf.member_offset("net_device", "ifindex")?,
            netns,
        })
    }
}

impl Offsets {
    fn read(btf: &Btf) -> Result<Self, String> {
        let common = btf.member_offset("sock", "__sk_common")?;
        Ok(Self {
            sk: btf.member_offset("socket", "sk")?,
            family: common + btf.member_offset("sock_common", "skc_family")?,
            dport: common + btf.member_offset("sock_common", "skc_dport")?,
            route: Route::read(btf, common)?,
        })
    }
}

/// Eight bytes of the exit program's stack below everything `fentry` keeps
/// there, which the route's reads land in.
const ROUTE_SCRATCH: i16 = -104;

/// The index of the interface the socket in `r7` routes by, into the row's
/// `dev`: zero when the socket has no cached route (a unix socket, an
/// unconnected datagram socket, a connection not yet routed) or lives in
/// another network namespace, whose index would name nothing here.
///
/// Uses `r8`, which the exit half loads only after `fill` returns, and leaves
/// `r6`, `r7` and `r9` alone.
fn route_interface(program: &mut Program, route: &Route) {
    if let Some((net_at, inum_at, own)) = route.netns {
        read_pointer(program, ROUTE_SCRATCH, 8, 7, net_at);
        program.jump_to("route_done", Insn::jeq_imm(8, 0, 0));
        read_narrow(program, ROUTE_SCRATCH, 4, 8, inum_at);
        program.push(Insn::ldx(BPF_W, 1, R10, ROUTE_SCRATCH));
        program.extend(Insn::ld_imm64(2, own as i64));
        program.jump_to("route_done", Insn::jne_reg(1, 2, 0));
    }
    read_pointer(program, ROUTE_SCRATCH, 8, 7, route.dst_cache);
    program.jump_to("route_done", Insn::jeq_imm(8, 0, 0));
    read_pointer(program, ROUTE_SCRATCH, 8, 8, route.dst_dev);
    program.jump_to("route_done", Insn::jeq_imm(8, 0, 0));
    read_narrow(program, FILL_DEV, 4, 8, route.ifindex);
    program.label("route_done");
}

/// The socket's cookie, family and peer port, into the row key.
///
/// `sock->sk` is null for a socket that was never given one or has been
/// released, and the helper refuses a null; such a call keeps a zero cookie and
/// is counted under it, which the collector reports as unnamed.
fn socket_identity(program: &mut Program, at: &Offsets) {
    program.push(Insn::ldx(BPF_DW, 1, 6, 0));
    program.push(Insn::ldx(BPF_DW, 7, 1, at.sk as i16));
    program.jump_to("fill_done", Insn::jeq_imm(7, 0, 0));
    program.push(Insn::mov64_reg(1, 7));
    program.push(Insn::call(GET_SOCKET_COOKIE));
    program.push(Insn::stx(BPF_DW, R10, FILL_OBJECT, 0));
    // `(family << 16) | dport`, the port as the kernel holds it -- network
    // order -- so the collector turns it round once, in one place.
    program.push(Insn::ldx(BPF_H, 1, 7, at.family as i16));
    program.push(Insn::lsh64_imm(1, 16));
    program.push(Insn::ldx(BPF_H, 2, 7, at.dport as i16));
    program.push(Insn::add64_reg(1, 2));
    program.push(Insn::stx(BPF_W, R10, FILL_AUX, 1));
    route_interface(program, &at.route);
    program.label("fill_done");
}

impl SocketProgram {
    pub fn load(narrowing: Narrowing) -> Result<Self, String> {
        let btf = crate::bpf::kernel_btf()?;
        let offsets = Offsets::read(btf)?;
        let mut timed = vec![Timed {
            function: fentry::expect("sock_recvmsg", &[&RECV_SIGNATURE])?,
            op: OP_RECV,
        }];
        let mut missing = Vec::new();
        for (name, required) in SENDS {
            match fentry::expect(name, &[&SEND_SIGNATURE]) {
                Ok(function) => timed.push(Timed {
                    function,
                    op: OP_SEND,
                }),
                Err(reason) if required => return Err(reason),
                Err(_) => missing.push(name.to_owned()),
            }
        }
        let fill = |program: &mut Program, _: &Timed| {
            socket_identity(program, &offsets);
            Ok(())
        };
        let inner = HistogramProgram::load(HistogramSpec {
            name: "omn_sock",
            timed,
            narrowing,
            gate: &|_, _| {},
            fill: &fill,
            max_pending: MAX_PENDING,
            max_rows: MAX_ROWS,
        })?;
        Ok(Self { inner, missing })
    }

    pub fn drain(&self, last: bool) -> Result<fentry::Drained, String> {
        self.inner.drain(last)
    }

    pub fn still_pending(&self) -> (usize, usize) {
        self.inner.still_pending()
    }

    pub fn dropped(&self) -> (u64, u64, u64) {
        self.inner.dropped()
    }

    pub fn missed(&self) -> u64 {
        self.inner.missed()
    }
}

/// The family and the peer port a row's `aux` carries, the port in host order.
pub fn family_and_port(aux: u32) -> (u16, u16) {
    ((aux >> 16) as u16, u16::from_be((aux & 0xffff) as u16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_port_is_turned_round_once() {
        let aux = (2u32 << 16) | u32::from(8080u16.to_be());
        assert_eq!(family_and_port(aux), (2, 8080));
    }

    #[test]
    fn the_identity_is_built_against_the_running_kernel() {
        let Ok(btf) = crate::bpf::kernel_btf() else {
            eprintln!("skipped: this kernel publishes no BTF");
            return;
        };
        let offsets = Offsets::read(btf).expect("the offsets");
        let mut program = Program::new();
        socket_identity(&mut program, &offsets);
        let program = program.build();
        assert!(program
            .iter()
            .any(|insn| insn.code == 0x85 && insn.imm == GET_SOCKET_COOKIE));
        // The null check comes before the helper that would refuse a null.
        let check = program
            .iter()
            .position(|insn| insn.code == 0x15 && insn.imm == 0)
            .expect("a null check");
        let call = program
            .iter()
            .position(|insn| insn.code == 0x85)
            .expect("the cookie helper");
        assert!(check < call);
        // The route's interface lands in the row's `dev`, and, where there
        // are namespaces, only after the socket's was compared with ours.
        let into_dev = program
            .iter()
            .position(|insn| insn.code == 0x62 && insn.off == FILL_DEV)
            .expect("the interface is written into the row's dev");
        if let Some((_, _, own)) = offsets.route.netns {
            let compared = program
                .windows(2)
                .position(|pair| {
                    pair[0].imm == own as u32 as i32 && pair[1].imm == (own >> 32) as i32
                })
                .expect("the namespace's inode is loaded whole");
            assert!(compared < into_dev);
        }
    }
}
