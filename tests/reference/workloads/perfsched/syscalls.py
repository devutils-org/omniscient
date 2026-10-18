"""A known number of two system calls nothing else in the interpreter makes.

    syscalls.py CALLS SLEEPS SLEEP_MS

`getppid` CALLS times, then `clock_nanosleep` SLEEPS times for SLEEP_MS each:
a count every tool must agree on exactly, and a latency the kernel fixes
from below, so a tracer's own overhead shows as the difference.
"""

import ctypes
import os
import sys

calls, sleeps, sleep_ms = int(sys.argv[1]), int(sys.argv[2]), float(sys.argv[3])
for _ in range(calls):
    os.getppid()


class Timespec(ctypes.Structure):
    _fields_ = [("tv_sec", ctypes.c_long), ("tv_nsec", ctypes.c_long)]


# clock_nanosleep itself, through libc: `time.sleep` makes it from Python 3.11
# on and `select` before (Rocky 9's 3.9, found with distroproof), so the
# syscall the pairs count would depend on the interpreter.
libc = ctypes.CDLL(None, use_errno=True)
CLOCK_MONOTONIC = 1
duration = Timespec(int(sleep_ms // 1000), int((sleep_ms % 1000) * 1_000_000))
for _ in range(sleeps):
    libc.clock_nanosleep(CLOCK_MONOTONIC, 0, ctypes.byref(duration), None)
