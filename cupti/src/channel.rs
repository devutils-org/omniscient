//! The channel to `omn`: frames batched into messages on the socket
//! `OMN_CUPTI_FD` names.
//!
//! Nothing here runs on a thread of the program's: CUPTI hands its buffers
//! back on its own thread or on the flush thread, and the closing flush runs
//! at exit. So a send may wait, unlike the preload shim's, which never does
//! -- but not forever: the socket's send timeout bounds it, and a batch that
//! could not be sent in that time is dropped, counted, and reported in the
//! next one that is ([`crate::wire::OP_LOST`]). The program is never failed
//! for it, and never hung.
//!
//! What an unsent batch loses is everything it would have told `omn`, and all
//! of it is counted: its records, and the counts it carried -- records CUPTI
//! dropped, records handed back unfinished -- because those are records too,
//! lost one step earlier. A kernel name it carried is forgotten as sent, so
//! the next kernel of that name sends it again rather than reaching `omn`
//! under an id it never learnt.

use crate::wire::{self, Writer};

/// How long one send may wait for `omn` to drain the socket.
const SEND_TIMEOUT_MS: i64 = 2_000;

pub struct Channel {
    fd: i32,
    buffer: Vec<u8>,
    at: usize,
    frames: u32,
    /// The process this channel belongs to, written in every message's
    /// header. A forked child holds a copy, and sends nothing with it.
    pid: u32,
    /// What the current batch accounts for: activity frames, and the counts
    /// of dropped and unfinished records it carries. All of it is lost if
    /// the batch cannot be sent.
    losses_in_batch: u64,
    /// Kernel names first sent in the current batch.
    names_in_batch: Vec<Vec<u8>>,
    /// Activity records produced, and records that could not be sent.
    pub seen: u64,
    pub lost: u64,
    reported_lost: u64,
    /// Kernel names already sent, by name, with their id.
    names: std::collections::HashMap<Vec<u8>, u32>,
    /// The next name id: never reused, so a name sent again after a lost
    /// batch cannot take an id another name holds.
    next_name: u32,
    /// Once a send has failed for a reason other than a timeout, `omn` is
    /// gone and nothing more is attempted.
    dead: bool,
}

impl Channel {
    /// The channel `OMN_CUPTI_FD` names, or `None` when the program was not
    /// launched by `omn`.
    pub fn from_environment() -> Option<Self> {
        let fd: i32 = std::env::var(wire::FD_VARIABLE).ok()?.parse().ok()?;
        let timeout = libc::timeval {
            tv_sec: SEND_TIMEOUT_MS / 1000,
            tv_usec: (SEND_TIMEOUT_MS % 1000) * 1000,
        };
        // SAFETY: setting an option on a descriptor this process inherited;
        // a failure leaves the default, which blocks, and is harmless here
        // because `omn` drains continuously.
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_SNDTIMEO,
                (&timeout as *const libc::timeval).cast(),
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            );
        }
        Some(Self::on(fd))
    }

    fn on(fd: i32) -> Self {
        let pid = std::process::id();
        let mut buffer = vec![0u8; wire::MESSAGE_MAX];
        Writer::new(&mut buffer, pid);
        Self {
            fd,
            buffer,
            at: wire::HEADER,
            frames: 0,
            pid,
            losses_in_batch: 0,
            names_in_batch: Vec::new(),
            seen: 0,
            lost: 0,
            reported_lost: 0,
            names: std::collections::HashMap::new(),
            next_name: 1,
            dead: false,
        }
    }

    fn writer(&mut self) -> Writer<'_> {
        Writer {
            buffer: &mut self.buffer,
            at: self.at,
            frames: self.frames,
        }
    }

    /// Appends one frame, sending the batch first when it would not fit.
    pub fn frame(&mut self, op: u8, payload: &[u8]) {
        if !self.writer().fits(payload.len()) {
            self.send();
        }
        let mut writer = self.writer();
        writer.frame(op, payload);
        let (at, frames) = (writer.at, writer.frames);
        self.at = at;
        self.frames = frames;
        match op {
            wire::OP_KERNEL | wire::OP_MEMCPY | wire::OP_MEMSET => {
                self.losses_in_batch += 1;
                self.seen += 1;
            }
            wire::OP_DROPPED | wire::OP_INCOMPLETE => {
                self.losses_in_batch += count(payload);
            }
            _ => {}
        }
    }

    /// The id of a kernel name, sending the name the first time it is met.
    pub fn name(&mut self, name: &[u8]) -> u32 {
        if let Some(id) = self.names.get(name) {
            return *id;
        }
        let id = self.next_name;
        self.next_name += 1;
        // A name longer than a frame can carry is cut; a mangled template
        // name of 60 KiB is not one a person reads to the end.
        let room = wire::MESSAGE_MAX - wire::HEADER - wire::FRAME_HEADER - 8 - 8;
        let kept = &name[..name.len().min(room)];
        let mut payload = Vec::with_capacity(4 + kept.len());
        payload.extend_from_slice(&id.to_ne_bytes());
        payload.extend_from_slice(kept);
        self.frame(wire::OP_NAME, &payload);
        self.names.insert(name.to_vec(), id);
        self.names_in_batch.push(name.to_vec());
        id
    }

    /// Sends what is batched.
    pub fn send(&mut self) {
        if std::process::id() != self.pid {
            // A forked child's copy of its parent's batch: the parent sends
            // it, so the child drops it rather than send it twice.
            self.forget_batch();
            return;
        }
        let reported_before = self.reported_lost;
        if self.lost > self.reported_lost {
            let lost = self.lost.to_ne_bytes();
            if self.writer().fits(lost.len()) {
                self.reported_lost = self.lost;
                let mut writer = self.writer();
                writer.frame(wire::OP_LOST, &lost);
                let (at, frames) = (writer.at, writer.frames);
                self.at = at;
                self.frames = frames;
            }
        }
        if self.frames == 0 {
            return;
        }
        let mut writer = self.writer();
        let length = writer.finish().len();
        let sent = !self.dead && {
            // SAFETY: sending bytes we own on the inherited descriptor;
            // MSG_NOSIGNAL so a vanished reader is an error, not a SIGPIPE
            // delivered to the program.
            let sent = unsafe {
                libc::send(
                    self.fd,
                    self.buffer.as_ptr().cast(),
                    length,
                    libc::MSG_NOSIGNAL,
                )
            };
            if sent < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EAGAIN) {
                    self.dead = true;
                }
                false
            } else {
                true
            }
        };
        if !sent {
            self.lost += self.losses_in_batch;
            self.reported_lost = reported_before;
            for name in self.names_in_batch.drain(..) {
                self.names.remove(&name);
            }
        }
        self.forget_batch();
    }

    fn forget_batch(&mut self) {
        self.at = wire::HEADER;
        self.frames = 0;
        self.losses_in_batch = 0;
        self.names_in_batch.clear();
    }
}

/// The count an `OP_DROPPED` or `OP_INCOMPLETE` frame carries first.
fn count(payload: &[u8]) -> u64 {
    payload
        .get(..8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_ne_bytes)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A channel whose reader is gone: every send fails.
    fn orphan() -> Channel {
        let mut fds = [0i32; 2];
        // SAFETY: a socket pair into an array we own.
        let made =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()) };
        assert_eq!(made, 0);
        // SAFETY: closing the reader's end, which this test owns.
        unsafe { libc::close(fds[1]) };
        Channel::on(fds[0])
    }

    #[test]
    fn an_unsent_batch_loses_the_counts_it_carried_too() {
        let mut channel = orphan();
        channel.frame(wire::OP_KERNEL, &[0; wire::KERNEL_FIELDS]);
        channel.frame(wire::OP_MEMCPY, &[0; wire::COPY_FIELDS]);
        channel.frame(
            wire::OP_DROPPED,
            &[&1_258u64.to_ne_bytes()[..], &[0; 8]].concat(),
        );
        channel.frame(wire::OP_INCOMPLETE, &7u64.to_ne_bytes());
        channel.send();
        assert_eq!(channel.lost, 2 + 1_258 + 7);
        assert_eq!(channel.seen, 2);
    }

    #[test]
    fn a_name_in_an_unsent_batch_is_sent_again_under_a_fresh_id() {
        let mut channel = orphan();
        let first = channel.name(b"saxpy");
        assert_eq!(channel.name(b"saxpy"), first, "interned while unsent");
        channel.send();
        let other = channel.name(b"gemm");
        let again = channel.name(b"saxpy");
        assert_ne!(again, first, "a name omn never learnt is sent again");
        assert_ne!(again, other, "and its id is no other name's");
    }
}
