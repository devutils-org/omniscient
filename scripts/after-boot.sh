#!/bin/sh
# The settings the reference pairs and the bench want, which a reboot resets
# (docs/dev/STATE.md, "What the user grants"). Run once after each boot, as yourself:
# sudo asks for the password once.
#
#   scripts/after-boot.sh
#
# - tracefs readable: the classic `bpf:tp` road, `perf-trace-count` and the
#   `sqlite3-durability` case read tracepoint ids under it (mode 700 at boot).
# - The package energy counter readable: the `energy-rapl-files` pair
#   (root-only since the counter was shown to leak what the machine computes).
# - eBPF program statistics on: `cost.json` then weighs what a collector's
#   programs spend inside the target, which it otherwise says is unknown.
set -eu

sudo mount -o remount,mode=755 /sys/kernel/tracing
echo "after-boot: /sys/kernel/tracing is $(stat -c %a /sys/kernel/tracing)"

for counter in /sys/class/powercap/intel-rapl:*/energy_uj; do
    [ -e "$counter" ] || continue
    sudo chmod o+r "$counter"
    echo "after-boot: $counter readable"
done

sudo sysctl -q kernel.bpf_stats_enabled=1
echo "after-boot: kernel.bpf_stats_enabled = $(cat /proc/sys/kernel/bpf_stats_enabled)"
