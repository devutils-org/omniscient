#!/usr/bin/env bash
# Prepares an isolated worktree for one work stream and prints its prompt.
#
#   scripts/spawn.sh stock
#   cd ../omniscient-stock && claude "$(cat ../omniscient-stock.prompt)"
#
# Streams own disjoint sets of files (see docs/dev/BRIEF.md), so the worktrees never need
# to rebase against each other and the merge order does not matter. A merge that
# conflicts anywhere other than an append seam means a stream edited something
# it does not own.
#
# This wave is the first product wave: the tool right on a stock machine
# (perf_event_paranoid 2, a cloud VM with no hardware PMU), which the CI's two
# runs showed and the development machine cannot, then installable and
# demonstrable end to end. A catalog entry belongs to the stream that owns its
# collector, whole. The two session indexes, examples/README.md and docs/GUIDE.md,
# belong to the coordinator and are held to the sessions on disk by a test, so
# a session added without a row is a red suite rather than a gap. The CI logs
# the streams reproduce from are in ../omniscient-ci-logs/.
#
# The prompt is written beside the worktree, so the agent starts in one line:
#
#   cd ../omniscient-<stream> && claude "$(cat ../omniscient-<stream>.prompt)"
set -euo pipefail

declare -A OWNS=(
    [stock]="src/capabilities.rs src/resolve.rs src/validate.rs (except check_stack) src/filter.rs src/collect/cpu/** (except profile.rs and the lines of cpu/mod.rs that read request.stack) src/collect/mem/page_faults.rs src/collect/power/** src/collect/gpu/** src/collect/runtime/** src/bpf/usdt.rs the cpu.* (except cpu.profile), power.*, gpu.*, runtime.* and mem.faults entries of catalog/*.yaml tests/scenario_{cpu,power,gpu,runtime}.rs tests/family_{cpu,power,gpu,runtime}.rs tests/portability.rs tests/catalog_model.rs scripts/matrix.sh scripts/vm/** .github/** (except workflows/release.yml) examples/{cpu,power,gpu,runtime}-*.yml"
    [results]="src/collect/mod.rs src/collect/launch.rs src/collect/task.rs src/main.rs src/session.rs src/request.rs (except StackMode) src/cli.rs src/catalog.rs src/emit.rs src/encode.rs src/render.rs src/triage/** src/bpf/syscall.rs shim/** src/collect/mem/heap/** tests/{budget,runner,triage,recorder,encoders,aggregation,cli}.rs tests/bench_*.rs examples/bench/** examples/{record,triage}-*.yml man/omn.1"
    [kernel]="src/bpf/** (except syscall.rs and usdt.rs) src/collect/{sched,proc,io,net}/** src/collect/mem/** (except page_faults.rs and heap/**) src/collect/psi.rs the sched.*, proc.*, io.*, net.* and mem.* entries of catalog/*.yaml (except mem.faults) tests/scenario_{sched,proc,io,net,mem}.rs tests/family_{sched,proc,io,net,mem}.rs tests/raw_tracepoints.rs examples/{sched,proc,io,net,mem}-*.yml (except sched-wakeup.yml)"
    [agent]="src/join.rs src/event.rs src/clock.rs mcp/** tests/join_engine.rs tests/cross_*.rs tests/vertical.rs examples/{cross,join,vertical}-*.yml examples/sched-wakeup.yml examples/mcp/** (new) docs/mcp.md"
    [unwind]="src/perf.rs src/stack.rs src/unwind.rs (new) catalog/unwind.yaml (new) src/collect/cpu/profile.rs the cpu.profile entry of catalog/cpu.yaml StackMode in src/request.rs and its parsing check_stack in src/validate.rs the lines of src/collect/cpu/mod.rs and src/collect/mem/mod.rs that read request.stack tests/stack_capture.rs tests/unwind.rs (new) examples/unwind-*.yml (new)"
    [install]="Makefile scripts/release.sh (new) .github/workflows/release.yml (new) README.md docs/DEMO.md"
)
declare -A TITLE=(
    [stock]="Stream P -- the stock machine"
    [results]="Stream R -- the results say it"
    [kernel]="Stream K -- the kernel programs"
    [agent]="Stream J -- an agent benchmarks a program"
    [install]="Stream D -- it installs"
    [unwind]="Stream U -- every way to unwind a stack"
)
declare -A LETTER=([stock]=P [results]=R [kernel]=K [agent]=J [install]=D [unwind]=U)
declare -A LOG=(
    [stock]="docs/portability.md (a section \"The fourteenth wave\")"
    [results]="docs/fixes/results.md"
    [kernel]="docs/families/kernel.md (a section \"The fourteenth wave\")"
    [agent]="docs/families/join.md (a section \"The fourteenth wave\") and docs/mcp.md"
    [install]="docs/install.md (with a section \"For the coordinator\" at its end)"
    [unwind]="docs/unwind.md"
)
declare -A SCENARIO=(
    [stock]="tests/scenario_cpu.rs, tests/family_cpu.rs, tests/portability.rs, tests/catalog_model.rs and the power, gpu and runtime scenario files"
    [results]="tests/runner.rs, tests/budget.rs, tests/triage.rs, tests/recorder.rs, tests/encoders.rs, tests/aggregation.rs and tests/cli.rs"
    [kernel]="the scenario_ and family_ files of sched, proc, io, net and mem, and tests/raw_tracepoints.rs"
    [agent]="tests/join_engine.rs, tests/cross_*.rs, tests/vertical.rs and cargo test -p omn-mcp"
    [install]="the install check your handoff describes, then tests/cli.rs"
    [unwind]="tests/stack_capture.rs, tests/unwind.rs, and cargo test --lib filtered to stack, unwind or perf"
)

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

prompt_file="${worktree}.prompt"
cat >"$prompt_file" <<PROMPT
You are ${TITLE[$stream]} of the Omniscient project.

Read these first, in this order:
  1. CLAUDE.md                 the standing invariants of this repository
  2. docs/dev/STATE.md                  what works today, measured, and what is left
  3. docs/dev/BRIEF.md                  the rules, then the section for Stream ${LETTER[$stream]}
  4. docs/contributing.md      what ground truth means here

Your work is docs/dev/BRIEF.md's section for Stream ${LETTER[$stream]}, whole: its numbered
items, its ground truth, and its handoff. Several items cite a line of
docs/open-requests.md, which is a snapshot taken before the twelfth wave: check
each against the code first, and one that has landed is a line in your handoff,
not work.

You own, and may only edit:
  ${OWNS[$stream]}
  ${LOG[$stream]}    -- your handoff, and the deliverable rather than a log about one

Everything else is read-only. Needing something from another stream's files is
not a reason to edit them: write it under "## Needs from another stream" in
docs/open-findings.md, under a heading naming your stream and the fourteenth
wave, with the exact signature you want, and carry on. That file is append-only
per stream -- wave three had six streams edit it freely and produced six
conflicts on it.

Build with \`make\`, not \`cargo build\`: the preload shim is a workspace member
nothing depends on, and the CAP_BPF and CAP_PERFMON file capabilities do not
survive a relink. Both failures are silent -- the tool reports fewer sources as
available, which reads like the machine changed rather than the binary.

HOW YOU TEST -- this is a rule, not advice, and CLAUDE.md "How much to test,
and when" is the long version. The full suite is ~2000 tests and 5 to 10
minutes; six agents running it by reflex is what makes a wave slow.

  - While iterating: ONLY \`cargo test --lib <your module>\` and
    \`cargo test --test <file>\` for ${SCENARIO[$stream]}. Never \`make check\`.
  - Before each commit: ONE \`make check\`. If red, ONE
    \`cargo test --workspace --no-fail-fast\` to see every failure at once.
  - A red test outside your paths: check docs/flaky.md and the "for the flaky
    tally" notes of docs/open-findings.md. If it is there, rerun it alone ONCE
    and move on. Never rerun the suite for it; never fix it.
  - The session-index test in tests/cli.rs WILL be red for sessions you add.
    Expected: put the rows in your handoff, the coordinator adds them.
  - \`make\` once per build before eBPF tests, not once per test.
  - Run \`cargo fmt\` BEFORE \`make\`, never after: a format that changes a
    source makes the next test build relink omn without its capabilities,
    and every eBPF test then fails as unprivileged.

HOW YOU READ -- also a rule. Everything you read stays in your context until
you finish, and a context that fills up is compacted mid-task, losing detail.

  - Locate before reading: \`grep -n\` for the symbol, then read only the line
    range you need. Never read a large file whole (src/collect/mod.rs,
    src/emit.rs, src/join.rs, docs/open-findings.md, docs/open-requests.md).
  - Filter test and build output: \`... 2>&1 | grep -E '^test result|FAILED|panicked'\`
    or \`| tail -40\`. Read a full failure only for the test you are fixing.
  - Read the background documents (CLAUDE.md, docs/dev/STATE.md, docs/dev/BRIEF.md,
    docs/contributing.md) once; after that, grep them for the passage you need.

THE PORTABILITY MATRIX runs alone, at the end of the wave, by the coordinator:
its guests load the machine, and load is what makes the other streams'
measuring scenarios fail. Do not start \`scripts/matrix.sh\` during the wave.
List in your handoff each cell your change should move (kernel, config, which
source goes from what to what), with the exact command.

You are done when your own tests pass, one \`make check\` is clean apart from the
index rows you listed and known flakes you named, and everything you were given
either works with its scenarios or is explained in your handoff.

Ground truth is the bar: a test that checks the code returns what the code
computed proves nothing. Choose a workload whose property you know independently
-- a known number of I/Os, an injected delay, a program that reports its own
state -- and check the tool against that.

A test that skips is not a test that passes, and a test that asserts what the
machine was doing is worse than either. Six streams share this machine. Assert
the separation rather than a calibrated threshold, carry the reference's
own resolution as slack, and where the property cannot be created here, skip
loudly with the numbers that show why, and say so in your handoff.

Commit on ${branch}. Do not merge; the coordinator does that.

PROMPT

echo "--- worktree ready: $worktree (branch $branch) ---"
echo "--- prompt written to $prompt_file; start the agent with: ---"
echo "cd $worktree && claude \"\$(cat $prompt_file)\""
