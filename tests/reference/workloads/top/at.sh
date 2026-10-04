#!/bin/sh
# Runs a pid-taking tool at a known moment of a workload, for the `top`
# family's `run: attach` pairs (catalog/references/top.yaml):
#
#     at.sh WHEN PID TOOL ARGS...
#
# WHEN is a phase the workload names itself (`hold-ready`, `hold-done`:
# hold.py writes it to /proc/<pid>/comm) or a number of seconds to sleep
# first. The runner starts the tool and omn together, the workload stopped,
# and lets the workload go `settle_ms` later; a tool that read at once would
# read a stopped shell. This waits, then `exec`s the tool, so its output and
# its exit status are the tool's own. A workload that ends before naming the
# phase fails here, saying so.
when=$1 pid=$2
shift 2
case $when in
    [0-9]*) sleep "$when" ;;
    *)
        until [ "$(cat "/proc/$pid/comm" 2>/dev/null)" = "$when" ]; do
            if ! kill -0 "$pid" 2>/dev/null; then
                echo "at.sh: process $pid ended without naming itself $when" >&2
                exit 1
            fi
            sleep 0.02
        done
        ;;
esac
exec "$@"
