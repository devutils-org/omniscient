# The twenty-fourth wave, stream top

Branch `feat/w24-top`, from `main` at `067db07` (the brief), `main` merged
once at `d7cc7a2` (streams run, lazy, triage, unwind). Files:
`view/src/views/system.rs`, `view/src/views/process.rs`, the new
`tests/view_top_batch.rs`, and the `system--*`, `process--*` and two
`frame--*` snapshots in `docs/view/gallery/` those views draw. Measured
on the development machine with the other streams loading it (load
average 12 to 18).

## What landed

| Commit | What |
|---|---|
| `a5f36db` | **defects in `system`**: series keys split on spaces as well as commas, because `omn` joins a timeline's axes with a space (`task:7 cpu:2`) and the view split on commas only, so a key of two axes named no task and drew as one label. `io.bytes` and `net.bytes` are asked with the `dir` key, and each device's directions are folded back into one row with read apart from written (in apart from out). A recording that kept only its timeline now says the split its events would (**`system` split by `dir`**). A run without a launched program titles the list "every task measured", not "the target's". |
| `29c0511` | **`process` fills the terminal**: each sparkline takes the width the figures leave (at least 24 cells, figures capped at 72 columns), and the lines share the height (up to 12 rows each). A line with nothing drawn keeps one row, and the time bar gets two. **`process` on `trust::usage`** (its own three-step gradient and `Color::Blue` are gone), **`ledger::reading`** (the memory peak is the ledger's "memory held") and **`flame::hottest`** (the CPU line names the flame's hottest function). **The suspect and the lock**: a test holds the time line's lock part to the workload's 0.25 s under the lock, apart from the sleep. |
| `390c2b7` | **the batch output**: `omn_view::views::system::batch(data, sort)`. It prints the machine's panels as one `machine SOURCE KEY mean= peak= last= total= [part=]` line per series, then every task as a whitespace-separated table (`ID KIND NAME CPU ON_CPU_NS RSS_BYTES FAULTS RUN_QUEUE_NS IO_CALLS IN_IO_NS LOCK_NS GPU_MEM_BYTES`). The numbers are raw, `-` marks a column not measured, and each column's losses, time errors and refusals go on `#` lines. `tests/view_top_batch.rs`. The `--batch` flag is stream run's (below). |
| `f9027ec` | **`system` lists the machine's processes from `proc.tasks`** (stream proc's source, `feat/w24-proc` `25789e3`). Each process is a row by its name. Its cpu within the run is each poll's share over its interval, plus a first reading over a life only when the task started inside the run. The row also carries a sparkline of its share of one cpu and its resident size at the last poll, on the same rows as the run's other per-task sources. The view's **need** for `proc.tasks` (`poll@1s`, `raw`, `@system`) is not on this branch: it is added when the source is on `main` (see "Not done"). |
| `42b073b`, `f0e5511` | the fill test live as well as replayed, and the `process-run` snapshot |

## Shown versus reasoned

Shown, on this machine:

- **The process table against `top -b` and `/proc`** (with stream proc's
  branch merged into a throwaway branch, the need added, and a session made
  of the `system` view's machine needs; below for why not the full one).
  A spinner (`/usr/bin/yes` copied as `burnA`, output to `/dev/null`) gets
  the row `198818 burnA 97.5% ... 5.88s 2.41MiB`. `top -b -d 3 -n 3 -p`
  read 94.6 %, 98.6 % and 96.9 % over the same seconds, and RES 2464 KiB.
  `VmRSS` was 2464 kB before and after, and omn's 2.41 MiB is 2523136 B,
  which is 2464 KiB exactly. ON CPU 5.88 s is the sum of six polls' shares
  over their intervals, which equals the polls' `cpu_ns` difference (6.79 s
  less 0.91 s). `/proc/<pid>/stat` ticks over the wider window around the
  whole command (omn's startup included) were 7.55 s.
- **The batch on a live run against the program's own clock**: a python
  program that burns 0.8 s and writes `time.process_time()` at its end
  read 0.8158 s, and the batch's rows held 0.8000 s (`fs:proc`'s 10 ms
  ticks; one record of `cpu.oncpu_time` lost, said on a `#` line). The
  batch on three recordings (`machine`, `workload-system`, `http-server`)
  equals each task's `count x mean` read from `results.json` by the test
  itself, every task the summary holds is a row, and the rows are sorted.
  `cargo test --test view_top_batch`: 3 passed.
- **The `dir` split on a real recording**: `omn top --headless` beside a
  64 MiB `dd oflag=direct` drew `259:0 1.27MiB read 2.84GiB written`
  (the other streams' builds were writing too). The same recording with its
  `raw` events stripped, so drawn from the timeline alone, drew the same two
  figures. `/proc/diskstats`' read sectors over a window enclosing the run
  moved 2608 sectors, 1.27 MiB, the same number.
- **The lock**: on `process-run` the timebar's lock part holds 0.25 s
  within 10 %, against the manifest's 0.25 s under the lock. The sleep part
  holds at least the 0.9 s the manifest asks (`doze`, then the idle
  hold), so the suspect is `sleep`, the largest state, and the time line
  reads `sleep 45.6% on cpu 40.0% lock 12.2%`.
- **Fill**: `the_screen_fills_the_terminal` at 80x24, 160x48 and 240x60,
  replayed and live half-way. Every note is drawn, the lines span more than
  one row each above 24 rows, and the time bar takes the width the figures
  leave.
- **The ledger and the flame**: the memory line's peak is the ledger's
  "memory held" figure, the CPU line names `flame::hottest`, and the CPU
  line's mean over the life is the ledger's cpu time within 2 %.

Reasoned, not shown:

- How `system` looks with `proc.tasks` on a full live `omn top` of the
  machine. The run below starved every poller, so the process table had
  one poll of each process: names and RSS, and no cpu.
- The 256-colour gradient on a real terminal: unchanged here, and stream
  views' item.

## Found, and filed (not mine to fix)

1. **A live `omn top` of the machine starves its pollers** (shared runner,
   `src/collect/mod.rs`, and `omn top`'s session in `src/main.rs`). A full
   `omn top --headless -d 8s -o DIR` with no target ran 17 s of window and
   recorded `cpu.util@system` **2 events** at `poll@250ms` (about 64
   expected), `io.bytes@system` 6, `power.energy` 1 and `proc.tasks` one
   poll, while `mem.faults` held 801657 events, `sched.switches` 375260
   and `cpu.profile` 181154. With a target the per-target trace sources
   watch the target. Without one they watch the whole machine, and their
   drains starve the poll rounds. The same machine sources alone in a
   session (`omn top -f` of the first eight requests of `--dump-session`)
   recorded 25 `cpu.util` events in 6 s, as asked. This was under the wave's
   load, and the coordinator could rerun it alone. A
   loss would be surfaced, but a poll round that never runs is not counted
   anywhere, against the invariant that losses are counted. Also seen in
   the first run of this stream: `io.bytes`' first event had `interval_ns`
   3.4 s at a 250 ms poll.
2. **`io.bytes` on `fs:proc` counts bytes from before the window** in its
   first event (stream io). The first event, at the window's opening,
   carries the delta since the collector's baseline, taken up to 3.4 s
   earlier, during startup. In a timeline that is the first point's value
   (833 MB at offset 0 in one recording), and without `interval_ns` the
   view draws it per 250 ms, as a spike.

## Not done, and why

1. **The `proc.tasks` need in `system`'s `NEEDS`**: the source is on
   `feat/w24-proc`, not yet on `main` when this was written. Adding the need
   before the source would have every `omn top` ask for a source the catalog
   does not have. It is nine lines after `power.energy`'s need:
   `Need { source: "proc.tasks", mode: "poll@1s", emit: &["raw"], keys: &[],
   stack: None, scope: Scope::System, why: "the process list: the machine's
   processes, each its name, its cpu and its memory" }`. Then the
   `system--*` snapshots are blessed again (`OMN_VIEW_BLESS=1 cargo test
   --test view_snapshots -- system`). If this stream's fix round runs after
   proc merges, it lands it. Otherwise the coordinator does, at the merge.
2. **`omn top --batch`** (the flag): `src/cli.rs` and `src/main.rs` are
   stream run's. The library half is landed and tested. Request below.
3. **`Capacity::of` reading the recording's capacity** (stream run's
   request to this stream): it needs the `Beside::capacity` field in
   `view/src/model.rs` (stream views'), which is not on `main`. Once it is,
   it is one arm in `Capacity::of`: `data.capacity` (or wherever `Beside`
   lands) in place of `Capacity::default()` for a replay.
4. **The suspect's reading of the lock in the sched collectors**: the view
   splits what the timebar gives. That the suspect is the sleep on this
   workload is right by the manifest (0.9 s of sleep against 0.25 s on the
   lock). Nothing to change in `process`.

## Privileged runs wanted

None beyond what `make` grants. To rerun finding 1 alone on a quiet
machine:

```console
make
target/debug/omn top --headless -d 8s -o /tmp/omn-top-machine
python3 -c "import json; d=json.load(open('/tmp/omn-top-machine/results.json')); [print(r['label'], r['events_seen']) for r in d['results']]"
```

## Decision lines for `DECISIONS.md`

- **The batch output prints raw numbers, never a unit's words**: shares as
  fractions, times in nanoseconds, sizes in bytes, `-` for not measured.
  Every trust mark goes on a `#` line, so a parser that skips comments
  reads only numbers, and a person reading it sees what not to trust.
- **A process's cpu within a run, from `proc.tasks`**, is its polls'
  shares over their intervals. A first reading, over the task's whole
  life, counts only when the task started inside the run.
- **A disk's or a link's directions are one row** with its parts. A discard
  or a flush is not added to the bytes, as `/proc/diskstats`' own total
  does not hold it.

## Needs from another stream

- **run** (`src/cli.rs`, `src/main.rs`): `omn top --batch [--sort
  cpu|memory|faults|io|runqueue]`. A replay prints
  `omn_view::views::system::batch(&data, sort)` on stdout and exits 0, or
  prints the `Err` as `error: <sentence>` and exits 2. A live run is
  headless (no `--out` needed), and at its end prints the same for the
  finished run's `Data`. In `top_replay`, before `if args.headless`:
  `if args.batch { let text = omn_view::views::system::batch(&data,
  &args.sort).map_err(|why| anyhow::anyhow!(why))?; print!("{text}");
  return Ok(ExitCode::SUCCESS); }`. Also a pair for stream reference's
  `top.yaml` once it exists: `omn top --batch --replay` against `top -b`'s
  `%CPU`/`RES` for a named spinner.
- **run** (`src/main.rs` `top_session`), or the shared runner: finding 1.
  One way out is that without a target, the per-target trace sources are
  not asked by default, or are asked with a sample period.
- **io**: finding 2.
- **views** (`view/src/views/flows.rs` `fill_machine`): `io.bytes` and
  `net.bytes` at `@system` are now asked with `dir`, since the merged needs
  take the union of the keys. Flows' machine band, which reads the timeline
  by its `iface:`/`dev:` prefix, then gets one sink per direction
  (`wlan0 dir:rx`, `wlan0 dir:tx`, and a disk's `dir:discard`). It should
  fold `dir:` series into one sink per device and leave out discard and
  flush, as `system::fold_dirs` does. Until then, a live `omn top`'s flows
  band lists directions apart. The recorded fixtures are unaffected.
- **views** (`view/src/model.rs`): `Key::Dir` in the mirror. Until then
  `system` reads a direction from `Key::Other`'s JSON (`key_text`). Once
  the variant exists, its `Display` must write `dir:<direction>` for that
  path to keep working.
- **views / coordinator** (`docs/view/system.md`, `docs/view/process.md`,
  not in this stream's row): `system` lists the machine's processes from
  `proc.tasks` and splits disk and network by `dir`. `process` fills the
  terminal, and its CPU line names the flame's hottest function.
