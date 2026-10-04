#!/usr/bin/env python3
"""The process view's workload: every view's load at once, on one process.

The main thread computes in `crunch` and `tidy`, sleeps in `doze`, waits in
`queue_up` for a lock `process-holder` sleeps under, touches a footprint a
page at a time, writes a file, and exchanges bytes with a child,
`process-peer`, over loopback. Every figure is in `manifest.json`.
"""

import mmap
import os
import socket
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
sys.dont_write_bytecode = True
import common  # noqa: E402

TRUTH = common.manifest(__file__)


def crunch(seconds):
    return common.burn(seconds)


def tidy(seconds):
    return common.burn(seconds)


def doze(seconds):
    time.sleep(seconds)


def queue_up(lock):
    lock.acquire()
    lock.release()


def holder(lock, taken):
    common.name(TRUTH["lock"]["holder"])
    with lock:
        taken.set()
        time.sleep(TRUTH["lock"]["seconds"])


def peer(port):
    common.name(TRUTH["socket"]["peer"])
    plan = TRUTH["socket"]
    with socket.create_connection(("127.0.0.1", int(port))) as connection:
        left = plan["sent"]
        while left:
            left -= len(connection.recv(min(plan["chunk"], left)))
        block = b"p" * plan["chunk"]
        left = plan["received"]
        while left:
            left -= connection.send(block[: min(plan["chunk"], left)])


def exchange():
    plan = TRUTH["socket"]
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    child = subprocess.Popen(
        [sys.executable, os.path.abspath(__file__), "peer", str(listener.getsockname()[1])]
    )
    connection, _ = listener.accept()
    with connection:
        block = b"s" * plan["chunk"]
        left = plan["sent"]
        while left:
            left -= connection.send(block[: min(plan["chunk"], left)])
        left = plan["received"]
        while left:
            got = len(connection.recv(min(plan["chunk"], left)))
            if not got:
                raise SystemExit("process: the peer closed early")
            left -= got
    listener.close()
    child.wait()


def main():
    common.name(TRUTH["thread"])
    cpu = TRUTH["cpu"]
    for _ in range(5):
        crunch(cpu["seconds"] / 5)
        tidy(cpu["minor"]["seconds"] / 5)
    doze(TRUTH["sleep"]["seconds"])

    lock, taken = threading.Lock(), threading.Event()
    held = threading.Thread(target=holder, args=(lock, taken))
    held.start()
    taken.wait()
    queue_up(lock)
    held.join()

    memory = TRUTH["memory"]
    region = mmap.mmap(-1, memory["bytes"])
    for offset in range(0, memory["bytes"], memory["page"]):
        region[offset] = 1

    plan = TRUTH["file"]
    common.write_file(
        os.path.join(common.scratch("process"), plan["name"]), plan["written"], plan["chunk"], plan["fd"]
    )
    exchange()
    time.sleep(memory["hold_s"])
    region.close()
    common.note("process: done")


if __name__ == "__main__":
    if sys.argv[1:2] == ["peer"]:
        peer(sys.argv[2])
    else:
        main()
