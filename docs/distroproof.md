# Omniscient on every distribution, with distroproof

Omniscient is a kernel-facing program: eBPF assembled by hand, perf events,
BTF, file capabilities, a preload shim injected into other programs. Every one
of those depends on the distribution it runs on -- its kernel, configured and
patched by its vendor, its libc, its defaults. This document is how its
portability is worked on with [distroproof](https://github.com/devutils-org/distroproof),
which boots each distribution's official cloud image on **its own kernel** and
runs a command in it, and what that found. It is meant to be read as
distroproof's living proof: a real program ported with it, every finding with
the environment difference distroproof showed and the commit that fixed it.

The plan is `ROADMAP.md`, phase 13; the decisions are `DECISIONS.md`, "Settled
after the twenty-fifth wave: the distribution matrix".

## Running it

```console
scripts/release.sh                                   # the archive, linked against glibc 2.17
mkdir dp && tar -C dp -xzf target/dist/omn-*.tar.gz  # what a user extracts
cd dp && distroproof run --target rocky-8 --workdir . -- sh smoke.sh
```

`smoke.sh` is whatever is being checked; the first run's is in the finding
below. The matrix is `distroproof.toml`, at the repository's root:

```console
scripts/release.sh                       # the archives, glibc 2.17 and musl
scripts/distroproof/fixtures.sh          # the USDT fixtures, static
distroproof up --no-tui --jobs 4 --json target/matrix/distroproof.json
scripts/distroproof/cells.py target/matrix/distroproof.json
cargo test --test portability            # the judges, over every cell
```

Each guest gets the archives, the fixtures and two scripts, never the
checkout. `setup` installs the archive a user would (musl where there is no
glibc loader) and grants the file capabilities on it -- or says that a kernel
before 5.8 has none to grant -- and the command is `omn selftest --format
json`, every source listed and run for 300 ms on a launched program and on
the machine, as user `test`. `cells.py` writes each guest's document as a
cell, with distroproof's `envprobe` report of the same guest beside it. 21
targets, four at a time, take about twelve minutes.

## The distributions

| Image | glibc | Kernel | The release starts | Sources usable (no capability) |
|---|---|---|---|---|
| `rocky-8` | 2.28 | 4.18.0-553.el8_10 | yes | 62 of 123 |
| `ubuntu-18.04` | 2.27 | 4.15.0-212 (no BTF) | yes | 52 of 123 |
| `fedora-previous` (Fedora 43) | 2.42 | 6.17.1-300.fc43 | yes | 98 of 123 (root) |
| `debian-sid` (forky/sid) | 2.43 | 7.2.11+deb14 (`perf_event_paranoid` 3) | yes | 98 of 123 (root) |
| `gentoo` (OpenRC, dist-kernel) | 2.43 | 6.18.54 | yes | 98 of 123 (root) |
| `alpine` (glibc archive) | musl 1.2.6 | 6.18.35-virt | **no**: `./omn/bin/omn: not found`; yes with `gcompat` | 91 of 123 (root) |
| `alpine` (musl archive) | musl 1.2.6 | 6.18.35-virt | yes | 93 of 123 (root) |
| Arch (the development host) | 2.44 | 7.2.9 | yes | 67 of 123 |

Every other image distroproof boots (Debian 10 to 13, Ubuntu 20.04 to 24.04,
Rocky 9, Fedora) has a glibc between 2.28 and the host's, so the same archive
starts there; that is the glibc floor's argument, and the matrix will show it
image by image.

### The matrix, 2026-10-19

| Target | Kernel | Configuration | Listed usable (of 123) |
|---|---|---|---|
| `debian-10` | 4.19.0-27 | nocaps (no CAP_BPF before 5.8) | 36 |
| `debian-11` | 5.10.0-46 | caps | 41 |
| `debian-12` | 6.1.0-54 | caps | 76 |
| `debian-13` | 6.12.113 | caps | 82 |
| `debian-sid` | 7.2.11 | caps | 82 |
| `ubuntu-18.04` | 4.15.0-212 | nocaps | 36 |
| `ubuntu-20.04` | 5.4.0-216 | nocaps | 39 |
| `ubuntu-22.04` | 5.15.0-198 | caps | 85 |
| `ubuntu-24.04` | 6.8.0-147 | caps | 93 |
| `rocky-8` | 4.18.0-553 (RHEL) | caps | 59 |
| `rocky-9` | 5.14.0-687 (RHEL) | caps | 86 |
| `fedora-previous` | 6.17.1 | caps | 96 |
| `fedora-latest` | 6.19.10 | caps | 96 |
| `arch` | 7.2.9 | caps | 96 |
| `gentoo` | 6.18.54 | caps | 96 |
| `alpine` (musl archive) | 6.18.35-virt | caps | 85 |
| `ubuntu-24.04-paranoid3` | 6.8.0-147 | `perf_event_paranoid` 3 | 93 |
| `debian-12-kptr2` | 6.1.0-54 | `kptr_restrict` 2 | 76 |
| `fedora-latest-lockdown` | 6.19.10 | `lockdown=confidentiality` | 55 |
| `debian-12-nopmu` | 6.1.0-54 | no PMU | 76 |
| `debian-12-root` | 6.1.0-54 | root | 94 |

Every cell holds the matrix's invariants: what is listed usable attaches,
and every refusal names its reason (`tests/portability.rs`, 26 judges).

## Findings

### 1. The release did not start on half the images (fixed)

**What distroproof showed.** Before any VM: the archive's binaries imported
`GLIBC_2.39` symbols (`scripts/release.sh floor`), because a binary needs the
newest glibc it was linked against and the build machine runs 2.44; the
documented floor was 2.34. distroproof's images put numbers on it: `rocky-8`
and `debian-10` ship 2.28, `ubuntu-18.04` 2.27, `debian-11` and
`ubuntu-20.04` 2.31 -- five of the thirteen images, and every RHEL 8 machine.

**The fix** (`scripts/release.sh`, `src/collect/mem/heap/mod.rs`). The
archive is linked against glibc 2.17 through zig (`cargo zigbuild --target
x86_64-unknown-linux-gnu.2.17`), whatever the build machine runs. One call
needed changing: `memfd_create`, whose glibc wrapper appeared in 2.27, is
made as the system call. The symbols glibc added later that the standard
library uses (`gettid`, `statx`, `getrandom`, `copy_file_range`) are weak
references it replaces with the system call where they are absent. `verify`
holds the floor at 2.17 and the documents to it.

**Checked with distroproof.** `rocky-8` (glibc 2.28, RHEL's 4.18) and
`ubuntu-18.04` (glibc 2.27, 4.15 without BTF): the archive starts, lists the
catalog with each refusal's reason, polls `cpu.util` ten times in a second,
and the preload shim reads the allocations of every process a shell starts.

**Then musl (fixed the same day).** On `alpine` the glibc binary was "not
found", the shell's word for a missing program interpreter
(`/lib64/ld-linux-x86-64.so.2`). Both answers were measured on Alpine 3.24,
and both are kept:

- **The glibc archive with `gcompat`** (`apk add gcompat`): it starts, 91
  sources usable as root, `cpu.util` 10 polls in 10, and the glibc shim
  injected into musl programs reads them -- 67.6 MiB in `python3`, busybox
  `sh`'s allocations. A fallback that costs nothing to build.
- **A second archive, `-linux-musl`**: `omn` and `omn-mcp` linked statically
  (`cargo zigbuild --target x86_64-unknown-linux-musl`), and the shim as a
  dynamic library against zig's musl (`-C target-feature=-crt-static`; built
  with the host's linker it depended on glibc's `libc.so.6`). Without
  `gcompat`: 93 sources usable, `sched.runqlat` attaches over `bpf:raw_tp`,
  and the musl shim reads 67.6 MiB in `python3`. A static binary loads no
  library into its process, so the NVIDIA sources are refused -- and the
  refusal now says the build is the reason, where the loader's "Dynamic
  loading not supported" read as the machine's lack. Two lines of the code
  needed changing for musl: `pthread_t` is a pointer there, and `time_t` is
  named through its field.

`verify` checks the musl archive's linking (no program interpreter, the shim
needing musl's `libc.so` alone) and leaves running it to an Alpine guest,
since on a glibc host its shim would be injected into glibc programs.

### 2. Gentoo, and two images more, added to distroproof (fixed in distroproof)

**Why.** The distributions the phase names include Gentoo, which distroproof
did not boot, and a rolling Debian and a second Fedora make the matrix's
two ends -- the next kernel a distribution ships, and the release a user
still runs. Each was added to distroproof's catalog
([devutils-org/distroproof#4](https://github.com/devutils-org/distroproof/pull/4)),
not worked around here (`DECISIONS.md`).

**What distroproof showed, and what was fixed there.**

- `gentoo` (the autobuilds' `di-amd64-cloudinit` image): the VM read **512
  bytes** of its disk in four minutes at 100 % cpu -- distroproof's metrics,
  the only signal, since nothing reached the serial console. The image is a
  GPT disk with no BIOS boot code; it boots under UEFI.
- Then `guest: kernel not found in /boot for 6.18.54`: distroproof extracts
  the kernel on the first boot so that later runs boot it directly (and may
  change its command line, which the `lockdown` variant needs). Gentoo's
  `installkernel` writes `/boot/kernel-<release>`, and its
  `modules/<release>/vmlinuz` is a link into a kernel source tree the image
  does not ship. The agent looks for `/boot/kernel-<release>` now; direct
  boot takes 13 s, against 18 s from the disk.
- `debian-sid` and `fedora-previous` booted at once.

**Checked.** On all three the glibc archive starts, lists the catalog,
polls `cpu.util` ten times a second, attaches `sched.runqlat` over
`bpf:raw_tp`, and the shim reads the allocations of what a shell starts.
`debian-sid` is the first image whose `perf_event_paranoid` is 3, Debian's
own patch, and the first on a 7.2 kernel other than the development host's.

### 3. The first matrix: 158 claims the kernel then refused, five defects (fixed)

The first run over the 21 targets found 158 runs of a source `omn list`
called usable that the kernel refused at start -- the one answer worse than
a refusal. Sorted, they were six classes; five were the tool's, each a probe
that asked a narrower question than the attach:

| Class | Where | What distroproof showed | Fix |
|---|---|---|---|
| Maps charged to `RLIMIT_MEMLOCK` | `debian-11` (5.10), ~45 runs | the user's limit is 64 KiB, soft and hard; before memory-cgroup accounting (5.11) BPF maps are charged to it, and the kernel's EPERM read as "needs CAP_BPF", which the binary held | the `bpf` probe creates a 1 MiB map and says the limit and its remedies where it is refused (`maps_charged_to_memlock`); `map_failure` says the same |
| Lockdown withholds `bpf_probe_read_kernel` | `fedora-latest-lockdown`, ~45 runs | every program reading a struct rejected by the verifier | the `bpf` probe loads the smallest program calling the helper (`kernel_reads_locked_down`) |
| No ftrace | `alpine` (`virt` kernel), 16 runs | fentry programs loaded and every attach refused with EBUSY; the kernel's config has no `CONFIG_FUNCTION_TRACER` | an fentry requirement is missing where the kernel's BTF lacks `register_ftrace_function` |
| `folio._folio_nr_pages` | `debian-13` (6.12), 4 runs | the member is `_folio_nr_pages` on 6.8 and 6.12, `_nr_pages` from 6.17 | the page-cache programs read either |
| Per-size THP statistics | `ubuntu-24.04` (6.8), 2 runs | `hugepages-2048kB/stats` absent where the catalog promised them from `kernel>=6.8` | the backend asks for the directory, not the version; the resolver falls back to `fs:proc` |

Two more were found before this run and fixed with it: `perf_event_paranoid`
3 (`debian-12`) refuses every perf event to a process without CAP_SYS_ADMIN,
CAP_PERFMON included -- Debian's and Ubuntu's patch -- and the `kprobe`
requirement now opens an event to see it, every refusal at that level saying
so; and the guests' roots were too small for the payload (`alpine`), which
distroproof now grows (its pull request #4).

The sixth class was the matrix's own: the program-probe sources had no
fixture in the guests. The fixtures are built statically with zig now
(`scripts/distroproof/fixtures.sh`) and carried with the archives.

**And two judges were wrong.** The uprobe-multi judge required a fallback
below 6.6, and `rocky-9`'s 5.14 attaches through the multi link without one:
RHEL backported it. The tool asks the kernel; the judge asked the version,
and now does not. The gaps judge, which explains every difference between two
cells by their resolved records, was written for cells of one userspace;
between distributions the differences are ftrace, cgroup v2, PSI -- facts
envprobe's report carries -- and it now leaves distroproof's cells to the
envprobe judge (task 4).

**After.** 21 of 21 targets, every cell holding the invariants.

