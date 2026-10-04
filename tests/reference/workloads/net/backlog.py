"""Overflow a listener's accept queue on loopback, then exit.

argv: connections to attempt (default 64), and the seconds to hold them
(default 2). The listener's queue is one deep and nothing ever accepts, so
every connection past the first couple overflows it: the kernel counts
`ListenOverflows` and `ListenDrops`, and the dropped SYNs are retransmitted.
"""

import socket
import sys
import time

n = int(sys.argv[1]) if len(sys.argv) > 1 else 64
hold = float(sys.argv[2]) if len(sys.argv) > 2 else 2.0

srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.bind(("127.0.0.1", 0))
srv.listen(1)
addr = srv.getsockname()

clients = []
for _ in range(n):
    c = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    c.setblocking(False)
    c.connect_ex(addr)
    clients.append(c)
time.sleep(hold)
for c in clients:
    c.close()
srv.close()
print(f"attempted {n} connections on a one-deep queue")
