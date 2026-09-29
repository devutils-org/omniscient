#!/usr/bin/env bash
# Prepares an isolated worktree for one work stream and prints its prompt.
#
#   scripts/spawn.sh budget
#   cd ../omniscient-budget && claude "$(cat ../omniscient-budget.prompt)"
#
# Streams own disjoint sets of files (see BRIEF.md), so the worktrees never need
# to rebase against each other and the merge order does not matter. A merge that
# conflicts anywhere other than an append seam means a stream edited something
# it does not own.
#
# This wave has no milestone left to land: finding 47 and the losses per ring
# (the runner), the targets the counting sources declare and nothing serves
# (perf and the resolver), the cgroup the sched and proc programs declare (the
# eBPF layer), the shapes older kernels write (the io, net and mem programs),
# and the join engine nobody owned for two waves. A catalog entry belongs to
# the stream that owns its collector, except each backend's `cost:` key, which
# is stream R's. The two session indexes, examples/README.md and GUIDE.md,
# belong to the coordinator and are held to the sessions on disk by a test, so
# a session added without a row is a red suite rather than a gap.
#
# The prompt is written beside the worktree, so the agent starts in one line:
#
#   cd ../omniscient-<stream> && claude "$(cat ../omniscient-<stream>.prompt)"
set -euo pipefail

declare -A OWNS=(
    [budget]="src/collect/mod.rs src/collect/launch.rs src/collect/task.rs src/main.rs src/session.rs src/request.rs src/cli.rs src/emit.rs src/encode.rs src/render.rs src/triage/** src/bpf/syscall.rs shim/** src/collect/mem/heap/** the cost: key of every backend line in catalog/*.yaml scripts/bench.sh tests/budget.rs tests/runner.rs tests/triage.rs tests/recorder.rs tests/encoders.rs tests/aggregation.rs tests/bench_*.rs examples/record-*.yml man/omn.1"
    [perf]="src/perf.rs src/resolve.rs src/catalog.rs (except the MatchKind enum, stream J's) src/validate.rs src/filter.rs src/capabilities.rs src/bpf/usdt.rs src/collect/cpu/** src/collect/mem/page_faults.rs src/collect/power/** src/collect/gpu/** src/collect/runtime/** the cpu.*, power.*, gpu.*, runtime.* and mem.faults entries of catalog/*.yaml (except their cost: keys) tests/scenario_{cpu,power,gpu,runtime}.rs tests/family_{cpu,power,gpu,runtime}.rs tests/portability.rs tests/catalog_model.rs scripts/matrix.sh scripts/vm/** .github/** examples/{cpu,power,gpu,runtime}-*.yml"
    [sched]="src/bpf/insn.rs src/bpf/btf.rs src/bpf/attach.rs src/bpf/mod.rs src/bpf/map.rs src/bpf/ring.rs src/bpf/fentry.rs src/bpf/wakeup.rs src/bpf/sched_*.rs src/bpf/runqlat.rs src/bpf/offcpu.rs src/bpf/migrate.rs src/bpf/proc_*.rs src/collect/sched/** src/collect/proc/** the sched.* and proc.* entries of catalog/*.yaml (except their cost: keys) tests/scenario_{sched,proc}.rs tests/family_{sched,proc}.rs tests/raw_tracepoints.rs examples/{sched,proc}-*.yml (except sched-wakeup.yml)"
    [devices]="src/bpf/io.rs src/bpf/io_*.rs src/bpf/net_*.rs src/bpf/mem_*.rs src/bpf/request.rs src/bpf/request_net.rs src/collect/io/** src/collect/net/** src/collect/mem/** (except page_faults.rs and heap/**) src/collect/psi.rs (new) the io.*, net.* and mem.* entries of catalog/*.yaml (except mem.faults, and their cost: keys) tests/scenario_{io,net,mem}.rs tests/family_{io,net,mem}.rs examples/{io,net,mem}-*.yml"
    [join]="src/join.rs src/event.rs src/stack.rs src/clock.rs the MatchKind enum of src/catalog.rs and its parsing mcp/** tests/join_engine.rs tests/cross_*.rs tests/stack_capture.rs tests/vertical.rs examples/cross-*.yml examples/join-*.yml examples/vertical-*.yml examples/sched-wakeup.yml"
)
declare -A TITLE=(
    [budget]="Stream R -- what a run costs the target"
    [perf]="Stream P -- the targets perf can serve"
    [sched]="Stream S -- the scheduler and the processes, per cgroup"
    [devices]="Stream N -- the older kernels serve what they can"
    [join]="Stream J -- the engine says what it knows"
)
declare -A LETTER=([budget]=R [perf]=P [sched]=S [devices]=N [join]=J)
declare -A LOG=(
    [budget]="docs/fixes/budget.md"
    [perf]="docs/portability.md (a section \"The thirteenth wave\")"
    [sched]="docs/families/kernel.md (a section \"The thirteenth wave\")"
    [devices]="docs/families/devices.md"
    [join]="docs/families/join.md (a section \"The thirteenth wave\")"
)
declare -A SCENARIO=(
    [budget]="tests/budget.rs, tests/runner.rs, tests/triage.rs, tests/recorder.rs and tests/encoders.rs"
    [perf]="tests/scenario_cpu.rs, tests/family_cpu.rs, tests/portability.rs, tests/catalog_model.rs and the power, gpu and runtime scenario files"
    [sched]="tests/scenario_sched.rs, tests/family_sched.rs, tests/scenario_proc.rs, tests/family_proc.rs and tests/raw_tracepoints.rs"
    [devices]="tests/scenario_io.rs, tests/scenario_net.rs, tests/scenario_mem.rs and their family_ files"
    [join]="tests/join_engine.rs, tests/cross_*.rs, tests/stack_capture.rs, tests/vertical.rs and cargo test -p omn-mcp"
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
  2. STATE.md                  what works today, measured, and what is left
  3. BRIEF.md                  the rules, then the section for Stream ${LETTER[$stream]}
  4. docs/contributing.md      what ground truth means here

Your work is BRIEF.md's section for Stream ${LETTER[$stream]}, whole: its numbered
items, its ground truth, and its handoff. Several items cite a line of
docs/open-requests.md, which is a snapshot taken before the twelfth wave: check
each against the code first, and one that has landed is a line in your handoff,
not work.

You own, and may only edit:
  ${OWNS[$stream]}
  ${LOG[$stream]}    -- your handoff, and the deliverable rather than a log about one

Everything else is read-only. Needing something from another stream's files is
not a reason to edit them: write it under "## Needs from another stream" in
docs/open-findings.md, under a heading naming your stream and the thirteenth
wave, with the exact signature you want, and carry on. That file is append-only
per stream -- wave three had six streams edit it freely and produced six
conflicts on it.

Build with \`make\`, not \`cargo build\`: the preload shim is a workspace member
nothing depends on, and the CAP_BPF and CAP_PERFMON file capabilities do not
survive a relink. Both failures are silent -- the tool reports fewer sources as
available, which reads like the machine changed rather than the binary.

HOW YOU TEST -- this is a rule, not advice, and CLAUDE.md "How much to test,
and when" is the long version. The full suite is ~1800 tests and 5 to 10
minutes; five agents running it by reflex is what makes a wave slow.

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

You are done when your own tests pass, one \`make check\` is clean apart from the
index rows you listed and known flakes you named, and everything you were given
either works with its scenarios or is explained in your handoff.

Ground truth is the bar: a test that checks the code returns what the code
computed proves nothing. Choose a workload whose property you know independently
-- a known number of I/Os, an injected delay, a program that reports its own
state -- and check the tool against that.

A test that skips is not a test that passes, and a test that asserts what the
machine was doing is worse than either. Five streams share this machine. Assert
the separation rather than a calibrated threshold, carry the reference's
own resolution as slack, and where the property cannot be created here, skip
loudly with the numbers that show why, and say so in your handoff.

Commit on ${branch}. Do not merge; the coordinator does that.

PROMPT

echo "--- worktree ready: $worktree (branch $branch) ---"
echo "--- prompt written to $prompt_file; start the agent with: ---"
echo "cd $worktree && claude \"\$(cat $prompt_file)\""
