"""Kernel lock contention one process makes on its own address space.

    contend.py THREADS SECONDS

THREADS threads map and unmap anonymous memory for SECONDS: every mapping
takes the process's `mmap_lock` for writing, and the interpreter lets go of
its own lock around the system call, so the threads queue on the kernel's
rwsem rather than on Python's.
"""

import mmap
import sys
import threading
import time

threads, seconds = int(sys.argv[1]), float(sys.argv[2])
deadline = time.monotonic() + seconds


def churn():
    while time.monotonic() < deadline:
        m = mmap.mmap(-1, 1 << 20)
        m[0] = 1
        m.close()


workers = [threading.Thread(target=churn) for _ in range(threads)]
for w in workers:
    w.start()
for w in workers:
    w.join()
