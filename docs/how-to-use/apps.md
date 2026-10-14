# The application suite: `omn` against programs of known shape

`bench/apps/` holds one manifest per archetype of application, and
`scripts/apps.py` (or `make apps`) runs them all and prints one table: what
was expected, what `omn` found, what the program itself said, and whether
they agree. It is the suite to run before a demo, after a change, or on a new
machine, and the place to add your own program.

```console
make apps                              # build, grant the capabilities, run every manifest
scripts/apps.py --list                 # what there is, and what each one needs here
scripts/apps.py cpu-bound disk-sync    # some of them
OMN=~/.local/bin/omn scripts/apps.py   # another binary
```

Each run keeps everything under `target/apps/<time>/`: per manifest and per
step, the exact command, `omn`'s stdout and stderr, the program's own stdout
and stderr, the results, and `summary.json` for the whole table.

## What each archetype checks

| Manifest | Archetype | What `omn` must find | Against |
|---|---|---|---|
| `cpu-bound` | compute | `omn latency`: limited by the cpu | the program's on-cpu time and wall clock |
| `run-queue` | cpu-saturated | limited by the run queue | its run queue wait (`schedstat`) |
| `disk-sync` | io-bound | limited by the disk | the time it spent in `fdatasync` |
| `lock-contended` | lock-bound | limited by a mutex | its wall clock |
| `child-process` | multi-process | follows the wait into the child that ran | -- |
| `worker-thread` | multi-thread | follows the wait into the busy thread | -- |
| `build-job` | build | follows a driver's chain of steps | -- |
| `syscall-heavy` | syscall-bound | `proc.syscall`: every call counted | the 200 000 calls it made |
| `fork-exec` | launch-bound | `proc.exec`: every exec | the 50 processes it started |
| `memory-resident` | memory-bound | `mem.rss`: the footprint | the 256 MiB it touched |
| `python-interpreter` | interpreted | limited by the cpu | -- |
| `sqlite-commits` | database | limited by the disk | -- |
| `http-roundtrips` | network | `net.tcp.connect`, `net.tcp.accept`: every connection | the 30 it made |

A check is `PASS`, `FAIL`, `SKIP` (this machine cannot run it, with the
reason: a missing program, no capabilities, tmpfs where a sync must wait) or
`KNOWN` (a filed defect on this architecture, shown with its reason rather
than hidden by a looser expectation). The exit status is 1 only when
something failed.

## Ground truth

A check compares `omn` with something `omn` did not compute:

- **`omn-truth`**: the triage fixture (`src/triage/workload.c`) prints its
  kernel's accounting on stderr as it exits: `omn-truth mode=cpu tid=...
  wall_ns=... oncpu_ns=... runq_ns=...`, plus `sync_ns`, `lock_wait_ns` where
  the mode has them. The suite takes the line of the task the verdict names.
- **`bench-report`**: the bench workload (`examples/bench/workload.c`) writes a
  JSON report to `$BENCH_REPORT`: `ops`, `bytes`, `elapsed_ns`.
- **`static`**: a number the command itself fixes (`values = { connections = 30 }`).

## Adding your program

A manifest is a TOML file in `bench/apps/`. The smallest one asks `omn
latency` what limits a program:

```toml
# bench/apps/my-server-startup.toml
kind = "startup"
about = "my server, started and stopped"
requires = ["my-server"]
command = ["my-server", "--exit-after-init"]

[latency]
limit = "disk"          # cpu, cpu-queue, disk, lock, sleep, ...: what you expect
says = ["I/O-bound"]    # words the verdict must contain
```

To check a number against your program's own, have the program print it and
say how to read it, or fix it in the command:

```toml
command = ["my-client", "--requests", "100"]
truth = "static"
values = { requests = 100 }

[[trace]]
label = "connects"
args = ["net.tcp.connect", "-m", "trace", "-e", "summary"]
value = "events"        # events, count, total or max (summed over the summary's series)
truth = "requests"
tolerance = 0.0         # a share of the truth; `slack` adds an absolute margin, `at_least` a floor
```

Fields:

| Field | Meaning |
|---|---|
| `kind`, `about` | the archetype, and one line for `--list` |
| `command` | what to launch; `{fixture}`, `{bench}`, `{apps}`, `{work}` and `{nr:getppid}` are expanded |
| `requires` | programs that must be on `PATH`, else SKIP |
| `needs` | `triage-fixture` or `bench-workload`: built before the run |
| `storage` | `true` when the work directory (`~/.cache/omn-apps/work/<name>`) must not be tmpfs |
| `setup` | commands run before, each a list of arguments |
| `truth` | `omn-truth`, `bench-report` or `static` |
| `values` | the static truth |
| `known` | `{ aarch64 = "reason" }` or `{ any = "reason" }`: failures become KNOWN |
| `[latency]` | `limit`, `says`, `objective`, and `check = [{ row, truth, tolerance }]` comparing a state's amount (`on-cpu`, `runnable`, `blocked: uninterruptible`, `whole`, ...) with a truth value in nanoseconds |
| `[[trace]]` | `label`, `args` (everything after `omn trace` but the target), `value`, `truth`, `tolerance`, `slack`, `at_least`, `offset` |

Every program runs with its stdout and stderr in files of its own
(`--program-stdout`, `--program-stderr`), so `omn`'s document is never mixed
with what the program prints.

## On a virtual machine

On an aarch64 OrbStack machine (no PMU, a container's pid namespace) the
suite passes 26 checks and marks 2 known; the cpu
counters and the power sources are not in it because no VM here serves them.
Run it on the physical machine for a demo: the mutex is named there, and the
stacks carry every name.
