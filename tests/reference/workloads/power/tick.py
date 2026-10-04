"""Wake one cpu every millisecond for S seconds: `tick.py SECONDS CPU`.

The cstate pair's load. A cpu's idle residency is credited in the cpuidle
sysfs files only when the cpu wakes, so on a NOHZ idle cpu one idle period,
seconds long, can sit uncredited across a read. Woken every millisecond, the
cpu's residency moves in steps of about a millisecond, and a read made at any
moment is within that of the true figure; the cpu stays mostly idle."""

import os
import sys
import time

if __name__ == "__main__":
    seconds = float(sys.argv[1]) if len(sys.argv) > 1 else 3.0
    cpu = int(sys.argv[2]) if len(sys.argv) > 2 else 1
    os.sched_setaffinity(0, {cpu})
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        time.sleep(0.001)
