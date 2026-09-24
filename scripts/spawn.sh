#!/usr/bin/env bash
# Prepares an isolated worktree for one work stream and prints its prompt.
#
#   scripts/spawn.sh hotattach
#   cd ../omniscient-hotattach && claude "$(cat ../omniscient-hotattach.prompt)"
#
# Streams own disjoint sets of files (see BRIEF.md), so the worktrees never need
# to rebase against each other and the merge order does not matter. A merge that
# conflicts anywhere other than an append seam means a stream edited something
# it does not own.
#
# This wave divides by roadmap task: the last task of phases 4, 6 and 8, and the
# first of phase 10. The lock is a named file list per stream. The shared layers
# -- join, emit, encode, render, resolve, validate, catalog, and the eBPF layer
# in src/bpf/{btf,insn,map,syscall,attach,usdt,mod}.rs -- belong to nobody this
# wave, and the coordinator is working in them on main while the streams run.
# The two session indexes, examples/README.md and GUIDE.md, belong to the
# coordinator and are held to the sessions on disk by a test, so a session
# added without a row is a red suite rather than a silent gap.
#
# The prompt is also written beside the worktree, so the agent can be started
# in one line:
#
#   cd ../omniscient-<stream> && claude "$(cat ../omniscient-<stream>.prompt)"
set -euo pipefail

declare -A OWNS=(
    [hotattach]="src/collect/mod.rs src/collect/launch.rs src/collect/task.rs src/session.rs src/request.rs tests/runner.rs tests/budget.rs examples/attach-*.yml"
    [network]="src/bpf/request*.rs src/bpf/net_*.rs src/collect/net/** catalog/net.yaml examples/vertical/** examples/vertical-*.yml examples/net-*.yml tests/vertical.rs tests/scenario_net.rs docs/families/net.md"
    [bench]="tests/bench_*.rs examples/bench/** examples/bench-*.yml scripts/bench.sh src/capabilities.rs"
    [triage]="src/triage/** src/cli.rs src/main.rs src/lib.rs tests/triage.rs examples/triage-*.yml man/omn.1"
)
declare -A TITLE=(
    [hotattach]="Stream H -- hot attach"
    [network]="Stream N -- the network rows"
    [bench]="Stream X -- the bench"
    [triage]="Stream T -- triage"
)
declare -A LOG=(
    [hotattach]="docs/scenarios/attach.md"
    [network]="docs/scenarios/vertical.md"
    [bench]="docs/bench.md"
    [triage]="docs/triage.md"
)
declare -A SCENARIO=(
    [hotattach]="tests/runner.rs and tests/budget.rs"
    [network]="tests/vertical.rs and tests/scenario_net.rs"
    [bench]="tests/bench_*.rs (the cheap, non-ignored ones)"
    [triage]="tests/triage.rs"
)

HOTATTACH_WORK='ROADMAP.md phase 6, task 4: a collector added and removed mid-session, without
restarting the run or breaking in-flight joins. DECISIONS.md settled that this
lives inside a session, with no daemon. The run loop is run_all_within in
src/collect/mod.rs; today every request shares one window. Build a request whose
window is its own and inside the run'"'"'s. The surface (a per-request start and
stop in the session file, then the CLI) is your decision: write it down, and
keep the dumped request reading back unchanged. Each of these is a scenario:
every document states its own window; a join across a detached source is not
counted as a loss (src/join.rs is not yours -- file the field it needs); attach
costs and first-tick losses are counted; a preload source attached late is
refused by name before the run. Ground truth: a two-phase program, one source
attached only for the first phase. The escalation of phase 10 is NOT this wave.'

NETWORK_WORK='The two rows of the ROADMAP.md phase 4 tree the tool does not draw yet: net_rx
and net_tx. docs/scenarios/vertical.md left the design: the connection'"'"'s cookie
at the request'"'"'s birth and end, bpf_get_socket_cookie on the socket the server
received on, a second argument on the same two USDT probes. Both rows are KEY by
the roadmap'"'"'s table, and the ladder must say so. The ground truth needs no
root: a client that sends half its request, sleeps a known 30 ms and sends the
rest puts 30 ms in net_rx; a client with a small receive buffer that reads
slowly puts a known wait in net_tx. The rows must still sum to the client'"'"'s
latency within 10 %. src/bpf/mod.rs is not yours: a new program file'"'"'s mod line
goes in your handoff. Also yours: drained: true on catalog/net.yaml.'

BENCH_WORK='ROADMAP.md phase 8: a cost, resolution and loss table for every (source,
backend) this machine serves, and the resolver'"'"'s choices confirmed or corrected.
cost.json and --backend force:<id> exist; the harness around them does not.
Workloads, warmup, >= 20 repetitions round-robin, median and IQR; then the
backend comparison, the resolution sweep (0.5 to 50 us) and the observer effect.
The variance gate comes FIRST and will refuse while three other agents build on
this machine: that refusal is the gate working. Do not produce a table from a
noisy machine; the real measurement runs once, alone, after the merge, and your
handoff gives the one command. Bench tests are #[ignore] and run through
scripts/bench.sh -- never in make check. src/capabilities.rs is yours: the
prefetcher state goes in the environment capture (captured, not controlled), and
stream S'"'"'s dynamic-linking probe request is yours to answer.'

TRIAGE_WORK='ROADMAP.md phase 10, everything but the escalation. The layer sits ABOVE the
engine and calls it like any other user: src/triage/ builds requests, runs them
through the library calls main.rs makes, and reads the results. It never edits
the collector layer, the join engine or the emit layer -- a filed request
instead. The objective comes first: a triage request that does not name one is
refused with the list. Then omn latency (TSA as a question, residue and evidence
carried through), omn saturate (USE, with Little'"'"'s law and every Shortfall
kept), and differential flame graphs from the existing encoders. Ground truth:
cpu-bound, I/O-blocked and lock-contended workloads each named correctly for the
objective asked, and one program run twice with one function'"'"'s cost doubled.
man/omn.1 is yours this wave, including the pending between/decompose request.'

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
    hotattach) WORK="$HOTATTACH_WORK" ;;
    network) WORK="$NETWORK_WORK" ;;
    bench) WORK="$BENCH_WORK" ;;
    triage) WORK="$TRIAGE_WORK" ;;
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
and when" is the long version. The full suite is ~1620 tests and 5 to 10
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
