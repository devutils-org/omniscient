"""A known number of two system calls nothing else in the interpreter makes.

    syscalls.py CALLS SLEEPS SLEEP_MS

`getppid` CALLS times, then `clock_nanosleep` SLEEPS times for SLEEP_MS each:
a count every tool must agree on exactly, and a latency the kernel fixes
from below, so a tracer's own overhead shows as the difference.
"""

import os
import sys
import time

calls, sleeps, sleep_ms = int(sys.argv[1]), int(sys.argv[2]), float(sys.argv[3])
for _ in range(calls):
    os.getppid()
for _ in range(sleeps):
    time.sleep(sleep_ms / 1000.0)
