"""Holds an EXCLUSIVE lock on a sqlite database while another program waits.

    python3 holder.py DATABASE HOLD_MS PROGRAM

The injection of case 1's off-CPU cause (`case.yml`, `a-lock-held-elsewhere`).
It takes `BEGIN EXCLUSIVE` on DATABASE, says `held` on stdout, and then waits
for a process whose program is PROGRAM and whose command line names DATABASE
to appear in /proc -- the target, once the tool has released it from its gate
and it has become PROGRAM. From that moment it holds the lock HOLD_MS more,
commits, and prints its own account on one line:

    held_after_target_ns=N target_seen=1 poll_ns=P

`held_after_target_ns` is its own CLOCK_MONOTONIC from the moment it saw the
target to the moment its COMMIT returned: how long the target could not have
had the lock. `poll_ns` is the longest gap between two looks at /proc, which
is how late it may have seen the target. It never touches the target; the
target is only ever waited for.
"""

import os
import sqlite3
import sys
import time

database, hold_ms, program = sys.argv[1], int(sys.argv[2]), sys.argv[3]
me = os.getpid()

connection = sqlite3.connect(database, isolation_level=None, timeout=0)
connection.execute("BEGIN EXCLUSIVE")
print("held", flush=True)


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
            and os.path.basename(argv[0]).decode(errors="replace") == program
            and any(database.encode() in arg for arg in argv[1:])
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
connection.execute("COMMIT")
released = time.monotonic_ns()
print(
    f"held_after_target_ns={released - start} target_seen={int(seen is not None)} poll_ns={poll_ns}",
    flush=True,
)
