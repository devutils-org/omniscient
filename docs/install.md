# Installing omn

What a stock Ubuntu, Fedora or Arch machine needs to run `omn`, and how to
tell what it then serves. Every number here was measured, on the development
machine (Arch, Linux 7.2.6, AMD Zen 2) or in the two CI runs on a hosted
Ubuntu 24.04 runner (Linux 6.17.0-azure); a claim about a distribution neither
of them is, is marked **(read from its documentation)**.

In short:

```console
$ tar -xzf omn-0.1.0-x86_64-linux-gnu.tar.gz -C ~/.local --strip-components=1
$ sudo setcap cap_bpf,cap_perfmon+ep ~/.local/bin/omn
$ omn list | tail -n 3 | head -n 1
101 sources: 92 usable here, 0 unverified, 9 out of reach on this machine.
```

---

## Getting it

### From a release archive

A release is one archive and its checksum. The archive is a prefix: it holds
exactly these six files under one top directory, and nothing else
(`scripts/release.sh list` prints the list the archive is checked against):

```
bin/omn                    the tool
bin/omn-mcp                an MCP server over what omn writes (docs/mcp.md)
bin/libomn_preload.so      the preload shim the mem.heap.*, sched.mutex and gpu.api sources load
share/man/man1/omn.1       the manual page
share/doc/omn/README.md
share/doc/omn/install.md   this file
```

```console
$ sha256sum --check omn-0.1.0-x86_64-linux-gnu.tar.gz.sha256
omn-0.1.0-x86_64-linux-gnu.tar.gz: OK
$ tar -xzf omn-0.1.0-x86_64-linux-gnu.tar.gz -C ~/.local --strip-components=1
```

`/usr/local` works the same way, with `sudo`. Keep the three files of `bin/`
together: `omn` looks for the shim **beside its own executable**, and `omn-mcp`
looks for `omn` beside its own. A symbolic link to `omn` from elsewhere on the
`PATH` is fine, since the kernel resolves it and `omn` finds the directory it
really lives in, but the capabilities have to be granted on the real file:
`setcap` refuses a symbolic link (measured, libcap 2.78:
`Invalid file '...' for capability operation`).

The binaries need **glibc 2.34 or later**, which `scripts/release.sh floor`
reads off the symbols they import, and fails on when a binary needs a newer
one: `verify` runs it on every archive before it is published. That is Ubuntu 22.04 and later, Debian 12,
Fedora 35 and later, RHEL 9 and Arch **(read from their documentation)**. They
need nothing else at run time: no libbpf, no clang, no kernel headers. The eBPF
programs are assembled by the tool itself.

### From source

Rust 1.82 or later and `make`. A C compiler is only for two demonstration
fixtures, and its absence is announced, not fatal.

```console
$ make install PREFIX=~/.local
...
installed omn, omn-mcp and the preload shim in /home/you/.local/bin.
no capability was granted. Without them the eBPF and per-cpu perf backends
report what they need instead of collecting; to grant them:

    sudo setcap cap_bpf,cap_perfmon+ep /home/you/.local/bin/omn
```

It builds with `--release --workspace --locked` (the shim is a workspace
member nothing depends on, so a plain `cargo build` would not build it) and
installs the archive's layout.

A prefix you cannot write, such as `/usr/local`, is two steps, the build as
yourself and the copy as root:

```console
$ make release
$ sudo make install PREFIX=/usr/local
the release build in target/release is newer than every source cargo recorded for it
...
```

Run as root, `make install` **does not build**: cargo under `sudo` is usually
not on root's `PATH` with rustup, and when it is, it leaves root-owned files in
`target/` that break your next `make`. It copies `target/release` instead,
after checking that each binary is newer than every source cargo recorded for
it in the `.d` file beside it, and refuses otherwise, naming the file that is
newer:

```console
$ touch shim/src/mutex.rs
$ sudo make install PREFIX=/usr/local
libomn_preload.so is older than shim/src/mutex.rs; run `make release` as yourself first
```

`DESTDIR` is honoured for packaging; the command it prints names the final
path, which is where a package's post-install step has to grant the
capabilities. `make uninstall PREFIX=...` removes what it installed.

It **grants no capability**, on purpose: that is a decision about your machine,
and not one `make install` can take for you.

---

## The two capabilities, and why

`omn` reads part of what it serves from files anyone can read (`/proc`, `/sys`,
cgroups) and from `perf_event_open` on the program you launched: 51 of the 92
sources the development machine serves need nothing more. The rest come from
eBPF programs it loads into the kernel, and loading one needs two
capabilities. The tool asks for exactly those:

| Capability | What it opens |
|---|---|
| `CAP_BPF` | `bpf()` itself: loading a program, creating its maps. With `kernel.unprivileged_bpf_disabled` at 1 or 2, as it is here (2, measured) and on every distribution named here **(read from their documentation)**, no program loads without it. |
| `CAP_PERFMON` | attaching a tracing program, and `perf_event_open` past what `perf_event_paranoid` allows an ordinary user: a whole cpu, the kernel's side of a stack, any counter at paranoid 3 and 4. |

Root has both, so `sudo omn ...` works too; the capabilities are the narrower
way of saying the same thing (`DECISIONS.md`, "Privileges"). They do not
include `CAP_SYS_ADMIN`, so a few paths stay out of reach and say so, among
them tracefs, which is root-only on every machine measured here.

`net.capture` alone needs a third, `CAP_NET_RAW`, which is never granted by
default: it also lets the binary send arbitrary frames, which is not
observing. Add it only if you want that source:

```console
$ sudo setcap cap_bpf,cap_perfmon,cap_net_raw+ep ~/.local/bin/omn
```

**An unprivileged run is refused, never degraded.** A source whose backends
all need what the binary does not have is refused with the capability it
needed and the command that grants it; it is not resolved onto a backend that
answers a different question. So a machine without the capabilities is not a
broken install, it is a smaller one, and `omn list` says how much smaller.

### Three ways to lose them without noticing

A file capability is an extended attribute of one file on one filesystem. It
does not survive the file being rewritten, and some filesystems ignore it.

- **Every reinstall drops it.** Measured: after `make install` over a binary
  that had it, `getcap` prints nothing. The same after `cp`, `cp -a`, and a
  `tar` round trip without `--xattrs`. Grant it again after every upgrade.
- **A filesystem mounted `nosuid` ignores it.** Measured on the development
  machine, whose `/tmp` is `nosuid`: a binary there with
  `cap_perfmon,cap_bpf=ep` serves 57 sources, the same as one with none, and
  tells you to run the `setcap` you already ran. Check with
  `findmnt -no OPTIONS -T "$(command -v omn)"`, and install on a filesystem
  without `nosuid` (`/usr/local`, and on most machines `~/.local`).
- **A container may not be able to hold it.** Docker and Podman leave
  `CAP_BPF` and `CAP_PERFMON` out of a container's bounding set by default, and
  a binary whose file capabilities cannot all be granted is refused at exec
  with `EPERM` **(read from `capabilities(7)` and the Docker documentation;
  not measured here)**. Run it on the host, or give the container
  `--cap-add BPF --cap-add PERFMON`.

---

## What `perf_event_paranoid` does

`/proc/sys/kernel/perf_event_paranoid` decides what an unprivileged process may
ask `perf_event_open` for, and so decides the counting and sampling sources of
a binary **without** `CAP_PERFMON`. With it, the setting no longer limits
`omn`.

| Value | Without `CAP_PERFMON` | Who ships it |
|---|---|---|
| -1 | everything, every cpu | nobody by default; the development machine is set there |
| 0 | a process's own counters, and whole cpus | |
| 1 | a process's own counters, user and kernel | |
| 2 | a process's own counters, user space only | the upstream kernel's default; Fedora and Arch keep it **(read from their documentation)** |
| 3, 4 | no `perf_event_open` at all for an ordinary user | a Debian and Ubuntu kernel patch; the Ubuntu runner is at 4, measured |

On the same Ubuntu 24.04 runner, without capabilities, the two CI runs differ
in this value and in two settings only the test suite uses (`omn list`,
measured):

| `perf_event_paranoid` | Usable | What moved |
|---|---|---|
| 4 (as shipped) | 32 of 101 | |
| 2 | 38 of 101 | `cpu.cycles`, `cpu.context_switches`, `cpu.migrations`, `mem.faults` and `sched.switches` become usable on a launched program, and `power.temp` |

A cloud VM often has no hardware PMU at all, whatever the setting. The runner
at paranoid 2 reported `cpu.cycles` usable on a VM with no cycle counter;
that is a known defect of the probe, being fixed this wave
(`docs/portability.md`).

To lower it until the next boot, or for good:

```console
$ sudo sysctl kernel.perf_event_paranoid=2
$ echo kernel.perf_event_paranoid=2 | sudo tee /etc/sysctl.d/60-perf.conf
```

You do not need to if you grant `CAP_PERFMON`: lowering the setting widens
what every program on the machine may do, where the capability widens one
binary.

---

## The kernel

**There is no single kernel floor: each backend states the kernel it needs**
(`DECISIONS.md`, "Kernel floor", amended in the fifteenth wave), and the
resolver probes it like any other requirement. Below a backend's version that
backend is refused by name with the version
(`kernel>=6.1: missing: kernel 5.15 is older than 6.1`), and the chain moves to
the next backend that serves, never quietly. In the catalog today:

| Requirement | Backends | Why that version |
|---|---|---|
| `kernel>=5.8` | 33 eBPF backends | the BPF ring buffer they publish through, and `CAP_BPF` / `CAP_PERFMON` |
| `kernel>=6.1` | 5: the `bpf:raw_tp` backends of `sched.runqlat`, `sched.offcpu` and `sched.switches`, and both tracepoint backends of `sched.lock` | the four-argument `sched_switch`; `lock:contention_*` |
| `kernel>=6.8` | 1: `mem.thp`'s `fs:sys` | its `fs:proc` fallback serves older kernels |
| `uprobe_multi` (6.6) | the `bpf:usdt` backends of `net.request` | every site is linked with one uprobe-multi link |

Every one of the five 6.1 backends has a fallback below it in its chain (a
`bpf:tp` program, or `sched.lock`'s `bpf:kprobe`), so on an older kernel the
source is served by that fallback, with what it degrades stated.

**5.15 is served.** Under 5.15, 6.1, 6.6 and 7.2 with the capabilities,
`omn list` calls 86, 88, 88 and 89 sources usable (measured in guests,
`docs/portability.md`). No kernel between 5.15 and 6.1 has been measured, so
whether the 6.1 backends would serve a 5.18 is not known; they keep 6.1 until
a cell says otherwise. Below 5.8 nothing has been measured; the 33 backends
that say `kernel>=5.8` are refused there by name.

It also wants the kernel's own type information, `/sys/kernel/btf/vmlinux`
(`CONFIG_DEBUG_INFO_BTF`). Without it the eBPF sources that read kernel
structures are refused, and a run attaches 53 sessions instead of 100 under
7.2 (measured in guests, `docs/portability.md`).

| | Kernel | Every backend's `kernel>=` met | BTF |
|---|---|---|---|
| Ubuntu 24.04 | 6.8, later with HWE **(read from its documentation)**; the hosted runner is 6.17, measured | yes | yes, measured on the runner |
| Ubuntu 22.04 | 5.15; 6.8 with the HWE kernel **(read from its documentation)** | with HWE; on 5.15 the six `kernel>=6.1`/`6.8` backends and `uprobe_multi` are refused and their fallbacks serve, measured in a 5.15.221 guest | yes **(read from its documentation)** |
| Fedora 40 and later | 6.8 and later **(read from its documentation)** | yes | yes **(read from its documentation)** |
| Arch | the current stable kernel, 7.2.6 here, measured | yes | yes, measured |

---

## What does this machine serve?

The one-line checks, in the order worth running them:

```console
$ omn list | tail -n 3 | head -n 1             # the whole answer, in one line
101 sources: 92 usable here, 0 unverified, 9 out of reach on this machine.

$ getcap "$(command -v omn)"                    # the capabilities, if any
/home/you/.local/bin/omn cap_perfmon,cap_bpf=ep

$ cat /proc/sys/kernel/perf_event_paranoid
$ findmnt -no OPTIONS -T "$(command -v omn)"    # nosuid here means no capabilities
$ omn list cpu                                  # one family: HERE says yes, no or ?
$ omn list sched.offcpu                         # one source: every backend and what it lacks
```

What the first line said, measured:

| Machine | Usable of 101 |
|---|---|
| development machine, capabilities granted, in `target/` (37f17c8, 2026-10-02) | 92 |
| development machine, none (a copy of the same binary, 37f17c8) | 51 |
| Ubuntu 24.04 hosted runner, none, paranoid 2 | 38 |
| Ubuntu 24.04 hosted runner, none, paranoid 4 as shipped | 32 |

`?` in the HERE column is not a soft yes: no probe covers that requirement, so
nothing was checked, and the resolver will not choose it while a verified
backend exists. `omn list <source>` names each requirement, why it is missing,
and the command that grants it.

A first real result, needing no capability:

```console
$ omn mem.rss --mode poll@100ms --emit summary -- python3 -c "import time; b = b'x' * (64 << 20); time.sleep(1)"
```

`GUIDE.md` in the source tree has more than a hundred sessions of what to ask
next.

---

## Checking a release

`make install-check` installs into a scratch prefix and runs the installed
`omn` with the checkout's `target/` renamed out of the way, from an empty
directory and with `OMN_PRELOAD_LIB` unset, so nothing it finds can come from
the build tree. `scripts/release.sh verify ARCHIVE` does the same to an
extracted archive, after checking its checksum and that it holds exactly the
six paths above, that no binary in it needs a glibc newer than 2.34, and that
the README and this file it carries still say 2.34. Each check compares against something the workload chose,
never against what the tool computed another way:

| Check | Against |
|---|---|
| `omn --version` | the version in `Cargo.toml` |
| `omn list` | as many sources as `catalog/*.yaml` declares, with no catalog near the binary: it is compiled in |
| `mem.rss` on a launched program | a program that writes 64 MiB and holds it peaks at 64 MiB or more; the same program without the write peaks below |
| `mem.heap.alloc` through the installed shim | a program making one allocation of 64 MiB, then one of 32 MiB: the largest the shim saw is that size, plus at most 64 KiB of the interpreter's header |
| the shim's path | with the shim moved away, the refusal names `PREFIX/bin/libomn_preload.so` |
| `omn-mcp` | its capability sweep is answered by `PREFIX/bin/omn` |

A number a check prints carries the run's losses beside it when the run had
any, on a pass as on a failure. A tree without its shim fails three of them,
measured, and one byte appended to the archive fails its checksum. Two builds of the same commit produce the
same archive byte for byte: it is written in the listed order, owned by uid 0,
and dated by the commit. The checks need `python3` for their workloads and
fail, rather than skip, without it. `.github/workflows/release.yml` runs both
on a `v*` tag, then attaches the archive and its checksum to the release.

---

## For the coordinator

Stream D's handoff for the fourteenth wave.

### Landed

1. **A release.** `scripts/release.sh` builds
   `target/dist/omn-<version>-<arch>-linux-gnu.tar.gz` and its `.sha256`, with
   `list`, `name`, `floor`, `verify ARCHIVE` and `check PREFIX` beside it.
   `make install PREFIX=...` installs the same layout and prints the `setcap`
   line for the installed path without running it; `make uninstall`,
   `make install-check`, `make dist` and `make dist-verify` are new.
   `.github/workflows/release.yml` builds, verifies and publishes on a `v*`
   tag, on `ubuntu-22.04` for the lowest glibc floor, and refuses a tag that
   does not match `Cargo.toml`. `make`, `make check` and the capability step
   are untouched.
   **Ground truth**: `make install-check` and `make dist-verify`, both green
   here: 101 sources listed against 101 entries in `catalog/*.yaml`; `mem.rss`
   peaks at 73 MiB with 64 MiB written and 9 MiB without; `mem.heap.alloc`
   sees 67108865 B for a 64 MiB `bytearray` and 33554433 B for 32 MiB; the
   refusal without the shim names the installed path; `omn-mcp` names the
   installed `omn`. Negative controls: a tree without its shim fails three
   checks; one appended byte fails the checksum; two builds of the same tree
   give the same sha256.
   Measured on an installed tree with the capabilities granted, on `/home`:
   93 usable, the same as `target/debug/omn`; `sched.offcpu` attaches; and
   `mem.heap.alloc` still reads the shim.
2. **`docs/install.md`**: this file. The kernel floor and BTF, the two
   capabilities and three ways to lose them (a reinstall, `nosuid`, a
   container), `perf_event_paranoid` with the CI's two runs as its measured
   table, the one-line checks, and the release check.
3. **`README.md` and `DEMO.md` for a stranger.** The README's first screen is
   what the tool is, how to install it, and one command to try; the long
   material is below it. `DEMO.md` gains an install paragraph at its top; its
   outputs are left for the coordinator to regenerate.

4. **Three findings of the adversarial review, fixed.**
   - *The glibc floor is enforced, not printed.* `GLIBC_FLOOR=2.34` in
     `scripts/release.sh` is the one value; `floor [FILE...]` fails on a
     binary needing more and names the symbols that do, `build` fails with
     it, and `verify` runs it on the extracted binaries and fails when the
     archive's own `README.md` or `install.md` no longer says "glibc 2.34 or
     later". No objdump is a failure (an unknown floor is not a pass).
     `release.yml`'s verify step therefore fails on a runner or dependency
     that raises the floor. **Ground truth**: a C program calling `strlcpy`
     (a `GLIBC_2.38` symbol, built here) fails with `needs 2.38, above the
     documented 2.34` and names `strlcpy (GLIBC_2.38)`; a C `main` returning
     0 and the three release binaries pass at 2.34.
   - *`sudo make install` no longer runs cargo as root.* As root, `install`
     only copies, after `release.sh built` checks each artifact against the
     sources cargo recorded in its own dep-info file (`target/release/*.d`);
     as a user it builds as before, so `make install-check` is unchanged.
     The dep-info list and not a directory list, because a directory list
     names files that are not compiled and would refuse forever: touching
     `src/triage/workload.c` does not make `make release` relink. **Ground
     truth**, as uid 0 in a user namespace (`unshare -r`, `PATH` without
     cargo): the install copies without running cargo, so it writes nothing
     in `target/` (ownership itself cannot be seen from a user namespace,
     which maps uid 0 back to the user); after `touch shim/src/mutex.rs` it refuses with
     `libomn_preload.so is older than shim/src/mutex.rs`; after `make
     release` it copies again; `touch src/triage/workload.c` does not refuse.
     Not measured: a real `sudo` (no password-less sudo here).
   - *Losses show on a pass.* Every `ok` line of checks 3 and 4 prints the
     run's losses beside the number when there are any. Verified on the
     helper's output (`123` gives nothing, `123 (the run lost {...})` gives
     the suffix); a lossy run could not be made to order here.

**`docs/open-requests.md`, the row for 2257** (README says formats are "not
written"): `README.md`'s output-formats paragraph now says every format has
its encoder and no longer describes one "whose encoder has not landed".
`DECISIONS.md:20` is the coordinator's.

### How it was tested

`bash -n scripts/release.sh`, `make install-check`, `make dist-verify` (after the review fixes too), the
negative controls above, and one `make check` after merging `main` (its result
is in the stream's summary). No Rust source changed, so no module test applies.

### Index rows

None: this stream added no session.

### For `DECISIONS.md`

- **The installed layout is flat: `bin/omn`, `bin/omn-mcp`, `bin/libomn_preload.so`.**
  `omn` finds the shim beside its own executable and `omn-mcp` finds `omn`
  beside its own. A `lib/omn/` directory with symbolic links in `bin/` would
  have put the capability on a file the user does not name, and `setcap`
  refuses a symbolic link (measured). A shared library in `bin/` is unusual
  and harmless; if the project wants `lib/omn/`, it is one more directory in
  `find_shim` (filed for R) and one line of `release.sh`.
- **Installing grants nothing.** `make install` and the archive print the
  `setcap` command for the installed path and leave it to the user; the
  capability step of `make` stays a build-tree convenience. A grant is a
  decision about the machine, a `DESTDIR` build cannot make it on the final
  path, and tar does not carry it anyway.
- **The release is built on the oldest hosted Ubuntu**, because the glibc
  floor is the newest symbol linked against, and `release.sh verify` fails
  above the documented 2.34 rather than printing it. Raising the floor is a
  decision: change `GLIBC_FLOOR` and the two documents together, or `verify`
  refuses.
- **As root, `make install` copies and never builds.** A system-wide install
  is `make release && sudo make install`. The alternative, building as
  `$SUDO_USER` from inside the root recipe, depends on that user's `PATH`
  surviving sudo's `secure_path`, which is exactly what fails with rustup.

### Filed for another stream

Under "Needs from another stream", "Stream D, the fourteenth wave", in
`docs/open-findings.md`:

Each re-read against the code at 37f17c8 (`docs/open-requests.md`):

- **P** (`src/capabilities.rs`): a binary on a `nosuid` mount carries its file
  capabilities and does not get them. *Partly landed*: the probe knows
  (`capabilities::withheld_file_capabilities` names the `nosuid` mount or
  `no_new_privs`, and the resolved record carries it); the attach's advice in
  `src/bpf/mod.rs` still tells the user to run the `setcap` they already ran.
- **R** (`shim/**`, `src/collect/mem/heap/mod.rs`): `aligned_alloc` and
  `memalign` were not hooked, so `dd bs=64M` showed a largest allocation of
  4 KiB and no loss. *Landed* in the fifteenth wave: the shim hooks
  `aligned_alloc`, `memalign`, `valloc` and `pvalloc`, and `find_shim` also
  looks in `<exe>/../lib/omn/`.
- **R** (`src/collect/mod.rs`, the refusal): with the shim missing,
  `mem.heap.alloc` was refused with "no collector is written for ...
  `bpf:usdt`" as its first line. *Landed*: the refusal leads with the first
  collected backend passed over and its reason (`src/main.rs`, `PassedOver`).
- **Coordinator**: the repository had no `LICENSE` file. *Landed* after the
  fifteenth wave: `LICENSE-MIT` and `LICENSE-APACHE`, `MIT OR Apache-2.0`, with
  `src/bpf/` under `MIT OR GPL-2.0`; `release.sh` and `make install` put both
  files under `share/doc/omn/`.

### Matrix cells

None should move: nothing here changes what a probe answers. A release is
worth one sweep **from an installed tree** rather than from `target/`, once,
by the coordinator, and the expected answer is the build tree's:

```console
$ make release && sudo make install PREFIX=/usr/local && sudo setcap cap_bpf,cap_perfmon+ep /usr/local/bin/omn
$ /usr/local/bin/omn list | tail -n 3 | head -n 1     # 92 here, as target/debug/omn
```

The first stock-kernel cell for a release is `release.yml`'s run on
`ubuntu-22.04` at the next tag: its `verify` step is the check above, on a
distribution kernel and without capabilities.
