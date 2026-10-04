"""A process the scheduler sees leave the cpu a known number of times.

    sleeper.py SLEEPS SLEEP_MS

It renames itself `omnref-sleep` first, so a system-wide recorder such as
`perf sched record` can be read for this process alone by its command name,
then sleeps SLEEPS times for SLEEP_MS each: one voluntary switch a sleep,
and one wakeup and one run-queue wait after it.
"""

import sys
import time

with open("/proc/self/comm", "w") as comm:
    comm.write("omnref-sleep")

sleeps, sleep_ms = int(sys.argv[1]), float(sys.argv[2])
for _ in range(sleeps):
    time.sleep(sleep_ms / 1000.0)
