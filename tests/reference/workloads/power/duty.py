"""Keep one cpu busy a known share of the time: `duty.py CPU SECONDS SHARE`.

A power pair's workload for turbostat's `Busy%` and `Avg_MHz`: pinned to CPU,
it spins SHARE of every 10 ms and sleeps the rest, for SECONDS, so the cpu's
busy share is about SHARE and its average frequency about SHARE times the
busy frequency -- numbers neither 0 nor 100 % for the two sides to agree on."""

import os
import sys
import time

if __name__ == "__main__":
    cpu = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    seconds = float(sys.argv[2]) if len(sys.argv) > 2 else 3.0
    share = float(sys.argv[3]) if len(sys.argv) > 3 else 0.5
    os.sched_setaffinity(0, {cpu})
    period = 0.010
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        start = time.monotonic()
        while time.monotonic() - start < period * share:
            pass
        rest = period - (time.monotonic() - start)
        if rest > 0:
            time.sleep(rest)
