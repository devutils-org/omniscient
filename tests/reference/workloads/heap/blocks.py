#!/usr/bin/env python3
"""A heap whose shape the test chose: N blocks of SIZE bytes from libc's
malloc, held until the end, then freed but for LEAK of them, after holding the peak for HOLD seconds so a poller sees
it. The blocks dwarf the interpreter's own allocations, so the totals a tool reports
are this workload's to within a declared tolerance.

usage: blocks.py [N] [SIZE] [LEAK] [HOLD]
"""

import ctypes
import sys
import time

n = int(sys.argv[1]) if len(sys.argv) > 1 else 64
size = int(sys.argv[2]) if len(sys.argv) > 2 else 1 << 20
leak = int(sys.argv[3]) if len(sys.argv) > 3 else 0
hold = float(sys.argv[4]) if len(sys.argv) > 4 else 0.0

libc = ctypes.CDLL(None)
libc.malloc.restype = ctypes.c_void_p
libc.malloc.argtypes = [ctypes.c_size_t]
libc.free.argtypes = [ctypes.c_void_p]
libc.memset.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_size_t]

blocks = []
for _ in range(n):
    p = libc.malloc(size)
    if not p:
        sys.exit("malloc failed")
    libc.memset(p, 1, size)
    blocks.append(p)

time.sleep(hold)

for p in blocks[leak:]:
    libc.free(p)
