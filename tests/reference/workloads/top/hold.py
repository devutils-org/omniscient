"""A process with a known footprint and a known cpu time, for the `top`
family (tests/reference/top.rs): `hold.py [MIB] [CPU_SECONDS] [LINGER]`.

It maps and touches MIB private anonymous mebibytes (default 64) and says
"ready" on stderr; it spins until its own cpu time reaches CPU_SECONDS
(default 2.0) and says "done"; it then sleeps LINGER seconds (default 1.0)
holding still -- its memory and its cpu total fixed -- and exits."""

import mmap
import sys
import time

MIB = int(sys.argv[1]) if len(sys.argv) > 1 else 64
CPU_SECONDS = float(sys.argv[2]) if len(sys.argv) > 2 else 2.0
LINGER = float(sys.argv[3]) if len(sys.argv) > 3 else 1.0

buf = mmap.mmap(-1, MIB << 20, flags=mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS)
for off in range(0, MIB << 20, 4096):
    buf[off] = 1
print("ready", file=sys.stderr, flush=True)

n = 0
while time.process_time() < CPU_SECONDS:
    n += 1
print("done", file=sys.stderr, flush=True)
time.sleep(LINGER)
