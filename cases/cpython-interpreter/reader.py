"""A pure-Python program that prepares, then waits in a blocking read.

    python3 reader.py FIFO STEPS

Case 3's off-CPU workload (`case.yml`, `a-blocking-read`). It first spends a
known amount of work on the cpu (`prepare`, a pure-Python loop of STEPS
steps), as a program that sets itself up before it waits for its input does.
It then opens FIFO, a named pipe the injecting process (`writer.py`) already
holds open for writing, so the open returns at once, and reads it: the `read`
blocks until the writer writes, which it does a known time after it saw this
program appear. It prints its own account on stdout, from its own clock:

    read_ns=N before_read_ns=M prepare_cpu_ns=C bytes=B

`read_ns` is `time.monotonic_ns()` (CLOCK_MONOTONIC) around the first `read`
that returned data; `before_read_ns` the same clock from the script's first
line to the read; `prepare_cpu_ns` the cpu time `prepare` took
(`time.process_time_ns()`).

The preparation is there so that the program's own cpu is well above what a
tracer spends to watch it: a program that only waits burns the interpreter's
start, some 14 ms, which is about what each of the triage's eBPF sources
costs over the same run, so a ceiling of 100 % of the target's cpu would be
crossed or not by chance (`docs/fixes/w17-python.md`, "Review").
"""

import os
import sys
import time

started = time.monotonic_ns()


def prepare(steps):
    total = 0
    for i in range(steps):
        total += i * i
    return total


def wait_for_input(path):
    descriptor = os.open(path, os.O_RDONLY)
    start = time.monotonic_ns()
    data = os.read(descriptor, 65536)
    waited = time.monotonic_ns() - start
    os.close(descriptor)
    return data, start, waited


cpu = time.process_time_ns()
prepare(int(sys.argv[2]))
cpu = time.process_time_ns() - cpu
data, start, waited = wait_for_input(sys.argv[1])
print(
    f"read_ns={waited} before_read_ns={start - started} prepare_cpu_ns={cpu} bytes={len(data)}",
    flush=True,
)
