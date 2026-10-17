//! `gpu.api`: how long the host spent inside each GPU API call, through the
//! preload shim.
//!
//! One event per call into an interposed entry point -- the CUDA driver's
//! launch, copy, synchronize and allocation calls, and Vulkan's submission and
//! wait calls -- stamped when the call was entered and carrying its duration
//! (`latency_ns`), its name and what it returned. `shim/src/gpu.rs` is the
//! other half, and says which programs it can see: those that call the API
//! through the dynamic linker. A program that fetched its own function
//! pointers reports nothing, and the stream's closing note says so by name.
//!
//! This is the host's view. `cuLaunchKernel` returns once the launch is
//! queued, so its latency is the submission and not the kernel; the kernel's
//! time is paid by whichever synchronize waits for it. `gpu.kernel` is the
//! device's view, through CUPTI, and the two are not interchangeable.

use crate::collect::mem::heap::{Record, ShimPrepared, Stream, Which, OP_GPU_CALL};
use crate::collect::task::Comms;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};

/// The interposed entry points, by the index the shim sends. The same list is
/// in `shim/src/gpu.rs`, and both sides test the order.
pub const ENTRY_POINTS: &[&str] = &[
    "cuLaunchKernel",
    "cuMemcpyHtoD_v2",
    "cuMemcpyDtoH_v2",
    "cuMemcpyHtoDAsync_v2",
    "cuMemcpyDtoHAsync_v2",
    "cuCtxSynchronize",
    "cuStreamSynchronize",
    "cuEventSynchronize",
    "cuMemAlloc_v2",
    "cuMemFree_v2",
    "vkQueueSubmit",
    "vkQueueSubmit2",
    "vkQueueWaitIdle",
    "vkDeviceWaitIdle",
    "vkWaitForFences",
    "vkQueuePresentKHR",
    "cuMemsetD8_v2",
    "cuModuleLoadData",
    "cuCtxCreate_v2",
    "nvtxRangePushA",
    "nvtxRangePop",
];

/// The name a user filters on: the entry point without the ABI suffix `cuda.h`
/// binds it to, so `api == 'cuMemcpyHtoD'` means what it says.
fn api_name(index: u64) -> Option<&'static str> {
    let name = ENTRY_POINTS.get(usize::try_from(index).ok()?)?;
    Some(name.strip_suffix("_v2").unwrap_or(name))
}

pub fn prepare() -> Result<Box<dyn Prepared>, String> {
    Ok(Box::new(ShimPrepared::reporting(
        Which::GpuApi,
        std::time::Duration::from_nanos(crate::request::Mode::DEFAULT_POLL_INTERVAL.as_nanos()),
        |stream, _| Ok(Box::new(ApiCollector::new(stream))),
    )?))
}

pub struct ApiCollector {
    id: CollectorId,
    stream: Stream,
    comms: Comms,
    /// Records naming an entry point this build does not know, which can only
    /// be a shim and a reader from different builds.
    unnamed: u64,
}

impl ApiCollector {
    fn new(stream: Stream) -> Self {
        Self {
            id: CollectorId::new("gpu.api/preload"),
            stream,
            comms: Comms::new(),
            unnamed: 0,
        }
    }

    fn events(&mut self, seq: u64) -> Vec<Event> {
        let pid = self.stream.pid();
        let mut events = Vec::new();
        for (offset, record) in self.stream.drain().into_iter().enumerate() {
            if record.op != OP_GPU_CALL {
                continue;
            }
            match self.event(record, pid, seq + offset as u64) {
                Some(event) => events.push(event),
                None => self.unnamed += 1,
            }
        }
        events
    }

    fn event(&mut self, record: Record, pid: i32, seq: u64) -> Option<Event> {
        let api = api_name(record.ptr)?;
        let result = record.extra as i64;
        let mut attrs = vec![
            Attr::uint("latency_ns", record.size),
            Attr::text("api", api),
            Attr::text(
                "family",
                if api.starts_with("vk") {
                    "vulkan"
                } else if api.starts_with("nvtx") {
                    "nvtx"
                } else {
                    "cuda"
                },
            ),
            // Zero is success in both APIs; Vulkan's positive codes are
            // statuses (`VK_TIMEOUT`, `VK_NOT_READY`) rather than failures, so
            // the code rides along and nothing here reads it as an error.
            Attr::int("result", result),
            Attr::uint("tid", u64::from(record.tid)),
            Attr::uint("pid", pid as u64),
        ];
        if let Some(comm) = self.comms.get(record.tid as i32) {
            attrs.push(Attr::text("comm", comm));
        }
        Some(Event {
            ts_ns: record.ts_ns,
            ts_err_ns: 0,
            kind: "gpu.api".to_owned(),
            // The device a call went to is the context's, and asking the
            // driver inside every call would cost more than the call; the
            // thread is what the shim knows.
            keys: vec![KeyValue::task(record.tid as i32)],
            attrs,
            collector_id: self.id.clone(),
            seq,
        })
    }
}

impl Collector for ApiCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "latency_ns"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        Ok(self.events(seq))
    }

    fn stop(&mut self) -> Vec<Event> {
        let trailing = self.events(u64::MAX / 2);
        self.stream.close();
        trailing
    }

    fn tick(&self) -> Option<std::time::Duration> {
        Some(crate::collect::mem::heap::DRAIN_INTERVAL)
    }

    fn notes(&self) -> Vec<String> {
        self.stream.notes()
    }

    fn losses(&self) -> Losses {
        let mut losses = self.stream.losses();
        if self.unnamed > 0 {
            losses.undecoded += self.unnamed;
            losses.note(format!(
                "{} calls named an entry point this build does not know: the shim and `omn` \
                 are from different builds",
                self.unnamed
            ));
        }
        losses
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::mem::heap::tests::{full_record, pipe_pair, write};

    fn collector() -> (std::os::fd::RawFd, ApiCollector) {
        let (read, write) = pipe_pair();
        let stream = Stream::only_reader(read, std::process::id() as i32, Which::GpuApi);
        (write, ApiCollector::new(stream))
    }

    /// Kept beside the shim's own copy of the test: the index is the only
    /// thing naming a call.
    #[test]
    fn the_entry_points_are_in_the_order_the_shim_sends_them() {
        assert_eq!(ENTRY_POINTS.len(), 21);
        assert_eq!(ENTRY_POINTS[0], "cuLaunchKernel");
        assert_eq!(ENTRY_POINTS[5], "cuCtxSynchronize");
        assert_eq!(ENTRY_POINTS[10], "vkQueueSubmit");
        assert_eq!(ENTRY_POINTS[15], "vkQueuePresentKHR");
        assert_eq!(ENTRY_POINTS[16], "cuMemsetD8_v2");
        assert_eq!(ENTRY_POINTS[20], "nvtxRangePop");
    }

    #[test]
    fn a_call_carries_its_duration_its_name_and_what_it_returned() {
        let (writer, mut collector) = collector();
        write(writer, &full_record(OP_GPU_CALL, 1, 4_200, 0, 9, 0));
        write(
            writer,
            &full_record(OP_GPU_CALL, 14, 77, (-4i64) as u64, 9, 0),
        );
        let events = collector.events(0);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].attr("latency_ns").unwrap().as_f64(),
            Some(4_200.0)
        );
        assert_eq!(
            events[0].attr("api").unwrap().to_string(),
            "cuMemcpyHtoD",
            "the ABI suffix is not part of the name a user filters on"
        );
        assert_eq!(events[0].attr("family").unwrap().to_string(), "cuda");
        assert_eq!(
            events[1].attr("api").unwrap().to_string(),
            "vkWaitForFences"
        );
        assert_eq!(events[1].attr("family").unwrap().to_string(), "vulkan");
        assert_eq!(events[1].attr("result").unwrap().as_f64(), Some(-4.0));
    }

    #[test]
    fn an_index_this_build_does_not_know_is_counted_rather_than_named() {
        let (writer, mut collector) = collector();
        write(writer, &full_record(OP_GPU_CALL, 999, 1, 0, 9, 0));
        assert!(collector.events(0).is_empty());
        assert_eq!(collector.losses().undecoded, 1);
    }

    /// A program that reported nothing is almost never a program that made no
    /// call, and the note has to name the way it most likely went around --
    /// as a note, which is rendered, and not as a loss, which is not.
    #[test]
    fn a_program_that_reported_no_call_is_told_why_it_might_not_have() {
        let (_writer, mut collector) = collector();
        collector.stop();
        let notes = collector.notes().join("\n");
        assert!(notes.contains("cuGetProcAddress"), "{notes}");
        assert!(collector.losses().is_clean());
    }
}
