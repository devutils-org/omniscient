"""A process the scheduler moves between cpus a known number of times.

    migrate.py MOVES

It renames itself `omnref-migrate` first, so a system-wide recorder such as
`perf sched record` can be read for this process alone by its command name,
then pins itself alternately to the first and the second cpu it may run on,
MOVES times. Each pin to the cpu it is not on moves it there before the call
returns: one `sched_migrate_task` a move, whatever the rest of the machine
does. The few migrations around it (start, exit) are the scheduler's own.
"""

import os
import sys

with open("/proc/self/comm", "w") as comm:
    comm.write("omnref-migrate")

moves = int(sys.argv[1])
cpus = sorted(os.sched_getaffinity(0))
if len(cpus) < 2:
    sys.exit("migrate.py needs two cpus")
a, b = cpus[0], cpus[1]
os.sched_setaffinity(0, {a})
for i in range(moves):
    os.sched_setaffinity(0, {b if i % 2 == 0 else a})
