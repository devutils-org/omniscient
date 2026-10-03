"""A pure-Python program that waits in a blocking read.

    python3 reader.py FIFO

Case 3's off-CPU workload (`case.yml`, `a-blocking-read`). It opens FIFO, a
named pipe the injecting process (`writer.py`) already holds open for
writing, so the open returns at once, and then reads it: the `read` blocks
until the writer writes, which it does a known time after it saw this
program appear. It prints its own account on stdout, from its own clock
around the read:

    read_ns=N bytes=B

`read_ns` is `time.monotonic_ns()` (CLOCK_MONOTONIC) around the first `read`
that returned data.
"""

import os
import sys
import time


def wait_for_input(path):
    descriptor = os.open(path, os.O_RDONLY)
    start = time.monotonic_ns()
    data = os.read(descriptor, 65536)
    waited = time.monotonic_ns() - start
    os.close(descriptor)
    return data, waited


data, waited = wait_for_input(sys.argv[1])
print(f"read_ns={waited} bytes={len(data)}", flush=True)
