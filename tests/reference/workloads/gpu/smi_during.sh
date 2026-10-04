#!/bin/sh
# Runs a workload and samples nvidia-smi beside it, over the same run.
#
#   smi_during.sh gpu   ARGV...   nvidia-smi --query-gpu, every 200 ms
#   smi_during.sh dmon  ARGV...   nvidia-smi dmon -s puc, every second
#   smi_during.sh apps  ARGV...   nvidia-smi --query-compute-apps, every
#                                 200 ms, the workload's own pid only
#
# nvidia-smi does not launch a program: it reads the device while something
# else runs. This wrapper is that something else, so that the samples cover
# the workload's lifetime and nothing after it.
#
# The samples go to standard error, beside the workload's own notes, never to
# standard output: `omn` can launch this wrapper (`omn trace ...
# --program-stderr FILE -- smi_during.sh gpu ARGV...`), so that omn and
# nvidia-smi observe the same run, and omn's standard output is its JSON. The
# parsers keep only the lines shaped as samples. The exit status is the
# workload's.
set -u
mode=$1
shift
"$@" &
workload=$!
case $mode in
gpu)
    nvidia-smi --query-gpu=power.draw,clocks.sm,temperature.gpu,utilization.gpu,memory.used \
        --format=csv,noheader,nounits -lms 200 >&2 &
    sampler=$!
    ;;
dmon)
    nvidia-smi dmon -i 0 -s puc -d 1 >&2 &
    sampler=$!
    ;;
apps)
    (
        while kill -0 "$workload" 2>/dev/null; do
            nvidia-smi --query-compute-apps=pid,used_memory --format=csv,noheader,nounits |
                grep "^$workload," >&2
            sleep 0.2
        done
    ) </dev/null &
    sampler=$!
    ;;
*)
    echo "smi_during.sh: unknown mode '$mode' (gpu, dmon or apps)" >&2
    kill "$workload"
    exit 2
    ;;
esac
wait "$workload"
status=$?
kill "$sampler" 2>/dev/null
wait "$sampler" 2>/dev/null
exit "$status"
