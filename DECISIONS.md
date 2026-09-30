# Omniscient — Decisions

Phase 0 of `ROADMAP.md`: the choices that constrain everything else. Each line is
either settled, with the reason, or open, with what it costs to leave it open.

A decision recorded here is not permanent, but changing one is expected to be
paid for across every collector written since. That is the point of the file.

---

## Settled

| Decision | Choice | Reason |
|---|---|---|
| Language | Rust | The correlator, not the collectors, is where the complexity lives: bounded open-state maps, interval and keyed joins, decompositions with a signed residue. *Amended in the fifteenth wave:* this line first said Rust gives CO-RE eBPF through libbpf-rs; the project uses no libbpf at all. It assembles its programs as instruction arrays (`src/bpf/insn.rs`) and relocates them from the running kernel's BTF at load (`src/bpf/btf.rs`), "Settled in the fifteenth wave" below. |
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
| Kernel floor | **No single floor: each backend states the kernel it needs** (`kernel>=5.8`, `kernel>=6.1`, `kernel>=6.8`), probed and refused by name like any other requirement. *Amended in the fifteenth wave.* | First settled as 6.1, so that `sched.lock` could rely on `lock:contention_*` and nobody would write a second shape of half the chains for 5.15. Both halves stopped being true: the floor was never enforced as a floor -- a requirement on each backend is what the resolver reads, and 33 eBPF backends say `kernel>=5.8`, five say `kernel>=6.1`, one says `kernel>=6.8` -- and since the thirteenth wave the programs read every prototype shape they know, so 5.15 is served ("Settled in the fifteenth wave" below). What the original line protected still holds: below a backend's version its source is refused with the version (`kernel>=6.1: missing: kernel 5.15 is older than 6.1`), never quietly degraded. |
| Privileges | `CAP_BPF` + `CAP_PERFMON`, on the binary | The eBPF backends need those two capabilities; root also works, because root has them. Without them each source is served only as its chain allows, and a backend that needs them is refused with the capability that is missing. *Amended in the fifteenth wave:* this line said an unprivileged run is never resolved onto a lesser backend. Since the fourteenth wave a perf event raised with the program's own registers is opened user-mode only at `perf_event_paranoid` 2, and says what that loses -- which is the same backend asked a stated subset of the question, not a lesser backend; "Settled in the fifteenth wave" below. |
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

## Settled in the thirteenth wave: what a run costs the target

| Decision | Choice | Reason |
|---|---|---|
| What a measurement costs | **What the tool spends plus what its collectors spend inside the target**, over the work the target did of its own (`observed − in-target`), which is the dilation the target would measure. A mechanism that runs in the target without an account is *unknown*, never zero; a ceiling over an unknown part is "not checked whole" (`within_budget: null`), not held. The tool's own share stays as `own_percent`. | Finding 47: `proc.syscall` in `count` was 0.42 % of the target's cpu in `cost.json` and +296 % in the target's own timed section, because the program runs in the target's context. |
| Where an eBPF program's run time comes from | **The kernel's own `bpf_prog_info` counters**, read through `/proc/self/fdinfo` for the programs a request loaded, and only while the kernel is timing programs. This tool does not ask for `CAP_SYS_ADMIN` to turn that on: where `kernel.bpf_stats_enabled` is off, the cost is unknown and the record says how to make it known. | `BPF_ENABLE_STATS` needs `CAP_SYS_ADMIN`, a capability far wider than the two this tool asks for. |
| Where the preload shim's cost comes from | **The shim times itself**, one call in 17, less the real function and the timing's own clock reads, and publishes it in the ring it shares with `omn`. | On the allocation workload its account is 62 to 64 ms against 64 to 65 ms the program lost (release shim); under a lock storm the rest of the loss is contention the shim amplifies, not its own cycles. |
| How a cost band is chosen | **The larger of the tool's share and the target's dilation on the family's reference workload**; a fallback is never declared cheaper per event than what it falls back from. | A band set on the tool's cpu alone called `proc.syscall` cheap. |
| What a counted rate divides by | **The counter's enabled time**, which for a per-task counter is the task's cpu, not the wall. | A counter that was not scheduled for part of the window did not count for that part. |
| A collector that follows one process, asked for several | **Refused**, decided at the runner by whether preparing or attaching it narrowed the target, rather than by a list of collectors. | `--pid a,b` used to measure `a` alone and say nothing. |
| What a shim stream may hold | **2^19 records a run**; past it the reader drops and counts. | The lock storm held 3.9 GiB of what it had read; bounded, `omn` peaked at 1.1 GiB and every record was seen or counted (524 288 and 3 475 712 of 4 000 000). |

`docs/fixes/budget.md` has the measurements and the band table.

---

## Settled in the thirteenth wave: the targets perf serves

| Decision | Choice | Reason |
|---|---|---|
| How a cgroup is counted | **Like a process**: one event, the sum over the cpus its tasks ran on, keyed on the cgroup, from one kernel-filtered event per cpu (`PERF_FLAG_PID_CGROUP`). A sampled source keeps each sample's cpu and task. | Its cpus are where its tasks happened to run, not what was asked about. |
| A container | **Refused until something resolves it to a cgroup**: `container: no` on every entry whose collector serves a cgroup but cannot resolve a container. | The target kind falls back to `cgroup` in the catalog model, so removing the key alone would have served a container as if it were a cgroup path. |
| Per-cpu ring sizes | **One size for every cpu**, halved for all when one is refused; a refusal at the floor names `perf_event_mlock_kb`, `RLIMIT_MEMLOCK` and `CAP_IPC_LOCK`. No probe. | P11: 5.15's default `RLIMIT_MEMLOCK` of 64 KiB, and cpu-by-cpu halving let the first cpus take the whole budget. The budget is per user, and only the mapping knows what the user's other sessions hold. |
| The CI cell | **The kernel's own defaults without capabilities**: `perf_event_paranoid` 2, unprivileged user namespaces, a delegated cgroup -- not the hosted image's hardening (Ubuntu's paranoid 4, AppArmor's namespace restriction). | The first run met the image's hardening and failed 58 tests that assumed the kernel's defaults. A distribution's hardening is a cell of its own, still to add: it is what a user on a stock Ubuntu meets first. |
| A mechanism this build lacks | **Refused at the factory, before the launch, with exit 3**, not by the validator. | A session asking for it stays valid for a build that has it. |

`docs/portability.md`, "The thirteenth wave", has the cells.

---

## Settled in the thirteenth wave: whose events a kernel program keeps

| Decision | Choice | Reason |
|---|---|---|
| What `--cgroup` means | **The cgroup and everything below it**, compared in the kernel by the target's level and the task's ancestor at that level, the way `cgroup_is_descendant` does. Not served inside a cgroup namespace. | A service's or a session's tasks sit in cgroups below the path a reader names; a filter on a task's own cgroup would keep nothing and read as a quiet machine. Inside a namespace the depth read from the path is short by the namespace's own, and nothing a process can read corrects it. |
| A thread and a process target | **A thread target compares the thread (`pid`), a process target the thread group (`tgid`)**, in one comparison every program uses (`insn::Whose`). | Each program had written its own against `tgid` alone, so `--tid` kept nothing for any thread but a process's first, and said nothing (`docs/scenarios/cross-io.md` defect 2). |
| A source bucketed in the kernel | **Reports the mean per measurement, weighted by the count**, and buckets in the kernel's own key. | `sched.offcpu --emit hist` lost individual blocks to a key with no bucket; with the bucket in the key, nothing needs `TOTAL_OF_ATTR`. |

`docs/families/kernel.md`, "The thirteenth wave — stream S", has the ground truth.

---

## Settled in the thirteenth wave: what an older kernel's shape is read as

| Decision | Choice | Reason |
|---|---|---|
| A tracepoint whose prototype moved on an older kernel | **The program reads every shape it knows**, and its catalog line lists the same shapes in the same order, held together by a test. Where a kernel splits a tracepoint in two (`kmalloc` and `kmalloc_node` on 5.15), the program attaches to both. | Each was a source the probe rightly called missing on 5.15 or 6.6 for one moved slot; without `kmalloc_node` the 5.15 guest counted 77 KiB for 2000 datagrams of 2 KiB. |
| A slot that means something else on another kernel | **A different attribute name** (`errno` on 5.15's `block_rq_complete`, against `status`), not a translation table. | A translated value would claim a meaning the kernel did not write. |
| Counter kinds that are different failures | **One series each, named in the key** (`iface:veth0/drops`). | A sum of drops, errors and overruns is none of the three. |

`docs/families/devices.md` has the cells and the guest scenarios.

---

## Settled in the fourteenth wave: what a stock machine is told

| Decision | Choice | Reason |
|---|---|---|
| When a source is usable | **Only through a backend this build collects.** The listing's `here` is "available means it attaches" for this binary; a backend with no collector is shown as such and never makes a source usable. The resolver's order is unchanged: a request still resolves to an unwritten backend when no written one is viable, and is refused as the build's gap (exit 3). | `cpu.cycles` read "usable through `pmu:sw`" on the CI's runner, a backend nobody wrote, and every run of it exited 3. Refusing it as the machine instead was written and reverted: the gap is real, and a `pmu:sw` collector would serve that machine. The listing was what lied. |
| A counter the kernel raises from its own code | **Probed with the kernel included; a user-mode-only answer is missing**, and a counter the machine would open for user mode only is refused, not noted. | A context switch or a migration never happens in user mode: at paranoid 2 `sched.switches -m count` read 0 for 406 switches. |
| A sampled source where the kernel allows no more | **Falls back to user mode when its events are raised with the program's registers** (page faults), and says what that loses; a counted one already did. | `mem.faults` was refused outright at paranoid 2; in user mode every fault the program's own code takes is there, and a caveat says which are not. |
| The CI | **Two cells**: the kernel's defaults (paranoid 2, a delegated cgroup) and the hosted image as shipped (paranoid 4), each with its own expected skips beside the common ones. | The two CI runs were the only ground truth for a stock machine, and each showed defects the other did not. |
| A requirement a collector's own start decides | **Probed by that start** (`phc` by `hwts::clock_support`, `ethtool` by resolving the family). | `net.hwts` read usable on a runner whose PTP clock was root's alone. |
| An `fentry:` requirement | **Carries its prototypes like a raw tracepoint's** (`fentry:name(a, b)`, each further shape after a `\|`); a bare name asks for existence only. | A kernel whose prototype the program has never seen reads missing in `omn list` instead of failing at the attach. |
| Where a check that asks the machine runs | **At the attach, never in a factory.** A factory's refusal is classified as the build's (exit 3); an unmarked refusal at the attach is the machine's (exit 1). | `sched.switches` on `pmu:sw` at paranoid 2 was first refused from the factory, and read as a gap in the build. |

`docs/portability.md` and `docs/families/kernel.md`, "The fourteenth wave", have the ground truth.

---

## Settled in the fourteenth wave: every way to unwind a stack

| Decision | Choice | Reason |
|---|---|---|
| What an unwind mode is | **A chain the chooser walks like a backend chain**: `sframe`, `fp+leaf`, `dwarf`, `lbr`, `fp`, `none`, each with `requires`, `copies`, `cost` and what it degrades, in `catalog/unwind.yaml`, held by the loader to the fallback rule. `--stack auto` takes the first mode whose every requirement is available; unknown never counts. | Several mechanisms can do one job, and the resolver's shape is how this project chooses between them. |
| Where the table lives | **`catalog/unwind.yaml`, compiled in, and read from a `--catalog` directory when it has one**. | `Catalog::load_dir` skips `catalog::UNWIND_TABLE` rather than parsing it as entries, and `stack::unwind::use_table_in` reads it; the fifteenth wave's resolver stream moved it. |
| What `auto` is decided by | **The target as well as the machine**: `fp+leaf` for a build with frame pointers (`main`'s prologue, or more than a quarter of the indexed functions), `dwarf` for one without, `fp` for a system-wide or cgroup profile. | The cheap mode where it loses nothing, the expensive one only where the cheap one would be a profile of leaves. A Rust debug binary is profiled with `dwarf` by default now. |
| `lbr` and `sframe` | **Declared, probed and not collected.** | No machine this project runs on offers either (no cpu here has a branch call stack; no kernel through 7.2 reads `.sframe`); an untested decoder is worse than a refusal. |
| The DWARF unwinder | **`framehop`**, over libdw. | Pure Rust, nothing to find at install; libdw would tie the release to elfutils' ABI. |
| At a paranoia that refuses the kernel | **Every mode is user-only, `none` included**, and the choice says what that loses. | The kernel refuses the unprivileged sampler itself, not only the stack's kernel half. |
| What a mode costs | **Measured from outside the tool**: the kernel's account of `omn` against the target's own clock. The tool's own figure is a lower bound, never the reference. | `cost.json` said 26 ms where `wait4` counted 986 ms beyond the target (filed). |
| Names for a stripped file | **debuginfod, fetched with `curl`, into elfutils' own cache, and never unless asked.** | The public servers are HTTPS; a TLS stack linked in for one optional request was the alternative. |
| A recovered frame | **Marked in its label** (` [recovered]`). | An inferred frame is different evidence from a walked one, and the label is what reaches the results, flame graphs and `omn diff`. |
| The IBS sources | **Keep frame pointers**: `auto` means `fp` there, and a forced mode that needs more is refused by name. | |

`docs/unwind.md` has the call tree each mode was held to.

---

## Settled in the fourteenth wave: what a run's results claim

| Decision | Choice | Reason |
|---|---|---|
| Where a run's windows close | **At one instant before the first collector is stopped**; a detached one just before its own stop. A join treats a source that ran to the end as watching to the end. | Closing after the last stop claimed spans a drained collector could not read (71 blocks of 930 in no row); one close per collector would have given one session's sources different windows. |
| What a container is | **Its cgroup, found by asking its runtime** (`docker`, `podman`, `nerdctl`, or `OMN_CONTAINER_RUNTIME`), bound before resolution. The session keeps the container, the resolved request the cgroup, and the run says which. A container no runtime knows is exit 1. | A runtime's state files are its own layout; asking it the way a user would is the one stable interface. |
| A container on an entry that says `container: no` | **Refused, not bound before validation.** The entries' key is what changes (filed for P and U). | Code overriding explicit catalog data would make the validator stop being a projection of it. |
| A dump's format | **Checked before resolution.** | Whether a format can hold a request does not depend on the machine. |
| The shim's account of a preempted call | **A timed span over 200 us is left out, counted, priced at the mean of the others, and reported** (`in_target_left_out`). | It is a scheduler slice, never the hook: 610 ms accounted for 396 ms lost. |
| A `paired` decomposition part | **Refused before the run** until the decomposition pairs over the windows the joins were given (filed for J). | Run as it is, it would be reported different from its own join. |

`docs/fixes/results.md` has the ground truth.

---

## Settled in the fourteenth wave: joins, agents and the release

| Decision | Choice | Reason |
|---|---|---|
| A join with no key | **`overlap`'s alone, reading its `from` source as one series keyed `machine`.** A keyless rule of any other kind is refused before the run; a keyless `overlap` over a source with several series is refused by name at apply time. | `KeyKind` names what an event carries, and machine-wide state carries nothing; a `machine` key variant would say otherwise. |
| The spread of a repeated benchmark through MCP | **Computed by the model from N answers; the walk-through says how** (`result.lifetime`, the bench's statistics, the gate's two conditions). | Reaffirms the thirteenth wave's no-tool decision with the use case run: a server-computed spread would be the one number no document carries. |
| Where the walk-through's approved sessions live | **`examples/mcp/approved/`, with the client configuration one level up.** | `list_approved` lists every session-shaped file under the directory, `.json` included. |
| Who answers a machine question asked through MCP | **The `omn` binary that would run the session.** | A copy of `omn` without its capabilities answered 52 usable sources against 93, silently. |
| The installed layout | **Flat: `bin/omn`, `bin/omn-mcp`, `bin/libomn_preload.so`.** | `omn` finds the shim beside itself and `omn-mcp` finds `omn`; `setcap` refuses a symbolic link, so `lib/omn/` with links in `bin/` would put the capability on a file the user does not name. |
| What installing grants | **Nothing.** `make install` and the archive print the `setcap` line for the installed path. | A grant is a decision about the machine; a `DESTDIR` build cannot make it on the final path, and tar does not carry it. |
| The glibc floor | **2.34, built on the oldest hosted Ubuntu, enforced by `release.sh verify`.** Raising it changes `GLIBC_FLOOR` and the two documents together. | The floor is the newest symbol linked against; printed rather than enforced, it would drift. |
| `make install` as root | **Copies, never builds**; a system-wide install is `make release && sudo make install`. | Building as `$SUDO_USER` from a root recipe depends on a `PATH` sudo's `secure_path` drops. |

`docs/families/join.md`, `docs/mcp.md` and `docs/install.md` have the ground truth.

---

## Settled in the fourteenth wave: a chain ordered by cost

| Decision | Choice | Reason |
|---|---|---|
| `sched.switches` per cgroup (was open) | **A `sched_switch` program, second in the chain, in `count` mode**, serving every target and reached first for a cgroup; `pmu:sw` stays first wherever it serves, and `trace` stays `pmu:sw`'s. The program's rows carry the kernel's voluntary/involuntary split (`--backend force:bpf:raw_tp -m count`). | On the question both answer, an inherited counter is exact and free and the program costs a run on every switch of the machine; the `woke` join needs a per-switch event. The first chain ordered by cost where two backends answer the same question and one answers more: "the backend that answers best first" holds for the question asked. |
| A loss counted over the whole machine | **Not slack for one workload's count.** A scenario whose count an eBPF program's skipped runs would excuse holds the count exact in a run that lost nothing, repeats until one does, and says so when none did. | The read-ahead lookup lost 395 000 to 477 000 runs against a shortfall of 3 to 15 of 256: as slack, it would hide a silent loss in the drain. |
| A sweep leg on a machine that withholds one source | **Leaves out that source, printed, and asks the rest**, not the whole leg. | Skipping the leg left every other source of it untested wherever `net.hwts` was missing. |

---

## Settled in the fifteenth wave: what the first lines became

Five things the code settled and this file never said. Three are Phase 0 lines
whose reasons no longer held, amended in place above; two are Phase 1 and
Phase 3 tasks the engine answered differently from the roadmap. The fourteenth
wave's audit of `ROADMAP.md` found each one; every reason below was read off
the code.

| Decision | Choice | Reason |
|---|---|---|
| How an eBPF program is built | **Assembled in Rust as an instruction array and relocated from the running kernel's BTF at load.** No libbpf, no clang, no object file, no `vmlinux.h`. | The same relocation compile-time CO-RE performs, done a few milliseconds later against the kernel that will run the program, so an offset cannot be stale. `omn` stays one binary: building it needs Rust and nothing else, and running it needs no libbpf (`docs/install.md`). It talks to `bpf()` itself, which is boundary 4's rule one layer down. |
| The kernel a source needs | **A `kernel>=` requirement on each backend that needs one**, probed like the rest, and no floor for the tool. | A backend is refused below its own version, by name, and the others still serve. 5.15 is served: 86 sources usable in a 5.15.221 guest with the capabilities, against 89 on 7.2 (`docs/portability.md`). |
| A perf event at `perf_event_paranoid` 2 without `CAP_PERFMON` | **Opened user-mode only when the program's own registers raise it, and noted; refused when only kernel code raises it.** | A user-mode page fault, instruction or profile sample is the program's own. A context switch or a migration never happens in user mode, so a user-mode count of it is zero. |
| The roadmap's `Span` struct | **Not written.** Evidence lives on `join::Correlation` (per rule) and `join::Attribution` (per upstream state). A decomposition's rows are the tree a span would have built. | What the engine emits is aggregated and bounded. A span per matched event would be a record per event. |
| Reordering | **No bounded window and no watermark. Each collection is sorted by timestamp after the fact**, and a join sorts what it merges. | Every output is a document over a closed window, so waiting for the close costs nothing the document was not already waiting for. A sort is exact at any lateness. |

**The eBPF layer.** `ROADMAP.md`'s Phase 0 table offered Rust with libbpf-rs,
C with libbpf, or Go with cilium/ebpf, with CO-RE mandatory. CO-RE's purpose is
kept and its mechanism is not. `src/bpf/btf.rs` reads `/sys/kernel/btf/vmlinux`
and answers two questions: where is this field, and does this type exist.
`src/bpf/insn.rs` writes the offsets into the instructions while `Program`
resolves jumps by label. A field the kernel does not have is an error naming
the type and the field, never a guessed offset. The costs, stated in
`docs/bpf-layer.md`: a program is written as instructions, and a mistake comes
back as a verifier log rather than a compiler error, which is why `LoadError`
carries the log. And the parser is the project's own. `fentry.rs` has a
second one, `FunctionIndex` (`docs/open-requests.md`, line 5520). Without BTF
the raw-tracepoint and fentry backends read *unverified*, and none of them is
chosen (the `nobtf` cells of the matrix). That is CO-RE's "mandatory", enforced
by the probe.

**The kernel floor.** The 6.1 floor was never a floor in the code.
`capabilities.rs` answers `kernel>=<version>` per requirement, and the
resolver reads requirements per backend. In the catalog today:

- `kernel>=5.8` is on 33 eBPF backends. 5.8 is the release that added
  `BPF_MAP_TYPE_RINGBUF`, which these programs publish through, and `CAP_BPF`
  and `CAP_PERFMON`. That is the kernel's history; the catalog lines do not say
  it, so this line does.
- `kernel>=6.1` is on five: the three `sched` programs written against the
  four-argument `sched_switch` (`runqlat`, `offcpu`, `switches`'s count), and
  `sched.lock`'s two backends, which need `lock:contention_*`.
- `kernel>=6.8` is on one, `mem.thp`'s `fs:sys` (its `fs:proc` fallback serves
  older kernels).

The thirteenth wave's stream N made the `io`, `net` and `mem` programs read
every prototype shape they know ("A tracepoint whose prototype moved on an
older kernel", above). With the capabilities the matrix attaches 77 sessions
on 5.15 and 100 on 7.2, and each difference in the listings is a requirement
the catalog wrote, answered by name (`docs/portability.md`). The matrix has no kernel between 5.15 and 6.1,
so whether the five 6.1 backends would serve a 5.18 is not known. They keep
6.1 until a cell says otherwise. The original line's other argument, one
fewer shape to test, is now paid on purpose: the portability matrix is where
the second shape is tested (`scripts/matrix.sh`).

**User mode at paranoid 2.** The kernel's default `perf_event_paranoid` is 2.
At 2, without `CAP_PERFMON`, a process may open a perf event on another process
it owns only with `exclude_kernel`. The original line refuses
`mem.faults`, `cpu.instructions` and `cpu.profile` there outright: the
question needs kernel mode, so answer none of it. The fourteenth wave served
them instead ("A sampled source where the kernel allows no more", above).
That choice is not the lesser backend the line ruled out, for three reasons:

- The backend, the event and the target are the same.
- What is left out is named. `mem.faults` says faults the kernel took on the
  program's behalf are not there. The counters say *"kernel-mode events are
  not in this count"* (`src/collect/cpu/counters.rs`). `cpu.profile` says its
  stacks are the *"user half only"*, or *"user-mode samples only"* without
  stacks (`src/collect/cpu/profile.rs`).
- Where user mode would change the answer rather than narrow it, the source is
  refused, with the sysctl and the capability that would serve it.
  `counted_only_in_kernel` (`src/capabilities.rs`) lists the events the
  scheduler raises. `sched.switches -m count` read 0 there against 406.

So the rule the line was protecting is kept: a result depends on who ran it
only in ways the result states. The line's other fear, a second chain shape
that doubles the resolver's test matrix, did not come true. The chain is the
same. What differs is one flag on the event and a note, and the CI's
`defaults` cell runs at paranoid 2 on every push.

**No `Span` struct.** Phase 1 asked for `Span {id, parent_id, start/end, keys,
rule_id, evidence}`: one record per link between two events, which chains into
a tree. The engine keeps and reports something coarser, and on purpose:

- `Correlation` is one rule over one run. It has the rule's id (`rule`), its
  sides (`from`, `to`), its `evidence` and, when crediting within a key was
  weaker, `within_key`. It also counts matched, unmatched, keyless and
  outside-window events, and every state's ending (`StateStats`: opened =
  closed + expired + overflowed + window_closed + still_open).
- `Attribution` is one upstream state: its key, its range or scope,
  `opened_ns` and `closed_ns`, how many downstream events it got, and how it
  ended. The report keeps the heaviest `rule.top` of them.
- The tree a `parent_id` chain would build is a decomposition (`decompose:`):
  a whole, its parts each with an evidence level, and a signed residue.

A span per matched event is a record per event: `examples/allocation-cost.yml`
attributes 2655 faults in one run (`STATE.md`), and a trace's count grows with
the run. That is
unbounded growth in the one place this file requires bounds ("Eviction is data,
not housekeeping", above). The engine keeps its open states under `capacity`
and its output under `top`.

What it costs: the results do not link one downstream event to the state it
was credited to. A reader cannot walk from one page fault to its allocation.
They see which allocations got the most faults, and how many faults found
none. If per-event links are ever wanted, they belong in `raw`, as an
attribute naming the state's key, not in a second struct. A naming hazard,
recorded so nobody reads it as the roadmap's: `join::Span` exists, and it is a
decomposition's whole (`window` or `lifetime`), not a link between events.

**No reorder window.** Phase 3 asked for a bounded window and a watermark:
finalize nothing until every collector has passed T. The runner does neither.
`run_all` sorts each collection by `ts_ns` when the window closes
(`src/collect/mod.rs`), and a flight-recorder document is sorted when it is
cut. The join engine merges its sources and sorts them by timestamp, with the
opening side first at an equal instant (`join::apply_within`), so a fault at
an allocation's own nanosecond is not unmatched.
`join::input_order_does_not_change_the_answer` holds the engine to that. The
reasons:

- Nothing is final before the window closes anyway. A result is a histogram, a
  ranking, a join or a decomposition over the whole window. A watermark would
  let events go out earlier to a consumer that does not exist.
- A watermark assumes a bounded pipeline latency, and the latency here is
  unbounded by design. A drained program's rows reach userspace at a drain, a
  grace period after the program's last write. `net.request` holds a request
  until the kernel publishes the drain record of its last transmit. The
  preload shim's stream is drained every 10 ms. A watermark at any bound
  shorter than the run either drops or misplaces what arrives after it, which
  is a silent loss. A sort after the fact has no lateness at which it is wrong.
- The rings do not order events anyway. A perf ring is per cpu. A
  `BPF_MAP_TYPE_RINGBUF` is one ring in commit order, not timestamp order.

What it costs a live stream: nothing leaves before its window closes. A run's
document is written at the end, and a recording's at each cut. The runner
holds every event of the window in memory until then. A recording's
retention bounds that, and so does the shim's 2^19 records a run; a plain run
of a busy trace does not bound it. The two things that decide mid-run do not
need a cross-collector order. The escalation's trigger reads one round's
states, and the recorder's predicate reads one round of its `on` request. Both
are evaluated per round. A consumer that wanted events in order *as they
happen* (`ndjson` written live, or the resident daemon the ninth wave ruled
out) would need the watermark, and with it a stated lateness bound and a
count of what arrived after it. That is where this line would be reopened.

---

## Settled in the fifteenth wave

What the other five streams settled, from their handoffs
(`docs/fixes/w15-{engine,kernel,probes,resolver,runner}.md`), each with the
test that holds it there.

**The engine** (`docs/fixes/w15-engine.md`):

| Decision | Choice | Reason |
|---|---|---|
| An integer attribute's form | **One canonical form**: `Uint` when non-negative, `Int` only when negative, and the two equal where they spell the same integer. The wire is unchanged. | Every consumer of the output (`results.json`, ndjson, `omn-mcp`) reads a number as a number; a tag would change every one of them to say what the value already says. `event::tests::a_non_negative_signed_integer_reads_back_as_what_was_written`. |
| A decomposition row's evidence | **Every row and every whole carries one, except a whole that is a bracket** (the window, the program's life), which contains the key's events and is joined to nothing. A document without it is refused when read, not shown with a dash. | `join::tests::a_row_without_an_evidence_level_is_never_built_and_never_read`. |
| A join state that matched nothing | **Not kept once it ends**, except for `wakeup`, whose chains count every wakeup. | Peak memory at one million events went from 179 MiB to 97 MiB; `join_engine::a_join_under_load_ends_every_state_it_opened_and_holds_no_more_than_its_map`. |

**The kernel programs** (`docs/fixes/w15-kernel.md`):

| Decision | Choice | Reason |
|---|---|---|
| A backend that serves fewer modes than its chain | **`modes:` on the backend**, so the resolver passes it over with the reason, rather than a factory refusing at the start. | A refusal after the probes said yes is finding 15's shape; the catalog is the one place that can say it before resolution. `net.hwts`: `so_timestamping` is `[trace]`, `netlink:ethtool` is `[poll, snapshot]`. `scenario_net::every_usable_net_source_starts_in_every_mode_that_resolves`. |
| When `io.latency` builds its histogram in the kernel | **Only when a log2 histogram is the whole request**: one `hist` emit, `trace`, no filter, keys within `dev`. Anything that needs the requests one at a time streams, and the run says which it was in `facts()`. | The choice changes nothing the request asked for. Not taken: the kernel path for `hist` beside `summary`, whose min and max would become bucket bounds, a silent change of meaning. `scenario_io::a_histogram_built_in_the_kernel_is_the_one_the_streamed_requests_make`. |
| How an overflow test overflows | **By construction**: a burst larger than the ring inside one system call (one `FUTEX_WAKE` of 16 384 threads), the kernel's return value as ground truth, one run. | A test that fills only on a slow machine skips on a fast one (flaky tally #3). `scenario_sched::an_overflowing_wakeup_ring_says_what_it_could_not_hold`. |

**The probes and the validator** (`docs/fixes/w15-probes.md`):

| Decision | Choice | Reason |
|---|---|---|
| `cpu.cycles` with no hardware PMU (was open) | **Served by the cpu clock** (`pmu:sw`, degraded: *"CPU time, not cycles: blind to DVFS"*), counter name `cpu_clock_ns`. | Every mechanism as a chain, with a fallback that says what it degrades. The cost: a number in the entry's unit that is nanoseconds, said on the `degraded:` line and in the counter's name. `scenario_cpu::a_cycle_count_and_its_cpu_clock_fallback_differ_by_the_core_s_frequency`. |
| A container | **Its cgroup, with no exception**: `container: no` beside a served cgroup is a catalog that does not load (`validate::container_complaints`). | `scenario_cpu::a_container_is_counted_as_the_cgroup_its_runtime_names`. |
| A (target, mode) pair no backend declares together | **The request's, exit 2, before resolution**, the way a mode the entry does not declare is; the resolver's half is `NoBackendDeclares`, exit 2, and a pinned backend keeps its "pinned" refusal. | `catalog_model::a_target_and_mode_no_backend_declares_together_is_refused_as_the_request`, `tests/cli.rs::a_target_and_mode_no_backend_declares_together_is_refused_as_the_request_s`. |
| A privilege refusal where the file carries the capability | **Names why the kernel withheld it** (a `nosuid` mount, `no_new_privs`, a grant without `e`), and never advises the grant already made. | `portability::a_binary_denied_its_file_capabilities_says_why_instead_of_advising_the_grant`. The attach-time advice (`bpf::permission_advice`) is still the old sentence (filed). |
| What the resolved record carries | **The privileges and the virtualization**, so a gap between two machines is explained from their two records; a matrix cell carries the hash of the binary that swept it (`cell.build`). | Phase 7's second criterion found the record missing both: 2754 gaps between the cells on disk only they could explain. `portability::every_gap_between_two_cells_is_explained_by_their_resolved_records`. |

**The resolver** (`docs/fixes/w15-resolver.md`):

| Decision | Choice | Reason |
|---|---|---|
| `--symbols debuginfod` where the collector would not ask | **Refused, exit 3** (the build's gap). Which collectors read it is `stack::UNWOUND_BY_CHOICE`. | A network request silently not made is a degradation nobody was told about. `tests/unwind.rs::symbols_from_debuginfod_with_nowhere_to_ask_are_refused_before_the_program_starts`. |
| The unwind choice in the resolved file | **Recorded only for the collectors that use it as chosen**; the IBS sources ask `choose` and serve frame pointers, and are left out. | Recording it for them would misstate their stacks. `tests/unwind.rs::the_resolved_file_says_how_the_stacks_will_be_unwound_before_the_run`. |
| A request keyed on what the selected backend does not attach | **Refused (`KeyNotCarried`), naming the backend that does and what stands between the request and it**; the chain is not walked on to it. The `--backend force:` hint only for a backend that serves the target and mode, is collected and whose requirements this machine has; the exit code is that backend's gap (2, 1 or 3). | Which backend serves is the resolver's decision, and a key is part of the question. `resolve::tests::a_backend_that_attaches_the_key_is_named_for_what_keeps_the_request_from_it`. |
| A stack choice `choose` refuses | **A resolver refusal (`StackRefused`)**, so `--dump-resolved` refuses what the run would, with its code from `unwind::Refusal::whose`. | `tests/unwind.rs::a_choice_the_run_refuses_is_refused_by_the_dump_with_the_same_reason`. A forced mode this machine cannot serve stays exit 3 until `tests/scenario_cpu.rs` takes the filed change to exit 1. |
| A no-collector refusal whose passed-over backend is collected and fits | **The machine's (exit 1)**, and it leads the refusal. | The build is not what is missing then; the machine or the install is. `tests/cli.rs::a_refusal_for_an_unwritten_backend_leads_with_the_one_passed_over`. |

**The runner** (`docs/fixes/w15-runner.md`):

| Decision | Choice | Reason |
|---|---|---|
| What a run cost the machine | **Reported whole, and weighed apart from what it cost the target**: `cost.json` carries this process's cpu before the window (startup, probes, the kernel's symbols) and after it beside the window's; the percentage and `--budget` stay over the window. | None of the rest dilated the target. A one-second profile with stacks said 82 ms where the kernel counted 1062 ms. `budget::what_a_profile_with_stacks_says_it_cost_adds_up_to_what_the_kernel_counted`. |
| A budget's move between backends | **An overlap, as short as the two calls, and announced**, rather than a gap. | A gap's events are counted by nobody and nothing could show them; an overlap is bounded and said. `budget::a_budget_s_move_between_backends_drops_nothing_and_says_where_both_counted`. |
| A bench row over its declared band | **Flagged, not failed; a row missing a column fails the table.** | A band a machine does not keep is a claim to correct; a row with nothing measured is a table that is wrong. `bench_harness::a_row_is_held_to_its_band_on_the_larger_of_its_share_and_its_dilation`. |

---

## Open

- **An `overlap` rule's `span` that encloses a drained row's occurrences.**
  A row of several episodes whose enclosing window fits in one interval could
  be credited whole and exactly, but the engine cannot tell an enclosing
  window from a sum of durations by the attribute's name, so the rule would
  have to say which. Leaving it open costs the credit of drained sources:
  41 184 of 41 639 reclaimed pages uncredited on the reclaim crossing, counted
  and said.
- **What one series made of two modes means to an emit** (the runner stream's
  proposal, `docs/fixes/w15-runner.md`). The budget's trace -> hist -> sample
  ladder needs it before it is built: proposed, a budget may move only onto a
  mode whose events the request's emits can take (a `hist` of traced events
  and an in-kernel `hist` merge; a `raw` does not survive the move and is
  refused up front). Leaving it open costs phase 6's ladder: a budget moves
  between backends of one mode only.
- **The budget's denominator in nanoseconds** (the same handoff). Proposed: a
  launched or pid target's cpu from `schedstat` (nanoseconds on cpu), not
  `stat` (10 ms ticks). Leaving it open costs every target that burns under a
  tick per check: `sched.offcpu --budget 5%` over a sleeping python loop ended
  16 % over its ceiling and was never moved.
- **Whether the `power` sources are the bench's guardrail** (phase 9), or the
  harness's own `thermal_throttle` read stays. The probes stream stated why the
  bench reads the counters directly (`docs/portability.md`, item 4) and filed
  the call for the runner stream; neither side landed it.

A new open line goes here with what it costs to leave it open, which is the
habit that kept this section honest.

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
