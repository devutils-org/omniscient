#!/bin/sh
# Run one bpftrace tool over one workload: under.sh TOOL.bt [ARG...] -- ARGV...
#
# The pairs' reference command is `sh under.sh TOOL --` and the runner appends
# the workload's argv; bpftrace takes the workload as one `-c` string and splits
# it on spaces itself, so the words are joined with spaces: a workload word
# with a space or a quote in it cannot go through here. bpftrace launches the workload and stops
# when it exits, printing its END maps: the tool sees one run from start to end.
# Every tool here is system-wide (none filters by the -c child), which is why
# the workloads name themselves and the parsers key by comm. `$SUDO` is the
# runner's `sudo -n` (`reference.sudo`), so bpftrace alone runs as root.
set -eu
tool=$1
shift
# The script where this distribution installs it: Arch and Fedora under
# /usr/share/bpftrace/tools, Debian and Ubuntu in /usr/sbin (found with
# distroproof's ref-debian-12, where every path the pairs named was absent).
if [ ! -e "$tool" ]; then
	name=${tool##*/}
	for dir in /usr/share/bpftrace/tools /usr/sbin /usr/share/doc/bpftrace/tools /usr/local/share/bpftrace/tools; do
		[ -e "$dir/$name" ] && { tool="$dir/$name"; break; }
	done
	# Alpine ships the scripts apart (bpftrace-tools): a machine without
	# them refuses the pair rather than handing bpftrace a missing path.
	[ -e "$tool" ] || { echo "under.sh: the bpftrace tool script $name is not installed here" >&2; exit 1; }
fi
args=""
while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do
	args="$args $1"
	shift
done
[ "$#" -gt 0 ] && shift
cmd="$*"
# shellcheck disable=SC2086 # $args is the tool's own flags, split on purpose
exec ${SUDO:-} bpftrace $args -c "$cmd" "$tool"
