# The open requests, as of the twelfth wave

State of `main` at f7b6a92 (2026-09-25). Every `### To ...` heading, every request bullet in an untitled `## Needs from another stream` section, and every numbered finding that asks for something, judged against the current code. Owners are the twelfth wave's `BRIEF.md` ownership table (R, V, W, D, M); "nobody" means a file outside every stream's set, which falls to the coordinator. A line number may appear more than once when one heading carries several requests.

Counts: LANDED 149, OPEN 95, PARTLY 27, SUPERSEDED 5.

`docs/open-findings.md` is append-only, and nothing in it marks a request
closed, so after eleven waves nobody could tell what was still wanted. This is
a dated snapshot, made by reading each request against the code: the next
brief is written from it, and it is redone rather than kept up to date.

**Closed by the coordinator since the snapshot** (twelfth wave, on `main`):
510 (the sudoers line: the recipe tries the absolute path first, the line is
escaped), 1360 (finding 19: measured at 1.2 s, the cache not needed), and the
documentation rows 168's `CATALOG.md` half, 1078, 1336, 1889, 2089, 2257,
2341, 4699, and 5256's `tests/cross_io.rs` and example halves. Stream M's
request for the evidence ladder in `src/join.rs` landed too. The rows owned by
R, V, W and D are with those streams now.

## OPEN and PARTLY

| line | status | heading | target file | what is left | owner this wave |
|---|---|---|---|---|---|
| 168 (§5) | PARTLY | `count` -> `raw` in the mode x emit matrix | `CATALOG.md` | code allows it (`src/request.rs:548`), but CATALOG.md §12 still lists `count` as `rate`, `summary` only | nobody |
| 168 (§5) | OPEN | a rate over the counter's own window | `src/emit.rs` | `emit::rate` still divides by `window_ns` (wall clock, `src/emit.rs:957-961`); `enabled_ns` on counted events is unused | R |
| 250 (§7) | PARTLY | `--pid a,b` measures only `a` | `src/request.rs`, collectors | some collectors take the list (`io/vfs.rs:86`, `mem/ibs.rs:177`, `runtime/usdt.rs:311`, `sched/mutex_usdt.rs:109`, the meter `collect/mod.rs:720`); ~25 call sites still use `Target::first_pid` and silently drop the rest, and nothing refuses a multi-pid request | R (request refusal); W/D for their collectors |
| 280 (§7) | OPEN | `examples/sched-wakeup.yml` can declare its join | `examples/sched-wakeup.yml` | the file still has no `joins:` block (wakeup joins exist only in the cross-* and join-decompose examples) | nobody |
| 314 (§8) | OPEN | `HistScale` cannot be serialized; `BucketLayout` duplicates it | `src/request.rs` | `HistScale` (`src/request.rs:683`) still derives no `Serialize`/`Deserialize`; `emit::BucketLayout` (`src/emit.rs:563`) still a copy | R |
| 323 (§8) | OPEN | `hist:linear:0` parses and should not | `src/request.rs` | `Emit::from_str` (`src/request.rs:628-632`) still accepts width 0; only the emit layer refuses it after the run (`src/emit.rs:728`) | R |
| 510 (§10) | OPEN | the sudoers line the Makefile documents cannot match | `Makefile`, `CLAUDE.md` | `Makefile:95` still passes relative `$(BINARY)` to `setcap` (no `$(abspath ...)`); the documented line (`Makefile:91`, `CLAUDE.md:104`) still has the unescaped comma | nobody |
| 546 (§11) | OPEN | `family_proc::a_count_without_its_limit...` panics on an index | `tests/family_proc.rs` | still indexes `["events"][0]` without first asserting the run produced events (`tests/family_proc.rs:143`) | W |
| 624 | OPEN | stream A: `sched.offcpu --emit hist` needs a log2 bucket in the key | `src/bpf/offcpu.rs` | the totals key is still `(tid, tgid, state, stack)` (`src/bpf/offcpu.rs:119-134`), no bucket; `docs/scenarios/sched.md` defect 1 still "half fixed" | D |
| 660 | OPEN | `sched.offcpu` (and `sched.wakeup`) declare `system: native` with an all-eBPF chain | `catalog/sched.yaml` | `targets` still `system: native` on both entries | V (catalog file) -- overlaps W's P8 (line 7121) |
| 710 (§12) | OPEN | symbols are not demangled | `src/stack.rs` (symbolizer) | no demangler in `src/` or `Cargo.toml` | nobody |
| 815 | OPEN | stream B to D: `check_stack` should refuse `dwarf`/`lbr` through `stack::callchain_for` | `src/validate.rs` | `check_stack` still only warns about dwarf cost (`src/validate.rs:579-583`); refusal happens only in the collector (`src/stack.rs:991`) | W |
| 885 | PARTLY | `SampledEvent::throttled()` accumulator | `src/perf.rs` | no accumulator in `src/perf.rs`; both known producers count `Record::Throttle` themselves (`mem/page_faults.rs:168-178`, `sched/switches.rs:133-151`), so a new sampling collector must still remember to | W |
| 957 | OPEN | `Unit::Watts` | `src/catalog.rs` | `enum Unit` (`src/catalog.rs:354-366`) has no `Watts` | W |
| 973 | OPEN | `render.rs`: a `gpu` line in the text environment dump | `src/render.rs` | `push_environment` (`src/render.rs:156-194`) still prints no `Environment::gpus` | R |
| 1040 | PARTLY | `emitted_fields(source_id, backend)` to cross the catalog's claim against the build | `src/collect/mod.rs` | per-collector tests `every_filter_field_the_catalog_declares_is_on_an_event` exist in ~16 collectors (e.g. `src/collect/sched/runqlat.rs:414`), but no `emitted_fields` and no catalog-wide sweep | R |
| 1078 | OPEN | `CATALOG.md` §1 documents a schema that has grown (per-backend `targets`/`filter_fields`) | `CATALOG.md` | §1 (`CATALOG.md:71-124`) now documents `equivalent`/`drained` but still not per-backend `targets` and `filter_fields` | nobody |
| 1241 | PARTLY | 16. every `bpf` backend credited with a filter | `src/resolve.rs`, `src/collect/mod.rs` | Collectors now apply the predicate themselves over drained rows (`src/collect/{sched,io,net,mem,proc,runtime}/mod.rs` read `request.filter`), but `placement()` still decides by `backend.id.starts_with("bpf")` (`src/resolve.rs:530`), so the resolved file still says `kernel`. Same ask as 2034 and 6972 (`filter_in_program`) | W |
| 1336 | PARTLY | 18. two false claims about kernel-side histograms | `src/validate.rs`, `docs/bpf-layer.md` | Warning conditioned (`budget_moves_hist`, `src/validate.rs:382`) and log2 bullet moved (`docs/bpf-layer.md:209`); but `docs/bpf-layer.md:195` still lists "fentry and USDT" as missing although `src/bpf/fentry.rs` and `src/bpf/usdt.rs` exist | nobody |
| 1360 | OPEN | 19. symbolising a machine-wide run, 2 min per second | `src/stack.rs` | Module cache is still per process (`Symbolizer.modules`, `src/stack.rs:357`, one `Symbolizer::for_process` per tgid in `sched/offcpu.rs:274`, `lock.rs`, `futex.rs`); no cache by binary, no lazy resolve, no bound | nobody |
| 1392 | OPEN | 20. `comm` is the name at row creation | `src/bpf/runqlat.rs`, `offcpu.rs`, `migrate.rs` | No re-read at exec, no `sched_process_exec` correction in those programs; first rows of a launch still say `comm=omn` | D |
| 1422 | OPEN | 21. a requirement can only be probed by reading (`writable:`) | `src/capabilities.rs` | No `writable:` prefix, no `access(W_OK)` probe; `mem.wss` still checks `clear_refs` at attach | V |
| 1469 | OPEN | 22. a NUMA node has no key | `src/event.rs` | No `KeyValue::Node` / `KeyKind::Node` (enum at `src/event.rs:89`) | nobody |
| 1503 | OPEN | 23. the PSI reader exists twice | `src/collect/mod.rs` (new `psi` module) | Still two copies, `src/collect/io/pressure.rs` and `src/collect/mem/pressure.rs`; no shared `psi` module | R (module in `collect/`), D (the two callers) |
| 1598 | PARTLY | 25. `pmu:` glob; qualified form answers vaguely | `src/capabilities.rs`, `src/catalog.rs` | Globs gone from the catalog (`catalog/io.yaml:232`); still no loader assertion refusing `*`, and a qualified `pmu:cpu/x` that is absent still answers Unknown (`sysfs_device_state` returns `None` on `/`, `src/capabilities.rs:853`) | V (probe), W (loader check) |
| 1649 | OPEN | 26. two readers of `cpu/online` | `src/capabilities.rs` | Now four copies: `perf.rs:1986`, `collect/power/sysfs.rs:85`, `bpf/attach.rs:560`, `bpf/syscall.rs:593` | V |
| 1735 | OPEN | 28. cost band per backend, run cost is per rate | `src/catalog.rs`, `CATALOG.md`, `DECISIONS.md` | The decision (per-unit cost vs a band stated at a rate) is not taken; nothing in `DECISIONS.md` or `catalog.rs` | nobody (decision), then W |
| 1809 | OPEN | 29. `Cost::Free` had no number (bands belong in `catalog.rs`) | `src/catalog.rs` | Bands still in `src/main.rs:611` `worst_case_percent`; same ask as 2301 | W |
| 1845 | OPEN | 31. instrumentation inside the target charged to it | `shim/`, `src/collect/mem/heap/`, `src/collect/mod.rs` (`Overhead`) | No `in_target_cpu_ns` on `Overhead` (`src/collect/mod.rs:542`); the shim publishes no cpu time | D (shim side), R (`Overhead` field) |
| 1889 | OPEN | Needs: update `docs/families/cpu.md` (4 passages) | `docs/families/cpu.md` | All four passages still stale (lines 189-231: topdown "unverified", oncpu_time "Wave 2", cpu key and per-cpu counting still listed as needs) | nobody |
| 1889 | OPEN | Needs: coordinator, finding 10 sudoers `abspath` | `Makefile` | `BINARY = target/$(PROFILE)/omn` still relative, `sudo -n setcap $(CAPS) $(BINARY)` (`Makefile:22,95`) cannot match the absolute sudoers rule | nobody |
| 1959 | OPEN | To `capabilities.rs`: probe hook for `net` (ethtool) | `src/capabilities.rs`, `src/collect/net/mod.rs` | No `collect::net::probe`; `ethtool` still deliberately unanswered (`src/capabilities.rs:998`), so `net.hwts` / `net.iface netlink` read unverified | V (+D for `net/mod.rs`) |
| 2034 | OPEN | To `resolve.rs`: `placement()` wrong about `af_packet` | `src/resolve.rs`, `src/catalog.rs` | No `kernel_filter` / `filter_in_program` on `Backend`; placement still by id prefix (`src/resolve.rs:530`) | W |
| 2056 | PARTLY | To `emit.rs`: a missing value is not a non-numeric one | `src/emit.rs` | `measured()` counts absent values as unmeasured in summary/hist/topk (`src/emit.rs:922`, 307fa56); `timeline`, `rate` and `stack` still call `value_of` and fail the run with `NotNumeric` (`src/emit.rs:946,966,1203`) | R |
| 2089 | OPEN | To `CATALOG.md`: the `net.hwts` row | `CATALOG.md` | Row unchanged (`CATALOG.md:254`), silent about the `netlink:ethtool` backend | nobody |
| 2102 | OPEN | To `render.rs`: a series whose interest is its variation | `src/render.rs` | No first-value-then-deltas rendering | R |
| 2127 | OPEN | 33. `Results.period_ns` for pprof | `src/emit.rs` | No `period_ns` on `Results`; pprof `period` still zero (`src/encode.rs:1560`) | R |
| 2127 | PARTLY | 33. a ratio/temperature profile sums what cannot be added | `src/validate.rs` / catalog loader | `check_catalog` refuses `rate` over a non-additive unit (`src/validate.rs:290`) but not `stack` | W |
| 2257 | OPEN | To `README.md` and `DECISIONS.md`: formats "not written" | `README.md`, `DECISIONS.md` | Both still say it (`README.md:174`, `DECISIONS.md:20` "ndjson, folded ... declared") | nobody |
| 2265 | OPEN | To `render.rs`: `chrome` carries raw events | `src/render.rs` | Still "`chrome` carries timelines" (`src/render.rs:476`) | R |
| 2280 | OPEN | To `request.rs`: `Budget` doc comment | `src/request.rs` | Doc still says "The resolver degrades ... `trace` to in-kernel `hist` to `sample`" (`src/request.rs:895`) | R |
| 2301 | OPEN | To `catalog.rs`: `Cost::worst_case_percent` / `band` | `src/catalog.rs` | Still in `src/main.rs:611,625` | W |
| 2316 | OPEN | To stream D (`emit.rs`): `Results.overhead` and `served_by` | `src/emit.rs`, `src/encode.rs` | Neither field on `Results` (`src/emit.rs:85`); cost still printed separately (`src/main.rs:829`) | R |
| 2341 | PARTLY | To the coordinator (ROADMAP ticks, CATALOG flag table, STATE item 5) | `ROADMAP.md`, `CATALOG.md` | STATE "not enforced" item is gone; `CATALOG.md:440` still says "the resolver degrades", and ROADMAP phase 6 "Exceeding the budget produces an announced degradation" is still unticked | nobody |
| 2374 | PARTLY | To D (`switches.rs`) and E (`page_faults.rs`): `not_built` | `src/collect/sched/switches.rs` | `page_faults.rs` superseded (count is served by the counting group, `page_faults.rs:19,208`); `switches.rs:177` still returns a bare `Err(String)` for `count`, not `collect::not_built`, so it exits 1 | D |
| 2420 | OPEN | To `resolve.rs`: a backend that cannot serve the requested mode (mem D7) | `src/resolve.rs`, `src/catalog.rs` | Modes still per entry (`src/catalog.rs:54`), `Backend` has no modes; same as P7 at 7107 | W |
| 2467 | OPEN | To `request.rs`: `count` mode must be able to rank | `src/request.rs` | `ModeKind::Count => &[Rate, Summary, Raw]` (`src/request.rs:548`), no `TopK` | R |
| 2493 | OPEN | To `catalog.rs`: `check_catalog` in the loader, and `Backend.keys` | `src/catalog.rs` | `check_catalog` still runs only from a unit test (`src/validate.rs:813`); no `keys` field, `backend_keys`, `check_backend_keys` | W |
| 2539 | OPEN | To `resolve.rs`: refuse a key the backend cannot attach | `src/resolve.rs`, `src/error.rs` | No `check_keys_carried` / `KeyNotCarried` | W |
| 2585 | PARTLY | To F (`net/iface.rs`): one event per counter kind (net 5) | `src/collect/net/iface.rs` | Hidden counters now stated in notes (`iface.rs:69`, `hidden`); series still errors only, no split by counter | D |
| 2600 | OPEN | To C (`emit.rs`): a ranking over an attribute (runtime 1) | `src/emit.rs` | `topk_by` groups by key axes only (`src/emit.rs:1140`), not by an attribute | R |
| 2619 | OPEN | To E (`collect/mem/`): a counted window for `mem.swap` | `src/collect/mem/`, `catalog/mem.yaml` | `mem.swap` still `modes: [poll, snapshot]`, no `rate` (`catalog/mem.yaml:461`) | D (collector), V (catalog line) |
| 2641 | OPEN | To D (`sched/offcpu.rs`): `TOTAL_OF_ATTR` closes sched 1 | `src/collect/sched/offcpu.rs` | No collector emits `TOTAL_OF_ATTR`; `docs/scenarios/sched.md:584` still "half fixed" | D |
| 2693 | OPEN | To A (`collect/mod.rs`, `error.rs`): mem D4 `UnsupportedTarget` reason | `src/collect/mod.rs` | Variant still `{ source_id, target }` with the reason inside `target` (`src/collect/mod.rs:112,1243`) | R |
| 2716 | OPEN | To A (`collect/mod.rs`): `Filtered.near_misses` (net 6) | `src/collect/mod.rs`, `src/encode.rs` | No `near_misses` anywhere | R |
| 2745 | OPEN | To F (`collect/net/`): `daddr` carries the port (net 6) | `src/collect/net/netlink.rs` / `tcp_rtt.rs` | `render` still appends `:port` (`src/collect/net/netlink.rs:65,72`), no caveat in `catalog/net.yaml` | D |
| 2755 | OPEN | To `request.rs`: `topk:N@attr` (runtime 1, io 7) | `src/request.rs`, `src/emit.rs`, `src/validate.rs` | `Emit::TopK(u32)` (`src/request.rs:586`), no `by` | R (request/emit), W (validate check) |
| 2883 | OPEN | To `tests/scenario_cpu.rs`: profile scenario takes the shared lock | `tests/scenario_cpu.rs` | Still `may_load_the_machine()` (`tests/scenario_cpu.rs:1025`) | W |
| 2915 | OPEN | From stream A: a unit test that fails under load (`what_a_run_reports_spending_is_the_cpu_it_actually_burned`) | `src/collect/mod.rs` | Test still stops on a 200 ms wall deadline (`until(deadline_ns)`, src/collect/mod.rs:3673) and panics on `NOTHING_WAS_BURNED`; stop condition should be `spun_ns >= 200 ms`. Still in the flaky tally (docs/scenarios/cross-machine.md:465, open-findings.md:6678) | R |
| 2947 | OPEN | From stream A: a counted window rounds up to the tick (`-m count -d 600ms` observes 1.0002 s) | `src/collect/mod.rs` | `pace_of` still gives `Mode::Count` `DEFAULT_POLL_INTERVAL` (src/collect/mod.rs:2317-2320) and the drive loop's sleep (mod.rs:2572-2590) does not include the run deadline in `next_due_ns`; a counted request's window should end at its duration | R |
| 3054 | PARTLY | To stream C, `src/emit.rs`: a series cannot be split by which counter it is (net 5) | `src/emit.rs` | Landed: collector states the omission as a degradation (docs/scenarios/net.md:793 "half fixed"). Left: no `Event.series` / sub-series; `SeriesKey::by` groups only by `KeyKind` (src/emit.rs:182) | R |
| 3092 | PARTLY | To stream C, `src/emit.rs`: a timeline point cannot carry its error bar (gpu 1) | `src/emit.rs` (or `catalog/gpu.yaml`) | Landed: error bar stated as a degradation (docs/scenarios/gpu.md:401 "half fixed"). Left: `Point` has no `ts_err_ns` (src/emit.rs:330-335) and `gpu.engine_busy` still does not declare `raw` (catalog/gpu.yaml emits `[timeline, summary]`) | R (emit.rs) or V (gpu.yaml) |
| 3119 | OPEN | To stream B, `catalog/net.yaml`: a caveat now true of two modes of three | `catalog/net.yaml` | Old caveat "These are counters, so a run reports the increment..." still at catalog/net.yaml:208 and :370; snapshot wording not added | V |
| 3139 | OPEN | To stream B / whoever takes §1: modes are declared per entry, not per backend | `src/catalog.rs`, `src/validate.rs` | `Backend` has no `modes` field (src/catalog.rs:154-205); validator checks only `entry.modes` (src/validate.rs:277,354); `gpu.engine_busy --mode count` on NVML still accepted | W |
| 3713 | OPEN | Sixth wave A item 5: a block key the plug's cached clock cannot collapse | `src/event.rs`, `src/join.rs` | `KeyValue::Blk { ptr, open_ts }` unchanged (src/event.rs:113); no third component; also listed under "Left" in docs/families/shared-layer.md | nobody |
| 3760 | PARTLY | To `src/bpf/btf.rs`: member lookup that walks anonymous members (net callers) | `src/bpf/btf.rs` -> `src/bpf/net_*.rs` | Mechanism landed (`Btf::member_offset` src/bpf/btf.rs:181, finding 41). Left: callers -- `net.tcp.lifetime`/`retransmit` still report only the local endpoint (src/bpf/net_lifetime.rs:22-30, src/collect/net/lifetime.rs:204), `net.drops` reads no `skb->protocol`/`skb_iif`/`dev` (src/bpf/net_drops.rs:24) | D |
| 3796 | PARTLY | To `src/bpf/btf.rs`: the names of an enum's members | `src/bpf/btf.rs` -> `src/collect/net/drops.rs` | `Btf::enum_variant` landed (src/bpf/btf.rs:290, used by io_pagecache.rs:275). Left: `net.drops` still reports the reason as a bare number and its doc comment still says BTF enums cannot be read (src/collect/net/drops.rs:14-21) | D |
| 3883 | OPEN | To `src/emit.rs`: a ranking over an attribute, met a second time (`net.drops` topk by reason) | `src/emit.rs`, `src/request.rs` | `Emit::TopK(u32)` only (src/request.rs:586); no `topk:N@<attr>`; `topk_by` groups by key axes (src/emit.rs:799) | R |
| 3892 | OPEN | To `src/emit.rs` and `src/render.rs`: a note is not always a degradation | `src/emit.rs`, `src/encode.rs` | `Results` has only `degradations` (src/emit.rs:133); collector notes are still folded into it (src/emit.rs:750-752) | R |
| 3972 | OPEN | `catalog/mem.yaml`: `direction` is not a filter field (page_alloc/kmalloc frees) | `src/collect/mem/**` (reframed in shared-layer.md "Something else") | Neither option taken: `mem.page_alloc` filter_fields `[order, comm]`, `mem.kmalloc` `[bytes, comm]`; frees still counted into a note, not events | D |
| 4083 | PARTLY | To `src/bpf/insn.rs`: two helper numbers | `src/bpf/insn.rs` | Landed (insn.rs:94, :110). Left: `src/bpf/proc_syscall.rs:42-52` still declares a local `extra_helper::GET_CURRENT_COMM` and calls it at :382 | D |
| 4168 | OPEN | To `src/bpf/attach.rs`/`map.rs`: narrow a program to a cgroup (proc) | `src/bpf/*`, `src/collect/proc/**` | No `CURRENT_TASK_UNDER_CGROUP`/`CGROUP_ARRAY`; `proc.exec/exit/signals` still refuse cgroup/container "needs a helper the eBPF layer does not have yet" (src/collect/proc/events.rs:168, :574) though catalog declares `cgroup: bpf_only` (catalog/proc.yaml). `GET_CURRENT_CGROUP_ID` exists only in src/bpf/mem_program.rs:41 | W (proc collectors) + D/V (eBPF layer) |
| 4292 | OPEN | To `src/bpf/`: a bounded read at an offset the verifier has not seen (`__data_loc`) | `src/bpf/` (insn), `src/bpf/sched_drm.rs` | `attach::tracepoint_layout` flags `__data_loc` (attach.rs:543) but no variable-offset ctx read; `gpu.job` still carries the scheduler thread name (sched_drm.rs:138-143) | nobody (shared insn layer); D for sched_drm.rs |
| 4304 | PARTLY | To stream B, `catalog/sched.yaml`: `sched.irq` promises the soft half | `catalog/sched.yaml`, `src/bpf/sched_irq.rs` | Catalog half landed as "say what it measures": summary is now "Time spent in hard interrupt handlers" (catalog/sched.yaml:285). Left: soft half not attached (src/bpf/sched_irq.rs:22-23) | D |
| 4328 | OPEN | To `src/bpf/` and `src/collect/mod.rs`: no `sched` source serves a cgroup | `src/collect/sched/**`, `src/bpf/sched_*.rs` | catalog/sched.yaml still declares `cgroup: bpf_only` on six entries; collectors still reject any non-pid target (e.g. src/collect/sched/runqlat.rs:70-78); no cgroup-path-to-id resolution in the sched path | D |
| 4662 | OPEN | per-range residency of live allocations | `src/collect/mem/heap/` | `mem.heap.live` publishes no per-range `KeyValue::Mem` event with `resident`; `pagemap.rs` has `resident` but only over freed ranges (catalog dropped `mem` instead, D2) | D |
| 4699 | PARTLY | the mTHP explanation is contradicted by measurement (D4) | `src/encode.rs`, `catalog/mem.yaml`, `DECISIONS.md` | encode.rs note fixed (`src/encode.rs:330`); `catalog/mem.yaml:108` caveat still says "the kernel picks larger folios for larger regions", and `DECISIONS.md:62` still credits multi-size THP | V (catalog), nobody (DECISIONS.md) |
| 4900 | OPEN | P3 defect 1: `sched.wakeup`/`sched.offcpu` apply the filter or refuse it | `src/collect/sched/wakeup.rs`, `offcpu.rs` | no predicate in either collector; `tests/cross_io.rs:1345` `the_scheduler_half_ignores_a_filter_and_says_nothing` still asserts the defect (re-filed at line 6925) | D |
| 4927 | OPEN | P3 defect 2: a thread target that compares the thread | `src/collect/sched/*.rs` | `wakeup.rs:48` still hands `first_pid()` to a tgid filter; only `futex.rs` has a `Thread` variant; `tests/cross_io.rs:1402` still pins the defect | D |
| 4968 | OPEN | P3: a match that pairs a close with the next wakeup of its task (`woke`) | `src/join.rs`, `src/catalog.rs` | no `Woke` variant in `MatchKind` (`src/catalog.rs:228-239`) | nobody (join.rs), W (catalog.rs) |
| 4988 | PARTLY | P3: a decomposition (segments by join, whole `io.syscall`) | `src/request.rs`, `src/join.rs`, `src/encode.rs` | `decompose:` with `by` and residue landed (`src/join.rs:1576`, 2850c5d); a part defined by a join (`join: halves`, `completion-released`) and a traced enclosing span as whole are not | nobody (join.rs) |
| 5032 | PARTLY | P4: a match kind whose evidence is `TEMPORAL` (`Overlap`) | `src/join.rs`, `src/catalog.rs` | TEMPORAL is now produced (`between`, `within_key`); the window-overlap match with a shared split, keyless for machine state, does not exist | nobody (join.rs), W (catalog.rs) |
| 5099 | OPEN | P4: a per-tick read of a counting PMU event | `src/collect/cpu/`, `catalog/cpu.yaml` | `cpu.instructions` still `modes: [count, sample]`, `cpu.cycles` `[count]` (`catalog/cpu.yaml:28,47`); no `poll` | W (collector), V (catalog) |
| 5132 | OPEN | P4: 2.6 MiB of flusher progress beside 90 MiB of kworker writes | `src/collect/io/writeback.rs` | never investigated; cgroup scope still refused as not built (`writeback.rs:55`) | D |
| 5256 | OPEN | "one run observes one target" is no longer true | comments in `src/main.rs`, `src/encode.rs`, tests, example | comments still there: `src/main.rs:742`, `src/encode.rs:1978`, `tests/cross_io.rs:686`, `tests/scenario_mem.rs:2033`, `tests/scenario_sched.rs:2133`, `examples/cross-machine-frequency.yml:54,81` | R (main.rs, encode.rs), D (scenario_mem/sched), nobody (cross_io.rs, example) |
| 5272 | OPEN | known-instruction scenario flaps under load (+0.69 %, double count suspected) | `tests/scenario_cpu.rs` | not investigated; only a `may_load_the_machine()` guard (`tests/scenario_cpu.rs:350`) | W |
| 5285 | PARTLY | `uprobe` PMU needs CAP_SYS_ADMIN, add uprobe-multi attach | `src/bpf/attach.rs`, `src/capabilities.rs` | Attach half is superseded: it lives in `syscall::link_uprobe_multi` (src/bpf/syscall.rs:386) and `usdt.rs:1060-1096`. The probe half is still open: `attach::probe_type_support("uprobe")` (src/bpf/attach.rs:199-211) still answers from the PMU `type` file, so `uprobe` reads available where every open gets EACCES. This overlaps P3 (line 7067) | V |
| 5353 | PARTLY | `insn.rs`: five encodings and a helper (USDT) | `src/bpf/insn.rs` | `add64_reg`, `lsh64_imm` and `PROBE_READ_USER` landed (insn.rs:150,155,101). `arsh64_imm`, `or64_imm` and `ld_imm64` are still local in `usdt.rs` `mod local` (usdt.rs:769-778), justified as single-caller. But `or64_imm` is now written a second time in `request_net.rs:159` | nobody |
| 5372 | OPEN | `resolve.rs`: the kernel placement is true for part of a filter | `src/resolve.rs` | `placement()` (resolve.rs:530) still credits every `bpf*` backend with "evaluates predicates in the program". USDT's `comm` predicate runs in the collector. This overlaps line 6972 ("where a predicate actually runs") | W |
| 5520 | OPEN | `btf.rs`: a function's type id and prototype | `src/bpf/btf.rs` | There is no `Btf::function`. The second BTF parser `fentry::FunctionIndex` is still in place (src/bpf/fentry.rs:168-331) | V |
| 5536 | OPEN | `syscall.rs`: four commands `fentry.rs` issues itself | `src/bpf/syscall.rs` | There is no `prog_load_tracing` with `attach_btf_id`: `prog_load_for` (syscall.rs:290) has no btf id. `raw_tracepoint_open` still takes `&str` (syscall.rs:337). `map_update` is still `#[cfg(test)]` (syscall.rs:461). There is no `prog_info`. `fentry.rs` still issues `BPF_PROG_LOAD`, `RAW_TRACEPOINT_OPEN` and `OBJ_GET_INFO_BY_FD` itself (fentry.rs:68-69,504,554) | nobody |
| 5553 | OPEN | `bpf/mod.rs`: five module lines | `src/bpf/mod.rs`, `src/bpf/attach.rs` | `fentry`, `io_fsync`, `io_vfs`, `net_connect` and `net_socket` are still declared with `#[path]` in `attach.rs` (attach.rs:57-69), and none of them is in `mod.rs` | V |
| 5566 | PARTLY | `insn.rs`: one width, three instructions, two helpers | `src/bpf/insn.rs` | `BPF_H`, `add64_reg`, `lsh64_imm`, `jge_reg` and `GET_SOCKET_COOKIE` landed (insn.rs:40,150,155,235,99). `GET_CURRENT_TASK` (35) is still local in `io_fsync.rs:54` | nobody |
| 5642 | OPEN | `tests/family_sched.rs`: a fixed attach delay | `tests/family_sched.rs` | `a_task_moved_between_two_cpus_is_counted_once_per_move` still sleeps a fixed 800 ms before moving the task (family_sched.rs:882) | D |
| 5652 | OPEN | `request.rs`: `count` on a kernel-built histogram | `src/request.rs` | `ModeKind::Count` still admits only rate, summary and raw (request.rs:548). There is no hist or topk for a counted drained histogram. Not blocking | R |
| 5659 | PARTLY | To the coordinator (stream T): session rows, `docs/bpf-layer.md`, `CATALOG.md`, `STATE.md` | `examples/README.md`, `GUIDE.md`, `docs/bpf-layer.md`, `CATALOG.md` | The four session rows landed. `docs/bpf-layer.md:190-199` "What is not here yet" still lists fentry and USDT as missing. `CATALOG.md` does not name the `fentry:<function>` requirement class | nobody |
| 6181 | OPEN | Stream H: three left from the fifth wave -- (a) `count` refuses `topk` | `src/request.rs` | `ModeKind::valid_emits` still gives `Count => [Rate, Summary, Raw]` (src/request.rs:548); no `TopK` arm | R |
| 6181 | OPEN | Stream H: three left -- (b) the counted window rounds up to the runner's tick | `src/collect/mod.rs` | the run deadline is still not among the wake-up times (`next_due_ns`, src/collect/mod.rs:2572-2590) and `pace_of` still falls back to `DEFAULT_POLL_INTERVAL` for `count` (src/collect/mod.rs:2319), so `-d 600ms` observes about 1 s | R |
| 6181 | OPEN | Stream H: three left -- (c) the spending test drives to a wall deadline | `src/collect/mod.rs` (test) | `what_a_run_reports_spending_is_the_cpu_it_actually_burned` (src/collect/mod.rs:3662) still uses `until(monotonic_ns() + 200 ms)`; it only panics with NOTHING_WAS_BURNED instead of driving to a cpu deadline | R |
| 6602 | OPEN | §47: cost bands describe the tool's cpu; an in-kernel collector's cost is in the target | `src/collect/mod.rs` (budget, `cost.json`), `catalog/*.yaml` | `cost.json` and `--budget` do not read the target's dilation (no "dilat" anywhere in `src/`); raw-tp per-event bands not revisited (`sched.futex` `bpf:raw_tp` still `cost: med`, catalog/sched.yaml:223) | R (budget), V (bands) |
| 6642 | PARTLY | §47 "Answered on main": the shim ring (two things left) | `catalog/mem.yaml`, `catalog/sched.yaml`, `docs/bench.md` | ring caveat landed in catalog/sched.yaml:263 only; catalog/mem.yaml has no "up to 3 MiB of `mem.rss` is the ring" caveat; bench heap rows still predate the ring (docs/bench.md:379 says "these rows want one more rerun") | V (mem.yaml caveat); nobody (bench rerun) |
| 6739 | OPEN | held request that does not resolve is an unserved layer, not a failed run | `src/main.rs`, `src/collect/mod.rs` | main.rs:208-224 still returns at the first `ResolveError`; no `Unserved` / `run_all_within_unserved` | R |
| 6832 | OPEN | same drain race in `io_pagecache.rs`, `net_qdisc.rs`, `net_softirq.rs` | `src/bpf/io_pagecache.rs`, `net_qdisc.rs`, `net_softirq.rs` | all three still `Hash::create` their row map (io_pagecache.rs:610, net_qdisc.rs:351, net_softirq.rs:347); none on `map::Rows` | D (BRIEF D item 2) |
| 6857 | OPEN | fentry last drain's 1 ms is not a grace period | `src/bpf/fentry.rs` | still `sleep(from_millis(1))` at fentry.rs:1343 | D (BRIEF D item 3) |
| 6871 | OPEN | one grace period per round, not per source | `src/bpf/map.rs`, `src/collect/mod.rs` | no `retire`/`take`, `grace_period` still private (map.rs:354), no `Collector::retire` | D (map.rs) + R (collector trait/round loop) |
| 6891 | OPEN | `stack` over `mem.latency` fails one run in ten | `src/collect/mem/ibs.rs`, `tests/scenario_mem.rs` | not diagnosed; ibs.rs still emits `latency_cycles` conditionally, error at emit.rs:726 still reachable | D (BRIEF D item 5) |
| 6925 | OPEN | three `sched` sources apply no filter | `src/collect/sched/mod.rs` (+ wakeup/offcpu/migrations collectors) | factories at sched/mod.rs:36, 39, 57 still pass no `filter` | D |
| 6952 | OPEN | `sched.switches` sampling event that follows threads | `src/collect/sched/switches.rs` | still `SampledEvent::open_software` (switches.rs:187), no `ThreadSampler` | D (BRIEF D item 4) |
| 6972 | OPEN | where a predicate actually runs (`filter_in_program`) | `src/catalog.rs`, `src/resolve.rs`, `catalog/*.yaml` | no `filter_in_program` key anywhere in `src/` or `catalog/` | W (BRIEF W item 2) |
| 6989 | OPEN | §48: a wakeup from an interrupt credited to the interrupted task | `src/bpf/wakeup.rs` | no `preempt_count` / `HARDIRQ_MASK` read in wakeup.rs | D (BRIEF D item 1) |
| 7028 | OPEN | P1: check the prototype in the probe | `src/bpf/mod.rs`, catalogs | `raw_tracepoint_support(name)` (bpf/mod.rs:153) takes a bare name; `KNOWN_DEFECTS` P1 entries still in tests/portability.rs:1563,1570 | V |
| 7049 | OPEN | P2: a BTF field the program reads | `src/capabilities.rs`, `src/bpf/mod.rs`, `catalog/net.yaml` | no `btf_member:` requirement class / `btf_member_support`; KNOWN_DEFECTS portability.rs:1579 | V |
| 7067 | OPEN | P3: `uprobe` requirement below 6.6 | `src/capabilities.rs`, `src/bpf/mod.rs` | `uprobe` still answered by the PMU's existence; KNOWN_DEFECTS portability.rs:1586 | V |
| 7079 | OPEN | P4: USDT backends need BTF | `catalog/runtime.yaml` | `bpf:usdt` still `requires: ["uprobe", "bpf"]` (runtime.yaml:15, 78, 104); KNOWN_DEFECTS portability.rs:1593 | V |
| 7087 | OPEN | P5: `net.request` below 6.6 | `src/bpf/request.rs`, `catalog/net.yaml` | no fallback, no `kernel>=6.6` in net.yaml; KNOWN_DEFECTS portability.rs:1600 | V |
| 7096 | OPEN | P6: NVML library without its driver | `catalog/gpu.yaml` or `src/collect/gpu/mod.rs` | no `/proc/driver/nvidia/version` requirement, probe does not call `nvmlInit_v2`; KNOWN_DEFECTS portability.rs:1607 | V |
| 7107 | OPEN | P7: a collector that refuses the mode | `src/resolve.rs`, `src/collect/mod.rs` | no `Factory::modes`; KNOWN_DEFECTS portability.rs:1620-1641 (four entries) | W |
| 7121 | OPEN | P8: `system: native` refused | `src/collect/cpu/profile.rs`, `src/collect/mem/page_faults.rs`, `src/collect/sched/` | KNOWN_DEFECTS portability.rs:1648-1662 (three entries) | W (cpu, page_faults), D (sched half) |
| 7130 | OPEN | P9: a per-task probe for a per-cpu event | `src/resolve.rs` | KNOWN_DEFECTS portability.rs:1669 | W |
| 7139 | OPEN | P10: two requirements that name the wrong thing | `catalog/power.yaml`, `catalog/mem.yaml` | power.yaml:56 still requires `/sys/devices/system/cpu/cpufreq`; `mem.swap` `fs:cgroup` requires nothing about `pswpin` | V |
| 7150 | OPEN | five tests fail without capabilities, and a skip with no reason | `tests/vertical.rs`, `tests/scenario_mem.rs`, `tests/scenario_proc.rs`, `tests/scenario_gpu.rs` | tests unchanged since the matrix merge; vertical.rs:921, 997, 1104 still print `stderr.lines().next()` | W (vertical.rs, scenario_proc.rs x2, skip reason), D (scenario_mem.rs), V (scenario_gpu.rs) |
| 7182 | PARTLY | `make matrix`, and when to push the workflow | `Makefile`, `.github/workflows/ci.yml` | `make matrix` landed (Makefile:130, 25e65f7); the workflow has never been pushed (4ca6aa4 is on no remote branch; main is 13 ahead of origin/main) | nobody (coordinator's push; `.github/**` is V's to adjust) |

## LANDED and SUPERSEDED

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

- Lines 510 and 1889 ask for the same fix: the sudoers line and `$(abspath)` in the `Makefile`.
- Finding 16, line 2034, line 5372 and line 6972 ask for the same thing: `placement()` in `src/resolve.rs:530` still decides where a predicate runs from the backend id's `bpf` prefix. That is W's.
- Line 2420 and P7 (line 7107) are the same request: the resolver still selects a backend that cannot serve the mode.
- Line 2301 and finding 29 are the same request: the cost bands in `src/main.rs:611` should move to `src/catalog.rs`.
- Line 4900 and line 6925 are the same request: the `sched` sources apply no filter. That is D's.
- Line 5285 (the probe half) and P3 (line 7067) are the same request: the `uprobe` probe. That is V's.
- Lines 2915/2947 and 6181 repeat the fifth wave's `count` window and the wall-deadline spending test. Those are R's.
- Lines 1336 and 5659 are the same request: `docs/bpf-layer.md` still lists fentry and USDT as missing.
