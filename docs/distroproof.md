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
| Distributions | **16** images on their own kernels -- Debian 10, 11, 12, 13 and sid; Ubuntu 18.04, 20.04, 22.04, 24.04; Rocky 8 and 9; Fedora 43 and 44; Arch; Gentoo; Alpine -- and **5** variants (paranoid 3, `kptr_restrict` 2, lockdown, no PMU, root) |
| Kernels | 4.15 to 7.2, RHEL's 4.18 and 5.14 with their backports |
| Time | the 21 targets in about **12 minutes**, four at a time, from one command |
| Findings in Omniscient | **16 defects** no test on the development machine had shown: **9 in the tool** (the release, the probes, the catalog, a collector) and **7 in its test suite** (a judge, six reference pairs) |
| Claims refuted | **158** runs of a source Omniscient called usable that the kernel refused at start, in the first matrix; **0** after |
| Cross-checked | every reason Omniscient gives for a refusal agrees with envprobe's independent reading of the same guest, on all 21 targets |
| Back into distroproof | 3 images (Gentoo, Debian sid, Fedora 43), 2 boot fixes, 3 `[run]` options, 2 envprobe facts (pull requests [#4](https://github.com/devutils-org/distroproof/pull/4), [#5](https://github.com/devutils-org/distroproof/pull/5)) |

## Reproducing it

```console
scripts/release.sh                       # the archives, glibc 2.17 and musl
scripts/distroproof/fixtures.sh          # the USDT fixtures, static
distroproof up --no-tui --jobs 4 --json target/matrix/distroproof.json
scripts/distroproof/cells.py target/matrix/distroproof.json
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
| `rocky-9` | 5.14 (RHEL) | file capabilities | 86 |
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

The spread is the point: the same binary serves 36 sources on Ubuntu 18.04
and 96 on Fedora, and says, source by source, why.

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
witnesses each distribution packages. Two runs: before and after the pairs'
own portability fixes (finding 10).

| Distribution | Kernel | First run: held / did not hold | After: held / did not hold / skipped |
|---|---|---|---|
| Arch | 7.2.9 | 69 / 2 | **71 / 0** / 32 |
| Fedora 44 | 6.19 | 68 / 3 | **68 / 3** / 32 |
| Rocky 9 | 5.14 (RHEL) | 43 / 3 | **61 / 1** / 41 |
| Ubuntu 24.04 | 6.8 | 57 / 5 | **60 / 2** / 39 |
| Debian 13 | 6.12 | 52 / 4 | **55 / 0** / 46 |
| Debian 12 | 6.1 | 49 / 7 | **51 / 4** / 46 |

(The two `opensnoop` runs whose bpftrace lost events -- 51 on Debian 13, 164
on Rocky 9 -- are counted as skipped, by the rule the runner already applied
to `perf trace`: a witness that lost events is no count.)

A skip says which witness the distribution lacks (`heaptrack`, `valgrind`,
`fio`, `nsys`, `ncu` are not packaged everywhere) or which setting it holds
(`perf_event_paranoid` 4 on Ubuntu refuses turbostat). What still does not
hold is the witnesses' versions, not Omniscient's numbers:

- **Debian 12's bpftrace** prints `execsnoop`, `biosnoop` and `biolatency` in
  shapes the parsers, written against Arch's 0.27, do not read; its **perf
  6.1** `sched latency` counts 10 switches for the whole machine where the
  workload made 100 (Omniscient: 103, as `perf sched timehist` and `strace`
  count elsewhere).
- **Ubuntu 24.04**: `biosnoop`'s output, and `perf top`, which drew no
  screen in the guest.
- **Fedora 44**: `opensnoop` counts 10 % fewer opens than Omniscient with the
  same probes as Arch's (open, below); Fedora's `libcrypto` keeps its
  symbols, so `perf` names functions where the pair expects the stripped
  object; `task-clock` by 0.5 ms over its band.
- **Rocky 9**: `perf lock contention`'s pair received no output from
  Omniscient (open).

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

- **`opensnoop`** counts 10 % fewer opens than Omniscient on Fedora 44, with
  the same probes as Arch's where the pair holds; not explained.
- **`perf lock contention`** on Rocky 9: Omniscient's side of the pair wrote
  no document; not looked into yet.
- **Debian 12's `perf sched latency`** counts 10 switches for the whole
  machine over two seconds: the witness, not Omniscient (whose count agrees
  with `perf sched timehist` and `strace` elsewhere).
- Not covered yet: aarch64, containers, NixOS -- each waits on distroproof
  booting it.
