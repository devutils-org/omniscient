#!/usr/bin/env python3
"""The heatmap view's workload: latency regimes in known phases.

The program sends a request byte to a child, `heatmap-peer`, over a loopback
TCP connection and waits in `recv` for the answer; the peer sleeps the phase's
pause before answering. Fast, slow, fast again: the phases and their pauses
are in `manifest.json`.
"""

import os
import socket
import subprocess
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
sys.dont_write_bytecode = True
import common  # noqa: E402

TRUTH = common.manifest(__file__)


def peer(port):
    common.name(TRUTH["peer"])
    with socket.create_connection(("127.0.0.1", int(port))) as connection:
        connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        for phase in TRUTH["phases"]:
            for _ in range(phase["requests"]):
                connection.recv(1)
                time.sleep(phase["pause_ms"] / 1e3)
                connection.send(b"a")


def main():
    common.name("heatmap-main")
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    child = subprocess.Popen(
        [sys.executable, os.path.abspath(__file__), "peer", str(listener.getsockname()[1])]
    )
    connection, _ = listener.accept()
    connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    with connection:
        for phase in TRUTH["phases"]:
            began = time.monotonic()
            for _ in range(phase["requests"]):
                connection.send(b"q")
                if connection.recv(1) != b"a":
                    raise SystemExit("heatmap: the peer closed early")
            common.note(f"heatmap: {phase['name']} took {time.monotonic() - began:.3f} s")
    listener.close()
    child.wait()
    time.sleep(TRUTH["tail_s"])


if __name__ == "__main__":
    if sys.argv[1:2] == ["peer"]:
        peer(sys.argv[2])
    else:
        main()
