# w26 requires — requirements read at load, and probed by kind

Phase 0a of the twenty-sixth wave (`docs/dev/BRIEF.md`, "0a · `requires`"), with
phase 1's `probe` items it absorbs.

## Landed

- `src/need.rs`: `Need`, what a `requires:` string means, read once. The
  catalog loader refuses a string no rule reads, naming the entry, the
  backend and the string (`a_misspelt_requirement_is_refused_by_name`); the
  YAML is unchanged and `omn list` shows the same strings.
- `Capabilities::probe_need`: one `match` over `Need` and `Word`. The old
  ordered chain of `strip_prefix` (`capabilities.rs:962-1143` at `bd09424`),
  `bpf::attach::probe_type_support` and `netlink_protocol` are gone; the
  `pmu:` test in `resolve.rs` reads the parsed requirement.
- **`kernel>=5.8` replaced on all 36 backends by `bpf_map:ringbuf`**,
  probed by creating the smallest ring buffer (`bpf::map_type_support`).
  The listing here is unchanged but for the string. Conservative: the
  drained backends (hash-map rows) may not publish through a ring; they keep
  the requirement they had, by feature rather than by release.
- The `bpf_probe_read_kernel` refusal names a kernel before 5.5 beside
  lockdown, since both are refused the same way.
- Probes written for what read `unknown`: `symbols:<name>` (the libc this
  process loaded), `usdt:libc` (its `.note.stapsdt`), `rdt:mbm` (resctrl's
  `mon_features`, else the cpu's `cqm_mbm` flags), `damon` (its sysfs,
  writable), `so_timestamping` (set on a socket of our own), `rocprofv3`
  (the gpu family's loadable check, `librocprofiler-sdk.so`). Left unknown
  on purpose, each with its reason: `usdt:jit`, `allocator-stats:*` (the
  program's, before it exists) and `xdp` (vocabulary).

## Shown

- `cargo test --lib`: `need`, `capabilities` (38), `catalog`, `resolve`,
  `bpf::attach` green. `the_requirements_once_unverified_are_answered`.
- `omn list --format json` identical to `bd09424`'s once `bpf_map:ringbuf`
  is read back as `kernel>=5.8`; 115 usable, 8 out of reach, unchanged.
- Here: `rdt:mbm` missing because resctrl is not mounted (the Ryzen 3900X
  has `cqm_mbm_total`); `damon` missing because its sysfs is root's.
- `cargo test --test cli`, `--test portability` green.

## Not done, moved

- **The `*_SIGNATURE` constants** stay: they are the program's own
  contract, the instructions are written against them, and the catalog line
  is the copy. Five programs hold their constant to the catalog in a test
  (`mem_program::assert_catalog_lists`); 23 do not, and there are four ways
  of checking a prototype (`check_signature` in six files,
  `expect_signature`, `fentry::expect`, `tracepoint_signature` read whole).
  Moved to `kernel`: a program declares its tracepoints and their shapes to
  the kernel collector module, which checks them at attach, and one test
  holds every declaration to its catalog line.
- **"Available means it fires"** (a backend whose tracepoint exists and
  never fires in a guest) moved to `power`, whose `bpf:raw_tp` rows are the
  ones it concerns: they require the driver.
- `rocky-8` read through the feature probe: the coordinator's sweep.
