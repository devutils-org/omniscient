# The sixteenth wave, stream docs

Stream docs owned `examples/gpu-draw-in-watts.yml`, `examples/power-energy.yml`,
`docs/scenarios/power.md`, `docs/install.md`, `docs/open-requests.md`,
`docs/contributing.md`, `README.md` and `docs/DEMO.md`. No file under `src/` or
`tests/` changed, so no `make check` was run (the brief's rule for a
documents-only stream). Every output pasted below was run on the development
machine (3900X, 24 cpus, Linux 7.2.7, paranoid -1, `CAP_BPF` and
`CAP_PERFMON` through `make`) at `37f17c8` on 2026-10-02, with the five other
streams' builds and tests sharing it.

## Landed

A document has no ground-truth test of its own. The check for each item is
the command whose real output it now quotes, or the code line it was read
against, plus `cargo test --test cli` (26 passed).

1. **`J/s` to `W` in the three documents.**
   - `examples/gpu-draw-in-watts.yml`: re-run. `system 104.49 J over 2.01s
     the counter was enabled = 52.08 W`, with `nvidia-smi
     --query-gpu=power.draw` sampled four times over the same window (53.17,
     53.21, 52.96, 53.16 W). The board is not idle here: its display is
     attached. Held by `scenario_gpu::a_counted_energy_window_divided_by_its_length_is_the_boards_draw`,
     run alone, and it passed.
   - `examples/power-energy.yml` and `docs/scenarios/power.md:84-86`:
     re-run. Quiet 23.73 W, loaded (a spinner on each of the 24 cpus)
     145.46 W. The quiet reading moved between 23.7 W and 54.7 W across
     runs because of the other streams' load, and the documents say so.
     `scenario_power::every_session_this_family_ships_validates_and_then_runs_or_refuses_by_name`
     was run alone and passed.
   - `docs/scenarios/power.md:345`: this output is from before the fix of
     defect 1, so it cannot be regenerated. Only its unit was changed to
     `W`, and a note under it says the output was not regenerated. Today
     `-e summary` gives `n=1` over the window, measured.
2. **`docs/install.md`, "The kernel": no single floor.** It now has a
   per-backend table counted from `catalog/*.yaml` at 37f17c8:
   - 33 backends need `kernel>=5.8`.
   - 5 need `kernel>=6.1`: the `bpf:raw_tp` backends of `sched.runqlat`,
     `sched.offcpu` and `sched.switches`, and both tracepoint backends of
     `sched.lock`.
   - 1 needs `kernel>=6.8`: `mem.thp`'s `fs:sys`.
   - Both `net.request` backends (`bpf:usdt`, `bpf:usdt:threads`) need
     `uprobe_multi` (6.6), and the source has no fallback: below 6.6 it is
     refused (`catalog/net.yaml:396-397`, caveat `:413-415`). A review found
     that the first version of the Ubuntu 22.04 row said its fallbacks served
     on 5.15. They do not, and the row, the table and the paragraph under the
     table now say that `net.request` is refused below 6.6.

   Each of the five 6.1 backends has a fallback below it in its chain
   (`catalog/sched.yaml:66`, `:99`, `:179`, `:218`). The section states that
   5.15 is served (86/88/88/89 sources usable on 5.15/6.1/6.6/7.2, from
   `docs/portability.md`). It also states that nothing between 5.15 and 6.1
   has been measured. The distribution table's "Floor met" column became
   "Every backend's `kernel>=` met", and Ubuntu 22.04's row says what 5.15
   refuses and that the fallbacks serve.

   The same file also changed in three other places:
   - Usable counts re-measured: 92 with capabilities, and 51 for a copy of
     the same binary without them (it was 93 and 57).
   - The "Filed for another stream" list re-read against the code: the shim
     hooks landed, the refusal that leads with the backend passed over
     landed (checked by running `mem.heap.alloc` from a copy with no shim
     beside it), the licence landed, and `nosuid` is partly landed.
3. **`docs/open-requests.md`, re-read at 37f17c8.**
   - The five rows marked open without being re-read have all landed:
     - **8388**: `ResolvedRequest.stack`, `src/resolve.rs:50`, set at
       `:700`.
     - **8552**: `cross_memory.rs:169` puts the reason on the skip line, and
       both CI cells print it that way. `cross_machine.rs:807` is `ok` on
       both cells. `:1087` says what the program printed.
     - **8612**: `NoBackendDeclares`, `src/resolve.rs:239`. `omn
       sched.switches --cgroup /sys/fs/cgroup -m trace` exits 2 here.
     - **8658**: `run_example_or_skip`; all three tests are `ok` on both
       cells.
     - **8747**: `PassedOver`, `src/main.rs:973`. Run here.
   - Every other OPEN or PARTLY row was re-checked against the code, with a
     line number for each:
     - Of the twelfth wave's snapshot, 2493 (`Backend.keys`) and 2883
       (`machine_to_itself`) landed and moved to the landed table.
       2539/7803 stays PARTLY: `KeyNotCarried` is raised, but no shipped
       backend declares `keys:`.
     - Of the later filings, 19 rows moved to LANDED: 7930, 8234, 8261,
       8295, 8366, 8388, 8399, 8437 (the account; the cost itself is filed
       again), 8489, 8497, 8546, 8552, 8612, 8631, 8642, 8658, 8728, 8747
       and 8756. 8711 is PARTLY, and 8067 is still open.
     - Row 5256's list of examples was redone.
   - New counts:
     - The snapshot's rows: 69 landed, 4 superseded, 16 partly done and 16
       open.
     - Later filings: 31 landed, 1 superseded, 1 partly done and 1 open.
4. **`README.md` and `docs/DEMO.md`.**
   - The licence section was already in the README at 37f17c8:
     `LICENSE-MIT`, `LICENSE-APACHE`, and `src/bpf` under `MIT OR GPL-2.0`
     with `Dual MIT/GPL` declared to the kernel. It was checked against
     `Cargo.toml`, the SPDX line on every `src/bpf/*.rs` (all of them carry
     it), `scripts/release.sh:60-61` and `Makefile:185-186`.
   - README changes:
     - "Linux 6.1 or later" now reads "5.15 is served, each backend states
       its kernel".
     - The `omn list` line was re-run: 92 usable, 0 unverified, 9 out of
       reach.
     - The `omn list mem` excerpt was re-run, including the misaligned
       `extreme` row (filed).
   - `docs/DEMO.md`: every command was re-run and the outputs refreshed (all
     five sections and `omn list`).
     - Section 1: the program's own 146.44 ms of lock wait against `omn`'s
       146.34 ms.
     - Section 2: a residue of -267.63 µs inside ±2.14 ms.
     - Section 3: 23309 events, and short by at least 175781 of 199090.
     - Section 4: both refusals word for word, exit 2.
     - Section 5: the cost lines. `cpu.ipc`'s line now says "and an unknown
       amount inside the target".
   - The bench's +296 % and 0.42 % are quoted from `docs/bench.md` and were
     not re-run (the bench runs alone).

`docs/contributing.md`: no request addressed to it, and nothing in it went
stale against the code that this stream could find. Unchanged.

## CI expectation

None of my items is a test that fails in CI. The only test this stream's
files feed is `tests/cli.rs` (the index and session validation). It
was `ok` on both cells at 37f17c8 (`the_shipped_examples_validate`), and only
the comments of the two sessions changed.

| Test | defaults | stock |
|---|---|---|
| `cli::*` (26) | green: documents and session comments only | green: same |

## Open

- `docs/scenarios/power.md:345` is a defect's historical output with only
  its unit edited. It cannot be regenerated, because the defect is fixed.
- The quiet reading in `examples/power-energy.yml` is a shared machine's,
  23.7 W to 54.7 W across runs. A reading on an idle machine is for the
  end-of-wave quiet window, if it is wanted.
- `docs/families/sched.md:57` still calls 6.1 "that floor". It is not this
  stream's file, and it is already filed under the fifteenth wave's docs
  heading.
- `tests/scenario_power.rs:466` quotes "64.4 W against 145.3 W" as measured
  on an idle machine. That is still true of the run it describes. Not mine.

## Needs privilege

None.

## For docs/DECISIONS.md

- **A quoted output that cannot be regenerated keeps its numbers and says
  so.** When a document quotes the output of a defect that has since been
  fixed, a re-run cannot reproduce it. The fix is to edit only what the tool
  now prints differently (here, the unit), mark the output as not
  regenerated, and say what the tool prints today beside it. Inventing a
  "current" version of a defect's output would be a fabricated record. This
  follows "Losses are counted and surfaced" applied to the documents: say
  what was not re-measured.
- **A document's machine numbers carry their commit and date** (`docs/install.md`'s
  usable-count table now does). The count moves with every probe fix (93 to
  92, 57 to 51 between two waves), so a number without its build cannot be
  re-checked.

## Filed for other streams

Under "Needs from another stream — the sixteenth wave, stream docs" at the
end of `docs/open-findings.md`:

- `src/render.rs:316,344`: COST is `{:<5}`, and `extreme` overflows it, so
  every `extreme` row's DET, HERE and COLLECTOR shift right in `omn list`.
- Coordinator (repository root, `scripts/release.sh`, `Makefile`): the
  GPL-2.0 text that `src/bpf`'s second licence option names is not in the
  tree.
- `docs/portability.md:184`: it lists `sched.switches` as refused on 5.15,
  but only its `bpf:raw_tp` backend needs 6.1, and `pmu:sw` and `bpf:tp`
  need no kernel version. The next matrix sweep is what answers it.

## Session index rows

None: no session file was added.
