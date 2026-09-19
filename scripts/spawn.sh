#!/usr/bin/env bash
# Prepares an isolated worktree for one work stream and prints its prompt.
#
#   scripts/spawn.sh bpf-attach
#   cd ../omniscient-bpf-attach && claude      # paste the prompt it printed
#
# Streams own disjoint sets of files (see BRIEF.md), so the worktrees never need
# to rebase against each other and the merge order does not matter. A merge that
# conflicts anywhere other than a family's registry line means a stream edited
# something it does not own.
set -euo pipefail

declare -A OWNS=(
    [cpu-scenarios]="examples/cpu-*.yml tests/scenario_cpu.rs docs/scenarios/cpu.md"
    [mem-scenarios]="examples/mem-*.yml tests/scenario_mem.rs docs/scenarios/mem.md"
    [io-scenarios]="examples/io-*.yml tests/scenario_io.rs docs/scenarios/io.md"
    [net-scenarios]="examples/net-*.yml tests/scenario_net.rs docs/scenarios/net.md"
    [sched-scenarios]="examples/sched-*.yml tests/scenario_sched.rs docs/scenarios/sched.md"
    [power-scenarios]="examples/power-*.yml tests/scenario_power.rs docs/scenarios/power.md"
    [gpu-scenarios]="examples/gpu-*.yml tests/scenario_gpu.rs docs/scenarios/gpu.md"
    [proc-scenarios]="examples/proc-*.yml examples/runtime-*.yml tests/scenario_proc.rs tests/scenario_runtime.rs docs/scenarios/proc.md docs/scenarios/runtime.md"
)
declare -A TITLE=(
    [cpu-scenarios]="Stream A -- cpu scenarios"
    [mem-scenarios]="Stream B -- mem scenarios"
    [io-scenarios]="Stream C -- io scenarios"
    [net-scenarios]="Stream D -- net scenarios"
    [sched-scenarios]="Stream E -- sched scenarios"
    [power-scenarios]="Stream F -- power scenarios"
    [gpu-scenarios]="Stream G -- gpu scenarios"
    [proc-scenarios]="Stream H -- proc and runtime scenarios"
)
declare -A LOG=(
    [cpu-scenarios]="docs/scenarios/cpu.md"
    [mem-scenarios]="docs/scenarios/mem.md"
    [io-scenarios]="docs/scenarios/io.md"
    [net-scenarios]="docs/scenarios/net.md"
    [sched-scenarios]="docs/scenarios/sched.md"
    [power-scenarios]="docs/scenarios/power.md"
    [gpu-scenarios]="docs/scenarios/gpu.md"
    [proc-scenarios]="docs/scenarios/proc.md"
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

Everything else is read-only to you -- including all of src/. A scenario that
finds a defect has done its job: write it under a "## Defects found" heading in
${LOG[$stream]}, with the session that shows it and the output it produced, and
keep going. A scenario suite that quietly worked around three bugs has measured
nothing.

examples/README.md and scripts/demo.sh belong to the coordinator, who
regenerates both from what lands. Do not edit them.

Build with \`make\`, not \`cargo build\`: the preload shim is a workspace member
nothing depends on, and the CAP_BPF and CAP_PERFMON file capabilities do not
survive a relink. Both failures are silent -- the tool reports fewer sources as
available, which reads like the machine changed rather than the binary.

Cover all four scenario classes named in BRIEF.md -- the capability sweep, the
backend comparison, the loss path and the refusal. Each is a different kind of
lie and each catches a different one.

You are done when \`make check\` is clean and every scenario either checks its
answer against a property the workload chose, or says in ${LOG[$stream]} what
it cannot verify and why.

${LOG[$stream]} is your deliverable as much as the tests are: it is what a
person reads to know what to measure in this family, and it is what the
coordinator reads instead of your diff.

Ground truth is the bar, and in this wave it is the whole job: a scenario that
checks the tool returned what the tool computed proves nothing. The workload
chooses the number -- forty megabytes touched, a hundred million instructions
retired, three hundred milliseconds asleep -- and the scenario checks the tool
found it.

A test that skips is not a test that passes. Where this machine cannot serve a
scenario, the skip must name what is missing, and the scenario must still be in
examples/ and in your document so a machine that can serve it runs it.

Commit on ${branch}. Do not merge; the coordinator does that.

PROMPT
