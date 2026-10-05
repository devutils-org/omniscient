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
| "Valid, and not built" is exit 3, wherever it is discovered, on a machine that lacks nothing the backend needs | `NoCollector` and `UnsupportedTarget` both exit 3 | The two exits mean different things to a caller: exit 1 is a problem with this run, and retrying or raising privileges may help; exit 3 is a problem with this build, and neither will. *Eighteenth wave: a backend both unbuilt and missing a requirement here is the machine's on every path ("one order for unbuilt and missing", `e04ef9a`); a test that wants the build's refusal pins a backend whose requirements hold, or branches on what `omn list` probes for it.* `mem.heap.alloc --pid` is a request the catalog declares -- a uprobe serves it -- and that this build cannot answer, and it exited 1, indistinguishable from a counter that failed to open. The same refusal is also now asked *before* "pass --duration", because advice for a request that cannot work is a dead end. |

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
| Whether a model may run a session | **Only when the person who started the server passed `--allow-run DURATION`**, for at most that duration, with a required budget, writing only under the server's directory -- and **never a session that launches a program**. *Amended in the thirteenth wave: a launching session the person approved, below. Amended in the eighteenth wave: with an optional budget, and every run says what a budget is a percentage of (`budget_against`); the duration stays the server's bound.* | Asking for a measurement is what phase 11 grants; running a program a model chose is changing the machine (boundary 5). The opt-in has `make CAPTURE=1`'s shape: the power closest to acting is granted by a person, when asked. |
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
| The kernel a source needs | **A `kernel>=` requirement on each backend that needs one**, probed like the rest, and no floor for the tool. | A backend is refused below its own version, by name, and the others still serve. 5.15 is served: 86 sources usable in a 5.15.221 guest with the capabilities, against 89 on 7.2 (`docs/portability.md`), an earlier sweep's count; the sixteenth wave's sweep, with its own binary, counts 71 against 80 on 7.2.7. |
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

## Settled in the sixteenth wave

What the six streams settled, from their handoffs
(`docs/fixes/w16-{cpu,io,mem,runner,engine,docs}.md`), each with the test that
holds it. The four lines that were open after the fifteenth wave are among
them: the enclosing window of a drained row (engine), what one series made of
two modes means to an emit and the budget's denominator (runner), and the
bench's guardrail (runner).

**The budget** (`docs/fixes/w16-runner.md`):

| Decision | Choice | Reason |
|---|---|---|
| What one series made of two modes means to an emit (was open) | **A budget may continue a series in another mode only for the emits that weigh each event by what it stands for** -- `hist`, `topk`, `rate`, `summary`, which read `weight`: a traced event weighs one, an in-kernel histogram row its bucket's count, a sampled event its period. A mode that cannot produce an emit the request asked for is not a rung. An emit the cheaper mode produces only by listing two kinds of event as one (`raw`, `timeline`, `stack`) makes the request invalid under a budget: refused before the run, exit 2. The one mode rung is `trace` -> `sample`, taken after the same-mode backends below. The in-kernel histogram is taken at the start when the emits allow it, never stepped onto. | An invalid combination is refused with the reason, never degraded silently; stepping onto a histogram mid-run would drop every emit it cannot answer. `request::tests::a_series_crosses_modes_only_in_the_emits_that_weigh_each_event`, `budget::a_budget_takes_no_mode_rung_whose_sampled_events_would_not_be_weighed`. |
| Which collector a mode rung may move onto | **Only one that weighs its sampled events** (`WEIGHS_ITS_SAMPLES` in `src/main.rs`; `mem.faults` on `pmu:sw` since the merge, whose samples carry `weight = period`). One that does not is passed over and named, not taken and flagged afterwards. | A sampled event with no weight counts once where it stood for its period: a moved series was 25 times short (61 892 faults against the task's 1 537 069). Weighed, a moved run counted 1 707 073 against the task's own 1 706 035 to 1 708 083 inside the window. `budget::a_traced_series_over_its_ceiling_counts_what_the_task_did_and_moves_only_onto_weighed_samples`. Which collectors weigh is the build's fact, to move into the collector registry when its owner adds the field. |
| The budget's denominator (was open) | **The scheduler's nanoseconds.** A process: its process cpu clock (every thread's `sum_exec_runtime`, exited ones included) plus its reaped children's ticks. A thread: its `schedstat`. `stat`'s ticks only where neither can be read, and the record says so. | A target that burns under a tick per check was weighed at 0 ns and a 5 % ceiling was never acted on (16 % over). `collect::task::tests::a_target_that_burns_under_a_tick_is_read_in_nanoseconds_against_its_own_clock`, `budget::a_target_that_burns_under_a_tick_per_check_is_weighed_and_the_budget_acts_on_it` (un-ignored at the merge). |
| A budget's move and a pin | **Not the same.** The ladder resolves onto its rung (`resolve::resolve_onto`) without marking it pinned; only the user's `--backend force:` says the chain was not walked. A move between modes on one backend is named as one ("two modes end to end"). | A note that says the user pinned what nobody pinned is a false record. `budget::a_budget_s_move_between_backends_drops_nothing_and_says_where_both_counted`. |
| The bench's thermal guardrail (was open) | **The harness's own read of `thermal_throttle`**, not `power.throttle` through the tool. | It wraps the bare baseline runs, which an `omn` beside them would turn into traced runs, and a guardrail measured by the tool under test would pass the tool's own defect in that source. `docs/bench.md`; `ROADMAP.md` phase 9 says so. |
| A filter field's elsewhere | **Chosen as a key's is**: forceable, then missing, then unwritten, then unfit; a `--backend force:` hint only for a backend this build collects and this machine runs. | `resolve::tests::a_filter_field_another_backend_carries_is_named_for_what_keeps_the_request_from_it`. |

**The engine** (`docs/fixes/w16-engine.md`):

| Decision | Choice | Reason |
|---|---|---|
| A drained row's enclosing window (was open) | **The rule's to name**: an `overlap` rule says `enclosing: <attr>`, the attribute holding a window from the row's stamp that encloses every occurrence. Such a row is credited whole to the one interval holding the window, and stays uncredited when it crosses one. Without it, `span` is a sum and the row is uncredited. A source without the attribute is refused (`MissingSpan`); `enclosing` on another match is refused (`EnclosingOutsideOverlap`). | A drained row carries both numbers, and a rule needs both. `mem.reclaim`'s `window_ns` is such a window today: `cross_machine::a_drained_reclaim_row_is_credited_whole_inside_one_second_of_pressure` credited all of `memory.stat`'s pgsteal in four runs (41 246 to 42 263 pages), against under 1.1 % read as a sum. |
| What an interface is | **Its index, within one network namespace.** A join compares `iface` by the kernel's index (`iface#<index>`), filled from the name at the read when a producer has only the name; the name is what a row prints, and a sub-series after `/` is not part of the interface. | A rename made a second key, and `net.iface`'s `lo/drops` could not join anything. `join::tests::an_interface_is_one_key_by_its_index_across_a_rename_and_a_sub_series`, `join_engine::two_sources_of_one_interface_join_by_it_and_credit_what_lo_moved` (4 112 000 bytes credited for 4 112 000 counted). *Superseded in the seventeenth wave: the namespace is part of the identity (below).* |
| `proc` | **Stays as vocabulary, with no producer**, the way a source with no collector does; a join `via: proc` is refused by name until a source declares it. | `CATALOG.md`'s container trap: removing the kind and adding it back later costs more than a refusal that names it. *Superseded in the seventeenth wave: the per-process procfs sources carry it (below).* |
| The engine's memory | **Bounded by its map and its `top`**, plus an index of any input stream that is not in order. A run's collections are handed to it apart (`join::apply_merging`), never laid end to end. | 0.6 MiB beyond a million events walked apart, 15.3 MiB laid end to end; the hour ran at 0.7 MiB a round. `join_engine::a_join_under_load_ends_every_state_it_opened_and_holds_no_more_than_its_map`, `join::tests::streams_walked_apart_meet_in_the_engines_order`. |

**The resolver's answers** (`docs/fixes/w16-cpu.md`, `w16-io.md`):

| Decision | Choice | Reason |
|---|---|---|
| A forced `--stack` mode whose requirement the machine lacks | **The machine's refusal, exit 1**; one the machine would serve with no collector is the build's, exit 3. | The two answers `Whose` already defines, rather than one code for both. `scenario_cpu::an_unwinder_this_build_lacks_is_refused_before_the_program_starts`. |
| A backend's own unit | **Declared on a fallback that says what it degrades, never on the head or an `equivalent` backend**; the result carries the unit of the backend that served it (`Entry::backend_unit`). | A value in another unit is another measurement. Chosen over renaming `cpu.cycles`'s unit or splitting the source, because a chain is the shape for several mechanisms of one job. `catalog::tests::a_backend_reports_in_its_own_unit_only_where_it_is_a_fallback_that_says_so`, `scenario_cpu::the_cpu_clock_fallback_of_cpu_cycles_reports_nanoseconds_and_the_counter_a_count`. |
| A `KeyNotCarried` whose attaching backend is unwritten | **The build's gap, exit 3**, as `KeyElsewhere::whose` says; `catalog/io.yaml`'s first shipped `keys:` make it reachable. | `scenario_io::a_request_keyed_on_what_the_counter_files_do_not_name_is_refused_with_the_backend_that_does`, `tests/cli.rs::a_request_keyed_on_what_its_backend_does_not_attach_is_refused_with_the_backend_that_does`. |
| A join over a request answered by an in-kernel histogram | **Refused by the validator, exit 2**, only where the collector really builds it (`collect::io::builds_histogram_in_kernel`, `io.latency` today). | The collector cannot see joins; the request is fine alone and wrong only with the join. Refusing a combination that works would be the opposite failure. `validate::tests::a_join_over_a_histogram_built_in_the_kernel_is_refused_with_the_emit_that_serves_it`, `family_io::a_join_on_blk_is_refused_only_over_a_histogram_the_kernel_builds`. |

**What a test holds the tool to** (`docs/fixes/w16-{cpu,io,mem,docs}.md`):

| Decision | Choice | Reason |
|---|---|---|
| A forced error path | **Forced with an error in the shape the layer below returns**, never a hand-built one. | A bare errno is what let paranoid 2's user-mode retry, which never ran, pass on the cell it was written for (`docs/scenarios/mem.md`, D14). `collect::mem::page_faults::tests::a_refusal_is_recognised_in_the_shape_perf_returns_it`. |
| A cgroup test that wants reclaim | **`memory.high`, not `memory.max`**, unless the limit itself is measured; a throttled child is not required to finish, since the throttling is the stall. | `max` adds an OOM kill that is the disk's speed, not the subject. `collect::mem::pressure::tests::a_cgroup_forced_to_reclaim_stalls_and_an_idle_one_does_not`. |
| Two backends of one source | **Answer one kernel counter under one attribute name, or carry different names.** | Matching on the day is not evidence (D6 matched, D15 did not). `scenario_mem::both_huge_page_backends_read_the_same_promotion_counter`. |
| A run that exited 0 | **Was served**: a skip looks for a machine's limit in the refusal, never in the caveats of a run that worked. | `scenario_mem` skipped a comparison on every machine for a wave by reading a caveat (D16). |
| A gauge | **Held to the kernel's own reading of the same gauge**, required only when the kernel's file, read at 1 ms, says a 20 ms poll could not have missed it. | A depth the sampling could not have seen is not evidence either way. `scenario_io::a_burst_of_writes_shows_up_as_a_queue_on_the_device_that_took_it`. |
| A count from a shared file | **Held from both sides by the kernel's own count**: over a window enclosing the session's (ceiling) and over the workload alone inside it (floor). | A ceiling alone passes a collector that answers 0. `scenario_io::a_cold_read_of_a_known_file_is_measured_against_the_bytes_it_had_to_move`. |
| Two program sets on one tracepoint | **Disagree by an absolute skew, measured per run by a witness**, not a relative tolerance assumed. | 120 ns at the median, 11.5 us once; the 2 % tolerance was 115 ns at 5.7 us. `scenario_io::a_histogram_built_in_the_kernel_is_the_one_the_streamed_requests_make`. |
| The user stack a sample copied | **Held to the target's pagemap**, not the mapping's extent. | The kernel copies resident memory only (`copy_from_user_nmi`). `unwind::each_mode_copies_what_it_declares_and_dwarf_costs_more_per_sample_than_fp`. |
| A tool the CI needs | **Installed by the workflow, not excused by the skip gate.** | The gate's own rule for workloads. The bpftool link in `.github/workflows/ci.yml`. |
| A quoted output that cannot be regenerated | **Keeps its numbers and says so**; only what the tool now prints differently is edited, and today's output is given beside it. | A "current" version of a fixed defect's output would be a fabricated record. `docs/scenarios/power.md`. |
| A document's machine numbers | **Carry their commit and date.** | The usable count moves with every probe fix (93 to 92, 57 to 51 between two waves). `docs/install.md`. |

---

## Settled in the seventeenth wave

What was decided before the wave (`docs/briefs/wave-17-programs.md`, "Decided
before this wave") and what the seven streams settled, from their handoffs
(`docs/fixes/w17-{symbols,cases,engine,core,python,build,io}.md`). The three
lines open after the sixteenth wave are among them: the network namespace,
one order for "unbuilt" and "missing", and the GPL-2.0 text.

**Decided before the wave** (the user, 2026-10-03):

| Decision | Choice | Reason |
|---|---|---|
| Phase 12 | **Worth doing**, as data: a case is a directory. Case 6's project is the **sqlite amalgamation**, fetched once and cached outside the repository. | Every ground truth until now was a workload written for the test. |
| Where perf-map symbols land | **First, as their own stream, read under `--symbols local`** (and so `debuginfod`), not behind a new rung. | Without them a CPython case can only check that the verdict says it cannot see; a `/tmp/perf-<pid>.map` is a file the target wrote on this machine and asks nothing of anyone. Landed: `stack_capture::a_python_function_is_named_in_the_profile_under_x_perf`. |
| The GPL-2.0 text (was open) | **Ships**: `LICENSES/GPL-2.0-only.txt`, installed under `share/doc/omn/LICENSES/` by `make install` and packed by `scripts/release.sh`; the README's licence section names it. | `src/bpf` is `MIT OR GPL-2.0`, and an option whose text is absent is half an offer. `make dist-verify`: exactly the nine paths `release.sh list` names (core). |
| The network namespace beside an interface's index (was open) | **Part of the identity**, `iface#<netns>:<index>` (`iface:<netns>:<name>` with no index). A producer that reads in this process's namespace writes this one's; `net.qdisc` writes the device's own, `dev->nd_net.net->ns.inum`, read in the kernel. Another namespace's interface is another key: counted unmatched or uncredited and labelled `iface:<name>@netns:<inode>`, never credited to this namespace's, and not refused, since a `--system` run legitimately sees every namespace's devices. | An index is per namespace and `net.qdisc` reads them all: 2000 of 2000 packets on another namespace's lo were credited to this one's, and are now 2000 uncredited and keyed by their namespace (engine; run by hand and as a test applied and reverted, filed for `tests/join_engine.rs`). `join::tests::another_namespaces_interface_of_the_same_index_is_not_credited_to_this_ones`, `bpf::net_qdisc::tests::an_interface_target_is_compared_with_its_namespace`. |
| One order for "unbuilt" and "missing" (was open) | **The chain's, on every path**: a backend both unbuilt and missing a requirement here is the machine's (exit 1) for the walk, a pin and the key hint alike. *"Valid, and not built" is exit 3* then holds on a machine that lacks nothing the backend needs. **Landed in the eighteenth wave** (`e04ef9a`, `1aa114f` cherry-picked with its eleven tests, and the pinned `net.hwts` twin in `267842a`; "Settled in the eighteenth wave"). | One request exited 1 or 3 by the path that reached it. |

**Symbols** (`docs/fixes/w17-symbols.md`):

| Decision | Choice | Reason |
|---|---|---|
| What a perf map names | **Only addresses in anonymous executable mappings** (unnamed or `[anon:...]`, never `[vdso]` or a deleted file) that no file names, read as the map grows and once more at the end (`Unwinder::finish()`, called by `cpu.profile`'s and `mem.ibs`'s `stop`). | A JIT writes lines as it compiles, and a map written at exit must still name the run. `stack_capture::a_perf_map_written_at_exit_names_the_frames_of_an_omn_profile`. |
| Where a name came from | **Per frame**: `Frame::named_by`, one of `elf`, `debuginfod`, `kallsyms`, `perf-map`, absent exactly when the frame has no name; the per-module `NameOrigin` stays for the facts line. | One module can be named by two sources and one stack by four. |
| A later map line over an earlier range | **Wins**; the overlapped line is dropped. | Code freed and generated again at the same place is the new code. |

**The engine** (`docs/fixes/w17-engine.md`):

| Decision | Choice | Reason |
|---|---|---|
| `proc` | **Has producers**: `proc.fd`, `proc.threads`, `mem.rss`, `mem.pss` and `mem.uss` carry it after `task`, read once at attach; a process whose namespace link is closed to this user carries `task` alone. No eBPF source writes it yet. | A join on one process across containers is served (`proc.threads` to `mem.rss` `via: proc`: 6 matched, 0 unmatched), and an exec keeps the pid namespace. `collect::proc::status::tests::a_count_carries_the_process_after_the_thread`. |
| A drained stall row's enclosing window | **First episode to last end** (`last_ns`, stored by a plain `stx`, exact because the row's key holds the thread id and a thread ends its episodes one at a time), or the drain's close where the kernel recorded no end; a `count` run's rows carry no window. | The 100 ms reclaim session credits 99.5 to 100 % of the cgroup's pgsteal with it, 2 % without (three runs, applied and reverted: the session's line is filed). `collect::mem::stall::tests::a_drained_rows_window_ends_at_its_last_episode_or_else_at_the_drain`, `a_counted_row_carries_no_enclosing_window`. |

**The emit layer** (`docs/fixes/w17-core.md`):

| Decision | Choice | Reason |
|---|---|---|
| A weighted row's sum | **Its exact `total_ns` wins over its bucket bounds** (`emit::SUM_NS_ATTR`): every emit that adds a duration up -- `hist`, `summary`, `rate`, `topk`, `stack` -- takes it where the row declares a weight; the buckets and `min` stay the bucket bounds, and `max` is raised to a row's exact mean so it is never below the mean beside it. | `value * weight` is a lower bound, up to a factor of two short, that nothing marked as one. `emit::tests::a_kernel_built_histogram_sums_what_its_rows_measured_not_their_low_bounds`, `a_stack_total_weighs_its_rows`, `a_kernel_built_row_s_max_is_never_below_its_mean`. |

**The block histogram** (`docs/fixes/w17-io.md`, `docs/scenarios/io.md` defect 19):

| Decision | Choice | Reason |
|---|---|---|
| The in-kernel block histogram's programs | **Their own pair**: the open writes only the time, into an entry that outlives its request and is consumed in place (no insert, no delete on the hot path); the close reads only the device and adds to a per-cpu row, summed once at the end. | The shared pair cost 781 ns a request at 3.16 M IOPS (23.57 %, the coordinator's run before the wave), reading fields no row carries, taking two bucket locks and two atomic adds on a line every cpu wrote. `bpf::io::tests::the_histogram_programs_hot_paths_call_only_what_a_row_needs`. The new cost is not measured yet; the criterion it is held to is open, below. |

**Cases** (`docs/fixes/w17-cases.md`, `w17-python.md`, `w17-build.md`):

| Decision | Choice | Reason |
|---|---|---|
| What a case is | **`cases/<name>/case.yml`, data; extractors a registry in `tests/cases.rs` keyed by name.** A program whose account has a known shape is a directory; one of a new shape adds one extractor beside the tests, never a line under `src/`. | ROADMAP phase 12: adding a program means adding a directory. Shown by five cases added after the format with no line under `src/` and no extractor. |
| A case's account of a new shape, from a stream that does not own the registry | **A script in the case's directory that prints `key=value`**, read by the `key-value` extractor; promoted to a registered extractor by the registry's owner when a second case needs it. | A case stays a directory, its parser beside the reference it reads. |
| How the agent asks | **The question's own session (`--dump-session`), approved into `--allow-launch`, run through `run_session`, read as `docs/mcp.md` says**: the decomposition's biggest row, then the dominant stack of that row's source. | The two ways must ask the same question; the server summarises nothing, so the reading is the model's and is written down. |
| What "the same suspect" means | **The same dominant state, and the same leaf where both are named**; an address is never compared with a name. | A name against an address is a difference in symbols, checked separately. |
| A check a case shows failing | **Listed under `open_findings` with where it was filed; printed, not failed, while it fails; a failure once it passes everywhere it was checked.** | No workaround in the case, and a fix elsewhere is noticed by the case that showed it. |
| A cause the reference says was not there | **A skip with the numbers** (each cause's `holds`). Kept in the case where most machines would make it dominant (`a-cold-file` on a fast NVMe). | That run asked another question; asserting on it would test the machine. |
| A cause the brief named that does not dominate | **Measured, reported with its numbers, and replaced by one that does** (gcc's cold headers: 45 ms of a 0.86 s compile; replaced by a cpu shared with two competitors). | A case holds the triage to its first suspect. |
| A program's account on stdout | **Handed to its stderr by `sh -c 'exec "$@" 1>&2'`, the pid unchanged**; `run_session` carrying stdout is filed. | Both ways read stderr, and the program is unchanged. |
| An interpreter with and without its perf map | **Two cases differing by the one flag** (`cpython-interpreter`, `cpython-perf-map`), helpers shared by symbolic links. | `needs` and `open_findings` are per case, and the two show different findings. |
| The cProfile reference | **A second run of the same program on the same input, used for which function, never how long.** | cProfile triples the run's time; its shares are robust, its durations are not the measured run's. |
| A server as a case's target | **Started on port 0, found and stopped by the case's client** (port from `/proc/<pid>/net/tcp`, SIGINT as at a terminal), its requests on one keep-alive connection. | The client is the load and the clock, and one serving thread alive at the end is what lets its `schedstat` be read. |
| The kernel's account of a task's time | **`/proc/<pid>/task/*/schedstat` of the target and every descendant, read from outside once a millisecond** (`cases/shared/watch.py`), the longest gap between reads its resolution. | The kernel's own split of on a cpu, waiting for one, and the rest, read without touching the target. |
| Cpu contention without touching the target | **Competitors pinned to cpu 0, the target launched with `taskset -c 0`.** | An affinity given at exec is the target's environment, like a cgroup. |
| A durability case's storage | **The case's `{cache}`, checked not to be tmpfs.** | A sync on tmpfs returns at once. |

---

## Settled in the eighteenth wave

What was decided before the wave (`docs/briefs/wave-18-findings.md`,
"Decided before this wave") and what the twelve streams settled, from the
decision lines of their handoffs (`docs/fixes/w18-*.md`). Two lines open after
the seventeenth wave close here: which task the triage decomposes, and one
order for "unbuilt" and "missing", now in the code. The phase 6 criterion
stays open, below.

**Decided before the wave**, each landed by the stream named:

| Decision | Choice | Reason |
|---|---|---|
| Which task the triage decomposes (was open) | **The main thread, then the task that ran in its largest row when that row is a sleep or a futex wait**: a child, in the wait's place, when the blocking stack names a wait for a child (`wait4`, `waitpid`, `waitid`, `wait3`, `wait`) or the child was busy for a quarter of the block; another thread of the process, beside the block, when it was busy (on a cpu or queued) for a quarter of it; a task no source placed in a process, in the block's place, said unplaced rather than guessed. Followed the same way down to eight tasks. No `--thread` option. Landed by triage. | It needs nothing from the person asking, and the three cases it turns are one shape: the program's work in a task its main thread waits on. `triage::a_program_whose_work_is_in_its_child_is_answered_by_that_child`, `..._in_another_thread_names_that_thread_beside_its_wait`; both ways of `make-sqlite`, `gcc-shell` and `http-server-stdlib`'s listing in `tests/cases.rs`. |
| One privilege model for `omn` and `omn-mcp` | **The capabilities go on `omn` alone**, by `make`, by the command `make install` and `scripts/release.sh` print, and by the sudoers line; `omn-mcp` collects through the `omn` beside it (or `--omn PATH`) and is never granted one. `tests/cases.rs` reads the capabilities of the `omn` the person runs and the `answered_by` of every `run_session` answer, and a cause whose two ways differ fails `both-ways-run-with-the-same-privileges` and is not compared. Landed by mcp and cases. | A grant on two files is two things to lose at a relink, and a person and a model asking one question must be answered by one machine; two ways run with different privileges answer from two machines. `release.sh check` compares the two sweeps; `a_run_through_the_server_is_as_privileged_as_omn` shows a `CAP_BPF` run through the server with nothing granted to it (the `CAP_PERFMON` half at paranoid 2 is the coordinator's run). |
| The CI and bpftool (was "the CI installs bpftool") | **Upstream's static build, pinned by version and sha256** (`libbpf/bpftool` v7.7.0), Ubuntu's `linux-tools` as the fallback; the step fails when none dumps `/sys/kernel/btf/vmlinux`. A runner without swap skips the swap tests with that reason. Landed by green. | The azure kernel's `linux-tools` ships no bpftool at all; a static binary has no library to miss, and dumping a BTF blob does not depend on the kernel the tool was built beside. |

**The CI** (`docs/fixes/w18-green.md`):

| Decision | Choice | Reason |
|---|---|---|
| The `defaults` cell's cgroup | **Two delegating levels deep** (`omn-ci`, `omn-ci/app`, the shell in `omn-ci/app/job`), the shape of a systemd user session; the step fails if a level lacks `cpu`, `memory`, `io` or `pids`. | A leaf holding processes can never hand a controller down, so "down to `/omn-ci/job`" is met one level up. The skip itself was two tests making one cgroup name, filed. |
| A test that pushes written pages to swap | **Limits with `memory.high`**, not `memory.max`; a zero-page test may keep `max`. | `max` kills when reclaim waits on a slow swap device; `high` throttles, and a throttled child is judged on what it says it wrote. |
| An ESRCH from a procfs file of a running process | **Not the end of the run**: read again with a fresh open; only a process gone or a zombie ends it. | A file opened on the address space an `execve` replaced answers ESRCH. `wss::tests::an_esrch_from_a_running_process_is_read_once_more_and_never_ends_the_run`; `footprint.rs` is filed. |

**The triage** (`docs/fixes/w18-triage.md`, `w18-names.md`):

| Decision | Choice | Reason |
|---|---|---|
| A followed task's silent row | **Unread for a child or an unplaced task; a zero for the program's own thread** from a source that watches every switch of its threads and lost no event. | A source that does not follow children says nothing about them; one that sees every switch says "never" by saying nothing. |
| `sched.mutex` in the triage | **A second pass**, only when a futex sleep (or, without `sched.futex`, any sleep) is the largest row on the verdict's way; never in the dumped session. | Every backend is `extreme`, and on a program that locks often it doubled the time it was asked to explain; a second run is paid only where the split can change the answer. |
| A source's measured cost beside the verdict | **Said when over 20 % of the target's cpu**, where the catalog's `extreme` band starts; not held against the criterion when the work was in a task outside the launched process. | A source that dilates what it measures must say so beside what it measured. |
| A verdict's room when every row was read | **The rows' own resolution**, not the whole's bracket. | On a loaded machine the program's exit is seen up to 60 ms late, which moves the residue, not a row. |
| What a sample is charged to | **Its innermost perf-map frame where its stack has one, else its leaf**, one rule (`diff::charged_to`) for `omn diff`'s own cost and the triage's hottest function. | Under a perf map the leaf is the runtime's on every sample: `py::score` was under 99 % of them and never a leaf. |
| How a perf-map frame is known | **By reading back the maps the run's names note cites**; a map gone is said, and its samples go to their leaves. | Results keep labels, not provenance; a prefix list of runtimes would guess. |
| A flat interpreter profile | **Said as one, naming no function as the suspect and not saying whose they are**, when the program or a frame module in half the samples is an interpreter, the hottest was charged by its leaf, and it holds under half the samples; with what writes a perf map for that runtime. | The interpreter's own work is in every program it runs; half keeps a C extension that does the work nameable. |
| `--symbols` on the triage | **Set on every request whose collector reads it** (`stack::symbol_readers()`: `cpu.profile`, `sched.offcpu`), on no other; `omn saturate` takes it and says no request read it. | Handing `debuginfod` to a source that refuses it would refuse the whole question. |
| A block's stack | **Leaf first to the program's own frame** -- the innermost perf-map frame, else the innermost in the executable (symlinks followed) -- **4 to 16 frames**. | Four printed none of the Python function, the fifth frame. |

**`omn-mcp`** (`docs/fixes/w18-mcp.md`):

| Decision | Choice | Reason |
|---|---|---|
| `run_session`'s budget | **Optional**, as on `omn`; every run says what a budget is a share of (`budget_against`: the target's cpu, the window, and a floor where the in-target part is unknown). | Requiring one of a model stopped a waiting program on the agent's way only, which made two ways disagree for a reason that was neither the program's nor the machine's. |
| The program's stdout | **Handed back apart from `omn`'s document**, matched byte for byte against `results.json` and never guessed; the whole stream in `program.stdout`, its last 64 KiB in the answer. | A program's own account is often on stdout. |

**The launcher** (`docs/fixes/w18-launcher.md`):

| Decision | Choice | Reason |
|---|---|---|
| A launched program's budget denominator | **Its process tree**: the program's cpu, every live or unreaped descendant's, and what each reaped, read from `/proc/<pid>/task/*/children` and kept at its high water; every descendant, not only those a collector follows. | The reaped children were already in the denominator; a running child out of it while a reaped one is in was the inconsistency. A gcc driver read 8581 % of its own 4 ms. |
| A stop | **Ends the program's tree** (frozen with `SIGSTOP`, then killed) and its own process group when it made one; the program is not put in a group of its own at launch. | A background group stops on its first terminal read, so `omn -- cat` would hang. |
| A run the budget stopped | **Says so on a line of its own**, before the verdicts, naming the source and the percentage; `cost.json` carries `stopped_by_budget`. | A stopped run said its budget "held". |
| A collector that weighs its samples | **Declares it in the registry** (`Factory::weighing_its_samples`); no table names it elsewhere. | A registry is an append seam; a table in `src/main.rs` was one more place to forget. |

**The profile** (`docs/fixes/w18-profile.md`):

| Decision | Choice | Reason |
|---|---|---|
| A collector's per-tick work | **Follows what the tick drained**: a process's maps, executable and CFI are re-read for the processes the drain holds samples of, before their stacks are interned. | Work done whatever the samples is paid by a program that waits as much as by one that burns, and was most of what a profile cost a program that waits (user-only 13.2 to 58.2 % before, 7.1 to 28.7 % after). |
| What paranoid 2 costs | **Said where the choice is made**: the user-only degradation says the samples shrink with the program's user time and the tool's per-tick work does not. | A reader should not infer from a percentage why a profile cost more than its band. |

**The scheduler** (`docs/fixes/w18-sched.md`):

| Decision | Choice | Reason |
|---|---|---|
| A sched wait | **The kernel's own account**: `sched.runqlat`'s waits and `sched.offcpu`'s `queued_ns` read from `sched_info` at the switch (`rq->clock - last_queued`, plus `run_delay`'s growth over the wait); the tracepoints' figure kept as `wake_to_run_ns`. Only these durations come from the scheduler's clock; timestamps stay `CLOCK_MONOTONIC`. | The reference a decomposition is held to is the task's `schedstat`, and the tracepoints parted from it by about 1.5 us a wait, on every wait; now 459.8 ms against the kernel's 459.2. |
| Where a block starts | **At `__schedule`'s entry** (`sched_entry_tp`), where the kernel stops the task's cpu account; kernels before 6.16 keep the switch and say so. | Starting at the switch put the pick's time in the wrong row. |
| Following the target's descendants | **Every sched source with a per-process filter, in the kernel** (`sched_process_fork`/`sched_process_free` and a map), as `cpu.profile` does in userspace. `sched.lock` and `sched.futex` not yet. | cc1's table had no `runnable` row where the kernel says it waited 2 s. |
| `--symbols debuginfod` on `sched.offcpu` | **Read** (`stack::NAMED_BY_CHOICE`); a request with no `DEBUGINFOD_URLS` is refused at attach. | The kernel walks the stack, so where the names come from is the only choice there is. |

**The block layer** (`docs/fixes/w18-io.md`):

| Decision | Choice | Reason |
|---|---|---|
| `io.inflight` | **The larger of the kernel's two in-flight accounts** (`/proc/diskstats`' column and `/sys/block/<dev>/inflight`), each carried. | On the CI's 6.17 the disk's line read 0 under a queue of 199; each is a lower bound, and keeping both visible lets a kernel where they part be seen. Why they part there is open. |
| A collector's stop and its programs | **Detaches the programs and does not close them**, so their run time is readable at the close. | The block programs closed at the stop read "0 runs for 0 ns" beside 26.8 M counted requests. The general form, reading the meters before the stops, is filed. |

**The engine** (`docs/fixes/w18-engine.md`):

| Decision | Choice | Reason |
|---|---|---|
| Where a join rule runs | **Per rule, `tier:` one of `agent`, `query` or `kernel`**, `agent` by default. `query` applies the same engine to a stored run's `raw` events (`join::apply_stored`), reading a detach back from each result's `stopped_because`, and is refused at validation unless both sides are stored in full; `kernel` is refused with the reason until a match kind has a program. | Phase 5 decided placement per rule; a tier that cannot answer what the agent would have is refused rather than degraded. `join_engine::a_rule_moved_to_the_query_tier_answers_from_the_stored_run_what_the_agent_did`. |
| `Btf::member_offset` and a typedef | **Followed, structs first.** | The loader should check what `net_qdisc` wrote by hand, and C names a typedef's members as it names a struct's; no caller's type is shadowed on the running kernel (audit test). |

**The bench** (`docs/fixes/w18-bench.md`):

| Decision | Choice | Reason |
|---|---|---|
| The cost bands | **Follow the bench's table, and a test holds them to it**: 43 corrected to the second table; `tests/bench_bands.rs` reads the record in `docs/bench.md` and fails when a cost line and its row disagree. | The bands are what `--budget` promises in advance, and they had drifted from the measurement by up to two bands with nothing failing. |
| A row's cost | **The larger of the tool's share and the target's dilation**, a dilation within the workload's resolution reading as zero. | An in-kernel collector's cost is in the target (finding 47), and a dilation the gate cannot resolve is not a measurement. |
| A fixed per-run cost | **Has no band**: `io.fsync` and `net.tcp.connect` through `bpf:fentry` keep their bands on their dilation; the ~55 ms attach is still counted by `cost.json`. | A band is a percentage, and a fixed cost is one only once a length is chosen. |
| `free` | **Under 0.1 %, and the perf counters are not under it** (0.13 to 0.20 % in `count`, now `low`). | The band's ceiling is a number (`Cost::worst_case_percent`). |
| An equivalent backend's band | **Never cheaper than what it stands in for**, for backends serving a common target (`tests/bench_bands.rs`). | `--budget` degrades onto what declares less; an equivalent backend is the same measurement, so a cheaper band promises a saving the move cannot deliver. |

**The resolver** (`docs/fixes/w18-resolver.md`):

| Decision | Choice | Reason |
|---|---|---|
| One order for "unbuilt" and "missing" | **Landed** (`e04ef9a`): a backend both unbuilt and missing a requirement here is the machine's (exit 1) on the walk, a pin and the key hint; the key hint ranks a carrier this machine can run and nobody wrote above one it cannot run (`Forceable`, `Unwritten`, `Missing`, `Unfit`), as the walk stops at the first backend it could run. | One request exited 1 or 3 by the path that reached it. Shown with and without the capabilities over the eleven tests. |

**The cases** (`docs/fixes/w18-cases.md`):

| Decision | Choice | Reason |
|---|---|---|
| The agent's reading | **Follows the triage's**: from the main thread's table, a `blocked` largest row is followed into the task busy for a quarter of it, with that task's own events for its stack (`raw` beside `stack`), perf-map frames first, and a flat interpreter profile names nothing. | `docs/mcp.md` promises a model can read a run the way `omn latency` does; a reading of its own would test a third way. |
| "The same suspect" for a cause that names no frame | **The same state, and whether each way names a leaf**; two named leaves are compared only where the case names its `leaf`. | The hottest leaf of a spread profile moves between runs; comparing two samples of the noise failed one run in four. |
| An open finding that alternates | **`sometimes: true`, scoped to a `cause:`**, filed with why and how often, closed by hand. | Listed, its passing runs fail the test; unlisted, its failing ones do; held, the case's other checks stop running. |

---

## Settled in the nineteenth wave

What was decided before the wave (`docs/briefs/wave-19-gpu-view.md`,
"Decided before this wave") and what the fourteen streams settled, from the
decision lines of their handoffs (`docs/fixes/w19-*.md`). Accepted boundary 2
is amended by the first line: the inside of an NVIDIA GPU is served, for a
launched program, through the vendor's own library.

**Decided before the wave**, each landed by the stream named:

| Decision | Choice | Reason |
|---|---|---|
| How CUPTI is reached | **From an injection library**, `cupti/` (`libomn_cupti.so`), a workspace member like the preload shim, named in `CUDA_INJECTION64_PATH` and loaded by the driver inside `cuInit`; CUPTI itself opened at runtime, nothing linked. `gpu.kernel`, `gpu.counters` and `gpu.profile` serve a **launch** and refuse a `pid`, the refusal quoting the entry's caveat. Landed by cupti. | CUPTI's activity and profiling APIs record only the process they are loaded in. |
| Nothing is wrapped | **`gpu.counters` reads CUPTI's range profiler from the injection library**; `vendor:ncu` left the chain. `ncu` and `nsys` may be second readers in a test, never what `omn` runs. Landed by profiler; `nsys` measured once by bench as a reference reader. | Boundary 4: a tool's output parsed is a tool wrapped. |
| Lost records | **Counted and said**: CUPTI's drop count asked after every buffer, unsent batches and records returned unfinished counted, each with its reason, into the run's losses. A flush thread drains every 50 ms. Landed by cupti: with two 64 KiB buffers, 50 000 launches kept 41 263 and dropped 8 737, kept + dropped = launched exactly. | A tracer that drops silently undercounts the device's time. |
| A GPU timestamp's error | **Measured by brackets** (monotonic, CUPTI, monotonic; the tightest of several) at the start, at each flush and at the end; the map is the line through the first and last, its error the widest half-bracket plus the furthest bracket from the line, at least 1 ns, and widened by the slope's error past the last bracket. *Amended by cupti: `ts_err_ns` is the conversion's error only* (below). | `CLOCK_MONOTONIC` is the one clock domain; a converted timestamp carries a non-zero, measured `ts_err_ns`. |
| The device's clocks | **Reported, never changed**: every NVML run states each device's shader clock lock, application clocks, persistence mode and clock event reasons at its start and where they moved at its end, and `gpu.kernel`, `gpu.counters` and `gpu.profile` carry the same in their facts (`timing_clock_facts()`, wired at the merge). A lock is claimed only from evidence; a range in the driver's performance levels reads **"not shown locked"**, never "unlocked"; unknown is never locked. Landed by device. *Since the twentieth wave the run's environment carries it too, when a `gpu` source is asked (gpu).* | Locking needs root and does not survive a reboot; an unlocked clock makes a timing depend on what ran before it; and NVML has no reader for a `-lgc` lock, so "unlocked" would be a confident guess. |
| A counter's reproducibility | **Measured per kernel and per run**, never a fixed list: the interquartile spread over the median of the undisturbed launches, at 10 %, with a verdict per counter (`reproducible here`, `not dominated by this workload`, `measured once`, `disturbed on every launch`) and the outliers beside it. Landed by profiler: `dram__bytes_read.sum` reproducible on a 64 MiB copy and not dominated on a compute spin; instruction counts bit for bit on both. | A full-range spread let one disturbed launch turn a count that repeats bit for bit into one that does not. |
| `gpu.profile` | **A new source: CUPTI's PC sampling in continuous mode**, aggregated per program counter and stall reason per drain, resolved to a source line from the modules the library keeps. Landed by profiler: the known hot loop's line held 95.9 % of the samples. | Where time goes inside a kernel, as `cpu.profile` says it on the cpu; continuous mode does not serialise the program's kernels. |
| A GPU tracer's cost | **Against a negative control**: the injection library loaded by the driver and disarmed (`OMN_CUPTI_REPORT=0`), the control against bare reported apart. Landed by bench: the control inside the floor on every regime. | Without it the dynamic loader's cost is published as the tracer's. |
| Ground truth for the GPU | **Toolkit-free**: PTX driven through `libcuda.so.1` opened at runtime (`tests/gpu_workload.py`, `tests/gpu_inside.py`, `examples/bench/gpu.c`), and a self-timing kernel writing `%globaltimer`, `clock64` and `%smid` per block as the reference `gpu.kernel` and `gpu.profile` are held to. Landed by cupti and profiler. | No `nvcc` to run the suite; a reference independent of CUPTI. |
| The view | **A projection, never a collector**: a view reads only `ctx` -- the event stream, live or replayed -- and never opens a source, reads a file or starts a program. Landed by core and held by every view. | What a view needs that no source gives is a request against that source's owner, not a second collector. |
| Unused, the view costs nothing | **Nothing outside `omn top` changed for it**: no collector, runner path or encoder; `omn trace` never constructs the view's tap. Landed by core. *Reasoned, not measured: the binary's startup against the previous one was not timed.* | The view is an opt-in bonus. |
| Every view runs on a recording | **`omn top --replay <run>` draws any view from a `results.json` or a flight recorder's document as it draws a live one, and `--gallery <run>` steps through all nine.** Landed by core. | All views compared on the same data, reproducible tests, a demonstration that is the same every time. |
| Trust is drawn | **One style, core's** (`view/src/trust.rs`): losses, a time error (`±?` when unknown), low determinism and a live run dim a figure; a late round and a degradation are a badge only; a refused source draws its reason in place. Landed by core, used by all nine. | A figure that cannot be trusted is shown as such where it is read, not in a footnote. |
| One binary carries the capabilities | **The view is a workspace member (`view/`, ratatui and crossterm) linked into `omn` as `top`.** Landed by core. | The privilege model settled in the eighteenth wave stays one file to grant. |

**CUPTI and the channel** (`docs/fixes/w19-cupti.md`):

| Decision | Choice | Reason |
|---|---|---|
| How the library talks to `omn` | **A sequenced-packet `AF_UNIX` socket per launch, in batches of at most 60 KiB, sends bounded by 2 s**; an unsent batch's records, drop and unfinished counts are counted and a name it carried is sent again. *This departs from the brief's shared ring.* | CUPTI hands records over a buffer at a time on a thread that is not the program's, so a send per batch costs nothing per record and needs no mapping set up before `cuInit`; sequenced packets keep a forked child's messages whole; the timeout bounds the program's exit when `omn` stops reading. |
| A forked child | **Reports nothing**: the exit handler and every send check the pid that initialised. | A child forked after `cuInit` cannot use CUDA and holds the parent's locks without its threads; flushing there hung the child for good (measured). A child that execs loads the library afresh. |
| `ts_err_ns` on a GPU event, and a duration's ground truth | **`ts_err_ns` is the clock conversion's error, not CUPTI's placement of the device's work nor the dispatch and drain inside a duration**; a kernel's duration is held to `span <= duration <= span + max(10 us, 1 %)` of its own `%globaltimer` span, and its times to containment in the host's launch window. | Measured, the difference is 1.0-1.3 us against a 68 ns calibration: block dispatch and drain that `%globaltimer` inside the kernel cannot see. Folding a declared microsecond into `ts_err_ns` would make every event's error a guess. |
| A refused target's reason | **Comes from the entry**: the validator quotes a caveat naming the flag and saying "refused", before the generic "declares no support". *Superseded: the reason lives on the `targets:` line since the twenty-first wave, and the caveat search is gone.* | The catalog is the data; a reason written per source in the validator is code per source. The stricter `targets: {pid: "no: <reason>"}` form is open for review (`docs/open-findings.md`). |
| The flush | **Never forced before exit.** | A forced flush returned 482 of 500 kernels with zero timestamps and never redelivered them; latency, a buffer (about 1 000 kernels) at a time, is the price. |
| Several CUPTI sources on one launch | **One socket, shared**, each source adding its `REPORT_*` bit; the inside readers' ops are 13 to 31 and a reader of activity skips them; their unsent records are counted apart. | A program has one `CUDA_INJECTION64_PATH`; one reader must not be billed for another's records. |
| What `gpu.kernel` carries | **Kernels, memcpy and memset**, marked by `op`. | They are device time as much as kernels are. |

**The device** (`docs/fixes/w19-device.md`):

| Decision | Choice | Reason |
|---|---|---|
| A reading the driver had not refreshed | **Counted and said, not dropped and not re-paced**: `driver_sample: repeat` per event and a degradation per run with the refresh observed; only for figures whose repeat can be told from a steady value -- the energy counter by its value, the occupancy by the driver's own sample timestamp. | The driver refreshes energy and draw every 100 ms and occupancy about once a second; a repeat read as a measurement is a figure twice. |
| The default poll | **Stays 1 s**, at or above the energy counter's and the draw's 100 ms and just under the occupancy's 1.0-1.13 s; the occasional occupancy repeat is counted and said. | One cadence for every source; a per-source default would need the request layer. |
| GPU energy | **`--mode count`**; a poll whose counter implies under half the draw read beside it says so with both figures. | Polling the energy counter makes it under-count, for every reader of the board (defect 7 of `docs/scenarios/gpu.md`: 23 W idle read at the ends, about 3 W read every 10 ms). |

**Inside a kernel** (`docs/fixes/w19-profiler.md`):

| Decision | Choice | Reason |
|---|---|---|
| `gpu.counters`' mechanism | **CUPTI's range profiler in kernel replay with automatic ranges**, from the injection library; clocks never locked. | Kernel replay gives a counter per launch without the program's cooperation. |
| A launch's window | **Witnessed by `sm__ctas_launched.sum` against the launch's own grid**: more blocks is a disturbed launch, counted and left out; fewer is a range that is not its launch's. | Measured, another process's kernels added the same 283 million instructions to every launch of a run, so the wrong count repeated and no spread could tell. |
| Pairing ranges with launches | **By order only when a batch's counts agree and no range has fewer blocks than its launch**; any other batch is not reported and counted unpaired. | A range carries no launch's identity, and one range with no recorded launch (a graph's kernel) would shift every later launch onto its neighbour's counters. |
| Counters beside kernel times or PC sampling | **Refused**, before the program runs. *Today at attach, labelled the machine's; the request's exit 2 is filed.* | Kernel replay makes kernel times those of replayed kernels; CUPTI cannot run the range profiler and PC sampling in one process. |
| A PC sampling configuration | **Checked by what CUPTI applied and by its effect**, not by the status code: attribute statuses, the period and mode read back, a run with no sample at all a lost read, CUPTI's sample count held to the samples reported. | The PC-sampling API accepts configurations it does not apply. |
| `_not_issued` stall reasons | **Left out.** | A subset of their reason; counted beside it, a sample counts twice. |

**The GPU bench** (`docs/fixes/w19-bench.md`):

| Decision | Choice | Reason |
|---|---|---|
| Where a GPU tracer is costed | **On GPU workloads, per regime** (launch-bound, compute-bound, memory-bound); a `gpu` source served on a launch alone leaves the backends table. | On workloads that make no GPU call a tracer reads as free (the shim at 0.14 %), and a per-launch tracer's cost moves by two orders of magnitude with the regime. |
| A GPU band | **Its worst regime, named in a caveat**: `gpu.kernel` `extreme` (+67 % of the time and +83 % of the cpu launch-bound, under 1 % otherwise), `gpu.api` `high` (+11.3 %). A cost per regime is open (below). | A band is a worst case (`worst_case_percent`) and the catalog has one per backend. |
| How a GPU row is judged | **On the largest of the share of the target's cpu, the time dilation and the process-cpu dilation**, a dilation under the workload's floor reading as zero; the floor from the IQR (`IQR / 1.349`). | A tracer inside the target can cost it cpu without stretching its time; one held-up repetition moved a standard-deviation floor to 24 % on a series whose IQR was 1.4 %. |
| A GPU workload's threads | **Placed apart**: the launching thread alone on one cpu for the timed section, every other thread on three others, the two reported apart. | On one shared cpu a tracer thread's work is the launching thread's wall time (+74-78 % then, +67 % placed apart). |
| A lossy GPU row | **Refused above one record in a hundred**, every loss in the record as `<lost> of <seen>`. | A tracer that drops records skips the work they cost. |
| The device's clocks before a timing | **Warmed, never set**: 500 ms of spin kernels in the workload. | An idle second slowed a 78 ms section to 119 ms, which a tracer with a slow start would be billed for. |

**`omn top`'s frame** (`docs/fixes/w19-core.md`):

| Decision | Choice | Reason |
|---|---|---|
| The view's crate | **`view/` depends on nothing of `omn`**, reading runs through a field-for-field mirror of `results.json` held to `omn`'s types on every fixture by `tests/view_model.rs`. | `omn` links it, and a crate cannot depend on the crate that links it; a mirror nobody checks drifts. |
| How the view watches a live run | **Through `collect::Escalate`**: an escalation that attaches nothing, hands the events read since the last hand-over to the drawing thread at most every 200 ms, and detaches everything when the view is quit. | The one hook the runner already shows a run in progress through; no collector or runner path changes. |
| A live figure | **A result's `raw` events until the run ends; every figure provisional (`live`) until then.** | Aggregates, losses, cost and stack names are what the runner says at its end. |
| A view's need | **Asked alone** -- validated, resolved, checked for a collector, set aside on a runner refusal before launch -- **and one that cannot be served is drawn as its refusal**; two views asking one source and scope in different modes are refused, naming both; a session file whose joins would lose a request is refused whole. | A source this machine lacks is no reason to show nothing of the others. |
| The view's own cost | **The drawing thread's cpu, beside the collectors' and the target's, as shares of one cpu, and taken out of the collectors' figures** (sampled against `getrusage` at every hand-over). | One unit for the three; without it the collectors read about 450 ms, twice their cost. |
| An aggregated figure's time error | **Unknown unless something saw its events**: `omn top -o` writes each result's worst `ts_err_ns` in `view.json`; elsewhere it is drawn `±?` and dimmed. | Unknown is never zero. |
| Fixtures and snapshots | **Recorded through `omn top --headless -o` by a committed script; every snapshot carries an `.ansi` twin that is compared too.** | Real runs that can be made again; the dimming is the trust style and plain text cannot show it. |
| A view may carry a recording of its own | **In its gallery, when no common fixture holds what it draws** (`lanes-gpu/`, `flows-run/`), recorded through `omn top` by a script beside it and drawn by the view's own test. From flows and lanes. | The common fixtures, and every other view's snapshots, do not move. |
| A view reads a source it did not declare | **Allowed when the run holds it, and it says where the run lacks it** (request reads `io.latency`'s `req`-keyed events). From request. | Its declared needs stay what it cannot draw without. |

**The nine views** (`docs/fixes/w19-<view>.md`; the user's verdict on each is
`docs/view/gallery/README.md`'s last column):

| Decision | Choice | Reason |
|---|---|---|
| system: one time axis | **One box, a section per resource, every chart in the same columns over the same run, each cell the peak of what fell in it**; a finished run's timelines placed on the run's axis by inference until `results.json` records each one's origin. | Side-by-side panels scaled to their own widths cannot line up a disk burst with the cpu that caused it. |
| system: colour | **Trust only**; charts in the terminal's colour. | Where most sources are `low` determinism, the exceptions must stand out. |
| system: zero and unmeasured | **Drawn apart**: a dot and a blank; a table cell nothing measured is `·`, never `0`. | An empty series and a series of zeros say different things. |
| flame: depth | **A chain that never visibly branches is one box** at the current zoom; the detail line says what it folds. | An icicle's depth is then where it branches: 88 frames under CPython fit 40 rows. |
| flame: two runs | **Drawn on the mean of the two, judged by the triage's arithmetic restated** (absolute, 3σ Poisson, losses said); `d` compares shapes, the second run scaled to the first's total. Off-cpu time has no noise model: ±5 % above a 0.25 % speck, said so. No icicle while live. | "The same program doing more" and "a different program" are different answers. |
| ledger: energy per process | **Machine joules times the target's share of the machine's busy cpu, in italics, labelled an estimate.** | RAPL counts a package; an unlabelled split would read as a measurement. |
| ledger: every line's price | **What knowing it cost `omn`** (attributed plus in-target cpu, `≥` where unknown), per line and in total. A line with nothing of the focused task says so, never zero. | On `http-server` the I/O line cost over twenty times the time it measured. |
| request: states inside a request | **By share, never in sequence**; only what was timed is placed in time (the network before and after, each block request). What sets a request apart is each state against the median of its siblings', above a twentieth of its span. | `net.request` keeps sums per state, so an order would be invented; on the vertical case the rule names every injected cause. |
| timebar: the suspect | **`omn latency`'s rule restated in the view crate**, held to the triage itself by `tests/view_timebar.rs`. A run without a decomposition by task is drawn as the view's own per-thread reading, said so. A per-process cpu total is a bracket per thread, never apportioned; `blocked` without `sched.offcpu`'s events is drawn whole. | A suspect by another rule would be a second answer to one question; a share no source attributes was not measured. |
| heatmap: what it draws | **`raw` events only**; a kernel-bucketed row at `total_ns / weight`, spread over its drain window as its collector stamps it; a whole-run kernel row not drawn; log2 rows, logarithmic density; a loss shaded only where the run can place it. | A view that drew `hist` at the end and events while live would change shape when the run ended; a point at a row's stamp claims a moment the kernel never measured. |
| lanes: states | **Rebuilt from instants** (switch-out to wakeup is a block), `sched.offcpu`'s windowed rows only labelling them; a GPU lane lines up with host threads in time, never by attribution; the cursor opens on a wait for the GPU. | A windowed total spread over columns draws a precision the source does not have; `gpu.kernel` is keyed on the process. |
| wake: chains | **Through each waker's latest wakeup before the one it issued, folded per thread into a tree**, two times per link (the waker's hold, the woken thread's quiet as an upper bound), stopping with its reason at an interrupt, a cycle, an unseen waker or the depth. | The waker's previous wakeup is the only link the source holds; an unseen waker on a launch target is the source's scope, not an origin. |
| flows: the kernel layer | **A junction, never edges**: the target's bytes left, the machine's right, on one scale, the ratio drawn and said; a ribbon is a sum of call rows, never a multiplication. | No source ties a socket to an interface or a file's bytes to a device. *Since the twentieth wave `net.socket` keys its route's interface and `io.fs` carries its block devices; the view does not draw those edges yet.* |

---

## Settled in the twentieth wave

What was decided before the wave (`docs/briefs/wave-20-left.md`, "Decided
before this wave") and what the fourteen streams settled, from the decision
lines of their handoffs (`docs/fixes/w20-*.md`). The two decisions the brief
asked to be proposed with numbers -- the budget's first check and a cost per
regime -- are still open, below, with the numbers the streams measured.

**Decided before the wave**, each landed by the stream named:

| Decision | Choice | Reason |
|---|---|---|
| The nine views | **All kept** (the user, 2026-10-04, after using them); the gallery's `Keep?` column says "yes (2026-10-04)" on every row. Landed by record. | Each shows something no other view does (`docs/view/gallery/README.md`). |
| The views are linked, and a process has a page | **What is selected opens the view that explains it, and back; a tenth view, `process`, is the first screen of a run with a target**, a tile per view's essence, a number and a colour each, each tile opening its view; `system` stays the first screen without one. Landed by view (the links, the history, the first-screen rule) and process (the tiles). *Both landed in the twenty-first wave: the ledger's blocked line opens `timebar`, and `process` is first on a live `--pid` or launch. The tiles were replaced by one line per resource ("Settled in the twenty-first wave").* | One process at a glance, and every figure one key from its detail. |
| The view is clickable | **A view reports the regions it drew; a click selects, a click on the selection goes in, the wheel scrolls or zooms, a right click comes back; mouse capture on by default, `M` turns it off; keys stay complete.** Landed by view, every view's regions, and record (a snapshot per view after a click). *Reasoned, not seen: a real terminal and tmux.* | The links make the mouse cheap; capture takes the terminal's own selection, so it must be one key away. |
| Every view has a workload | **One program per view and one for `process`, each with a truth manifest** (`view/workloads/`); the sources held to it (`tests/view_workloads.rs`, workloads) and each view held to it on screen (`tests/view_truth.rs`, record); the recordings of these programs are fixtures beside the realistic ones. | Ground truth for a view, as for a source, and fixtures a re-recording redraws within their measurement (shown on a second recording). |
| A field, not a sentence | **What a collector says that a program would read becomes a field of the result.** Landed for `sched.wakeup`'s names and interrupt, `net.socket`'s interface, `io.fs`'s block devices, `row_stamp` and the drains' drops. *`net.request`'s counts since the twenty-first wave (`Results::fields`).* | A sentence a view parses breaks on a rewording. |
| The comparison with the reference tools | **The next wave**, not this one. *Done in the twenty-second wave ("Settled in the twenty-second wave"), its pairs not yet run through the shared runner.* | One wave with nothing else pending. |
| The open decisions | **Stay open unless a stream's measurement settles one.** Both were proposed with numbers; neither is settled (below). | A decision is taken on a measurement, not on a deadline. |

**The GPU** (`docs/fixes/w20-gpu.md`):

| Decision | Choice | Reason |
|---|---|---|
| A target the tool killed | **The tool's failure, said as such, never only a lost read**: an injection library says when its exit path starts (`OP_EXITING`), and a death after that is reported in a `degraded:` line naming the source. For every injector, not only CUPTI. *The exit status: non-zero since the twenty-first wave (`Collector::failure`, settled there).* | A crash the tool caused, read as the program's, is a silent wrong answer. |
| Counters beside kernel times or PC sampling | **The request's refusal, exit 2, before the launch**, decided on the backends the resolver chose (`launch_refusals`); `omn validate` of such a session says valid, since it does not resolve. *Amends the nineteenth wave's "at attach, labelled the machine's".* | A rule about what serves the requests is decided once they are resolved; a pair served by another vendor is not refused. |
| A refused target's reason | **In the catalog's `targets:` line** (`targets: {pid: "no: <reason>"}`), the loader refusing a `no` without a reason; until the loader reads it, the caveat search stays. *Amends the nineteenth wave's "from a caveat". Landed in the twenty-first wave (stream data, `1b7773c`): the loader reads it, and the caveat search is gone.* | The reason sits on the axis it explains and is data the loader can hold present; a reworded caveat today degrades a refusal to "declares no support" silently. |
| A lighter CUPTI activity kind as a fallback | **None**: the other kernel record serialises the program's kernels. `omn`'s 1.4 to 1.8 us a record is its event model's copies and allocations, not CUPTI's. | A fallback that changes what is measured is not a degraded equivalent. |

**The profile and the unwinder** (`docs/fixes/w20-rings.md`):

| Decision | Choice | Reason |
|---|---|---|
| A sampler's losses per ring | **Every loss the run counts for a ring, drops and throttles, reported on that ring.** | A placement that needs all of them is refused by the one missing: the runner test was red for two waves on the throttles alone. |
| A test of shares or paths | **Samples at a period under its workload's shortest phase, or on a workload whose phase is decorrelated from the period.** | One sample per loop iteration aliases: 229 of 299 samples in one branch at the batch's own period. |

**The triage** (`docs/fixes/w20-triage.md`):

| Decision | Choice | Reason |
|---|---|---|
| A limit's spelling | **The JSON spells it as the text does** (`cpu-queue`, `other-block`); the old spelling is still read. | Every reader had a normalization step. |
| A sample in an eBPF program | **The tool's cost, never the program's hottest function**: set aside, counted and said. | A durability case named `bpf_trace_run2`. |
| A thread blocked on a connection beside an empty block | **Followed (`waiting`)**, recognised by its stack's send and receive frames; an idle helper asleep beside it is not. | `http-server-stdlib`'s slow client: the request's wait is the serving thread's send, not the main thread's `select`. |
| A job's wait | **Followed into one child, with the rest of the job beside it** (`followed.job`: every other busy task off the way); the job as one decomposition waits on `ppid`. | Without `ppid`, "the processes under it" can only mean every process off the way. |
| A lossy lower row | **Undecides the verdict when it could overtake the largest at its source's loss rate** (without bound when the loss is throttling). | A lossy profile under load named the run queue for a spin. |

**The budget and the run's account** (`docs/fixes/w20-budget.md`):

| Decision | Choice | Reason |
|---|---|---|
| A one-time cost the tool can name | **Weighed apart from the rate and said apart** (`overhead.one_time_cpu_ns`: a symbol table loaded inside the window), outside the percentage and every check, inside what the tool spent. *The first half of the open line below.* | A debuginfod profile's first check fell from 142-152 % to 45-51 %, its whole window from 36-39 % to 16-17 %. |
| A launched program's stderr | **Can be a file of its own** (`OMN_PROGRAM_STDERR`), not passed on to the program; `run_session` always asks for it (`program_stderr`). The `--program-stderr` flag landed in the twenty-first wave. | An agent reads the program's account apart from `omn`'s lines. |
| What a run leaves behind | **Nothing it launched**: `omn` is the subreaper of what it launches, and the end of every run -- a stop, or the program exiting on its own -- ends and counts what it adopted; a library caller is not and does not. A daemon meant to outlive the run is launched outside `omn`. | An orphan held `omn`'s stderr open and a run through `run_session` hung 71.7 s, unsaid. |
| An adopted orphan's cpu | **In the launched program's denominator while it lives**, as the rest of its tree is. *Decided by the eighteenth wave's rule (the denominator is the process tree); the meter's change landed in the twenty-first wave (stream run, `1c15958`), with numbers.* | It is the program's work, and the budget weighs the program. |
| A session refused at resolution | **Names every request it cannot resolve**, and exits with the first gap to fix (request, then build, then machine). | One refusal at a time is one run per refusal. |

**The collectors** (`docs/fixes/w20-{sched,net,cpu,io}.md`):

| Decision | Choice | Reason |
|---|---|---|
| An interrupt's wakeup | **Names the task it was served in (`served_in_tid`), never as the waker**, beside the context; `irq` or `softirq` from four programs on the interrupt tracepoints. Timer and IPI wakeups carry no number. | For a softirq served in `local_bh_enable` that task is the cause; for an interrupt that only landed on it, it is not -- the context says which. |
| `sched.offcpu`'s stamp | **At the drain that closed its window, said in the caveat**; the blocks it adds up ended in `(ts_ns - window_ns, ts_ns]`. Open only if a join needs the first wait. | The views already read rows as ending at `ts_ns`. |
| `net.socket`'s interface | **The route's, read at the call, in this namespace only**; a socket in another namespace carries none. | Another namespace's index would name some other interface here. |
| A byte counter's direction | **An attribute today**, carried to the results by `raw` (`net.bytes`, `io.bytes`). *Superseded in the twenty-first wave: a key, `dir`, splits the series when a request asks it.* | Re-keying a series breaks every reader of it. |
| A collector's run-level counts | **Fields of the result**: a `Collector::fields` hook, default empty, copied into `Results::fields`. *Landed in the twenty-first wave, `net.request`'s four counts first.* | "A field, not a sentence". |
| `cpu.oncpu_time` per thread | **Through `sched_stat_runtime`, not `sched_switch`**: the scheduler's own increments, credited every tick, so a thread never switched out is not invisible; exact against `schedstat` over a window whose edges find it blocked. `poll` only; `trace` (a slice an event) unbuilt until a view asks. | The `sched_switch` version read nothing for a pinned spinner. |
| A filesystem's block devices | **An attribute (`blk_devs`), not a key**: a set, as `io.bytes` keys each device. | A btrfs spans several devices; nothing has a set-valued key. |

**The shared layers** (`docs/fixes/w20-shared.md`):

| Decision | Choice | Reason |
|---|---|---|
| An equivalent declares no cheaper band than what it stands in for | **Held by the catalog loader**, beside the other chain invariants. | A `--catalog <dir>` that broke it loaded; the test read only the embedded catalog. |
| Which rules `omn join` applies | **Every rule in the file, whatever its `tier:`**; a `kernel` one is refused by `apply_stored`'s own check. | Over a stored run every rule is at the query tier, and its answer is the agent tier's (shown). |
| A rule `omn join` cannot answer | **Refuses the whole command, exit 2, with the reason.** | An invalid combination is refused, never degraded silently. |
| An ESRCH from a procfs footprint read | **Read once more while the process runs; the end only for a gone or zombie process.** | The ESRCH across an `execve` is not the process ending; `wss.rs` already read it so. |

**The views** (`docs/fixes/w20-{view,workloads,record,process}.md`):

| Decision | Choice | Reason |
|---|---|---|
| Going in | **A click selects, a second click on the selection goes in; no double-click timer.** | crossterm reports no double click, and a timer would make the snapshot harness depend on time. |
| The tenth view | **Reached by `0`, its tab labelled `0`**; `1` to `9` keep their views. | No habit and no test changed. |
| `⌫` | **The frame's while there is a history, a view's otherwise** (the flame's zoom out, also on `u`). | Back is the link's other half. |
| The process view's needs | **None of its own**: its tiles read what the other nine ask. | Adding it changed no live session. |
| A time error's dimming | **Only at a resolution it can move** (a tenth of a cell); elsewhere a figure dims as before. | ±295 ns on 27 ms cells dimmed every lane alike. |
| How a drained source stamps its rows | **Said by the collector (`row_stamp`), or nothing**: absent is not read as "at its own event". | A reader then falls back to what it knows. |
| A drained collector's drops | **Placed at the drains that read them** (`drains: [{ts_ns, dropped}]`, summing to the run's); a polled collector's read once, at its stop. | A clone of every polled collector's losses each round for a count almost always zero. |
| A view workload's parameters | **Read from its manifest**, the truth and the program's input at once. | One place to change a figure. |
| Threads that must not wait for each other | **Run in turn, or in separate processes.** | Two Python threads burning at once wait on the interpreter's lock: 781 ms off the cpu nobody chose. |
| A polled source in a workload | **Gets a lead and a tail** (0.3 s and 0.6 s at a 250 ms poll), declared in the manifest. | A poll's first read is its baseline, and a poll after the exit cannot read the target. |
| An exact figure | **Held exactly unless the source counted a loss, then as a lower bound.** | A counted loss is said, and the figure is what is left. |
| A view's truth | **Read off its screen**, not its model. | The screen is what the user reads; a rewording fails with a clear message. |
| The workload recordings | **Fixtures like the others**: every view drawn on every workload (22 MB in the tree, `system-wakeup` cut to one second). | Each view on a load known in advance, beside the realistic ones. |
| A machine run | **Claims no target**: a result metered on the cpus is the machine's account in the ledger and the cost corner. | A per-thread `cpu.oncpu_time` keyed every task, and the corner read "target 753 % cpu". |
| A process tile's colour | **The suspect's state, on the tile that explains it; a tile with nothing to read is never marked.** | Never a threshold. |
| A stack in a tile | **Charged to its two deepest frames of own code, caller first** (interpreter frames when present). | A shared helper does not hide whose work it was. |
| A program that declares no request | **Not an error on the process screen**: the tile says it dim, with the refusal's reason. | Most programs declare none. |

**The documents and the cases** (`docs/fixes/w20-docs.md`):

| Decision | Choice | Reason |
|---|---|---|
| `CATALOG.md`'s Cost column | **The YAML's chain, written out**, in chain order; the YAML is the authority. Held by a test since the twenty-first wave (`tests/cli.rs`). | The column drifted after the eighteenth wave. |
| The cases' budget check | **Gone rather than unchecked**; it comes back with a ceiling the case states if a budget is wanted. | A check that cannot pass is noise in every report. |

---

## Settled in the twenty-first wave

What was decided before the wave (`docs/briefs/wave-21-seams.md`, "Decided
before this wave") and what the seven streams settled, from the decision
lines of their handoffs (`docs/fixes/w21-*.md`). One open line is settled
(a byte counter's direction, by the brief's key); the adopted orphans'
denominator, which the brief asked to be proposed with numbers, landed with
them. The other open lines stay open; no stream measured them.

**Decided before the wave**, each landed by the stream named:

| Decision | Choice | Reason |
|---|---|---|
| The `process` view | **The system view for one process** (the user, 2026-10-04, after seeing the tile grid): one header, one line per resource -- CPU, Memory, Disk, Network, GPU, Energy, Time -- each a sparkline over the program's life, its current value, mean, peak and share of the machine, the time line split by state with the suspect `omn latency` names; `Enter` or a click opens the line's page (CPU `flame`, Disk `heatmap` or `flows`, Network `flows`, Time `timebar`, the header's cost `ledger`). The first screen of `omn top -- <program>` and of `--pid`; `system` without a target. The tile grid is gone. Landed by process, and run (the first screen). | One process at a glance; the detail lives in the dedicated pages. |
| Usage colour | **By how full the resource is**: green free, through yellow, to red full, by the share of a capacity that exists (cores, memory, a GPU's engine time and VRAM); a resource with no known capacity keeps one neutral hue and says why. Trust never washes the hue out: a figure with a loss, a time error or low determinism keeps it and carries its mark (underlined, its badge). Landed by views (`trust::usage`, `system`) and process (its own three-step copy until it calls `trust::usage`; "What remains"). | A measured fraction of a known bound, not a chosen threshold; an honest screen is not a grey one. |
| A collector can fail the run | **`Collector::failure()`**, read after `stop`, carried through the run, a detach and a recording; `omn` writes every result first, then exits 1 with the sentence (`measure`, `record` and `omn top` alike). A lost read stays a loss. Landed by run (`9f65296`, `ce5cde2`); the GPU's inside readers answer it from `died_in_exit`'s condition. | A script reading exit 0 takes the tool's failure for the program's clean run. |
| A collector can publish fields | **`Collector::fields()` copied into `Results::fields`**, serialized only when not empty. Landed by data (`ec2b229`); `net.request` publishes `never_ended`, `unborn`, `replaced`, `filtered_out` (collectors, `f847a6d`), which the request view reads in place of sentences (views). | "A field, not a sentence". |
| Direction is a key | **`KeyKind::Dir`** over a closed set (`read`, `write`, `discard`, `flush`, `rx`, `tx`); `io.bytes`, `io.iops`, `io.merges` and `net.bytes` on `fs:proc` give one event per direction under `--keys ...,dir`, and the one event they always gave otherwise. Landed by data (`2439ba8`) and collectors (`c7adc61`). *Settles the twentieth wave's open line.* | Every aggregating emit gets the split, and no reader of today's series moves. |
| An adopted orphan's cpu | **In the launched program's denominator while it lives**, read as `omn`'s live descendants plus its children account when `omn` adopts; `omn`'s own helpers (`curl`, a container runtime's `inspect`) set aside. Landed by run (`1c15958`, `c1ba7ba`), with numbers: a 5 % budget moved a 1 % request onto a worse backend in 3 runs of 3 before, 0 of 3 after, for about 2 ms of the tool's cpu over a 2 s run when an orphan exists and none measurable otherwise. | It is the program's work, and the budget weighs the program. |

**The run's outcome and its surface** (`docs/fixes/w21-run.md`):

| Decision | Choice | Reason |
|---|---|---|
| A collector that broke its program | **Fails the run after the results are written**: exit 1 with the sentence on stderr; the results, their lost read and their `degraded:` line stand. | What was read before the break was read. |
| `--program-stderr` beside a command that launches nothing | **Refused, exit 2, with the reason**, as any flag that cannot mean anything is; a `-f` session is allowed, since it may launch. | An invalid combination is refused, never degraded silently. |

**What results carry** (`docs/fixes/w21-data.md`):

| Decision | Choice | Reason |
|---|---|---|
| A collector's fields | **Counts** (`BTreeMap<String, u64>`), by name, absent when empty. A field that is not a count would need another type, and none was asked. | What a caveat says in a sentence, as a number a program reads. |
| Direction | **A closed set**: a new direction is a code change. | A typo in a collector cannot invent a series. |
| A stack row's series | **Named only under `--keys`**: without keys the table is the one every reader has always read; with keys a flame is rooted at the series, one tower per series. A difference (`omn diff`) sums the series back before comparing. | Two runs' tids differ; their stacks do not. |
| A refused target's reason | **On its `targets:` line**; an entry-level bare `no` is a loader error. A backend-level `no` still needs no reason: it narrows a chain, it is not what a refusal quotes. | An absent kind already means `no`, so a written one exists only to say why. |
| What stack labels cannot say | **Travels beside them** (`named_by`, `perf_maps`, `executables`), never inside `Results::stacks`. | No reader of `stacks` moves. |

**The collectors** (`docs/fixes/w21-collectors.md`):

| Decision | Choice | Reason |
|---|---|---|
| A request's `rx_bytes` | **From its first byte's arrival**, not its birth; `tx_bytes` from its birth. | A server declares a request once it has parsed it, so a birth-to-end count reads 0 for most requests. Shown exact against the client's request lines and the worker's `sent=`. |
| A byte count whose start was not seen | **Absent, not zero** (two net flags), as `net_rx` already is. | A zero is a measurement. |
| A polled counter's baseline | **Taken before the release**; the runner's immediate first tick passed over rather than reported as a microsecond interval. | The program's first I/O belongs in the first interval. |
| `cpu.oncpu_time` on `fs:proc` at a launched program's exit | **The interval the reap took away is one counted `failed_reads`, with its reason**, until the runner can read the zombie. | A loss is counted and said, always. |

**The views** (`docs/fixes/w21-{views,process,record}.md`):

| Decision | Choice | Reason |
|---|---|---|
| The usage gradient | **Six even steps of the share of a known capacity, in the 256-colour cube**, none near white or black; no capacity means one neutral hue and a sentence, never a guessed bound. | Readable on a dark and a light terminal and in 256 colours (chosen for it; not yet looked at by the user). |
| A hued figure under a trust mark | **Underlined, not dimmed**, its badge beside it. | Trust must not wash the colour out. |
| A futex wait in a sleep | **Taken out of the sleep, up to the sleep**, and drawn as a lock beside the rest. | The old half-coverage rule named a whole sleep a lock or nothing. |
| The `process` view's time axis | **The program's life** (`lifetime`); the run's window when there is none. | The lines describe the program. |
| A recording's capacity | **None**: drawn in the neutral hue, said, rather than with the viewing machine's. | Another machine's cores are a guess about this one. |
| One process's energy | **An estimate, live only**, by its share of the busy cores, said `estimated`; the machine's otherwise, said so. | No source measures one process's energy. |
| omn's cost in the `process` header | **The frame corner's figure**, not a share of the program's cpu. | Two figures for one cost would disagree. |
| A recording that breaks a view test | **Recorded once more before the test moves**; a test that assumed an old recording's shape builds the shape it needs. | Two of three such breaks were the machine (a cold page cache, sampling noise); the third repeated exactly and is a finding. |

**The documents and the bench** (`docs/fixes/w21-docs.md`):

| Decision | Choice | Reason |
|---|---|---|
| A replaying GPU arm | **Measured at its own size** (2000 launches), against a bare run and a control of that size, never against the 50 000-launch baseline. | Otherwise it costs two hours or is compared across two sizes. |
| A case's job | **Added to a row only where the case says the wait is for a child**; on the MCP way it is every busy task off the chain, an upper bound, so `gcc-shell`'s finding closes on a loosened comparison, not on a change in triage. | The MCP way cannot tell a child's tasks from a thread's. |
| A plan the vertical server cannot hold | **Refused, exit 2, with a sentence.** | Silent truncation is a degradation nobody is told about. |

---

## Settled in the twenty-second wave

What was decided before the wave (`docs/briefs/wave-22-replaces.md`,
"Decided before this wave") and what the twelve streams settled, from the
decision lines of their handoffs (`docs/fixes/w22-*.md`). No open line is
settled: no stream measured one. One is opened (below, "Open"): how a pair
reads a value `omn` computes, which the families each answered their own way.

**Decided before the wave**, each landed by the streams named:

| Decision | Choice | Reason |
|---|---|---|
| What the tools are | **Witnesses, never what `omn` runs on**: nothing in `src/` calls `perf`, `bpftrace`, `strace` or any other tool; they run only in `tests/reference.rs` and its families. Held: no file under `src/` changed in the wave. | The rule "do not wrap `perf`" (above, the GPU family's "a vendor library is called, not a vendor tool"): a tool's text output is a format nobody promised to keep, and a second tool is a second clock. |
| A pair | **Data**: one file per family, `catalog/references/<family>.yaml`, read by the test only (the catalog loader does not embed it); the schema in `catalog/references/README.md` (reference). 117 pairs in eleven files. | Adding a comparison is adding an entry, as adding a source is. |
| The parsers | **Code, one module per family** (`tests/reference/<family>.rs`, `parsers()`), each unit-tested on a captured output under `tests/reference/captures/<family>/`. A file written from a tool's documentation is named a format fixture (`<tool>.format.txt`), never a capture (bpftrace, heap, after review). | A parser checked against numbers dressed as the tool's checks nothing. |
| Same execution | **Where both can, one run of the workload seen by both**; otherwise alternate runs, said in `why`. Landed as `run: attach` in the runner, and by the families' own forms (below). | Twelve streams load the machine: a tolerance across two runs is partly noise. |
| The verdict | **A table in the repository**, `docs/replaces/<family>.md`, one row per tool and use, each row backed by a pair or `no pair` and why (`every_row_names_a_pair_or_why_none`); `docs/replaces.md` the index and the summary across families. | A verdict anyone can rerun. |
| A missing tool or privilege | **Skips, with the machine's reason**; the command that installs or grants it in the handoff, run by the user after the wave. | A skip is not a pass and is never silent. |
| A gap in `omn` | **Filed, not fixed, this wave** (`STATE.md`, "What remains", item 1). | A comparison that edits what it compares proves nothing. |

**The runner** (`docs/fixes/w22-reference.md`):

| Decision | Choice | Reason |
|---|---|---|
| `omn`'s exit 1 | **A skip, quoting `omn`'s reason, only when `omn` wrote nothing and the workload succeeds alone**; a failure when `omn` wrote its results first, when an attached workload failed on its own, or when the workload fails with no observer. Exits 2 and 3 fail; a use `omn` cannot serve is declared with `gap:`. | Exit 1 is the machine's half and the run's half at once; only the first is a skip. |
| A tool's non-zero exit | **A skip only when its stderr carries a refusal wording and the workload succeeds alone**; any other parse failure fails the pair. | A workload's "Permission denied" is not the tool's refusal. |
| Several matches of a path | **Summed**, so per-thread series add up to the process. | The process is what the tools report. |
| The tolerance | **The larger of `rel` and `abs`.** | A count near zero needs an absolute floor, a large one a relative band. |

**How the families compare** (`docs/fixes/w22-{perfstat,perfrecord,perfsched,bpftrace,sysstat,top,heap,net,power,gpu,io}.md`):

| Decision | Choice | Reason |
|---|---|---|
| A machine-wide tool | **Observes the same seconds as `omn`**: the tool wraps `omn` (perfstat's `--system`, `--cgroup`, `--pid`; power), or both run alongside the workload (io, net, sysstat, bpftrace's `execsnoop` and `tcplife`), the asymmetry of the two windows said in `why`. | `omn` refuses `--system` beside a launched program (a gap, filed), and separate system-wide runs differed by 2.5x on cycles under the wave's load. |
| A tool that takes a pid | **One run**: a launcher starts the workload and hands its pid to the tool and to `omn` (top's `onpid.py`); a pid-attached count compared per on-cpu nanosecond, not raw (perfstat). *Superseded in the twenty-third wave: `run: attach` starts the workload stopped before its exec, `onpid.py` is gone and the count is compared raw (below).* | Two windows that differ by `omn`'s start and exit are not one count. |
| A tool that launches its own child | **Two runs, quantities the workload fixes compared exactly**, latencies by median within one log2 bucket (bpftrace); heap's interposers and valgrind on alternate runs of a workload deterministic to the block. | bpftrace's `-c` child, valgrind's malloc and two `LD_PRELOAD` interposers cannot share a process. |
| A total-printing tool | **Read before and after the workload** (`around.py`), never two runs subtracted (net). | A counter's increase over one run is the measurement. |
| Units | **The parser converts to `omn`'s units and names** (ns, bytes, ratios, `[memcpy HtoD]`); a pair never converts beyond a declared scale. | One place for each conversion. |
| A count the tool could not make | **Absent, never zero** (perfstat's `<not counted>`, every perfsched parser refusing an empty output). | A zero is a measurement. |
| A pair that misses on a filed defect | **Stays, marked `known_defect`**, reported KNOWN, and failing once it agrees (sysstat's `pidstat -w`); a defect that shows in some runs only is held by its own test instead (the poll's leading event). | A pair that alternates between KNOWN and agreeing is a flaky test, not a hold. |
| Where a tool is not a reference | **Not compared**: `strace`'s per-call latency on short calls (16 us under ptrace against 0.25), nsys's host time per driver call, `gpu.profile` against ncu's source counters. | The tool's own cost is inside the number. |
| Tool options fixed for every pair | `strace -c -w`; valgrind `--run-libc-freeres=no`; heaptrack `--record-only`; `turbostat --no-msr` (unprivileged through perf at paranoid at most 0); C workloads built once without PIE into `target/reference/`; workloads named at exec (a symlink), not by `prctl`; files under `target/`, never `/tmp`; fio with `thread` until `io.latency` follows forks. | Each is what made one side measure what the other does (each handoff says why). |
| `git stash` in a worktree while other streams run | **Never**: the stash list is shared by every worktree; two streams' pops crossed and a review fix reached `main` only at the merge (bpftrace, `bc9f9a5`). Use a commit or another worktree. | Lost work, found only by the full suite. |

---

## Settled before the twenty-third wave

- **A reference pair reads a value `omn` computes through a parser of
  `omn`'s output.** The runner reads one dotted path into `omn`'s JSON,
  summing what `*` and filters match; most families' values are not at one
  path (a series total is a mean times a count, a function's share is its
  leaves summed over the stack table over the total). The twenty-second
  wave's families answered three ways -- a jq expression, a parser in the
  family's module, and five path grammars of their own -- and the runner
  could run none of them. Settled: `compare.omn_parse: <name>` names a
  `Parser` in the family's module, beside the tool's parsers and
  unit-tested on a captured `omn` output the same way; the dotted path stays
  for a value `omn` holds at one place. Code beside the tool's parser keeps
  both sides of a comparison reviewable in one file and tested on real
  output; jq would add a dependency to the test and a second language to
  read, and per-family grammars are what left 105 pairs unrunnable.

## Settled in the twenty-third wave

From the decision lines of the thirteen handoffs (`docs/fixes/w23-*.md`). No
open line is settled and none is opened: the five below, under "Open", stand
as they were.

**The runner** (`docs/fixes/w23-runner.md`):

| Decision | Choice | Reason |
|---|---|---|
| A field the runner does not read | **Refused per pair**, named by `every_pair_is_well_formed`; the family's file still loads, its other pairs run, its rows resolve. Prose goes in `why`, `use`, `gap` or a comment. | A field that is ignored is a pair that runs as something it does not say. |
| `alongside` | **A name for `attach` with no `{pid}`**: the same start-up, settle and wait; `{pid}` refused, the workload optional. | One mechanism, two names for what a reader means. |
| `tool_wraps_omn` | **Requires `-o {out}`**; a tool counting through inherited perf events does not wrap the capable `omn` (its events end at the exec of a non-dumpable binary, and it reads 0). | Two outputs on one stdout; and a count of 0 that is not a skip. |
| `omn_launches_tool` | **`exec`s the tool**, so `omn` watches the tool's own pid; the tool's exit status read through a pidfd (`PIDFD_INFO_EXIT`, Linux 6.15), a non-zero status failing the pair unless its stderr refuses, a kernel that cannot tell skipping it. | `omn` exits 0 whatever its program did; an unknown status never counts as success. |
| One-sided and log2 tolerances | `side: above`/`below` **allows nothing on the other side**, its `why` saying why `omn` can only be on one; `log2_buckets` stands alone, and a value not above 0 is in no bucket. | A lost median (0) must never hold against a real one. |
| `known_defect` | **Excuses only numbers outside the tolerance**; a path or key error still fails, and agreeing numbers fail ("remove `known_defect`"). | The marker cannot outlive the defect. |
| The tally of a filtered run (`OMN_REFERENCE_PAIR`) | **Printed, not left for `reference_tally`.** | It is not the family's. |

**How the families compare** (`docs/fixes/w23-{bpftrace,gpu,heap,io,net,perfrecord,perfsched,perfstat,power,sysstat,top}.md`, and `w23-cross.md` for the last row):

| Decision | Choice | Reason |
|---|---|---|
| Where `omn`'s value is read | **A dotted path where `omn` holds it at one place** (series `.0` where one series is expected, so a second fails rather than adds; `.*` where the sum is meant); **an `omn_parse` parser** for a mean weighted by count, a mean times a count, a key holding `]`, an event picked by two attributes, a share over a stack table. Every separate pair's `omn` writes `-o {out}`. | The settled rule (above); and a workload that prints must never land inside the JSON. |
| A value that follows the clock or the load (cycles, cpu time) | **One run, `omn` launching the tool, a one-sided band** (perfstat); counts the tool's own process would swamp (branch and cache misses) on separate runs, medians of five. | `perf`'s own share only adds to `omn`'s count; it is 18 % of the branch misses and about 40 % of the cache misses. |
| A pid-taking tool | **`run: attach`**, the workload stopped before its exec, the raw count compared (perfstat, top; supersedes `onpid.py` above); the tool released at the workload's phase by `at.sh` polling the `comm` the workload writes (top). | An attach that starts before the exec misses nothing; the runner sends the workload's output nowhere, and its `comm` is visible from outside. |
| A system-scoped source against a tool | **`alongside`, the tool over the same seconds as `omn`'s `-d`** (power, net, sysstat, io's iostat), the load launched by the runner after `settle_ms`; not `tool_wraps_omn` while `omn` refuses a system scope beside a launch. A window holds the whole job when a rate is compared (io). | One load both windows see, two windows of equal length; a rate inside a moving job moves with it. |
| A tolerance under load | **More runs, not a wider band** (perfsched's sleeps, five); a pair that misses on an open finding becomes `known_defect` once the finding is filed, not wider (perfrecord's `load-latency`). | A band widened to go green says nothing. |
| PSS | **`omn`'s last poll**, not its maximum; smem's PSS one-sided, `omn` above (top). | PSS moves with every process mapping the same pages, and smem itself is one. |
| A cpuidle residency pair | **The measured cpu woken every millisecond across both windows** (power). | Residency is credited only on wake; a read in a long idle period is stale by an unbounded amount. |
| A cpu-frequency pair under a held load | **`omn`'s `last`** (power). | The reading at window open precedes the load. |
| A sampled workload | **Never periodic near the sampler's period**; a C workload built in `setup`, never inside the observed pid (perfrecord). | `hot.c`'s 1 ms rounds phase-locked with a 997 Hz sampler. |
| bpftrace's medians and bitesize | **`log2_buckets: 1`**, the exact offset of the two grids; bitesize compares the `[4K, 8K)` bucket, not the histogram's count. | `rel: 1.1` bounded nothing below; python3's own reads move the count between runs. |
| tcpdump on `lo` | **`omn`'s raw events counted without the `outgoing` copy** (net). | `lo` hands a packet socket each frame twice; libpcap keeps one. |
| A GPU pair without a loaded driver | **Skips through `requires: [/dev/nvidiactl]`** (gpu). | A CUDA workload without a driver exits 2, which would fail every pair for the machine's reason. |
| A cross-machine session whose window must contain a program | **The session launches the program, and the program says when it is done**; a `/proc/<pid>/io` reference is net of the cgroup's `pswpin` delta over the run; a filesystem's devices are its members (`/sys/fs/btrfs/*/devices`), not its mount's source (cross). | Neither a duration nor a head start holds under load; zram swap-ins are reads no block source sees; btrfs spreads a file's chunks over its members. |

## Settled after the twenty-third wave

- **A range in the GPU's performance levels is not free clocks** (the
  `nvidia-smi -lgc` run, 2026-10-05). With `nvidia-smi -lgc 1500,1500` in
  force on the RTX 2080 Ti, NVML's performance levels still read 300 to
  645, 2100, then 2205 MHz at every level above: the lock does not show
  there. So the reading stays "not shown locked", never "unlocked", and a
  lock needs another source of evidence (filed against `shader_lock` in
  `src/collect/gpu/mod.rs`). Persistence mode was read back both ways, on
  and off.

## Settled in the twenty-fourth wave: the lazy mode

- **A family name where a source is expected composes that family's
  default** (stream lazy, `docs/lazy.md`). `omn trace cpu -- prog`, `omn mem
  --pid 1234 -d 5s`, `omn io --cost med -- prog`: a family name has no dot,
  so it never collides with a source. The default is catalog data, a `-
  default:` item at the end of the family's own `catalog/<family>.yaml`,
  naming the requests it composes per band (`low`, `med`, `high`, the
  catalog's cost words without `free` and `extreme`); the loader refuses one
  that names a source, mode, emit, key or target the family does not have,
  that puts a source in a band its cheapest backend cannot keep, or that
  takes the person's target with a source refusing a program or a pid.
  `--cost` absent is the cheapest band the family has; a band it does not
  declare falls to the dearest under it and says so; one under its cheapest
  is refused with the band to ask. The run prints every request it composed
  in the long form before anything is resolved, then says what it chose,
  degraded and lost like any run, and names each request this machine serves
  through a backend that declares more than the band. **`--cost` is not a
  budget**: it chooses the requests, and the measured ceiling stays
  `--budget`'s, which a person can add -- a 1 % ceiling stops a short
  program's 100 ms poll on its first round, which is not what a beginner
  asked for. A family with no safe default (`runtime`, `none:` with the
  reason) refuses with the reason and its sources. A default never runs a
  session from `examples/`; a default may name only its own family's
  sources, so a machine-wide family (`power`) runs on `--system` with a
  duration, never on a program.


Checked at the merge against `src/lazy.rs`, `src/cli.rs` and the nine
`default:` items in `catalog/*.yaml`: as written. `--cost` takes `low`, `med`
or `high` (`FamilyDefault::BANDS`), the composition prints its long form
first, and `omn list` ends with a `DEFAULTS:` section. Two of the family
streams asked for a default to grow (`net.tcp.lifetime` in net's `med`,
`proc.tasks` in proc's `low`); neither is in yet, and neither changes the
rule.

## Settled in the twenty-fourth wave

From the decision lines of the sixteen handoffs (`docs/fixes/w24-*.md`); the
lazy mode's are above. No open line is settled. One is opened (`cpu.profile`'s
`pmu` row, under "Open").

**The run's shape** (`docs/fixes/w24-run.md`):

| Decision | Choice | Reason |
|---|---|---|
| A program beside a target | **The window, not a second target**: the runner launches it, ends with it and binds it to no request; `launch:` beside the requests in a session. A request that launches a program of its own beside it is refused (exit 2). | A run launches one program; `perf stat -a -- prog` and `turbostat prog` measure the machine for a program's life. |
| `--pid` | **Ends with its process**, through a pidfd; `-d` is a ceiling, not a requirement. A tid is not watched. | `perf stat -p` ends when the pid exits; a pidfd names a process. |
| `-r N` | **Spreads summaries only**: mean, sample deviation, min, max and perf's relative error per series; every run's own results kept beside the spread, each run's losses said. A request with no `summary` is refused. | One number per series per run is what a repetition is read against. |
| Exit 1 | **Its last line is one of three fixed sentences**: the machine refused, the run failed before writing, or it failed after writing. A machine refusal after a run started is a failed run. | A caller may match them, and the set stays closed. |
| The launcher | **Searches `PATH` itself and the child execs once**; the exit is seen with `WNOWAIT` and reaped after the closing round. | The program's `execve` count is its own, and the closing round reads the zombie. |
| A bounded target picked by default | **Not done**: `omn trace cpu.util -- prog` is refused with a hint naming `--system -- prog`. | Turning "this program" into "this machine" is the user's call, not the tool's. |

**The results layer** (`docs/fixes/w24-emit.md`):

| Decision | Choice | Reason |
|---|---|---|
| A backend's own emits | **`emits:`, a non-empty subset of the entry's**; the resolver passes over a backend that does not produce an asked emit, with the reason, before the run. | heap 1: an emit refused after the run is an invalid combination degraded. |
| `summary.total` | **Written only where the values add up** (the entry declares `rate`, `topk` or `stack`); absent over a level; `>=` when records were lost. | A sum of readings of a level means nothing; a total withheld is never wrong. |
| `summary@<attr>`, `hist@<attr>` | **As `topk:N@<attr>`**: rows `<attr>=<value>`, `<attr>=?` for events carrying none (said), validated against the entry's `filter_fields`, refused when the resolved backend never fills the attribute. | One grammar for every per-attribute emit, and the filter's rule for what a backend carries. |
| A ranking that counted nothing | **One zero row named by the target**, over a window it measured; none when no window was measured; no `-0.0`. | An empty table reads as a run that did not happen. |
| A regrouped poll | **One tick's readings added into one measurement**, only where the values add up; a level keeps its readings apart (the review's fix). | io 5's devices folded per tick; three RTTs are not one. |
| `func` | **A derived key**, the leaf frame of each event's stack, read by the emit layer; no collector attaches it; `--keys func` with `--stack none` refused, events without a stack counted in a note. | A key every stack-carrying source gets by one catalog line. |

**The collectors** (`docs/fixes/w24-{cpu,gpu,io,mem,net,power,proc,sched}.md`):

| Decision | Choice | Reason |
|---|---|---|
| A counting source's `sample` mode | **Samples on its own counter** through one collector (`profile::SAMPLED`): `cpu.cycles -m sample -e stack` is the cycles profile, `perf record -e <event>` is `<source> -m sample`. Cycles at a fixed period of cycles; the rest in the kernel's frequency mode, each sample weighing one until the period is decoded. | One sampler, every event; a fixed period has no natural unit for the others. |
| `cpu.profile` | **The on-cpu time profile**; its `pmu` (cycles) row has no collector on purpose. | `omn latency` and `omn diff` read its samples as periods of nanoseconds. |
| Task-clock | **`cpu.oncpu_time -m count` on `pmu:sw`**, last in the chain, not a source of its own. | The same quantity; no existing resolution moves. |
| `cpu.pressure`'s attributes | **The PSI reader's names** (`stall_ratio`, `some_avg10`, ...), as `mem.pressure` and `io.pressure`. | One family of sources, one naming. |
| A polled source whose reading describes the moment before | **Takes its window-open read as a baseline, not an event** (`cpu.util`, `cpu.pressure`, `power.freq`, `net.bytes`, `net.packets`); a polled counter reads nothing in `stop()`, only a `count` closes there (`net.iface`, `net.backlog`). | A share of a few milliseconds weighed like a whole interval was sysstat 2 and `top-cpus`' bias. |
| A process target in io | **Follows the processes it starts, in every io program** (block, fsync, page cache, VFS calls), through `sched_common::Descendants`; `--pid` included. | A forked fio read 0 requests. |
| An fentry collector's teardown | **Unlinks its trampolines after the window**, not in `stop()`. | The unlink walks every ftrace record: a fixed cost of the tool, like the attach. |
| `io.util` | **A share of 1, not a percentage**, `queue` (aqu-sz) beside it. | Every ratio in the catalog is a share. |
| `io.vfs` | **One event per interval**, each function's count beside the total, filterable on `op`, until a key for the function exists. | No `op` key on the seam yet. |
| A rate in Hz on a sampling source | **A target the kernel adjusts towards**; a run states the rate it delivered over the time the followed tasks ran, never over the wall (`mem.latency`). | A loss is counted and surfaced: 66-74 % of the rate asked was silent. |
| A per-process count the kernel keeps only per cgroup | **Read through a counting kprobe on the function doing the work**, no program loaded, last in its chain (`mem.swap` on `perf:kprobe`); a pid's default stays the cgroup's flow. | No existing resolution moves; the kprobe is opt-in by `--backend`. |
| A connection's process | **Its owner**: the task current at the first transition a process makes on its own socket (`SYN_SENT`, `FIN_WAIT1`, `LAST_ACK`), as `tcplife` attributes it; the task at `TCP_CLOSE` only where none was seen, said in `attributed`, and a loss under a process target. | The task a close interrupts is not the connection's. |
| A counter that did not move | **Absent from a `net.snmp` interval**, as in `nstat`; a snapshot lists every counter. | `nstat`'s own reading. |
| `power.freq` | **APERF/MPERF through the `msr` perf unit first**, ahead of sysfs. | It opens unprivileged, every number covers the interval, and an idle cpu reads the clock it ran at, not a setpoint. |
| Energy domains | **A key, `domain`; unkeyed, the package alone.** | Domains nest; one series across them counted the cores twice. |
| A wakeup per process | **A voluntary context switch**: one sleep, ended by one wakeup. | procfs counts them per thread, unprivileged. |
| `gpu.mem` on a process | **The process tree, walked at each reading.** | The launched pid alone missed every program under a wrapper. |
| An NVML event's `ts_err_ns` | **The span of its read**; a read the driver held past 25 ms is said with the reason, a late tick it causes stays counted. | The value is from somewhere between the call and the timestamp. |
| End-of-run facts not about the window | **Read after the window**, their cpu in `after_window_cpu_ns`. | NVML's event reasons cost 17 to 32 ms a call. |
| A held-clocks share | **The largest of the limiting reasons' shares**, clamped at 1. | A lower bound on the union NVML does not give. |
| A syscall row's task name | **Part of the kernel's key, read on every call**: a rename is two rows. | bpftrace 1: a renamed task's calls counted under its old name. |
| `proc.tasks` | **The machine's process table on `fs:proc`**: one event per process (`--system`) or per thread (`--pid`, launch), or the one thread of `--tid` under its process, per poll; `busy` a share of one cpu since the previous poll, or over the task's life when first seen (`since`); the resident set from `statm`, as `top` reads it. | `top`'s first use, and the table `omn top` reads. |
| `proc.exec` on a pid or a launch | **That process and its descendants**, followed by the parent each exec carries; an exec of unknown parentage a counted loss. | bpftrace 3: per-launch exec lists. |
| Per-call syscall records | **Their own source, `proc.calls`**, beside the bucketed `proc.syscall`, not a mode of it. | A record per call and a row per name are different costs. |
| A counted `sched.switches` rate on a task | **Per second of the run**; the counter's enabled time, which advances only while the task runs, is kept as `oncpu_ns` and is not the denominator. | sysstat 1: 33 235/s for about 945/s. |
| A filter field named `kind` on a sched source | **The row's kind** (an interrupt's half, a lock's type), not the source's. | bpftrace 2: `kind == "hard"` returned no series. |
| `sched.slices`' `queue_ns` | **The kernel's `sched_info` account**, as `sched.runqlat`'s; the tracepoints' wakeup-to-switch figure has its own name (`wake_to_run_ns`); `off_ns` and `queue_ns` add up to the gap between a switch out and the next switch in, the wait counting the part closed into `run_delay` on a cpu the task was moved from. | One account per quantity, and the two parts of a gap add up. |

**The pairs** (`docs/fixes/w24-reference.md`):

| Decision | Choice | Reason |
|---|---|---|
| A tool's own reported loss | **Fails the parse naming it** (`Lost N events`, `LOST N events!`), never compared as a count. | A short count is neither omn's defect nor the machine's refusal. Whether a loss should skip rather than fail is filed (`perf-trace-count`). |
| `teardown:` | **Mirrors `setup:`**: always runs, and a failure there fails the pair. | A cgroup left behind changes the next run. |
| Under `omn_launches_tool` | **The program omn launches runs alone before a skip** (the tool with the workload), not the workload. | A job file as the workload is not a program. |
| A missing `requires` entry | **A path "does not exist here", a binary "is not installed here".** | The skip says what is missing. |
| Exit 1 | **Read by omn's last line first**, the heuristics only when it is absent. | Stream run's three sentences. |

**The views, the triage and the unwinder** (`docs/fixes/w24-{views,top,triage,unwind}.md`):

| Decision | Choice | Reason |
|---|---|---|
| A view's layout | **Scales with the terminal**: every growable element takes the rows and columns the fixed lines leave, up to a bound that keeps it readable; held at 80x24, 160x48 and 240x60 to reaching the last quarter of both. | The user's remark: no view draws in a corner. |
| The flame's own cost | **Charged as the triage's**: the innermost frame a perf map named, else the leaf; for stacks without `named_by` the view charges the leaf and says so, and does not read maps back from the viewing machine. | One provenance, the same functions; a map on the viewing machine may not be the run's. |
| The batch output | **Raw numbers, never a unit's words**: shares as fractions, nanoseconds, bytes, `-` for not measured; every trust mark on a `#` line. | A parser skipping comments reads numbers only, and a person still sees what not to trust. |
| A process's cpu within a run, from `proc.tasks` | **Its polls' shares over their intervals**; a first reading over the task's life counts only when the task started inside the run. | A life's share is not the run's. |
| A disk's or a link's directions | **One row with its parts**; a discard or a flush is not added to the bytes. | `/proc/diskstats`' own total does not hold them. |
| Several launches' stderr | **`FILE.<n>`**, numbered from 1 across both passes, `FILE` keeping the last launch's. | One file per run a reference can be read from, and the last where one run leaves it. |
| The perf-map names | **The results' own** (`named_by`); a map read back from disk only for stacks without provenance, by the path and `(dev, ino)` the results recorded. | A map in `/tmp` can be another process's. |
| A copied stack whose ip is in no known module | **Re-reads the target's maps before giving up**, a bounded number of times. | A launched program's unwinder is built before its `execve`. |
| A workload holding a profile's shares | **Decorrelates its phase from the sampling period by construction** (a pseudo-random pad outside the measured branches, at least one period wide). | A period no divisor of the workload did not hold: 22.9 points off a share. |

## Open

- **The phase 6 criterion for the in-kernel histogram** (io, pending the
  coordinator's `null_blk` run, `STATE.md` "Needs privilege" item 2). The
  eighteenth wave found why the program statistics read 0 runs (the block
  programs were closed at the stop, before the run read their account) and
  keeps them loaded until the close, so the run can now say how much of the
  348 ns a request is the programs' own. If it is a small part, the criterion
  becomes a per-request cost in nanoseconds on that load, since 1 % of a
  device that does no work is 25 ns for two tracepoint dispatches and two
  clock reads; if it is most of it, as stream io reasons (150 to 300 ns),
  `tp_btf` for the two device reads is next (`docs/fixes/w18-io.md`).
  Leaving it open costs phase 6 its last box. **Measured after the
  eighteenth wave** (`STATE.md`, "Needs privilege"): 11.54 %, 332 ns a
  request; with the statistics on, 457 ns a request of which the programs
  ran 298.5 (149.3 ns a run, twice a request), and less the statistics' own
  cost about 173 of the 332. Most of it, so `tp_btf` is the next cut, by the
  rule above; whether the criterion becomes a per-request cost is still
  open, to be judged on what `tp_btf` leaves.
- **What the budget's first check may stop a run on** (eighteenth wave,
  profile and bench, filed to `src/collect/mod.rs`). The first check, 0.1 s
  into a launched run, weighs a one-time cost -- the first debuginfod symbol
  load, about 25 ms of a debug build; `bpf:fentry`'s 55 ms attach -- against
  the few milliseconds a waiting target has burnt, and stopped `cpu.profile`
  at 116 to 179 % where its rate is under 30 %. **Half landed in the
  twentieth wave** (stream budget, `docs/fixes/w20-budget.md`): a one-time
  load the tool can name is weighed apart from the rate
  (`overhead.one_time_cpu_ns`, settled above), which brought a debuginfod
  profile's first check from about 142-152 % to 45-51 %. It does not settle
  it: the first 100 ms of a profile of a waiting program still carry 8 to 80
  ms of other first-drain work against the 18 to 25 ms the program burnt, and
  5 runs of 6 were stopped 0.1 s in, at 44.69 to 310.44 %, where the same
  program's whole windows read 16 to 26 %. **Proposed**: no stop while the
  collection is over its ceiling by less than 10 ms of the tool's cpu
  (spent - ceiling x target < 10 ms). On the five stopped runs the excess was
  7.3, 5.4 and about 4 ms (not stopped under the rule) and 10.9 and 75 ms
  (stopped); a collector over its ceiling at its running rate passes 10 ms
  within a second or two of a busy target. A slack in milliseconds rather
  than a floor on the target's cpu, because a floor never stops anything on
  a program that waits 99 % of the time; its cost is bounded, 10 ms of the
  tool's cpu over budget once per run. Leaving it open costs every budgeted
  run of a waiting program at 99 Hz its answer at the first check.

- **A cost per regime, or a `cost_note`, in the catalog's schema** (nineteenth
  wave, bench). A band is a backend's worst regime, so `gpu.kernel` is
  `extreme` for its +67 % on a launch-bound program though it costs under
  1 % on long kernels. **Proposed with numbers in the twentieth wave**
  (stream gpu, `docs/fixes/w20-gpu.md`), from the bench's two tables:
  `gpu.kernel` through `vendor:cupti` +64.5 % `launch`, +1.81 % `compute`,
  +0.15 % `memory`; `gpu.api` through `preload` +10.5 / +2.75 / +0.01 %;
  `gpu.counters` +282 934 / +351 / +1 841 % -- one backend's bands two orders
  of magnitude apart by regime. The proposal: `cost` stays the worst regime's
  band, the one the bench gates and the resolver ranks by, and a backend may
  add `cost_by_regime: {launch: extreme, compute: med, memory: low}`, checked
  by the bench row by row, read by the budget's first check and printed by
  the run's cost line beside the measured figure. Leaving it open costs every
  `--budget` under 20 % its `gpu.kernel` on a program of long kernels,
  refused for a cost it would not pay, and the cost line saying `extreme`
  where the measurement says `med`.
- **Whether the latency session asks `sched.wakeup`** (twentieth wave,
  triage). A thread blocked on a connection beside the main thread's block is
  recognised by its stack's send and receive frames; a C program's `send`
  shows as an unnamed libc frame, so it is not. Who woke the main thread
  answers who it waited on with no frame names, at the price of one more
  source in the cheap pass. Leaving it open costs C servers whose libc frames
  are unnamed their slow client.

- **`cpu.profile`'s `pmu` (cycles) row** (twenty-fourth wave, stream cpu).
  The cycles profile is `cpu.cycles -m sample -e stack` now, and
  `cpu.profile` stays the on-cpu time profile (settled above), so the row
  has no collector on purpose: made the chain's first served backend, it
  would turn every `cpu.profile` into a cycles profile that `omn latency` and
  `omn diff` read as nanoseconds. Either the row is removed from the chain,
  or it stays as a pointer whose refusal names `cpu.cycles -m sample`.
  Leaving it open costs nothing measured: `omn list` counts one more
  backend without a collector, and a request pinned to it is refused as the
  build's gap rather than pointed at the source that serves it.

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
   *Amended in the nineteenth wave: CUPTI is closed but loadable, and from an
   injection library the driver loads at `cuInit` it serves `gpu.kernel`,
   `gpu.counters` and `gpu.profile` for a **launched** program -- nothing
   wrapped, no `nvcc`, nothing on the device changed. What stays out: a
   process `omn` did not launch, and a machine without the vendor's library.
   There is still no kernel path.*
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
