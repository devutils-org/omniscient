# The twenty-third wave, stream top

Branch `feat/w23-top`, from `main` at `f42b3e8` (stream runner and stream
cross merged). Every pair of `catalog/references/top.yaml` is now the
runner's, and all eleven hold.

## What landed

| Commit | What |
|---|---|
| `f50447f` | `top.yaml` rewritten to the README's schema (ids, `run: attach` / `alongside`, paths with `[source=...]`/`[emit=...]`/`[key=...]` filters, `omn_parse`); `hold.py` names its phase in its `comm`; `at.sh` (new) waits for it and execs the tool; two parsers of omn's output (`omn_cpu_total`, `omn_cpu_share`) tested on captured omn JSON; smem's capture re-taken from the installed smem 1.5-8; `onpid.py` and `run_pairs.py` deleted; `docs/replaces/top.md` with the runner's verdicts and pair ids |
| this | the handoff |

### How the pairs run now

- **Per-process pairs are `run: attach`.** The runner starts the workload
  stopped before its exec and gives both observers its pid. omn is
  therefore attached before the program begins, so the TIME+ pair, which
  had to be alternate runs in the twenty-second wave (an attach counted
  from the attach), is one run now.
- **The tool reads at a known phase through `at.sh WHEN PID TOOL ...`.**
  The runner starts the tool with omn, before it lets the workload go, and
  sends the workload's stderr nowhere, so `onpid.py`'s "wait for `ready` on
  stderr" cannot work. `hold.py` writes `hold-ready` then `hold-done` to
  `/proc/self/comm`; `at.sh` polls `/proc/<pid>/comm` (20 ms) and execs the
  tool, so the output and exit status are the tool's. With a number as
  WHEN, it sleeps that long first. If the process ends before naming the
  phase, it exits 1 and says so.
- **The two cpu-share pairs line up their windows**: omn's `--window
  1s..3s` against `at.sh 1 ... top -b -n 2 -d 2`, so top's second iteration
  and omn's window cover the same two seconds, give or take omn's startup.
- **`cpu.oncpu_time`'s total** (`count * mean` summed over the series) and
  **that total over `observed_ns`** are `omn_parse` parsers, because the path
  grammar has no arithmetic.

## Each pair's outcome

Three full runs of `pairs_top` plus three of each PSS pair alone, all on the
wave's loaded machine:

| Pair | Outcome | Measured |
|---|---|---|
| `top-res` (`mem.rss` / `top` RES) | holds, rel 0.01 | equal, 3 of 3 |
| `top-time` (`cpu.oncpu_time` / `top` TIME+) | holds, abs 30 ms | 2.0009-2.0015 s against 1.99 s (`bpf:raw_tp`) |
| `top-cpu` (`cpu.oncpu_time` / `top` %CPU) | holds, abs 0.05 | 0.002-0.005 apart, about 0.99 |
| `top-cpus` (`cpu.util --system` / `top` %Cpu(s)) | holds, abs 0.05 | 0.0003-0.019 apart, busy 0.09-0.44 |
| `pmap-x-rss` | holds, rel 0.01 | equal |
| `pmap-xx-rss` | holds, rel 0.01 | equal |
| `pmap-xx-pss` | holds, rel 0.01 | 0.01-0.44 % (omn's `last` poll) |
| `pmap-xx-uss` | holds, rel 0.01 | equal or 12 KiB |
| `smem-rss` | holds, rel 0.01 | equal |
| `smem-pss` | holds, `{rel: 0.02, side: above}` | omn above by 0.45-0.89 % |
| `smem-uss` | holds, rel 0.01 | equal or 12 KiB |

`tally: reference top: 11 held, 0 skipped, 0 gaps, 0 known defects, 0 failed`
(each of the three runs).

### What the runner read differently from the twenty-second wave's harness, and whose defect it is

- **PSS against omn's maximum** (`series.*.max`): `pmap-xx-pss` was 0.63 %
  off once, where it was 0.01-0.04 % before. The pair was at fault. PSS
  divides each shared page by the number of processes that map it, so it
  moves whenever another python process on the machine starts or exits,
  and eleven streams run python. The maximum over a 3 s poll is the moment
  with the fewest such processes, not the moment the tool read. Both PSS
  pairs now compare omn's **`last`** poll, the one closest to the tool's
  read. RSS and USS keep `max`: they do not move.
- **smem's PSS gap grew** from 0.27-0.29 % (Debian's script, run in place)
  to 0.45-0.89 % (the AUR's smem 1.5-8, installed). The mechanism is the
  same: smem is a python process that maps the interpreter while it reads,
  so the workload's share of those pages falls, and it can only fall. The
  pair is now one-sided (`side: above`). The bound comes from that
  mechanism: at most half the pages the workload shares, (RSS - USS) / 2,
  about 3.4 MiB or 4.8 %. The band is set at 2 %, which is reasoned, not
  measured. No defect in omn.
- Nothing else moved. RSS and USS still read the same pages, and TIME+
  and the cpu shares agree as closely as the twenty-second wave measured.

## Shown vs reasoned

- **Shown**: the eleven pairs through the runner, three runs, all held,
  numbers as above (`make`, then `cargo test --test reference pairs_top --
  --nocapture`). `cargo test --test reference top::`: 10 passed (8 tool
  parsers' tests, 2 new ones on captured omn output). `every_pair_is_well_formed`
  names no top pair, and `every_row_names_a_pair_or_why_none` names no top
  row. Both tests stay red because of other families' pairs and rows.
  `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt
  --check` are clean.
- **Shown by hand**: `omn trace cpu.oncpu_time --pid` attached to a shell
  stopped before it execs `hold.py` counts the program's whole cpu (2.0013
  s against top's 1.99). omn does **not** stop when the pid exits: it runs
  its `-d` to the end, so the TIME+ pair's `-d 6s` is wall time spent.
- **Reasoned**: the smem-pss band of 2 % (above). The `%Cpu(s)` pair's 0.05
  is shown only for busy between 0.09 and 0.44.

## Not done, and why

- The rows that still say "no pair" (VIRT/SHR, the process table of the
  whole machine, per mapping, `top -H`, htop, btop, `smem -u`/`-m`) are
  gaps in omn, filed in `docs/fixes/w22-top.md`'s "Gaps in omn". They are
  not this wave's work.
- `omn top` still has no pair: it has no batch output to parse.
- No `make check`, no `cargo test --workspace`, per the brief.

## Privileged runs wanted

None. Every top pair runs unprivileged. `cpu.oncpu_time` goes through
`bpf:raw_tp` only once `make` has granted the capabilities. Without them it
falls back to `fs:proc` in 10 ms ticks, and TIME+'s 30 ms band was shown
only through `bpf:raw_tp` in this wave.

## Decision lines

- **A pid-taking tool waits on the workload's `comm`, not on its output.**
  The runner sends the workload's output nowhere. A phase written to
  `/proc/self/comm` can be seen from outside without the runner's help.
  `at.sh` execs the tool, so the tool's status is what the runner sees.
- **TIME+ is one run under `attach`**: the workload is stopped before its
  exec, so an attach misses nothing of the program.
- **PSS compares omn's last poll**, since the value moves with the machine.
  RSS and USS compare the maximum.
- **smem's PSS is one-sided**, since smem only adds a process that maps the
  same pages.
- Parsers of omn's output (`omn_*`) live beside the tools' in
  `tests/reference/top.rs` and take the first `cpu.oncpu_time` result and
  its `summary` output by name, not by index.

## Needs from another stream

- **Coordinator** (files this stream does not own): the deleted
  `onpid.py`/`run_pairs.py` are still mentioned in `docs/dev/STATE.md` (line ~1350:
  "through `onpid.py`; TIME+ on alternate runs"), `docs/DECISIONS.md` (line
  ~1246, "top's `onpid.py`"), and `docs/open-findings.md` (lines
  ~12377-12384, the request for `onpid_attach`, now answered by `run:
  attach`: close it). `docs/replaces.md`'s top line can say "11 held, one
  run each".
- **runner** (optional): `run: attach` could carry a field for "release
  the tool at the workload's phase" instead of `at.sh`. Not needed: `at.sh`
  works and the schema stays small.
- **src** (later): `omn trace --pid P -d D` keeps running after P exits, to
  the end of D. Stopping when every target has gone would save the TIME+
  pair about 2.5 s. `cpu.oncpu_time`'s summary has no total (gap 1 of the
  twenty-second wave), which is why a parser divides.

## Tests run

`cargo test --test reference top::` (10 passed),
`cargo test --test reference pairs_top -- --nocapture` (three times: 11
held), `OMN_REFERENCE_PAIR=pmap-xx-pss` and `smem-pss` (three times each),
`every_pair_is_well_formed` and `every_row_names_a_pair_or_why_none` (no top
entries in either failure), `cargo clippy --workspace --all-targets -- -D
warnings`, `cargo fmt --check`.

## An incident

A shell line that put `cd` and a background job in one `&&` chain wrote a
capture (`omn-cpu-oncpu-time-window.json`) into the main checkout's
`tests/reference/captures/top/` for a moment. It was found by
`git status` there and removed. The main checkout was clean again before
anything else ran, and nothing else was touched.
