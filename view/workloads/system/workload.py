#!/usr/bin/env python3
"""The system view's workload: a known cost on every panel of the machine.

The program and a child process, `system-burn`, burn `burn_s` of cpu each at
once -- two cores' worth. A launched program's on-cpu time holds its own
share through `fs:proc`, and the child's as well through `bpf:raw_tp`, which
follows descendants. Then the program writes a file to a disk and fsyncs it, then sends bytes to itself over
loopback. Every figure is in `manifest.json`.
"""

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


def burner(name):
    common.name(name)
    common.burn(TRUTH["burn_s"])


def drain(connection, size, chunk):
    left = size
    while left:
        got = len(connection.recv(min(chunk, left)))
        if not got:
            raise SystemExit("system: the loopback closed early")
        left -= got


def main():
    common.name("system-main")
    time.sleep(TRUTH["lead_s"])
    rival = subprocess.Popen([sys.executable, os.path.abspath(__file__), "burn", TRUTH["burner"]])
    common.burn(TRUTH["burn_s"])
    rival.wait()

    plan = TRUTH["file"]
    where = common.scratch("system")
    common.write_file(os.path.join(where, plan["name"]), plan["written"], plan["chunk"], plan["fd"])

    plan = TRUTH["loopback"]
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    sender = socket.create_connection(listener.getsockname())
    receiver, _ = listener.accept()
    reader = threading.Thread(target=drain, args=(receiver, plan["bytes"], plan["chunk"]))
    reader.start()
    block = b"y" * plan["chunk"]
    left = plan["bytes"]
    while left:
        left -= sender.send(block[: min(plan["chunk"], left)])
    reader.join()
    for each in (sender, receiver, listener):
        each.close()
    time.sleep(TRUTH["tail_s"])
    common.note("system: done")


if __name__ == "__main__":
    if sys.argv[1:2] == ["burn"]:
        burner(sys.argv[2])
    else:
        main()
