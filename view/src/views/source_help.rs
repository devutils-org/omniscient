//! Plain-language descriptions for the sources used by the family pages.

/// What is measured and how to interpret its values. Other sources fall back
/// to the catalog's own description, rather than a guessed explanation.
pub fn explanation(source: &str) -> Option<(&'static str, &'static str)> {
    Some(match source {
        "cpu.util" => ("How busy the machine's CPUs are, split by state.", "Values describe CPU use on the machine, not just the selected process."),
        "cpu.oncpu_time" => ("CPU time used by a process or thread while it was actually running.", "A task key identifies the thread. Time spent sleeping or waiting is not CPU time."),
        "cpu.profile" => ("Samples of where execution was running in the call stack.", "More samples suggest more on-CPU time; a sample is an estimate, not an exact call count."),
        "mem.rss" => ("Resident memory: pages of the process currently held in RAM.", "RSS is not necessarily private memory: shared pages can be included."),
        "mem.faults" => ("Page faults: occasions when a process had to resolve a memory page.", "A fault does not always mean a disk read. Minor and major faults can have different costs."),
        "mem.pressure" => ("Time tasks spent stalled because memory was under pressure.", "This is a machine-wide pressure measure, not the amount of RAM used."),
        "io.bytes" => ("Bytes transferred by block devices, split by device and direction.", "These are device-side measurements; they can differ from a process's file reads and writes."),
        "io.latency" => ("How long block I/O requests took to complete.", "Each observation is a request duration. A high value indicates a slow request, not necessarily the whole program's delay."),
        "io.syscall" => ("File I/O system calls made by the target, including their duration and bytes where available.", "The series value is determined by the recording's selected attribute; inspect its unit and detailed view."),
        "io.fs" => ("File-system activity associated with files and operations.", "What can be attributed to a file depends on the collected events and backend."),
        "net.bytes" => ("Bytes sent and received by network interfaces.", "Interface totals are machine-wide; loopback traffic can appear on both sides."),
        "net.socket" => ("The target's socket operations, including transferred bytes and operation durations where available.", "Check the unit and selected attribute: a duration is not a byte count."),
        "net.tcp.retransmit" => ("TCP segments that had to be sent again.", "The measurement is machine-wide; it is not necessarily attributable to the selected process."),
        "net.request" => ("Requests declared by the application, including their duration and associated activity.", "The application must declare requests; an uninstrumented program may not provide this source."),
        "gpu.engine_busy" => ("How busy the GPU's engines were during sampled intervals.", "This is device-wide utilization; the driver's sampling window limits time precision."),
        "gpu.mem" => ("GPU memory held, at the scope of the request.", "Check the source label: device-wide and target measurements are different scopes."),
        "gpu.kernel" => ("GPU kernels launched by the target and their execution durations.", "A kernel is a GPU operation; its duration is not automatically CPU time or end-to-end request latency."),
        "power.energy" => ("Energy measured by hardware power counters, usually for the machine or a hardware domain.", "Energy is measured in joules; watts describe energy per second. Hardware-domain energy is not automatically per-process."),
        "sched.offcpu" => ("Periods when a thread was not running on a CPU.", "Off-CPU time can include sleeping, waiting for I/O or locks, and other states; use the time view for the split."),
        "sched.runqlat" => ("Time a runnable thread waited in the scheduler's run queue before running.", "This is scheduling delay, not time the thread voluntarily slept."),
        "sched.futex" => ("Time and activity around futex operations, commonly used for thread synchronization.", "A futex operation is evidence of synchronization; it does not by itself identify every lock or its owner."),
        "sched.switches" => ("Scheduler context switches involving threads.", "A switch records a scheduling transition; it does not alone explain why a thread stopped."),
        "sched.wakeup" => ("Events that make a thread runnable and, where recorded, who woke it.", "A wake-up is not the same as the thread immediately running; it may wait in the run queue."),
        _ => return None,
    })
}

/// Short labels for scanning a list before opening a source.
pub fn short(source: &str) -> Option<&'static str> {
    Some(match source {
        "cpu.util" => "Machine CPU use by state",
        "cpu.oncpu_time" => "CPU time by process or thread",
        "cpu.profile" => "Where CPU time was spent",
        "mem.rss" => "Process memory held in RAM",
        "mem.faults" => "Memory page faults",
        "mem.pressure" => "Time stalled by memory pressure",
        "io.bytes" => "Bytes read and written by disks",
        "io.latency" => "Block I/O request duration",
        "io.syscall" => "File reads and writes by target",
        "io.fs" => "Activity associated with files",
        "net.bytes" => "Traffic by network interface",
        "net.socket" => "Target socket traffic and duration",
        "net.tcp.retransmit" => "TCP segments sent again",
        "net.request" => "Declared application requests",
        "gpu.engine_busy" => "GPU engine utilization",
        "gpu.mem" => "GPU memory held",
        "gpu.kernel" => "GPU kernel execution time",
        "power.energy" => "Hardware energy consumption",
        "sched.offcpu" => "Time threads spent off CPU",
        "sched.runqlat" => "Time waiting for CPU scheduling",
        "sched.futex" => "Futex synchronization activity",
        "sched.switches" => "Thread context switches",
        "sched.wakeup" => "Who woke a thread",
        _ => return None,
    })
}
