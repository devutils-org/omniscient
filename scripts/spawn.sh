#!/usr/bin/env bash
# Prepares an isolated worktree for one work stream and prints its prompt.
#
#   scripts/spawn.sh join-engine
#   cd ../omniscient-join-engine && claude     # paste the prompt it printed
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
    [join-engine]="src/join.rs src/request.rs src/session.rs src/encode.rs the MatchKind enum in src/catalog.rs tests/join_engine.rs examples/join-*.yml"
    [run-model]="src/collect/mod.rs src/collect/launch.rs src/collect/task.rs src/collect/mem/heap/** tests/runner.rs"
    [fentry]="src/bpf/attach.rs src/bpf/fentry.rs src/bpf/io*.rs src/bpf/net*.rs src/collect/io/** src/collect/net/** catalog/io.yaml catalog/net.yaml examples/io-*.yml examples/net-*.yml tests/scenario_io.rs tests/scenario_net.rs"
    [usdt]="src/bpf/usdt.rs src/collect/runtime/** catalog/runtime.yaml examples/runtime-*.yml tests/scenario_runtime.rs"
)
declare -A TITLE=(
    [join-engine]="Stream J -- the join engine, and the decomposition"
    [run-model]="Stream R -- the run model"
    [fentry]="Stream T -- fentry, and the five sources behind it"
    [usdt]="Stream U -- USDT, and the runtime family"
)
declare -A LOG=(
    [join-engine]="docs/families/join.md"
    [run-model]="docs/families/runner.md"
    [fentry]="docs/families/io.md and docs/families/net.md"
    [usdt]="docs/families/runtime.md"
)

ENGINE_WORK='Three of the seventh wave four crossings independently asked for the same missing
match kind, which is a gap in the model rather than a preference: a `between`
whose evidence is TEMPORAL by construction. Today the only rule that validates
between an allocation and a resident size is `eq` on task, and it reports KEY --
which overstates what crediting-by-recency knows. The evidence level is the
point of the change, not the arithmetic.

The decomposition is the second half, and two streams wrote down the same shape
for it in docs/open-findings.md: a whole, a list of parts, and a **signed
residue that is never rounded away**. Each row carries its kind (sampled,
polled, traced), its evidence, its resolution and its Shortfall. A sampled
profile and a traced duration are not the same kind of number, and the table
has to say which is which.

Read docs/scenarios/cross-memory.md and cross-compute.md first: both compute in
a test what you are about to make the engine say, and their numbers are the
check on your version.

Trap: src/encode.rs renders results and src/render.rs renders requests and
refusals. The sixth wave brief named the wrong one and cost a stream an hour.'

RUNNER_WORK='`CollectError::MixedTargets` refuses any session naming two targets, and every
confounder check has exactly that shape: the machine state is system or cpu, the
measurement is pid or cgroup. The machine crossing had to run a second omn
beside the first, and measured what that costs -- a cpu-target counter also
counts whatever else ran on that core, which under load was 70% of it, and the
machine PSI diluted a 12.5 ms cgroup stall to 6.0 ms.

docs/open-findings.md has the minimum that would serve and the two functions it
needs: any number of machine targets beside at most one workload target, with
only the workload target driving the launch and the stop condition.

Trap: everything here changes what a run *is*, which is what DECISIONS.md open
line on session-versus-daemon is about. Do not settle that line in passing. If a
change would make a run something other than one window and one document, write
it down instead.'

FENTRY_WORK='BPF_PROG_TYPE_TRACING with an attach_btf_id: typed by BTF, cheaper per call than
a kprobe, and it fails to load rather than reading the wrong field.
Btf::tracepoint_signature is the pattern to copy for reading a prototype, and
member_offset descends anonymous members since the seventh wave -- so the fields
these five sources need are reachable.

io.fs glob is yours to decide, and it is the one the seventh wave deliberately
left: converting tracepoint:ext4:* means choosing which filesystem io.fs is
about, and a machine with btrfs and xfs mounted would get the ext4 answer or
nothing. Its first backend is bpf:fentry on vfs_read, which is the right answer
and is now a mechanism you have. Name what you attach to and say why.

Trap: io.syscall and net.socket sit on paths that fire on everything. Aggregate
in the kernel or do not land them -- src/bpf/runqlat.rs is the worked example,
and insn::push_log2_bucket is one shared sequence now, so a histogram you build
agrees with the emit layer by construction rather than by luck.

A source is landed when its scenarios are, in all four classes.'

USDT_WORK='The uprobe half exists. What is missing is reading .note.stapsdt: a base address,
a semaphore address, a provider and a name, plus the argument descriptors that
say where each argument lives at the probe site.

The measurement that frames this, from docs/open-findings.md: a preload shim
costs 132 ns against a uprobe 740 ns for the same event. USDT is not the cheap
path; it is the path that reaches a program you did not launch and cannot
preload into, and one that names its own events rather than having them guessed.
Say that in your log rather than letting a reader assume a probe is free.

You own src/bpf/usdt.rs and read src/bpf/attach.rs without writing it: the
uprobe attach is already there, and parsing an ELF note section is not an attach
kind. If you find you need a change in attach.rs, that is a request to stream T,
not an edit.

Trap: pick a target whose notes you can rely on, and where you cannot, skip
loudly with what the ELF actually contained.'

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
    join-engine) WORK="$ENGINE_WORK" ;;
    run-model) WORK="$RUNNER_WORK" ;;
    fentry) WORK="$FENTRY_WORK" ;;
    usdt) WORK="$USDT_WORK" ;;
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

You are done when \`make check\` is clean and everything you were given either
works with its scenarios, or is explained in ${LOG[$stream]}.

Ground truth is the bar: a test that checks the code returns what the code
computed proves nothing. Choose a workload whose property you know independently
-- a known number of I/Os, an injected delay, a program that reports its own
state -- and check the tool against that.

A test that skips is not a test that passes, and a test that asserts what the
machine was doing is worse than either. This wave is more exposed to that than
any before it: a crossing is two sources on a shared machine at the same time.
Assert the separation rather than a calibrated threshold, carry the reference
own resolution as slack, and where the property cannot be created here, skip
loudly with the numbers that show why, and say so in ${LOG[$stream]}.

Commit on ${branch}. Do not merge; the coordinator does that.

PROMPT
