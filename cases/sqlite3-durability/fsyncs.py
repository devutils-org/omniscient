"""Counts the syncs a run of the same commits makes, in a run of its own.

    python3 fsyncs.py EMPTY SYNCHRONOUS FILE

The reference of case 2's causes (`case.yml`) for what each commit asks of
the disk. It runs the same commits as the measured run -- `sqlite3 DATABASE
'PRAGMA synchronous=SYNCHRONOUS' '.read FILE'`, on a fresh copy of the empty
database EMPTY beside it, as the measured run is -- under `strace -f -c`,
counting only `fsync` and `fdatasync` (with `--seccomp-bpf`, so no other call
stops), and prints one line:

    wal=W commits=C fsyncs=F per_commit=P

`wal` is 1 when the database's journal is a write-ahead log, `commits` the
INSERT statements in FILE, each its own transaction, `fsyncs` the syncs the
run made, `per_commit` their ratio. This run is a second run of the same
input, never the measured one: the tool never sees it, and the measured run
is never traced.
"""

import os
import shutil
import subprocess
import sys
import tempfile

empty, synchronous, script = sys.argv[1], sys.argv[2], sys.argv[3]
# Beside the empty one, so on the same storage, and fresh: the measured run's
# database is never this one, and every run of this script starts as empty.
database = os.path.join(os.path.dirname(empty), "fsyncs.db")
for leftover in ("", "-journal", "-wal", "-shm"):
    try:
        os.remove(database + leftover)
    except FileNotFoundError:
        pass
shutil.copyfile(empty, database)

mode = subprocess.run(
    ["sqlite3", database, "PRAGMA journal_mode;"],
    capture_output=True,
    text=True,
    check=True,
).stdout.strip()
with open(script) as file:
    commits = sum(1 for line in file if line.startswith("INSERT"))

with tempfile.TemporaryDirectory() as scratch:
    summary = os.path.join(scratch, "strace")
    subprocess.run(
        [
            "strace", "-f", "--seccomp-bpf", "-c", "-e", "trace=fsync,fdatasync",
            "-o", summary,
            "sqlite3", database, f"PRAGMA synchronous={synchronous}", f".read {script}",
        ],
        stdout=subprocess.DEVNULL,
        check=True,
    )
    fsyncs = 0
    with open(summary) as file:
        for line in file:
            fields = line.split()
            # % time, seconds, usecs/call, calls, [errors,] syscall
            if len(fields) >= 5 and fields[-1] in ("fsync", "fdatasync"):
                fsyncs += int(fields[3])

print(
    f"wal={int(mode == 'wal')} commits={commits} fsyncs={fsyncs} "
    f"per_commit={fsyncs / max(commits, 1)}"
)
