# The open requests, as of the fifteenth wave

State of `main` at d1f0182 (2026-09-30), after the fourteenth wave's merge.
The twelfth wave's snapshot (f7b6a92) judged every request in
`docs/open-findings.md` up to the eleventh wave's sections. It left 122 rows
OPEN or PARTLY, and each of them is judged again here against the code. The
requests filed since, under the twelfth, thirteenth and fourteenth waves'
headings, are judged below them. A line number is the line of
`docs/open-findings.md` the request starts at. That file is append-only, so
the number stays valid.

`docs/open-findings.md` never marks a request closed, which is why this file
exists. It is a dated snapshot, made by reading each request against the code.
The next brief is written from it, and it is redone rather than kept up to
date. Owners are left out: the fifteenth wave's streams did not exist when
these were filed, and whoever owns a row's file owns the row.

Counts, in rows of the tables below (a row may carry several line numbers
that ask for one thing): of what the snapshot left open, **67 landed and 4
superseded, 17 partly done and 17 still open**; of what was filed since, 12
landed, 1 superseded, 1 partly done and 20 open. Every line number the
snapshot left open appears in one of the first two tables.

## Still open

| line | status | request | file | what is left, and where |
|---|---|---|---|---|
| 546 (§11) | OPEN | `family_proc::a_count_without_its_limit...` fails on an index | `tests/family_proc.rs` | `run["results"][0]["outputs"][0]["events"][0]` (`:143`) is read before anything asserts an event exists; the failure is `expect("attributes")`, not "no event" |
| 973 | OPEN | a `gpu` line in the text environment dump | `src/render.rs` | `push_environment` (`:185`) prints kernel, clocks, BTF, cgroup2, tracefs and paranoia, and not the cards the resolved file carries |
| 1040 | PARTLY | `emitted_fields(source_id, backend)`, the catalog's filter fields crossed against the build | `src/collect/mod.rs` | per-collector `every_filter_field_the_catalog_declares_is_on_an_event` tests only; no `emitted_fields`, no sweep over the whole catalog |
| 1503 | PARTLY | 23. one PSI reader | `src/collect/mod.rs` | one reader now (`src/collect/psi.rs`), declared as `collect::mem::psi` (`src/collect/mem/mod.rs:17`) and imported by `io/pressure.rs` through `mem`; declaring it in `collect/mod.rs` is filed again at 8067 and 8206 |
| 1649 | PARTLY | 26. readers of `cpu/online` | `src/collect/power/sysfs.rs` | `capabilities::online_cpus` (`src/capabilities.rs:1396`) is used by `perf.rs` and `bpf/attach.rs:637`; `power/sysfs.rs:85` still reads its own, under a root it is given for tests |
| 1809, 2301 | PARTLY | 29. the cost bands in `catalog.rs` | `src/main.rs`, `src/catalog.rs` | `Cost::worst_case_percent` moved (`src/catalog.rs:361`); the bands' words are still `band` in `src/main.rs:786` |
| 1889 | PARTLY | `docs/families/cpu.md`'s stale passages | `docs/families/cpu.md` | "Needs from the shared layer" items 2 to 4 (`:238-248`) still ask for exit 3 for an unbuilt mode, a rate over `enabled_ns` and stack capture; all three exist |
| 2056 | PARTLY | a missing value is not a non-numeric one | `src/emit.rs` | `stack` counts absent values as unmeasured now (`:1446`); `timeline_by` (`:1109`) and `rate_by` (`:1128`) still call `value_of` and fail the run with `NotNumeric` |
| 2102 | OPEN | a series whose interest is its variation | `src/render.rs` | no first-value-then-deltas rendering |
| 2265 | OPEN | `chrome` carries raw events | `src/render.rs` | `:505` says "`chrome` carries timelines"; `OutputFormat::accepts` (`src/format.rs:195`) takes `timeline` and `raw` |
| 2280 | OPEN | the `Budget` doc comment | `src/request.rs` | `:1249` still says the resolver degrades `trace` to in-kernel `hist` to `sample`; the budget degrades at the runner and says which (`tests/budget.rs`) |
| 2341 | PARTLY | ROADMAP ticks, CATALOG flag table | `ROADMAP.md` | the `CATALOG.md` half is gone; phase 6's "Exceeding the budget produces an announced degradation" is still `[~]` (`ROADMAP.md`, phase 6), the coordinator's to tick |
| 2493 | PARTLY | `check_catalog` in the loader, and `Backend.keys` | `src/catalog.rs` | the loader runs `check_catalog` (`:766`); a backend declares no `keys` |
| 2539, 7803 | PARTLY | refuse a key the selected backend cannot attach | `src/resolve.rs`, `src/catalog.rs` | `ResolveError::KeyNotCarried` exists (`src/resolve.rs:249`) and `main.rs` classifies it; nothing raises it until `Backend.keys` exists |
| 2693 | OPEN | `UnsupportedTarget` carries its reason apart | `src/collect/mod.rs` | still `{ source_id, target }` with the reason inside `target` (`:121`) |
| 2716 | OPEN | `Filtered.near_misses` | `src/collect/mod.rs`, `src/encode.rs` | nothing named `near_miss` in `src/` |
| 2883 | OPEN | the profile scenario takes the machine | `tests/scenario_cpu.rs` | every test takes `may_load_the_machine()`, the read side of `MACHINE` (`:1810`); nothing takes the write side |
| 3092 | PARTLY | a timeline point carries its error bar | `src/emit.rs`, `catalog/gpu.yaml` | stated as a degradation; `Point` is still `{ offset_ns, value }` (`src/emit.rs:363`), and `gpu.engine_busy` still emits `[timeline, summary]`, no `raw` |
| 3119 | OPEN | a caveat true of two modes of three | `catalog/net.yaml` | *"These are counters, so a run reports the increment..."* unchanged at `:208` and `:368`, with no snapshot wording |
| 3760 | PARTLY | BTF members behind anonymous unions, for the `net` callers | `src/bpf/net_drops.rs` | `net.tcp.lifetime` and `.retransmit` carry the remote endpoint (`src/bpf/net_lifetime.rs:105-140`); `net.drops` still reads no `skb->protocol`, `skb_iif` or `dev`, and says why (`:22-28`) |
| 3972 | PARTLY | `direction` as a filter field | `catalog/mem.yaml`, `src/collect/mem/**` | `mem.page_alloc` has it (`:133`); `mem.kmalloc` is `[bytes, comm]` (`:152`) |
| 4083 | OPEN | `GET_CURRENT_COMM` from `insn.rs` | `src/bpf/proc_syscall.rs` | still a local `extra_helper::GET_CURRENT_COMM` (`:45-54`), called at `:415` |
| 4292 | OPEN | a bounded read at a `__data_loc` offset | `src/bpf/insn.rs`, `src/bpf/sched_drm.rs` | `attach::tracepoint_layout` flags the field (`src/bpf/attach.rs:535`); no variable-offset read, and `gpu.job`'s ring is the scheduler thread's name (`sched_drm.rs:138`) |
| 4662 | OPEN | per-range residency of live allocations | `src/collect/mem/heap/live.rs` | its events carry `task` only (`:60`) |
| 5132 | OPEN | 2.6 MiB of flusher progress beside 90 MiB of kworker writes | `src/collect/io/writeback.rs` | not investigated; a cgroup scope is refused as not built (`:57`) |
| 5256 | PARTLY | "one run observes one target" is no longer true | tests and examples | gone from `src/main.rs` and `src/encode.rs`; still in `tests/scenario_mem.rs:2098`, `tests/scenario_sched.rs:2307` and seven examples (`mem-allocator-ledger`, `mem-kernel-sweep`, `mem-reclaim-squeeze`, `proc-bpf-sweep`, `net-not-built`, `sched-sweep`, `net-handshake-to-close`), each to be read against `DECISIONS.md`'s "One workload per run" |
| 5272 | OPEN | the known-instruction scenario overshoots by +0.69 % | `tests/scenario_cpu.rs` | the double-count suspicion is recorded (`docs/flaky.md:138`) and not tested |
| 5520 | OPEN | `Btf::function`, a function's id and prototype | `src/bpf/btf.rs` | the second parser, `fentry::FunctionIndex`, is still there (`src/bpf/fentry.rs:168`) |
| 5536 | OPEN | the commands `fentry.rs` issues itself | `src/bpf/syscall.rs` | `fentry.rs` still defines `BPF_PROG_LOAD` and `BPF_RAW_TRACEPOINT_OPEN` (`:68-69`) and calls `bpf()` (`:533`); `syscall::raw_tracepoint_open` still takes a name (`:337`) |
| 5553 | OPEN | five module lines in `bpf/mod.rs` | `src/bpf/mod.rs`, `src/bpf/attach.rs` | `fentry`, `io_fsync`, `io_vfs`, `net_connect`, `net_socket` still declared by `#[path]` in `attach.rs` (`:61-69`) |
| 5566 | PARTLY | `GET_CURRENT_TASK` from `insn.rs` | `src/bpf/io_fsync.rs` | still a local constant (`:54`), called at `:349` |
| 5652 | PARTLY | `count` over a kernel-built histogram | `src/request.rs` | `count` ranks now (`topk`, `:715-720`); it still has no `hist` |
| 6642 | PARTLY | §47, the shim ring | `docs/bench.md` | the ring's caveat is on `mem.rss`; the bench's heap rows still predate the ring (`STATE.md`, "the bench's second table") |
| 6871 | PARTLY | one grace period per round, not per source | `src/bpf/map.rs`, `src/collect/mod.rs` | `map::grace_period` is public (`:359`) and each drain waits its own (`:328`, `fentry.rs:1377`); no round-level retire |

## Landed or superseded since the twelfth wave's snapshot

| line | status | request | where |
|---|---|---|---|
| 168 (§5) | LANDED | `count` -> `raw` in the mode x emit matrix | `CATALOG.md:526` |
| 168 (§5) | LANDED | a rate over the counter's own window | `emit::rate_by` reads `enabled_ns` (`src/emit.rs:1138`) |
| 250 (§7) | LANDED | `--pid a,b` measures only `a` | refused where a collector follows one process (`Target::narrowed_to_one`, `src/collect/mod.rs:2026`; `DECISIONS.md`, the thirteenth wave) |
| 280 (§7) | LANDED | `examples/sched-wakeup.yml` declares its join | its `joins:` block |
| 314 (§8) | LANDED | `HistScale` serializable, `BucketLayout` not a copy | one type (`src/request.rs:939`; `src/emit.rs:734`) |
| 323 (§8) | LANDED | `hist:linear:0` refused | `Emit::from_str` (`src/request.rs:814`) |
| 510 (§10), 1889 | LANDED | the sudoers line, `$(abspath)` | `Makefile:97`, `:107` |
| 624 | LANDED | `sched.offcpu --emit hist` with a log2 bucket in the key | `KEY_BUCKET` (`src/bpf/offcpu.rs:161`) |
| 660, 7121 | LANDED | `system: native` on an all-eBPF chain (P8) | retired from `KNOWN_DEFECTS` (`docs/portability.md`, "P7/P8/P9 entries retired") |
| 710 (§12) | LANDED | symbols demangled | `rustc-demangle` and `cpp_demangle` (`Cargo.toml`) |
| 815 | SUPERSEDED | `check_stack` refuses `dwarf` and `lbr` | `dwarf` is served; the unwind chooser refuses a forced mode it cannot serve, by name (`src/stack.rs`, `src/unwind.yaml`) |
| 885 | LANDED | `SampledEvent::throttled()` | `src/perf.rs:822` |
| 957 | LANDED | `Unit::Watts` | `src/encode.rs:1864` |
| 1078 | LANDED | `CATALOG.md` §1: per-backend `targets` and `filter_fields` | `CATALOG.md:119` |
| 1241, 2034, 5372, 6972 | LANDED | where a predicate runs (`filter_in_program`) | `Backend::filter_in_program` (`src/resolve.rs:703`); `DECISIONS.md`, "Where a predicate runs" |
| 1336, 5659 | LANDED | `docs/bpf-layer.md` no longer lists fentry and USDT as missing | "What is not here yet" (`docs/bpf-layer.md:235`); `fentry:` in `CATALOG.md:135` |
| 1360 | LANDED | 19. symbolising a machine-wide run | measured at 1.2 s, the cache not needed (open-findings 7307) |
| 1392 | LANDED | 20. `comm` at row creation | `src/bpf/proc_renames.rs` (`docs/families/kernel.md`, §3) |
| 1422 | LANDED | 21. `writable:` | `src/capabilities.rs:530` |
| 1469 | LANDED | 22. a NUMA node key | `KeyValue::Node` (`src/event.rs:150`) |
| 1598 | LANDED | 25. `pmu:` glob; the qualified form | `check_requirement_globs` (`src/catalog.rs:467`); `sysfs_device_state` answers missing for an absent unit (`src/capabilities.rs:950`) |
| 1735 | LANDED | 28. a band per backend against a cost per rate | `DECISIONS.md`, "How a cost band is chosen" (the thirteenth wave) |
| 1845 | LANDED | 31. instrumentation inside the target charged to it | `Overhead::in_target_cpu_ns` (`src/collect/mod.rs:670`) |
| 1959 | LANDED | the `ethtool` probe | `collect::net` probes (`src/collect/net/mod.rs:29-43`) |
| 2089 | LANDED | `CATALOG.md`'s `net.hwts` row | names `netlink:ethtool` (`CATALOG.md:361`) |
| 2127 | LANDED | 33. `Results.period_ns` for pprof | `src/encode.rs:2032` |
| 2127 | LANDED | 33. `stack` over a unit that does not add | `check_catalog` (`src/validate.rs:310`) |
| 2257 | LANDED | formats "not written" | `DECISIONS.md:20`, every format with its encoder |
| 2316 | LANDED | `Results.overhead`, `served_by` | `src/emit.rs:152`, `:157` |
| 2374 | SUPERSEDED | `sched.switches -m count` through `not_built` | `count` is handed to `counters::SCHED_SWITCHES` (`src/collect/sched/mod.rs:139-148`); the branch in `switches.rs:241` is no longer reached |
| 2420, 7107 | LANDED | a backend that cannot serve the mode (P7) | `Backend.modes` (`src/catalog.rs:232`) |
| 2467, 6181 (a) | LANDED | `count` ranks | `ModeKind::Count` admits `topk` (`src/request.rs:715-720`) |
| 2585, 3054 | LANDED | one series per counter kind (net 5) | `iface:<name>/<kind>` (`src/collect/net/iface.rs:41-46`); `DECISIONS.md`, "Counter kinds that are different failures" |
| 2600, 2755, 3883 | LANDED | a ranking over an attribute | `Emit::TopK { k, by }` (`src/request.rs:762`) |
| 2619 | LANDED | a counted window for `mem.swap` | `modes: [poll, snapshot, count]` (`catalog/mem.yaml:483`) |
| 2641 | SUPERSEDED | `TOTAL_OF_ATTR` closes sched 1 | the bucket is in the key and the row carries its `weight` (`src/collect/sched/offcpu.rs:387-388`) |
| 2745 | LANDED | `daddr` carries the port | the port is its own field (`src/collect/net/netlink.rs:56-64`) |
| 2915, 6181 (c) | LANDED | the spending test drives to a wall deadline | driven to 200 ms of spun cpu (`src/collect/mod.rs:5522`) |
| 2947, 6181 (b) | LANDED | a counted window rounds up to the tick | the run's deadline is among the wake-up times (`src/collect/mod.rs:3741-3743`) |
| 3139 | LANDED | modes per backend | `Backend.modes` (`src/catalog.rs:232`); `DECISIONS.md`, "A backend's modes" |
| 3713 | SUPERSEDED | a block key the plug's cached clock cannot collapse | decided instead: two lives under one scope are paired by order and rated `TEMPORAL` within the key, counted in `states.scope_collapsed` (`DECISIONS.md`, the thirteenth wave) |
| 3796 | LANDED | an enum's member names, for `net.drops` | `btf.enum_variant("skb_drop_reason", ..)` (`src/collect/net/drops.rs:261`) |
| 3892 | LANDED | a note is not always a degradation | `Collector::facts()`, reaching results as `notes` (open-findings 7937) |
| 4168, 4328 | LANDED | a program narrowed to a cgroup; `sched` serving a cgroup | `insn::Whose::Cgroup`, `push_task_filter` (open-findings 8036); `DECISIONS.md`, "What `--cgroup` means" |
| 4304 | LANDED | `sched.irq` promises the soft half | `softirq_entry`/`softirq_exit` attached (`src/bpf/sched_irq.rs:1-3`) |
| 4699 | LANDED | the mTHP explanation | `catalog/mem.yaml` and `DECISIONS.md`, "What the measurement taught" |
| 4900, 6925 | LANDED | the `sched` sources apply the filter | every factory passes `request.filter` (`src/collect/sched/mod.rs:60-107`) |
| 4927 | LANDED | a thread target compares the thread | `DECISIONS.md`, "A thread and a process target" |
| 4968 | LANDED | `woke` | `MatchKind::Woke` (`src/join.rs:880`) |
| 4988 | LANDED | a decomposition part a join defines, a measured whole | `value: paired` from a `woke` join; `Whole::Measured` (`src/join.rs:2336`) |
| 5032 | LANDED | a match whose evidence is `TEMPORAL` over a shared split | `overlap`, keyless over machine state (`join::MACHINE`) |
| 5099 | LANDED | a per-tick read of a counting PMU event | `cpu.instructions`, `cpu.cycles`: `modes: [count, poll, sample]` (`catalog/cpu.yaml:13`, `:28`) |
| 5285, 7067 | LANDED | the `uprobe` probe below 6.6 (P3) | `docs/portability.md`, P3 "On cells" |
| 5353 | LANDED | `insn.rs` encodings for USDT | `Insn::or64_imm` (`src/bpf/insn.rs:200`); no `mod local` left in `usdt.rs` |
| 5642 | LANDED | a fixed attach delay in `family_sched` | the task is moved until the run ends, and the moves inside the window are counted |
| 6602 | LANDED | §47, cost in the target | `cost.json` and `--budget` weigh `in_target`; bands follow the bench's dilation (`DECISIONS.md`, the thirteenth wave) |
| 6739 | LANDED | a held request that does not resolve is an unserved layer | `collect::Unserved` (`src/main.rs:238`) |
| 6832 | LANDED | the drain race in `io_pagecache`, `net_qdisc`, `net_softirq` | `Rows::create` (`io_pagecache.rs:623`, `net_qdisc.rs:363`, `net_softirq.rs:359`) |
| 6857 | LANDED | fentry's last drain waits a grace period | `map::grace_period` (`src/bpf/fentry.rs:1377`) |
| 6891 | LANDED | `stack` over `mem.latency` | `measured()` in `stack` (`src/emit.rs:1446`) |
| 6952 | LANDED | `sched.switches` follows threads | `ThreadSampler` (`src/collect/sched/switches.rs:23`, `:77`) |
| 6989 | LANDED | §48, a wakeup from an interrupt | `preempt_count`, `HARDIRQ_MASK` (`src/bpf/wakeup.rs:92-94`) |
| 7028 | LANDED | P1, the prototype in the probe | `docs/portability.md`, P1 |
| 7049 | LANDED | P2, `btf_member:` | `docs/portability.md`, P2 |
| 7079 | LANDED | P4, USDT needs BTF | `docs/portability.md`, P4 |
| 7087 | LANDED | P5, `net.request` below 6.6 | `docs/portability.md`, P5 |
| 7096 | LANDED | P6, NVML without its driver | `docs/portability.md`, P6 |
| 7130 | LANDED | P9, a per-task probe for a per-cpu event | retired from `KNOWN_DEFECTS` |
| 7139 | LANDED | P10, two requirements that named the wrong thing | `docs/portability.md`, P10; the guest's single root cgroup is decided "not a probe defect" |
| 7150 | LANDED | tests that failed without capabilities, a skip with no reason | skip lines give the refusal (`tests/vertical.rs:166-177`), gated by `.github/scripts/skip-gate.sh` |
| 7182 | LANDED | `make matrix`, and the workflow pushed | `.github/workflows/ci.yml` is on `origin/main`, and the CI has run on both cells |

## Filed since the snapshot

The twelfth, thirteenth and fourteenth waves' "Needs from another stream"
sections (`docs/open-findings.md`, line 7210 onward). A heading whose request
landed at its own wave's merge is not repeated here unless something of it is
left.

| line | status | request | file | what is left, and where |
|---|---|---|---|---|
| 7221 | LANDED | `omn list --format json` carries the sweep | | `here` on each entry (`src/main.rs:1537`); `omn-mcp` still has `parse_sweep` over the text (`mcp/src/binary.rs:299`), which can go |
| 7331, 7344 | LANDED | `Btf::var_id`; `THIS_CPU_PTR`, `load_btf_id` | | `src/bpf/btf.rs:168`; `src/bpf/insn.rs:126`, `:299`. `wakeup.rs` keeps a test-only `var_id` over the raw blob (`:673`) |
| 7377 | LANDED | a waker of 0 is not always the idle task | | `interrupt (hardirq)` chains (`src/encode.rs:441`) |
| 7451 | LANDED | what `proc.syscall`'s comparison skipped | | `SKIPPED_BY_PREDICATE` (`src/bpf/proc_syscall.rs:84`) |
| 7469 | LANDED | who applied a predicate | | `AppliedBy` in the text (`src/render.rs:166-168`) |
| 7581, 7843 | LANDED | `CATALOG.md` §1's requirement notations | | `btf_member:`, `writable:` and the prototypes (`CATALOG.md:135-160`) |
| 7605 | SUPERSEDED | the `cgroup`/`container` targets nothing serves | | the fourteenth wave's `container: no`, and its removal from the eleven entries that serve a cgroup, filed at 8247 and 8331 (below) |
| 7930 | PARTLY | `ProgramStats::for_run` onto `bpf::enable_run_time_stats` | `src/collect/mod.rs` | the re-export exists (`src/bpf/mod.rs:109`); `for_run` still issues `BPF_ENABLE_STATS` itself (`src/collect/mod.rs:761`, `:825`) |
| 7944 | LANDED | two bench tests share a report file | | the name carries a per-call counter (`examples/bench/harness.rs:777`) |
| 8030 | LANDED | `SCHED_SWITCHES` is used | | `src/collect/cpu/counters.rs:246` |
| 8067, 8206 | OPEN | declare `psi` where it lives | `src/collect/mod.rs`, `src/collect/mem/mod.rs` | `pub mod psi` is still in `mem/mod.rs:17`; moved in one commit with K's three lines |
| 8131 | LANDED | `topk` back into `net.bytes` | | `emits: [rate, timeline, summary, topk]` (`catalog/net.yaml:130`) |
| 8221 | LANDED | the kernel's count is a fact (`net.softirq`) | | `facts()` holds both of the kernel's counts (`src/collect/net/softirq.rs:495`) |
| 8234, 8247, 8331 | OPEN | `container: no` dropped where a cgroup is served | `catalog/cpu.yaml`, `catalog/mem.yaml`, `src/validate.rs`, `tests/scenario_cpu.rs` | ten `container: no` in `catalog/cpu.yaml`; the entries and the refusal test change in one commit |
| 8261, 8623 | OPEN | `.github/expected-skips.txt`: the cgroup-ancestry wording in, `/sys/class/block/root` out | `.github/expected-skips.txt` | the `/sys/class/block/root` line is still there (`:75`) |
| 8295 | OPEN | `join::decompose_within(.., windows)`, so a `paired` part pairs over the joins' windows | `src/join.rs` | no `decompose_within`; `Session::check_paired_parts` still refuses before the run (`src/request.rs:1684`) |
| 8366 | OPEN | `--symbols local\|debuginfod` | `src/request.rs`, `src/cli.rs` | the mechanism is landed; no flag in `src/cli.rs` |
| 8388 | OPEN | the unwind choice in the resolved file | `src/main.rs`, `src/resolve.rs` | `STATE.md`, "What remains", item 2 |
| 8399 | OPEN | the mode table beside the catalog | `src/catalog.rs` | still `src/unwind.yaml`, because `load_dir` parses every file of `catalog/` as entries |
| 8437 | OPEN | `cost.json` for a profile with stacks leaves most of it out | `src/collect/mod.rs` | 26 ms reported where the kernel counts 986 ms (`STATE.md`) |
| 8489 | OPEN | a rate over joules printed as watts | `src/encode.rs` | the rate line still appends `/s` to the unit (`:1267`) |
| 8497 | OPEN | "here" names a build's gap (optional) | `src/render.rs` | |
| 8546 | OPEN | *"bpftool could not dump the BTF"* gives no reason | `src/bpf/btf.rs` | `:670` unchanged |
| 8552 | OPEN | skip lines the gate cannot read | `tests/cross_memory.rs:147`, `tests/cross_machine.rs:807`, `:1032` | `STATE.md`, "What remains", item 2 |
| 8588 | LANDED | a context switch is kernel mode | | the probe opens without `exclude_kernel` for the kernel-only events (`counted_only_in_kernel`, `src/capabilities.rs:1040`), and the counter refuses rather than notes (`src/collect/cpu/counters.rs:681`) |
| 8612 | OPEN | a (target, mode) pair no backend declares is the request's, exit 2 | `src/resolve.rs` or `src/validate.rs` | `sched.switches --cgroup -m trace` reads as the machine's |
| 8631 | OPEN | two lines for `collect/mod.rs`: `pub mod net`, `ProgramStats::for_run` | `src/collect/mod.rs` | `mod net` is private (`:26`) |
| 8642 | OPEN | a preempted program skips every run behind it | `docs/bpf-layer.md` | the coordinator's; `STATE.md` lists it unscheduled |
| 8658 | OPEN | three `mcp` protocol tests fail rather than skip without capabilities | `mcp/tests/protocol.rs` | |
| 8711 | OPEN | capabilities on the file, not in the process; a `nosuid` mount | `src/capabilities.rs` | nothing says `nosuid` in `src/` |
| 8728 | OPEN | the shim hooks `aligned_alloc`, `memalign`, `valloc`, `pvalloc`; its refusal; `find_shim` under `lib/omn/` | `shim/**`, `src/collect/mem/heap/mod.rs` | the shim replaces `posix_memalign` (`shim/src/lib.rs:852`) and none of the four; `dd bs=64M` reads a largest allocation of 4 KiB and no loss, **a silent loss** |
| 8747 | OPEN | the "no viable backend" refusal leads with the backend passed over | `src/collect/mod.rs` | |
| 8756 | OPEN | a `LICENSE` file | the repository root | none; `Cargo.toml` says Apache-2.0 |
| (STATE) | LANDED | `CLAUDE.md`'s *"86 usable sources and 51"* | `CLAUDE.md` | 92 and 51, this wave |

## Landed by the twelfth wave's snapshot

The snapshot's own rows, as it judged them at f7b6a92. They are kept as
written; a line number in them is the one it had then.

| line | status | heading | evidence |
|---|---|---|---|
| 10 (§1) / 1095 | LANDED | fields a collector could emit and does not (cpu/mem/runtime `comm`, io `dev`, sched `pid`, power `cpu`) | `src/collect/cpu/counters.rs:478`, `mem/heap/alloc.rs:82`, `io/diskstats.rs:319`, `sched/switches.rs:120`, `power/freq.rs:237`, `runtime/jit_map.rs`; commit 11b77ae |
| 79 (§2) / 1052 | LANDED | emit the two new keys (`KeyValue::Dev` per device, `KeyValue::Cpu` per cpu) | `src/collect/io/diskstats.rs:324,346`; `src/collect/power/{freq,cstate,throttle}.rs`; commit 11b77ae |
| 132 (§4) | LANDED | closing read of a polled counter: `net/dev.rs` must not read at stop | see line 931 |
| 168 (§5) | LANDED | an exit code for "valid, but not built" (mode refusals) | see line 1000 |
| 168 (§5) | LANDED | `hwmon` probe | `src/capabilities.rs:617` `probe_hwmon`; commit 569df7f |
| 261 (§7) | LANDED | `scoped_eq` has no producer | `KeyValue::Blk` emitted by `src/collect/io/block.rs:414,805` |
| 330 (§8) / 716 (§13) | LANDED | `sched.switches` ground-truth test measures a process-wide count | the reference now reads `/proc/thread-self/status` (`src/collect/sched/switches.rs:263`), and the perf event is disabled before the reference is read (`:304-310`) |
| 354 | LANDED | stream A: a correlation is not always a range (`Correlation::match_kind`) | `src/join.rs:100,297`; `src/encode.rs:316` keys the heading off `MatchKind` |
| 380 | LANDED | stream D: refuse a malformed join before collecting (`join::check`) | `src/join.rs:656` `check`, `:698` `check_in`, called from `src/validate.rs:243`; commit 2850c5d |
| 404 (§9) | LANDED | 36 sources need a `bpf:raw_tp` backend | finding 36 (line 3285, "Finding 9, landed"); e.g. `catalog/sched.yaml:13` |
| 510 (§11) | LANDED | `family_power` idle-share bound | `tests/family_power.rs:399` now bounds at `0..50` as an error of scale, not 1.5 |
| 510 (§11) | LANDED | gpu shared-device tests, io substituted-counters lead-in | fixed in the finding itself (`device_to_ourselves()` skip; 700 ms lead-in) |
| 600 | LANDED | stream B: `symbolize` / `Frame` seam for `sched.offcpu --emit stack` | `src/stack.rs:140` `Frame`, `:433` `symbolize`; `sched.offcpu` emits `stack` (`catalog/sched.yaml:18`) with a stack id in the key (`src/bpf/offcpu.rs:133` `KEY_STACK`) |
| 650 | LANDED | for whoever owns `catalog/*.yaml`: the `sched.offcpu` raw_tp backend and two caveats | `catalog/sched.yaml:13` |
| 748 | LANDED | stream B to A: `Results::stacks`, `Output::Stack`, `folded` | `src/emit.rs:140` `stacks`, `:257` `Output::Stack`; commit c77cd85 |
| 836 | LANDED | stream B: the first profiling collector | `src/collect/cpu/profile.rs` |
| 868 | LANDED | what would let `topk` answer `io` and `net` (Dev/Cpu keys emitted) | same as line 1052 |
| 902 | LANDED | `page_faults.rs` and `switches.rs`: throttles into `Losses::throttled` | `src/collect/mem/page_faults.rs:177-182`, `src/collect/sched/switches.rs:150-151`; commit 11b77ae |
| 915 | LANDED | stream A, `encode.rs`: the `LOSS:` line names undecoded and throttled | `src/encode.rs:805`; commit 92422c7 |
| 931 | LANDED | `net/dev.rs`: `stop` reads nothing | `src/collect/net/dev.rs:212`, test `stopping_reads_nothing_because_the_runner_closed_the_window` (`:235`); commit b514dff |
| 988 | SUPERSEDED | `CATALOG.md` `--budget` line promises degradation | the budget is now enforced (moves to a cheaper backend or stops: `src/cli.rs:176-179`, `man/omn.1:262-270`; commit d7e566a), so `CATALOG.md:440` is now true in substance |
| 1000 | LANDED | `Prepared`'s refusals need to say whose gap they are | `CollectError::Start { whose: Whose }` (`src/collect/mod.rs:57,119-127`), read by `exit_for` (`src/main.rs:51`); commit 3aed2db |
| 1071 | LANDED | coordinator: `STATE.md` items 3 and 4 are done | neither "Per-backend targets" nor "Keys for a device" appears in `STATE.md` any more |
| 1113 | LANDED | 14. capabilities remove three sources (dumpable) | `src/main.rs:85` `prctl(PR_SET_DUMPABLE, 1)`; 3ea2038 |
| 1164 | LANDED | 15. `bpf:tp` available when only its directory is | `src/capabilities.rs:543` `classic_tracepoint_support`, `:554` `probe_type_support` |
| 1293 | LANDED | 17. emit cannot weigh an event (`WEIGHT_ATTR`) | `src/emit.rs:57` `WEIGHT_ATTR = "weight"`, read by hist/summary/rate (`weight_of`, `src/emit.rs:966`) |
| 1540 | LANDED | 24. a session polls at its fastest collector's pace | per-collector `Pace`, `src/collect/mod.rs:2284,2362` |
| 1889 | LANDED | Needs: two globbed `io` requirements | `catalog/io.yaml:232` (ext4 glob removed), `:263` (filemap named) |
| 1889 | LANDED | Needs: stream B, no second factory for `mem.cache_*` | no `cache_*` factory in `src/collect/mem/mod.rs` |
| 2006 | LANDED | To the coordinator: `CAP_NET_RAW` | `DECISIONS.md:165` (opt-in), `Makefile:27` `make CAPTURE=1` |
| 2237 | LANDED | To E (`main.rs`): binary output bytes | `src/main.rs:812` `run_output_bytes`, `:822` `write_all` |
| 2408 | LANDED | To B (`catalog/io.yaml`): `io.inflight -m count`, `io.bytes -m snapshot` | `catalog/io.yaml:180` `io.inflight` modes `[poll, snapshot]`; `io.bytes` modes `[count, trace, poll]` |
| 2429 | LANDED | To the coordinator: exit codes in `man/omn.1` and `GUIDE.md` | `man/omn.1:588-599`; `GUIDE.md:111-116` |
| 2559 | LANDED | To F: what a snapshot of a counter answers (net 3) | `src/collect/net/backlog.rs:61` `drops_since_boot`, `src/collect/net/iface.rs:100` `errors_since_boot`; fac621b |
| 2787 | LANDED | To `man/omn.1`: `=~` is a substring test | `man/omn.1:245-247` |
| 2808 | LANDED | To A: a filter that fails to parse exits 2 | `src/main.rs:91-104` (every clap refusal exits `EXIT_REJECTED`) |
| 2822 | LANDED | To `tests/family_sched.rs`: the one-in-five flake | `workload_name` in `tests/family_sched.rs:466`; 51d698c |
| 2852 | SUPERSEDED | To E (`page_faults.rs`): disclose the non-inherited event | `mem.faults` now opens an inherited event per thread per cpu (`src/collect/mem/page_faults.rs:14`); 455a818 |
| 2963 | LANDED | For the coordinator, `examples/`: two sessions describe a defect that is gone | examples/proc-one-reading.yml:18 and examples/proc-exit.yml:19 carry the suggested paragraphs verbatim |
| 3008 | LANDED | To stream B, `catalog/io.yaml`: the read/write split has no emit (io 7) | `raw` declared on io.bytes (catalog/io.yaml:80), io.iops (:119), io.merges (:161) |
| 3035 | LANDED | To stream C, `src/encode.rs`: the loss headline names three of five (io 9) | src/encode.rs:805 prints all five: "LOSS: {} dropped, {} failed reads, {} late ticks, {} undecoded, {} throttled" |
| 3158 | LANDED | 34. Polling cadence anchored on boot | Finding records its own fix; test `the_grid_is_anchored_on_the_first_sample_rather_than_on_boot` (src/collect/mem/cadence) |
| 3216 | LANDED | 35. Six tests asserted what this machine can serve | Finding records its own fix (scenarios rewritten to accept either honest answer); nothing requested |
| 3285 | SUPERSEDED | 36. Finding 9 landed; residue "io.fs/io.readahead glob, waiting on fentry/usdt" | io.readahead now names two tracepoints + raw_tp (catalog/io.yaml:296-298); io.fs's ext4 glob backend removed, `bpf:fentry` first (catalog/io.yaml:232-236); fentry/usdt landed in wave 8 |
| 3366 | LANDED | 37. A distribution does not say it is missing its own tail | `Shortfall` on hist/summary (src/emit.rs:392-399, `short: Option<Shortfall>` src/emit.rs:275,319); `bound()`/`push_shortfall()` in src/encode.rs:1307,1329 |
| 3441 | LANDED | 38. BTF presence is not attachability | `omn list --verify-attach` and tests/raw_tracepoints.rs; nothing requested beyond it |
| 3518 | LANDED | From stream F to coordinator: finding 37's nine lines in `src/encode.rs` | src/encode.rs:1307 `fn bound`, :1329 `fn push_shortfall` |
| 3540 | LANDED | From stream F: `rate`, `topk`, `stack` are short in the same way | Finding 45 (open-findings.md:5674); `pub short: Option<Shortfall>` on further outputs (src/emit.rs:515,538,695) |
| 3567 | LANDED | Sixth wave A item 1: `bpf_get_current_comm` in the helper table | src/bpf/insn.rs:94 `GET_CURRENT_COMM` |
| 3583 | LANDED | Sixth wave A item 2: member offset descending anonymous members | src/bpf/btf.rs:181 `member_offset`; finding 41; io.pagecache collector exists (src/bpf/io_pagecache.rs) |
| 3631 | LANDED | Sixth wave A item 3: enum values by name | src/bpf/btf.rs:290 `enum_variant` (renamed from `enum_name`) |
| 3655 | LANDED | Sixth wave A item 4: the glob `io.readahead` requires | catalog/io.yaml:296-298, exactly the three backends requested |
| 3822 | LANDED | To `src/bpf/insn.rs`: three helpers written twice (+ `read_narrow`) | src/bpf/insn.rs:445 `read_field`, :466 `read_narrow`, :488 `count_drop`, :527 `push_log2_bucket` |
| 3859 | LANDED | To `catalog/net.yaml`: four entries offer what no backend can serve | net.tcp.lifetime `targets: {system: native}`, no daddr/dport; net.qdisc `keys: [iface]` and requires enqueue+dequeue (catalog/net.yaml); (net.tcp.retransmit has since regained pid/cgroup `bpf_only`) -- collector half tracked under 3760 |
| 3911 | LANDED | `catalog/mem.yaml`: `mem.reclaim` requires half of the source | memcg pair in both backends' requires (catalog/mem.yaml, mem.reclaim) |
| 3947 | LANDED | `catalog/mem.yaml`: `mem.compaction` requires no ending | `raw_tracepoint:mm_compaction_end` in requires (catalog/mem.yaml, mem.compaction) |
| 3959 | LANDED | `catalog/mem.yaml`: two entries declare a `mem` key nothing carries | mem.page_alloc `keys: [task, cgroup]`, mem.kmalloc `keys: [task]` |
| 4111 | LANDED | To `src/bpf/insn.rs`: three program fragments written three times | src/bpf/insn.rs:445,488,527 (finding 41: one copy each) |
| 4137 | LANDED | To `src/bpf/map.rs`/`syscall.rs`: capability advice on map creation | src/bpf/map.rs:55 `pub fn map_failure` calling `permission_advice` |
| 4197 | LANDED | To `catalog/*.yaml`: three key declarations nothing emits | proc.syscall, proc.exec, proc.exit all `keys: [task]` (catalog/proc.yaml) |
| 4215 | SUPERSEDED | To `src/emit.rs`/`src/encode.rs`: a note is not a degradation (proc defect 7) | Duplicate of 3892 (tracked there as OPEN) |
| 4238 | LANDED | To `src/bpf/insn.rs`: two helpers the shared list does not name (sched) | src/bpf/insn.rs:94,110; `sched_common.rs::extra_helper` gone (only proc_syscall.rs keeps one, see 4083) |
| 4256 | LANDED | To `src/bpf/insn.rs`: no bitwise `and` | src/bpf/insn.rs:171 `pub fn and64_imm` |
| 4272 | LANDED | To `src/bpf/attach.rs`: the tracefs root and the `format` file | src/bpf/attach.rs:450 `tracefs_root`, :475 `tracepoint_layout` |
| 4347 | LANDED | 39. `src/bpf/mod.rs` was an unnamed append seam | Named in later briefs: `src/bpf/mod.rs` owned by V in BRIEF.md:69; wave 8 "To the coordinator: one line in src/bpf/mod.rs" |
| 4423 | LANDED | 40. Survey: DBI left open in DECISIONS.md | DECISIONS.md:197 "Settled in the tenth wave: no dynamic binary instrumentation backend" |
| 4522 | LANDED | 41. The sixth wave's requests, landed once | Itself the landing record (docs/families/shared-layer.md); scenario_io flakiness now recorded in docs/scenarios/io.md:686 |
| 4593 | LANDED | D1: say `TEMPORAL` for crediting within a key | `Correlation::within_key` `src/join.rs:310`, set at 1401; test asserts `Some(Evidence::Temporal)` at 3479; 4dc2ac3 |
| 4623 | LANDED | a match kind `TEMPORAL` by construction (`between`) | `MatchKind::Between` `src/catalog.rs:238`, `fn between` `src/join.rs:1127`, tests `between_onto_a_traced_source_is_refused_before_the_run`; 2850c5d |
| 4647 | LANDED | D2: three entries declare a `mem` key no collector emits | `catalog/mem.yaml:17` mem.rss `[task, cgroup]`, `:173` heap.live `[task]`, `:195` heap.retained `[task]`; 2fa706b |
| 4684 | LANDED | D3: one shim stream per launch | `ConflictingEnvironment => Whose::Build` `src/collect/mod.rs:214`; `share_launch` `src/collect/mem/heap/mod.rs:539`; fc4fd85 |
| 4724 | LANDED | split the queue wait out of a block (finding 1) | `queued_ns`/`blocked_ns` `src/collect/sched/offcpu.rs:351-352`; closed as finding 42 |
| 4749 | LANDED | the launched program's lifetime (finding 4) | `pub struct Lifetime` `src/collect/launch.rs:41`; fc4fd85 |
| 4774 | LANDED | a decomposition beside the joins (P2) | `Decomposition` `src/join.rs:1576`, `DecompositionOutput` `src/join.rs:1752`; 2850c5d |
| 4820 | LANDED | refuse a wakeup join onto a drained total (finding 5) | `drained: true` `catalog/sched.yaml:65,70`; refusal text `src/join.rs:263`; 2850c5d |
| 4848 | LANDED | rank a wakeup join by chain (finding 6) | fold by chain `src/join.rs:1423`; `(StillOpen, Wakeup) => "still open at the end"` `src/encode.rs:357`, test at `src/encode.rs:3114` |
| 4865 | LANDED | a closing read from the reap (finding 8) | as `Lifetime::cpu_ns` from `wait4` rusage `src/collect/launch.rs:57-70` (not as a series point); 6e10f35 |
| 4874 | LANDED | what a `cpu-clock` sample counts (finding 2) | caveat `catalog/cpu.yaml:132-135` |
| 4946 | LANDED | P3 defect 3: refuse `eq` on a scoped key, or rate it lower | `MatchKind::Eq if is_scoped(rule.via) => Evidence::Temporal` `src/join.rs:1471`; 3adb003 |
| 4958 | LANDED | P3 defect 4: wakeup attributions grouped by chain | same fold as 4848, `src/join.rs:1423`; 2850c5d |
| 5066 | LANDED | P4: more than one target in one run | `workload_of` skips machine targets `src/collect/mod.rs:1775`; 3adb003 / fc4fd85 |
| 5119 | LANDED | P4: `power.throttle` names a temperature tracepoint | `catalog/power.yaml:130-131` now `thermal_zone_trip` + `cdev_update`; 2fa706b |
| 5145 | LANDED | 42. queue wait inside a block (closed) | itself a closure record; `src/collect/sched/offcpu.rs:351` |
| 5194 | LANDED | publish the lifetime | `Results::lifetime` `src/emit.rs:123`, copied at 840; text `src/encode.rs:844`; 72b41e6 |
| 5219 | LANDED | `Target::is_machine` in request.rs | `src/request.rs:117`; private copy gone from `src/collect/mod.rs`; 9e313ab |
| 5235 | LANDED | D3 is fixed, its tripwire is red | `tests/cross_memory.rs:835` `d3_two_shim_sources_share_one_launch_and_both_answer`; 5d5162f |
| 5326 | LANDED | `syscall.rs`: `prog_load` with an attach type, and `link_create` | `prog_load_for` (src/bpf/syscall.rs:290) and `link_uprobe_multi` (syscall.rs:386), commit 142717a. The fentry `attach_btf_id` half is tracked under line 5536 |
| 5362 | LANDED | To the coordinator: `pub mod usdt;` and the indexes | src/bpf/mod.rs:62 has `pub mod usdt;`. The seven `runtime-usdt-*` rows are in examples/README.md and GUIDE.md |
| 5381 | LANDED | `sched.mutex`: glibc publishes the mutex | src/collect/sched/mutex_usdt.rs, and the `bpf:usdt` backend on `sched.mutex` in catalog/sched.yaml. Commit 8001b68 |
| 5400 | LANDED | Finding 43: the lifetime in the text encoder | src/encode.rs:828 prints `results.lifetime` with its bracket. Commit 10006ac |
| 5573 | LANDED | `runqlat.rs` and `proc_syscall.rs`: three losses nothing counts | The drain race is fixed by the generation slot (`map::select_rows`, runqlat.rs:87, proc_syscall.rs:92), commit fe03ec0. The skips are counted by `fn skipped` in both, commit edda3de. Item 3 is fentry-only and informational |
| 5602 | LANDED | Every raw-tracepoint program: the kernel's skips are a loss nothing counts | `fn skipped` in runqlat, offcpu, wakeup, migrate, every `sched_*`, proc_syscall, proc_events, mem_alloc and mem_stall (`mem_program.rs` is helpers only). Commit edda3de |
| 5631 | LANDED | `join.rs`: an event without the rule's key is unattributable | `Correlation::keyless` (src/join.rs:325,871-911). Commit 1e375b5 |
| 5674 | LANDED | Finding 45: the shortfall mark in the text encoder for rate, topk and stack | `push_shortfall` and `bound` in src/encode.rs:906-1058 and 1307. Commit 10006ac |
| 5753 | LANDED | To the coordinator: three join session rows, and the stale scenario docs | The `join-between-residency`, `join-decompose-memory` and `join-decompose-compute` rows are in examples/README.md and GUIDE.md. cross-memory.md D1 is marked fixed (line 342), cross-compute.md findings 5 and 6 are marked landed (line 485), cross-io.md defects 3 and 4 are marked fixed (lines 386, 411) |
| 5769 | LANDED | `CATALOG.md`: the `drained` backend field | CATALOG.md:116-120. Commit 23ab60d |
| 5782 | LANDED | Each family's catalog: `drained: true` on the rest | `drained: true` is on sched.migrations, lock, futex, irq and numa_balance, on proc.syscall, net.softirq, net.qdisc and gpu.job. The request's "mem.stall" and the `allocations.rs` backend are really mem.reclaim/compaction and mem.page_alloc/kmalloc, all flagged. Commit 23ab60d |
| 5802 | LANDED | `mem/heap/`: a free that carries its size | `freed_bytes` and `freed_untracked` in src/collect/mem/heap/alloc.rs:78. Commit 1cd228d |
| 5819 | LANDED | Stream R: `whole: lifetime` | `Span::Lifetime` (src/join.rs:1612-1631,1918). Commit 663be0a |
| 5837 | LANDED | `man/omn.1`: `between` and `decompose:` | man/omn.1:431-449 and 490 (between, decompose:, TEMPORAL). Commit 10006ac |
| 5847 | LANDED | `tests/aggregation.rs`: a float compared exactly | tests/aggregation.rs:312 compares with an `f64::EPSILON` bound. Commit 307fa56 |
| 5871 | LANDED | `encode.rs`: a loss reason beside clean counters is never printed | src/encode.rs:853-860 prints the reasons whatever the counters say. Commit 307fa56 |
| 5884 | LANDED | `capabilities.rs`: a probe for `dynamic-linking` | src/capabilities.rs:598 (`find_shim`). Commit 569df7f |
| 5901 | LANDED | `resolve.rs`: the `hist` warning names a kernel a preload backend lacks | The warning is now guarded by `budget_moves_hist` (src/validate.rs:382-389,460-465), which requires one drained and one non-drained implemented backend |
| 5909 | LANDED | `catalog/mem.yaml`: three filter fields on `mem.heap.alloc` | `filter_fields: [bytes, comm, op, freed_bytes, freed_untracked]` in catalog/mem.yaml. Commit 307fa56 |
| 5929 | LANDED | `usdt.rs`: glibc's mutex probes, acknowledged | src/collect/sched/mutex_usdt.rs:11-53. Commit 8001b68 |
| 5939 | LANDED | To the coordinator (stream S): seven sessions, `OMN_SHIM_FD`, the 40-byte record, STATE's ns per event | The sched and gpu sessions have their rows. The shim uses `OMN_SHIM_FD` (shim/src/lib.rs:52). STATE.md now gives about 70 ns on the shared ring (line 221). One stale doc comment, src/collect/mem/heap/mod.rs:15, still says `OMN_HEAP_FD` |
| 5960 | LANDED | `catalog.rs`: a cycle unit | `Unit::Cycles` (src/catalog.rs:365,378). `mem.latency` has `unit: cycles` in catalog/mem.yaml. Commit 307fa56 |
| 5973 | LANDED | `emit.rs`: an event whose value was not measured | `unmeasured` in src/emit.rs:505,529,708. Commit 307fa56 |
| 5989 | LANDED | tests/cli.rs: a test that ran out of candidates | tests/cli.rs:142-153 tries every declared mode x emit of every uncollected source; coordinator note at open-findings.md:6159 |
| 6032 | SUPERSEDED | `map_update` outside `#[cfg(test)]` | made moot by task storage (next row); `map_update` is still `#[cfg(test)]` at src/bpf/syscall.rs:461, as the coordinator note at 6156 says |
| 6043 | LANDED | io_pagecache `last` map onto task storage | src/bpf/io_pagecache.rs:593,612 `TaskStorage::create(..., "omn_pc_last")` |
| 6056 | LANDED | a row read per `req` is IDENTITY | `rate()` src/join.rs:2488, used in `Readout::row` src/join.rs:2471; commit 1e375b5 |
| 6072 | LANDED | a key the whole carries is a table | src/join.rs:2097-2103 extends `per_key` with `whole_readout.keys()` |
| 6085 | LANDED | seconded: an event without the key is unattributed | `Correlation::keyless` src/join.rs:325, `with_keyless` src/join.rs:1528; commit 1e375b5 |
| 6094 | LANDED | a row that names who released it | `Part::names` src/join.rs:1681, `Row::named: Option<Named>` src/join.rs:1821, filled at src/join.rs:2136; commit 1e375b5 |
| 6110 | LANDED | usdt.rs: use syscall.rs's uprobe-multi load/link | src/bpf/usdt.rs:1074,1095 are now thin wrappers over `syscall::prog_load_for` / `syscall::link_uprobe_multi`, which takes `semaphore: Option<u64>` (src/bpf/syscall.rs:386-392) |
| 6118 | LANDED | insn.rs: three comparisons, one addition, four helpers | src/bpf/insn.rs:150 `add64_reg`, :230/235/240 `jgt_reg`/`jge_reg`/`jne_reg`, :101/113/115/118 the four helper ids; no local copies left under src/bpf/ |
| 6127 | LANDED | coordinator: two registration lines and the indexes | src/bpf/mod.rs:49,82; src/collect/net/mod.rs:19; GUIDE.md:371-372 and examples/README.md:210-211 |
| 6171 | LANDED | man/omn.1: exit status and `=~` | man/omn.1:583-599 (1 = cannot serve here, 2 = cannot mean); man/omn.1:245-247 (`=~` is a substring test) |
| 6214 (unheaded, "Still open") | LANDED | drain racing the increment in runqlat and proc_syscall | generation slot / `map::select_rows` in src/bpf/runqlat.rs:87-89 and src/bpf/proc_syscall.rs:92,789; commit fe03ec0 |
| 6223 | LANDED | net/dev.rs `stop` reads nothing; capture refusal names `make CAPTURE=1` | src/collect/net/dev.rs:212-214 plus test `stopping_reads_nothing_because_the_runner_closed_the_window` (:235); src/collect/net/capture.rs:176-178 |
| 6244 | LANDED | `syscall == 110` drops every call pushed down | commit 455a818 ("compared with `nr` in the userspace pass") |
| 6262 | LANDED | `proc.exec` accepts a filter and applies none | commit 455a818 (filter applied, counted and said for proc.exec/exit/signals) |
| 6275 | LANDED | `--backend force:` walks past missing probes | commit 455a818 ("a pinned backend is refused before the launch when ... its probes are missing") |
| 6291 | LANDED | `mem.faults` `count` declared and refused | commit 455a818 (`count` is the kernel's page-fault counter through the counter group) |
| 6301 | LANDED | coordinator (stream X) | nothing was asked: "no registration lines and no sessions" |
| 6313 | LANDED | a row read through another key is rated by it (`via`) | `Part::via: Option<KeyKind>` src/join.rs:1688, `rate(by).min(rate(via))` src/join.rs:2471-2474; commits 478bc4f, 048e96e |
| 6356 | LANDED | a `to` event outside the `from` window (`outside_window`, `apply_within`) | `Correlation::outside_window` src/join.rs:332, `apply_within` src/join.rs:781-803; the validator refuses a join from a source with two different windows (comment at src/main.rs:771-774); `tests/runner.rs::a_join_across_a_detached_source_does_not_count_what_fell_after_it_as_a_loss` (:1576) is no longer ignored; commit 048e96e |
| 6400 | LANDED | main.rs: pass the windows to the join | src/main.rs:775-792 builds `windows` and calls `join::apply_within` |
| 6415 | LANDED | the window on `Results` | `Results::window: crate::collect::Watched` src/emit.rs:105 |
| 6435 | LANDED | counters.rs: what a late counter cannot see | the better fix: `ThreadCounters` opens one inheriting group per thread (commit 455a818); unit test `a_counter_opened_on_a_running_process_counts_the_threads_it_already_had` (src/collect/cpu/counters.rs:1220); `tests/runner.rs::a_counter_attached_late_counts_the_threads_already_running_or_says_it_cannot` (:1391) not ignored |
| 6464 | LANDED | cli.rs: `--window` | src/cli.rs:192-195 `window: Option<Window>`, :503 `window: args.window`, test `a_window_on_the_command_line_is_the_window_a_session_file_carries` (:618); man/omn.1:272 |
| 6481 | LANDED | `window: None` in every `Request` literal | src/resolve.rs:583, src/validate.rs:685, tests/catalog_model.rs:69 (the build requires it) |
| 6490 | LANDED | DECISIONS.md: the concurrent-requests line | DECISIONS.md:39 "One workload per run", rewritten as asked |
| 6509 | LANDED | a filtered part that matched nothing is a zero | src/join.rs:2422-2424 `Some(0.0)` with "no event of this key passed the filter"; commit 048e96e |
| 6536 | LANDED | a profile that follows threads | src/collect/cpu/profile.rs:16-20 ("It follows every thread", `TaskSampler`); commit 455a818 |
| 6548 | LANDED | coordinator: the triage fixture in `make` | Makefile:42 (`triage-fixture` in `build`), Makefile:64-65 |
| 6574 | LANDED | coordinator: `=~` in the manual and the assertion that waits for it | man/omn.1:245-247; `tests/scenario_runtime.rs::defect_2_the_match_operator_is_a_substring_test_and_says_so` no longer checks the manual |
| 6584 | LANDED | coordinator: DECISIONS.md triage lines | DECISIONS.md:237-238 |
| 6611 | LANDED | §47: the shim's cost quoted as 132 ns corrected | src/collect/sched/mutex.rs:20 cites finding 47 (9ca6983) |
| 6618 | LANDED | §47: bound the shim stream, batch the write | shared ring, `OMN_SHIM_RING_FD` (shim/src/lib.rs:56, src/collect/mem/heap/mod.rs:533), commit 0c7f897 |
| 6627 | LANDED | §47: attach dominates a short fentry run -- caveat | catalog/io.yaml:358, catalog/net.yaml:25 (abc6d37) |
| 6631 | LANDED | §47: NVML power read costs 34 ms -- poll slower | caveats catalog/gpu.yaml:78, 101 (abc6d37) |
| 6634 | LANDED | §47: `runtime.jit` reports 360 losses | jit_map.rs counts one failed read, test `polls_that_found_no_map_at_all_are_reported_at_the_end`, commit 0c7f897 |
| 6636 | LANDED | §47: tracing a contended program shortens it -- caveat | catalog/sched.yaml:79, 108, 206 (abc6d37) |
| 6688 | LANDED | `via: None` in `src/triage/latency.rs` | src/triage/latency.rs:194 |
| 6702 | LANDED | un-ignore the detach scenario, bound changed | tests/runner.rs:1576 no `#[ignore]`, bound `matched < FIRST + SECOND / 2 && unmatched < SECOND` at :1629 (b62c7c5, fe18ccf) |
| 6724 | LANDED | `tests/scenario_runtime.rs` not fmt-clean (J) | assert split over lines at tests/scenario_runtime.rs:929-932 (d40d500) |
| 6770 | LANDED | lift the join refusal once `apply_within` lands | `join::apply_within` (src/join.rs:781), used at src/main.rs:792; `nothing_reads_across_a_held_window` now refuses only decompositions (src/request.rs:1356), commit 131e124 |
| 6781 | LANDED | `Results::window` | `pub window: crate::collect::Watched` at src/emit.rs:105 (048e96e) |
| 6790 | LANDED | `DECISIONS.md`, the escalation's mechanism | DECISIONS.md:245 "Settled in the eleventh wave: how a run escalates" (fe18ccf) |
| 6808 | LANDED | index rows for `attach-escalate-phases.yml` | GUIDE.md:384, examples/README.md:223, docs/triage.md:586 (fe18ccf) |
| 6813 | LANDED | `tests/scenario_runtime.rs` not formatted on main (E) | same fix as line 6724 (d40d500) |

## Requests filed more than once

- 1809 and 2301: the cost bands. The number moved to `src/catalog.rs`, the words did not.
- 2539 and 7803: `KeyNotCarried`, waiting on `Backend.keys` (2493).
- 1503, 8067 and 8206: the one PSI reader, declared where it lives.
- 7930 and 8631: `ProgramStats::for_run` onto `bpf::enable_run_time_stats`.
- 4083 and 5566: two helpers still local to their programs (`GET_CURRENT_COMM`, `GET_CURRENT_TASK`), one line each in `src/bpf/insn.rs`.
- 5520 and 5536: `fentry.rs` keeps its own BTF parser and its own `bpf()` calls; both go through the shared layer, or neither does.
