//! Concise reading guides for the interactive views.

/// Purpose, first steps, and limits of each view.
pub fn explanation(name: &str) -> (&'static str, &'static str, &'static str) {
    match name {
        "overview" => ("Start here to see the available resource families.", "Use hjkl or arrows to move through the family grid; gg and G go to the first and last family. Enter opens it, Tab changes view, and F returns here.", "An empty or refused measurement is not the same as zero activity."),
        "system" => ("See the machine's resource activity alongside its processes and measurement cost.", "Compare the resource panels over time, then select a process and press Enter for its ledger.", "Machine-wide values are not automatically attributable to one process."),
        "process" => ("See one process across CPU, memory, I/O, network, GPU, energy and time.", "Select a resource row and press Enter for its detailed analysis. The suspect describes the largest known time category.", "Energy can be machine-wide and some activity may be unavailable or unattributed."),
        "ledger" => ("Compare the resources consumed by a target, including the cost and quality of measurement.", "Select a resource and press Enter to inspect it. A comparison run lets you compare the same lines.", "Different resource units cannot be added together; unknown attribution is not zero."),
        "flame" => ("Explore where on-CPU samples and off-CPU time occur in call stacks.", "Width represents samples or blocked time; select a frame and zoom to explore its callers and callees.", "Samples are estimates, and incomplete stacks cannot reveal frames that were not captured."),
        "request" => ("Compare individual application requests and the evidence associated with their duration.", "Select a request to focus other views on it and investigate its threads or waits.", "The application must declare requests; this view cannot infer every request from arbitrary traffic."),
        "timebar" => ("Understand how a thread or request's elapsed time is divided among execution and waits.", "Select a bar to inspect its states and follow it to a request or thread.", "The bars sum durations; they do not show the order in which those states occurred."),
        "heatmap" => ("See how the distribution of measured latencies changes over time.", "Look for bands, spikes and phase changes. Select a column to inspect a slow observation.", "Time and latency resolution depend on the source; the chart does not itself establish a cause."),
        "lanes" => ("See activity and waits across threads or GPU streams on a shared time axis.", "Compare lanes at the same time, then select a thread or stream for more detail.", "At a wide zoom cells summarize intervals rather than individual instants."),
        "wake" => ("Explore which threads or interrupts woke another thread.", "Select a thread to follow its wake-up chain and move to its lane.", "Wake-up relationships do not by themselves prove exact blocked time or a complete causal chain."),
        "flows" => ("Follow observed I/O and network bytes across the target and machine layers.", "Compare sockets, files, interfaces and devices to see where measured volumes differ.", "Layers can count the same traffic differently, and not every connection is attributable to an interface."),
        "family-cpu" => family("CPU"), "family-memory" => family("memory"), "family-disk" => family("disk I/O"),
        "family-network" => family("network"), "family-gpu" => family("GPU"), "family-energy" => family("energy"), "family-time" => family("scheduling and time"),
        _ => ("Explore the current view.", "Use the footer and ? for available actions.", "Figures depend on the sources and scope of the recording."),
    }
}

fn family(name: &str) -> (&'static str, &'static str, &'static str) {
    let purpose = match name {
        "CPU" => "Explore CPU sources and their recorded measurements.",
        "memory" => "Explore memory sources and their recorded measurements.",
        "disk I/O" => "Explore disk I/O sources and their recorded measurements.",
        "network" => "Explore network sources and their recorded measurements.",
        "GPU" => "Explore GPU sources and their recorded measurements.",
        "energy" => "Explore energy sources and their recorded measurements.",
        _ => "Explore scheduling and time sources and their recorded measurements.",
    };
    (purpose, "Select a source with Up/Down. Its detail panel lists the available summary series; Page Up/Down scrolls the detail. Enter opens an advanced view.", "The table shows the first summary series; scope and unit vary by source. Blank or refused data is not zero.")
}
