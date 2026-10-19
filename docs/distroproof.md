# What distroproof found in Omniscient

[Omniscient](../README.md) is a Linux observability tool that talks to the
kernel directly: eBPF programs assembled by hand, perf events, BTF, file
capabilities, a preload shim injected into other programs. All of that
depends on the machine it runs on -- the kernel, as its distribution configured
and patched it, the C library, the defaults an administrator never changed.
For twenty-five waves of development it was tested on one machine.

[distroproof](https://github.com/devutils-org/distroproof) boots each
distribution's official cloud image **on its own kernel** in QEMU/KVM, runs a
command in it, and attaches to every result an `envprobe` report of the
machine -- kernel config, sysctls, LSMs, capabilities, BPF and perf support,
libc -- so that a failure comes with the facts that explain it.

This document is what happened when Omniscient was run through it: every
finding, the evidence distroproof put in front of it, the cause, and the
commit that fixed it. It is meant to be read as distroproof's living proof.

## At a glance

| | |
|---|---|
| Distributions | **16** images on their own kernels -- Debian 10, 11, 12, 13 and sid; Ubuntu 18.04, 20.04, 22.04, 24.04; Rocky 8 and 9; Fedora 43 and 44; Arch; Gentoo; Alpine -- and **5** variants (paranoid 3, `kptr_restrict` 2, lockdown, no PMU, root); **3** arm64 images (Debian 13, Ubuntu 24.04, Fedora 44, emulated); **8** rootless podman containers (Debian and Alpine, four privilege levels) |
| Kernels | 4.15 to 7.2, RHEL's 4.18 and 5.14 with their backports |
| Time | the 21 targets in about **12 minutes**, four at a time, from one command |
| Findings in Omniscient | **26 defects** no test on the development machine had shown: **12 in the tool** (the release, the probes, the catalog, a collector) and **14 in its test suite** (a judge, the reference runner, twelve reference pairs) |
| Claims refuted | **158** runs of a source Omniscient called usable that the kernel refused at start, in the first matrix; **0** after |
| Cross-checked | every reason Omniscient gives for a refusal agrees with envprobe's independent reading of the same guest, on all 21 targets |
| Back into distroproof | 3 images (Gentoo, Debian sid, Fedora 43), 2 boot fixes, 3 `[run]` options, 2 envprobe facts (pull requests [#4](https://github.com/devutils-org/distroproof/pull/4), [#5](https://github.com/devutils-org/distroproof/pull/5)) |

## Reproducing it

```console
scripts/release.sh                       # the archives, glibc 2.17 and musl
ARCH=aarch64 scripts/release.sh          # the same for the arm64 guests
scripts/distroproof/fixtures.sh          # the USDT fixtures, static, per architecture
distroproof up --no-tui --jobs 4 --json target/matrix/distroproof.json
scripts/distroproof/cells.py target/matrix/distroproof.json
scripts/containers.sh                    # the rootless podman cells, on this machine
cargo test --test portability            # the judges, over every cell
```

[`distroproof.toml`](../distroproof.toml) is the matrix. Each guest gets the
release archives, the fixtures and two scripts -- never the checkout. Its
`setup` installs the archive a user would (the musl one where there is no
glibc loader); distroproof grants the file capabilities on the installed
binary (`file_caps`); the command is `omn selftest --format json`, every
source listed with its reasons and run for 300 ms on a launched program and
on the machine, as an ordinary user; distroproof brings the document back
(`collect`) beside its envprobe report. On the host,
[`tests/portability.rs`](../tests/portability.rs) judges every cell: what is
listed usable attaches, every refusal names its reason, and every reason
agrees with envprobe.

[`distroproof-reference.toml`](../distroproof-reference.toml) runs the
reference pairs -- Omniscient against `perf`, `bpftrace`, `strace`, the bcc
tools, on the same workload, within a declared tolerance -- on six
distributions with the witnesses their own packages carry.

## The matrix

| Target | Kernel | Configuration | Sources listed usable (of 123) |
|---|---|---|---|
| `debian-10` | 4.19 | no capabilities (none before 5.8) | 36 |
| `debian-11` | 5.10 | file capabilities | 41 |
| `debian-12` | 6.1 | file capabilities | 76 |
| `debian-13` | 6.12 | file capabilities | 82 |
| `debian-sid` | 7.2.11 | file capabilities | 82 |
| `ubuntu-18.04` | 4.15 | no capabilities | 36 |
| `ubuntu-20.04` | 5.4 | no capabilities | 39 |
| `ubuntu-22.04` | 5.15 | file capabilities | 85 |
| `ubuntu-24.04` | 6.8 | file capabilities | 93 |
| `rocky-8` | 4.18 (RHEL) | file capabilities | 59 |
| `rocky-9` | 5.14 (RHEL) | file capabilities | 90 |
| `fedora-previous` | 6.17 (Fedora 43) | file capabilities | 96 |
| `fedora-latest` | 6.19 (Fedora 44) | file capabilities | 96 |
| `arch` | 7.2.9 | file capabilities | 96 |
| `gentoo` | 6.18 | file capabilities | 96 |
| `alpine` (musl archive) | 6.18 `virt` | file capabilities | 85 |
| `ubuntu-24.04-paranoid3` | 6.8 | `perf_event_paranoid` 3 | 93 |
| `debian-12-kptr2` | 6.1 | `kptr_restrict` 2 | 76 |
| `fedora-latest-lockdown` | 6.19 | `lockdown=confidentiality` | 55 |
| `debian-12-nopmu` | 6.1 | no PMU | 76 |
| `debian-12-root` | 6.1 | root | 94 |
| `debian-13-arm64` | 6.12 arm64, emulated | file capabilities | 82 |
| `ubuntu-24.04-arm64` | 6.8 arm64, emulated | file capabilities | 86 |
| `fedora-latest-aarch64` | 6.19 aarch64, emulated | file capabilities | 88 |
| `podman-{debian,alpine}-root` | the host's 7.2, rootless podman | root of the container | 44 |
| `podman-{debian,alpine}-user` | the same | uid 1000 in the container | 44 |
| `podman-{debian,alpine}-capadd` | the same | `--cap-add CAP_BPF,CAP_PERFMON,CAP_SYS_ADMIN` | 57 |
| `podman-{debian,alpine}-privileged` | the same | `--privileged` | 59 |

The spread is the point: the same binary serves 36 sources on Ubuntu 18.04
and 96 on Fedora, and says, source by source, why.

The arm64 cells run the aarch64 archives (`ARCH=aarch64 scripts/release.sh`)
in guests distroproof emulates (TCG): 7 to 8 minutes a cell. The container
cells run on this machine's kernel through rootless podman
(`scripts/containers.sh`): a user namespace whose capabilities are its own,
podman's default seccomp profile, and no tracefs. No eBPF source attaches in
any of them -- `unprivileged_bpf_disabled` is 2 and a namespace's CAP_BPF is
not the machine's -- and the perf sources are refused by the seccomp filter
until CAP_PERFMON or CAP_SYS_ADMIN is added, which lifts it.

## Findings

Each finding is what was seen, what distroproof showed, the cause, the fix,
and what the next run said.

### 1. The release did not start on a third of the images

**Seen.** `./omn/bin/omn --version` on Rocky 8: nothing but a loader error.

**What distroproof showed.** The images put numbers on a promise nobody had
checked: Rocky 8 and Debian 10 ship glibc 2.28, Ubuntu 18.04 2.27, Debian 11
and Ubuntu 20.04 2.31 -- and the archive imported `GLIBC_2.39` symbols,
because a binary needs the newest glibc it was linked against and the build
machine ran 2.44. The documented floor said 2.34; it was already false.

**Fix** ([805ec52](https://github.com/moha-bekh/omniscient/commit/805ec52)).
The archive is linked against glibc **2.17** through zig (`cargo zigbuild
--target x86_64-unknown-linux-gnu.2.17`), whatever the build machine runs.
One call changed: `memfd_create`, whose glibc wrapper is 2.27's, is made as
the system call. `scripts/release.sh verify` holds the floor.

**After.** The archive starts on every glibc image, Rocky 8's 2.28 and Ubuntu
18.04's 2.27 included.

### 2. Alpine: "not found"

**Seen.** `./omn/bin/omn: not found` -- the shell's words for a missing
program interpreter, on a file that is plainly there.

**What distroproof showed.** envprobe's `libc` section: `musl`, interpreter
`/lib/ld-musl-x86_64.so.1`.

**Fix** ([d43c6de](https://github.com/moha-bekh/omniscient/commit/d43c6de)).
Both answers were measured and both kept: the glibc archive runs once
`gcompat` is installed, shim included; and a second archive, `-linux-musl`,
links `omn` statically and the shim against musl (built with the host's
linker, it had depended on glibc's `libc.so.6`). A static binary loads no
library, so the NVIDIA sources are refused -- and the refusal now says the
build is the reason, where the loader's "Dynamic loading not supported" read
as the machine's lack.

**After.** Alpine, musl archive, nothing installed: 93 sources usable as
root, eBPF attaching, the shim reading a musl `python3`'s allocations.

### 3. Debian's `perf_event_paranoid` 3: CAP_PERFMON is not enough

**Seen.** On `debian-12`, `mem.swap` listed usable, then refused at start:
"the kernel refused a counting kprobe (Permission denied)" -- from a binary
holding CAP_PERFMON.

**What distroproof showed.** envprobe: `kernel.perf_event_paranoid = 3`.
Upstream Linux knows no level 3; Debian's and Ubuntu's kernels carry a patch,
older than CAP_PERFMON, that refuses every perf event to a process without
CAP_SYS_ADMIN. Ubuntu 22.04 and 24.04 ship 4.

**Fix** ([b38b6c2](https://github.com/moha-bekh/omniscient/commit/b38b6c2)).
The `kprobe` requirement opens an event -- a disabled software counter, which
costs nothing -- rather than trusting the capability held, and every refusal
that names a level above 2 says what this kernel means by it and that
`sysctl kernel.perf_event_paranoid=2` lets CAP_PERFMON in.

### 4. Kernel 5.10: eBPF maps charged to a 64 KiB limit

**Seen.** On `debian-11`, about 45 eBPF runs listed usable and refused:
"creating the ring buffer needs CAP_BPF and CAP_PERFMON" -- both held.

**What distroproof showed.** The test user's `RLIMIT_MEMLOCK` is 64 KiB, soft
and hard. Before memory-cgroup accounting (Linux 5.11) every eBPF map is
charged to it, and the kernel's EPERM reads exactly like a missing
capability. envprobe now reports the limit (`privileges.memlock_soft`), which
it did not before this finding.

**Fix** ([42b9d4d](https://github.com/moha-bekh/omniscient/commit/42b9d4d)).
The `bpf` requirement creates a 1 MiB map; refused where a small one is
created, the maps are charged to the limit, and the requirement is missing
with the limit and its remedies (`ulimit -l`, `LimitMEMLOCK=`, CAP_SYS_RESOURCE).
The same words replace "needs CAP_BPF" wherever a map is refused.

### 5. Lockdown withholds what every program reads

**Seen.** On `fedora-latest-lockdown`, every eBPF source listed usable and
refused by the verifier.

**What distroproof showed.** envprobe: `security.lockdown = confidentiality`.
In that mode the kernel withholds `bpf_probe_read_kernel`, and nearly every
program of Omniscient's reads a kernel struct.

**Fix** ([42b9d4d](https://github.com/moha-bekh/omniscient/commit/42b9d4d)).
The `bpf` requirement loads the smallest program that calls the helper, and
names lockdown when the verifier refuses it.

**After.** 55 sources listed usable under lockdown, every one attaching.

### 6. A kernel without ftrace -- and a probe that was right on 7.2 and wrong on 5.10

**Seen.** On `alpine`, every fentry program loaded and every attach was
refused with EBUSY.

**What distroproof showed.** At first nothing: envprobe said
`CONFIG_FTRACE=y`. Reading Alpine's kernel config in the guest showed no
`CONFIG_FUNCTION_TRACER` -- functions compiled with no call site for a
trampoline. envprobe now reports the function tracer and the dynamic-ftrace
options ([#5](https://github.com/devutils-org/distroproof/pull/5)), and
Alpine reads `CONFIG_FUNCTION_TRACER=n`.

**The fix, twice.** The first one asked the kernel's BTF for
`register_ftrace_function`. On the next run, the envprobe judge
([below](#the-cross-check)) failed on `debian-11`: Omniscient said "built
without ftrace" where envprobe read `CONFIG_FUNCTION_TRACER=y`. A 5.10 BTF,
made by an older pahole, leaves most functions out, ftrace's with them. The
second fix ([693fadb](https://github.com/moha-bekh/omniscient/commit/693fadb))
reads `/proc/kallsyms` for `__fentry__`, which names its symbols even where
their addresses are hidden.

**After.** Alpine refuses every fentry source by name; Debian 11 keeps them.
Two tools reading one machine independently caught a probe that one machine
could not have.

### 7. A kernel struct member renamed between 6.12 and 6.17

**Seen.** On `debian-13`, `io.pagecache` and `io.readahead` refused at start:
"this kernel's `folio` has no member named `_nr_pages`".

**What distroproof showed.** The BTF of each guest: 6.8 and 6.12 name it
`_folio_nr_pages`, 6.17 and later `_nr_pages`. Every kernel Omniscient had
met was on one side.

**Fix** ([42b9d4d](https://github.com/moha-bekh/omniscient/commit/42b9d4d)).
The programs read whichever name the kernel has.

### 8. "kernel>=6.8": a version the catalog believed

**Seen.** On `ubuntu-24.04`, `mem.thp` refused: no per-size huge page
statistics.

**What distroproof showed.** Ubuntu's 6.8 does not publish
`hugepages-2048kB/stats`, which the catalog promised from `kernel>=6.8`.

**Fix** ([42b9d4d](https://github.com/moha-bekh/omniscient/commit/42b9d4d)).
The backend asks for the directory, not the version, and the resolver falls
back to `/proc/vmstat` where it is absent -- what the refusal had told users
to do by hand.

### 9. RHEL's backports: the test suite asked versions too

**Seen.** On `rocky-9`, the judges failed: USDT sources attached through
uprobe-multi links on 5.14 "without saying they fell back".

**What distroproof showed.** They did not fall back: RHEL 9 carries
uprobe-multi, a 6.6 feature, in its 5.14. Omniscient asks the kernel and was
right; the judge assumed the version and was wrong.

**Fix** ([db67711](https://github.com/moha-bekh/omniscient/commit/db67711)).
The judge no longer asks the version.

### 10. The reference pairs had only ever held on Arch

Running the pairs on six distributions with their own witnesses found that
several encoded the development machine:

- **Paths.** The bpftrace pairs named `/usr/share/bpftrace/tools/*.bt`, Arch's
  and Fedora's; Debian and Ubuntu install them in `/usr/sbin`.
- **Interpreter versions.** The syscall workload slept with `time.sleep`,
  which is `clock_nanosleep` from Python 3.11 and `select()` before -- on
  Rocky 9's 3.9 the pairs counted a call that was never made. It calls
  `clock_nanosleep` through libc now.
- **Tool versions.** An older bpftrace prints `killsnoop`'s signal as a
  number (`12`, not `SIGUSR2`); Debian's perf has no `lock contention -b`;
  Fedora maps `libcrypto.so.3.5.5` where Arch maps `libcrypto.so.3`.
- **Refusals counted as failures.** A `runqlat.bt` that cannot build against
  the installed kernel headers (Debian 12), and a cgroup the guest's user may
  not create, are the machine refusing, and now skip quoting it.

Fixed in [87191de](https://github.com/moha-bekh/omniscient/commit/87191de)
and [0de94c4](https://github.com/moha-bekh/omniscient/commit/0de94c4).
The results, per distribution, are [below](#the-reference-pairs).

### 11. "kernel>=6.1": the version the kernel floor was chosen at

**Seen.** On `rocky-9`, `perf lock contention -b` counted the workload's
lock waits, and Omniscient refused `sched.lock` beside it: "kernel>=6.1:
missing: kernel 5.14 is older than 6.1". The reference pair read it as no
output at all (below).

**What distroproof showed.** RHEL 9's 5.14 carries `lock:contention_begin`
and `contention_end`, mainline 6.1 tracepoints, as it carries uprobe-multi
(finding 9). Six backends said `kernel>=6.1` -- `sched.lock`'s two,
`sched.runqlat`, `sched.offcpu`, `sched.wakeup`, `sched.switches`' count and
`cpu.oncpu_time`'s -- beside the tracepoint prototypes they read, which the
probes already ask by name; the version was the floor's leftover
(`DECISIONS.md`, "The kernel floor": "they keep 6.1 until a cell says
otherwise").

**Fix.** The version is gone from the six; the prototypes stay. On
`rocky-9` the four sources it held back attach, on a launched program and on
the machine (86 sources usable, now 90), and 19 292 lock waits were counted
in the diagnostic run; on 5.15 and older the same sources are still refused,
now by the prototype that differs. The pair's empty output was the runner's
defect too: wrapped by a tool that exits 0, omn's refusal was on the shared
stderr and read as "not JSON"; it is read there now, and skips quoting it.

### 12. Seven defects of the pairs and their runner: a distribution, a tool version, a count read as a share

Found by the second and third runs, all in the test suite:

- `stripped-library` compared perf's per-function rows with an object: on
  Fedora `libcrypto` keeps its symbols, so perf named functions. Both sides
  are keyed by object now (`perf report --sort dso`, `omn -k object`).
- `execsnoop.bt` of bpftrace 0.17 prints `TIME(ms) PID ARGS`; the parser
  reads the header for the column.
- `biosnoop.bt` and `biolatency.bt` of bpftrace 0.17 and 0.20 attach no
  probe on 6.1 and 6.8: the tool refused, a skip quoting its probes.
- bpftrace with `-c` drops what is left in its buffer when its child exits
  (Rocky 9: 474 and 575 of 604 opens, no `Lost` line); the workloads linger
  half a second after their last action. A loss bpftrace reports at exit
  (`Total lost event count`) is read as one.
- `task-clock`'s band was a share; perf's own start is a count (16 ms on
  Fedora's guest), now an absolute floor beside it.
- The GPU pairs failed in every guest: their workload file was not copied.
- A tool that exits 0 over omn's refusal (finding 11).

### 13. arm64: a kernel with ftrace and no symbol for it

**Seen.** On `debian-13-arm64`, `ubuntu-24.04-arm64` and
`fedora-latest-aarch64`, `io.vfs`, `io.fs` and the other fentry sources were
refused: "this kernel was built without ftrace".

**What distroproof showed.** envprobe read `CONFIG_FUNCTION_TRACER=y` on all
three. The probe (finding 6) looked for the call site's symbol, `__fentry__`
on x86 and `_mcount` on arm64; an arm64 kernel that patches its function
entries (`-fpatchable-function-entry`, `DYNAMIC_FTRACE_WITH_CALL_OPS`) has
none, and has the tracer's own `ftrace_caller` and `function_trace_call`,
which x86 has too and Alpine's `virt` kernel, built without ftrace, lacks
(read from `/proc/kallsyms` in the four guests).

**Fix.** The probe accepts any of the four. The arm64 cells went from 76, 80
and 82 sources usable to 82, 86 and 88, every one attaching.

### 14. A container's seccomp filter, blamed on perf_event_paranoid

**Seen.** In every rootless podman container, the perf sources were refused
with "perf_event_paranoid is 2" -- a level at which a process may count its
own user mode.

**What envprobe showed.** `seccomp: filter`: podman's default profile refuses
`perf_event_open` unless the container is given CAP_PERFMON or
CAP_SYS_ADMIN, and the 13 perf sources came back as soon as it was.

**Fix.** An EPERM where the level allows the event, under a filter
(`Seccomp: 2` in `/proc/self/status`), is said to be the filter's, with what
lifts it; the envprobe judge holds that reason to envprobe's `seccomp`.

## The cross-check

envprobe and Omniscient read the same guest independently, so their answers
can be held against each other. The judge
`every_distroproof_cell_s_reasons_agree_with_envprobe` does, both ways:

| Omniscient says | envprobe must read |
|---|---|
| `perf_event_paranoid is N` | `kernel.perf_event_paranoid = N` |
| `unprivileged_bpf_disabled` is N | the same sysctl, N |
| the `btf` requirement available / missing | `btf.vmlinux` present / absent |
| the kernel withholds `bpf_probe_read_kernel` | `lockdown = confidentiality`, and only then |
| tracefs / cgroup v2 not mounted | the same, and only then |
| built without ftrace | `CONFIG_FUNCTION_TRACER` not `y`, and only then |
| this process may lock N KiB | `memlock_soft` = N KiB |

Over the 21 targets every reason agrees -- after the one it caught (finding 6).
A planted cell checks that it fails on a wrong cause.

## The reference pairs

Every pair (`tests/reference.rs`, [`docs/replaces.md`](replaces.md)) on six
distributions, one at a time, against the release a user installs, with the
witnesses each distribution packages. Three runs: before and after the pairs'
own portability fixes (finding 10), and after findings 11 and 12, with the
pairs added since (128 in all, the GPU ones now skipping for want of a GPU
rather than failing for want of their workload file in the guest).

| Distribution | Kernel | First run: held / did not hold | Second: held / did not hold / skipped | Third (2026-10-19) |
|---|---|---|---|---|
| Arch | 7.2.9 | 69 / 2 | 71 / 0 / 32 | **74 / 1** / 50 |
| Fedora 44 | 6.19 | 68 / 3 | 68 / 3 / 32 | **72 / 2** / 51 (75 / 0 the run before) |
| Rocky 9 | 5.14 (RHEL) | 43 / 3 | 61 / 1 / 41 | **66 / 0** / 59 |
| Ubuntu 24.04 | 6.8 | 57 / 5 | 60 / 2 / 39 | **61 / 1** / 61 |
| Debian 13 | 6.12 | 52 / 4 | 55 / 0 / 46 | **57 / 0** / 66 |
| Debian 12 | 6.1 | 49 / 7 | 51 / 4 / 46 | **53 / 1** / 69 |

(An `opensnoop` run whose bpftrace lost events is counted as skipped, by the
rule the runner already applied to `perf trace`: a witness that lost events
is no count -- 80 and 115 lost in the third run.)

A skip says which witness the distribution lacks (`heaptrack`, `valgrind`,
`fio`, `nsys`, `ncu` are not packaged everywhere), which setting it holds
(`perf_event_paranoid` 4 on Ubuntu refuses turbostat), or that the witness
cannot attach on that kernel (Debian 12's and Ubuntu's `biosnoop.bt` and
`biolatency.bt` probe `blk_account_io_*`, which their kernels no longer
have). What still does not hold:

- **Debian 12's perf 6.1** `sched latency` counts 1 or 2 switches for the
  workload where it made 100 (Omniscient: 101, as `perf sched timehist` and
  `strace` count elsewhere): the witness.
- **Ubuntu 24.04**: `perf top`, which draws no screen in the guest.
- **Fedora 44** (third run only): `cpu.ipc`, 3.37 against perf's 3.69 on
  separate runs, held in the two runs before it; `cpu.context_switches.system`,
  328 over perf's 1715 where 257 were allowed -- the pair's band was a share
  only, and sudo's and perf's own switches are a count: it has an absolute
  floor now.
- **Arch** (guest): `cpu.cache_misses`, omn 17 % over perf on separate runs,
  where on the development machine the same pair reads omn under perf
  (`docs/open-findings.md`): a shared last-level cache counted by two
  separate runs.

## What Omniscient gave back to distroproof

Porting a demanding program found what distroproof needed:

| | Pull request |
|---|---|
| Three images: `gentoo` (the autobuilds' cloud-init image), `debian-sid` (the next kernel Debian ships), `fedora-previous` | [#4](https://github.com/devutils-org/distroproof/pull/4) |
| Gentoo boots under UEFI (under SeaBIOS the VM read 512 bytes of its disk and spun -- distroproof's metrics were the only signal) and its kernel is found at `/boot/kernel-<release>`, so later runs boot it directly | [#4](https://github.com/devutils-org/distroproof/pull/4) |
| Every overlay grows by 4 GiB: Alpine's root had 21 MiB free | [#4](https://github.com/devutils-org/distroproof/pull/4) |
| `file_caps`: file capabilities on an installed binary, where `caps` raised ambient ones every child inherits | [#5](https://github.com/devutils-org/distroproof/pull/5) |
| `collect`: files brought back from the guest, where a 1 MB document had to travel as standard output | [#5](https://github.com/devutils-org/distroproof/pull/5) |
| `DP_TARGET` and `DP_IMAGE` in the guest | [#5](https://github.com/devutils-org/distroproof/pull/5) |
| envprobe reports the function tracer and `RLIMIT_MEMLOCK`, the two causes it could not show (findings 4 and 6) | [#5](https://github.com/devutils-org/distroproof/pull/5) |

## The lesson

Ask the kernel, never its version. Three times a version was believed and
three times a distribution disproved it: the catalog's `kernel>=6.8` for
huge page statistics (Ubuntu's 6.8 has none), the judge's "uprobe-multi is
6.6" (Rocky 9's 5.14 has it), and the BTF that should name ftrace's functions
(5.10's leaves them out). Omniscient's probes already asked features for this
reason; distroproof is what showed where they still did not, on machines one
developer does not have.

## Open

- **Debian 12's `perf sched latency`** counts 1 or 2 switches for the
  workload over two seconds: the witness, not Omniscient (whose count agrees
  with `perf sched timehist` and `strace` elsewhere).
- **`cpu.cache_misses`** reads apart from perf on separate runs, in opposite
  directions here and in the Arch guest; **`cpu.ipc`** missed once on Fedora.
- **NixOS** is not covered. It publishes no image a NoCloud seed configures:
  Hydra's only cloud image is Amazon's, set up from EC2's metadata service,
  which distroproof would have to serve, and building one needs `nix`. On
  NixOS itself the glibc archive would not start (there is no
  `/lib64/ld-linux-x86-64.so.2`); the static musl one would.
- **aarch64 on hardware**: the arm64 cells are emulated, so their hardware
  counters are TCG's, not a core's.
