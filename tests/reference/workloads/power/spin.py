"""Keep N cpus busy for S seconds: `spin.py SECONDS [PROCS]`.

A power pair's workload: long enough for a package energy counter to move by
tens of joules, short enough to keep the reference suite quick."""

import multiprocessing
import sys
import time


def spin(seconds):
    end = time.monotonic() + seconds
    x = 0
    while time.monotonic() < end:
        x += 1


if __name__ == "__main__":
    seconds = float(sys.argv[1]) if len(sys.argv) > 1 else 2.0
    procs = int(sys.argv[2]) if len(sys.argv) > 2 else 4
    workers = [multiprocessing.Process(target=spin, args=(seconds,)) for _ in range(procs)]
    for w in workers:
        w.start()
    for w in workers:
        w.join()
