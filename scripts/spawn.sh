#!/usr/bin/env bash
# Prepares an isolated worktree for one work stream and prints its prompt.
#
#   scripts/spawn.sh vertical
#   cd ../omniscient-vertical && claude     # paste the prompt it printed
#
# Streams own disjoint sets of files (see BRIEF.md), so the worktrees never need
# to rebase against each other and the merge order does not matter. A merge that
# conflicts anywhere other than an append seam means a stream edited something
# it does not own.
#
# This wave divides by layer rather than by family, so the lock is a named file
# list per stream. The shared eBPF layer -- btf.rs, insn.rs, map.rs, syscall.rs
# -- belongs to nobody; attach.rs belongs to the fentry stream, because fentry
# is an attach kind. The two session indexes, examples/README.md and GUIDE.md,
# belong to the coordinator and are held to the sessions on disk by a test, so a
# session added without a row is a red suite rather than a silent gap.
set -euo pipefail

declare -A OWNS=(
    [vertical]="src/bpf/map.rs src/bpf/syscall.rs src/bpf/io.rs src/bpf/request*.rs src/collect/io/block.rs src/collect/net/request*.rs catalog/net.yaml tests/vertical.rs examples/vertical-*.yml"
    [pagecache]="src/bpf/io_pagecache*.rs src/collect/io/** (except block.rs) catalog/io.yaml examples/io-*.yml tests/scenario_io.rs"
    [shim]="shim/** src/collect/mem/heap/** src/collect/sched/** src/collect/gpu/** catalog/sched.yaml catalog/gpu.yaml examples/sched-*.yml examples/gpu-*.yml tests/scenario_sched.rs tests/scenario_gpu.rs"
    [ibs]="src/perf.rs src/collect/mem/** (except heap/**) catalog/mem.yaml examples/mem-*.yml tests/scenario_mem.rs"
)
declare -A TITLE=(
    [vertical]="Stream V -- the vertical case"
    [pagecache]="Stream P -- the page cache"
    [shim]="Stream S -- the shim, pointed elsewhere"
    [ibs]="Stream B -- memory by IBS"
)
declare -A LOG=(
    [vertical]="docs/scenarios/vertical.md"
    [pagecache]="docs/families/io.md"
    [shim]="docs/families/sched.md and docs/families/gpu.md"
    [ibs]="docs/families/mem.md"
)
declare -A SCENARIO=(
    [vertical]="tests/vertical.rs"
    [pagecache]="tests/scenario_io.rs"
    [shim]="tests/scenario_sched.rs and tests/scenario_gpu.rs"
    [ibs]="tests/scenario_mem.rs"
)

VERTICAL_WORK='ROADMAP.md phase 4, the milestone it calls the real one: one request that reads
a file, decomposed boundary by boundary, each line carrying its evidence, checked
against a server whose delays the test injected. The segments must sum to the
client-measured latency within 10 %, and each injected delay must land in its row.

The one new mechanism is bpf_task_storage. Write a request id into the current
task storage where the request enters, read it at block_rq_issue in src/bpf/io.rs,
and put req:<id> on the event: that is what makes block_io IDENTITY rather than
KEY. Where a request is born -- derived from the connection, or declared by the
server through a USDT probe -- is your decision. Write down which and why.'

PAGECACHE_WORK='io.pagecache and io.readahead, which have waited only on a collector since the
seventh wave landed the BTF walk into folio.mapping. The catalog already names
the two tracepoints io.readahead reads. Ground truth: a file of known size read
cold then warm, and sequential against random. Aggregate in the kernel, and give
every row program a stack-region test and a byte-coverage test first: finding 46
is the file that had neither. src/collect/io/block.rs is stream V, not yours.'

SHIM_WORK='sched.mutex and gpu.api through the LD_PRELOAD shim that already interposes
malloc. A launch carries ONE shim stream since the eighth wave: extend what it
sends, do not open a second pipe. Ground truth for sched.mutex: two threads, a
known number of acquisitions, a known hold time. gpu.api may not be reachable on
this headless NVIDIA machine: prove the interposition on a symbol you can call
and skip loudly with what the machine offered. src/collect/mem/heap/** is yours;
the rest of src/collect/mem is stream B.'

IBS_WORK='mem.latency and mem.sharing through AMD IBS op sampling, which this Zen 2 machine
has: the dynamic PMU type is in /sys/bus/event_source/devices/ibs_op/type, and a
sample carries the data address and the load latency. src/perf.rs is yours.
Ground truth: a walk that lands in L1 on one pass and in memory on another, and
two threads on one cache line against two on two. Read docs/scenarios/mem.md
first -- the family already measures a 64 MiB walk against a 4 KiB one.
src/collect/mem/heap/** is stream S, not yours.'

stream="${1:-}"
if [[ -z "$stream" || -z "${OWNS[$stream]:-}" ]]; then
    echo "usage: $0 <stream>" >&2
    echo "streams: ${!OWNS[*]}" >&2
    exit 2
fi

repo="$(git rev-parse --show-toplevel)"
branch="feat/${stream}"
worktree="${repo}/../omniscient-${stream}"

if [[ -d "$worktree" ]]; then
    echo "worktree already exists: $worktree" >&2
else
    git -C "$repo" worktree add -b "$branch" "$worktree" HEAD >&2
fi

case "$stream" in
    vertical) WORK="$VERTICAL_WORK" ;;
    pagecache) WORK="$PAGECACHE_WORK" ;;
    shim) WORK="$SHIM_WORK" ;;
    ibs) WORK="$IBS_WORK" ;;
esac

cat <<PROMPT

--- worktree ready: $worktree (branch $branch) ---
--- start the agent there, and give it this prompt: ---

You are ${TITLE[$stream]} of the Omniscient project.

Read these first, in this order:
  1. CLAUDE.md                 the standing invariants of this repository
  2. STATE.md                  what works today, measured, and what is left
  3. BRIEF.md                  the rules, then the section for your stream
  4. docs/contributing.md      what ground truth means here

You own, and may only edit:
  ${OWNS[$stream]}
  ${LOG[$stream]}    -- your handoff, and the deliverable rather than a log about one

Everything else is read-only. Needing something from another stream files is not
a reason to edit them: write it under "## Needs from another stream" in
docs/open-findings.md, under a heading naming your stream, with the exact
signature you want, and carry on. That file is append-only per stream -- wave
three had six streams edit it freely and produced six conflicts on it.

Build with \`make\`, not \`cargo build\`: the preload shim is a workspace member
nothing depends on, and the CAP_BPF and CAP_PERFMON file capabilities do not
survive a relink. Both failures are silent -- the tool reports fewer sources as
available, which reads like the machine changed rather than the binary.

${WORK}

HOW YOU TEST -- this is a rule, not advice, and CLAUDE.md "How much to test,
and when" is the long version. The full suite is ~1600 tests and 5 to 10
minutes; four agents running it by reflex is what made the last wave slow.

  - While iterating: ONLY \`cargo test --lib <your module>\` and
    \`cargo test --test <file>\` for ${SCENARIO[$stream]}. Never \`make check\`.
  - Before each commit: ONE \`make check\`. If red, ONE
    \`cargo test --workspace --no-fail-fast\` to see every failure at once.
  - A red test outside your paths: check the flaky tally (findings 41-46 in
    docs/open-findings.md, and docs/scenarios/io.md). If it is there, rerun it
    alone ONCE and move on. Never rerun the suite for it; never fix it.
  - The session-index test in tests/cli.rs WILL be red for sessions you add.
    Expected: put the rows in your handoff, the coordinator adds them.
  - \`make\` once per build before eBPF tests, not once per test.

You are done when your own tests pass, one \`make check\` is clean apart from the
index rows you listed and known flakes you named, and everything you were given
either works with its scenarios or is explained in ${LOG[$stream]}.

Ground truth is the bar: a test that checks the code returns what the code
computed proves nothing. Choose a workload whose property you know independently
-- a known number of I/Os, an injected delay, a program that reports its own
state -- and check the tool against that.

A test that skips is not a test that passes, and a test that asserts what the
machine was doing is worse than either. Four streams share this machine. Assert
the separation rather than a calibrated threshold, carry the reference
own resolution as slack, and where the property cannot be created here, skip
loudly with the numbers that show why, and say so in ${LOG[$stream]}.

Commit on ${branch}. Do not merge; the coordinator does that.

PROMPT
