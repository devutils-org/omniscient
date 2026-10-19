#!/usr/bin/env python3
"""A deterministic workload for the bpftrace family's pairs.

usage: omnref act.py ACTION N

`omnref` beside this file is a symlink to python3: the kernel names a task
after the path it was executed by, so this process is `omnref` from its first
system call, and a system-wide tool that keys by comm finds exactly this
workload's events while an omn filter on `comm` selects the same ones. The
name is set at exec on purpose, not by prctl(PR_SET_NAME) once running:
omn's proc.syscall keeps the comm an aggregate first saw, so a rename halfway
through is invisible to its comm filter (docs/fixes/w22-bpftrace.md).

It performs ACTION N times, and nothing else that the pair compares, then
lingers LINGER seconds (0.5 by default): bpftrace -c stops when its child
exits, and what is still in its output buffer then is dropped without a
`Lost` line -- distroproof's Rocky 9 printed 474 and 575 of 604 opens.
The linger is a sleep, which no pair counts; `spin` does not linger, since
its pairs' medians would take the idle half second in.

  getppid   N getppid(2) calls                 syscount, proc.syscall
  open      N openat(2) of one regular file    opensnoop, proc.syscall
  signal    N kill(self, SIGUSR2), ignored     killsnoop, proc.signals
  connect   N TCP connections to a local port  tcpconnect, tcplife, net.tcp.*
  refused   N connections to a closed port     tcpdrop, net.drops
  read      N 4 KiB O_DIRECT reads of a file   bitesize, biosnoop, biolatency
  vfsread   N 1-byte reads of a regular file   vfsstat, io.fs
  spin      N busy children on every CPU       runqlat, sched.runqlat
"""

import ctypes
import os
import signal
import socket
import sys
import tempfile
import time

def getppid(n):
    for _ in range(n):
        os.getppid()


def open_(n):
    with tempfile.NamedTemporaryFile(dir="/var/tmp") as f:
        for _ in range(n):
            os.close(os.open(f.name, os.O_RDONLY))


def signal_(n):
    signal.signal(signal.SIGUSR2, signal.SIG_IGN)
    me = os.getpid()
    for _ in range(n):
        os.kill(me, signal.SIGUSR2)


def connect(n):
    srv = socket.socket()
    srv.bind(("127.0.0.1", 0))
    srv.listen(n)
    port = srv.getsockname()[1]
    payload = b"x" * 65536
    for _ in range(n):
        c = socket.create_connection(("127.0.0.1", port))
        a, _ = srv.accept()
        c.sendall(payload)
        got = 0
        while got < len(payload):
            got += len(a.recv(len(payload)))
        c.close()
        a.close()
    srv.close()


def refused(n):
    probe = socket.socket()
    probe.bind(("127.0.0.1", 0))
    port = probe.getsockname()[1]
    probe.close()
    for _ in range(n):
        try:
            socket.create_connection(("127.0.0.1", port)).close()
        except ConnectionRefusedError:
            pass


def read(n):
    block = 4096
    fd, path = tempfile.mkstemp(dir="/var/tmp")
    try:
        os.write(fd, os.urandom(block * 64))
        os.fsync(fd)
        os.close(fd)
        fd = os.open(path, os.O_RDONLY | os.O_DIRECT)
        buf = ctypes.create_string_buffer(block * 2)
        # O_DIRECT wants an aligned buffer: take the aligned 4 KiB inside it.
        addr = (ctypes.addressof(buf) + block - 1) & ~(block - 1)
        aligned = (ctypes.c_char * block).from_address(addr)
        libc = ctypes.CDLL(None, use_errno=True)
        for i in range(n):
            if libc.pread(fd, aligned, block, (i % 64) * block) != block:
                raise OSError(ctypes.get_errno(), "O_DIRECT pread")
        os.close(fd)
    finally:
        os.unlink(path)


def vfsread(n):
    with tempfile.NamedTemporaryFile(dir="/var/tmp") as f:
        f.write(b"x")
        f.flush()
        fd = os.open(f.name, os.O_RDONLY)
        for _ in range(n):
            os.pread(fd, 1, 0)
        os.close(fd)


def spin(n):
    seconds = 2.0
    kids = []
    for _ in range(n * (os.cpu_count() or 1)):
        pid = os.fork()
        if pid == 0:
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                pass
            os._exit(0)
        kids.append(pid)
    for pid in kids:
        os.waitpid(pid, 0)


ACTIONS = {
    "getppid": getppid,
    "open": open_,
    "signal": signal_,
    "connect": connect,
    "refused": refused,
    "read": read,
    "vfsread": vfsread,
    "spin": spin,
}

if __name__ == "__main__":
    action, n = sys.argv[1], int(sys.argv[2])
    ACTIONS[action](n)
    if action != "spin":
        time.sleep(float(os.environ.get("LINGER", "0.5")))
