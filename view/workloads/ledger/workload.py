#!/usr/bin/env python3
"""The ledger view's workload: a known cost on every resource.

In turn: `cpu_s` of cpu time, `memory.bytes` of memory touched a page at a
time and held to the end, a file written, and bytes exchanged with a child,
`ledger-peer`, over loopback. Every figure is in `manifest.json`.
"""

import mmap
import os
import socket
import subprocess
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
sys.dont_write_bytecode = True
import common  # noqa: E402

TRUTH = common.manifest(__file__)


def exchange(connection, send, receive, chunk):
    block = b"l" * chunk
    left = send
    while left:
        left -= connection.send(block[: min(chunk, left)])
    left = receive
    while left:
        got = len(connection.recv(min(chunk, left)))
        if not got:
            raise SystemExit("ledger: the peer closed early")
        left -= got


def peer(port):
    common.name(TRUTH["peer"])
    plan = TRUTH["socket"]
    with socket.create_connection(("127.0.0.1", int(port))) as connection:
        left = plan["sent"]
        while left:
            left -= len(connection.recv(min(plan["chunk"], left)))
        block = b"p" * plan["chunk"]
        left = plan["received"]
        while left:
            left -= connection.send(block[: min(plan["chunk"], left)])


def main():
    common.name("ledger-main")
    common.burn(TRUTH["cpu_s"])

    memory = TRUTH["memory"]
    region = mmap.mmap(-1, memory["bytes"])
    for offset in range(0, memory["bytes"], memory["page"]):
        region[offset] = 1

    plan = TRUTH["file"]
    where = common.scratch("ledger")
    common.write_file(os.path.join(where, plan["name"]), plan["written"], plan["chunk"], plan["fd"])

    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    child = subprocess.Popen(
        [sys.executable, os.path.abspath(__file__), "peer", str(listener.getsockname()[1])]
    )
    connection, _ = listener.accept()
    with connection:
        plan = TRUTH["socket"]
        exchange(connection, plan["sent"], plan["received"], plan["chunk"])
    listener.close()
    child.wait()

    time.sleep(memory["hold_s"])
    region.close()
    common.note("ledger: done")


if __name__ == "__main__":
    if sys.argv[1:2] == ["peer"]:
        peer(sys.argv[2])
    else:
        main()
