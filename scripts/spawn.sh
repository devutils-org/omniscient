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
    [fix-runner]="src/main.rs src/cli.rs src/collect/mod.rs src/error.rs"
    [fix-catalog]="catalog/*.yaml src/validate.rs"
    [fix-emit]="src/emit.rs src/filter.rs src/encode.rs src/render.rs"
    [fix-stacks]="src/stack.rs src/collect/cpu/** src/collect/sched/**"
    [fix-mem]="src/collect/mem/** src/collect/proc/** src/collect/power/**"
    [fix-net]="src/collect/net/** src/collect/io/** src/collect/gpu/** src/collect/runtime/**"
)
declare -A TITLE=(
    [fix-runner]="Stream A -- exit codes and the runner"
    [fix-catalog]="Stream B -- what the catalog claims"
    [fix-emit]="Stream C -- emit, filter, render"
    [fix-stacks]="Stream D -- stacks, and the two families that produce them"
    [fix-mem]="Stream E -- mem, proc and power readings"
    [fix-net]="Stream F -- net, io, gpu and runtime readings"
)
declare -A LOG=(
    [fix-runner]="docs/fixes/runner.md"
    [fix-catalog]="docs/fixes/catalog.md"
    [fix-emit]="docs/fixes/emit.md"
    [fix-stacks]="docs/fixes/stacks.md"
    [fix-mem]="docs/fixes/mem.md"
    [fix-net]="docs/fixes/net.md"
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
  plus ${LOG[$stream]}, and two things that belong to the defects you fix:
    - any assertion in tests/scenario_*.rs that names a defect you fixed
    - the entry for that defect in docs/scenarios/<family>.md, marked fixed
      with its reproduction left in place

Everything else is read-only. Needing something from another stream's files is
not a reason to edit them: write it under "## Needs from another stream" in
docs/open-findings.md with the exact signature you want, and carry on.

Build with \`make\`, not \`cargo build\`: the preload shim is a workspace member
nothing depends on, and the CAP_BPF and CAP_PERFMON file capabilities do not
survive a relink. Both failures are silent -- the tool reports fewer sources as
available, which reads like the machine changed rather than the binary.

Start from the defects themselves: docs/scenarios/<family>.md, under
"## Defects found". Each carries the session that shows it and the output it
produced. BRIEF.md groups them and tells you which are yours; it does not
restate them.

**A defect is fixed when the scenario that found it asserts the new behaviour
and would fail against the old.** A fix with no test is a claim. Some of what is
listed will turn out to be something else once you look -- say so in
${LOG[$stream]} rather than forcing a fix onto a diagnosis that was wrong.

You are done when \`make check\` is clean and every defect you were given is
either fixed with its scenario updated, or explained in ${LOG[$stream]}.

Ground truth is the bar: a test that checks the code returns what the code
computed proves nothing. The scenarios you are fixing already hold the
independent reference -- a kernel counter, an injected delay, a program that
reports its own state -- so keep it and change what is asserted about it.

A test that skips is not a test that passes. Where this machine cannot serve a
fix -- no GPU, no CAP_NET_RAW, no capabilities -- say so in ${LOG[$stream]}
rather than claiming a fix you could not exercise.

Commit on ${branch}. Do not merge; the coordinator does that.

PROMPT
