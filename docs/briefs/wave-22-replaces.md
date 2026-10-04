# Brief — one tool instead of twenty

The twenty-second wave: **`omn` against every tool it can replace**. Unifying
them is why this project exists; this wave is the proof, tool by tool. For
each tool and each of its common uses: what `omn` serves of it, whether it
gives **the same numbers on the same workload** within a declared tolerance,
and what is missing for `omn` to replace it fully. The missing parts are
filed as the next work, not done here.

It runs as **twelve streams, all at once**: one for the shared runner, eleven
for the families of tools. Each is merged as soon as it is done, reviewed
while it is merged, its fixes merged after; the full suite runs once at the
end.

Read `CLAUDE.md` first -- **"How much to test, and when", its section "In a
wave of parallel streams"** -- then your section here.

**This is the only brief.** What comes after this wave is written in
`STATE.md` under "What remains", and nowhere else.

---

## Decided before this wave

- **The tools are witnesses, never what `omn` runs on.** `DECISIONS.md`'s
  rule stands: nothing in `src/` calls `perf`, `bpftrace`, `strace` or any
  other tool. They run only in `tests/reference.rs` and its families.
- **A pair is data.** One file per family, `catalog/references/<family>.yaml`
  (read by the test only; the catalog loader does not embed it), a list of:

  ```yaml
  - source: cpu.instructions              # the omn source compared
    tool: perf stat                       # the tool, as a person types it
    use: "perf stat -e instructions -- prog"   # the common use this replaces
    workload: [python3, tests/reference/workloads/perfstat/spin.py, "2"]  # argv
    omn: [trace, cpu.instructions, -m, count, -e, summary, --format, json]   # args before `--`
    reference:
      command: [perf, stat, -x, ",", -e, instructions, --]   # the workload's argv is appended
      parse: perf_stat_csv                # a Parser's name in the family's module
      requires: [perf]                    # binaries; a missing one skips with that reason
      privileged: false                   # true: run through `sudo -n`, skip if not granted
    compare:
      key: ""                             # which parsed value ("" for a scalar; a bucket, a tid)
      omn: summary.total                  # where in omn's JSON the value is
      tolerance: {rel: 0.02}              # or {abs: N}
      why: "both read the same PMU counter over the same process; multiplexing off"
  ```

  A family may add fields its runs need; the runner ignores what it does not
  know, and stream reference documents the schema in
  `catalog/references/README.md`.
- **The parsers are code, one module per family**:
  `tests/reference/<family>.rs` exposes `pub fn parsers() -> Vec<Parser>`
  (`tests/reference/common.rs`: a name, and `fn(stdout, stderr) ->
  Result<Values, String>` returning keyed values, `""` for a scalar). Each
  parser is unit-tested on a captured output of the tool, committed under
  `tests/reference/captures/<family>/`.
- **Same execution, where the tools allow it.** Twelve streams load this
  machine: a tolerance set on two separate runs under that load is noise.
  Where both can, `omn` and the tool observe **the same run** of the workload
  (the tool launches it and `omn` attaches by pid, or the reverse); where they
  cannot, alternate runs and compare distributions, and say which in `why`.
- **The verdict is a table in the repository.** `docs/replaces/<family>.md`,
  one row per tool and use: the tool and its use, the `omn` command that
  replaces it, the share of that use covered, same numbers (yes / within N /
  no, with the pair that shows it), and what is missing. `docs/replaces.md`
  (stream reference) is the index and the summary across families.
- **A missing tool or privilege skips, with the machine's reason.** Not
  installed here: `iostat`, `mpstat`, `pidstat`, `sar` (sysstat), `htop`,
  `smem`, `heaptrack`, `tcpdump`, `powertop`, the bcc tools (`bpftrace`'s own
  tools in `/usr/share/bpftrace/tools/` are). `bpftrace`, `turbostat` and
  `tcpdump` need root, which `sudo -n` does not grant here. Write the pairs
  and parsers anyway (a captured output from the tool's documentation or one
  you can produce), let them skip with the reason, and list in the handoff
  the exact command that installs or grants what would run them. The user
  installs and grants after the wave.

---

## How you test, and how you read

- **Test what you produce**: your module's parser tests, `cargo test --test
  reference` (all pairs; yours must pass or skip with the machine's reason),
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt
  --check`. No `make check`, no `cargo test --workspace`.
- **Never read a tool's output whole**: filter it first (`--stdio` with a top
  N, `-c`, `-x,`, `head`, `jq`, `grep`). A `perf report`, a `strace` or a
  `tcpdump` can be thousands of lines.
- Workloads short: a few seconds is enough to compare two numbers. A family's
  own workloads go under `tests/reference/workloads/<family>/`.
- No sysctl, no module, no GPU clocks, no `scripts/matrix.sh` or
  `scripts/bench.sh`. `make` before eBPF runs.
- Your worktree builds without incremental compilation (its untracked
  `.cargo/config.toml`; never commit it).

## Where you work, and how you hand off

`/home/moha/repos/omniscient-w22-<stream>` on `feat/w22-<stream>`. Handoff
`docs/fixes/w22-<stream>.md`: what landed, which pairs pass and with what
tolerance, which skip and why, the gaps found (each a line `omn` would need),
the commands to install or grant what was missing, and requests to other
files. **Never edit `src/`**: a gap in `omn` is filed, not fixed, this wave.

---

## The streams

| Stream | Tools |
|---|---|
| **reference** | the runner, in `tests/reference.rs` and `tests/reference/common.rs`: load every `catalog/references/*.yaml`, run `omn` and the tool, parse, compare, skip with the machine's reason; `catalog/references/README.md`; `docs/replaces.md`; the reference skips in `.github/expected-skips*.txt`; a test that every row of `docs/replaces/*.md` names a pair or says why none |
| **perfstat** | `perf stat`: cycles, instructions, IPC, branches and misses, cache misses, context switches, migrations, page faults; per process, per cgroup, system-wide |
| **perfrecord** | `perf record`/`report` (the top functions and their shares against `cpu.profile`), `perf mem`, `perf c2c`, `perf top` |
| **perfsched** | `perf sched` (latency, timehist), `perf lock`, `perf trace -s`, `strace -c`, `strace -T` |
| **bpftrace** | the tools in `/usr/share/bpftrace/tools/` and their bcc namesakes: `biolatency`, `biosnoop`, `bitesize`, `runqlat`, `runqlen`, `execsnoop`, `killsnoop`, `opensnoop`, `syscount`, `tcpconnect`, `tcpaccept`, `tcplife`, `tcpretrans`, `tcpdrop`, `vfsstat`, `writeback`, `swapin`, `oomkill`; and the bcc ones without a `.bt` (`offcputime`, `wakeuptime`, `cachestat`, `ext4slower`, `memleak`, `hardirqs`, `softirqs`) |
| **sysstat** | `vmstat`, `free`, `numastat`, `slabtop`, `uptime`'s load, `/proc/pressure`; `iostat`, `mpstat`, `pidstat`, `sar` |
| **top** | `top`, `htop`, `btop` against `omn top` and `cpu.util`/`mem.*` per process; `pmap`, `smem` (RSS, PSS, USS) |
| **heap** | `valgrind --tool=massif` and `dhat`, `heaptrack`, `memleak` against `mem.heap.*` |
| **net** | `ss` (`-ti`, `-s`), `nstat`, `ip -s link`, `tcpdump`'s counts |
| **power** | `turbostat`, `cpupower monitor`, `powertop`, the RAPL files, against `power.*` |
| **gpu** | `nvidia-smi` (query and `dmon`), `nsys` (kernel counts and durations), `ncu` (instruction counts) against `gpu.*` |
| **io** | `fio`'s own completion latency and IOPS, `iostat`'s columns from `/proc/diskstats`, against `io.*` |

A family stream owns `catalog/references/<family>.yaml`,
`tests/reference/<family>.rs`, `docs/replaces/<family>.md`, and
`tests/reference/{workloads,captures}/<family>/`. Their skeletons are on
`main` already, empty and compiling; stream reference owns the rest of
`tests/reference*` and may extend `common.rs` without breaking the
`parsers()` signature.

---

## Exit criteria for the wave

- Every tool above has its rows in `docs/replaces/`, each with a pair that
  runs (passing or skipping with the machine's reason) or a reason there is
  none.
- `docs/replaces.md` says, across families, what `omn` replaces fully, what
  in part and with which numbers, and what it does not.
- Every gap filed as a line of work for the waves after, in the handoffs.
- The full suite, run once at the end, green apart from the flaky tally.
