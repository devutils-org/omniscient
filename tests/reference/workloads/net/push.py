"""Push a known volume through a loopback TCP connection, then exit.

argv: megabytes to push (default 16), and the seconds to spread them over
(default 0: as fast as the loopback takes them). The sender and the receiver are two
threads of this one process, so a pid-scoped observer sees both ends.
"""

import socket
import sys
import threading
import time

MIB = 1 << 20
total = int(sys.argv[1]) * MIB if len(sys.argv) > 1 else 16 * MIB
spread = float(sys.argv[2]) if len(sys.argv) > 2 else 0.0

srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.bind(("127.0.0.1", 0))
srv.listen(1)
received = 0


def drain():
    global received
    conn, _ = srv.accept()
    while True:
        chunk = conn.recv(1 << 16)
        if not chunk:
            break
        received += len(chunk)
    conn.close()


t = threading.Thread(target=drain)
t.start()
cli = socket.create_connection(srv.getsockname())
block = b"x" * MIB
blocks = total // MIB
for _ in range(blocks):
    cli.sendall(block)
    if spread:
        time.sleep(spread / blocks)
cli.shutdown(socket.SHUT_WR)
t.join()
cli.close()
srv.close()
print(f"pushed {received} bytes")
