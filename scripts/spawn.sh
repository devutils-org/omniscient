#!/usr/bin/env bash
# Prepares an isolated worktree for one work stream and prints its prompt.
#
#   scripts/spawn.sh recorder
#   cd ../omniscient-recorder && claude "$(cat ../omniscient-recorder.prompt)"
#
# Streams own disjoint sets of files (see BRIEF.md), so the worktrees never need
# to rebase against each other and the merge order does not matter. A merge that
# conflicts anywhere other than an append seam means a stream edited something
# it does not own.
#
# This wave is two milestones and two sets of debts: the flight recorder, phase
# 11's MCP surface, the probes the portability matrix caught answering
# available for sources that do not attach, and the kernel-side attributions
# and drains the eleventh wave filed. The lock is a named file list per stream.
# The shared layers not listed below belong to nobody. The two session indexes,
# examples/README.md and GUIDE.md, belong to the coordinator and are held to
# the sessions on disk by a test, so a session added without a row is a red
# suite rather than a gap.
#
# The prompt is written beside the worktree, so the agent starts in one line:
#
#   cd ../omniscient-<stream> && claude "$(cat ../omniscient-<stream>.prompt)"
set -euo pipefail

declare -A OWNS=(
    [recorder]="src/collect/mod.rs src/collect/launch.rs src/collect/task.rs src/session.rs src/request.rs src/cli.rs src/main.rs src/emit.rs src/encode.rs src/render.rs src/triage/** tests/recorder.rs tests/runner.rs tests/budget.rs tests/triage.rs examples/record-*.yml man/omn.1"
    [probes]="src/capabilities.rs src/catalog.rs src/validate.rs src/resolve.rs src/filter.rs src/perf.rs src/bpf/mod.rs src/bpf/request*.rs src/bpf/btf.rs src/bpf/attach.rs src/bpf/usdt.rs catalog/*.yaml src/collect/cpu/** src/collect/proc/** src/collect/gpu/** src/collect/power/** src/collect/runtime/** src/collect/mem/page_faults.rs tests/portability.rs tests/scenario_cpu.rs tests/scenario_proc.rs tests/scenario_gpu.rs tests/scenario_power.rs tests/scenario_runtime.rs tests/family_cpu.rs tests/family_proc.rs tests/vertical.rs scripts/matrix.sh scripts/vm/** .github/**"
    [kernel]="src/bpf/wakeup.rs src/bpf/map.rs src/bpf/fentry.rs src/bpf/io_*.rs src/bpf/net_*.rs src/bpf/sched_*.rs src/bpf/mem_*.rs src/bpf/runqlat.rs src/bpf/offcpu.rs src/bpf/migrate.rs src/bpf/proc_syscall.rs src/collect/sched/** src/collect/io/** src/collect/net/** src/collect/mem/** (except page_faults.rs) tests/scenario_sched.rs tests/family_sched.rs tests/scenario_io.rs tests/scenario_net.rs tests/scenario_mem.rs"
    [mcp]="mcp/** (a new workspace member) the members line of Cargo.toml, Cargo.lock"
)
declare -A TITLE=(
    [recorder]="Stream R -- the flight recorder"
    [probes]="Stream V -- the probes tell the truth"
    [kernel]="Stream D -- the kernel's attributions and drains"
    [mcp]="Stream M -- a model reads it"
)
declare -A LOG=(
    [recorder]="docs/recorder.md"
    [probes]="docs/portability.md"
    [kernel]="docs/families/kernel.md"
    [mcp]="docs/mcp.md"
)
declare -A SCENARIO=(
    [recorder]="tests/recorder.rs, tests/runner.rs, tests/budget.rs and tests/triage.rs"
    [probes]="tests/portability.rs and the scenario_* and family_* files you own"
    [kernel]="tests/scenario_sched.rs, tests/family_sched.rs, tests/scenario_io.rs, tests/scenario_net.rs and tests/scenario_mem.rs"
    [mcp]="the tests under mcp/tests/ (cargo test -p omn-mcp)"
)

RECORDER_WORK='STATE.md "What remains" item 1, as DECISIONS.md settled it: a session whose
duration is until a predicate fires, with a retention bound and one document
per trigger -- a trigger and a retention bound, not a new kind of program. The
mechanism is your decision; write it down for DECISIONS.md in your handoff. One
document covers one window (the span the retention held when it fired) and
carries its trigger, predicate and firing value; eviction is a counted loss
through Shortfall; one clock; the predicate uses src/filter.rs grammar over
what a round read (V owns filter.rs -- file what you need); the budget holds
over a run with no end; a bound on documents and on the maximum duration.
Also stream E'"'"'s filed request: a held request that does not resolve is a
layer this machine cannot serve, not a failed run (collect::Unserved). Ground
truth: a workload that idles, bursts a known amount at a moment it reports on
its own clock, twice -- exactly two documents, each window holding its burst,
evictions counted against what the workload says it did.'

PROBES_WORK='The portability matrix (docs/portability.md) found ten classes, P1-P10, where a
probe answers available and the attach then refuses -- each filed under "the
eleventh wave, stream P" in docs/open-findings.md with what is wanted. Fix each
in the probe or the catalog so the answer is missing WITH the reason the attach
used to give, and remove its KNOWN_DEFECTS entry in tests/portability.rs. P8'"'"'s
sched half is stream D'"'"'s. Also stream S'"'"'s request to src/catalog.rs: which
fields each backend filters in the program, so resolve::placement says Kernel
only for those. And the suite without capabilities: the four tests of stream P'"'"'s
"five tests" list that are in your files, and tests/vertical.rs'"'"'s skip reason.
Ground truth is the matrix: rerun scripts/matrix.sh on the cells a fix touches
(about 9 GB of disk free; the cache is target/matrix/) and each goes from
failing to passing, while the 7.2 cell keeps attaching what it attached. You
touch the catalog loader and the resolver, which are shared layers.'

KERNEL_WORK='What the eleventh wave filed against kernel programs nobody owned. (1) Finding
48: a wakeup made from a hard or soft interrupt is credited to the interrupted
task -- read the preempt count and, in interrupt context, record the waker as
the interrupt and keep the event only when its wakee is the target'"'"'s. (2) The
drain race in io_pagecache.rs, net_qdisc.rs and net_softirq.rs: onto map::Rows
as stream K filed it, with assert_rows_taken_once and the stack-region entry.
(3) fentry::HistogramProgram'"'"'s last drain sleeps 1 ms as a grace period a
preemptible kernel does not honour. (4) sched.switches follows threads on
perf::ThreadSampler (perf.rs is V'"'"'s: use it, do not change it), and the sched
half of P8. (5) mem.latency -e stack fails one run in ten with latency_cycles
not numeric, and scenario_mem.rs'"'"'s capability-less test from stream P'"'"'s list.
Ground truth: finding 48'"'"'s reproduction (the half-blocked workload beside
interrupt load) over several runs; a known number of events drained on a short
tick, every one in a row or a counted loss; N threads switching a known number
of times.'

MCP_WORK='ROADMAP.md phase 11: an MCP server over stdio, a new workspace member mcp/
building omn-mcp over the omn library, exposing what exists -- the catalog, a
run'"'"'s resolved file and results document, the capability sweep -- and not a
report generator. Nothing in src/ is yours: if the library lacks an entry point,
file it and read the file formats. It reads and does not run (boundary 5): a
tool may validate and resolve a session and say what it would cost and degrade;
decide whether one that runs a bounded session through the omn binary exists,
and write down why. Every number handed out carries what it is missing --
Shortfall, losses, evidence level, ts_err_ns, degradations; IDENTITY and
TEMPORAL must never read alike. Keep to the dependencies the workspace has.
Ground truth: run example sessions with omn -f, point the server at the output,
and check with a protocol client in mcp/tests/ that every number, loss,
shortfall and evidence level comes back unchanged; a lossy run'"'"'s answer
carries its shortfall; a call that would change the machine is refused.'

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
    recorder) WORK="$RECORDER_WORK" ;;
    probes) WORK="$PROBES_WORK" ;;
    kernel) WORK="$KERNEL_WORK" ;;
    mcp) WORK="$MCP_WORK" ;;
esac

prompt_file="${worktree}.prompt"
cat >"$prompt_file" <<PROMPT
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
and when" is the long version. The full suite is ~1750 tests and 5 to 10
minutes; four agents running it by reflex is what made the eighth wave slow.

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
  - Run \`cargo fmt\` BEFORE \`make\`, never after: a format that changes a
    source makes the next test build relink omn without its capabilities,
    and every eBPF test then fails as unprivileged.

You are done when your own tests pass, one \`make check\` is clean apart from the
index rows you listed and known flakes you named, and everything you were given
either works with its scenarios or is explained in ${LOG[$stream]}.

Ground truth is the bar: a test that checks the code returns what the code
computed proves nothing. Choose a workload whose property you know independently
-- a known number of I/Os, an injected delay, a program that reports its own
state -- and check the tool against that.

A test that skips is not a test that passes, and a test that asserts what the
machine was doing is worse than either. Four streams share this machine. Assert
the separation rather than a calibrated threshold, carry the reference's
own resolution as slack, and where the property cannot be created here, skip
loudly with the numbers that show why, and say so in ${LOG[$stream]}.

Commit on ${branch}. Do not merge; the coordinator does that.

PROMPT

echo "--- worktree ready: $worktree (branch $branch) ---"
echo "--- prompt written to $prompt_file; start the agent with: ---"
echo "cd $worktree && claude \"\$(cat $prompt_file)\""
