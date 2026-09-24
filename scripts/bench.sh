#!/usr/bin/env bash
# The self-benchmark: ROADMAP.md phase 8. docs/bench.md says what each step
# measures and what it found.
#
#   scripts/bench.sh              everything, in order, stopping at the gate
#   scripts/bench.sh gate         the variance gate on this machine as it is
#   scripts/bench.sh loaded       the gate on this machine loaded on purpose
#   scripts/bench.sh counts       forced backends against exact counts
#   scripts/bench.sh resolution   the 0.5-50 us resolution sweep
#   scripts/bench.sh observer     the observer effect
#   scripts/bench.sh backends     the cost table and the resolver's verdicts
#   scripts/bench.sh --smoke ...  three repetitions: the mechanics, never a table
#
# Run it alone. The gate refuses a machine where anything else is building,
# which is the gate working: a table from a noisy machine is worse than none.
# The bench is not part of `make check` -- its tests are #[ignore] -- because
# a suite that measures for half an hour is what the testing rule prevents.
#
# It measures the release build, because a debug build's collector is
# unoptimised Rust and its cost is not the tool's. `make release` builds the
# preload shim beside it and grants the capabilities, which a relink drops.

set -euo pipefail
cd "$(dirname "$0")/.."

smoke=0
steps=()
for arg in "$@"; do
    case "$arg" in
        --smoke) smoke=1 ;;
        gate|loaded|counts|resolution|observer|backends) steps+=("$arg") ;;
        -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
        *) echo "bench: unknown step \`$arg\`" >&2; exit 2 ;;
    esac
done
# `loaded` is not in the default sequence: it proves the gate refuses, and a
# real run needs the gate to pass. It is its own step, run on purpose.
[ ${#steps[@]} -eq 0 ] && steps=(gate counts resolution observer backends)

make release
export OMN_BENCH_BIN="$PWD/target/release/omn"
if ! getcap "$OMN_BENCH_BIN" 2>/dev/null | grep -q cap_bpf; then
    if [ "$smoke" = 1 ]; then
        # The mechanics do not depend on the build; the prices do. A smoke run
        # takes the debug binary that `make` can grant, and its documents say
        # DEBUG build in their first line.
        make
        export OMN_BENCH_BIN="$PWD/target/debug/omn"
        echo "bench: (smoke) the release build has no capabilities; measuring the debug build"
    else
        echo "bench: $OMN_BENCH_BIN has no capabilities; the eBPF and per-cpu perf rows" >&2
        echo "       would be missing from the table, so the bench stops here." >&2
        echo "       grant them: sudo setcap cap_bpf,cap_perfmon+ep $OMN_BENCH_BIN" >&2
        exit 1
    fi
fi
if [ "$smoke" = 1 ]; then
    export OMN_BENCH_REPETITIONS=3
    echo "bench: SMOKE RUN -- three repetitions, the gate's refusal recorded rather than obeyed;"
    echo "       nothing written is a measurement"
fi
echo "bench: load average $(cut -d' ' -f1-3 /proc/loadavg) on $(nproc) cpus"

# A smoke run goes on past a refusal, which is the one thing it is for; a real
# run stops at the first, because every later step stands behind the gate.
run() {
    local file=$1 test=$2
    cargo test --test "$file" "$test" -- --ignored --exact --test-threads=1 --nocapture || {
        [ "$smoke" = 1 ] && echo "bench: (smoke) \`$test\` refused or failed; going on"
    }
}

for step in "${steps[@]}"; do
    echo "bench: -- $step"
    case "$step" in
        gate)       run bench_harness the_gate_on_this_machine ;;
        loaded)     run bench_harness the_gate_refuses_a_machine_loaded_on_purpose ;;
        counts)     run bench_backends the_forced_backends_count_what_the_workload_did ;;
        resolution) run bench_resolution every_timestamping_backend_resolves_fifty_microseconds_and_the_sweep_says_how_far_below ;;
        observer)
            run bench_observer the_observer_metric_flags_a_sequence_serialized_on_purpose
            run bench_observer the_collectors_on_an_asynchronous_sequence ;;
        backends)   run bench_backends every_served_backend_is_costed_and_the_resolver_is_judged_by_it ;;
    esac
done
echo "bench: done; the documents are under target/bench/"
