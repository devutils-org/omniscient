#!/bin/sh
# distroproof's `command` for distroproof-advice.toml: the advice rules
# whose setting is the machine's, each run before and after the change, as
# root in a guest that is nobody's machine. Every setting changed is put back.
# The answers come back as advice-guest.json, which tests/advice_truth.rs
# judges; standard output keeps what a person reads.
set -u
omn=/opt/omn/bin/omn
fixture=$PWD/target/dp-advice/$(uname -m)/fixture
answers=$PWD/advice-answers
mkdir -p "$answers"

pair() { # name objective setup-before setup-after -- program...
    name=$1 objective=$2 before=$3 after=$4
    shift 5
    eval "$before"
    "$omn" advise --objective "$objective" --save "$answers/$name-before.json" -- "$@"
    eval "$after"
    "$omn" advise --objective "$objective" --against "$answers/$name-before.json" \
        --save "$answers/$name-after.json" -- "$@"
}

# --- bdi.readahead-off: every backing device at 0, then each put back: a file's readahead is its filesystem's bdi, which is
# the disk's for ext4 and one of its own for btrfs.
kept_bdis=$PWD/bdi-kept
: > "$kept_bdis"
for file in /sys/class/bdi/*/read_ahead_kb; do
    [ -w "$file" ] && echo "$file $(cat "$file")" >> "$kept_bdis"
done
if [ -s "$kept_bdis" ]; then
    all_off() { awk '{print $1}' "$kept_bdis" | while read -r file; do echo 0 > "$file"; done; }
    put_back() { while read -r file value; do echo "$value" > "$file"; done < "$kept_bdis"; }
    "$fixture" readseq "$PWD/data.bin" 64 normal >/dev/null
    pair readahead throughput all_off put_back -- \
        "$fixture" readseq "$PWD/data.bin" 64 normal
    put_back
else
    echo "advice: no writable backing device; the readahead pair is not run"
fi

# --- mm.thp-defrag-stall: huge-page faults on fragmented memory.
thp=/sys/kernel/mm/transparent_hugepage
if [ -w "$thp/defrag" ]; then
    kept_enabled=$(sed 's/.*\[\(.*\)\].*/\1/' "$thp/enabled")
    kept_defrag=$(sed 's/.*\[\(.*\)\].*/\1/' "$thp/defrag")
    echo always > "$thp/enabled"
    # Most of what is free, so that after every other page is given back no
    # 2 MiB block is left free anywhere.
    available=$(awk '/MemAvailable/ {print int($2 / 1024)}' /proc/meminfo)
    hold=$((available * 95 / 100))
    seen() { # what the kernel says it did, for the log
        echo "advice: $1: $(grep -E '^(compact_stall|compact_success|compact_fail|thp_fault_alloc|thp_fault_fallback) ' /proc/vmstat | tr '\n' ' ')"
        echo "advice: $1: buddyinfo $(tr -s " " < /proc/buddyinfo | tr "\n" "|")"
    }
    fragmented() { # the memory fragmented again before each run
        pkill -f "fixture fragment" 2>/dev/null
        sleep 1
        sync; echo 3 > /proc/sys/vm/drop_caches
        "$fixture" fragment "$hold" 120 &
        sleep 6
        seen "fragmented $hold MB"
    }
    pair thp latency "echo always > $thp/defrag; fragmented" \
        "seen 'after the first run'; echo madvise > $thp/defrag; fragmented" -- "$fixture" huge 600
    seen "after the second run"
    pkill -f "fixture fragment" 2>/dev/null
    echo "$kept_enabled" > "$thp/enabled"
    echo "$kept_defrag" > "$thp/defrag"
else
    echo "advice: no writable $thp/defrag; the THP pair is not run"
fi

# One document: {"<pair>": {"before": ..., "after": ...}}.
{
    printf '{'
    first=1
    for before in "$answers"/*-before.json; do
        [ -e "$before" ] || continue
        name=$(basename "$before" -before.json)
        after=$answers/$name-after.json
        [ -e "$after" ] || continue
        [ $first -eq 1 ] || printf ','
        first=0
        printf '"%s":{"before":' "$name"
        cat "$before"
        printf ',"after":'
        cat "$after"
        printf '}'
    done
    printf '}\n'
} > advice-guest.json
echo "advice: $(grep -o '"[a-z]*":{"before"' advice-guest.json | wc -l) pair(s) in advice-guest.json"
