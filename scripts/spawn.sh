#!/usr/bin/env bash
# Prepares an isolated worktree for one work stream and prints its prompt.
#
#   scripts/spawn.sh escalation
#   cd ../omniscient-escalation && claude "$(cat ../omniscient-escalation.prompt)"
#
# Streams own disjoint sets of files (see BRIEF.md), so the worktrees never need
# to rebase against each other and the merge order does not matter. A merge that
# conflicts anywhere other than an append seam means a stream edited something
# it does not own.
#
# This wave is one milestone and three sets of debts: the escalation, and the
# requests the tenth wave filed against the engine, the silent degradations and
# the drains. The lock is a named file list per stream. The shared layers not
# listed below belong to nobody, and the coordinator is working in them on main
# while the streams run. The two session indexes, examples/README.md and
# GUIDE.md, belong to the coordinator and are held to the sessions on disk by a
# test, so a session added without a row is a red suite rather than a gap.
#
# The prompt is written beside the worktree, so the agent starts in one line:
#
#   cd ../omniscient-<stream> && claude "$(cat ../omniscient-<stream>.prompt)"
set -euo pipefail

declare -A OWNS=(
    [escalation]="src/triage/** src/cli.rs src/request.rs src/session.rs src/collect/mod.rs src/collect/launch.rs src/collect/task.rs tests/triage.rs tests/runner.rs tests/budget.rs examples/triage-*.yml examples/attach-*.yml man/omn.1"
    [debts]="src/join.rs src/emit.rs src/encode.rs src/render.rs src/main.rs tests/join_engine.rs tests/aggregation.rs tests/encoders.rs tests/vertical.rs examples/join-*.yml examples/vertical-one-request.yml"
    [silent]="src/filter.rs src/resolve.rs src/perf.rs src/collect/cpu/** src/collect/proc/** src/collect/mem/page_faults.rs catalog/cpu.yaml catalog/proc.yaml catalog/mem.yaml tests/scenario_cpu.rs tests/scenario_proc.rs tests/scenario_mem.rs tests/family_cpu.rs tests/family_proc.rs examples/cpu-*.yml examples/proc-*.yml"
    [drains]="src/bpf/map.rs src/bpf/runqlat.rs src/bpf/offcpu.rs src/bpf/proc_syscall.rs src/bpf/sched_*.rs src/bpf/migrate.rs src/bpf/mem_alloc.rs src/bpf/mem_stall.rs src/collect/sched/** tests/scenario_sched.rs tests/family_sched.rs"
)
declare -A TITLE=(
    [escalation]="Stream E -- the escalation"
    [debts]="Stream J -- the engine's debts"
    [silent]="Stream S -- the silent degradations"
    [drains]="Stream K -- drains that lose nothing"
)
declare -A LOG=(
    [escalation]="docs/triage.md"
    [debts]="docs/families/join.md"
    [silent]="docs/fixes/silent.md"
    [drains]="docs/families/drains.md"
)
declare -A SCENARIO=(
    [escalation]="tests/triage.rs, tests/runner.rs and tests/budget.rs"
    [debts]="tests/join_engine.rs, tests/aggregation.rs, tests/encoders.rs and tests/vertical.rs"
    [silent]="tests/scenario_cpu.rs, tests/scenario_proc.rs and tests/scenario_mem.rs"
    [drains]="tests/scenario_sched.rs and tests/family_sched.rs"
)

ESCALATION_WORK='ROADMAP.md phase 10, item 3: once triage names a suspect, attach that layer'"'"'s
detailed collectors mid-run, detach the rest, and hold the budget. Triage exists
(src/triage/) and so does a request window (hot attach); what is missing is a
window decided during the run from the cheap pass'"'"'s own numbers. The mechanism
is your decision -- have the runner ask a trait between rounds, the way it asks
Cheaper, and keep triage out of the collector layer. Scenarios: what it
escalated to and why, the budget held and measured, a detailed layer this
machine cannot serve reported with reasons, and joins across the switch kept
honest. Ground truth: a program cpu-bound for a second then lock-bound for a
second escalates to the lock layer and its waits match its own clock. Also:
--window on the command line (stream H'"'"'s filed request). Not the flight
recorder -- that is the next wave.'

DEBTS_WORK='Four requests the tenth wave filed against src/join.rs and src/emit.rs, under
"the tenth wave" in docs/open-findings.md, with signatures: apply_within and
Correlation::outside_window for a join across a detached source (and main.rs
passing the windows); the window on Results; Part::via so net_rx and net_tx are
rated KEY rather than IDENTITY (the two vertical lines and the tests/vertical.rs
assertion that flip with it are yours); a filtered part that matched nothing is
a zero, not a missing row. Ground truth for the first: the ignored scenario in
tests/runner.rs (40 matched, 25 outside the window, not 67 matched) -- you do
not own that file, so your handoff says it is ready and the coordinator
un-ignores it at the merge.'

SILENT_WORK='Five places where the tool accepts a request and answers a different question
without saying so, filed under "the tenth wave, stream X" and "stream H": a
pushed-down proc.syscall number filter that drops every call; proc.exec
accepting a filter and applying none; --backend force: walking past missing
probes to fail at attach; mem.faults declaring count and refusing it; and a
per-task counter attached late counting one thread (open one per entry of
/proc/<pid>/task). Check cpu.profile, sched.switches and mem.faults for the
same, and answer stream T'"'"'s request for a profile that follows threads. Each
fix makes the answer right or the refusal say why. Ground truth: each filed
command as a scenario failing on main; for the counter, the ignored test in
tests/runner.rs, which you do not own -- say in your handoff that it is ready.'

DRAINS_WORK='A drained program'"'"'s drain takes a row with lookup-and-delete while the program
is about to add to it, and the addition lands in a freed element -- measured by
the eighth wave, still open. fentry::HistogramProgram solves it with two row
maps and a generation word; put that scheme in src/bpf/map.rs once and move
runqlat, proc_syscall and every other drained program in your files onto it,
keeping each program'"'"'s public methods unchanged (stream S owns the proc
collectors). Ground truth: a known number of events drained on a short tick,
every one in a row or a counted loss, over several runs. Second task:
a_blocking_stack_names_the_call_that_blocked failed in two of three full runs
and passes alone -- find whether it is the machine or the symbolizer racing the
exit, and fix the tool or make the scenario assert what it can promise. Every
row program you touch gets a stack-region and a byte-coverage test (finding 46).'

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
    escalation) WORK="$ESCALATION_WORK" ;;
    debts) WORK="$DEBTS_WORK" ;;
    silent) WORK="$SILENT_WORK" ;;
    drains) WORK="$DRAINS_WORK" ;;
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
