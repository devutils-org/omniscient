#!/usr/bin/env bash
# Prepares an isolated worktree for one work stream and prints its prompt.
#
#   scripts/spawn.sh io-bpf
#   cd ../omniscient-io-bpf && claude          # paste the prompt it printed
#
# Streams own disjoint sets of files (see BRIEF.md), so the worktrees never need
# to rebase against each other and the merge order does not matter. A merge that
# conflicts anywhere other than a family's registry line means a stream edited
# something it does not own.
#
# Nobody owns the shared eBPF layer -- src/bpf/attach.rs, insn.rs, map.rs,
# btf.rs, syscall.rs -- nor catalog/*.yaml. A stream that needs something from
# either writes the request in docs/open-findings.md and the coordinator lands
# it once.
set -euo pipefail

declare -A OWNS=(
    [io-bpf]="src/bpf/io*.rs src/collect/io/**"
    [net-bpf]="src/bpf/net*.rs src/collect/net/**"
    [mem-bpf]="src/bpf/mem*.rs src/collect/mem/**"
    [proc-bpf]="src/bpf/proc*.rs src/collect/proc/**"
    [sched-bpf]="src/bpf/sched*.rs src/collect/sched/** src/collect/gpu/**"
    [emit-loss]="src/emit.rs src/render.rs"
)
declare -A TITLE=(
    [io-bpf]="Stream A -- io through the block layer"
    [net-bpf]="Stream B -- net through skb and sock"
    [mem-bpf]="Stream C -- mem through the allocator"
    [proc-bpf]="Stream D -- proc through process events"
    [sched-bpf]="Stream E -- sched contention and interrupts, and gpu.job"
    [emit-loss]="Stream F -- the distribution that does not say it is short"
)
declare -A LOG=(
    [io-bpf]="docs/families/io.md"
    [net-bpf]="docs/families/net.md"
    [mem-bpf]="docs/families/mem.md"
    [proc-bpf]="docs/families/proc.md"
    [sched-bpf]="docs/families/sched.md"
    [emit-loss]="docs/families/emit.md"
)
# The scenario files each stream also owns, by the file-name prefix that was the
# lock in wave four and gave eight streams zero conflicts.
declare -A SCENARIOS=(
    [io-bpf]="examples/io-*.yml tests/scenario_io.rs docs/scenarios/io.md"
    [net-bpf]="examples/net-*.yml tests/scenario_net.rs docs/scenarios/net.md"
    [mem-bpf]="examples/mem-*.yml tests/scenario_mem.rs docs/scenarios/mem.md"
    [proc-bpf]="examples/proc-*.yml tests/scenario_proc.rs docs/scenarios/proc.md"
    [sched-bpf]="examples/sched-*.yml examples/gpu-*.yml tests/scenario_sched.rs tests/scenario_gpu.rs docs/scenarios/sched.md docs/scenarios/gpu.md"
    [emit-loss]="tests/aggregation.rs tests/encoders.rs"
)

EBPF_WORK='The catalog already declares every backend you are writing, and since the pass
recorded as finding 36 it resolves onto one this machine serves: your sources
answer "no collector is written for X on backend bpf:raw_tp yet", exit 3. That
is the shape to start from -- run it, see the refusal, then make it a
measurement. You should not need to touch catalog/*.yaml for an attach path.

Attach through attach::raw_tracepoint, not raw_tracepoint_open and not the
classic path. Check the tracepoint prototype before you build instructions:
offcpu.rs refuses a kernel whose sched_switch has a different signature, because
a program written for one shape reads a task_struct* out of a slot holding an
integer and gets a plausible number. Aggregate in the kernel wherever the source
is a distribution -- src/bpf/runqlat.rs is the worked example.

**A source is landed when its scenarios are**, in all four classes:
docs/briefs/wave-4-scenarios.md defines them and GUIDE.md explains them for a
reader -- ground truth, the capability sweep, the loss path, the refusal. A
collector without them is half the work.'

EMIT_WORK='You are the one stream here that adds no source, and finding 37 of
docs/open-findings.md is your whole brief. A hist, a summary or any percentile
built from a run that dropped events or was throttled renders exactly like one
built from a complete run -- and the events it is missing are the slow ones,
because whatever stalled the machine is what stopped the measuring. So every
percentile from a lossy run is a lower bound and nothing says so.

Do not change what is already right. poll cannot produce hist at all, and
CATALOG.md section 12 now says why that is also the coordinated-omission guard.
The five loss kinds are counted and surfaced already. You are adding the step
after: the mark beside the value, the direction of the bias, and "unknown"
rather than a fraction where the count is genuinely unknown -- which is the
answer DECISIONS.md already chose for the throttled case.

Two things must not be conflated: emit::WEIGHT_ATTR means a kernel-built bucket
stands for N measurements and is complete; a run that dropped N events is not.
Stream C is about to become WEIGHT_ATTR heaviest user. Read findings 17 and 18
before touching the weighting path.

Your scenarios substitute the losses rather than choosing a workload -- the io
scenarios show how, by substituting a /proc/diskstats the test wrote. Render in
every format that carries a distribution, and check the negative too: **a clean
run must gain nothing**, or the mark becomes noise and readers learn to ignore
it.'

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

if [[ "$stream" == "emit-loss" ]]; then
    WORK="$EMIT_WORK"
else
    WORK="$EBPF_WORK"
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
  ${SCENARIOS[$stream]}
  ${LOG[$stream]}    -- your handoff, extended rather than replaced

Everything else is read-only, including the shared eBPF layer
(src/bpf/attach.rs, insn.rs, map.rs, btf.rs, syscall.rs) and catalog/*.yaml.
Needing something from either is not a reason to edit it: write it under
"## Needs from another stream" in docs/open-findings.md with the exact signature
you want, and carry on. If two streams need the same thing the coordinator lands
it once, which is what the second wave learned and the third paid for.

Build with \`make\`, not \`cargo build\`: the preload shim is a workspace member
nothing depends on, and the CAP_BPF and CAP_PERFMON file capabilities do not
survive a relink. Both failures are silent -- the tool reports fewer sources as
available, which reads like the machine changed rather than the binary.

${WORK}

You are done when \`make check\` is clean and every source you were given either
collects with its scenarios, or is explained in ${LOG[$stream]}.

Ground truth is the bar: a test that checks the code returns what the code
computed proves nothing. Choose a workload whose property you know independently
-- a known number of I/Os, an injected delay, a program that reports its own
state -- and check the tool against that.

A test that skips is not a test that passes, and a test that asserts what the
machine was doing is worse than either. Three of the last four sessions lost
time to exactly that; findings 11 and 35 have them. Assert the separation rather
than a threshold, and where the property cannot be created here -- no GPU, no
CAP_NET_RAW, a device someone else is using -- skip loudly with the numbers that
show why, and say so in ${LOG[$stream]}.

Commit on ${branch}. Do not merge; the coordinator does that.

PROMPT
