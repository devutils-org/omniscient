# The twenty-second wave, stream top

Branch `feat/w22-top`, from `main` at `05ecaa0` (the brief, no stream merged
yet). `BRIEF.md`, the streams table's **top** row: `top`, `htop`, `btop`
against `omn top` and `cpu.util`/`mem.*` per process; `pmap`, `smem` (RSS,
PSS, USS). `STATE.md`, "What remains", item 8. Measured on the development
machine with the other eleven streams loading it (load average up to 67).

## What landed

| Commit | What |
|---|---|
| `7781ad7` | `catalog/references/top.yaml`: **nine pairs**. `tests/reference/top.rs`: **five parsers** (`top_batch_pid`, `top_batch_cpus`, `pmap_x_total`, `pmap_XX_total`, `smem_uss_pss_rss`), eight unit tests on captures in `tests/reference/captures/top/`. `tests/reference/workloads/top/`: `hold.py` (a footprint and a cpu total fixed by its arguments, phases announced on stderr) and `onpid.py` (launches the workload, waits for its `ready` or `done` line, runs a pid-taking tool with `{pid}` substituted). `docs/replaces/top.md`: 21 rows. |
| this | the handoff |

The pairs, all alternate runs of `hold.py 64 2 1` (omn launches its own run;
the tool sees one launched by `onpid.py`):

| Pair (source / tool / key) | omn path | Tolerance | Measured (3 runs, then the YAML's exact commands once) |
|---|---|---|---|
| `mem.rss` / `top` / `res` | `summary.max` | rel 0.01 | 75584-75620 KiB against 75616-75620; 0.00-0.05 % |
| `cpu.oncpu_time` / `top` / `time` | `lifetime.cpu_ns` | abs 30 ms | 1.99 s against 2.0024-2.0072 s; 12-17 ms |
| `mem.rss` / `pmap` / `""` (`-x`) | `summary.max` | rel 0.01 | 75484-75496 KiB against 75616-75620; 0.17-0.18 % |
| `mem.rss` / `pmap` / `rss` (`-XX`) | `summary.max` | rel 0.01 | 75476-75492 KiB against 75500-75620; under 0.2 % |
| `mem.pss` / `pmap` / `pss` | `summary.max` | rel 0.03 | 69604-70122 KiB against 69651-70815; up to 1.5 % |
| `mem.uss` / `pmap` / `uss` | `summary.max` | rel 0.01 | 68804-68808 KiB against 68808-68820; 0.02 % |
| `mem.rss`, `mem.pss`, `mem.uss` / `smem` | `summary.max` | rel 0.01 / 0.03 / 0.01 | skip: `smem` not installed |

## Shown versus reasoned

Shown, on this machine:

- Every parser on a real capture of the tool on `hold.py`, except smem's
  (`cargo test --test reference top::`: 8 passed).
- **The six runnable pairs agree within their tolerances**, by a throwaway
  harness that ran each YAML entry's exact `omn` and `reference.command`
  (workload appended) and read the values as the parsers do. The runner
  (stream reference) was not on `main` when this landed: **no pair has yet
  been run by `tests/reference.rs`**. `cargo test --test reference` here
  runs only the parser tests.
- `omn trace mem.rss --pid PID` and `omn trace cpu.oncpu_time --tid TID`
  read a running `hold.py` correctly (18.0 MiB against `VmRSS` 18092 kB;
  1 s of cpu per second polled).
- `top -b -n 2 -d 1`'s busy ratio against `omn trace cpu.util --system -d
  2s`, three alternate runs: 0.60/0.67, 0.25/0.62, 0.26/0.18. Hence no pair
  (below).
- `btop --help` lists no batch or text output.

Reasoned, not shown:

- The PSS gap is `onpid.py`'s own `python3` sharing the interpreter's pages
  with the workload (omn's launch has no python beside it). Consistent with
  the sign and the size (1-1.5 %, roughly libpython's shared pages halved),
  not isolated by an experiment.
- The smem parser is tested on a capture **written by hand** in the format
  smem's manual gives for `-H -c "uss pss rss"`, with `pmap -XX`'s numbers;
  smem's real output may differ in padding. The test says so.
- The smem `-P '^python3 \S*hold\.py'` regex matching the workload alone
  (not `onpid.py`, not smem itself) is reasoned from the command lines.

## Not done, and why

- **No pair for `top`'s `%Cpu(s)` against `cpu.util --system`.** The machine
  is the target, so alternate seconds are different workloads: 0.37 apart at
  worst above. The parser `top_batch_cpus` is ready; the pair needs the
  runner to run both over the same seconds (request below).
- **No pair for `%CPU`** (`top -n 2 -d 1` against `cpu.oncpu_time -e rate`):
  not attempted, time. `top_batch_pid` already parses `cpu`.
- **`htop`**: not installed, and interactive only; **`btop`**: installed,
  interactive only. Rows say so; no pair is possible without a batch mode.
- Per-thread (`top -H`) and VIRT/SHR: rows only, no omn source to pair.

## Gaps in omn, each a line of work (filed, not fixed)

1. `cpu.oncpu_time`'s summary has **no total**: a pid's cpu over the run is
   `count * mean`. The TIME+ pair reads the reap's `lifetime.cpu_ns`
   instead, so a `--pid` target (no reap) has no comparable field.
2. **No per-process table of the whole machine**: `mem.rss`, `mem.pss`,
   `mem.uss` take no `system` target, and `cpu.oncpu_time --system` is
   `bpf:raw_tp` only. `omn top`'s system view shows totals, not processes --
   `top`'s, `htop`'s and `btop`'s first use.
3. **No virtual size and no resident split**: VIRT (`VmSize`) and SHR
   (`RssFile + RssShmem`) have no source or key.
4. **No per-mapping key** on the memory sources (`pmap -x`'s table, `smem
   -m`), and no Swap/AnonHugePages/Locked fields.
5. **One source per `omn trace`**: `smem -c "uss pss rss"` prints three
   footprints at once; omn needs three runs or a `-f` session.
6. **`--system` refuses a program** (`the argument '--system' cannot be used
   with '[PROGRAM]...'`): "run this and watch the machine" is `omn top --
   prog` or a session file, not `omn trace`.
7. `cpu.util`'s summary carries only the busy ratio, keyed `system` and per
   cpu; `top`'s per-state split (`us`, `sy`, `wa`, `st`) is not in it.
8. No process tree (`htop`'s `F5`), no user key (`smem -u`).
9. Every thread of a pid in one `fs:proc` run (`/proc/<pid>/task/*/stat`)
   without eBPF.

## Privileged runs and installs wanted

None of this family's pairs needs a privilege. To run the three skipped
smem pairs:

```console
sudo pacman -S smem           # Arch; the pairs' `requires: [smem]` then passes
cargo test --test reference   # once stream reference's runner is merged
```

`htop` (`sudo pacman -S htop`) would not add a pair: it has no batch mode.

## Decision lines

- **Pid-taking tools run through `onpid.py`**, which launches the workload
  and passes `{pid}`: a pair's reference command ends with `--` and gets the
  workload appended, as the schema says, with no runner support needed.
- **Alternate runs of a deterministic workload**, not one run observed
  twice: omn takes no pid from another launcher's output, and none of these
  tools launches a program. `hold.py` fixes the footprint and the cpu total
  by its arguments so alternate runs agree to 0.2 % (memory) and 17 ms (cpu).
- **A system-wide pair under load is left out rather than given a tolerance
  that always passes**: abs 0.25 still failed once in three.
- Parsers report bytes and nanoseconds, omn's units; `%` becomes a ratio.

## Requests to other streams

- **reference** (the runner):
  - substitute nothing in `reference.command`: `{pid}` is `onpid.py`'s, not
    the runner's; it must be passed through verbatim;
  - `compare.omn` paths used here: `summary.max` and `summary.mean` (the
    first `summary` output's single series) and `lifetime.cpu_ns` (the
    first result's `lifetime`);
  - an **empty `workload: []` passes no `--`** to omn and nothing to the
    tool, for system-wide pairs;
  - a way to run omn and the tool **over the same seconds** (both started
    together, for a system target), or `runs: N` with a median, so the
    `%Cpu(s)` pair can exist.
- **sysstat**: `top`'s `MiB Mem`/`MiB Swap` lines, its load average and
  `smem -w` are pointed at your `free` and `uptime` rows in
  `docs/replaces/top.md`.

## Tests run

`cargo test --test reference` (8 passed: this family's parser tests; no
runner yet), `cargo test --test cli` (28 passed), `cargo clippy --workspace
--all-targets -- -D warnings` (clean), `cargo fmt --check` (clean). No
`make check`, no workspace test.
