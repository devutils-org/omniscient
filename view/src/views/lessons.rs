//! Short, task-oriented lessons for the recordings used by the TUI demos.
//! Each lesson states an observation, a limitation, and a follow-up.

pub struct Lesson {
    pub observe: &'static str,
    pub beware: &'static str,
    pub next: &'static str,
}

pub fn source(name: &str) -> Option<Lesson> {
    let (observe, beware, next) = match name {
        "cpu.util" => ("Compare CPU use by state and over time. A sustained busy period can help locate a window to investigate.", "Machine utilization includes other processes. A high percentage alone does not identify which work matters to your target.", "Select the target in system, inspect its CPU time, then use flame to see where sampled CPU time goes."),
        "cpu.oncpu_time" => ("Compare CPU time across tasks and intervals. A hot thread can be a useful starting point for a profile.", "CPU time measures running, not elapsed time. A thread that waits for I/O or a lock can have little CPU time and still be slow.", "Compare with timebar or sched.runqlat, then inspect cpu.profile for the busy thread."),
        "cpu.profile" => ("Look for wide stacks and functions with high own cost. Compare the same workload before and after a change.", "Samples estimate where CPU time went; they are not exact function calls. Missing or truncated stacks can hide callers.", "Zoom into the hot path in flame, inspect callers, and validate changes with repeated equivalent runs."),
        "mem.rss" => ("Follow resident memory over the run and compare the current value with the peak and workload phases.", "RSS includes shared resident pages; summing process RSS can double count shared memory. A high RSS alone does not prove a leak.", "Compare repeated phases and allocation or live-heap data when collected; check memory pressure before judging impact."),
        "mem.faults" => ("Look for fault bursts around allocation, startup or file access and compare them with the affected tasks.", "A page fault is not necessarily a disk access. Minor and major faults have different implications; counts alone do not give bytes or latency.", "Check whether faults are split by type and compare the same window with I/O and memory-pressure data."),
        "mem.pressure" => ("Identify intervals when tasks stalled due to memory pressure, and relate those intervals to workload latency.", "Pressure is not the percentage of RAM used and is not automatically attributable to one process.", "Compare RSS, reclaim or swap activity where collected, and check which target was active during the stall."),
        "io.bytes" => ("Compare read and write activity per block device during the period of interest.", "Device traffic is machine-wide and can include other processes, cache effects and background writeback.", "Compare the target's io.syscall activity and io.latency, then use flows to inspect differences between layers."),
        "io.latency" => ("Compare the distribution of block request durations; a long tail may matter even when the average is low.", "These are block-request durations, not automatically application request latency. Sampling, losses and attribution affect what was observed.", "Use heatmap to locate slow periods and compare with queue activity and the target's I/O syscalls."),
        "io.syscall" => ("Inspect file operations and the recorded attribute to locate active threads, file descriptors and slow calls.", "A syscall's recorded value may be a duration or a byte count. Cached I/O and device I/O are different layers.", "Compare the selected operation and unit with io.bytes and io.latency; inspect flows for the target's file activity."),
        "io.fs" => ("Use file-associated events to find which files or operations contributed to observed file activity.", "File attribution depends on the backend and recording. Missing file events do not establish that no I/O happened.", "Compare with io.syscall and the underlying device in flows when those sources are available."),
        "net.bytes" => ("Compare incoming and outgoing traffic per interface over the interval of interest.", "Interface totals include the whole machine; loopback and network layers can count traffic more than once.", "Compare with the target's net.socket data in flows, keeping interface and socket scopes separate."),
        "net.socket" => ("Find socket operations, transferred bytes or durations associated with the target, depending on the recorded attribute.", "A socket-operation duration is not necessarily network transit time or request latency. Inspect the unit and operation.", "Compare a slow period with request, net.bytes and TCP retransmissions where those measurements exist."),
        "net.tcp.retransmit" => ("Look for retransmissions in the same time window as observed network problems.", "The count is machine-wide in this view and is not enough to attribute a problem to the target or identify packet loss as its cause.", "Check socket and request behavior, traffic volume and the relevant interface before drawing conclusions."),
        "net.request" => ("Compare declared requests by duration and associated evidence; investigate outliers rather than only the average.", "Only instrumented requests are represented. Missing declarations and lost events limit the comparison.", "Select a request and follow it into timebar or lanes to compare its waits and active threads."),
        "gpu.engine_busy" => ("Look for sustained busy and idle periods in the GPU engine measurements.", "This device-wide sampled value does not identify a specific process or kernel, and a long sampling window can hide short bursts.", "Compare with target kernel activity and GPU memory, accounting for the driver's sampling window."),
        "gpu.mem" => ("Compare held GPU memory across the run and identify the scope of the selected source.", "Device-wide and target memory are different; reserved memory is not necessarily actively used by a kernel.", "Compare with kernel activity and the workload's allocation phases; verify which scope was collected."),
        "gpu.kernel" => ("Compare kernel durations and their timing across streams to identify costly or idle phases.", "GPU duration is not the same as CPU time or end-to-end latency; asynchronous launches can overlap other work.", "Open lanes to compare kernels with host-thread activity and check sampling and timestamp quality."),
        "power.energy" => ("Compare energy and power over equivalent windows or workloads; energy is accumulated work, power is its rate.", "Hardware domains are not automatically per-process, and changing workload duration changes total energy.", "Compare runs with similar work and duration, and inspect CPU or GPU activity before attributing a difference."),
        "sched.offcpu" => ("Identify threads spending substantial time away from the CPU and inspect their recorded states and stacks.", "Off-CPU time combines different reasons, including sleep and blocking; it is not automatically a bottleneck.", "Use timebar to compare states and lanes or wake to investigate timing and wake-up relationships."),
        "sched.runqlat" => ("Look for runnable threads spending time in the run queue, especially during a latency increase.", "A runnable thread is ready to run; this delay is different from sleeping or waiting on I/O. One spike does not establish CPU saturation.", "Compare the same window with CPU utilization and other runnable threads, then inspect lanes and the workload's CPU limits."),
        "sched.futex" => ("Look for futex waits that overlap slow threads or request phases.", "Futexes support synchronization, but a futex event alone does not identify the lock, its owner or prove contention.", "Compare timebar and off-CPU stacks; inspect wake relationships or application-level lock data where available."),
        "sched.switches" => ("Compare context-switch activity across threads and time windows.", "A high switch count can be normal for the workload; a switch alone does not tell you why a thread stopped.", "Inspect lanes with off-CPU and wake-up data to understand the surrounding waits and handoffs."),
        "sched.wakeup" => ("Follow observed wake-up relationships and look for repeated handoffs involving the slow thread.", "Being awakened means runnable, not necessarily running immediately; some wakers or events may be outside the recording.", "Compare wake chains with run-queue delay and lanes before attributing a delay to a particular waker."),
        _ => return None,
    };
    Some(Lesson {
        observe,
        beware,
        next,
    })
}

pub fn view(name: &str) -> Option<Lesson> {
    let (observe, beware, next) = match name {
        "overview" => ("Start with the family whose activity coincides with the question you are investigating.", "An empty family is not proof of zero activity: a source may be absent or refused.", "Open a family, select a source, and use its guide to check scope, units and quality."),
        "system" => ("Compare machine resources and process activity over the same window to locate a useful starting point.", "A machine-wide peak may belong to another process and does not prove target attribution.", "Select the target for its ledger, then follow the resource that changed into its detailed view."),
        "process" | "ledger" => ("Compare the target's resource lines and follow the one that changes during the slow phase.", "CPU, bytes, memory and energy have different units and scopes; a machine value is not automatically process cost.", "Open the resource detail and compare equivalent runs before deciding where to optimize."),
        "flame" => ("Width represents the recorded weight of a stack: samples on CPU or time off CPU. Look for wide paths and high own cost.", "Inclusive width includes descendants, while own cost is attributed at the leaf. A wide parent is not automatically the function to change.", "Zoom to inspect callers, compare similar runs and validate any change against the same workload."),
        "request" => ("Compare requests and focus on outliers or groups that share evidence, such as a wait or an I/O phase.", "The recording contains declared requests and observed evidence only. A missing request or state may be uninstrumented or lost.", "Select a request and investigate its timebar or lanes, then compare it with a faster sibling."),
        "timebar" => ("Compare the shares of running, runnable and blocked states for the thread or request under investigation.", "A bar summarizes durations; it does not reveal the order of states, and unknown portions must not be treated as zero.", "Follow the selected thread to lanes for timing, or inspect stacks and wakeups for the dominant wait."),
        "heatmap" => ("Look for latency bands, outliers and changes over time, not just a single average.", "A band shows a distribution of observations, not its cause; time resolution and missing events can alter the picture.", "Select a slow interval and compare the same window with resource activity and request or thread details."),
        "lanes" => ("Align threads and GPU streams on the same time axis to examine overlap, waits and handoffs.", "At wide zoom a cell summarizes an interval; parallel activity in a cell does not establish exact event order.", "Zoom around the interesting interval and follow the selected thread to timebar or wake."),
        "wake" => ("Trace observed wake-up relationships for a thread with a substantial wait.", "A wake-up makes a thread runnable, but it can still wait to run; unobserved wakers leave gaps.", "Compare the chain with run-queue latency and thread lanes over the same interval."),
        "flows" => ("Compare byte volumes at the process, file, socket, interface and device layers where recorded.", "The same logical bytes can be counted differently due to caching, loopback or background work; the diagram does not prove a direct mapping.", "Follow a discrepancy to the relevant file or socket, then compare with I/O or network latency."),
        "family-cpu" => ("Start with CPU time and utilization; profiles help locate where running time went.", "Machine utilization and target CPU time have different scopes.", "Compare the busy window with scheduling waits and open flame for the target."),
        "family-memory" => ("Track resident memory and faults alongside memory-pressure stalls.", "RSS is not private memory, and faults do not always mean disk I/O.", "Inspect allocation patterns and compare pressure with the latency window."),
        "family-disk" => ("Compare block traffic, request durations and target I/O operations.", "Device activity and process syscalls measure different layers.", "Use heatmap and flows to inspect a slow interval and its data path."),
        "family-network" => ("Compare target socket activity, network interface traffic and request behavior.", "Machine-wide traffic and retransmissions are not automatically attributable to the target.", "Inspect flows and compare request or socket timing with the same window."),
        "family-gpu" => ("Compare GPU engine activity, memory and target kernel durations.", "Device-level sampling and target kernel traces have different scopes and time resolution.", "Open lanes and compare kernel timing with host-thread waits."),
        "family-energy" => ("Compare energy or power for equivalent work and time windows.", "Hardware-domain measurements are not automatically per-process.", "Compare CPU and GPU activity alongside the energy change."),
        "family-time" => ("Separate running, runnable and off-CPU states when investigating elapsed time.", "A large wait category needs context; sleeping may be intentional.", "Use timebar, lanes and wake to inspect the thread and interval."),
        _ => return None,
    };
    Some(Lesson {
        observe,
        beware,
        next,
    })
}
