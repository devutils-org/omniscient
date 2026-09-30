"""The program the approved sessions of this directory benchmark.

A stand-in for yours: replace `python3 program.py` in each session with the
command that runs your program, and nothing else in the sessions has to change.

It does a fixed amount of work in three phases -- compute, wait, compute --
so that what dominates its time is known before anything measures it: about
two thirds of its life on a cpu, one third blocked in a sleep. Then it says
what it did, on its own clock and from the kernel's own account of it, in one
line on stderr prefixed `omn-truth`, which `run_session` hands over with what
`omn` said (`said_by_omn`). The tests of `omn-mcp` hold the tool to that line.
"""

import sys
import time


def compute(rounds):
    total = 0
    for i in range(rounds):
        total += i * i
    return total


def main():
    rounds = int(sys.argv[1]) if len(sys.argv) > 1 else 3_000_000
    sleep_s = float(sys.argv[2]) if len(sys.argv) > 2 else 0.2
    started = time.monotonic_ns()
    compute(rounds)
    slept_from = time.monotonic_ns()
    time.sleep(sleep_s)
    slept_to = time.monotonic_ns()
    compute(rounds)
    ended = time.monotonic_ns()
    with open("/proc/self/schedstat") as stat:
        oncpu_ns, runq_ns, _ = (int(field) for field in stat.read().split())
    print(
        f"omn-truth started_ns={started} ended_ns={ended} wall_ns={ended - started} "
        f"slept_ns={slept_to - slept_from} oncpu_ns={oncpu_ns} runq_ns={runq_ns}",
        file=sys.stderr,
        flush=True,
    )


main()
