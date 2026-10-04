#!/usr/bin/env python3
"""The timebar view's workload: a known split of wall clock by state.

One thread, `worker`, lives through four phases whose lengths are in
`manifest.json`: it computes, sleeps, waits for a lock `holder` sleeps under,
and computes again on a core it shares with a `rival` process pinned there,
so that half of that phase's wall clock is spent runnable and not running.
"""

import os
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
sys.dont_write_bytecode = True
import common  # noqa: E402

TRUTH = common.manifest(__file__)
PHASES = {phase["name"]: phase for phase in TRUTH["phases"]}


def compute(seconds):
    return common.burn(seconds)


def rest(seconds):
    time.sleep(seconds)


def wait_for_lock(lock):
    lock.acquire()
    lock.release()


def holder(lock, taken, seconds):
    common.name(PHASES["lock"]["holder"])
    with lock:
        taken.set()
        time.sleep(seconds)


def contend(seconds):
    """Burns `seconds` on a core a rival process burns as long on."""
    core = sorted(os.sched_getaffinity(0))[0]
    rival = subprocess.Popen(
        [sys.executable, os.path.abspath(__file__), "rival", str(core), str(seconds)],
        stdout=subprocess.PIPE,
    )
    rival.stdout.read(1)
    os.sched_setaffinity(0, {core})
    compute(seconds)
    rival.wait()


def rival(core, seconds):
    """The other process on the worker's core: started by `contend`."""
    common.name(PHASES["contended"]["rival"])
    os.sched_setaffinity(0, {int(core)})
    os.write(1, b"x")
    common.burn(float(seconds))


def worker():
    common.name(TRUTH["thread"])
    compute(PHASES["compute"]["seconds"])
    rest(PHASES["sleep"]["seconds"])
    lock, taken = threading.Lock(), threading.Event()
    held = threading.Thread(target=holder, args=(lock, taken, PHASES["lock"]["seconds"]))
    held.start()
    taken.wait()
    wait_for_lock(lock)
    held.join()
    contend(PHASES["contended"]["seconds"])


def main():
    common.name("timebar-main")
    each = threading.Thread(target=worker)
    each.start()
    each.join()
    time.sleep(TRUTH["tail_s"])
    common.note("timebar: done")


if __name__ == "__main__":
    if sys.argv[1:2] == ["rival"]:
        rival(*sys.argv[2:4])
    else:
        main()
