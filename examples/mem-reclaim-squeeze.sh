#!/bin/sh
# The workload for examples/mem-reclaim-squeeze.yml: a program made to reclaim.
#
#   sh examples/mem-reclaim-squeeze.sh
#
# Direct reclaim is the kernel doing the allocator's work inside the allocating
# thread, and it cannot be asked for: it happens when a thread wants memory and
# there is none. Waiting for a machine to run out is not a scenario, so this
# creates the shortage on purpose and in a box -- a cgroup with a thirty-two
# mebibyte limit -- and writes ninety-six mebibytes of page cache through it.
# Page cache rather than anonymous memory deliberately: clean page cache is
# always reclaimable, so the cgroup reclaims continuously and is never killed,
# which an anonymous workload of the same size would be within a second.
#
# It ends by printing, on standard error, two numbers that are the scenario's
# ground truth, read from the kernel rather than from the tool:
#
#   omn-scenario cgroup_id <n>        the cgroup's inode, which is the id the
#                                     kernel gives a BPF program and therefore
#                                     the id on `omn`'s events
#   omn-scenario pgsteal_direct <n>   pages this cgroup reclaimed, from its own
#                                     memory.stat, before and after (`pgsteal`
#                                     on a kernel that does not split it)
#
# `mem.reclaim` reports `pages_reclaimed` off the kernel's own `end` tracepoint.
# The two are counted in different places by different code and must agree.
#
# Exit 64 means the machine cannot host the scenario -- no delegated cgroup with
# the memory controller, or nowhere to write that is not a ramdisk -- and says
# which. That is a machine's limit and not a failure; writing the file to a
# tmpfs would charge the pages to the same cgroup, which turns a reclaim
# scenario into an out-of-memory kill.
set -e

# The cgroup this user may create under: the nearest ancestor of our own that
# already has the memory controller in its subtree_control. A cgroup whose
# parent does not delegate the controller cannot be given a limit.
own=$(sed -n 's/^0:://p' /proc/self/cgroup)
# A process in the root cgroup reads `0::/`, and the trailing slash would carry
# the walk past /sys/fs/cgroup to `/`, whose dirname is itself: forever.
dir=/sys/fs/cgroup${own%/}
cg=
while [ -n "$dir" ] && [ "$dir" != /sys/fs/cgroup ] && [ "$dir" != / ]; do
	case " $(cat "$dir/cgroup.subtree_control" 2>/dev/null) " in
	*" memory "*)
		if mkdir "$dir/omn-reclaim.scope" 2>/dev/null; then
			cg=$dir/omn-reclaim.scope
			break
		fi
		;;
	esac
	dir=$(dirname "$dir")
done
if [ -z "$cg" ]; then
	echo "omn-scenario skip no delegated cgroup with the memory controller under $own" >&2
	exit 64
fi

# Somewhere disk-backed to write. A tmpfs file is memory, charged to the same
# cgroup, and unreclaimable without swap.
work=
for candidate in /var/tmp "$TMPDIR" /tmp; do
	[ -n "$candidate" ] && [ -w "$candidate" ] || continue
	case "$(stat -f -c %T "$candidate")" in
	tmpfs | ramfs) continue ;;
	esac
	work=$candidate
	break
done
if [ -z "$work" ]; then
	rmdir "$cg"
	echo "omn-scenario skip no disk-backed temporary directory to write through" >&2
	exit 64
fi

echo 33554432 > "$cg/memory.max"
file=$work/omn-reclaim-churn.$$
# `pgsteal_direct` where the kernel splits the count (6.x); `pgsteal` where it
# does not (5.15). A cgroup's limit is enforced by the allocating task itself
# -- a memcg has no kswapd -- so on 5.15 the whole count is the direct one.
if grep -q '^pgsteal_direct ' "$cg/memory.stat"; then
	counter=pgsteal_direct
else
	counter=pgsteal
fi
steal() { sed -n "s/^$counter //p" "$cg/memory.stat"; }
before=$(steal)

# In a child, because the shell that reads memory.stat afterwards and removes
# the cgroup must not be inside it. `$$` in a subshell is the parent's pid,
# which would move this shell in and leave the cgroup busy at the end.
sh -c 'echo $$ > "$1/cgroup.procs" || exit 1
	dd if=/dev/zero of="$2" bs=1M count=96 status=none
	cat "$2" > /dev/null' _ "$cg" "$file" || true

rm -f "$file"
after=$(steal)
echo "omn-scenario cgroup_id $(stat -c %i "$cg")" >&2
echo "omn-scenario pgsteal_direct $((after - before))" >&2
rmdir "$cg"
