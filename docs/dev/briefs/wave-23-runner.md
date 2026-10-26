# Brief — every verdict through the runner

The twenty-third wave: **the twenty-second wave's pairs, run by the shared
runner**. The comparison measured `omn` against eleven families of tools, but
each family measured with a harness of its own, written before the shared
runner existed: on `main` today `every_pair_is_well_formed` rejects 105 pairs
in five families, and no verdict in `docs/replaces/` is the runner's. This
wave makes every pair the runner's, and nothing else: the gaps the comparison
found (`docs/dev/STATE.md`, "What remains", item 1 after its first bullet) are the
next waves'.

It runs in **two phases**. First the runner learns what the families asked
of it, alone with one small test fix beside it; it is merged before any
family starts. Then **eleven family streams at once**, each rewriting its
pairs to the schema the runner now reads, running them until they hold, and
deleting its own harness. Each stream is merged as soon as it is done,
reviewed while merged, its fixes merged after; the full suite runs once at
the end.

The lesson this shape answers (`docs/fixes/w22-reference.md`): a shared
schema decided in prose is not enough. The runner and its validator land
first, and the families write against code.

Read `CLAUDE.md` first -- **"How much to test, and when", its section "In a
wave of parallel streams"** -- then your section here.

**This is the only brief.** What comes after this wave is written in
`docs/dev/STATE.md` under "What remains", and nowhere else.

---

## Decided before this wave

- **A value `omn` computes is read by a parser of `omn`'s output**
  (`docs/DECISIONS.md`, settled at this brief): `compare.omn_parse: <name>`, a
  `Parser` in the family's module beside the tool's parsers, unit-tested on a
  captured `omn` output under `tests/reference/captures/<family>/` the same
  way. The dotted path stays for a value `omn` holds at one place. No jq, no
  path grammar of a family's own: the five grammars the families invented
  (`summary[*].mean`, `rate[dir:read].total`, `series[<key>].sum`,
  `<emit>.<field>`, prose) go, each pair moving to the path or to a parser.
- **The schema is code first.** Stream runner extends `tests/reference/
  runner.rs` and `catalog/references/README.md` with what the families asked
  (its section), and `every_pair_is_well_formed` refuses what it does not
  know how to run. A family that needs one more field after phase 1 files it
  and declares the pair `gap:` with the reason; it does not add a field the
  runner ignores.
- **A disagreement is a finding, not a tolerance.** A pair whose numbers
  differ because `omn` is wrong is marked `known_defect:` with the finding
  (runner: it runs, reports the defect, and fails once the numbers agree, so
  the marker cannot outlive the defect). A tolerance is widened only by a
  mechanism written in `why` (two separate runs, a poll's window, a sampling
  rate), never to make a pair green.
- **The machine has every witness installed now**: sysstat, htop, smem,
  heaptrack, tcpdump, powertop, the bcc tools, bpftrace, turbostat, ethtool,
  fio, perf, strace, valgrind, nvidia-smi, nsys, ncu, cpupower.
  `sudo -n` grants none of them, so the root-only witnesses (`bpftrace`,
  `memleak` and the bcc tools, `tcpdump`, `turbostat`, `perf` on tracepoints
  while `/sys/kernel/tracing` is mode 700) still skip with the machine's
  reason. `perf_event_paranoid` is -1. A pair that skipped in the
  twenty-second wave because a tool was missing now runs: it must hold.

---

## How you test, and how you read

- **Your family alone**: `cargo test --test reference pairs_<family> --
  --nocapture`, one pair with `OMN_REFERENCE_PAIR=<id>`, the parsers with
  `cargo test --test reference <family>::`, the data with
  `every_pair_is_well_formed` and `every_row_names_a_pair_or_why_none`. Then
  `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt
  --check`. No `make check`, no `cargo test --workspace`.
- **Eleven families measure this machine at once.** A pair red only under
  that load is rerun alone once; a pair that needs a quiet machine says so
  in `why` and uses `runs:` with medians. The coordinator's full suite at the
  end, on a quiet machine, is the verdict.
- `make` once per build before running pairs (the capabilities); never
  `sysctl`, a module, GPU clocks, `scripts/matrix.sh` or `scripts/bench.sh`.
- Read `catalog/references/README.md` whole (it is the schema), your
  family's `docs/fixes/w22-<family>.md` and your three files; everything
  else by grep and line range.

## Where you work, and how you hand off

- A worktree per stream, `../omniscient-w23-<stream>`, branch
  `feat/w23-<stream>`, with an untracked `.cargo/config.toml` that turns
  incremental compilation off. Never edit the main checkout.
- **Never `git stash`**: the stash is shared by every worktree of this
  repository, and in the twenty-second wave two streams swapped trees
  through it. Commit work in progress on your branch instead.
- A stream owns the files of its row and nothing else; a change elsewhere is
  filed in the handoff under "Needs from another stream". **No family touches
  `src/`, `view/`, `tests/reference/runner.rs`, `tests/reference.rs` or
  `tests/reference/common.rs`.**
- Hand off in `docs/fixes/w23-<stream>.md`: what landed, shown vs reasoned,
  each pair's outcome (holds / skipped and why / gap / known defect), not done
  and why, privileged runs wanted with exact commands, decision lines,
  requests to other streams.

| Stream | Phase | Owns |
|---|---|---|
| runner | 1 | `tests/reference.rs`, `tests/reference/runner.rs`, `tests/reference/common.rs`, `catalog/references/README.md`, `.github/expected-skips.txt` |
| cross | 1 | `tests/cross_machine.rs`, `examples/cross-machine-*.yml` |
| bpftrace, gpu, heap, io, net, perfrecord, perfsched, perfstat, power, sysstat, top | 2 | `catalog/references/<family>.yaml`, `tests/reference/<family>.rs`, `tests/reference/{workloads,captures}/<family>/`, `docs/replaces/<family>.md` |

`docs/replaces.md` (the index) is the coordinator's, at the end.

---

## The streams

### runner -- what the families asked, as code

Read `docs/fixes/w22-*.md` ("Needs from another stream" and "decision
lines") and `docs/open-findings.md`'s twenty-second-wave sections for stream
reference. Add, each with a validation rule in `every_pair_is_well_formed`
and a unit test of the runner on a stub tool (no real witness):

1. **`compare.omn_parse: <name>`**: a parser from the family's `parsers()`
   applied to `omn`'s output (stdout, or `{out}/results.json`) returning
   keyed values, compared key by key like the tool's. Exclusive with
   `compare.omn`.
2. **One run, three shapes** beside `separate` and `attach`, named in the
   README with what each is for:
   - **`tool_wraps_omn`** (power, perfstat): `<command> omn <omn> --
     <workload>`, the tool's numbers over `omn` and the program, `omn`'s
     over the program;
   - **`omn_launches_tool`** (io, gpu): `omn <omn> -o {out} -- <command>
     <workload>`, the tool's stdout and stderr captured to files the parser
     reads, not into `omn`'s JSON;
   - **`alongside`** (io, net, bpftrace): `omn` and the tool started,
     `settle_ms`, then the workload launched by the runner, both ending on
     their own duration; the same as `attach` without `{pid}`, so decide
     whether it is a name for that or a mode, and say so in the README.
3. **`setup:`** an argv run before each side (perfstat), and **`requires_setting:`**
   a file and the bound its value must meet (`/proc/sys/kernel/
   perf_event_paranoid` at most 0, power), skipping with the file, its value
   and the bound.
4. **Tolerances**: one-sided (`{rel: N, side: above}` / `below`, io), and on
   log2 bucket indices (`{log2_buckets: 1}`, bpftrace: a value one bucket
   apart counts as equal).
5. **`known_defect: "<finding>"`** (sysstat): runs the pair; outside its
   tolerance it prints `known defect:` and passes; inside it, it fails with
   "the defect no longer reproduces: remove `known_defect`".
6. **The tally**: one line per family at the end of `pairs_<family>` --
   held, skipped, gaps, known defects, failed -- and a `reference_tally` test
   (or the existing summary) a coordinator can read across families.

Keep every pair that is well-formed today well-formed; the five families'
rejected pairs are theirs to rewrite in phase 2, not yours. Update
`.github/expected-skips.txt` for any new skip line. Hand off with the
README's diff summarised for the families: they read it first.

### cross -- the memory-limit session's window

`tests/cross_machine.rs`,
`a_memory_limit_turns_a_cached_read_into_device_reads_and_each_byte_carries_its_rung`
is red under the full suite twice (`docs/open-findings.md`, "the writeback
lands after the program's window", and the twenty-first wave's "reclaimed
pages outside the program's intervals"). Bound the window by the cgroup's own
writeback, not by the program's exit (`memory.stat`'s `file_writeback` and
`file_dirty` back to 0, or a `syncfs` in the workload before it exits), and
read which edge the stray reclaimed pages sit on before deciding the second
assertion. Show it green five times in a row alone and once under load
(`stress-ng --cpu 0 --io 4` or a parallel `cargo build` beside it), or file
why not. Only that file and its example; nothing under `src/`.

### The eleven families -- bpftrace, gpu, heap, io, net, perfrecord, perfsched, perfstat, power, sysstat, top

The same task each, on its own files, after the runner is merged (merge
`main` first):

1. Rewrite every pair of `catalog/references/<family>.yaml` to the README's
   schema: the dotted path where `omn` holds the value at one place, else a
   parser of `omn`'s output (`compare.omn_parse`) unit-tested on a capture of
   `omn`'s real output produced now. Check every `omn` argv against today's
   `omn` (a source's real `-e` and keys), not the handoff's memory of it.
2. Run them: `cargo test --test reference pairs_<family> -- --nocapture`
   until every pair holds, skips with the machine's reason, declares a `gap:`
   or carries a `known_defect:` with its finding. The numbers your own
   harness showed in the twenty-second wave are the expectation: a pair the
   runner reads differently is a defect in the pair, the runner or `omn`, and
   you say which.
3. Replace each capture written from documentation by the tool's real output
   where the tool now runs here without root (your handoff's list), and move
   the parser tests' numbers to it.
4. Delete your harness (`run_pairs.py`, a `mod runner`, ignored tests) once
   the runner reads what it read; nothing in your module runs a tool.
5. Back every row of `docs/replaces/<family>.md`
   (`every_row_names_a_pair_or_why_none`), with the verdict the runner now
   gives -- "same numbers" only where a pair holds, with its id.

Per family, what the twenty-second wave left (`docs/fixes/w22-<family>.md`):

- **perfstat**: `{pid}` outside `attach`; the setup workloads and
  `tool_wraps_omn`.
- **perfrecord**: index paths (`results[0]`) and prose comparisons -- a
  function's share and `perf c2c`'s per-line share are `omn_parse`;
  `perf mem`'s mean is `sum / count`.
- **top**: `{pid}` outside `attach` (`top -p`, `pmap`, `smem`); `smem` is
  installed now; `summary[system]` is a key filter the grammar lacks.
- **gpu** and **io**: `[*]` and `[dir:read]` filters, `omn_launches_tool`;
  io's `iostat -dx` pairs need nothing more now.
- **heap**: heaptrack is installed now; `memleak` needs root.
- **sysstat**: every tool is installed now; `pidstat -w` is the
  `known_defect` (`sched.switches`' rate over a 12-22 ms window).
- **bpftrace**, **perfsched**, **net**, **power**: well-formed but never run
  by the runner; the root-only witnesses skip, the rest must hold.
