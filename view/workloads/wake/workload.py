#!/usr/bin/env python3
"""The wake view's workload: a known chain of hand-offs.

Three processes, `relay-a`, `relay-b` and `relay-c`, sit in a ring of pipes.
A token goes around it `laps` times (`manifest.json`): each relay blocks
reading its pipe, holds the token on the cpu for a moment, and writes it to
the next. Every hand-off is one wakeup of a known waker and a known wakee.
Separate processes rather than threads, so that no interpreter lock hands the
cpu from one to another behind the pipes' back.
"""

import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
sys.dont_write_bytecode = True
import common  # noqa: E402

TRUTH = common.manifest(__file__)


def hold():
    end = time.perf_counter() + TRUTH["hold_us"] / 1e6
    while time.perf_counter() < end:
        pass


def relay(name, read_fd, write_fd, first):
    common.name(name)
    read_fd, write_fd = int(read_fd), int(write_fd)
    laps = TRUTH["laps"]
    if first == "1":
        hold()
        os.write(write_fd, b"t")
        laps -= 1
        for _ in range(laps):
            os.read(read_fd, 1)
            hold()
            os.write(write_fd, b"t")
        os.read(read_fd, 1)
    else:
        for _ in range(laps):
            os.read(read_fd, 1)
            hold()
            os.write(write_fd, b"t")


def main():
    common.name("wake-main")
    ring = TRUTH["ring"]
    pipes = [os.pipe() for _ in ring]
    children = []
    for index, name in enumerate(ring):
        read_fd = pipes[index - 1][0]
        write_fd = pipes[index][1]
        children.append(
            subprocess.Popen(
                [sys.executable, os.path.abspath(__file__), "relay", name,
                 str(read_fd), str(write_fd), "1" if index == 0 else "0"],
                pass_fds=(read_fd, write_fd),
            )
        )
    for read_fd, write_fd in pipes:
        os.close(read_fd)
        os.close(write_fd)
    for child in children:
        child.wait()
    common.note("wake: done")


if __name__ == "__main__":
    if sys.argv[1:2] == ["relay"]:
        relay(*sys.argv[2:6])
    else:
        main()
