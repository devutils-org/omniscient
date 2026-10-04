#!/bin/sh
# Run one bpftrace tool over one workload: under.sh TOOL.bt [ARG...] -- ARGV...
#
# The pairs' reference command is `sh under.sh TOOL --` and the runner appends
# the workload's argv; bpftrace takes the workload as one `-c` string and splits
# it on spaces itself, so the words are joined with spaces: a workload word
# with a space or a quote in it cannot go through here. bpftrace launches the workload and stops
# when it exits, printing its END maps: the tool sees one run from start to end.
# Every tool here is system-wide (none filters by the -c child), which is why
# the workloads name themselves and the parsers key by comm.
set -eu
tool=$1
shift
args=""
while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do
	args="$args $1"
	shift
done
[ "$#" -gt 0 ] && shift
cmd="$*"
# shellcheck disable=SC2086 # $args is the tool's own flags, split on purpose
exec bpftrace $args -c "$cmd" "$tool"
