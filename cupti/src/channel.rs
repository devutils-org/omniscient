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

use crate::wire::{self, Writer};

/// How long one send may wait for `omn` to drain the socket.
const SEND_TIMEOUT_MS: i64 = 2_000;

pub struct Channel {
    fd: i32,
    buffer: Vec<u8>,
    at: usize,
    frames: u32,
    /// Activity frames in the current batch: what is lost if it cannot be
    /// sent.
    records_in_batch: u64,
    /// Activity records produced, and records that could not be sent.
    pub seen: u64,
    pub lost: u64,
    reported_lost: u64,
    /// Kernel names already sent, by name, with their id.
    names: std::collections::HashMap<Vec<u8>, u32>,
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
        let pid = std::process::id();
        let mut buffer = vec![0u8; wire::MESSAGE_MAX];
        Writer::new(&mut buffer, pid);
        Some(Self {
            fd,
            buffer,
            at: wire::HEADER,
            frames: 0,
            records_in_batch: 0,
            seen: 0,
            lost: 0,
            reported_lost: 0,
            names: std::collections::HashMap::new(),
            dead: false,
        })
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
        if matches!(op, wire::OP_KERNEL | wire::OP_MEMCPY | wire::OP_MEMSET) {
            self.records_in_batch += 1;
            self.seen += 1;
        }
    }

    /// The id of a kernel name, sending the name the first time it is met.
    pub fn name(&mut self, name: &[u8]) -> u32 {
        if let Some(id) = self.names.get(name) {
            return *id;
        }
        let id = self.names.len() as u32 + 1;
        // A name longer than a frame can carry is cut; a mangled template
        // name of 60 KiB is not one a person reads to the end.
        let room = wire::MESSAGE_MAX - wire::HEADER - wire::FRAME_HEADER - 8 - 8;
        let kept = &name[..name.len().min(room)];
        let mut payload = Vec::with_capacity(4 + kept.len());
        payload.extend_from_slice(&id.to_ne_bytes());
        payload.extend_from_slice(kept);
        self.frame(wire::OP_NAME, &payload);
        self.names.insert(name.to_vec(), id);
        id
    }

    /// Sends what is batched.
    pub fn send(&mut self) {
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
            self.lost += self.records_in_batch;
            self.reported_lost = reported_before;
        }
        self.at = wire::HEADER;
        self.frames = 0;
        self.records_in_batch = 0;
    }
}
