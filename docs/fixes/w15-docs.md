# The fifteenth wave, stream docs

Stream docs owned `docs/DECISIONS.md`, `docs/ROADMAP.md`, `CLAUDE.md`, `docs/keys.md`
(new), `docs/open-requests.md` and `docs/contributing.md`. No file under `src/`
or `tests/` changed. Every reason written into a decision was read off the
code, with the file it came from named beside it.

## Landed

Each item below lists the check that holds it. A document has no ground-truth
test of its own, so the check is the code the document was read against, plus
`cargo test --test cli` for the index and session tests (20 passed).

1. **`docs/DECISIONS.md`, the three Phase 0 lines whose reasons no longer held,
   amended in place.** They are backed by a new section, "Settled in the
   fifteenth wave: what the first lines became".
   - *Language*: Rust, with no libbpf. The programs are instruction arrays
     (`src/bpf/insn.rs`) relocated from the running kernel's BTF at load
     (`src/bpf/btf.rs`). The old reason, which cited libbpf-rs and "a
     watermark reorder buffer", is gone. Checked against `Cargo.toml` (no
     libbpf crate) and `docs/install.md` ("no libbpf ... at run time").
   - *Kernel floor*: no single floor. Each backend has its own `kernel>=`.
     Counted in `catalog/*.yaml`: 33 at `>=5.8`, 5 at `>=6.1`, 1 at `>=6.8`.
     They are answered per requirement by `src/capabilities.rs:511`, and 5.15
     is served (`docs/portability.md`: 86 usable on 5.15.221 with caps, 89 on
     7.2).
   - *Privileges*: the user-mode path at paranoid 2 is recorded as the same
     backend asked a stated subset of the question, not a lesser backend.
     Checked against the notes in `src/collect/cpu/counters.rs:681-700`,
     `src/collect/mem/page_faults.rs:368` and `src/collect/cpu/profile.rs:681`,
     and against `counted_only_in_kernel` (`src/capabilities.rs:1040`).
2. **`docs/DECISIONS.md`: no `Span` struct, and no bounded reorder window.** Each
   records why, what it costs, and the naming hazard `join::Span` presents.
   Checked against `join::Correlation`, `join::Attribution`,
   `rule.top` (`src/join.rs:2043`), and the sorts at `src/collect/mod.rs:2427`,
   `:4373` and `src/join.rs:1017`. Also checked against
   `join::input_order_does_not_change_the_answer`.
3. **`docs/ROADMAP.md`**:
   - a note under Phase 0's output;
   - phase 1's `Span` task and its criterion, pointing at the decision;
   - phase 3's watermark task and its reordering criterion, pointing at the
     decision, with what the criterion could still ask of the runner's sort;
   - phase 1's key-note criterion, pointing at `docs/keys.md`;
   - phase 9's power sentence made true: the bench reads `thermal_throttle`
     in `examples/bench/harness.rs`, `throttle_count`. It is marked pending
     the probes stream's decision;
   - phase 11's "It reads; it does not run" rewritten to what `--allow-run`
     and `--allow-launch` settled.

   No box ticked.
4. **`docs/keys.md`**: every `KeyKind` with the boundary it crosses, what it
   survives, what it does not, and the files that produce it. Every producer
   was found by grepping `src/collect/**` for its `KeyValue` constructor, and
   how keys compare was read from `join::identity`. Two findings came out of
   it (filed, below).
5. **`CLAUDE.md`**: *"86 usable sources and 51"* now reads *"92 ... and 51"*,
   as `docs/dev/STATE.md` measures. Nothing else changed.
6. **`docs/open-requests.md`, refreshed at d1f0182.** All 122 rows the twelfth
   wave's snapshot left open were judged again against the code, each with a
   file and a line. A script checked that every one of their line numbers is
   in the new tables. The twelfth, thirteenth and fourteenth waves' filings got
   a table of their own. The snapshot's own LANDED rows are kept as written.
   - Of the snapshot's open rows: 67 landed, 4 superseded, 17 partly done and
     17 still open.
   - Of what was filed since: 12 landed, 1 superseded, 1 partly done and 20
     open.

`docs/contributing.md` needed nothing. No request in `docs/open-findings.md`
names it, and nothing in it contradicts the code.

### Review fixes

- `docs/keys.md`, first comparison rule: it said a join compares the `Display`
  of `KeyValue` and nothing more, so `blk`'s open timestamp takes no part. But
  `KeyValue::Blk`'s `Display` prints the timestamp (`blk:{ptr:#x}@{open_ts}`,
  `src/event.rs`); what keeps it out of the identity is the special case in
  `join::identity` (`src/join.rs`), which formats only `blk:{ptr:#x}`, and
  `join::scope` reads `open_ts` apart. The rule now says identity is the
  `Display` except that `blk` drops its open timestamp. The `iface` paragraph
  was left as is: `Iface`'s `Display` prints the name only, so the index truly
  is unprinted and uncompared. Checked with `cargo test --test cli`.

## Open

- **The phase 9 guardrail sentence** is pending the probes stream: whether the
  `power` sources become the bench's guardrail, or the harness's own
  `thermal_throttle` read stays. The sentence says so.
- **Two ROADMAP boxes** (phase 1's five structs, phase 3's reordering) are
  still `[~]`. The coordinator decides whether a recorded decision closes them.
- **In `docs/open-requests.md`, the 8388, 8552, 8612, 8658 and 8747 rows**
  are marked open from `docs/dev/STATE.md`'s list of the fourteenth wave's filings.
  They were not re-read line by line: the wave's merge was a day old, and
  each names a file another stream owns this wave.

## Needs privilege

None. `cargo test --test cli` needs the preload shim built
(`cargo build -p omn-preload`, done here) and no capability. Without the shim,
two of its tests fail on `mem.heap.alloc`, which is the `make` versus
`cargo build` trap `CLAUDE.md` describes.

## For docs/DECISIONS.md

The five decisions above are in `docs/DECISIONS.md` already, since this stream owns
it. One decision is not taken here and is left for the probes stream: the
power guardrail (phase 9). This stream landed nothing on it.

## Filed for other streams

Under "Needs from another stream — the fifteenth wave, stream docs", at the
end of `docs/open-findings.md`:

- **`src/event.rs`, `src/join.rs`: `iface` is compared by its name alone.**
  A rename makes a second key, and `net.iface`'s `iface:lo/drops` cannot join
  `net.qdisc`'s `iface:lo`. Some producers leave the index at 0.
- **`src/request.rs`, `src/event.rs`: `proc` has no producer.** Processes are
  keyed `task:<pid>`.
- **`docs/dev/STATE.md`, `docs/install.md`, `docs/families/sched.md`**: three places
  still call 6.1 the floor.
- **The probes stream**: the power guardrail decision, to be sent under "For
  docs/DECISIONS.md" in its handoff.

## Session index rows

None. No session file was added.
