# Omniscient, in one page

**`omn` is a Linux observability tool that says what it did not see.** It
measures a program or a machine from the kernel directly (`perf_event_open`,
eBPF, procfs, sysfs, netlink; no `perf`, `bpftrace` or `bcc` underneath), and
every number it prints comes with what it lost, how it was obtained, and how
far it can be trusted.

Most profilers give you a clean-looking answer even when it is wrong. `omn` is
built around the opposite rule:

- **Losses are counted and printed, always.** A dropped sample, a throttled
  period and a late read each appear on the result, with the bias they
  introduce.
- **A request that cannot mean anything is refused, with the reason**, rather
  than answered with a number that means something else.
- **Every correlation says what backs it**: `IDENTITY` > `KEY` > `WAKEUP` >
  `TEMPORAL`, so a match by shared id never reads like a match by coincidence.
- **The tool measures its own cost**, including what it spends inside the
  target, and says *unknown* rather than zero when it cannot.

Every output below is real: it was run on the development machine (AMD Zen 2,
24 cpus, Linux 7.2) on 2026-10-02, at the fifteenth wave's merge (`37f17c8`),
with other builds sharing the machine, and only trimmed where marked `…`.

To run them yourself: install a release or `make install PREFIX=~/.local`,
grant the two capabilities with
`sudo setcap cap_bpf,cap_perfmon+ep ~/.local/bin/omn`, and check what your
machine serves with `omn list`. `docs/install.md` has the details, and what a
stock distribution serves without them. Several sections run sessions of
`examples/` or fixtures `make` builds, so run those from the source tree.

---

## 1. Why is this program slow? Ask, and it names the cause

`omn latency` runs cheap sources first, then attaches the detailed ones where
the time went. Here the workload is a program that fights over a mutex, and
it reports its own ground truth as it exits:

```console
$ omn latency --objective latency -- target/omn-triage-fixture/workload lock 150 2000
omn-truth mode=lock … wall_ns=158418461 oncpu_ns=3179570 runq_ns=28060 lock_wait_ns=146441769 locks=150

  for latency, task:40760 of the run spent most of its time asleep in a pthread mutex
  another thread held: lock-contended: …

    STATE              SOURCE          KIND     EVIDENCE      AMOUNT  RESOLUTION   SHARE
    on-cpu             cpu.profile     sampled  KEY           3.01ms    +-1.00ms   1.8 %
      …
    runnable           sched.runqlat   traced   KEY         551.50us       exact   0.3 %
    blocked: sleeping  sched.offcpu    traced   KEY         158.90ms       exact  97.3 %
      -> 146.34ms waiting in pthread_mutex_lock over 151 acquisition(s), most of it on the mutex at 0x5589aed09140 …
      -> 148.09ms waiting in futex(2) over 76 wait(s) [sched.futex KEY]
      -> 92 % of it from one stack, leaf first: … pthread_mutex_lock … <- main <- …
    whole              the program's life lifetime -  163.24ms    +-9.91ms 100.0 %
    RESIDUE            whole - parts                  781.34us   +-10.91ms
```

The program measured **146.44 ms** of lock waiting on its own clock, and `omn`
found **146.34 ms**, and named the mutex and the stack.

## 2. Where did a thread's wall clock go? Down to the microsecond

Three sources of three different kinds (a sampled profile, and two exact
in-kernel sums) are put in one table. The **residue**, whatever the parts do
not explain, is printed rather than rounded away:

```console
$ omn -f examples/join-decompose-compute.yml
decompose where-the-time-went  per task, in ns

  task:41249
    STATE     SOURCE          VALUE     KIND     EVIDENCE      AMOUNT  RESOLUTION   SHARE
    on-cpu    cpu.profile     samples   sampled  KEY         208.63ms    +-1.00ms  53.1 %
    runnable  sched.runqlat   total_ns  traced   KEY           2.76ms       exact   0.7 %
    blocked   sched.offcpu    total_ns  traced   KEY         181.68ms       exact  46.3 %
    parts                                                    393.06ms
    whole     the program's life gate to reap lifetime -     392.80ms    +-1.14ms
    RESIDUE   whole - parts                                 -267.63us    +-2.14ms
```

On-cpu, waiting for a cpu and blocked add up to the program's life to within
**268 µs**, inside a stated resolution of ±2.1 ms. The whole carries no
evidence level (`-`): it is the program's life, a bracket joined to nothing.

## 3. Ask for too much, and it tells you what you lost

A profile at 100 000 samples a second, which no kernel will deliver:

```console
$ omn cpu.profile -m sample@100000hz -e stack -d 2s -- python3 -c 'import time
t=time.time()
while time.time()-t<3: sum(i*i for i in range(1000))'
cpu.profile  via pmu:sw  (cpu.profile)
  23309 events over 2.00s, stopped because the requested duration elapsed
  LOSS: 175781 dropped, 0 failed reads, 2 late ticks, 0 undecoded, 963 throttled
    185x the kernel throttled profile sampling: an unknown number of samples in each
         throttled period were not taken (see kernel.perf_event_max_sample_rate)
    185x the profile ring overflowed: samples were taken and never read, so the stacks
         here are a biased subset -- the busy phases are the ones that overflow
  degraded: stacks unwound by `dwarf`: 8 KiB of user stack is copied with every sample,
            so the rings fill about a hundred times faster than under frame pointers …
  note: stacks unwound by `dwarf`, chosen by `--stack auto` over sframe (…) and
        fp+leaf (target:frame_pointers: missing: /usr/bin/python3 was built without
        frame pointers …)

  stack
    >= short by at least 175781 of 199090 events (88.3 % unaccounted for), and by an
       unknown number more: the kernel throttled this event 963 times …
```

A flame graph drawn from this run would look clean. `omn` says that the
hottest phases are the ones that are missing, that Python was built without
frame pointers, and that it unwound the stacks from a copy of them instead --
which is also why the rings overflowed as often as they did.

## 4. Ask something meaningless, and it refuses

```console
$ omn mem.rss --pid 1 -m trace -e raw
error: [mem.rss #1] `mem.rss` cannot be observed in `trace` mode
  hint: this source supports poll, snapshot

$ omn io.latency --system -m trace -e hist --filter "bites > 4096"
error: [io.latency #1] `io.latency` has no field `bites` to filter on; did you mean `bytes`?
  hint: `io.latency` exposes dev, rwbs, bytes, comm
```

## 5. What did measuring cost? Including inside the target

A hardware counter, then an eBPF program that runs on every syscall of the
target, in the target's own context:

```console
$ omn cpu.ipc -m count -e summary -- /bin/sh -c 'i=0; while [ $i -lt 200000 ]; do i=$((i+1)); done'
  summary
    task:42963 n=1     min 2.138  mean 2.138  max 2.138  last 2.138
cost: cpu.ipc (count) via `pmu` spent 834.0us of cpu and an unknown amount inside the target
  against the target's 512.5ms (0.16 %, declared `free`) …
caveat: A ratio, not an amount: a spin loop retires instructions at a high IPC while doing nothing. …

$ omn proc.syscall -m count -e summary -- /bin/sh -c 'for i in $(seq 2000); do : > /dev/null; done'
cost: proc.syscall (count) via `bpf:raw_tp` spent 968.0us of cpu and an unknown amount inside
  the target against the target's 21.8ms (4.43 %, declared `extreme`) … the kernel is not timing
  eBPF programs on this run: `kernel.bpf_stats_enabled` is 0 … `sysctl kernel.bpf_stats_enabled=1`
  before the run makes what the programs spend inside the target measurable
```

The benchmark measured this source at **+296 %** on a syscall-heavy target,
while the tool's own cpu said 0.42 %. So the catalog now declares it `extreme`,
and the cost line says the in-target part is unknown instead of implying it is
zero.

---

## What it covers

```console
$ omn list
101 sources: 92 usable here, 0 unverified, 9 out of reach on this machine.
```

- **Nine families**: `cpu`, `mem`, `io`, `net`, `sched`, `power`, `gpu`, `proc`,
  `runtime`. That is counters and profiles, page faults and the heap, block and
  filesystem latency, TCP and packets, scheduling, locks and wakeups, energy and
  frequency, NVIDIA GPUs, syscalls, and USDT probes in runtimes.
- **One vocabulary** of five axes (source, target, mode, emit, keys),
  kept as data in `catalog/*.yaml`. The CLI, the validator and the resolver
  are all read from it, and the resolver says which backend it chose and why it
  passed over the others.
- **Joins across families**, for example which allocation caused which page
  faults, or which wakeup released which block, each with its evidence level.
- **Sessions** in YAML, JSON or TOML, and nine output formats, among them
  `pprof`, `speedscope`, `folded`, `chrome` and `json`.
- **Portability checked on real kernels** (5.15, 6.1, 6.6, 7.2) in virtual
  machines: a source the tool calls available must actually attach there.
- **`omn-mcp`**, an MCP server, so an AI agent can read the catalog and past
  runs and plan a measurement. It runs a bounded one only when allowed, and it
  launches only the sessions its operator wrote.

## Try it

```console
make          # build, and grant the two capabilities eBPF and per-cpu perf need
make demo     # a guided tour: eight questions against your own machine
```

`README.md` is the long introduction, `GUIDE.md` has a hundred-odd sessions to
run, and `STATE.md` says what works today and what does not.
