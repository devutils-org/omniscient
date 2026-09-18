#!/usr/bin/env bash
# Runs every demonstration this repository ships, showing the command and its
# output. Nothing here is staged: each block runs the real binary against this
# machine, and a block that cannot run on it says why, which is half the point.
#
#   make demo
#
# Takes about a minute. Nothing is written outside the scratch directory.
set -u

BINARY="${BINARY:-target/debug/omn}"
SCRATCH="$(mktemp -d)"
trap 'rm -rf "$SCRATCH"' EXIT

bold=''; dim=''; reset=''
if [ -t 1 ]; then bold=$'\033[1m'; dim=$'\033[2m'; reset=$'\033[0m'; fi

section() {
    printf '\n%s%s%s\n' "$bold" "$1" "$reset"
    printf '%s%s%s\n' "$dim" "$(printf '─%.0s' $(seq 1 ${#1}))" "$reset"
    [ $# -gt 1 ] && printf '%s\n' "$2"
    return 0
}

run() {
    # Echo the command the way it would be typed: an argument holding a space
    # or a shell metacharacter is quoted, or `--filter "bytes > 4096"` reads
    # like a redirect.
    local shown=''
    for argument in "$@"; do
        case "$argument" in
            *[!A-Za-z0-9_./=:@-]*) shown="$shown \"$argument\"" ;;
            *) shown="$shown $argument" ;;
        esac
    done
    printf '\n%s$%s%s\n' "$dim" "$shown" "$reset"
    # A demonstration must never hang, and a block that fails is still part of
    # what is being shown.
    timeout 40 "$@" 2>&1 | sed 's/^/    /'
    return 0
}

if [ ! -x "$BINARY" ]; then
    echo "no binary at $BINARY -- run \`make\` first" >&2
    exit 1
fi

cat <<'INTRO'
Omniscient describes a measurement along five axes -- source, target, mode, emit
and keys -- and refuses the combinations that cannot mean anything.

Everything below is this binary, on this machine, now.
INTRO

# ---------------------------------------------------------------------------
section "1. What this machine can serve" \
"Three different questions, kept apart: what the machine offers (HERE), what
this build can measure (COLLECTOR), and what is neither."

run "$BINARY" list mem
run "$BINARY" list mem.rss

# ---------------------------------------------------------------------------
section "2. Measuring a program that is running" \
"The program is held at a gate until the collectors are attached, so the
measurement starts before its first instruction and ends when it does."

run "$BINARY" mem.rss --emit timeline --mode poll@100ms -- \
    python3 -c "import time; b=[]; [(b.append(bytearray(8<<20)), time.sleep(0.1)) for _ in range(5)]"

# ---------------------------------------------------------------------------
section "3. The number no single source can give" \
"malloc() allocates nothing: pages become resident on first write. So an
allocation profile and a memory curve never line up, and neither answers which
allocation cost memory. Joining them by the range each allocation owns does."

run "$BINARY" -f examples/allocation-cost.yml

# ---------------------------------------------------------------------------
section "4. Talking to the kernel directly" \
"Counters, netlink, RAPL and a perf ring buffer -- no perf(1), no bpftrace, no
second clock in the middle."

run "$BINARY" cpu.instructions -m count -e summary -- /bin/sh -c 'i=0; while [ $i -lt 20000 ]; do i=$((i+1)); done'
run "$BINARY" net.tcp.rtt --system -m poll@500ms -d 1s -e summary
run "$BINARY" power.energy --system -m poll@500ms -d 1500ms -e timeline
run "$BINARY" mem.faults --mode trace --emit rate -- python3 -c "b = bytearray(32<<20)"

# ---------------------------------------------------------------------------
section "5. Who unblocked whom" \
"The wakeup chain is the only way to attribute latency across a block without
instrumenting the application. It needs CAP_BPF and CAP_PERFMON; without them
the run is refused by name rather than served by something that answers a
different question."

run "$BINARY" sched.wakeup --mode trace --emit raw -- /bin/sh -c 'sleep 0.05'

# ---------------------------------------------------------------------------
section "6. What it refuses, and why" \
"A refusal is a result. Each of these fails before anything is attached, and
says what the source does accept."

run "$BINARY" mem.bandwidth --pid 1 -m count -e rate
run "$BINARY" mem.rss --pid 1 -m trace -e raw
run "$BINARY" io.latency --system -m trace -e hist --filter "bites > 4096"
run "$BINARY" cpu.profile --pid 1 -m sample -e stack --format folded

# ---------------------------------------------------------------------------
section "7. The record it keeps beside the numbers" \
"Two measurements taken through different backends are not comparable, and
without this file nobody can tell that they were."

run "$BINARY" proc.fd --pid $$ -m poll@100ms -d 300ms -e summary --out "$SCRATCH/run"
printf '\n%s$ ls %s/run%s\n' "$dim" "$SCRATCH" "$reset"
ls "$SCRATCH/run" | sed 's/^/    /'
printf '\n%s$ jq -c "{backend, environment}" %s/run/resolved.json%s\n' "$dim" "$SCRATCH" "$reset"
python3 - "$SCRATCH/run/resolved.json" <<'PY' | sed 's/^/    /'
import json, sys
d = json.load(open(sys.argv[1]))[0]
e = d["environment"]
print("backend:", d["backend"]["id"], "rank", d["backend"]["rank"])
for k in ("kernel_release", "arch", "clocksource", "cpu_governor",
          "transparent_hugepage", "smt", "perf_event_paranoid"):
    if e.get(k) is not None:
        print(f"{k}: {e[k]}")
PY

# ---------------------------------------------------------------------------
section "8. The same request, several ways out" \
"A dumped request is a session file the tool reads back, so a measurement
prototyped on the command line is frozen into a file without being retyped."

run "$BINARY" net.bytes --iface lo -m count -d 600ms -e rate --format json
run "$BINARY" net.bytes --iface lo -m count -d 600ms -e rate --dump-request --format yaml

# ---------------------------------------------------------------------------
section "9. Every session this repository ships"

for session in examples/*.yml examples/*.toml; do
    case "$(basename "$session")" in
        allocation-cost.yml) continue ;;   # shown in full above
    esac
    run "$BINARY" -f "$session"
done

# ---------------------------------------------------------------------------
section "10. And one that is written for a machine other than this one" \
"Most of these sources have no collector yet. The session still validates,
because validation asks the catalog and not the machine."

run "$BINARY" validate -f examples/web-latency.yml

printf '\n%sdemo complete.%s\n' "$bold" "$reset"
