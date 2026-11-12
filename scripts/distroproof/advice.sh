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

# --- irq.on-the-workload-cpus: a spin pinned to cpu 0, a ping flood from
# another cpu, the network card's interrupts on cpu 0 and then on the last.
nic=$(awk '$2 == "00000000" {print $1; exit}' /proc/net/route)
cpus=$(nproc)
nic_irqs=""
if [ -n "$nic" ] && [ "$cpus" -ge 3 ] && command -v ping >/dev/null; then
    # The card's lines: its MSI vectors where it has them, its INTx line
    # otherwise; a virtio card's are its PCI parent's.
    for dir in "/sys/class/net/$nic/device" "/sys/class/net/$nic/device/.."; do
        [ -d "$dir/msi_irqs" ] && nic_irqs="$nic_irqs $(ls "$dir/msi_irqs")"
        [ -z "$nic_irqs" ] && [ -r "$dir/irq" ] && [ "$(cat "$dir/irq")" != 0 ] && nic_irqs=$(cat "$dir/irq")
    done
fi
kept_irqs=$PWD/irq-kept
: > "$kept_irqs"
for irq in $nic_irqs; do
    file=/proc/irq/$irq/smp_affinity_list
    was=$(cat "$file" 2>/dev/null) || continue
    # A managed vector refuses the write: it is not this pair's to move.
    echo 0 > "$file" 2>/dev/null && echo "$irq $was" >> "$kept_irqs"
done
if [ -s "$kept_irqs" ]; then
    last=$((cpus - 1))
    irqs_to() { awk '{print $1}' "$kept_irqs" | while read -r irq; do echo "$1" > "/proc/irq/$irq/smp_affinity_list"; done; }
    # /proc/net/route spells the gateway as one little-endian hex word.
    set -- $(awk '$2 == "00000000" {print $3; exit}' /proc/net/route | sed 's/../& /g')
    gateway=$(printf '%d.%d.%d.%d' "0x$4" "0x$3" "0x$2" "0x$1")
    taskset -c 1 ping -f -q "$gateway" >/dev/null 2>&1 &
    flood=$!
    sleep 1
    echo "advice: $nic's lines$(awk '{printf " %s", $1}' "$kept_irqs") under a ping flood of $gateway"
    pair irq latency "irqs_to 0" "irqs_to $last" -- taskset -c 0 "$fixture" spin 1 400000000
    kill "$flood" 2>/dev/null
    wait "$flood" 2>/dev/null
    while read -r irq value; do echo "$value" > "/proc/irq/$irq/smp_affinity_list"; done < "$kept_irqs"
else
    echo "advice: no network card line whose affinity can be written (or no ping, or under 3 cpus); the irq pair is not run"
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
