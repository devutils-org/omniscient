"""Hold one loopback TCP connection busy with small round trips, then exit.

argv: the listening port (default 47311), and the seconds to run (default
3). A fixed port is what lets an observer started before this process find
the connection: `ss` by `sport = :<port>`, omn by its `sport` filter.
"""

import socket
import sys
import threading
import time

port = int(sys.argv[1]) if len(sys.argv) > 1 else 47311
secs = float(sys.argv[2]) if len(sys.argv) > 2 else 3.0

srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port))
srv.listen(1)


def echo():
    conn, _ = srv.accept()
    while True:
        b = conn.recv(64)
        if not b:
            break
        conn.sendall(b)
    conn.close()


t = threading.Thread(target=echo)
t.start()
cli = socket.create_connection(("127.0.0.1", port))
cli.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
end = time.monotonic() + secs
trips = 0
while time.monotonic() < end:
    cli.sendall(b"ping")
    cli.recv(64)
    trips += 1
    time.sleep(0.001)
cli.close()
t.join()
srv.close()
print(f"{trips} round trips on port {port}")
