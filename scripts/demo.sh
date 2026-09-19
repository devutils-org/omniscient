#!/usr/bin/env bash
# Runs the demonstrations this repository ships, showing the command and its
# output. Nothing here is staged: each block runs the real binary against this
# machine, and a block that cannot run on it says why, which is half the point.
#
#   make demo               the guided tour: eight questions, about seven seconds
#   scripts/demo.sh --all   the tour and then every session, about ninety seconds
#   scripts/demo.sh mem     one family's sessions
#   scripts/demo.sh --list  what there is, without running any of it
#
# See GUIDE.md for what each session shows and why.
#
# Nothing is written outside the scratch directory.
set -u

MODE=tour
FAMILY=""
case "${1:-}" in
    "")            MODE=tour ;;
    --all)         MODE=all ;;
    --list)        MODE=list ;;
    --tour)        MODE=tour ;;
    -h|--help)
        sed -n "2,13p" "$0" | sed "s/^# \{0,1\}//"
        exit 0 ;;
    -*)
        echo "unknown option: $1 -- try --help" >&2
        exit 2 ;;
    *)             MODE=family; FAMILY="$1" ;;
esac

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
# The `name:` a session gives itself, unquoted, for the listing.
session_name() {
    sed -n 's/^name: *//p' "$1" | head -1 | sed -e 's/^"//' -e 's/"$//' -e "s/^'//" -e "s/'$//"
}

sessions_of() {
    # The family prefix is how a session names what it is about; the five that
    # carry none belong to no family and are listed under `general`.
    case "$1" in
        general) ls examples/allocation-cost.yml examples/web-latency.yml \
                    examples/filters-and-formats.yml examples/saturation.toml 2>/dev/null ;;
        *)       ls examples/"$1"-*.yml 2>/dev/null ;;
    esac
}

FAMILIES="cpu mem io net sched power gpu proc runtime general"

if [ "$MODE" = list ] || [ "$MODE" = tour ]; then
    section "9. Every session this repository ships" \
"Each one is a measurement written down, and each is explained in GUIDE.md.
Run one with \`omn -f <file>\`, a family with \`scripts/demo.sh <family>\`, or
all of them with \`scripts/demo.sh --all\`."
    for family in $FAMILIES; do
        found="$(sessions_of "$family")"
        [ -z "$found" ] && continue
        printf '\n  %s%s%s\n' "$bold" "$family" "$reset"
        for session in $found; do
            name="$(session_name "$session")"
            printf '    %-34s %s\n' "$(basename "$session")" "${name:-—}"
        done
    done
fi

if [ "$MODE" = all ] || [ "$MODE" = family ]; then
    if [ "$MODE" = family ]; then
        found="$(sessions_of "$FAMILY")"
        if [ -z "$found" ]; then
            echo "no sessions for \`$FAMILY\` -- families: $FAMILIES" >&2
            exit 2
        fi
        section "The \`$FAMILY\` sessions"
        for session in $found; do run "$BINARY" -f "$session"; done
        printf '\n%sdone.%s\n' "$bold" "$reset"
        exit 0
    fi
    section "9. Every session this repository ships"
    for family in $FAMILIES; do
        for session in $(sessions_of "$family"); do
            case "$(basename "$session")" in
                allocation-cost.yml) continue ;;   # shown in full above
            esac
            run "$BINARY" -f "$session"
        done
    done
fi

# ---------------------------------------------------------------------------
section "10. And one that is written for a machine other than this one" \
"Most of these sources have no collector yet. The session still validates,
because validation asks the catalog and not the machine."

run "$BINARY" validate -f examples/web-latency.yml

printf '\n%sdemo complete.%s\n' "$bold" "$reset"
