"""A process with a known footprint and a known cpu time, for the `top`
family (tests/reference/top.rs): `hold.py [MIB] [CPU_SECONDS] [LINGER]`.

It maps and touches MIB private anonymous mebibytes (default 64) and names
itself `hold-ready`; it spins until its own cpu time reaches CPU_SECONDS
(default 2.0) and names itself `hold-done`; it then sleeps LINGER seconds
(default 1.0) holding still -- its memory and its cpu total fixed -- and
exits. The name is the process's `comm` (`/proc/<pid>/comm`), which is what
`at.sh` waits on: the runner sends the workload's output nowhere, and a
pid-taking tool must read it at a known phase."""

import mmap
import sys
import time

MIB = int(sys.argv[1]) if len(sys.argv) > 1 else 64
CPU_SECONDS = float(sys.argv[2]) if len(sys.argv) > 2 else 2.0
LINGER = float(sys.argv[3]) if len(sys.argv) > 3 else 1.0


def phase(name):
    with open("/proc/self/comm", "w") as comm:
        comm.write(name)


buf = mmap.mmap(-1, MIB << 20, flags=mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS)
for off in range(0, MIB << 20, 4096):
    buf[off] = 1
phase("hold-ready")

n = 0
while time.process_time() < CPU_SECONDS:
    n += 1
phase("hold-done")
time.sleep(LINGER)
