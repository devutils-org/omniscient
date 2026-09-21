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
| Output formats | text, json, yaml, toml now; ndjson, folded, speedscope, pprof, chrome declared | Documents (a request, a listing) and results (a histogram, a profile) are different payloads. Result formats are declared with the emits they can carry and reported as not yet implemented rather than silently approximated. |
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
| One request per run, for now | A session with several is refused | Running them in sequence would measure several different moments and present them as one. Concurrent attach is the honest fix, and it lands with hot attach. |
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
showed why: with multi-size transparent huge pages the fault count is nearly
flat across sizes — 263 faults for 1 MiB, 528 for 32 MiB, from 4 KiB per fault
up to 62 KiB. The collector is faithful; the metric is not what it looks like.

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

## Open

These do not block the request surface, which is machine-independent by
construction. They block the resolver, which is the next milestone.

| Decision | Options | What it costs to decide either way |
|---|---|---|
| Usage target | One-off session · continuous daemon · **daemon with a flight recorder** | A session can buffer and aggregate at the end. A daemon must aggregate online, bound every map, and support hot attach and detach, which phase 6 already assumes. Deciding "daemon" now is cheaper than retrofitting it. |

The third option is new, and it is the same decision seen from the side that
makes it concrete. A **flight recorder** is a bounded circular buffer kept
permanently, with a snapshot written out when something the run is watching for
happens -- a latency over a threshold, a loss, an exit code. It is what catches
a p99.9 that cannot be reproduced, which is the class of problem this tool is
least able to serve today: a run is one window, opened by hand, and the
interesting moment has to happen inside it.

It is listed here rather than in the roadmap because it is not a feature to
schedule, it is the run model. "A run is one window and one document" is written
into `run_all`, into the resolved file, into every emit and into how losses are
reported. A flight recorder means a run with no end, a buffer that evicts, and
an output triggered by a predicate over the events rather than by a duration --
which is *exactly* the "aggregate online, bound every map" column above, plus a
trigger. So deciding "daemon" and deciding "flight recorder" are one decision
with two names, and the cost of leaving it open is the same cost, already
stated: every collector written in the meantime assumes the window.

What the project already has that would serve it: `Losses` counts and surfaces
eviction, the join engine's open-state has a bounded capacity with expiry and
overflow counted, and `--budget` measures a run against the target. What it does
not have is a run that does not end.

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
   written down in `BRIEF_NEXT.md` under what is left, at the priority it
   deserves.
