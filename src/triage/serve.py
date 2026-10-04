"""A server with a slow client, for `tests/triage.rs`: the triage's ground truth.

The main thread waits on a pipe for the request to be served; the serving
thread blocks in `sendall` on a connection whose peer -- a third thread, the
client -- sleeps MS before it reads. No thread runs while the main thread
waits: the serving thread's block on the connection is what it waits on
(`http-server-stdlib`'s slow client). Python's socket module is a library of
its own (`_socket.cpython-...so`), whose frames say the block is on a
connection; a C program's `send` is an unnamed libc frame. The serving
thread prints the kernel's accounting of itself on stderr, prefixed
`omn-truth`, as `workload.c` does.

    python3 serve.py MS
"""

import os
import socket
import sys
import threading
import time

BYTES = 4 << 20


def truth(mode, start):
    with open("/proc/thread-self/schedstat") as stat:
        oncpu, runq, _ = (int(field) for field in stat.read().split())
    wall = time.monotonic_ns() - start
    print(
        f"omn-truth mode={mode} tid={threading.get_native_id()} pid={os.getpid()} "
        f"wall_ns={wall} oncpu_ns={oncpu} runq_ns={runq}",
        file=sys.stderr,
        flush=True,
    )


def main():
    ms = int(sys.argv[1])
    server, peer = socket.socketpair()
    server.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
    peer.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
    done_r, done_w = os.pipe()

    def serving():
        start = time.monotonic_ns()
        server.sendall(b"x" * BYTES)
        truth("serve-worker", start)
        os.write(done_w, b"1")

    def client():
        time.sleep(ms / 1000)
        got = 0
        while got < BYTES:
            got += len(peer.recv(1 << 16))

    start = time.monotonic_ns()
    threads = [threading.Thread(target=client), threading.Thread(target=serving)]
    for thread in threads:
        thread.start()
    os.read(done_r, 1)
    truth("serve", start)
    for thread in threads:
        thread.join()


if __name__ == "__main__":
    main()
