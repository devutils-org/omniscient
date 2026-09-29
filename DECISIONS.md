# Omniscient — Decisions

Phase 0 of `ROADMAP.md`: the choices that constrain everything else. Each line is
either settled, with the reason, or open, with what it costs to leave it open.

A decision recorded here is not permanent, but changing one is expected to be
paid for across every collector written since. That is the point of the file.

---

## Settled

| Decision | Choice | Reason |
|---|---|---|
| Language | Rust | The correlator, not the collectors, is where the complexity lives: bounded open-state maps, a watermark reorder buffer, interval joins. Rust also gives CO-RE eBPF through libbpf-rs without a second language for the userspace side. |
| Clock | `CLOCK_MONOTONIC`, single domain | Non-negotiable per the roadmap. `perf_event_open` sets `use_clockid` + `clockid` explicitly; eBPF uses `bpf_ktime_get_ns()` only, never `bpf_ktime_get_boot_ns()`, which includes suspend time. Device domains (GPU, NIC) calibrate explicitly and carry a non-zero `ts_err_ns`. |
| Scope | Single machine | Inter-machine clock drift is a different class of problem from local correlation. Fleet scope is not refused, it is deferred. |
| Request surfaces | CLI and session file, one model | Both build the same `Request`. `--dump-request` prints a runnable session file, which is what keeps the two from drifting; an end-to-end test asserts the round trip. |
| Input formats | YAML, JSON, TOML | Inferred from the extension, forced with `--input-format`, and `-` reads standard input. One serde model behind all three, so the choice is about what writes the file, never about what can be expressed. |
| Output formats | text, json, yaml, toml, ndjson, folded, speedscope, pprof, chrome, every one with its encoder | Documents (a request, a listing) and results (a histogram, a profile) are different payloads. Result formats are declared with the emits they can carry. A format is declared before its encoder is written, so the surface is stable, and one in that state is reported as not yet implemented rather than silently approximated (`OutputFormat::status` in `src/format.rs`); none is today. |
| Catalog location | `catalog/*.yaml`, compiled into the binary | A build always carries a complete catalog. `--catalog DIR` and `OMN_CATALOG_DIR` load from disk for iteration. |
| Defaulting policy | Default only where the catalog leaves one option | If a source declares a single mode, a single emit for that mode, or a single target kind, omitting the flag picks it and the choice is printed. Where the catalog offers several, the flag is required. Choosing between two legitimate answers is how a tool reports one number while the user reads another. |

---

## Settled by the first vertical slice

`mem.rss` was taken from the command line to a rendered number, and a few things
that looked like details turned out to be decisions.

| Decision | Choice | Reason |
|---|---|---|
| Unprobed is not available | A probe answers available, missing, or **unknown**, and unknown never counts as available | The resolver skips a backend whose requirements it could not check when a fully verified one exists, and says so in the output when it cannot. Assuming what was not checked is how a tool reports a measurement it never took. |
| Restricted is not absent | Paths are reported as readable, present-but-restricted, or absent | tracefs is mounted on nearly every machine and readable on nearly none without privileges. "Absent" sends the reader looking for a kernel option instead of for `sudo`. |
| Backend ids are unique within a chain | Enforced when the catalog loads | Two backends called `fs:proc` cannot be told apart by `--backend force:`, and a collector cannot know which one it is serving. The chain is now `fs:proc:smaps_rollup` and `fs:proc:statm`. |
| Equivalent is not degraded | A non-first backend either states what it degrades or declares `equivalent: true` | The NVML path for a GPU and the netlink path for a counter lose nothing; forcing a degradation note onto them would be noise, and leaving the field empty would be a lie. |
| The value attribute is named by the collector | Not inferred from the unit | A source may carry several numbers. Guessing which one is the measurement is the kind of shortcut that silently aggregates the wrong column. |
| A dead target ends the run, it is not a lost read | ENOENT and ESRCH both mean gone | Counting the last read of an exiting process as a loss made every clean run look lossy, which would have taught everyone to ignore the loss line. |
| One workload per run | A session runs any number of requests in one window -- one workload beside any number of machine targets, each request with its own window inside the run since the tenth wave's hot attach. Two *workloads* are refused. | Running requests in sequence would measure several moments and present them as one, which is why they share a window. Two workloads are still refused because hot attach does not answer the three questions that have two answers each: whose exit ends the run, which program a launch means, and what an overhead is a percentage of. |
| `--no-resolve` | Writing a session down does not require this machine to be able to serve it | A session authored on a laptop for a server must not be blocked by the laptop's tracefs permissions. |

---

## Settled by the first correlation

Joining `mem.heap.alloc` to `mem.faults` by interval produced the first result
neither source could give alone, and settled a few more things on the way.

| Decision | Choice | Reason |
|---|---|---|
| A join is declared beside its sources | `joins:` in the session file, `{id, from, to, via, match, opens, closes, window, capacity}` | Running the sources without the rule gives two unrelated numbers and the rule without the sources gives nothing, so they are one measurement and belong in one file. |
| Only the interval match is implemented | `eq`, `scoped_eq` and `wakeup` are refused by name | Generalizing an engine against match kinds nothing has exercised makes it fit the imagined cases. The roadmap's own rule: generalize once there are several real joins. **Since honoured and superseded**: all four match kinds are implemented, and `wakeup` has a producer. `scoped_eq` still has none — it needs `blk:rq@open_ts`, which is what makes `io.latency` the collector phase 4 waits on. |
| Open and close are predicates, not flags | The filter grammar, reused: `opens: "op == 'alloc'"` | The same expression type the CLI parses, now also evaluated in userspace, which is what filter placement needs anyway. |
| Sources of one session share one window | `run_all` launches once and drives every collector on one tick loop | Running them one after another would measure several different moments and present them as one, and a join across them would correlate a program with its own past. |
| A run is one document | `{results: [...], joins: [...]}` | Concatenating results and a correlation produced a file no JSON parser accepts. The shape was wrong, not the formatter. |
| Eviction is data, not housekeeping | Window, capacity, and counts for expired and overflowed states | An attribution missing because its range was evicted looks exactly like an attribution that never happened. |

### What the measurement taught

The first correlation reported ~515 faults for an 8 MiB allocation, where 4 KiB
pages would need 2048. The kernel's own counter agreed with ours, and a sweep
showed the fault count nearly flat across sizes — 263 faults for 1 MiB, 528 for
32 MiB, from 4 KiB per fault up to 62 KiB. The collector is faithful; the metric
is not what it looks like.

The explanation first written here, multi-size transparent huge pages, was a
hypothesis nobody measured, and it is wrong on the development machine: every
multi-size size is `never` there. What predicts the counts -- twenty of
twenty-one runs at seven sizes exactly, the last off by the interpreter's first
fault -- is THP `always` giving one fault per aligned 2 MiB page a mapping
wholly covers plus one per 4 KiB page outside them. So a count depends on how a
range is backed and on where it lands: the same 8 MiB mapping costs 515 faults,
or 4 when it lands on four aligned huge pages, which it did on five runs of
twelve. The table is in `docs/scenarios/cross-memory.md`; the THP mode is in the
environment capture for exactly this reason.

A derived "resident bytes" column had already been written from `faults x page
size`. It was wrong by a factor of four on this machine, and it is gone. Fault
counts now say where faults happened and explicitly do not convert to bytes.
This is the failure mode the project exists to prevent, found in its own output.

---

## Settled before the eBPF layer

The two lines that blocked the eBPF layer, and one the layer's first day forced.
They are what `sched` — and, through it, 62 of the catalog's 155 backends — are
written against.

| Decision | Choice | Reason |
|---|---|---|
| Kernel floor | 6.1 | `lock:contention_*` exists from 6.1, and requiring it removes both the futex + uprobe fallback for `sched.lock` and the untested no-BTF path, where CO-RE relocations would have to degrade into something nobody exercises. A floor is a claim the project can keep: 5.15 would mean shipping two shapes of half the chains to cover one LTS that leaves support before this tool has a second family of eBPF backends. Below 6.1 the affected sources are refused by name, with the version, never quietly degraded. |
| Privileges | `CAP_BPF` + `CAP_PERFMON` | The eBPF backends need those two capabilities; root also works, because root has them. An unprivileged run is refused with the capability that is missing, never resolved onto a lesser backend that answers a different question. This keeps one chain shape to reason about and to test, which is the cost the open line was weighing: a second, unprivileged shape doubles the resolver's test matrix while making every result depend on who ran it. |
| Attach through raw tracepoints first | `BPF_RAW_TRACEPOINT_OPEN`, with `bpf:tp` behind it | A classic tracepoint program is attached by reading the numeric event id from tracefs, which is mounted and unreadable on a normal machine — including the development one, where `/sys/kernel/tracing/events/sched` is `Permission denied` at uid 1000. A raw tracepoint needs only BTF and the event name, so `bpf:raw_tp` has strictly fewer requirements than `bpf:tp` and is ordered ahead of it. The two see the same events; the difference is what must be readable to attach, so the fallback declares `equivalent: true`. |

---

## Settled by the first device family

`gpu` is the first family whose sources belong to a device rather than to a
process, and the first whose only interface is a vendor library. Both forced
decisions that outlive it.

| Decision | Choice | Reason |
|---|---|---|
| A collector is bound to a target, not to a pid | `Prepared::attach` takes the `Target` whole | Every collector was handed a process whether or not its source had one. A GPU clock, a RAPL domain and an interface counter have none, and a sentinel pid standing in for one is how a device figure ends up presented as a program's. A collector that needs a process now refuses by name. Reached twice, independently, by the `gpu` and `net` threads. |
| The target decides the scope, and the event records it | `scope` attribute on every event | `gpu.mem` reports a process's VRAM under a process target and the device's under a system target. They are both bytes of VRAM and mean different things, so they never share a series, and the file says which one it holds without a reader who remembers the command line. |
| A launch target sets the window, not the scope | Stated as a caveat on every device-scoped source | `omn gpu.temp -- ./prog` measures the device while `prog` ran, alongside everything else using it. Useful, and one step from being read as attribution. |
| A filter runs where the resolver said it would | Applied in `run_all` for any backend not credited with pushing it down | It was parsed, validated, given a placement in the resolved file, and never applied: `--filter "device == 99"` kept every event. A predicate that is reported and not executed is the worst shape a bug takes here, because the numbers look like an answer to the question that was asked. |
| What a filter removed is reported | `kept`, `dropped`, and the fields no event carried | Removed events are not losses, but they were collected and paid for, and a series holding five of three hundred looks exactly like a series of five. Naming the fields nothing produced turns an empty series into a diagnosis -- it immediately found `mem.rss` declaring a `comm` filter field its collector never emits. |
| A family answers its own requirement classes | `collect::<family>::probe(requirement) -> Option<Probe>` | A probe for `nvml` or `hwmon:amdgpu` in `capabilities.rs` is a probe far from the code that knows what it means. Keeping it with the family means a family cannot be added without the means to check what it needs. |
| Energy is a counter, never an integral of sampled power | `gpu.power` reads the device's energy counter and refuses a device that has none | Integrating NVML's sampled draw overstates energy by about 5 % at 1 Hz and a third at 20 Hz on the development GPU: the driver returns an already-filtered value, so sampling faster resamples the same number. Polling more made the integral worse, which is the opposite of what an integral should do. |
| A vendor library is called, not a vendor tool | `dlopen` of `libnvidia-ml.so`, never `nvidia-smi` | Same rule as "do not wrap `perf`". Parsing a tool's output is a dependency on a text format nobody promised to keep. `nvidia-smi` is used in one test, as an independent second reader. |

---

## Settled about the edges of a run

Three conventions that were each decided twice, differently, in different parts
of the tool -- which is how a reader comparing two families got a different
answer from each.

| Decision | Choice | Reason |
|---|---|---|
| The runner closes the window | The last round of `sample` calls is the closing read; `Collector::stop` returns what is already buffered and does not read again | `run_all` reads every collector and *then* asks whether the run is over, so the last round already lands at the end of the window. `net/dev.rs` reads once more in `stop` on the grounds that the last interval would otherwise be dropped, and it is not: measured, what is left for that read to cover is tens of microseconds of a 100 ms tick, and the point it produces sits at the same offset as the previous one with a delta of zero. `io` and `power` refuse to read there for exactly that reason. A `count` window is the exception -- it has no series, and its single measurement *is* the closing read. `the_runner_closes_the_window` measures the leftover rather than asserting the rule. |
| A throttled period is not a dropped event | `Losses::throttled`, counted apart from `Losses::dropped` | A drop count is a number of events the ring reported losing. A throttle is the kernel declining to deliver for a while, and how many events happened inside that while is not recorded anywhere. `sched.switches` added one to `dropped` per throttle, which states a count nobody has; `mem.faults` ignored throttling entirely, so a throttled run read as complete. Counted apart, a run that hit `kernel.perf_event_max_sample_rate` says it cannot tell how much it lost, which is the true answer. |
| "Valid, and not built" is exit 3, wherever it is discovered | `NoCollector` and `UnsupportedTarget` both exit 3 | The two exits mean different things to a caller: exit 1 is a problem with this run, and retrying or raising privileges may help; exit 3 is a problem with this build, and neither will. `mem.heap.alloc --pid` is a request the catalog declares -- a uprobe serves it -- and that this build cannot answer, and it exited 1, indistinguishable from a counter that failed to open. The same refusal is also now asked *before* "pass --duration", because advice for a request that cannot work is a dead end. |

---

## Settled in the ninth wave: the run model

The line that stood longest in the open section, and the one every collector
written since was assuming an answer to.

| Decision | Choice | Reason |
|---|---|---|
| Usage target | **A session.** Hot attach lives inside one; the flight recorder is a *mode* of one; there is no resident daemon. | The open line bundled three things and only one of them needs a daemon. |

**Hot attach does not need a daemon.** The open line said a daemon "must support
hot attach and detach, which phase 6 already assumes", and that is where the
decision was stuck. But phase 6's exit criterion is *a collector can be added
and removed mid-session*, and phase 10's escalation is *enable that layer's
detailed collectors and turn off the rest* -- both inside one run. So hot attach
is a runner task, dispatchable now, and it is what the concurrent-requests
deferral and the escalation were both waiting for.

**The flight recorder needs a run with no fixed end, not a process with no
end.** A session whose duration is *until a predicate fires*, whose buffers are
the bounded rings and maps it already has, and whose output is one document per
trigger keeps every invariant this project holds: one document covers one
window -- the span the retention held when the trigger fired -- eviction is
counted and surfaced as a loss the way the join engine's expiry already is, and
there is one clock. What it adds is a trigger and a retention bound, not a new
kind of program.

**A daemon is a second product.** A process lifecycle, an IPC surface, and a
privilege boundary other than the binary's own file capabilities -- which this
file settled on as the one privilege shape, and which finding 40 declined to
double for exactly this reason. None of the three is measurement, and each would
be paid for by every collector.

**What it commits to, and what it rules out.** Phase 6's fourth task -- hot
attach and detach inside a session -- is the next piece of run-model work, and a
flight-recorder mode (`duration: until <predicate>`, a retention bound, one
document per trigger) follows it. A resident agent is ruled out for this tool.
The cost the open line named -- *every collector written in the meantime
assumes the window* -- stops being a cost: the window stays the unit, and a
triggered snapshot has one too.

Reversible, like every line here, at the price the preamble names: paid across
every collector written since.

---

## Settled in the tenth wave: `CAP_NET_RAW` is opt-in

| Decision | Choice | Reason |
|---|---|---|
| `CAP_NET_RAW` for `net.capture` | **Never granted by `make`; `make CAPTURE=1` grants it with the other two.** The capture's refusal names that command. | It is the only capability here that acts rather than observes. |

The scenario wave left this open on purpose, and the argument that settles it
is not the one it was left open for. It was left open because the capability
lets the binary read every frame on the machine -- but `CAP_BPF` and
`CAP_PERFMON` already let a tracing program read kernel memory, packets
included, so reading is not what `CAP_NET_RAW` adds. What it adds is **sending**:
a raw socket can put any frame on any interface, spoofed or not. Boundary 5
says this tool observes and does not act on the machine, and a capability whose
distinctive power is acting has no place in the default grant, however narrow
the one source that wants it.

It is also the only capability that one source alone needs. Every eBPF and
per-cpu perf backend shares the other two, which is why `make` grants them
unasked; `net.capture` is one source out of a hundred, and a person who wants it
can say so once per build. So the capability is opt-in rather than refused: the
refusal already names it, and `make CAPTURE=1` is what grants it. Like the other
two it does not survive a relink, and like them it is re-applied by the build
that asked for it. A sudoers line for it is its own line, because it is its own
command.

Reversible at the price of one Makefile variable. What would reverse it is a
capture path that does not need a raw socket at all -- a BPF program copying
frames into a ring from a tracepoint -- which would move `net.capture` under
the two capabilities already granted.

---

## Settled in the tenth wave: no dynamic binary instrumentation backend

| Decision | Choice | Reason |
|---|---|---|
| Dynamic binary instrumentation | **Never a backend.** If it ever earns a place it is as an import of another tool's output, the way LTTng-UST would be, never as a mode of a session. | Under a DBI engine the program is not running on this machine and not on this clock, which are the two things every other line in this file assumes. |

It was left open because the tree held a contradiction: `Cost::Extreme` was
documented as *"replay or dynamic binary instrumentation"* while the accepted
boundaries put DBI outside the kernel-level core. The contradiction resolves
without deleting anything, because the band never needed DBI to exist: the two
backends that declare it, `gpu.counters`' `vendor:ncu` and `vendor:rocprof`,
are **kernel replay** -- the GPU profiler runs each kernel several times to
read more counters than the hardware has. So the band stays, its comment names
replay only, and the boundary stands.

What decided the rest is the reasoning the open line already gave, taken in
order of what it would cost:

- **The clock.** Under DBI wall time dilates twenty- to a hundred-fold. That is
  not an uncertain timestamp, which `ts_err_ns` could carry; it is a different
  time base, and every temporal join, every wakeup chain and every
  decomposition means nothing across it. Phase 1's one non-negotiable is one
  clock domain.
- **The machine.** Its counters, caches and timings are the simulator's. A
  session that mixed a DBI source with any other would be measuring two
  machines at once, and the request model would need a whole-session mode to
  refuse that -- a shape it does not have and should not grow for one backend.
- **Determinism.** A cachegrind run is exact about a cache that is not this
  machine's. The `Determinism` enum says "never `exact`" on purpose.

Someone who wants what Valgrind or DynamoRIO measures should run Valgrind or
DynamoRIO; this tool's thesis is the real machine. Reversible, like every line
here, by writing the import -- which touches no collector.

---

## Settled in the tenth wave: triage asks for its objective

| Decision | Choice | Reason |
|---|---|---|
| A triage request without an objective | **Refused with the list, never defaulted.** | "What is this limited by" has no answer until someone says *for what*: median latency, the tail, throughput and energy do not name the same culprit. A layer that picked one silently would report a confident wrong suspect, which is the failure this project is built against, one level up. |
| A method asked an objective it does not answer | **Refused, naming the method that does.** `omn latency` answers latency and tail; `omn saturate` answers throughput, tail and energy. | A method that answered anyway would be answering a different question under the one asked. |
| `cost` as an objective | **Not one.** | Nothing here measures a price, and an objective nothing measures would be a label on a guess. |

Enforced by `src/triage/objective.rs` and its tests, not by this file.

---

## Settled in the eleventh wave: how a run escalates

| Decision | Choice | Reason |
|---|---|---|
| What an escalation may attach | **Only a request the session declared**, held with a window `on <layer>`, validated, resolved and prepared before the launch. The run decides *when*, never *what*. | An escalation can then never reach a backend the resolver did not choose, or a source answering a question nobody asked -- ruled out by construction rather than by a check. |
| Who decides | **The runner asks a trait between rounds** (`collect::Escalate`: decide, shed, close), after the round's reads, and applies detaches before attaches; `src/triage/` answers. | The collector layer stays ignorant of triage, the way it is of the catalog behind `Cheaper`; and detaching first means a run never pays for two detailed layers at once. |
| The surface | **A session's `escalate:` block**, not a subcommand. | The session is the record of what was asked, and `-o`, `--budget`, `cost.json` and the resolved file come with `omn -f` for free. |
| When a layer is escalated to | **The same state of the main thread named in two readings in a row** (200 ms each), a state being named at more than half of a reading and more than any other. | A reading that straddles a phase change must not attach a layer for the phase that just ended. |
| One window per request | **A layer let go is not attached again.** | A collection has one window; a request that comes and goes is the flight recorder's (settled below, in the twelfth wave). |

`docs/triage.md`, "The escalation", has the measurements: a program cpu-bound
then lock-bound gets the lock layer 0.4 to 0.5 s after the switch, and its
waits land within what the program timed itself.


---

## Settled in the eleventh wave: what the engine, the drains and the pin answer

| Decision | Choice | Reason |
|---|---|---|
| A key a source never produced | **A zero only from a source that watched the whole run and lost nothing**; `not read`, with the reason, from one that lost events, was narrowed by a window, or cannot say when it watched. A filtered part the filter emptied is a zero with its note. | A source that looked and found nothing has measured a zero; one that may have lost that very key has not, and unknown never counts as available. The old answer read `0` with no note either way. |
| How a drained program hands its rows over | **Two row maps behind a one-slot map-of-maps, a swap, and an explicit grace period** (`membarrier(MEMBARRIER_CMD_GLOBAL)`, 5 to 18 ms here) before the retired map is emptied; a kernel that withdraws the command is refused at load. | The kernel's own map-of-maps update returned in 20 to 30 us, which is not every running program, and raw-tracepoint programs are preemptible on this kernel: a sleep is a bet and the grace period is not. |
| A pinned backend with no collector in this build | **The build's answer (exit 3)**, even when that backend's probes are missing on this machine too. A pinned backend whose probes are missing and whose collector exists is refused by the resolver before the launch (exit 1). | The build's gap holds on every machine; reporting the machine's instead sends a reader after privileges for a collector nobody wrote. |
| How portability is checked | **Prebuilt kernels booted in KVM** (virtme-ng, the host's root filesystem), each configuration made inside the guest (no capabilities, root, BTF hidden, tracefs hidden), every source listed *and run*. Known defects are recorded in `tests/portability.rs` and fail when they stop matching. | Building kernels costs disk and hours this machine does not have; hiding a file reproduces the configuration exactly. A defect list that must stay true cannot drift into a list of excuses. |

`docs/families/join.md`, `docs/families/drains.md`, `docs/fixes/silent.md` and
`docs/portability.md` have the measurements behind each line.

---

## Settled in the twelfth wave: what a model is handed

| Decision | Choice | Reason |
|---|---|---|
| Who answers a machine question asked through MCP | **The `omn` binary that would run the session.** `omn-mcp` probes nothing and holds no capability; every machine-dependent answer names the binary and the capabilities on it. Validation, which is machine-independent, runs on the library. | A probe answers for the process that asks. A copy of `omn` without its capabilities answered 52 usable sources against 93, silently -- the relink failure, one binary over. |
| Whether a model may run a session | **Only when the person who started the server passed `--allow-run DURATION`**, for at most that duration, with a required budget, writing only under the server's directory -- and **never a session that launches a program**. *Amended in the thirteenth wave: a launching session the person approved, below.* | Asking for a measurement is what phase 11 grants; running a program a model chose is changing the machine (boundary 5). The opt-in has `make CAPTURE=1`'s shape: the power closest to acting is granted by a person, when asked. |
| What an answer is | **The documents as written, regrouped per request**, with each request's losses, shortfalls, degradations, unverified requirements and timestamp error restated first, and two additions only: `evidence_means` beside every evidence level, and `elided` where raw events were cut, counted in `missing`. | Every field a shaped answer leaves out is a place a shortfall falls through; and `IDENTITY` and `TEMPORAL` are one word each, so what each establishes is written beside it. |

`docs/mcp.md` has the protocol, the checks (1346 numbers matched key by key
against the files, a lossy run's shortfall carried unchanged) and the refusals.

---

## Settled in the twelfth wave: the flight recorder

| Decision | Choice | Reason |
|---|---|---|
| The surface | **A session's `record:` block** (`--until` on the command line), beside `duration:`, not `duration: until <predicate>`. | The session is the record of what was asked, and a recording has four more things to say than a duration can hold. `duration` stays what it was -- the longest the run lasts -- which is the bound a recording needs anyway. |
| The bounds | **`retain`, `documents` and `duration` are required, refused when absent; `after` defaults to zero.** | A run with no fixed end is bounded only by what it is told. `after` is not a bound: zero is "the document ends at the round that fired". Nothing is defaulted, so the resolved file has nothing to confess. |
| The predicate | **The filter grammar, over the events one round of the `on` request read, after the request's own filter**, with `value` naming the collector's measured attribute. Fields: `value`, `kind`, the backend's `filter_fields`. | The vocabulary that exists; `value` is rewritten to the collector's attribute in the runner (`read_value_as`), so `src/filter.rs` needed nothing. |
| When it fires | **On the round its predicate becomes true**: a round with a match after one without. A match while a document is open is counted in it (`fired_again`) and writes nothing. | A level held across many rounds is one event of interest. Re-arming on any non-matching round, empty ones included, treats a polled level and a stream of discrete events alike. |
| The window | **`[fire - retain, cut]`, `cut` the first round at or after `fire + after`**, never before the run's open. Windows of two documents may overlap; each is what the retention held. | "The span the retention held when it fired", plus the after-span a burst needs to be in the document whole. The backward reach is exact -- `fired_ns - opened_ns == retain` is asserted. |
| Eviction | **By event timestamp, in the runner's buffers, at most every `retain/16`** (a document is cut from the exact window whatever this is); counted per request, in events and rounds, **only for what no document held**. | A count nobody is shown is a silent loss. An event evicted after a document held it is not a loss. |
| Eviction as a loss | **`Losses::evicted` and `Shortfall::evicted`**, on each document's loss line and beside its distributions -- **without the `>=` mark** (`Shortfall::bounds_below`). | What was evicted is older than the window; every event inside the window is there, so the window's numbers are not lower bounds. They are the window's and not the run's, and the shortfall sentence says exactly that. |
| A document's losses | **The difference since the previous document's cut** (`Losses::since`), plus what was evicted since it. | A ring's drops cannot be placed in time; the delta since the last document is an upper bound for this one's window and is never counted twice. |
| The budget | **Weighed over a rolling span, the longer of `retain` and 2 s**, from the latest budget check at or before its start (`Spending::baseline`). | A share of a run that idled an hour is a share of that hour: the burst after it would be under any ceiling. Measured below. |
| Delivery | **A sink the runner calls as each document is cut**; `main` encodes and writes it at once. | A flight recorder's documents are wanted precisely when the run does not end cleanly. |
| What is not allowed beside it | escalation, windows, decompositions, `count` and `snapshot` modes. | Each would make a document's span disagree with a part of it. |

`docs/recorder.md` has the ground truth: a program that bursts 300 descriptors
twice gets exactly two documents, each window holding its burst, with the
retention bound exact to the nanosecond and the evictions counted against what
the program says it did before them.

---

## Settled in the twelfth wave: who a kernel event belongs to, and what the resolver promises

| Decision | Choice | Reason |
|---|---|---|
| A wakeup issued in interrupt context | **Its waker is the interrupt** (tid 0 and a `waker_context`), and a target keeps it only as its wakee. The served-softirq bit decides, not the whole softirq byte. | The task an interrupt lands on did not wake anyone: crediting it put 1954 wakeups of an interrupt load on a pinned driver, none after. Bottom halves disabled is still the task's own context. |
| A `sched` source on a process target | **Observes the process, threads included, not the processes it starts**; `sched.switches` counts its children's samples and leaves them out, like the other `sched` programs. | What lets their events be joined per task; following the whole tree broke `cross_compute`'s join. |
| A two-map drain | **Waits for a grace period before it reads the map it retired**; none relies on an interval or a sleep, `fentry`'s histograms included. | The eleventh wave measured that neither the kernel's swap nor a sleep is one here. |
| A backend's modes | **A backend may declare the modes it serves**, replacing the entry's for that backend; the loader checks a non-empty subset that leaves no entry mode unserved, and the resolver passes over a backend whose modes leave the request's out, with the reason in the chain. | A mode is a property of a mechanism -- a file holding a total cannot trace -- so it is catalog data, not a collector's refusal after the launch (P7). |
| Where a predicate runs | **`kernel` only for what a program compares**: `filter_in_program` per backend, and a predicate every part of which is a one-value `==` over those fields. Everything applied after delivery is `userspace`, and the resolved file says who applied it (`applied_by`). | `kernel` is a promise about cost, and every `bpf` backend used to be credited with it. |
| A system or core target on a perf backend | **Probed per cpu by the resolver**, as a requirement of its own in the resolved file; the machine is sampled with one event per cpu, not inherited, and the idle task's samples are counted and left out. | The listing's per-task probe cannot answer for a per-cpu event (P9), and a sampler that follows no process could not serve the machine at all (P8). |

`docs/families/kernel.md` and `docs/fixes/resolver.md` have the measurements.

---

## Settled in the twelfth wave: what a probe asks

| Decision | Choice | Reason |
|---|---|---|
| A tracepoint's prototype | **Written in the catalog as well as in the program**, and the two change in the same commit. | The probe has to answer before anything is loaded, so it cannot ask the program; a mismatch shows at once on the development machine (the source reads missing), and a test catches a malformed one. Three of the matrix's classes passed on 7.2 and failed on 6.6 for want of it (P1). |
| `uprobe` and `uprobe_multi` | **`uprobe` means this process can place one**, not that the PMU exists; **`uprobe_multi` is asked of the kernel** with a link to a file that does not exist, not read off the version. Without the capabilities it cannot be asked, and reads unknown, never available. | A PMU that exists and an open that returns `EACCES` are different answers (P3, P5). |
| A GPU library | **The probe initialises it** and counts devices: 3 to 5 ms, once per process. | Loading `libnvidia-ml.so` is not the question the collector's first call asks; a library without its driver loaded and then refused (P6). |
| A key a kernel may not write | **A requirement of its own**, `cgroup2:<file>:<key>`, read from the nearest cgroup that has the file. | Which keys a kernel writes does not depend on the cgroup; `mem.swap` read available on kernels that write no `pswpout` (P10). |
| A guest's single root cgroup | **Not a probe defect** (`mem.cgroup`). | The probe cannot know the target's cgroup before the launch, and the refusal after it is exact. |

`docs/portability.md`, "The twelfth wave", says which class was verified on a
matrix cell and which needs the matrix run again on an idle machine
(`power.cstate`, and the capability-less cells swept before W's and D's fixes).
---

## Settled in the thirteenth wave: what a model may launch, and what the engine pairs

| Decision | Choice | Reason |
|---|---|---|
| Whether a model may run a session that launches a program | **Only a session file in the directory the person starting `omn-mcp` passed with `--allow-launch DIR`**, which needs `--allow-run` beside it. The file is run as it states it, from its own directory: program, arguments, environment and working directory are the file's, and nothing in a tool call changes them. `list_approved` lists them with their plans; a name that leaves the directory is refused. | The model picks which approved session runs and when, never what runs, so boundary 5's reason holds: no code a model chose runs on the machine. It is what lets an agent benchmark a program its owner described, which the user asked for. |
| Benchmarking through MCP | **Repeated `run_session` calls**, not a tool of its own. | The server hands over documents and does not summarise them, and the bench's variance gate needs a quiet machine and a release binary, which a model's call cannot promise. |
| Two lives of a block request under one scope | **Paired by order and rated `TEMPORAL` within the key**, counted in `states.scope_collapsed`; the scope is not made unique. | No component both block collectors read changes per request, and order is exact while neither side loses an event. |
| A decomposition part read from a join | **Allowed, and only from a `woke` join** (`value: paired`). | It is the one match whose pairs have a distance that is a duration. |
| Symbol names | **Demangled by `rustc-demangle` (legacy and v0) and `cpp_demangle` (Itanium)**; a name neither reads whole is shown as the linker wrote it, and `Frame::mangled` keeps it. | Both schemes are specified in full, with corner cases a partial reader gets wrong and confidently. `rustc-demangle` is the Rust project's own and has no dependencies; `cpp_demangle` depends on `cfg-if` only; `std`, `backtrace` and `pprof` use them. |

`docs/families/join.md`, "The thirteenth wave", has the ground truth for each,
and `docs/mcp.md` the launch rules and their refusals.

---

## Open

Nothing is open. The last line, dynamic binary instrumentation, was settled in
the tenth wave, below. A new open line goes here with what it costs to leave it
open, which is the habit that kept this section honest.

---

## Accepted boundaries

Restated from `ROADMAP.md` because they are decisions, not omissions.

1. **No bit-exact determinism.** PMU counters are stable (typical CV < 0.1 %) but
   never exact: skid, speculation, IRQs, SMT. Strict determinism is dynamic
   binary translation, outside the kernel-level core.
2. **No inside view of NVIDIA GPUs.** CUPTI is closed and there is no kernel
   path. AMD and Intel are covered through the DRM scheduler and kernel PMUs.
3. **Writes are less attributable than reads.** Writeback leaves in a kworker and
   the originating context is lost: `KEY` evidence, not `IDENTITY`.
4. **No wrapping of `perf`, `bpftrace` or `bcc`.** Omniscient talks to
   `perf_event_open`, `bpf()`, procfs, sysfs and netlink directly.
5. **It observes; it does not tune.** Every knob the kernel offers a performance
   engineer -- `madvise`, `sched_setaffinity`, `posix_fadvise`, `io_uring`
   parameters, a queueing discipline, `nr_requests`, a cgroup limit -- is out of
   scope, and so is anything that modifies the target to measure it: `ptrace`
   and `userfaultfd` are interception rather than observation. The reason is not
   modesty, it is that a tool which measures and changes in one run produces
   measurements nobody can interpret: the second reading is of a machine the
   first reading altered. Omniscient will report that readahead is not being
   used; the person decides what to do about it. This is stated because "why
   does it not set X" is otherwise a perceived bug rather than a boundary, and
   because it is half of what "performance engineering" usually means.
6. **LTTng, and tools like it, are peers rather than backends.** LTTng-kernel
   attaches the same kernel tracepoints through its own out-of-tree modules, its
   own ring buffers and CTF. Consuming it would mean shelling out, which
   boundary 4 refuses, or reimplementing its chain to reach events this tool
   already reaches by a shorter path -- there is nothing to recover. LTTng-UST
   is the interesting half and is still not a backend: it is fast precisely
   because it writes in-line into its own buffer without a trap, which is also
   why its events do not exist for an eBPF program. If it is ever worth having,
   it is worth having as an **import** -- a CTF stream merged into the timeline
   after the fact, a consumer of one more format rather than a source. That is
   written down in `STATE.md` under what remains, at the priority it
   deserves.
