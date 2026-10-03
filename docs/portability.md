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
| `sched.runqlat`, `sched.offcpu`, `sched.lock` | 6.1 | `kernel>=6.1: missing: kernel 5.15 is older than 6.1` -- the floor, refused by name as `DECISIONS.md` says |
| `sched.switches` | 6.1 | its `bpf:raw_tp` backend only: `kernel>=6.1` is that backend's requirement, not the source's. `pmu:sw` and the `bpf:tp` count program below it name no kernel, and the sixteenth wave's sweep found the source usable on 5.15 through `pmu:sw` ("The sixteenth wave's sweep", below). Listed here as refused at first, which read a backend's refusal as the source's |
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

---

## The twelfth wave

Stream V -- *the probes tell the truth*. The eleventh wave's matrix found that
*available means it attaches* failed on the kernels nobody had tried; this
wave's P1 to P6 and P10 are the classes where the probe, or the catalog line it
answers, said yes first. Each is now answered **missing, with the reason the
attach used to give**, before anything is launched.

### What each class became

| Class | The fix | Where |
|---|---|---|
| **P1** a tracepoint's prototype | A `raw_tracepoint:` requirement may carry the prototype its program reads, `name(arg, ...)`, or several, `name(a, b)|(a, b, c)`. The probe compares it with `Btf::tracepoint_signature` and refuses in `check_signature`'s words: *"this kernel declares `sched_process_exit(task_struct *)`, and this program is written for ..."*. A bare name still asks for existence alone. Forty-seven requirements carry their prototype. | `src/bpf/mod.rs` (`raw_tracepoint_support`, `parse_raw_tracepoint`, `prototype_mismatch`), `catalog/{io,mem,net,proc,sched}.yaml` |
| **P2** a BTF field read at attach | `btf_member:struct.member`, answered by `Btf::member_offset` -- the lookup the program makes -- so the probe's refusal is the attach's: *"this kernel's `sock` has no member named `sk_ino`"*. On `net.drops`, `net.tcp.lifetime`, `net.tcp.retransmit`. | `src/bpf/mod.rs` (`btf_member_support`), `src/capabilities.rs`, `catalog/net.yaml` |
| **P3** USDT below 6.6 under the two capabilities | `uprobe` now means *this process can place a uprobe*: a uprobe-multi link, or else the `uprobe` PMU, which needs `CAP_SYS_ADMIN` and is asked with a path that does not exist (the privilege is checked before the path). A no names both routes. The fallback in `usdt.rs` asks the PMU before attaching, so a forced run leads with what is missing instead of *"grant CAP_BPF and CAP_PERFMON"* to a binary that has both. | `src/bpf/attach.rs` (`uprobe_support`, `uprobe_pmu_permitted`), `src/bpf/usdt.rs` |
| **P4** USDT needs BTF | `btf` on every `bpf:usdt` and `bpf:uprobe` line. | `catalog/{runtime,sched}.yaml` |
| **P5** `net.request` below 6.6 | A new requirement `uprobe_multi`, asked of the kernel: the smallest program loaded for `BPF_TRACE_UPROBE_MULTI` and linked to a file that does not exist -- `ENOENT` from a kernel that knows the link, `EINVAL` from one older than 6.6. Both `net.request` backends require it, and a caveat says 6.6. A forced backend's `EINVAL` now names the link and 6.6 instead of the bare errno. | `src/bpf/usdt.rs` (`uprobe_multi_support`), `src/bpf/request.rs`, `catalog/net.yaml` |
| **P6** NVML without its driver | `nvml` initialises the library, counts devices and shuts it down, once per process (3 to 5 ms, paid only by a process that asks about a GPU source): *"libnvidia-ml loads and does not initialise, so no NVIDIA driver is serving it: ... Driver Not Loaded"*. | `src/collect/gpu/mod.rs` |
| **P10** the guest's shape | `mem.swap`'s `fs:cgroup` requires `cgroup2:memory.stat:pswpout` -- a new form, a key of a flat-keyed cgroup file, read where this process lives and walking up to the root. `power.freq` and `power.cstate` require `cpu0/cpufreq` and `cpu0/cpuidle`, the directories their collectors read. `mem.cgroup` on a launched program in a guest stays recorded: everything there lives in the root cgroup, which has no `memory.current`, and the refusal is exact -- that is the guest, not the probe. | `src/capabilities.rs` (`probe_cgroup_key`), `catalog/{mem,power}.yaml`, `tests/portability.rs` |

Also taken, from stream W and stream D's requests: `modes: [poll]` on
`cpu.oncpu_time`'s `fs:cgroup` and `fs:proc` and on `power.cstate`'s `fs:sys`
(P7's data for W's resolver rule); `sched.wakeup`'s caveat rewritten in D's
words; `sched.switches`'s `pmu:sw` declares the targets its collector serves,
so a cgroup or a container is passed over before the start; `Btf::var_id`;
`usdt.rs`'s `mod local` and `request_net.rs`'s `or64_imm` replaced by
`insn.rs`'s shared instructions; and `KNOWN_DEFECTS` loses P1 to P6, P7's
`cpu.oncpu_time`, `power.cstate` and `mem.faults`, all of P8 and P9, and
P10's `mem.swap` and `power.freq`. The capability-less test of stream P's
"five tests" list, in `tests/scenario_gpu.rs`, now holds the `gpu.job`
refusal to the binary's own `bpf` answer: naming `CAP_BPF` exactly when the
listing says `bpf` is missing.

### How each is verified

The cells under `target/matrix/cells` were swept at the end of this stream's
work, with a binary that carried every fix above except `power.cstate`'s path.
A second sweep was started and stopped by the user; it overwrote `here-caps`
with the final binary and destroyed `5.15.221-nocaps` (its `sweep.json` is
gone; its `guest.log` is qemu being killed). The matrix was **not** rerun
after that, by instruction. So twenty-one cells are on disk, and:

| Class | Verification |
|---|---|
| P1 | **On cells.** `proc.exit`, `mem.reclaim` missing on 5.15, 6.1 and 6.6 by prototype; `mem.kmalloc`, `mem.compaction`, `io.latency`'s `bpf:raw_tp` missing on 5.15; all attach on 7.2 and here. Unit tests on the parser, the comparison and every prototype in the catalog. |
| P2 | **On cells.** `btf_member:sock.sk_ino` missing on 5.15, 6.1, 6.6 (and unverified in every `nobtf` cell); `net.drops` and `net.tcp.lifetime` attach on 7.2. |
| P3 | **On cells.** 5.15 and 6.1 `caps`: `uprobe` missing, naming both routes; `root` and `notracefs` (both uid 0) there still attach through the PMU and say they fell back; 6.6 and 7.2 attach. |
| P4 | **On cells.** `btf` missing on the USDT backends in every `nobtf` cell. |
| P5 | **On cells.** `uprobe_multi` missing on 5.15 and 6.1, as root too; `net.request` attaches on 6.6 and 7.2. The forced-backend wording is code reading only. |
| P6 | **On cells.** `nvml` missing in every guest; available and attaching in `here-caps`. Unit test: the probe says yes exactly when an independent `Nvml::open` counts a device. |
| P10 `mem.swap` | **On cells.** `cgroup2:memory.stat:pswpout` missing on 5.15, 6.1, 6.6, available on 7.2. Unit test against this process's own `memory.stat`. |
| P10 `power.freq` | **On cells.** `cpu0/cpufreq` missing in every guest; attaches here. |
| P10 `power.cstate` | **Code reading only; needs a matrix rerun.** The cells were swept with the old requirement, `/sys/devices/system/cpu/cpuidle`, which exists in a guest; the collector reads `cpu<first>/cpuidle`, which does not. The three surviving `nocaps` cells (6.1, 6.6, 7.2) show exactly that refusal with no entry for it, so `every_cell_on_disk_holds_the_invariants` is red on them until they are swept again. The requirement now names the directory the refusal names; on this host it reads available and the source attaches. |
| P7/P8/P9 entries retired | **On cells** (W and D's fixes, merged before this branch's sweep), except in `here-nocaps`, which is from 20:27, before either: its six violations are the stale cell, not a regression. |
| `sched.switches` targets | **Unit-level only**: `omn trace sched.switches --cgroup /sys/fs/cgroup` now says *"`pmu:sw` cannot attribute per cgroup"* before the start. The matrix does not run a cgroup target. |

**P11, new** (recorded, not fixed): on 5.15 under `caps`, `cpu.profile
--system` is refused at the ring: *"perf_event_open on cpu 3 was refused (mmap
of 8 ring pages: Operation not permitted)"*. Not investigated; the likely
reading, unconfirmed, is that 5.15 charges the per-cpu rings against
`perf_event_mlock_kb` and lets only `CAP_IPC_LOCK` exceed it. It is P8's
machine target, served since this wave, meeting an older kernel, and the probe
cannot see it without opening the events. For whoever owns `src/perf.rs`:
confirm the cause, then either a probe of the per-cpu ring or a refusal that
names it.

**What clears the red test**: `scripts/matrix.sh here` without capabilities
(about 90 s, no guest) for `here-nocaps`, and the `nocaps` cells of 5.15,
6.1, 6.6 and 7.2 for `power.cstate` and the lost cell -- on an idle machine.
`KNOWN_DEFECTS` then holds three entries: P7's `sched.switches` (waiting on
stream D's collector or on `modes: [trace]` with its test's wording), P10's
`mem.cgroup` (the guest's), and P11.

### For `DECISIONS.md`

- **A prototype is written in the catalog as well as in the program.** The
  probe must answer before anything is loaded, so it cannot ask the program;
  the price is that a program changing the shape it accepts changes its
  requirement in the same commit. A mismatch shows on the development machine
  at once (the source reads missing), and
  `every_prototype_in_the_catalog_is_well_formed` catches a typo.
- **`uprobe` means this process can place one**, not that the PMU exists; and
  `uprobe_multi` is asked of the kernel with a link to a nonexistent file,
  not read off the version. Without the capabilities the load cannot be asked,
  so `uprobe_multi` is unknown there (never available), and `bpf` is what
  reads missing.
- **A GPU probe initialises its library.** Loading `libnvidia-ml.so` is not
  the question a collector's first call asks; 3 to 5 ms once per process is
  the price of the answer being the collector's.
- **A kernel-dependent key is a requirement**, `cgroup2:<file>:<key>`, read
  from the nearest cgroup that has the file: which keys a kernel writes does
  not depend on which cgroup it writes them for.
- **The guest's root cgroup is not a defect** (`mem.cgroup`): the probe cannot
  know the target's cgroup before it is launched, and the refusal is exact.

### Filed or left

- Filed in `docs/open-findings.md` under this stream: the prototype contract
  for the raw-tracepoint programs' owner, with the two widenings the matrix
  measured (`sk_ino` through `sk_socket->file->f_inode`; the one-slot-moved
  prototypes of 6.6 and 5.15); the four new notations for `CATALOG.md` §1;
  P7's `sched.switches -m count` for stream D; and the `cgroup`/`container`
  targets of the counting sources, `cpu.profile` and `mem.faults`, for
  stream W next wave (not taken: the collectors over `perf::Scope::CgroupOnCpu`
  are promised, and removing the keys changes what `tests/scenario_cpu.rs`
  and `tests/family_cpu.rs` exercise).
- Left for stream D: `src/bpf/wakeup.rs` keeps its own `var_id` walk;
  `Btf::var_id` is here now, and `PreemptCount::locate` can call it and drop
  the copy.
- Left for the coordinator: P11 above, for `src/perf.rs`'s owner.

**Index rows for sessions added**: none.

### This wave's check

One `make check` after merging `main`, with the capabilities on the binary:
red in `tests/portability.rs` only, so one `cargo test --workspace
--no-fail-fast` followed -- **1911 passed, 1 failed, 8 ignored**. The one
failure is `every_cell_on_disk_holds_the_invariants`, on the stale cells
described above (three `power.cstate` on the `nocaps` guests, six in
`here-nocaps`), not in the flaky tally and not a flake: it is red until those
cells are swept again. `cargo clippy --all-targets -- -D warnings` and
`cargo fmt --check` clean. No other test failed, flaky or not.

---

## The thirteenth wave

Stream P -- *the targets perf can serve*. Four things: the one matrix class
nobody had read (P11), the `cgroup` and `container` targets the counting
sources, `cpu.profile` and `mem.faults` declared and no collector served (P8's
shape one target over), the CI's first run, and the loader's and the
resolver's filed requests.

### P11: the per-cpu rings on 5.15, confirmed and fixed

**The cause**, measured in a 5.15.221 guest under `caps` (uid 1000,
`perf_event_paranoid` 2, `perf_event_mlock_kb` 516, four vcpus):

| `ulimit -l` (KiB) | `cpu.profile --system` |
|---|---|
| 64 (the guest's) | refused: *"perf_event_open on cpu 3 was refused (mmap of 8 ring pages: Operation not permitted)"* |
| 128, 256 | refused, the same |
| 8192 | attaches |
| 64, with `perf_event_mlock_kb` 2064 | attaches |

`perf_mmap` lets one user lock `perf_event_mlock_kb` per online cpu for its
rings (2064 KiB here) and charges what goes past that to `RLIMIT_MEMLOCK`,
unless `perf_event_paranoid` is -1 or the process holds `CAP_IPC_LOCK`.
`CAP_PERFMON` does not lift it. The kernel's default `RLIMIT_MEMLOCK` was 64
KiB until 5.16 and is 8 MiB since, and a busybox guest has no PAM to raise it:
so the same rings map on 6.1 and not on 5.15. The reading the twelfth wave
guessed (*"only `CAP_IPC_LOCK` exceeds it"*) was half of it; the other half is
the default limit, and it is why 6.1 did not show it.

**And the tool made it worse.** `SampledEvent::open` halves a ring until it
maps, cpu by cpu: cpus 0 and 1 took 256-page rings (1028 KiB each), cpu 2 went
past the per-user share into the 64 KiB limit at 16 pages, and cpu 3 had
nothing left even at the 8-page floor. Four rings of 128 pages fit the share
exactly.

**The fix** (`src/perf.rs`, `ThreadSampler::open_per_cpu`): every cpu gets the
same ring. A size one cpu is refused is given up for all of them and the set is
opened again at half of it, so the first cpus cannot take the budget from the
last. When even the floor does not fit, the refusal names the budget rather
than the errno (`ring_budget_refusal`):

```
cannot start collecting `cpu.profile`: the sampling rings of 4 cpus do not fit what this user may
lock: cpu 3 was refused a ring of 8 pages (mmap of 8 ring pages: Operation not permitted), after the
others took theirs, 144 KiB in all at that size. The kernel lets a user lock
/proc/sys/kernel/perf_event_mlock_kb per online cpu (16 KiB here) and charges the rest to
RLIMIT_MEMLOCK (64 KiB here; `ulimit -l`), unless perf_event_paranoid is -1 or the process holds
CAP_IPC_LOCK -- CAP_PERFMON does not lift it. Raise either limit, or close this user's other perf
sessions
```

That is the 5.15 guest with `perf_event_mlock_kb` lowered to 16 on purpose.
With the guest's own 516, `cpu.profile --system` and `mem.faults --system`
attach there now, which is the ground truth the brief named; `KNOWN_DEFECTS`
loses P11. **No probe was added**: the budget is per user and counts every
ring the user's other sessions hold, which only the mapping itself can
answer, and for a machine or a cgroup target nothing is launched before the
rings are mapped, so the refusal already comes before the start.

### The `cgroup` targets, served; the `container` targets, refused by name

`perf_event_open` has a third shape beside a task and a cpu: an event on a cpu
that counts only while a task of one cgroup v2 directory (or one below it)
runs there (`PERF_FLAG_PID_CGROUP`, `perf::Scope::CgroupOnCpu`). So a cgroup
is one event per online cpu, and the privilege it asks is a cpu's
(`perf_allow_cpu`: a cgroup's tasks may be anybody's) -- the resolver adds
`per-cpu perf events` to a `pmu:` backend for a cgroup target as it does for
`--system`.

| Source | `--cgroup` | How |
|---|---|---|
| `cpu.cycles`, `.instructions`, `.ipc`, `.branch_misses`, `.cache_misses`, `.context_switches`, `.migrations`, and `mem.cache_l1`/`_llc`/`_tlb`, `mem.faults -m count` | served | `src/collect/cpu/cgroup.rs`: a counter group per cpu (`PerCpuGroups::open_cgroup`), read at the end and **summed into one event keyed on the cgroup**, with the times summed the way a process's threads are |
| `cpu.topdown` | served, not verifiable here | the same groups; this AMD part has no issue-slot counter, so the source is unavailable for every target and its cgroup path is exactly as verified as its system path |
| `cpu.profile` | served | `ThreadSampler::open_cgroup`: a cpu-clock ring per cpu, every sample keyed on its task, its cpu and the cgroup |
| `mem.faults` (sample, trace) | served | the same, over the page-fault software event |
| all of them, and `cpu.throttle` | `container: no` | nothing in the tool resolves a container name to its cgroup |

**Decided per source, and why the same way for all**: a cgroup is reported
like a process, one number summed over the cpus, not like a machine. A
machine's cpus are what `--system` asks about and are kept apart; a cgroup's
cpus are where its tasks happened to run, and a count split across them
describes the scheduler rather than the cgroup. A sampled source keeps each
sample's `cpu` key, so nothing is lost there.

**The container**, a finding of its own: removing `container: native` from an
entry does not refuse a container. `TargetKind::fallback` makes a `container`
fall back to `cgroup`, so an entry that serves the cgroup and says nothing
accepts the container, and the collector refused it at the start -- the shape
this item is about. The entries say `container: no`, and the validator words
the refusal (`target_rejection_reason`): *"`mem.faults` cannot be measured per
container: a container has to be resolved to its cgroup first, which this tool
does not do; pass --cgroup with the container's cgroup path"*, exit 2, before
anything is resolved.

**Ground truth** (`tests/scenario_cpu.rs`, "The cgroup target"). The test
makes a cgroup under its own (skipping, with the reason, where the hierarchy
is not delegated) and runs a workload that forks: the child moves itself into
the cgroup and does one unit of work, the parent stays outside and does ten,
at the same time. The child reports its pid, its `CLOCK_MONOTONIC` window and
its own count of what it did, using system calls only between the fork and
its exit.

| Scenario | The child, inside | The parent, beside | Asserted |
|---|---|---|---|
| `a_cgroup_is_counted_for_its_own_loop_and_not_for_the_one_beside_it` | 50 M turns of `dec`/`jnz`, 10^8 instructions | 10^9 | count in [0.995, 1.25) x 10^8, one event, keyed on the directory's inode |
| `a_profile_of_a_cgroup_samples_its_task_and_not_the_one_beside_it` | 200 ms of its own cpu | 2 s | every sample's task is the child, none the parent; samples within 10 % + 2 of 997 Hz x the child's thread clock; every sample carries the cgroup key |
| `a_cgroup_s_faults_are_its_own_and_not_its_neighbour_s` | 2000 fresh pages, its `getrusage` delta | ten times that | count and trace: at least the child's own fault count, short of child + half the parent's; every traced fault is the child's |

`omn` announces nothing when its events are open, and a binary carrying file
capabilities is not dumpable, so its descriptor table cannot be read to wait
on either. The order is therefore **checked, not arranged**: the workload
starts a lead after `omn`, the window is far longer than the work, and the
result's own `window.opened_ns`/`closed_ns` must hold the child's stamps -- a
run that does not is taken again with twice the lead, three times at most.
The durations make an ordered run likely; the stamps make it one.
(`SIGINT` is not a way to end a run early: `omn` dies of it.)

And `a_container_is_refused_before_the_start_where_only_its_cgroup_is_served`
holds the container refusal for four of the sources.

### The CI's first run

Run 36606817052 on `a12e2a5`: Linux 6.17.0-1022-azure, **`perf_event_paranoid`
4**, `unprivileged_bpf_disabled` 2, BTF readable. fmt, clippy, the build and
the matrix cell were green; the suite had 58 failures and the skip gate
refused 37 skips. The cell `ci-6.17.0-nocaps` passed every judge of
`tests/portability.rs`.

**What 4 is.** A Debian patch Ubuntu carries and upstream never took: at 4 an
unprivileged process may not open even its own counters. About 45 of the 58
failures are tests that treat the resolver's correct *"has no viable backend
on this machine"* as a failure. The tool was right in all of them.

**Decided: the cell is configured to the kernel's own defaults**
(`.github/workflows/ci.yml`, "Configure the cell"), and says why above the
step:

- `perf_event_paranoid` 2. The cell exists to exercise the unprivileged perf
  path, which at 4 is never exercised; the paranoid-4 class is a real one and
  stays filed with each owner, for a paranoid-4 cell once they are fixed.
- `kernel.apparmor_restrict_unprivileged_userns` 0 (Ubuntu from 23.10 refuses
  `unshare -Urm`; upstream allows it): six skips and two failures were that.
- A delegated cgroup, `/sys/fs/cgroup/omn-ci` owned by the runner's user with
  cpu, memory, io and pids handed down and the job's shell in its leaf `job`:
  the shape a systemd user session gives a workstation, where the cgroup
  scenarios make their cgroups. Each later step moves its own shell there
  first (with sudo, because a move is checked against the common ancestor,
  the root).
- `linux-tools-$(uname -r)`, for `btf.rs`'s `bpftool` cross-check.

**Answered in stream P's files** -- each now holds at paranoid 4 and on a
machine without the hardware, by skipping with the machine's reason where the
subject is something else, or by a floor that does not assume this machine:
`collect::cpu::counters` (the cache test, and a worker thread that panicked
when its test skipped), `tests/catalog_model.rs` (the sweep's floor is on the
pairs asked, not on what the machine serves), `tests/family_cpu.rs` (two),
`tests/scenario_cpu.rs` (four; the stdout warning uses `cpu.oncpu_time`,
which every Linux serves), `tests/scenario_gpu.rs` (two), `tests/family_power.rs`
(two), `tests/scenario_power.rs` (four) -- and one tool defect:

**A qualified `pmu:` event that is not published answered unknown**
(`open-requests 1598`). `pmu:power/energy-pkg/` on the runner, which has no
RAPL: *"needs an event encoding this tool does not build yet"*, so
`power.energy` was listed as assumed, resolved, and refused at the start.
`sysfs_device_state` now reads the unit's own directory: *"this machine
publishes no `power` performance unit"*, or *"the `cpu` performance unit
publishes no `x` event"*.

**The skip gate**: `.github/expected-skips.txt` gains the classes the cloud VM
lacks, each with its reason -- the GPU's userspace (CUDA, Vulkan), lowercase
`intel-rapl`, a hwmon chip, SMT, unprivileged bpf's *"create the row maps"*,
and the aggregation test's state skip -- and one line **filed, not expected**:
the block-device lookup of `/dev/root` in `tests/scenario_io.rs`. Checked
against the first run's log with the new list: what is left unexpected is
exactly the cgroup, user-namespace, bpftool and `cross-memory` skips the
configuration step removes. None of this has run on a runner yet: the next
push is its test.

**Filed**, one request per owner, under "the thirteenth wave, stream P" in
`docs/open-findings.md`: two 6.17 prototypes (S's `fentry.rs`, N's
`mem_stall.rs`), the runner's two inherited descriptors (S's
`scenario_proc`), `net.hwts` listed usable without a PTP clock (N), and the
paranoid-4 tests of R, J, S, N and the coordinator.

### The loader and the resolver, each checked first

| Request | Found | Now |
|---|---|---|
| `check_catalog` in the loader (2493) | open | `Catalog::from_entries` runs it: a `--catalog <dir>` promising what no build keeps does not load |
| `Backend.keys` and `check_backend_keys` (2493), `KeyNotCarried` (2539) | open | **not taken**: the refusal needs a `ResolveError` variant, and `src/main.rs` (R's) matches the enum without a wildcard, so it cannot land here alone. Filed for R with the arm; the field without the refusal would be vocabulary nothing reads |
| `check_stack` refuses `dwarf`/`lbr` (815) | refused at the factory | **kept there, and tested**: the factory runs before the launch and answers the build's gap with exit 3; the validator answers what a request means, and a session asking for dwarf is valid for a build with an unwinder. `an_unwinder_this_build_lacks_is_refused_before_the_program_starts`: the program's first act is to create a file, and it is not there |
| `stack` over a non-additive unit (2127) | `rate` only | `check_catalog` refuses `stack` the same way |
| a `pmu:` glob refused by the loader (1598) | open | any `*` or `?` in a requirement, outside a raw tracepoint's prototype, is refused naming it |
| a qualified `pmu:unit/event` absent answers missing (1598) | unknown | missing, naming the unit or the event |
| one reader of `cpu/online` (1649) | four copies | `capabilities::online_cpus`/`parse_cpu_list`; `perf::` re-exports them, `power::sysfs` calls them; `bpf/attach.rs` (S) and `bpf/syscall.rs` (R) filed |
| `writable:` (1422) | open | `writable:<path>` (with `[pid]`) asks `access(W_OK)`; filed for N's `mem.wss` |
| `SampledEvent::throttled()` (885) | partly | an accumulator on `SampledEvent`, `ThreadSampler` and `TaskSampler`, `PERF_RECORD_UNTHROTTLE` not counted; `cpu.profile` and `mem.faults` report from it |
| `Unit::Watts`, `gpu.power` in watts (957) | open | **not taken**: `src/encode.rs` (R's) matches `Unit` exhaustively; filed with the arm |
| a per-tick read of a counting PMU event (5099) | open | `--mode poll` on the seven counting entries of `cpu` (`timeline` added to their emits), per task, per cpu and per cgroup: each tick the difference from the one before, with `interval_ns`; a tick the task spent asleep is a count of zero, not a loss. `the_ticks_of_a_polled_counter_add_up_to_the_loop_it_counted` |

Each loader refusal is held by
`tests/catalog_model.rs::a_catalog_carrying_a_promise_no_build_can_keep_is_refused_when_it_is_loaded`:
a copy of the catalog with an emit no mode produces, `rate` and `stack` over a
ratio, and a glob, each refused with its reason; a prototype with a `*` is
not a glob.

### The tally's rows

- **#8** (`per_cpu_utilisation_reports_one_series_for_each_core_asked_for`):
  already landed -- `util.rs` computes `busy` in ticks before the division.
- **#9 and `open-requests 5272`** (the known-instruction scenario, +0.69 %):
  the workload now brackets its loop with two counters of its own, user mode
  and every mode, and the difference -- what the kernel ran in its context
  while the loop did -- is the scenario's slack above; nothing is allowed
  below. The twice-the-startup excess was that: a longer run under load takes
  more interrupts and preemptions in its context.
- **#24** (`a_resident_kernel_separates_from_an_idle_device...`): who else is
  on the device is asked again after the idle baseline and after the busy
  run, not only before, and a tenant skips the test with its name.

### What the matrix says now

Swept at the end of this stream's work, with the committed binary: the four
kernels under `caps`, `root` and `nocaps`, twelve cells, on a machine the
other streams were loading (the judges read no timings). **Every judge passes
in every cell**, and `KNOWN_DEFECTS` holds two entries: P7's `sched.switches`
(stream S's) and P10's `mem.cgroup` (the guest's root cgroup).

| Kernel | `caps` attached | `root` attached | `nocaps` attached | eleventh wave's `caps` |
|---|---|---|---|---|
| 5.15.221 | 80 | 84 | 42 | 77 |
| 6.1.188 | 89 | 93 | 42 | 86 |
| 6.6.157 | 93 | 94 | 42 | 90 |
| 7.2.6 | 103 | 104 | 43 | 100 |

(Runs that started and ended cleanly, of 140 a cell.) On 5.15 under `caps`,
`cpu.profile` attaches on both targets, the launched program and `--system`:
P11 is gone from the cell that showed it. In every guest `power.energy`'s
`pmu:power/energy-pkg/` now reads *"missing: this machine publishes no `power`
performance unit"* instead of unknown. **Cells still to verify**, for the coordinator's sweep at the end of the wave
(this stream started no matrix run after the twelve above):

| Cells | Command | What should change |
|---|---|---|
| 5.15, 6.1, 6.6, 7.2 under `nobtf` and `notracefs` | `scripts/matrix.sh v5.15.221 v6.1.188 v6.6.157 host --configs nobtf,notracefs` | `power.energy`'s `pmu` backend reads missing (no `power` unit) rather than unknown; nothing of BTF or tracefs changes; every judge passes |
| `here-caps` | `scripts/matrix.sh here` (with the capabilities) | `power.energy` still attaches through `pmu` (this host has the `power` unit); every judge passes |
| `here-nocaps` | strip both links of the binary (`setcap -r target/debug/omn target/debug/deps/omn-*`), then `scripts/matrix.sh here` | the counting sources on `--system` are refused before the start for per-cpu events as before; every judge passes |
| `ci-*-nocaps` | the next push | the configured cell (paranoid 2): the counting sources serve a launched program; the skip gate's unexpected list should be empty but for what the first push names |

### For `DECISIONS.md`

- **A cgroup is counted like a process**: one event, the sum over the cpus it
  ran on, keyed on the cgroup. Its cpus are where its tasks happened to run,
  not what was asked about; a sampled source keeps each sample's cpu.
- **A container is refused until something resolves it**: `container: no` on
  every entry that serves a cgroup through a collector that cannot resolve a
  container, because the kind falls back to `cgroup` otherwise.
- **Per-cpu rings are sized together**: one size for every cpu, halved for
  all when one is refused, and a refusal at the floor names
  `perf_event_mlock_kb`, `RLIMIT_MEMLOCK` and `CAP_IPC_LOCK`. No probe: the
  budget is per user and only the mapping knows what the user's other
  sessions hold.
- **The CI cell is the kernel's own defaults without capabilities**, not the
  hosted image's: paranoid 2, unprivileged user namespaces, a delegated
  cgroup. Distribution-specific hardening is a cell of its own, later.
- **A build's gap is refused at the factory, not by the validator**: exit 3
  before the launch, and a session asking for it stays valid for a build that
  has it.

### Filed or left

- Filed in `docs/open-findings.md`, "the thirteenth wave, stream P": for S,
  for N, for R, for J and for the coordinator, as listed above, and three
  flakes for the tally.
- Left: `Backend.keys`/`KeyNotCarried` and `Unit::Watts`, each waiting on one
  arm in R's files; the matrix cells this stream did not sweep (below).
- **A mistake of this stream's, for the coordinator**: to stop a full run of
  its own whose build had dropped the capabilities, this stream ran `pkill -f
  'cargo test --workspace --no-fail-fast'`, which matches that command in
  every worktree. Another stream's run of the same command at that moment
  (about 20:34) would have been killed with it and should be taken again.

**Index rows for sessions added**: none; this stream adds no session.

### This wave's check

The last `make check` of this branch stopped at the first red binary, a unit
test of this stream's that failed under the load of five streams and passed
alone (`a_profile_of_the_machine_finds_this_thread_for_the_cpu_it_burned`,
filed for the tally), so the equivalent was run whole on the committed tree:
`cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean, and
one `cargo test --workspace --no-fail-fast` with the capabilities on the
binary -- **1922 passed, 4 failed, 8 ignored**:

- `cli::a_collector_that_needs_a_process_refuses_a_system_target_with_the_reason`:
  red on `main` since the twelfth wave (the coordinator's, filed with what it
  should ask now);
- `collect::tests::weighing_a_run_costs_a_fraction_of_the_run` (tally #12) and
  `scenario_sched::perf_counts_the_blocks_a_raw_tracepoint_counts_and_preemption_besides`
  (the twelfth wave's "for the flaky tally"): in the tally, not rerun;
- `scenario_io::a_pipe_wait_is_in_the_syscall_layer_and_not_in_the_file_layer`:
  not in the tally, passed alone once, filed.

Earlier full runs of the same work met, once each and passing alone: six
`scenario_net` scenarios at load 16 (the file passed whole alone, with the
capabilities), `triage::a_cpu_bound_program_is_named_cpu_bound_for_latency`,
`recorder::a_recording_is_held_to_its_budget...`,
`scenario_mem::a_gibibyte_of_reads...`, `scenario_power::a_core_held_busy...`
and `scenario_proc::what_the_signal_ring_could_not_hold...` (tally #7).
`tests/portability.rs`: 16 passed, 1 ignored, judging the cells above.

The commits between the brief and this one were checked as the final tree,
not one by one: the three of code, then this document.

## The fourteenth wave

The CI's second run (`4d7731b`, `perf_event_paranoid` 2, Linux 6.17.0-azure)
failed 53 of 1994 tests and skipped 10 times for reasons the gate refused, and
most of it traces to two things a developer's machine cannot show: a cpu with
no hardware PMU, and a kernel that counts user mode only for an unprivileged
process. The logs are kept outside the repository, in
`/home/moha/repos/omniscient-ci-logs/` (`run1-paranoid4`, `run2-paranoid2`).
This section is what stream P changed, how each change was checked here --
where neither configuration can be made without root -- and what the next
push's two cells should show.

### How a stock cell was rehearsed here

Not the real thing, and said so wherever it is used: the suite, or one file of
it, run as a transient user service that may open no perf event and is granted
no file capability:

```console
systemd-run --user --pipe --wait --quiet --same-dir -E PATH="$PATH" -E HOME="$HOME" \
  -p SystemCallFilter=~perf_event_open -p SystemCallErrorNumber=EACCES \
  cargo test --test scenario_cpu -- --nocapture
```

`perf_event_open` answers `EACCES` for every event, as at paranoid 4, and
NoNewPrivileges keeps `cap_bpf,cap_perfmon` on `target/debug/omn` from being
granted, as on the runner. What it cannot make: a cpu with no PMU (`ENOENT`
per event), paranoid 2's user-mode-only answer, and the sysctl's own value,
which still reads -1 inside it. The sweep of such a cell under the `stock`
configuration and the judges over it were the check for items 1 and 2.

### 1. A probe that opens what it promises

`probe_pmu` already opened every named, cache and sysfs event it could encode,
and on the runner `pmu:PERF_COUNT_HW_CPU_CYCLES` read *"missing: ... is not
available on this cpu"* (run 2's cell). What was wrong was three answers
around it:

- **The listing called a source usable through a backend nobody wrote.**
  `cpu.cycles` read *"usable through `pmu:sw`"* -- the software clock, every
  requirement available, no collector -- and every run of it exited 3.
  `resolve::availability` now passes over a backend with no collector, as the
  resolver's walk already did: out of reach here, with `pmu`'s missing event
  and `pmu:sw`'s "no collector yet" on the lines below. On this machine,
  fourteen sources changed answer (`omn list --format json` of `main`'s binary against
  this branch's, both with the capabilities): `power.throttle`, `gpu.kernel`,
  `mem.bandwidth`, `net.xdp` and `gpu.counters` leave usable or unverified
  (each was so only through an unwritten backend), and `cpu.profile`,
  `cpu.oncpu_time`, `io.bytes`, `io.iops`, `io.merges`, `sched.switches`,
  `power.freq`, `power.cstate` and `net.hwts` now name the written backend a
  request resolves to. 91 usable here, 1 unverified.
- **The scheduler's counters read zero in user mode.** A context switch and a
  migration are raised from kernel code with kernel registers, so an event
  with `exclude_kernel` drops every one: run 2 measured *"the counter says 0
  context switches where the kernel's own counter says 50"*. The probe asks
  for `PERF_COUNT_SW_CONTEXT_SWITCHES` and `..._CPU_MIGRATIONS` with the kernel
  included, and answers missing with *"raised by the kernel's own code, so a
  count of user mode only reads zero ... needs perf_event_paranoid at 1 or
  below, or CAP_PERFMON"*; the counting collector refuses a group the machine
  would open for user mode only. Ground truth,
  `capabilities::tests::a_switch_counter_that_excludes_the_kernel_counts_none_and_is_not_offered`:
  thirty 1 ms sleeps on the test's own thread, against
  `/proc/thread-self/status` -- the user-mode counter reads exactly 0, the
  full one between 30 and the kernel's count. That upper bound holds by
  construction: the counter is opened disabled and enabled only after the
  first status read, and disabled before the second, so its window lies
  inside the kernel's bracket and a switch in the gaps is the kernel's alone.
  (As first landed, the counter ran from the open to the read, a window
  holding the bracket, so the bound it asserted was in fact equality, red
  whenever the thread was preempted in a gap.)
- **AMD's op sampler read available wherever its unit is published.**
  `pmu:ibs_op` was a device-presence answer; in the rehearsal it read available
  and both `mem.latency` and `mem.sharing` were refused at the attach -- the
  sandboxed sweep's judge found it. It is opened now the way its collector
  opens it (one event following this process on one cpu, `swfilt` where the
  unit has it), and refused says so by name. Ground truth,
  `capabilities::tests::the_op_sampler_is_available_exactly_when_it_opens`:
  an open of the unit made without the probe's code (its type read straight
  from sysfs, no config bit, the kernel's ops included, so it asks for more
  than the probe does) -- where it opens, the probe must be available, where
  it is refused, missing with `perf_event_paranoid` named. Then a refusal is
  made whatever the machine allows: a seccomp filter on one thread of the
  test's own answers EACCES to `perf_event_open`, as a sandbox does, and the
  probe run on that thread must read missing, naming the sysctl and
  CAP_PERFMON. This host publishes `ibs_op`: it opens, the probe says
  available, and under the filter missing -- thirty runs of both tests, all
  green at a load of 8.

Beside them, found on the same runs:

- **`mem.faults` was refused at paranoid 2.** It sampled with the kernel
  included, which paranoid 2 refuses outright. It falls back to user mode now
  and says so: a fault is raised with the registers of the code that took it,
  so every fault the program's own code takes is there, and the faults the
  kernel takes on its behalf are not. Ground truth,
  `page_faults::tests::sampled_in_user_mode_the_programs_own_faults_arrive_and_the_kernels_do_not`:
  a block the test's code writes and a block only `read()` from `/dev/zero`
  fills -- in user mode, 4 to 64 samples land in the first and exactly 0 in the
  second; with the kernel, the second gets 4 to 64. A caveat on the entry says
  the same.
- **`cpu.throttle`'s explanation was wrong.** Its header said the throttling
  counters appear only with a quota; they appear with the cpu controller
  (this machine's `app.slice`: `cpu.max` at `max 100000` and the three
  counters, at zero). The runner's delegated cgroup has exactly that shape,
  so `family_cpu`'s disclosure test skipped *"this session's cgroup has a
  quota"*, and a cgroup with no quota reported silent zeros. Both shapes are
  disclosed now, each as *"... cannot be throttled by one -- which is not the
  same as not having been"*.

**Considered and not done: refusing as the machine.** The resolver still
resolves `cpu.cycles` on the runner to `pmu:sw` and the runner refuses it as
the build's gap (exit 3), with *"`pmu` was passed over: ... not available on
this cpu"* on the next line. Refusing it as the machine (exit 1) was written
and reverted: it turned every `--dump-request` of a source whose written
backends the machine withholds into a refusal (`io.latency --system` without
capabilities: `tests/cli.rs` and `tests/budget.rs`, three tests), and the
gap is real -- a `pmu:sw` collector would serve that machine. The listing was
what lied, and it is what changed. The tests that reach a written backend
through such a machine now skip on it (`withheld_here` in `scenario_cpu`,
`written_backend_withheld` in `family_cpu`).

### 2. The CI, on two cells

`.github/workflows/ci.yml`'s job is a matrix of two cells, `fail-fast: false`:

| Cell | Configured | What it is for |
|---|---|---|
| `defaults` | `perf_event_paranoid` 2, AppArmor's user-namespace restriction off, a delegated cgroup (`omn-ci/job`) | the kernel's own defaults: the unprivileged perf path is exercised |
| `stock` | nothing: paranoid 4, the restriction on, root's service cgroup | the hosted image as shipped: every perf test skips with the machine's reason |

Each sweeps its matrix cell (`ci-6.17-nocaps` and `ci-6.17-stock`), runs the
suite, and gates the skips: `.github/scripts/skip-gate.sh` reads several
expected files now, and the `stock` cell adds `.github/expected-skips-stock.txt`
-- the sysctl (`perf_event_paranoid is [3-9]`), `unshare -Urm` refused, and a
cgroup the runner's user cannot create, each read off run 1's gate. The judge
in `tests/portability.rs` learned the `stock` configuration (every `pmu:`
requirement missing by the sysctl or by the cpu, and `bpf` missing like
`nocaps`) and one more rule: a source listed usable through a backend with no
collector fails the cell. Both were checked against a synthetic cell each, and
the rehearsed stock cell (39 of 101 usable) and this host's (91) pass every
judge.

The ten unexpected skips of run 2, sorted:

| Skip | Whose | Now |
|---|---|---|
| `json` is one document ... (×2) | K's `scenario_mem`, over `mem.faults` refused at paranoid 2 | runs: `mem.faults` samples in user mode; the skip line's wording filed for K |
| `cross-memory-which-allocation.yml` withholds a source (×2) | J's `cross_memory`, same cause | runs; the skip line's layout filed for J |
| this session's cgroup has a quota | P's `family_cpu` | runs: it read the counters as a quota |
| this machine publishes no `cpu` unit | P's `capabilities` | worded *"no `cpu` performance unit"*, which the gate's hardware line reads |
| `cpu.cycles`: perf_event_open on cpu 0 refused (ENOENT) | P's `percpu` | worded *"this cpu has no hardware counter for cycles"*, from the probe |
| /bin/sh could not be copied | P's `scenario_cpu` | the copy is renamed into place and a spawn that meets `ETXTBSY` is retried; the cause is the likeliest reading, not confirmed |
| the program did not report itself | J's `cross_machine` | filed |
| bpftool could not dump the BTF | K's `btf.rs` | filed |

Of run 2's 53 failures, this stream's eight are answered: the three
`family_cpu`, the three `scenario_cpu`, `scenario_gpu`'s forced-backend count
(the runner serves `gpu.api` alone; the count now comes from the listing) and
`counters::context_switches_agree...` (refused by name instead of reading 0).
The rest are other streams' and are filed, one request per owner, in
`docs/open-findings.md`, "the fourteenth wave, stream P": `cpu.profile` (U's,
behind 27 of them), the counting sources a cpu with no PMU withholds (R's
tests, K's sweeps), `sched.switches` on `pmu:sw` (K's tests, which the new
probe answer turns from a zero into a refusal), and the prototypes and fd
counts of 6.17-azure (K's).

### 3. What the thirteenth wave filed against this stream's files

| Filed | Checked | Now |
|---|---|---|
| `check_decomposition_in` against the session's joins (J) | the validator still called `check_decomposition` | the session's joins are passed; `validate::tests::a_part_read_from_a_join_is_checked_against_the_sessions_joins` (the joined session validates, one with a join of another name is refused naming it) |
| an attribute a ranking names is one the entry declares (R) | not checked: `topk:5@nosuch` ranked every event as `nosuch=?` | refused with the entry's attributes and the nearest (`validate::tests::a_ranking_by_an_attribute_the_entry_does_not_declare_is_refused`) |
| `SCHED_SWITCHES`'s `dead_code` (S) | `sched.switches` hands its counted requests to it | the `allow` is gone |
| `Unit::Watts` and `gpu.power` in watts (open-requests 957) | R's vocabulary is on `main` | `gpu.power` gains `count` and `rate`: one reading at each end of the window and `enabled_ns` between them, so the rate is the mean draw in watts. Ground truth, `scenario_gpu::a_counted_energy_window_divided_by_its_length_is_the_boards_draw`: against `nvidia-smi --query-gpu=power.draw` sampled every 200 ms over the same window, from another process (21.09 W against 20.47 W here, idle); the bar is a factor of two, which a rate of millijoules or a running total summed each second would break. `examples/gpu-draw-in-watts.yml`. The text still prints `J/s`, filed for R |
| `KeyNotCarried` once R's vocabulary is on `main` | the variant is there; `Backend.keys`, which says what a backend attaches, is not, and it is `src/catalog.rs` (R's) | left: the resolver half is `check_filter_fields` over the other list, and waits on the field (the thirteenth wave's filing has its signature) |
| the multi-size THP caveat on `mem.faults` (N's text) | still "the kernel picks larger folios" | N's text, and a second caveat for user-mode sampling |
| `mem.swap` answers `rate` (N, for `validate.rs`'s test) | the test already uses `mem.rss` | landed before this wave |

### This wave's check

`make check` stops at the first red binary, and `tests/cli.rs`'s index test is
red on this branch by design (the row above), so each commit of code was
checked the way the rule allows: `cargo fmt --check` and `cargo clippy
--all-targets -- -D warnings` clean, and one `cargo test --workspace
--no-fail-fast` with the capabilities on the binary. The last one, on the
final tree at load 15 to 20 with the other streams running: **1981 passed, 29
failed, 8 ignored**, and none of the 29 in this stream's paths:

- `cli::the_session_indexes_hold_exactly_the_sessions_on_disk`: the row above.
- 21 of `scenario_io`'s 39, nearly all *"No space left on device"* (19 of
  the file's panics) -- `/home` at 98 %, 4.5 GB free, with six worktrees'
  `target/` on it; one of them rerun alone passed. Among the rest are the two
  tallied ones below, and `a_page_a_write_appends_is_created_and_never_a_miss`
  (*"a megabyte appended is 256 pages created, left: 0"*), which ran beside
  them and is most likely the same full disk.
- In the tally or already filed: `family_sched::a_task_moved_between_two_cpus_is_counted_once_per_move`
  (`docs/flaky.md` #5), `scenario_io::a_sequential_read_is_read_ahead_of...`
  and `a_pipe_wait_is_in_the_syscall_layer...` (`docs/open-findings.md`), the
  two `bench_harness` tests (one report file shared by two tests, filed to
  nobody in the thirteenth wave), and `scenario_net::every_datagram_sent_to_a_closed_port...`
  (filed above for the tally; it fails alone here too).
- Not in any tally, each passed alone once:
  `cross_compute::a_lossy_profile_marks_its_row_and_leaves_the_traced_rows_alone`
  (*"`sched.runqlat` summed 19.896 ms against the kernel's 26.609 ms"*),
  `join_engine::a_filtered_part_that_matched_nothing_is_a_zero_and_the_table_adds_up`
  and `scenario_sched::sharing_one_file_is_the_only_difference_between_these_two_runs`
  (*"attributed a wait to pid 925331, which is not one of the 12 it started"*).

This stream's own files, run alone on the final tree, both on this host and in
the rehearsed stock cell: `family_cpu`, `scenario_cpu`, `scenario_gpu`,
`portability`, `catalog_model`, and the power, gpu and runtime family and
scenario files, all green; `scenario_power::a_core_held_busy...` failed once in
the first full run for the machine's state and is fixed above.

After a review, one more pass, checked the same way (`make check` stops at
the index test; one `cargo test --workspace --no-fail-fast` at load 6 to 8,
capabilities on the binary; clippy and fmt clean): **2004 passed, 7 failed,
8 ignored**, none in this stream's paths. The index test (the row below);
`family_sched::a_task_moved_between_two_cpus...` (`docs/flaky.md` #5),
`scenario_sched::an_overflowing_wakeup_ring...` (#3),
`scenario_io::a_sequential_read_is_read_ahead_of...`,
`scenario_sched::perf_counts_the_blocks_a_raw_tracepoint...` and
`runner::a_source_attached_for_one_phase...` (each in
`docs/open-findings.md` or `docs/scenarios/attach.md`), and one in no tally,
`scenario_sched::a_mutex_nobody_else_wants_is_held_as_long...` (*"3230753 ns
against 135630634 ns, which is not a separation"*): each passed rerun alone
once. The pass changed:

- the switch counter's test, whose window now lies inside the kernel's
  bracket (item 1 above);
- a test of `pmu:ibs_op` of its own (item 1 above);
- `scenario_cpu::a_profile_of_a_cgroup_samples_its_task_and_not_the_one_beside_it`,
  which failed once in a review's run of this stream's files
  (*"the window did not hold the child's work in any run"*) and passed alone.
  The cause is measured: `omn cpu.profile --cgroup` takes 930 to 975 ms here
  from its spawn to its window's open at a load of 8, and the shared
  `over_a_cgroup` helper led the workload by 300, 600 and 1200 ms, so the first
  two runs were always late and the third had a quarter of a second to spare.
  The next lead is now taken from the late run's own spawn-to-open, half as
  long again, and there are four runs: three runs in a row pass, 20 s each.

### Index rows for sessions added

`examples/gpu-draw-in-watts.yml`, in
`examples/README.md` and `GUIDE.md`'s gpu table:

```
| `gpu-draw-in-watts.yml` | How many watts the board drew over a window, from its energy counter. |
```

### For `DECISIONS.md`

- **A source is usable only through a backend this build collects.** The
  listing's `here` is "available means it attaches" for this binary; a backend
  with no collector is shown as such and never makes a source usable. The
  resolver's order is unchanged: a request still resolves to an unwritten
  backend when no written one is viable, and is refused as the build's gap.
- **A counter the kernel raises from its own code is probed with the kernel
  included**; a user-mode-only answer is missing, because the count it gives
  is zero whatever the program does.
- **A sampled source falls back to user mode where the kernel allows no
  more**, when its events are raised with the program's registers (page
  faults), and says what that loses; a counted one already did.
- **The CI has two cells**, the kernel's defaults and the hosted image as
  shipped, and each has its own expected skips beside the common ones.

### Filed or left

- Filed in `docs/open-findings.md`, "the fourteenth wave, stream P": for R
  (four groups of tests that should skip where the machine withholds a source;
  a rate over joules printed as watts; optionally the `here` line), for K
  (`sched.switches` on `pmu:sw`, the `scenario_mem` sweeps, two skip lines),
  for J (two skip lines and one refusal), and three for the flaky tally.
- Left: `KeyNotCarried`'s resolver half, on `Backend.keys` in R's
  `src/catalog.rs`; the `pmu:sw` collector for `cpu.cycles` (cpu time under
  the name of cycles, which the catalog declares as a degraded fallback and
  nobody wrote); the matrix cells above.
- For the coordinator: `CLAUDE.md`'s *"86 usable sources and 51"* is a count
  this branch changes (91 with the capabilities here).

### The matrix cells this change should move

Nothing here was swept in a guest (the matrix runs alone, at the end of the
wave). What should change, and how to see it:

| Cells | Command | What should change |
|---|---|---|
| `ci-6.17-nocaps` (`defaults`) | the next push | `cpu.cycles` `here: no`, `pmu` missing *"not available on this cpu"*; `cpu.context_switches`, `cpu.migrations` and `sched.switches`'s `pmu:sw` missing *"reads zero ... CAP_PERFMON"*; `mem.faults` attaches for a launched program; `pmu:ibs_op`, if that cpu publishes it at all, answered by opening it; every judge passes, and the gate's unexpected list holds only the filed rows |
| `ci-6.17-stock` (`stock`, new) | the next push | every `pmu:` requirement missing by the sysctl; the suite fails only in the filed rows; the gate passes with both expected files |
| 5.15, 6.1, 6.6, 7.2 under `nocaps` | `scripts/matrix.sh v5.15.221 v6.1.188 v6.6.157 host --configs nocaps` | where the guest's `perf_event_paranoid` is 2 or above (`cell.sh` prints it; this host's `/etc/sysctl.d/99-perf.conf` sets -1, and whether a guest applies it is not checked here), the switch and migration counters read missing rather than attaching and counting zero; at 1 or below nothing changes; `mem.faults` attaches either way; judges pass |
| the same four under `caps` | `scripts/matrix.sh v5.15.221 v6.1.188 v6.6.157 host --configs caps` | no source changes except those listed above as leaving usable through an unwritten backend (`power.throttle` where `thermal_throttle` is absent, and the like); the new judge (no usable-through-unwritten) passes |

## The fifteenth wave

What stream probes changed, each with the test that holds it; the handoff is
`docs/fixes/w15-probes.md`.

### 1. A container is served wherever its cgroup is

`omn` binds `--container NAME` to the cgroup its runtime names before
resolution (stream R, the fourteenth wave), and eleven entries still said
`container: no` beside a served cgroup, so the validator refused the
container first: `cpu.cycles`, `.instructions`, `.ipc`, `.branch_misses`,
`.cache_misses`, `.topdown`, `.context_switches`, `.migrations`, `.throttle`,
`cpu.profile` and `mem.faults`. The key is gone from those, from the three
`mem.cache_*` entries (the same counter group) and from `sched.switches` and
its two programs (which serve a cgroup in `count`). A container falls back to
`cgroup` in every one; the validator's *"which this tool does not do"* is gone;
and the loader refuses a catalog that says `container: no` beside a served
cgroup (`validate::container_complaints`).

- `scenario_cpu::a_container_is_counted_as_the_cgroup_its_runtime_names`: a
  cgroup made here, a `sleep` in it that a runtime script names as the
  container, and `cpu.instructions --container` counts the child's 10^8
  retired instructions inside it and none of the parent's 10^9 beside it.
- `scenario_cpu::a_container_is_asked_of_its_runtime_where_its_cgroup_is_served`:
  `cpu.instructions`, `cpu.profile`, `mem.faults` and `cpu.throttle
  --container web` against a runtime that knows no container answer exit 1
  with the runtime's own *"No such object: web"*, never *"per container"*.
- `tests/cli.rs::a_container_is_served_by_a_source_that_serves_its_cgroup`
  (R's) runs instead of skipping: 388 switches counted in the container's
  cgroup, 385 blocks of the task inside by the kernel's count.

### 2. A pair no backend declares is the request's; capabilities the kernel withheld are named

- **A (target, mode) pair the entry accepts one half at a time and no backend
  declares together is exit 2**, before resolution, naming each backend's
  half: `sched.switches --cgroup -m trace` read *"has no viable backend on
  this machine"*, exit 1, and now reads *"no backend of its chain declares
  both, so no machine could serve it (`pmu:sw` not per cgroup, in count,
  trace; `bpf:raw_tp` per cgroup, in count; ...)"*. It is the only such
  triple in the catalog today.
  `validate::tests::a_target_and_mode_no_backend_declares_together_are_refused_as_the_request`,
  and `a_pair_refused_as_the_catalogs_would_not_have_resolved_on_this_machine`,
  which holds every refused triple to the resolver's own walk on this machine.
- **A binary whose file capabilities the kernel did not grant says so.** The
  probe reads the file's `security.capability` and the process's `CapEff`;
  where the file carries `CAP_BPF` or `CAP_PERFMON` and the process lacks it,
  a privilege refusal names why instead of advising the `setcap` already run:
  the `nosuid` mount (from `mountinfo`, with `findmnt -no OPTIONS -T <exe>`),
  `no_new_privs`, or a grant without `e`.
  `capabilities::tests::a_mount_is_nosuid_exactly_when_statvfs_says_so` holds
  the mount reading to `statvfs`'s `ST_NOSUID`;
  `a_binary_s_file_capabilities_are_read_as_getcap_prints_them` the decoding
  to `getcap`; `portability::a_binary_denied_its_file_capabilities_says_why_instead_of_advising_the_grant`
  runs the real binary under `PR_SET_NO_NEW_PRIVS`. The attach-time advice in
  `src/bpf/mod.rs` is filed (not this stream's file). A `nosuid` mount cannot
  be tested end to end without root: copying a file's capabilities needs
  `CAP_SETFCAP`.
- `.github/expected-skips.txt`: the cgroup-ancestry skip wording in, the
  `/sys/class/block/root` line out.

### 3. `cpu.cycles` on a cpu with no hardware PMU

Served, through the `pmu:sw` backend the catalog already declared degraded
(*"CPU time, not cycles: blind to DVFS"*): the kernel's cpu clock, in the same
counter group and window, under the counter name `cpu_clock_ns`, with the
degradation in the result. On a cloud VM with no PMU `cpu.cycles` now measures
where it was refused as the build's gap (exit 3). Decided for the chain rather
than for a refusal by name: `DECISIONS.md`'s open line said leaving it open
cost `cpu.cycles` on most cloud VMs, and the project's line is every mechanism
as a chain, with a fallback that says what it degrades. The unit stays the
entry's (`count`), which is the price: a reader who ignores the `degraded:`
line reads nanoseconds as cycles, at one gigahertz.

- `counters::tests::the_cpu_clock_fallback_counts_the_nanoseconds_the_thread_ran`:
  50 ms of the thread's own `CLOCK_THREAD_CPUTIME_ID`, counted to a tenth.
- `scenario_cpu::a_cycle_count_and_its_cpu_clock_fallback_differ_by_the_core_s_frequency`:
  through the binary, 400 ms more of burned cpu counted 400 ms more to 15 %,
  and the hardware counter over the fallback is the core's frequency, within
  cpufreq's range (4.2 GHz here).

### 4. The bench's guardrail and `power.*`

Not changed here, and why. `ROADMAP.md` phase 9 says `power` has been
collected unconditionally since phase 8 as a guardrail. What the bench does
(`examples/bench/harness.rs`, the runner stream's) is read
`/sys/devices/system/cpu/cpu*/thermal_throttle/{core,package}_throttle_count`
before and after each repetition and invalidate the repetition if the sum
moved -- the very files `power.throttle`'s `fs:sys` backend reads, and none of
`power.freq`, `power.energy` or `power.temp`. The direct read is defensible (a
polled source reads a sysfs file per cpu per tick inside the window it
guards, and the bench resolves 1 % differences), but it is not "`power.*`
collected", and a machine without the counters -- this AMD one, any VM --
answers `None` in the bench where the resolver would name the missing file.
Filed for the runner stream with a signature (`docs/open-findings.md`, the
fifteenth wave, stream probes): run `power.throttle` over each repetition, or
keep the read and correct the phase 9 sentence.

### 5. Two cells compared directly, and the resolved file held to a gap

- `portability::two_cells_list_different_sources_for_the_condition_between_them`
  (phase 2): for every guest, the cell that withholds (`nocaps`, `nobtf`,
  `notracefs`, `caps` against `root`) lists no source the one that grants
  does not, the capability and BTF pairs list strictly fewer, and every
  source usable only where the condition is granted names it in the
  withholding cell's reason. On the cells on disk: 41 against 71 to 80 for
  `nocaps`/`caps`, 45 against 75 to 81 for `nobtf`/`root`, 52 against 92 for
  `here-nocaps`/`here-caps`.
- `portability::every_gap_between_two_cells_is_explained_by_their_resolved_records`
  (phase 7): over every two of the 22 cells of the finished 2026-09-30
  sweep, each source whose availability differs has requirements that
  answered differently, and the refusing side's probe names the cause in its
  own words -- a BTF it could not read, a capability, the sysctl, a
  tracepoint this kernel does not define, hardware the machine does not
  publish. The field of the two resolved records standing for that cause has
  to differ the way that refuses on the refusing side (a `kernel>=`
  requirement: that side's release older, the other's not); a field that
  merely differs explains nothing, and a reason naming nothing a record
  carries fails. Explained: `kernel_btf` (1716), `tracefs` (991),
  `kernel_release` (255), `gpus` (200, NVML), `perf_event_paranoid` (20).
  **2754 are not explained yet** -- 2514 only the privileges could explain,
  197 only the virtualization, 43 both -- because the resolved record carried
  neither. (A first version credited the alphabetically first field that
  differed, and put 1740 on `kernel_release` and 296 on `gpus`: a `bpf` gap
  between `caps` on one kernel and `nocaps` on another is the capabilities',
  a `hwmon` gap between this host and a guest is the virtualization's.) The
  record carries both now: `Environment.privileges`
  (uid, the effective capabilities this tool asks for, `no_new_privs`, what
  the file carries that the process was not given) and
  `Environment.virtualization` (`none`, or the firmware's name for the
  machine), held to `capget` and `systemd-detect-virt --vm`. The test counts
  those and says so until the matrix is swept again; with new cells a gap
  only a missing field explains fails. `a_gap_is_explained_only_by_a_recorded_field_that_differs`
  and `a_gap_is_credited_to_the_cause_the_refusal_names_not_to_any_field_that_differs`
  judge the judge on planted cells.
- A cell now records its `build` (a hash of the binary that swept it): two
  cells differing by what the build answers (a probe not written, a listing
  rule) are the build's difference, not the machine's, and are held only
  between cells of one build. Cells on disk predate it: 83 such differences,
  against `here-nocaps`, swept on 2026-09-29.

### The matrix cells this change should move

| Cells | What should change |
|---|---|
| every cell | `environment.privileges`, `environment.virtualization` and `cell.build` recorded; `every_gap_...` explains the 2754 it counts as unrecorded today, or fails naming the ones it cannot |
| every guest | nothing for `cpu.cycles`: the guests expose the host's PMU and stay on `pmu`; `pmu:sw` now reads `collector written` in the listing |
| `ci-6.17-*` | `cpu.cycles` served on the runner's vcpu, `pmu:sw`, with the degradation |

## The sixteenth wave's sweep

All 22 cells swept again on 2026-10-02, alone, with one binary
(`cell.build` `cd394a8a8550c199`, `main` after the sixteenth wave's merge):
`here-caps`, `here-nocaps`, and 5.15.221, 6.1.188, 6.6.157 and 7.2.7 under
`caps`, `root`, `notracefs`, `nobtf` and `nocaps`. Every guest cell completed
with 0 broken; `6.1.188-caps` has a sweep again (it had only a qemu `vhost`
error before). `here-nocaps` was swept from a relinked binary, which carries no
capabilities, and `make` put them back. Usable sources, as `omn list` counts
them since the fourteenth wave (through a backend this build collects):

| Kernel | caps | root | notracefs | nobtf | nocaps |
|---|---|---|---|---|---|
| 5.15.221 | 71 | 75 | 75 | 45 | 41 |
| 6.1.188 | 74 | 78 | 78 | 45 | 41 |
| 6.6.157 | 78 | 79 | 79 | 45 | 41 |
| 7.2.7 | 80 | 81 | 81 | 45 | 41 |
| here (7.2.7, no guest) | 92 | | | | 56 |

The table under "The cells" above is an earlier binary's, and the two counts
are not comparable.

What the cells hold now:

- every cell records `environment.privileges`, `environment.virtualization`
  and `cell.build`;
- `every_gap_between_two_cells_is_explained_by_their_resolved_records`:
  among the 22 fresh cells every gap is explained by a recorded field that
  differs the way that refuses, none is the build's, none waits on a missing
  field (phase 7's criterion, ticked). The first pass left 58 unexplained, all
  the judge's (`3e78c7c`): `runtime.*` through `uprobe` on 5.15 and 6.1 is
  refused for two causes at once, uprobe-multi (6.6) and the uprobe PMU's
  `CAP_SYS_ADMIN`, and the judge read only the privilege
  (`Cause::KernelBelowOr` now); and `net.hwts`'s `so_timestamping` has no
  probe, which the judge counted as the build's difference "under one build"
  where the real difference is `phc` (an unwritten probe is the build's only
  where the two cells answered it differently);
- `here-caps`: `net.hwts --system` runs in `poll` through `netlink:ethtool`
  and exits 0; `here-nocaps`: the five stale "usable through a backend with no
  collector" violations are gone; the invariants judge passes;
- `sched.switches` is usable on 5.15 through `pmu:sw`: only its `bpf:raw_tp`
  backend needs 6.1, so the row "What differs by kernel" gives it is a
  backend's refusal, not the source's (the docs stream's question,
  `docs/open-findings.md`).

The five `7.2.6-*` cells on disk predate the host's move to 7.2.7 and record
none of the new fields; they alone account for the judge's 1316 "not
explained yet". `rm -r target/matrix/cells/7.2.6-*` clears them. Logs:
`target/matrix-here.log`, `target/matrix-all.log`.
