#!/bin/sh
# The workload for examples/mem-kernel-storm.yml: more rows than the map holds.
#
#   sh examples/mem-kernel-storm.sh
#
# `mem.kmalloc` counts in the kernel, in a hash map of
# `(task, cgroup, size bucket, direction)` rows that userspace drains every
# hundred milliseconds. The map is bounded -- four thousand and ninety-six rows
# -- and a bound nothing has ever reached is a loss path nobody has exercised.
# This reaches it.
#
# The dimensions of a row are a task's identity and a size, and a single task
# produces at most a couple of dozen rows however hard it allocates. So the only
# way to fill the map is *tasks*: this forks about three thousand short-lived
# processes from four shells at once, and each of them allocates in several size
# classes on its way through `fork`, `execve` and `exit`. Between two drains
# that is more distinct rows than the map holds, the kernel-side update fails,
# and the program counts what it could not record.
#
# **The `execve` is not decoration.** A bare subshell is a new task and does
# almost nothing in the slab allocator: measured here, three times as many of
# them filled a third as much of the map. What makes a row is a task allocating
# in a size class nobody else is in, and loading a program is what spreads one
# task across several.
#
# A run that hits the bound must say so: the allocations behind a row the map
# had no room for are counted nowhere else at all -- the helper returns an error
# in the kernel and the event is simply gone. `omn` reports them as `dropped`,
# with the capacity in the reason.
#
# On a machine that forks more slowly than this one the bound may not be
# reached, and that is a fact about the machine rather than a failure. The
# scenario says so with the numbers: what the peak row count was, and what the
# capacity is.
SHELLS=${1:-4}
ROUNDS=${2:-350}

storm() {
	i=0
	while [ "$i" -lt "$ROUNDS" ]; do
		( : ) &
		/bin/true &
		i=$((i + 1))
	done
	wait
}

k=0
while [ "$k" -lt "$SHELLS" ]; do
	storm &
	k=$((k + 1))
done
wait
