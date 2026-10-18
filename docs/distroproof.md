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
below. The matrix over every image (`distroproof.toml`) is task 3 of the
phase and not written yet.

## The distributions

| Image | glibc | Kernel | The release starts | Sources usable (no capability) |
|---|---|---|---|---|
| `rocky-8` | 2.28 | 4.18.0-553.el8_10 | yes | 62 of 123 |
| `ubuntu-18.04` | 2.27 | 4.15.0-212 (no BTF) | yes | 52 of 123 |
| `alpine` | musl 1.2.6 | 6.18.35-virt | **no**: `./omn/bin/omn: not found` | -- |
| Arch (the development host) | 2.44 | 7.2.9 | yes | 67 of 123 |

Every other image distroproof boots (Debian 10 to 13, Ubuntu 20.04 to 24.04,
Rocky 9, Fedora) has a glibc between 2.28 and the host's, so the same archive
starts there; that is the glibc floor's argument, and the matrix will show it
image by image.

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

**Left.** musl: on `alpine` the binary is "not found", the shell's word for a
missing program interpreter (`/lib64/ld-linux-x86-64.so.2`). A static musl
build cannot `dlopen` NVML or CUPTI, and the shim must match the libc of the
program it is injected into; whether Alpine is served by a second archive, by
`gcompat`, or named as unsupported is still to decide.
