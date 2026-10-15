# Giving the demo: ten minutes, six acts

A script for showing `omn` to someone: what to type, what to say, which line
of the output to point at, and what to check it against. Every command here
was run on the aarch64 OrbStack machine and on the development machine;
where they differ, the difference is said.

The thread through all of it is one sentence: **`omn` says what limits a
program, and it can be checked, because the programs here tell us the
answer themselves.**

---

## Before: five minutes of preparation

```console
make                                  # build, the fixtures, and the two capabilities
getcap target/debug/omn               # must print cap_bpf,cap_perfmon: every relink drops them
alias omn=target/debug/omn            # the commands below say `omn`
make apps                             # the dress rehearsal: every check PASS or KNOWN
```

- **Run it on the physical machine** if you can. A VM usually has no hardware
  PMU (no instruction counts, no IPC), no RAPL (no energy), and on aarch64 the
  pthread mutex is not named. Everything below still runs on a VM; the
  "Plan B" notes say what changes.
- A terminal of at least 120 x 40 for act 5.
- Warm the caches once (`make apps` does): the first run of anything pays
  for loading the eBPF programs and reading the kernel's BTF.
- Record act 5 in advance as a fallback (below), so a live view that
  misbehaves costs you nothing.

---

## Act 1 -- what this machine can serve (1 min)

```console
omn list | grep 'usable here'
omn list cpu.instructions
```

**Say:** "Before measuring anything, the tool asks the machine. There are three
separate questions: what the vocabulary knows, what this build can collect, and
what this machine serves *right now*."

**Point at:** the `118 sources: N usable here ... M out of reach` line, then,
in `omn list cpu.instructions`, the reason it gives for a source this machine
cannot serve (on a VM: `PERF_COUNT_HW_INSTRUCTIONS is not available on this
cpu`). A probe answers yes, no or unknown, and unknown never counts as yes.

---

## Act 2 -- why is this program slow? (3 min)

The program is `target/omn-triage-fixture/workload`: you choose what limits
it, and as it exits it prints the kernel's own accounting of itself, the line
starting `omn-truth`. `omn` does not read that line. It is the answer key.

**A program fighting over a lock:**

```console
omn latency --objective latency -- target/omn-triage-fixture/workload lock 150 2000
```

**Say:** "I did not tell it what to look for. It ran cheap sources first, saw
where the time went, and attached the detailed ones there."

**Point at:** the first sentence (on the physical machine: *asleep in a pthread
mutex another thread held: lock-contended*), the table of states that adds up
to the wall clock with a residue inside its resolution, and the `->` line
giving the time spent waiting in `futex(2)`. Compare it with `lock_wait_ns` on
the `omn-truth` line.

**Then two others, quickly:**

```console
omn latency --objective latency -- target/omn-triage-fixture/workload queue 4 150
mkdir -p ~/.cache/omn-demo && omn latency --objective latency -- target/omn-triage-fixture/workload io 100 ~/.cache/omn-demo
```

- `queue`: four threads on one cpu. **Point at** `runnable ... waiting for a
  cpu`, and compare `runnable` with `runq_ns` in `omn-truth`: they agree to
  within a fraction of a millisecond.
- `io`: a hundred `fdatasync`s. **Point at** `blocked uninterruptibly, which is
  a device and usually a disk: I/O-bound`, against `sync_ns`. The directory
  must not be on tmpfs, where a sync waits for nothing.

**Plan B (VM):** a VM's timers can be coarse -- on the OrbStack machine a
20 us sleep lasts 2.9 ms -- and the lock workload sleeps between its
acquisitions, so there its sleeps outweigh the mutex and the answer rightly
names the sleep. The stacks inside libc are partly addresses. Lead with
`queue` and `io`, which are as sharp there as on the physical machine.

---

## Act 3 -- it follows the work where it went (1 min)

```console
omn latency --objective latency -- target/omn-triage-fixture/workload child 200 2
```

**Say:** "The program itself does nothing: it waits for a child, which waits
for a grandchild, which computes. A tool that only looks at the program says
*sleeping*. This one follows the wait."

**Point at:** *while its child ran, and task:N, that child, is decomposed in
its place: ... cpu-bound*. `thread 200` shows the same thing across threads.

---

## Act 4 -- it refuses questions that cannot mean anything (1 min)

```console
omn mem.bandwidth --pid 1 -m count -e rate
omn mem.rss --pid 1 -m trace -e raw
omn io.latency --system -m trace -e hist --filter "bites > 4096"
```

**Say:** "Most tools give you a number for any question. DRAM bandwidth is a
property of the memory controller: a per-process number does not exist, so it
refuses rather than divide the machine's figure and call it yours."

**Point at:** each `error:` and its `hint:`, what the source does accept, and
`did you mean bytes?`. Nothing was attached, nothing ran: the validator refuses
before anything touches the kernel.

---

## Act 5 -- the picture (2 min)

```console
omn top -- target/omn-triage-fixture/workload lock 400 2000
```

**Say:** "Everything it measures, live, in the terminal." Press `2` for the
flame graph (on-cpu and off-cpu stacks side by side), `5` for the time bar of
where the wall clock went, `?` for the keys, `q` to quit. Without `--`, `omn
top` is the whole machine.

**Fallback, recorded beforehand:**

```console
omn top --headless -o ~/.cache/omn-demo/run -d 10s -- target/omn-triage-fixture/workload lock 400 2000
omn top --replay ~/.cache/omn-demo/run              # the same views, from the file
omn top --gallery ~/.cache/omn-demo/run             # step through every view
```

---

## Act 6 -- and it holds at scale (2 min)

```console
make apps
```

**Say:** "Thirteen kinds of program -- compute, a saturated run queue, durable
writes, a lock, a child, a thread, a build, syscalls, fork-heavy, memory, an
interpreter, a database committing, a network round trip -- each against the
number the program reports about itself."

**Point at:** the MEASURED and TRUTH columns side by side (299.90 ms against
300.82 ms of cpu; 200 000 syscalls against 200 000; 30 connections against
30), the cost note (what the measurement cost the program), and the `KNOWN`
rows: a known defect is shown with its reason, not hidden by a looser
expectation. That is the same rule as the losses: **what it did not see, it
says.**

---

## If there is time

| Show | Command | Point at |
|---|---|---|
| `strace -c` in one run, from the kernel | `omn trace proc.syscall -m count -k syscall -e summary --program-stdout /dev/null -- ls -R /usr` | one row per syscall, by name |
| the lazy mode | `omn cpu -- target/omn-triage-fixture/workload cpu 500` | it prints the requests it composed, in long form, then the profile |
| a flame graph file | `omn trace cpu.profile -m sample@997hz -e stack --format speedscope -o ~/.cache/omn-demo/prof -- target/omn-triage-fixture/workload cpu 500` | open `results.speedscope.json` in speedscope.app |
| the cost of looking | any run's `cost:` line | what the collection spent against what the target spent, and *unknown* rather than zero when the kernel did not time it |
| piping a result | `omn trace cpu.cycles -m count -e summary --format yaml --program-stdout ls.out -- ls -R \| yq` | the program's output in its own file, `omn`'s document alone on stdout |

---

## Questions people ask

- **"How is this different from perf or bpftrace?"** It speaks to
  `perf_event_open`, eBPF, procfs and netlink directly, with one clock; it
  chooses the mechanism per machine and says which; it refuses invalid
  questions; and every number carries its losses. `docs/replaces.md` maps the
  tools it replaces, command by command.
- **"What does it cost?"** Every run prints it. `docs/bench.md` has the
  measured table per source and backend.
- **"Does it work in a container?"** Yes, with the capabilities: targets and
  task ids are translated between the container's pid namespace and the
  kernel's (`docs/keys.md`).
- **"Can an agent use it?"** `omn-mcp` serves the catalog and runs approved
  sessions over MCP (`docs/mcp.md`).
