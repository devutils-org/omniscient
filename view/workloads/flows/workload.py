#!/usr/bin/env python3
"""The flows view's workload: known bytes per file and per socket.

Writes two files of known sizes, reads the larger back, and exchanges a known
number of bytes with a child process, `flows-sink`, over a loopback TCP
connection. Every size is in `manifest.json`.
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
CHUNK = TRUTH["chunk"]


def receive_exactly(connection, size):
    left = size
    while left:
        got = len(connection.recv(min(CHUNK, left)))
        if not got:
            raise SystemExit("flows: the peer closed early")
        left -= got


def send_all(connection, size):
    block = b"s" * CHUNK
    left = size
    while left:
        left -= connection.send(block[: min(CHUNK, left)])


def sink(port):
    """The child at the other end: takes what was sent, answers its share."""
    common.name(TRUTH["socket"]["peer"])
    with socket.create_connection(("127.0.0.1", int(port))) as connection:
        receive_exactly(connection, TRUTH["socket"]["sent"])
        send_all(connection, TRUTH["socket"]["received"])


def main():
    common.name("flows-main")
    time.sleep(TRUTH["lead_s"])
    where = common.scratch("flows")
    for each in TRUTH["files"]:
        common.write_file(os.path.join(where, each["name"]), each["written"], CHUNK, each["fd"])
    for each in TRUTH["files"]:
        if each["read"]:
            common.read_file(os.path.join(where, each["name"]), CHUNK, each["fd"])

    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    child = subprocess.Popen(
        [sys.executable, os.path.abspath(__file__), "sink", str(listener.getsockname()[1])]
    )
    connection, _ = listener.accept()
    with connection:
        send_all(connection, TRUTH["socket"]["sent"])
        receive_exactly(connection, TRUTH["socket"]["received"])
    listener.close()
    child.wait()
    time.sleep(TRUTH["tail_s"])
    common.note("flows: done")


if __name__ == "__main__":
    if sys.argv[1:2] == ["sink"]:
        sink(sys.argv[2])
    else:
        main()
