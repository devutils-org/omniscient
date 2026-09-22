# `runtime` — the family, and USDT underneath it

Owned by stream U in the eighth wave. This is the deliverable, not a log about
one: what the family measures now, how each claim is checked, what it costs,
and what is left with the reason it was left.

## Where the family stands

| Source | Backend | State |
|---|---|---|
| `runtime.custom` | `bpf:usdt` | **collects**, trace and count; scenarios below |
| `runtime.gc` | `bpf:usdt` | **collects**; checked against a fixture in CPython's probe shape, and refused by name against the real python3 here |
| `runtime.alloc` | `bpf:usdt` | **collects** trace; checked against a fixture in HotSpot's probe shape |
| `runtime.jit` | `fs:jit-map` | collects, since wave one |
| `runtime.jit` | `bpf:usdt` | not built: needs a string read (below) |
| `runtime.custom` | `bpf:uprobe` | not built: needs a symbol declaration, and CAP_SYS_ADMIN (below) |

```
$ omn list runtime
runtime.gc      trace         low    low     yes   all
runtime.jit     trace         free   low     yes   1 of 2
runtime.alloc   trace, poll   med    high    yes   all
runtime.custom  trace, count  med    low     yes   1 of 2
```

Three of the family's four sources had no collector at the start of this wave;
all four have one now, covering four of their six backends.

## What a probe costs, which is the first thing to know

**USDT is not the cheap path.** A probe site is a `nop`; attaching turns it
into a breakpoint, and every hit is a trap into the kernel and back. Finding 6
of `docs/open-findings.md` measured **740 ns** per event for a uprobe against
**132 ns** for the preload shim on the same event. Measured again here, with
the fixture's tick probe in a loop whose iteration costs 2 ns:

| | 2,000,000 hits | per hit |
|---|---|---|
| no probe attached | 4 ms | 2 ns |
| streamed to userspace (`trace`) | 1.45 s | ~700 ns |
| counted in the kernel (`count`) | 1.04 s | ~520 ns |

So a probe on a hot path makes a 4 ms program run for over a second, and almost
all of that is charged to the *target*: the trap runs in its context. The run's
own `cost:` line shows it -- the collector spent 1.6 ms of cpu against the
target's 1.04 s in the counted case.

What USDT buys is not speed. It is **reach** -- a program this tool did not
launch and cannot preload into, found by pid -- and **names**: the program says
which events it has, with typed arguments, instead of having them guessed from
symbols that move between builds. Every caveat of the three sources says the
price in those words, so the reader learns it at the moment it matters.

## How USDT works here

`src/bpf/usdt.rs` is the mechanism; `src/collect/runtime/usdt.rs` decides which
probes each source means; `src/collect/runtime/objects.rs` decides which files
to look in.

**Reading the notes.** Each `.note.stapsdt` entry gives a probe's provider and
name, its location as a virtual address, the address `.stapsdt.base` had at
link time, a semaphore address (or zero), and an argument string. The location
and semaphore are shifted by however far `.stapsdt.base` moved since (prelink),
then turned into file offsets through the `PT_LOAD` segment that maps them,
because the kernel attaches by file and offset. Only x86-64 ELF64 is read;
anything else is refused by ELF machine number, because argument descriptors
name one architecture's registers.

**Arguments.** `-4@%esi` is four bytes, signed, in `esi`; `8@-80(%rbx)` is eight
bytes of user memory at `rbx - 80`; `-4@$7` is the constant 7. Registers are
read from the `pt_regs` the probe hands the program, at offsets taken from the
kernel's BTF; memory with `bpf_probe_read_user`; each value is sign- or
zero-extended from its declared size. An operand this build cannot compute --
`counter(%rip)`, a symbol whose address only the loaded image knows, or an
indexed form -- is *carried as unread*, and so is a memory read that faulted:
the argument is absent from the event, never zero, and the run counts it by
probe and descriptor.

**Which files.** A running process says what it maps in `/proc/<pid>/maps`. A
launched program is attached before it execs, so what it *will* map is read
the way `ld.so` decides it: the executable (a `#!` script is read through its
interpreter), its `PT_INTERP`, and the closure of its `DT_NEEDED` entries
through its own `DT_RUNPATH`, `LD_LIBRARY_PATH`, `/etc/ld.so.conf` and the
defaults. A library it will `dlopen` later is named nowhere before it runs, and
the run's degradation line lists the objects it read so that absence is visible.
`objects::tests` checks the prediction against the libc this test process
actually has mapped.

**Selecting.** `runtime.custom` is the probe you name, and the catalog cannot
carry the name, so it travels in the filter: `--filter "provider == 'libc' &&
probe == 'setjmp'"`. The filter is evaluated three-valued against each site --
`provider`, `probe` and `object` known, every other field unknown -- and only
sites it could match are attached. That is the one placement under which a
filtered-out probe genuinely costs nothing, which is what the resolver claims
for every `bpf` backend. The rest of the predicate (`comm`, say) is evaluated
per hit, and the run says how many it removed. A custom request that names no
provider or probe is refused with exit 2 and the whole inventory of what the
target carries, which is what a reader needs to write the filter.

`runtime.gc` and `runtime.alloc` carry their own tables: the begin/end pairs of
CPython, node, HotSpot and ruby (two phases), and HotSpot's `object__alloc`,
the one allocation probe that publishes a size in bytes.

**Two shapes.** In `trace` mode each hit is a record in a ring. In `count` mode,
when nothing is left of the filter once the probes are chosen, each program
adds one to its site's slot in a per-cpu array and nothing crosses the ring --
`docs/bpf-layer.md`'s counting shape, for the same reason: a probe's rate is a
property of the workload. The flood scenario shows why it matters: streamed,
two million hits delivered 377,969 and counted 1,622,031 as dropped; counted,
exactly two million and nothing lost.

## The attach, and why it is not `attach::uprobe`

**The `uprobe` PMU needs CAP_SYS_ADMIN, and this project does not grant it.**
`attach::uprobe` opens a perf event on the `uprobe` PMU, and nothing had ever
called it. Called for the first time here, with `cap_bpf,cap_perfmon+ep`
effective:

```
uprobe PMU, this process      EACCES
uprobe PMU, another process   EACCES
uprobe PMU, cpu 0             EACCES
kprobe PMU, cpu 0             opens
```

`perf_uprobe_event_init` keeps `CAP_SYS_ADMIN` where its kprobe twin accepts
`CAP_PERFMON`: a uprobe writes a breakpoint into another program's text. So the
existing `bpf:uprobe` path cannot work under the project's privilege model, and
`capabilities.rs` reports the `uprobe` requirement **available** from the PMU's
type file while the open is refused -- finding 15's shape exactly, a probe that
answers a different question from the attach. Filed for stream T.

**A `BPF_TRACE_UPROBE_MULTI` link works with CAP_BPF and CAP_PERFMON** (Linux
6.6), and is better on three counts:

- it follows a **process**, every thread of it: the threads scenario has eight
  threads created after the run started, 500 hits each, all counted;
- it takes each site's semaphore as a **`ref_ctr_offset`**, so the kernel raises
  the counter in the target's image of the file -- including one mapped after
  the link exists. A launched program sees its guarded probe's semaphore raised
  from the first line of `main`; a running one sees it raised at attach, and the
  fixture reports both from inside;
- one hit is one run of the program, where a machine-wide perf scope would run
  it once per cpu's event.

The link and the `BPF_PROG_LOAD` with an `expected_attach_type` it needs live in
a fenced section of `src/bpf/usdt.rs`, marked as belonging in `attach.rs` and
`syscall.rs`, which are not this stream's files. The requests to move them, with
signatures, are in `docs/open-findings.md` under stream U. On a kernel that
refuses the link (6.1 to 6.5), the sites fall back to `attach::uprobe` and the
run says that route needs CAP_SYS_ADMIN and maintains no semaphore.

**The semaphore protocol, therefore, is the kernel's.** A first version raised
semaphores from userspace through `/proc/<pid>/mem`. That works for a running
process and cannot work for a launched one before it execs -- the address does
not exist yet -- so a guarded probe in a program's first milliseconds was lost.
The link removed that window and the userspace version was deleted.

## Ground truth

Every scenario is `tests/scenario_runtime.rs` and a session in `examples/`.
The workload is a C fixture built at test time against systemtap's own
`<sys/sdt.h>` -- the notes are written by the header every USDT program uses,
not by this project -- and it prints what it did: how many times it fired, with
which arguments, whether it saw its semaphore raised, how long it slept by its
own clock.

| Scenario | Session | Checked against |
|---|---|---|
| a named probe, 50 hits | `runtime-usdt-probes.yml` | 50 events; `arg0..3` equal the index, its negation as a signed short, the constant 7, and 1000+index read through a pointer; the guarded probe counted exactly as often as the fixture saw its semaphore raised, and raised before `main` |
| libc's own probes | `runtime-usdt-libc.yml` | 25 `setjmp` and 25 `longjmp` hits carry the fixture's `jmp_buf` address, every `longjmp` its value 3 -- plus glibc's own `setjmp` before `main`, on another buffer |
| GC pauses | `runtime-usdt-gc.yml` | each pause contains the fixture's own measurement of its 20 ms sleep and exceeds it by less than half; generation and collected count exact |
| allocations | `runtime-usdt-alloc.yml` | 100 objects, 80,800 bytes, min 16 and max 1600, exactly |
| every thread | `runtime-usdt-threads.yml` | 8 × 500 hits from 8 tids that are not the main thread |
| flood | `runtime-usdt-flood.yml` | streamed: taken + dropped = 2,000,000 whichever way the race went; counted: exactly 2,000,000 |
| a running process | (`--pid`, made by the test) | the fixture waits for its own semaphore, reports it raised, and 20 hits arrive |
| no probe named | (the fixture) | exit 2, with the inventory |
| a real runtime without probes | `runtime-usdt-absent.yml` | refused, naming `python:gc__start` and what python3's ELF contains; where python3 has its probes, five `gc.collect()` must be five generation-2 pauses |
| no capabilities | (a copy of the binary) | exit 1, naming CAP_BPF and `setcap` |

And the parser against an independent one: `bpf::usdt::tests` compares every
probe of this machine's libc with binutils' `readelf -n` -- provider, name,
location, semaphore and argument string, all 78 agree.

**What the fixture is not.** No runtime on this machine is built with its
probes: python3.14 and libpython carry no `.note.stapsdt` (28 and 30 sections,
none of them notes), node and ruby neither, and there is no JVM. So the GC and
allocation scenarios use a fixture that publishes CPython's and HotSpot's exact
probe shapes, which checks everything this build does with those probes and
nothing about whether a real CPython fires them where its source says. The
`absent` scenario is the real-runtime check, and on a machine whose python3 has
its probes (Fedora's) it measures instead of refusing -- neither branch is a
skip.

The target whose notes a machine can be relied on to have is **libc**: glibc's
build put 78 probe sites under 55 names in it here, and `ld.so` has 13 more.

## Decisions taken in the catalog

### The requirement is the uprobe and the program, not the probe

The three USDT backends required `usdt`, `usdt:gc` and `usdt:alloc`, which no
probe answers. The resolver treats an unknown requirement as not available --
correctly -- so `runtime.custom` resolved past `bpf:usdt` onto `bpf:uprobe`,
which has no collector and cannot attach here anyway. They now require
`uprobe` and `bpf`, which is what the machine must provide. Whether the target
carries the probe is a fact about the target, read from its ELF notes at attach
and refused there by name, with the inventory -- the same argument this family
made in wave one for `runtime.jit` requiring `/tmp` rather than the map.

`runtime.jit`'s `bpf:usdt` keeps `usdt:jit`, deliberately: it has no collector,
and an answered requirement would make the resolver pick it over the map
backend that does collect.

### The keys are `task`

`runtime.gc` and `runtime.custom` declared `req` and `runtime.alloc` declared
`mem`. No probe in the tables carries a propagated request identity, and
HotSpot's `object__alloc` carries a class name and a size but no address, so
neither key could be produced. An entry offering a key no collector emits is
finding D2 of the seventh wave again; they declare `task` only.

### `runtime.custom` names its probe in the filter

Wave one proposed a `probe:` field in the request model. The filter already
carries exactly the right information (`provider`, `probe`, `object`), the
resolver already credits a `bpf` backend with evaluating it in the kernel, and
selecting sites from it is what makes that claim true. No request-model change
was needed for USDT; a symbol uprobe still needs somewhere to name the symbol.

### `runtime.alloc` loses its `preload` backend

The brief suggested implementing `runtime.alloc` through the existing
`LD_PRELOAD` shim, and it would have produced a source that duplicates
`mem.heap.alloc` under a second name. The shim sees `malloc`; a managed runtime
allocates objects out of a heap it maps for itself. V8 never calls `malloc` for
the JavaScript heap, so a `runtime.alloc` built on the shim would have reported
Node's internal C allocations and called them the runtime's allocation activity.

The chain is now the USDT backend alone, with a caveat pointing at
`mem.heap.alloc` for the C allocator beneath the runtime. `runtime.alloc` is
Wave 2 work, and reporting it as out of reach is the truthful state.

### `runtime.jit`'s file backend requires `/tmp`, not the map

The backend was declared as requiring `/tmp/perf-[pid].map`. That is a fact about
how the observed program was started, not about this machine, and a requirement
that is probed against the tool's own pid before a program has even been
launched answers a question nobody asked. It would also refuse every launch
target: the map cannot exist before the program does.

What the machine must provide is `/tmp`, which is probed and verified. Whether
the map appears is a fact about the run, and the run reports it — as data, in
the loss line, with the reason.

## What is left, and why

| What | Why it is left |
|---|---|
| `runtime.jit` over `bpf:usdt` | HotSpot's `method__compile__begin`/`end` pass the method as a pointer and a length; reading it needs a bounded string read (`bpf_probe_read_user_str`) into the record, which this program does not do yet. The table is the easy half. |
| `runtime.custom` over `bpf:uprobe` | a probe on a symbol needs somewhere to name the symbol, and the `uprobe` PMU needs CAP_SYS_ADMIN; the uprobe-multi link would serve a symbol too, once `attach.rs` has it |
| `runtime.alloc` in `poll` | the catalog declares it; the collector refuses it as not built (exit 3). A per-interval total is the counting shape with a drain per tick |
| probes in a library loaded with `dlopen`, for a launched program | nothing names it before the program runs; a `--pid` target sees it, because the map does |
| a launched `sh -c` whose child carries the probes | the objects are read from `argv[0]`; the link follows one process |
| symbol-relative and indexed operands | carried as unread and counted; resolving `sym(%rip)` needs the symbol table and the load bias |
| the pre-exec child | a launched program is linked while it is still a fork of `omn`, so a libc probe the child hits before `execve` is counted as the target's. The gate path hits none of libc's probes, which is why no scenario sees it |
| other architectures | refused by ELF machine number |

## The JIT map, from wave one

These findings stand as they were written.

### The JIT symbol map is the one runtime signal that needs no probe

`runtime.jit` reads `/tmp/perf-<pid>.map`, the file a runtime maintains so that
a profiler can name code that exists in no object file. Node writes it with
`--perf-basic-prof`, a JVM with an agent. It is append-only, so the bytes that
appeared since the last read are the code objects created since the last read,
which is a compilation stream without a single probe, a capability or a kernel
version.

Measured on `node --perf-basic-prof` running a recursive fibonacci: 2452 code
objects over a 50 ms run, of which 1839 are V8's own builtins and the rest is
compiled JavaScript.

### A file with no timestamps produces events that say so

The map records that code exists. It does not record when the compiler ran, when
it finished, or when the line was written. So an entry is known to have appeared
between two reads and no more precisely than that, and every event carries that
gap as `ts_err_ns` — the first use in this project of a field the event model
has had since phase 1.

The first read is the harder case. Its entries were in the file before anything
was watching, and for a `--pid` target they may be hours old: the file records
nothing that would bound it. They carry `u32::MAX`, the largest error the model
can express, and an attribute that marks them, rather than the small error that
would be true for a launched program and a lie for an attached one.

Timing a compilation from this file is not possible, and the backend says so as
a degradation rather than subtracting two line arrivals and calling the
difference a duration. That number would be the interval between two code
objects being registered, which is not how long either took to compile.

### A kind is only a kind when the runtime says it is

V8 writes `JS:~name file:line`, `Builtin:Name`, `RegExp:pattern`: the kind of
code comes before the first colon, and separating builtins from compiled
JavaScript is most of what makes the output readable. Reading "everything before
the first colon" as the kind also turns a JVM agent's
`com.example.Service::handle` into a kind called `com.example.service`, which is
a taxonomy this tool invented. Only the prefixes V8 is known to write are read
as kinds; anything else is `code`.

### A map that is not there yet has lost nothing

Every poll before the runtime creates its map finds no file, and the first
version of this collector counted each one as a failed read. A node run then
reported a loss on every invocation, for a file that appeared five milliseconds
later with all of its contents intact: nothing was lost, and a loss line that
cries wolf on every run is how a project teaches its users to ignore the loss
line.

The distinction that matters is whether the map was ever read. Polls that found
nothing before the first successful read are counted and held: if the map is
eventually read, they were harmless; if it never is, they are reported as failed
reads with the reason, because a run that watched a program for a second and saw
nothing must say that it saw nothing — not that nothing was compiled. A map that
existed and then disappeared is a loss immediately, because entries written
after the last read are gone.

## Needs from another stream

Filed in `docs/open-findings.md` under stream U, with signatures:

- **stream T, `attach.rs`**: a uprobe-multi attach kind, so the fenced section
  of `usdt.rs` can move; and the `uprobe` requirement answered by what the
  attach needs rather than by the PMU's type file.
- **`syscall.rs`**: `prog_load` with an `expected_attach_type` -- stream T needs
  it for fentry as well, so it should land once -- and `link_create` for a
  uprobe-multi link.
- **`insn.rs`**: `lsh64_imm`, `arsh64_imm`, `or64_imm`, `ld_imm64`,
  `add64_reg` and `helper::PROBE_READ_USER`, declared locally in `usdt.rs` for
  now.
- **the coordinator**: one line in `src/bpf/mod.rs` (`pub mod usdt;`), which no
  stream owns and a new file in `src/bpf/` cannot do without; rows for the seven
  `runtime-usdt-*.yml` sessions in `examples/README.md` and `GUIDE.md`, which is
  the one red test (`cli::the_session_indexes_hold_exactly_the_sessions_on_disk`)
  on this branch; and `docs/scenarios/runtime.md`, whose "what cannot be
  verified here" section is now out of date.
- **`sched`**: glibc publishes `libc:mutex_entry`, `mutex_acquired` and
  `mutex_release` on this machine, which is a route to `sched.mutex` that needs
  no preload.
