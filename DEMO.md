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
24 cpus, Linux 7.2) on 2026-09-29, and only trimmed where marked `…`.

---

## 1. Why is this program slow? Ask, and it names the cause

`omn latency` runs cheap sources first, then attaches the detailed ones where
the time went. Here the workload is a program that fights over a mutex, and
it reports its own ground truth as it exits:

```console
$ omn latency --objective latency -- target/omn-triage-fixture/workload lock 150 2000
omn-truth mode=lock … wall_ns=160236395 oncpu_ns=2974760 runq_ns=106790 lock_wait_ns=148473514 locks=150

  for latency, task:849326 of the run spent most of its time asleep in a pthread mutex
  another thread held: lock-contended: …

    STATE              SOURCE          KIND     EVIDENCE      AMOUNT  RESOLUTION   SHARE
    on-cpu             cpu.profile     sampled  KEY           3.01ms    +-1.00ms   1.8 %
      …
    runnable           sched.runqlat   traced   KEY         614.24us       exact   0.4 %
    blocked: sleeping  sched.offcpu    traced   KEY         159.33ms       exact  97.6 %
      -> 148.41ms waiting in pthread_mutex_lock over 151 acquisition(s), most of it on the mutex at 0x5562baf8c140 …
      …
      -> 93 % of it from one stack, leaf first: … pthread_mutex_lock <- main <- …
    whole              the program's life           163.29ms    +-9.11ms  100.0 %
    RESIDUE            whole - parts                339.33us   +-10.12ms
```

The program measured **148.47 ms** of lock waiting on its own clock, and `omn`
found **148.41 ms**, and named the mutex and the stack.

## 2. Where did a thread's wall clock go? Down to the microsecond

Three sources of three different kinds (a sampled profile, and two exact
in-kernel sums) are put in one table. The **residue**, whatever the parts do
not explain, is printed rather than rounded away:

```console
$ omn -f examples/join-decompose-compute.yml
decompose where-the-time-went  per task, in ns

  task:848902
    STATE     SOURCE          VALUE     KIND     EVIDENCE      AMOUNT  RESOLUTION   SHARE
    on-cpu    cpu.profile     samples   sampled  KEY         203.61ms    +-1.00ms  54.3 %
    runnable  sched.runqlat   total_ns  traced   KEY           1.81ms       exact   0.5 %
    blocked   sched.offcpu    total_ns  traced   KEY         169.28ms       exact  45.2 %
    parts                                                    374.70ms
    whole     the program's life gate to reap                374.78ms    +-5.90ms
    RESIDUE   whole - parts                                  +75.48us    +-6.90ms
```

On-cpu, waiting for a cpu and blocked add up to the program's life to within
**75 µs**, inside a stated resolution of ±6.9 ms.

## 3. Ask for too much, and it tells you what you lost

A profile at 100 000 samples a second, which no kernel will deliver:

```console
$ omn cpu.profile -m sample@100000hz -e stack -d 2s -- python3 -c 'import time
t=time.time()
while time.time()-t<3: sum(i*i for i in range(1000))'
cpu.profile  via pmu:sw  (cpu.profile)
  179738 events over 2.01s, stopped because the requested duration elapsed
  LOSS: 19026 dropped, 0 failed reads, 7 late ticks, 0 undecoded, 874 throttled
    170x the kernel throttled profile sampling: an unknown number of samples in each
         throttled period were not taken (see kernel.perf_event_max_sample_rate)
      2x the profile ring overflowed: samples were taken and never read, so the stacks
         here are a biased subset -- the busy phases are the ones that overflow
  degraded: 45510 of 65330 samples landed in a function that sets up no frame pointer,
            so the frames under the leaf are not its callers …
```

A flame graph drawn from this run would look clean. `omn` says that the
hottest phases are the ones that are missing, and that Python was built
without frame pointers.

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
    task:846237 n=1     min 2.099  mean 2.099  max 2.099  last 2.099
cost: cpu.ipc (count) via `pmu` spent 1.7ms of cpu … against the target's 552.7ms (0.31 %, declared `free`)
caveat: A ratio, not an amount: a spin loop retires instructions at a high IPC while doing nothing. …

$ omn proc.syscall -m count -e summary -- /bin/sh -c 'for i in $(seq 2000); do : > /dev/null; done'
cost: proc.syscall (count) via `bpf:raw_tp` spent 954.0us of cpu and an unknown amount inside
  the target against the target's 23.3ms (4.09 %, declared `extreme`) … the kernel is not timing
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
101 sources: 93 usable here, 4 unverified, 4 out of reach on this machine.
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
