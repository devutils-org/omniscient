"""Writes into a named pipe a known time after a program starts reading it.

    python3 writer.py FIFO HOLD_MS PROGRAM

The injection of case 3's off-CPU cause (`case.yml`, `a-blocking-read`). It
creates FIFO, opens it for reading and writing (so neither its open nor the
reader's blocks), says `ready` on stdout, and waits for a process whose
command line names FIFO and runs PROGRAM to appear in /proc -- the target,
once the tool has released it from its gate. From that moment it waits
HOLD_MS more, writes one line into the pipe, and prints its own account:

    held_after_target_ns=N target_seen=1 poll_ns=P

`held_after_target_ns` is its own CLOCK_MONOTONIC from the moment it saw the
target to the moment its write returned: how long the target's read could
not have had data. `poll_ns` is the longest gap between two looks at /proc,
how late it may have seen the target. It never touches the target.
"""

import os
import sys
import time

fifo, hold_ms, program = sys.argv[1], int(sys.argv[2]), sys.argv[3]
me = os.getpid()

if os.path.exists(fifo):
    os.unlink(fifo)
os.mkfifo(fifo)
descriptor = os.open(fifo, os.O_RDWR)
print("ready", flush=True)


def target():
    for entry in os.listdir("/proc"):
        if not entry.isdigit() or int(entry) == me:
            continue
        try:
            with open(f"/proc/{entry}/cmdline", "rb") as file:
                argv = file.read().split(b"\0")
        except OSError:
            continue
        if (
            argv
            and os.path.basename(argv[0]).decode(errors="replace").startswith(program)
            and any(fifo.encode() in arg for arg in argv[1:])
        ):
            return int(entry)
    return None


deadline = time.monotonic() + 60
poll_ns = 0
seen = None
last = time.monotonic_ns()
while seen is None and time.monotonic() < deadline:
    if target() is not None:
        seen = time.monotonic_ns()
    now = time.monotonic_ns()
    poll_ns = max(poll_ns, now - last)
    last = now
    if seen is None:
        time.sleep(0.001)

start = seen if seen is not None else time.monotonic_ns()
time.sleep(hold_ms / 1000)
os.write(descriptor, b"the input the reader waited for\n")
written = time.monotonic_ns()
time.sleep(0.2)
os.close(descriptor)
os.unlink(fifo)
print(
    f"held_after_target_ns={written - start} target_seen={int(seen is not None)} poll_ns={poll_ns}",
    flush=True,
)
