"""Moves itself across every allowed cpu a fixed number of times: each
affinity change forces one migration, so the count follows the argument."""

import os
import sys

n = int(sys.argv[1]) if len(sys.argv) > 1 else 500
cpus = sorted(os.sched_getaffinity(0))
if len(cpus) < 2:
    pass
    sys.exit(0)
for i in range(n):
    os.sched_setaffinity(0, {cpus[i % len(cpus)]})
    # Run long enough on the new cpu for the move to have happened.
    for _ in range(200):
        pass
os.sched_setaffinity(0, set(cpus))
