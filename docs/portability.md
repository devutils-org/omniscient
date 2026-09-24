# Portability: the kernel matrix, the degradation runs, and the CI

`ROADMAP.md`'s test strategy had two rows nobody had built:

| Row | What it asks |
|---|---|
| Portability | a VM kernel matrix: floor, LTS, recent, no BTF |
| Degradation | run without privileges, without BTF, without tracefs -- check that it degrades **while saying so** |

Until this wave every claim the tool makes about another machine had been
checked on one. This document is stream P's deliverable for the eleventh wave:
what the matrix is, how to run it, what it checks, and what twenty-two cells
found. The CI workflow is the last section.

---

## In one paragraph

Twenty guests -- four kernels under five configurations -- plus the host with
and without its capabilities, each
running the capability sweep: every one of the 101 sources listed, and every
source **run**, 140 runs a cell, because available means it attaches. Every
answer carried its reason in every cell, the floor was refused by name, the
uprobe-multi fallback said so on 6.1 and never on 6.6, and every removed
requirement (BTF, tracefs, the capabilities) was named in every probe that
depends on it. **What failed is "available means it attaches"**: between 19
and 33 runs a cell were listed usable and refused when started. They fall into
ten classes, P1 to P10 below. Seven are finding 15's shape -- the probe answers
a narrower question than the attach asks -- and three of those (P1, P2, P5)
are exactly what a kernel matrix exists to find: they pass on the host's 7.2
and fail on 6.6, the LTS just under it. None of them degrades silently: every
refusal names what it lacked. Each is filed in `docs/open-findings.md` with the
signature wanted, and each is recorded in `tests/portability.rs` so the cell
stays judged -- an entry that stops matching fails the test and has to go.

---

## Running it

```console
scripts/matrix.sh                      every kernel under every configuration (about 70 min)
scripts/matrix.sh here                 this machine alone, no guest (about 2 min)
scripts/matrix.sh v6.1.188 --configs caps,root
scripts/matrix.sh check                judge the cells already on disk
cargo test --test portability          the same judging, as part of the suite
```

The `Makefile` is the coordinator's; the target this stream would add is:

```make
# The portability matrix: docs/portability.md. Boots prebuilt kernels in KVM
# under virtme-ng; about 70 minutes and 2 GB of kernels under target/matrix/.
matrix: build
	@scripts/matrix.sh
```

(and `matrix` in `.PHONY`). `scripts/matrix.sh` runs `make` itself, so the
recipe's `build` prerequisite only saves a second relink check.

**What it needs**: `/dev/kvm`, `qemu-system-x86_64`, `virtme-ng`
(`pipx install virtme-ng`; 1.41 here), `jq`, `bsdtar`, and the network the
first time. A host without `busybox` gets a static one fetched into
`target/matrix/` and checked against a pinned digest, since it runs as the
guest's init.

### How a cell is made

- **The kernels are prebuilt**, Ubuntu's mainline builds, fetched by
  `scripts/vm/fetch-kernel.sh` into `target/matrix/kernels/<version>/` --
  image and modules only, about 500 MB each, extracted with `bsdtar` because an
  Arch host has no `dpkg`. The host's own kernel boots from `/boot`. Nothing is
  compiled but the tool.
- **The guest runs the host-built binary** on the host's filesystem, read-only
  through virtiofs with a copy-on-write overlay, so what differs between cells
  is the kernel and the configuration and never the build.
  `--disable-microvm` is not optional: Ubuntu builds `virtio_mmio` as a module
  the initramfs does not load, and a microvm has nothing else, so the guest
  panics looking for its root.
- **The configuration is made inside the guest** by `scripts/vm/cell.sh`, and
  checked before anything is measured -- a cell that believes it hid BTF and did
  not would pass every judge for the wrong reason:

| Config | uid | What is different |
|---|---|---|
| `caps` | 1000 | the deployment `make` produces: `cap_bpf,cap_perfmon+ep` on the binary. virtiofs drops the host's file capabilities, so `cell.sh` grants them again on the guest's copy-on-write view; the host's file is untouched |
| `root` | 0 | nothing removed |
| `nocaps` | 1000 | no capability on the binary |
| `nobtf` | 0 | `/sys/kernel/btf` under an empty tmpfs: the file the tool reads is absent, which is all a kernel without BTF is to it |
| `notracefs` | 0 | tracefs and debugfs unmounted (debugfs goes too, because it automounts tracefs on first access) |

The two removals run as root so each differs from `root` by exactly the thing
removed: as uid 1000 tracefs is already unreadable, and hiding it would change
nothing the tool can see.

- **The sweep** is `sweep_this_machine`, an `#[ignore]` test in
  `tests/portability.rs`, run inside the guest as the configured user. It asks
  `omn list <source>` for every source and records the listing verbatim, then
  runs each source on up to two targets -- a launched program and the whole
  machine, the two that need no setup; a source that takes neither is run on a
  cpu, an interface, a pid or a cgroup -- for 300 ms, with a watchdog. The
  mode and emit are the first pair the validator accepts (`--no-resolve`) that
  the machine also resolves. The USDT sources are given the repository's own
  fixtures (`target/omn-usdt-fixture/fixture`, extracted from the source
  `tests/scenario_runtime.rs` embeds, and the vertical-case server), because a
  runtime's probes are a property of the target and not of the kernel.
- **The judges run on the host**, in the ordinary suite, over whatever cells
  are on disk. The listing is parsed again at judging time rather than trusted
  from the guest, so a parser fixed on the host needs no guest booted again --
  which is how the one parser defect this stream made was fixed (below).

---

## What each cell is checked for

Seven judges, each of which is first made to catch a defect planted in a cell
built by the test (`an_honest_cell_passes_every_judge` and the eleven after
it), because a judge that passes everything says nothing about the cells it
passed:

| Judge | The invariant |
|---|---|
| available means it attaches | every source listed usable, run through a backend that has a collector, starts; and at least one request of it resolves |
| every answer has its reason | a missing or unverified requirement names what was looked for; an unavailable source has a missing requirement on every backend; unknown never counts as available |
| out of reach is refused, with why | a source listed unavailable does not run, does not hang, and says why |
| a fallback says what it loses | a run through a backend whose catalog entry states a degradation prints `degraded` |
| a removed requirement is named | hidden BTF: `btf` missing naming `/sys/kernel/btf/vmlinux`, and every raw tracepoint and fentry not available and naming it; unmounted tracefs: every `tracepoint:` missing with `tracefs is not mounted`; no capabilities: `bpf` missing naming the capability |
| the floor is refused by name | every `kernel>=X.Y` requirement is available exactly when the kernel is at least X.Y, and otherwise says `older than` |
| uprobe-multi is 6.6 | a USDT source that attached below 6.6 says it fell back from uprobe-multi; at or above 6.6 it never does |

Two more tests hold the documents themselves honest:
`every_cell_covers_the_whole_catalog` refuses a cell swept against a different
catalog, and `the_listing_parser_reads_the_real_listing` checks the parser
against the binary's own listing, so a change of layout fails there instead of
turning every cell into `no`.

A backend that resolves and has no collector is not an attach failure: it is
vocabulary, and the listing's COLLECTOR column already says so. The matrix is
about the kernel.

---

## The cells

Measured on the development machine (AMD family 17h, 24 threads), guests with 4 vcpus and
2 GB. "Available" is what `omn list` calls usable; "attached" counts runs that
started and ended cleanly, out of 140. "Recorded" is the refusals the table of
known defects covers.

| Cell | Available | Attached | Refused after "usable" | Recorded |
|---|---|---|---|---|
| 5.15.221 caps | 86 | 77 | 33 | 33 |
| 5.15.221 root | 89 | 81 | 30 | 30 |
| 5.15.221 nocaps | 52 | 40 | 29 | 29 |
| 5.15.221 nobtf | 87 | 52 | 23 | 23 |
| 5.15.221 notracefs | 87 | 81 | 30 | 30 |
| 6.1.188 caps | 88 | 86 | 29 | 29 |
| 6.1.188 root | 89 | 90 | 26 | 26 |
| 6.1.188 nocaps | 52 | 40 | 29 | 29 |
| 6.1.188 nobtf | 87 | 52 | 23 | 23 |
| 6.1.188 notracefs | 89 | 90 | 26 | 26 |
| 6.6.157 caps | 88 | 90 | 25 | 25 |
| 6.6.157 root | 89 | 91 | 25 | 25 |
| 6.6.157 nocaps | 52 | 40 | 29 | 29 |
| 6.6.157 nobtf | 87 | 52 | 23 | 23 |
| 6.6.157 notracefs | 89 | 91 | 25 | 25 |
| 7.2.6 caps | 89 | 100 | 19 | 19 |
| 7.2.6 root | 90 | 101 | 19 | 19 |
| 7.2.6 nocaps | 52 | 41 | 28 | 28 |
| 7.2.6 nobtf | 89 | 53 | 22 | 22 |
| 7.2.6 notracefs | 90 | 101 | 19 | 19 |
| here (7.2.6, caps, no guest) | 93 | 118 | 7 | 7 |
| here (7.2.6, nocaps, no guest) | 57 | 81 | 5 | 5 |

Every other judge is clean in every cell. Read the "attached" column down the
`caps` rows and the story is the kernel's: 77, 86, 90, 100.

### What differs by kernel, and whether it is the catalog's reason

The listings of the `caps` and `root` cells differ from 7.2's in five sources,
and every difference is a requirement the catalog wrote, answered by name:

| Source | Below | Why the listing changed |
|---|---|---|
| `sched.runqlat`, `sched.offcpu`, `sched.switches`, `sched.lock` | 6.1 | `kernel>=6.1: missing: kernel 5.15 is older than 6.1` -- the floor, refused by name as `DECISIONS.md` says |
| `io.pagecache` | 7.2 | `raw_tracepoint:mm_filemap_get_pages: missing: this kernel defines no mm_filemap_get_pages tracepoint; it is absent from its own BTF` |
| `io.readahead` | 7.2 | the same tracepoint, and it takes its `bpf:fentry` backend instead |
| `mem.thp` | 6.8 | `kernel>=6.8` on `fs:sys`; `fs:proc` answers |

**uprobe-multi is 6.6, checked**: as root on 5.15 and 6.1, every USDT source
attached through the uprobe PMU and its run said *"this kernel refused a
uprobe-multi link (Invalid argument), which Linux 6.6 introduced"*; on 6.6 and
7.2 none did. Under `caps` below 6.6 the fallback is refused instead -- that is
P3.

**The degradations degrade while saying so.** In every `nobtf` cell, `btf`
reads *"/sys/kernel/btf/vmlinux is absent, so CO-RE is unavailable"* and every
raw tracepoint and fentry reads unverified, *"cannot read
/sys/kernel/btf/vmlinux"* -- which is the honest answer, since those are looked
up in BTF -- and no raw-tracepoint backend is chosen. In every `notracefs`
cell every `tracepoint:` requirement reads *"tracefs is not mounted"*. In every
`nocaps` cell `bpf` names `CAP_BPF`, `CAP_PERFMON` and the sysctl, and 52
sources remain: procfs, sysfs, per-task perf, netlink, the shim.

---

## What the cells found

Each class is a place where `omn list` says usable and the run refuses. Every
refusal names its reason, so none is a silent degradation; what is wrong is
that the probe or the resolver said yes first -- and for a launched target the
program has already been started by then. Each is filed under "the eleventh
wave, stream P" in `docs/open-findings.md` with the signature wanted, and
recorded in `KNOWN_DEFECTS` in `tests/portability.rs` with the cells it applies
to and the words the refusal must contain.

**P1. A tracepoint's prototype is checked at attach, not by the probe.** The
probe asks whether `btf_trace_<name>` exists; the programs then compare the
tracepoint's prototype with the one they were written for, and refuse:

```
proc.exit     (<= 6.6)   this kernel declares `sched_process_exit(task_struct *)`, and this program is written for `sched_process_exit(task_struct *, bool)`
mem.reclaim   (<= 6.6)   `mm_vmscan_direct_reclaim_begin(int, gfp_t)` against `(gfp_t, int, mem_cgroup *)`
io.latency    (5.15)     `block_rq_complete(request *, int, unsigned int)` against `(request *, blk_status_t, unsigned int)`
mem.kmalloc   (5.15)     `kmalloc(...)` has five arguments there, six here
mem.compaction(5.15)     `mm_compaction_begin` takes four integers there, a `compact_control *` here
net.request   (5.15)     `sched_switch(bool, task_struct *, task_struct *)` against the four-argument form
```

The refusal is right and careful -- *"which does not fail, it produces a
number"* -- and it is at attach. The check exists (`Btf::tracepoint_signature`);
it is the probe that does not call it. On 5.15 this also means the floor is
enforced for the sources that say `kernel>=6.1` and not for the ones whose raw
tracepoints happen to exist below it.

**P2. A BTF field is looked up at attach, not by the probe.** `net.drops`,
`net.tcp.lifetime` and `net.tcp.retransmit` read `sock.sk_ino`, which 6.6 does
not have: *"this kernel's `sock` has no member named `sk_ino`"*. Listed usable
on 5.15, 6.1 and 6.6.

**P3. Below 6.6, a USDT site needs CAP_SYS_ADMIN, and the probe says usable.**
Under `caps` on 5.15 and 6.1 the uprobe-multi link is refused, the fallback
opens the `uprobe` PMU, and the kernel wants `CAP_SYS_ADMIN` for that. The
`uprobe` requirement is answered by the PMU's existence. And the refusal gives
the wrong advice: *"needs CAP_BPF and CAP_PERFMON ... Grant them to the
binary"* -- to a binary that carries both -- before the parenthesis that
names `CAP_SYS_ADMIN`.

**P4. The USDT backends need BTF and do not require it.** In every `nobtf`
cell `runtime.gc`, `runtime.alloc` and `runtime.custom` are listed usable
(`bpf:usdt` requires `uprobe` and `bpf`) and refuse: *"cannot read
/sys/kernel/btf/vmlinux"*. `net.request`'s backend does require `btf`, and is
correctly out of reach there.

**P5. `net.request` has no route below 6.6.** `src/bpf/request.rs` links every
site with uprobe-multi and has no fallback, so on 6.1 -- as root too -- it
fails with a bare *"the kernel refused to link the program to 0x387e: Invalid
argument"*. Its catalog entry requires nothing that says 6.6.

**P6. NVML loadable, no driver loaded.** The guests see the host's
`libnvidia-ml.so` and no GPU, and every `gpu.*` source with a `vendor:nvml`
backend is listed usable and refuses: *"nvmlInit failed: Driver Not Loaded"*.
The probe answers "the library loads", which is what it says it answers; the
same machine state is a real one -- a container with the userspace library, or
a host after a driver upgrade and before a reboot.

**P7. The resolver selects a collector that refuses the mode.** The resolver
walks past an unimplemented backend to a fallback whose collector does not
serve the requested mode, and the collector refuses at start:

```
cpu.oncpu_time -m trace   -> fs:proc   "`cpu.oncpu_time` through `/proc` re-reads a total, which is `--mode poll`"
power.cstate   -m trace   -> fs:sys    "an idle transition is an event ... cpuidle holds residency totals, so poll them"
mem.faults     -m count   -> pmu:sw    "`count` mode does not sample page faults"      (stream S's item 4)
sched.switches -m count   -> pmu:sw    "`count` mode for `pmu:sw` needs the perf layer's counting support"
```

In every cell, the host included. `trace` is each of the first two sources'
first mode.

**P8. A system target the catalog declares and the collector refuses.**
`cpu.profile`, `mem.faults` and `sched.switches` declare `system: native`, and
their collectors need a pid: *"profiling a whole machine needs one sampling
event per cpu and the privileges for them; a system target names no process to
follow"* -- with `CAP_PERFMON` on the binary. In every cell, the host
included.

**P9. A per-task probe answers for a per-cpu event.** Without capabilities,
`perf_event_paranoid` at 2 lets a process count itself and not a cpu. The
`pmu:` probe opens a per-task event, so ten counting sources (`cpu.cycles`,
`.instructions`, `.ipc`, `.branch_misses`, `.cache_misses`,
`.context_switches`, `.migrations`, `mem.cache_l1`, `.cache_llc`, `.cache_tlb`)
are listed usable, and `--system` refuses with the right reason: *"counting
cpu 0 observes every task on it, which needs perf_event_paranoid at 0 or below
(it is 2 here) or CAP_PERFMON"*. Only in `nocaps` cells; the target decides
here, so the fix is in the resolver's per-request answer rather than the
source's listing. On the host without capabilities it does not happen: its
`perf_event_paranoid` is -1, so the entry applies only to a cell whose own
resolved file recorded a value above 0.

**P10. The guest's own shape.** Three refusals that belong to the virtual
machine rather than to the tool, recorded for the guest cells only:

- `mem.cgroup` on a launched program: everything in a virtme guest lives in
  the root cgroup, which publishes no `memory.current`. The refusal is exact.
- `mem.swap` on a launched program, on 6.6 and below: the root cgroup's
  `memory.stat` has no `pswpin`/`pswpout` there, and has on 7.2. A
  kernel-dependent file the catalog does not name as a requirement.
- `power.freq` on `fs:sys`: the catalog requires
  `/sys/devices/system/cpu/cpufreq`, which exists in the guest; the collector
  reads `cpu0/cpufreq`, which does not -- *"the ordinary case in a virtual
  machine"*. The requirement names the wrong path.

### A defect of this stream's own, found by the matrix

The first full run parsed each listing inside the guest, and the parser took a
`needs` line whose reason had a full stop in it -- *"(... so bpf() needs
CAP_BPF). Grant them"* -- for a backend header, dropping every capability
refusal. The `nocaps` cells showed it at once: 41 sources "out of reach with
nothing missing". The listing is now stored verbatim and parsed at judging
time, and `the_listing_parser_reads_every_answer_kind` carries that line.

---

## The CI

`.github/workflows/ci.yml`, one job on `ubuntu-24.04`: `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, a build, the matrix cell of the
runner, the whole suite, and a gate on the skips. It has **not been pushed**:
the coordinator decides when it reaches the remote, and so it has never run.

- **It is the unprivileged cell**: a distribution kernel, no file
  capabilities, no GPU. `make` is not used to build, because its capability
  step runs `sudo -n setcap` and a hosted runner's sudo needs no password -- the
  build would grant what this cell exists to go without. The workflow builds
  with `cargo build --workspace` and the two C fixtures with their own `make`
  targets, and fails if the binary carries a capability anyway.
- **It sweeps itself before the suite**, as `ci-<kernel>-nocaps`, so the
  suite's own `tests/portability.rs` judges a real cell on every push, and the
  cell and the test log are uploaded as an artifact.
- **A skip is not a pass.** `.github/scripts/skip-gate.sh` reads every
  `skipped:`/`SKIPPED` line of the log and matches its *reason* against
  `.github/expected-skips.txt`, one regular expression per class of reason: the
  capabilities by name, unprivileged perf and bpf, tracefs, and hardware a
  cloud VM lacks (GPU, IBS/PEBS, RAPL, sensors, cpufreq, hardware counters). A
  skip whose reason matches none fails the job, naming it; each class is
  counted in the log either way. Reasons rather than test names, because a
  skip line says what the machine lacked, and that is the property of the
  cell. Workloads the tests launch -- a C compiler and `<sys/sdt.h>`, python3,
  node, iproute2 -- are installed, so a skip for one of them is the workflow's
  fault and the gate refuses it.

**What is not known until it runs**: the runner's kernel (Azure's 6.8 or
later), its `perf_event_paranoid`, and whether its hypervisor exposes hardware
counters. The first push will show whether the gate's list is right and
whether the runner's cell meets a defect the table does not record. Both are
expected, and both are one-line changes, in `.github/expected-skips.txt` and
in `KNOWN_DEFECTS` respectively.

### What the first push will meet, measured here

The CI cell was rehearsed on this machine: the binary stripped of its
capabilities (both of its hard links -- `target/debug/omn` is a link to cargo's
`deps/` copy, and a copy of one alone is re-linked by the next test build),
swept as `here-nocaps`, and the whole suite run with `--nocapture`:
**1772 passed, 6 failed, 10 ignored.**

- **Five tests outside this stream fail rather than skip without
  capabilities**, so the CI will be red on them until their owners change
  them: `a_program_that_declares_no_request_is_refused_by_name`
  (`tests/vertical.rs`), `every_mode_and_emit_of_the_kernel_sources_is_served_or_refused_by_the_right_code`
  (`tests/scenario_mem.rs`), `the_combinations_these_sources_do_not_have_are_refused_with_the_reason`
  and `the_syscall_filter_the_resolved_file_claims_is_the_one_that_ran`
  (`tests/scenario_proc.rs`), and
  `what_this_machine_cannot_check_about_gpu_job_is_named_with_its_numbers`
  (`tests/scenario_gpu.rs`). Each asserts what a privileged machine answers.
  Filed in `docs/open-findings.md`.
- The sixth was this stream's own P9 entry, too broad for a host whose
  `perf_event_paranoid` is -1; it now reads the cell's value.
- **The gate**, on that log: 152 skip lines, every one matched but one --
  `tests/vertical.rs` prints the *first line* of a refusal as its skip reason,
  and here that line is `note: mode defaulted to trace`, so the skip says
  nothing about why. That is the gate doing its job; filed with the five. The
  gate was checked the other way too: a planted *"no C compiler to build the
  fixture"* skip fails it.

The gate reads each skip with the indented lines under it, because a skip that
quotes a refusal puts the probes' reasons there. Under a parallel suite those
lines interleave with other tests' output, so the resolver's own
classification -- *"has no viable backend on this machine"*, which it prints
only when every backend was refused by a probe -- is an expected class in its
own right, and the cell's sweep is what checks that each probe's reason is
real. A cloud runner will lack more hardware than this machine, never less
privilege, so the list may need a hardware class the first push names.

---

## Limits

- **One request per target kind.** A source is run on a launched program and
  on the machine, with one mode. "Available means it attaches" is checked for
  those, not for every mode and emit.
- **The judges do not read numbers.** A cell says whether a source attached
  and what it said, not whether its measurement was right on that kernel:
  ground truth on other kernels is the family scenarios' job, and those run on
  the host.
- **The guests are not the host's hardware.** KVM exposes the PMU under
  `-cpu host` and no GPU, no RAPL, no cpufreq, no IBS. The hardware-dependent
  sources are checked on the host cell only.
- **`perf_event_paranoid` is 2 in the guests** (Ubuntu's mainline kernels) and
  -1 on the development host; the `nocaps` cells are the ones where that
  matters, and they say which value they saw.
- **Timings are not judged**, so the matrix may run on a loaded machine. A
  cell took 60 to 115 s of sweep plus a few seconds of boot.

---

## Handoff

**Files**: `scripts/matrix.sh`, `scripts/vm/cell.sh`,
`scripts/vm/fetch-kernel.sh`, `tests/portability.rs`,
`.github/workflows/ci.yml`, `.github/scripts/skip-gate.sh`,
`.github/expected-skips.txt`, this document. Nothing outside the stream's
paths changed.

**The `make matrix` target**: in "Running it" above.

**Needs from other streams**: `docs/open-findings.md`, "the eleventh wave,
stream P" -- one request per class, P1 to P10, with the signature.

**Index rows for sessions added**: none; this stream adds no session.

**Ignored tests this stream makes ready**: none. Its own ignored test,
`sweep_this_machine`, stays ignored by design: it measures the machine it runs
on, and `scripts/matrix.sh` and the CI run it.

**Known flakes met**: see "The commit's check" below.

**Seen once and not reproduced**: one of five sweeps of the host
(`scripts/matrix.sh here`) failed after 115 s with its output filtered away;
the four after it passed. Nothing in the documents it would have written was
kept, so there is nothing to say about it but that it happened.

### The commit's check

One `make check` on this branch, with the capabilities on the binary and
twenty-two cells on disk for `tests/portability.rs` to judge: red on one test
outside this stream, so one `cargo test --workspace --no-fail-fast` followed,
as the rule asks -- **1776 passed, 2 failed, 10 ignored**, clippy and
`cargo fmt --check` clean. Both failures passed when rerun alone, once each,
and neither is in the flaky tally yet, so both are added to it under stream P
in `docs/open-findings.md`:

- `every_mode_and_emit_of_the_sampled_sources_is_served_or_refused_by_the_right_code`
  (`tests/scenario_mem.rs`): `mem.latency sample stack` appeared among the
  exit-1 refusals, in both full runs, and not alone.
- `the_page_cache_sessions_say_what_their_headers_predict`
  (`tests/scenario_io.rs`): once, in the `--no-fail-fast` run.

`tests/portability.rs` itself: 16 passed, 1 ignored (`sweep_this_machine`), in
every run.
