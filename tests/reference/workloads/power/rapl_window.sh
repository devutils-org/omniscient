#!/bin/sh
# Read package 0's RAPL counter on each side of a command, the way a person
# reads the RAPL files by hand: `rapl_window.sh COMMAND...`. The command keeps
# its stdout; the readings go to stderr. energy_uj is root-only on current
# kernels unless made readable (`chmod o+r`); unreadable, `cat` says
# "Permission denied" and the pair skips as refused. The parser
# turns the two readings into joules, one wrap allowed.
d=/sys/class/powercap/intel-rapl:0
max=$(cat "$d/max_energy_range_uj") || exit 1
before=$(cat "$d/energy_uj") || exit 1
"$@"
status=$?
after=$(cat "$d/energy_uj") || exit 1
echo "energy_uj_before=$before energy_uj_after=$after max_energy_range_uj=$max" >&2
exit $status
