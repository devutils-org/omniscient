#!/bin/sh
# Run a command while cpu 1 is woken every millisecond: `cpu1_ticks.sh
# COMMAND...`. The ticks start 0.1 s before the command and last 3 s, so they
# cover both observers' windows from their first read to their last: omn's
# opens about 0.06 s after the runner starts both sides, the command's 0.1 s
# after. The ticker writes nothing, and its stdout and stderr are not the
# command's, so the runner does not wait on it past the command.
python3 tests/reference/workloads/power/tick.py 3 1 >/dev/null 2>&1 </dev/null &
sleep 0.1
exec "$@"
