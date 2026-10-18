"""What every view workload shares: its manifest, its threads' names, and a
burn of cpu time whose length the program measures on its own thread's clock.

A workload reads its parameters from the `manifest.json` beside it, so the
program and the truth a test holds the tool to cannot drift apart: changing a
duration in the manifest changes what the program does.

Nothing is printed on standard output: a launched program inherits the stream
`omn` writes its results to. Notes go to standard error.
"""

import ctypes
import json
import os
import sys
import time

libc = ctypes.CDLL(None, use_errno=True)
PR_SET_NAME = 15


def manifest(program):
    """The manifest beside `program` (pass `__file__`)."""
    path = os.path.join(os.path.dirname(os.path.abspath(program)), "manifest.json")
    with open(path) as file:
        return json.load(file)


def name(comm):
    """The calling thread's name as the kernel shows it (`PR_SET_NAME`)."""
    libc.prctl(PR_SET_NAME, comm.encode()[:15], 0, 0, 0)


def note(text):
    print(text, file=sys.stderr, flush=True)


def burn(seconds):
    """Stays on the cpu until this thread has run for `seconds` more.

    Measured on the thread's own cpu clock (`CLOCK_THREAD_CPUTIME_ID`), so a
    busy machine stretches the wall clock it takes and not the cpu time it
    spends: exact to one pass of the loop, a few microseconds.
    """
    end = time.thread_time() + seconds
    total = 0
    while time.thread_time() < end:
        for value in range(200):
            total += value * value % 7
    return total


def scratch(view):
    """A directory for the files a workload writes, emptied first.

    Under the checkout's `target/`, on a disk rather than in memory, so that a
    write has a device under it for the views that draw devices.
    """
    root = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "../.."))
    path = os.path.join(root, "target", "omn-view-workloads", view)
    os.makedirs(path, exist_ok=True)
    for entry in os.listdir(path):
        os.unlink(os.path.join(path, entry))
    return path



def write_file(path, size, chunk, fd):
    """Writes `size` bytes to `path` in writes of `chunk`, through descriptor
    `fd`, and fsyncs it.

    The descriptor is chosen so that a source that reports bytes per
    descriptor reports this file's apart from the interpreter's own reads at
    startup, which reuse the low numbers.
    """
    # Random bytes, a fresh block per write: a filesystem that compresses
    # (btrfs with `compress=zstd`) would write a repeated pattern to the
    # device as a few percent of its size, and the device column would be
    # measuring the compressor.
    opened = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    os.dup2(opened, fd)
    os.close(opened)
    try:
        left = size
        while left:
            left -= os.write(fd, os.urandom(min(chunk, left)))
        os.fsync(fd)
    finally:
        os.close(fd)


def read_file(path, chunk, fd):
    """Reads `path` to its end in reads of `chunk`, through descriptor `fd`;
    returns the bytes read."""
    opened = os.open(path, os.O_RDONLY)
    os.dup2(opened, fd)
    os.close(opened)
    total = 0
    try:
        while True:
            got = len(os.read(fd, chunk))
            if not got:
                return total
            total += got
    finally:
        os.close(fd)
