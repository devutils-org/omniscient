//! `gpu.api` through CUPTI's callback API (`vendor:cupti`): every call into
//! the CUDA driver API, timed inside the program by the injection library
//! (`cupti/src/callback.rs`).
//!
//! The same events as the preload shim's (`api.rs`): when a call was entered,
//! how long it took (`latency_ns`), its name and what it returned. Two
//! differences, both said in the catalog: this sees every driver API call,
//! those reached through `cuGetProcAddress` included -- the CUDA runtime
//! fetches every entry point that way, which the shim cannot see -- and it
//! sees nothing but the driver API: Vulkan and NVTX stay the shim's.
//!
//! The library reads `CLOCK_MONOTONIC` at both ends inside the program, so an
//! event is stamped in this tool's clock with no calibration and no error.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use super::kernel::{find_library, wire, KernelPrepared, Socket};
use crate::collect::task::Comms;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::Target;

/// Why a running process is refused, in this source's words.
const REFUSES_PID: &str = "CUPTI's callbacks are subscribed by the injection library the CUDA \
     driver loads at `cuInit` from `CUDA_INJECTION64_PATH`, so `gpu.api` on `vendor:cupti` needs \
     `-- <program>`: a running process has initialised CUDA already";

pub fn prepare() -> Result<Box<dyn Prepared>, String> {
    Ok(Box::new(CallbackPrepared {
        inner: KernelPrepared::new(find_library()?, wire::REPORT_CALLBACK)?,
    }))
}

/// `gpu.kernel`'s socket and library, with this reader's bit, so one launch
/// gives the program one `CUDA_INJECTION64_PATH` whatever CUPTI sources ask.
struct CallbackPrepared {
    inner: KernelPrepared,
}

impl Prepared for CallbackPrepared {
    fn launch_environment(&self) -> Vec<(String, String)> {
        self.inner.launch_environment()
    }

    fn share_launch(&mut self, earlier: &mut dyn Prepared) -> bool {
        self.inner.share_launch(earlier)
    }

    fn shareable(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(&mut self.inner)
    }

    fn requires_launch(&self) -> Option<&'static str> {
        Some(REFUSES_PID)
    }

    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let pid = target
            .first_pid()
            .ok_or_else(|| format!("{REFUSES_PID}; a {} target names none", target.kind()))?;
        let socket = self.inner.socket();
        socket.borrow_mut().close_write();
        Ok(Box::new(CallbackCollector::new(
            socket,
            self.inner.reader(),
            pid,
        )))
    }
}

pub struct CallbackCollector {
    id: CollectorId,
    socket: Rc<RefCell<Socket>>,
    reader: usize,
    launched: i32,
    /// Each process's names, by the id the library gave them.
    names: BTreeMap<u32, HashMap<u32, String>>,
    comms: Comms,
    losses: Losses,
    errors: Vec<String>,
    calls: u64,
}

/// The name a user filters on: the entry point without the ABI suffix
/// (`cuMemcpyHtoD_v2` is `cuMemcpyHtoD`), as the shim's events name it.
fn api_name(name: &str) -> &str {
    name.split_once("_v")
        .filter(|(_, version)| version.chars().all(|c| c.is_ascii_digit()))
        .map_or(name, |(stem, _)| stem)
}

impl CallbackCollector {
    fn new(socket: Rc<RefCell<Socket>>, reader: usize, launched: i32) -> Self {
        Self {
            id: CollectorId::new("gpu.api/vendor:cupti"),
            socket,
            reader,
            launched,
            names: BTreeMap::new(),
            comms: Comms::new(),
            losses: Losses::default(),
            errors: Vec::new(),
            calls: 0,
        }
    }

    fn drain(&mut self, seq: u64) -> Vec<Event> {
        let messages = self.socket.borrow_mut().take(self.reader);
        let mut events = Vec::new();
        for message in messages {
            let Some((pid, frames)) = wire::parse(&message) else {
                self.losses.undecoded += 1;
                self.losses.note(
                    "a message from the injection library was not one this build reads: the \
                     library and `omn` are from different builds",
                );
                continue;
            };
            for frame in frames {
                if let Some(event) = self.frame(pid, frame, seq + events.len() as u64) {
                    events.push(event);
                }
            }
        }
        events
    }

    fn frame(&mut self, pid: u32, frame: wire::Frame<'_>, seq: u64) -> Option<Event> {
        let payload = frame.payload;
        match frame.op {
            wire::OP_NAME if payload.len() >= 4 => {
                let id = u32::from_ne_bytes(payload[..4].try_into().expect("4"));
                let name = String::from_utf8_lossy(&payload[4..])
                    .trim_end_matches('\0')
                    .to_owned();
                self.names.entry(pid).or_default().insert(id, name);
                None
            }
            wire::OP_API => {
                let Some(call) = wire::Api::decode(payload) else {
                    self.losses.undecoded += 1;
                    return None;
                };
                Some(self.event(pid, &call, seq))
            }
            wire::OP_API_UNPAIRED => {
                self.losses.dropped += 1;
                self.losses.note(
                    "a driver API call returned whose entry the callbacks did not see: it was \
                     under way when they were subscribed",
                );
                None
            }
            wire::OP_ERROR => {
                self.errors.push(format!(
                    "pid {pid}: {}",
                    String::from_utf8_lossy(payload).trim_end_matches('\0')
                ));
                None
            }
            // Every other frame on the shared socket is another reader's.
            _ => None,
        }
    }

    fn event(&mut self, pid: u32, call: &wire::Api, seq: u64) -> Event {
        self.calls += 1;
        let name = self
            .names
            .get(&pid)
            .and_then(|names| names.get(&call.name))
            .map_or("?", String::as_str);
        let mut attrs = vec![
            Attr::uint("latency_ns", call.duration_ns),
            Attr::text("api", api_name(name)),
            Attr::text("family", "cuda"),
            Attr::int("result", i64::from(call.result)),
            Attr::uint("tid", u64::from(call.tid)),
            Attr::uint("pid", u64::from(pid)),
        ];
        if let Some(comm) = self.comms.get(call.tid as i32) {
            attrs.push(Attr::text("comm", comm));
        }
        Event {
            ts_ns: call.start_ns,
            ts_err_ns: 0,
            kind: "gpu.api".to_owned(),
            keys: vec![KeyValue::task(call.tid as i32)],
            attrs,
            collector_id: self.id.clone(),
            seq,
        }
    }
}

impl Collector for CallbackCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "latency_ns"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        Ok(self.drain(seq))
    }

    fn stop(&mut self) -> Vec<Event> {
        self.drain(u64::MAX / 2)
    }

    fn tick(&self) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_millis(50))
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }

    fn failure(&self) -> Option<String> {
        (!self.errors.is_empty()).then(|| self.errors.join("; "))
    }

    fn notes(&self) -> Vec<String> {
        if self.calls == 0 {
            vec![format!(
                "no driver API call was reported for pid {}: the program may never have called \
                 `cuInit`, or CUPTI was not loadable in it (the run's failure says which)",
                self.launched
            )]
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_drops_its_abi_suffix_and_nothing_else() {
        assert_eq!(api_name("cuMemcpyHtoD_v2"), "cuMemcpyHtoD");
        assert_eq!(api_name("cuLaunchKernel"), "cuLaunchKernel");
        assert_eq!(api_name("cuCtxCreate_v4"), "cuCtxCreate");
        assert_eq!(api_name("cuGraph_vertex"), "cuGraph_vertex");
    }

    /// A name frame then a call frame make one event, named and timed in the
    /// program's own clock; another reader's frame makes none.
    #[test]
    fn a_call_is_an_event_named_by_its_frame() {
        let socket = KernelPrepared::new(
            std::path::PathBuf::from("/nonexistent"),
            wire::REPORT_CALLBACK,
        )
        .unwrap()
        .socket();
        let mut collector = CallbackCollector::new(socket, 0, 1);
        let mut name = 5u32.to_ne_bytes().to_vec();
        name.extend_from_slice(b"cuMemcpyHtoD_v2\0");
        assert!(collector
            .frame(
                9,
                wire::Frame {
                    op: wire::OP_NAME,
                    payload: &name
                },
                0
            )
            .is_none());
        let call = wire::Api {
            start_ns: 1_000,
            duration_ns: 4_200,
            name: 5,
            result: 0,
            tid: 9,
        }
        .encode();
        let event = collector
            .frame(
                9,
                wire::Frame {
                    op: wire::OP_API,
                    payload: &call,
                },
                0,
            )
            .expect("an event");
        assert_eq!(event.ts_ns, 1_000);
        assert_eq!(event.attr("latency_ns").unwrap().as_f64(), Some(4_200.0));
        assert_eq!(event.attr("api").unwrap().to_string(), "cuMemcpyHtoD");
        let other = [0u8; 40];
        assert!(collector
            .frame(
                9,
                wire::Frame {
                    op: wire::OP_KERNEL,
                    payload: &other
                },
                1
            )
            .is_none());
    }
}
