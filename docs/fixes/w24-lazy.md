# The twenty-fourth wave, stream lazy

The lazy mode, as `BRIEF.md` decided it: a family name where a source is
expected composes that family's default, catalog data in the family's own
file, in the band `--cost low|med|high` picks. Every family has a default or
says why not, each run on this machine on a workload of that family's shape
and checked against something independent of `omn`.

Branch `feat/w24-lazy`: `5142aa5` (the mode, the defaults, the loader, the
tests) and the commit carrying `docs/lazy.md`, the `DECISIONS.md` line and
this handoff. No shared layer was changed beyond the `default:` block of
`src/catalog.rs` the brief gives this stream.

## What landed

- **The spelling** (`src/lazy.rs::spell_family`, called in `src/main.rs`'s
  `main` around `normalize`): `omn [trace] <word>` with a word of no dot is
  rewritten to `omn trace --family <word>`, a hidden flag of `TraceArgs`, so
  the parser keeps its one positional for a source. A word that is not a
  family is refused naming both what a source looks like and the families
  (`omn trace foo`: "`foo` is neither a source nor a family ...", exit 2).
- **The schema** (`src/catalog.rs`): `FamilyDefault` (`family`, `question`,
  `none`, `low`, `med`, `high`) and `DefaultRequest` (`source`, `mode`,
  `emit`, `keys`, `stack`, `target: system`, `label`), read from a `-
  default:` item of a family file (`parse_file_with_defaults`), kept on the
  `Catalog` (`default_of`, `defaults`, `with_defaults`). `from_entries` is
  unchanged; `embedded` and `load_dir` add the defaults through
  `with_defaults`, so `--catalog <dir>` is checked the same way.
- **The loader's refusals** (`check_default`): a source not of the family, a
  mode it lacks, an emit it does not produce in that mode, a key it does not
  attach, a band under its cheapest backend's declared cost, a fixed target
  other than `system` or one the source does not serve, the person's target
  given to a source that refuses `launch` or `pid`, two defaults for one
  family, `none` with no reason or beside requests, no band at all, a band
  machine-wide in one band and not another, and (serde) any other key, such
  as a `free:` band.
- **The composition** (`src/lazy.rs::compose`): the band (absent: the
  cheapest declared; undeclared: the dearest under it, said; under the
  cheapest: refused with the band to ask), the person's target (required,
  unless every request of the band is machine-wide, which then needs
  `--system` or nothing, and `--duration`), `--duration` and `--budget`
  passed to every request; the per-request flags (`--mode`, `--emit`,
  `--keys`, `--filter`, `--stack`, `--symbols`, `--backend`, `--window`,
  `--until`, `--label`) refused by name; `--cost` beside a source or a
  session file refused, pointing at `--budget`.
- **What the run says**, first, before anything is resolved: the band and
  the family's question, each request in the long form (`note: composed, in
  the long form: omn trace cpu.profile --mode sample@99hz --emit stack --
  ./prog`), and for several requests that they run over one window and how
  to write them as a session file. After resolution
  (`lazy::band_notes`, called beside the budget notes in `trace`): each
  request this machine serves through a backend declaring more than the band.
- **The defaults** (appended at the end of each `catalog/<family>.yaml`):

  | Family | Question | low | med | high |
  |---|---|---|---|---|
  | cpu | where the cpu time goes | `cpu.profile` 99 Hz `stack` | 997 Hz | 997 Hz, `--stack dwarf` |
  | mem | how much is held, does it grow | `mem.rss` 100 ms | + `mem.faults` count | + `mem.pss` |
  | io | how long reads and writes wait | -- | `io.syscall` trace hist | + `io.latency` |
  | net | how long sockets and connects wait | `net.socket` trace hist | + `net.tcp.connect` | -- |
  | sched | switched out how often, off cpu how long | `sched.switches` count | + `sched.offcpu` hist | `offcpu` hist + stack |
  | proc | threads and open files | `proc.threads`, `proc.fd` 100 ms | -- | -- |
  | gpu | how busy, how much memory | `gpu.engine_busy` (system), `gpu.mem` | -- | -- |
  | power | energy and clock of the machine | `power.energy` count, `power.freq` (system) | -- | -- |
  | runtime | `none:` which runtime is not known before the run | | | |

- **`omn list`** (text): a `DEFAULTS:` section after the table, every
  family's with no selection, one family's with `omn list <family>`, nothing
  for a source. No line starts with a source id, so the tests that read the
  table's rows by their first word are untouched.
- **`docs/lazy.md`**, for a beginner; **`DECISIONS.md`**, "Settled in the
  twenty-fourth wave: the lazy mode".
- **`tests/lazy.rs`**, 12 tests: the composition and its long form; every
  long form printed, typed back, asks for the same requests (cpu, mem, io,
  net, sched, proc at every band); the implicit `trace`; a band falling and
  a band refused; runtime's refusal with its four sources; nine refusals by
  name; power on the machine; the listing; the loader keeping a default and
  eleven refusals of one, through `Catalog::load_dir` on a copied catalog;
  and `omn mem` on this machine against the program's own `VmHWM`.
  `src/lazy.rs` carries 5 unit tests (the rewrite, the bands, the quoting,
  every embedded family has a default, band choice).

## Shown on this machine, and against what

Each with `target/debug/omn` after `make` (capabilities granted), under the
load of the other streams. Workloads in the scratchpad, written for this:

- **cpu**, a C program (`-O1 -fno-omit-frame-pointer`) whose `hot_a` runs 3
  times the iterations of `hot_b`. Samples landing in `hot_a`/`hot_b`: low
  62/21 (ratio 2.95, 83 samples at 99 Hz over 0.84 s of cpu), med 616/201
  (3.06), high 603/203 (2.97). Known ratio 3. Cost lines: 7.95 %, 17.80 %,
  9.90 % of the target's cpu, through `pmu:sw` declared `med`; the low band
  run said, before it started, that this machine serves `cpu.profile`
  through `pmu:sw`, which declares `med`.
- **mem**, python touching 8 x 32 MiB: `mem.rss` max 265.7 MiB = 272 077 KiB
  against the program's own `VmHWM` 272 116 kB (40 KiB short, one poll's
  growth); same at med and high (`mem.pss` max 260.4 MiB). `--pid` on a
  `sleep`: 2.4 MiB against `VmRSS` 2452 kB. The low band said `mem.rss`
  is served here by `fs:proc:smaps_rollup`, declared `med`. `mem.faults`
  4654 for 256 MiB touched is reasoned only (THP-backed), not checked.
- **io**, `dd` writing 64 x 1 MiB with `conv=fsync`: `io.syscall` n=131,
  against `strace -f -c`'s 64 writes + 67 reads = 131, exact. `--cost low`
  refused with "`--cost med`", exit 2.
- **net**, python over loopback, 200 x 1 KiB each echoed: `net.socket` n=400
  + 401 against strace's 400 `sendto` + 401 `recvfrom`, exact;
  `net.tcp.connect` 1 established against strace's 1 `connect`.
- **sched**, python sleeping 10 x 50 ms: `sched.offcpu` n=10, mean 50.35 ms
  (known 50 ms); at high its stacks sum to 502.6 ms (known 500 ms);
  `sched.switches` 11-12, at least the 10 known.
- **proc**, python with 8 threads beside the main one and 20 files open:
  `proc.threads` max 9, `proc.fd` max 23 (20 + the three standard
  streams). `--cost high` fell to `low` and said so.
- **gpu**, `tests/gpu_workload.py arena 268435456 300 800` on the RTX 2080
  Ti: `gpu.mem` 156.0 MiB (the context) then 412.0 MiB, a step of 256 MiB,
  the known allocation (shown on the long form with `-e timeline`; the
  default's `summary` gives the max, 412 MiB). Both requests were said to be
  served by `vendor:nvml`, declared `med`, above the low band.
- **power**, `omn power --system -d 3s` beside `sudo -n turbostat --show
  PkgWatt --interval 3`: 301.84 J / 3 s = 100.6 W against turbostat's
  100.85 W package power (0.2 %). `omn power -- true` refused, pointing at
  `--system -d 5s`.
- **runtime**: refused, exit 2, with the reason and `runtime.gc`,
  `runtime.jit`, `runtime.alloc`, `runtime.custom`.

Reasoned only: that `--cost` should not imply `--budget`. Shown once: `omn
mem --budget 1% -- python3 mem.py` stopped the run on its first 100 ms poll
(4.98 % against 1 %), which is why the band is not turned into a ceiling.

## Not done, and why

1. **The JSON listing carries no defaults.** `omn list --format json` is an
   array of entries that tests and `omn-mcp` read as such; adding the
   defaults means changing that shape (an object, or a second document),
   which is the results layer's and `omn-mcp`'s to decide. The text listing
   has them.
2. **`omn-mcp` does not offer the lazy mode.** `plan_session` takes a
   session; a `family` + `cost` argument composing through
   `omn::lazy::compose` is the natural next step, not this stream's files.
3. **A low band this machine cannot keep is said, not avoided.** On this
   machine `cpu.profile` (`pmu:sw`), `mem.rss` (`fs:proc:smaps_rollup`, the
   head of its chain, with `fs:proc:statm` free below it) and both gpu
   sources (`vendor:nvml`) declare `med` at `--cost low`. Choosing the
   backend by band would be the resolver's change; the note says it today.
4. **`man/omn.1` and `CATALOG.md`** do not yet describe the mode or the
   `default:` item: the coordinator's at the end of the wave, from
   `docs/lazy.md` and the `DECISIONS.md` line.

## Needs from another stream

- **io**: `io.syscall` on a launched shell script counts only the shell's
  own calls, not its children's: `omn trace io.syscall -m trace -e summary
  -- io.sh` (a script running two `dd`s) gives n=9, where `-- dd ...` alone
  gives the exact 131. The same through the long form, so not the lazy
  mode's; it makes `omn io -- ./script.sh` read as an idle program. Either
  follow the launched tree, or say in the run that children are not
  followed.
- **run** (merge note): this stream touched `src/cli.rs` (the `source`
  positional's help and `required_unless_present_any`, the hidden `family`
  field, `cost`, `target_from` made `pub`) and `src/main.rs` (`main`'s
  parse wraps `normalize` in `omn::lazy::spell_family`; in `trace`, the
  `--cost` refusal, the `family` branch beside `cli::to_session`, the band
  notes after the budget notes; `list`'s text output appends
  `omn::lazy::listing`). Keep both sides.
- **triage** (constraint, nothing to do): `tests/triage.rs` removes
  `sched.runqlat` from a copied `sched.yaml` and expects the questions'
  own refusal; a default naming `sched.runqlat` would make that catalog
  fail to load first, so the sched default does not use it. A family
  stream adding a source to its default should know a test may trim it.

## Privileged runs wanted

None: every run above needed only the capabilities `make` grants and
`sudo -n turbostat`.

## Decision lines for `DECISIONS.md`

Written by this stream, as the brief says: "Settled in the twenty-fourth
wave: the lazy mode".

## Tests run

`cargo test --test lazy` (12 passed), `cargo test --lib lazy` (5),
`cargo test --lib catalog::` (14), `cargo test --test catalog_model` (19),
`cargo test --test cli` (28); alone, the tests outside this stream that read
the listing or a copied catalog: `runner the_json_listing_carries`,
`family_sched no_sched_backend_needs_tracefs`, `family_io` and `family_mem`
`the_listing_agrees`, `raw_tracepoints verifying_the_attach`, `triage
an_input_this_machine_cannot_serve`, `scenario_proc a_machine_that_cannot_
serve` (1 each, all passed). `cargo clippy --workspace --all-targets -- -D
warnings` and `cargo fmt --check` clean.
