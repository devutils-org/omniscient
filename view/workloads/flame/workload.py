#!/usr/bin/env python3
"""The flame view's workload: functions of known weight, on and off the cpu.

Two named threads, one after the other. `hot` spends its cpu in `heavy` and
`light`, four to one; `warm` spends its cpu in `medium` and sleeps in `nap`.
Every share is in `manifest.json`, read here, and spent in ten slices
interleaved so that no moment of a thread's life belongs to one function. Run
under `-X perf` so that each Python function has a name a profiler can read.

The threads run in turn rather than together: two Python threads burning at
once take turns on the interpreter's lock, and each would then be off the cpu
for as long as the other held it -- an off-cpu time this program did not
choose. The main thread waits in `Thread.join` for both, which is off-cpu time
it did choose: the whole run.
"""

import os
import sys
import threading
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
sys.dont_write_bytecode = True
import common  # noqa: E402

TRUTH = common.manifest(__file__)
SLICES = TRUTH["slices"]


def heavy(seconds):
    return common.burn(seconds)


def light(seconds):
    return common.burn(seconds)


def medium(seconds):
    return common.burn(seconds)


def nap(seconds):
    time.sleep(seconds)


FUNCTIONS = {"heavy": heavy, "light": light, "medium": medium, "nap": nap}


def thread(plan):
    common.name(plan["name"])
    work = list(plan["on_cpu"].items()) + list(plan["off_cpu"].items())
    for _ in range(SLICES):
        for function, seconds in work:
            FUNCTIONS[function](seconds / SLICES)


def main():
    common.name("flame-main")
    for plan in TRUTH["threads"]:
        each = threading.Thread(target=thread, args=(plan,))
        each.start()
        each.join()
    common.note("flame: done")


if __name__ == "__main__":
    main()
