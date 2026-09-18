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
| Only the interval match is implemented | `eq`, `scoped_eq` and `wakeup` are refused by name | Generalizing an engine against match kinds nothing has exercised makes it fit the imagined cases. The roadmap's own rule: generalize once there are several real joins. |
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

## Open

These do not block the request surface, which is machine-independent by
construction. They block the resolver, which is the next milestone.

| Decision | Options | What it costs to decide either way |
|---|---|---|
| Usage target | One-off session · continuous daemon | A session can buffer and aggregate at the end. A daemon must aggregate online, bound every map, and support hot attach and detach, which phase 6 already assumes. Deciding "daemon" now is cheaper than retrofitting it. |

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
