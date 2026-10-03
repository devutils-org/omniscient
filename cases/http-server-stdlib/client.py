"""The client of case 4: asks a running `python3 -m http.server`, and times it.

    python3 client.py ROOT PATH COUNT [--cold FILE] [--pace BYTES MS]

The injection of case 4's causes (`case.yml`); no load generator is
installed, so this is both the load and the latency clock. It says `ready` on
stdout, then waits for the server to appear in /proc -- a `python3` running
`http.server` whose command line names ROOT, the directory it serves -- and
to listen, reading the port from the kernel's table of the server's own
sockets (`/proc/<pid>/net/tcp` against the inodes of its descriptors): the
server was asked for any free port (`0`).

It then sends COUNT requests for PATH, one after the other on one HTTP/1.1
connection (the server is started with `-p HTTP/1.1`), so one thread of the
server -- the one `http.server` starts for the connection -- serves them all
and is alive to be read when the last response is in. Each response is read
to its end.

- `--cold FILE`: before each request, FILE (the file PATH names) is dropped
  from the page cache, `posix_fadvise(POSIX_FADV_DONTNEED)` on the client's
  own descriptor -- the server's are never touched -- and mincore(2) counts
  how many of its pages were left resident.
- `--pace BYTES MS`: the client reads BYTES at a time and sleeps MS between
  reads, with a receive buffer of BYTES, so the server's writes wait for it.

Once done it reads the server's own accounts, closes the connection and stops
the server with SIGINT, as a person stops it at its terminal ("Keyboard
interrupt received, exiting.", exit 0). Its account, on one line:

    requests=N bytes=B latency_ns=L cpu_ns=C handler_run_ns=R
    handler_wait_ns=W handler_off_ns=O handler_off_share=S paced_ns=P
    resident=K poll_ns=Q

- `latency_ns`: the sum over the requests of the client's CLOCK_MONOTONIC
  from sending the request to the response's last byte.
- `cpu_ns`: the server's utime plus stime, `/proc/<pid>/stat`, every thread
  of it, ended ones included, at a resolution of one clock tick.
- `handler_run_ns`, `handler_wait_ns`: the first two fields of the serving
  thread's `/proc/<pid>/task/<tid>/schedstat`, nanoseconds on a cpu and
  runnable waiting for one, over its life (the connection's).
- `handler_off_ns`: `latency_ns` less those two, at least 0: the time the
  thread serving the requests was neither on a cpu nor waiting for one;
  `handler_off_share` is its share of `latency_ns`.
- `paced_ns`: the client's own sleeps between reads (`--pace`).
- `resident`: pages of the cold file found resident after each drop, summed.
- `poll_ns`: the longest gap between two looks at /proc.
"""

import ctypes
import ctypes.util
import http.client
import mmap
import os
import signal
import socket
import sys
import time

root, path, count = sys.argv[1], sys.argv[2], int(sys.argv[3])
options = sys.argv[4:]
cold = options[options.index("--cold") + 1] if "--cold" in options else None
pace = None
if "--pace" in options:
    at = options.index("--pace")
    pace = (int(options[at + 1]), int(options[at + 2]))
me = os.getpid()
print("ready", flush=True)

libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)
libc.mmap.restype = ctypes.c_void_p
libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int,
                      ctypes.c_int, ctypes.c_long]
libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
libc.mincore.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_char_p]


def server():
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
            and os.path.basename(argv[0]).decode(errors="replace").startswith("python3")
            and b"http.server" in argv
            and root.encode() in argv
        ):
            return int(entry)
    return None


def port_of(pid):
    """The port a listening socket of `pid` is bound to, from the kernel."""
    inodes = set()
    try:
        for fd in os.listdir(f"/proc/{pid}/fd"):
            link = os.readlink(f"/proc/{pid}/fd/{fd}")
            if link.startswith("socket:["):
                inodes.add(link[8:-1])
    except OSError:
        return None
    for table in ("tcp", "tcp6"):
        try:
            with open(f"/proc/{pid}/net/{table}") as file:
                next(file)
                for line in file:
                    fields = line.split()
                    # `st` 0A is TCP_LISTEN.
                    if fields[3] == "0A" and fields[9] in inodes:
                        return int(fields[1].rsplit(":", 1)[1], 16)
        except OSError:
            continue
    return None


def resident(file_path):
    """How many of the file's pages are in the page cache, by mincore(2)."""
    fd = os.open(file_path, os.O_RDONLY)
    try:
        size = os.fstat(fd).st_size
        address = libc.mmap(None, size, mmap.PROT_READ, mmap.MAP_SHARED, fd, 0)
        if address in (None, ctypes.c_void_p(-1).value):
            raise OSError(ctypes.get_errno(), "mmap")
        pages = (size + mmap.PAGESIZE - 1) // mmap.PAGESIZE
        vector = ctypes.create_string_buffer(pages)
        failed = libc.mincore(address, size, vector) != 0
        libc.munmap(address, size)
        if failed:
            raise OSError(ctypes.get_errno(), "mincore")
        return sum(byte & 1 for byte in vector.raw)
    finally:
        os.close(fd)


def drop(file_path):
    fd = os.open(file_path, os.O_RDONLY)
    try:
        os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
    finally:
        os.close(fd)


deadline = time.monotonic() + 60
poll_ns = 0
pid = port = None
last = time.monotonic_ns()
while port is None and time.monotonic() < deadline:
    if pid is None:
        pid = server()
    if pid is not None:
        port = port_of(pid)
    now = time.monotonic_ns()
    poll_ns = max(poll_ns, now - last)
    last = now
    if port is None:
        time.sleep(0.001)
if port is None:
    sys.exit(f"no server serving {root} listened within 60 s")

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
if pace is not None:
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, pace[0])
sock.connect(("127.0.0.1", port))
connection = http.client.HTTPConnection("127.0.0.1", port)
connection.sock = sock

latency = received = left = paced = 0
for _ in range(count):
    if cold is not None:
        drop(cold)
        left += resident(cold)
    start = time.monotonic_ns()
    connection.request("GET", path)
    response = connection.getresponse()
    while True:
        chunk = response.read(pace[0] if pace else 1 << 20)
        if not chunk:
            break
        received += len(chunk)
        if pace is not None:
            before = time.monotonic_ns()
            time.sleep(pace[1] / 1000)
            paced += time.monotonic_ns() - before
    latency += time.monotonic_ns() - start
    if response.status != 200:
        sys.exit(f"GET {path} answered {response.status}")

handler_run = handler_wait = 0
for tid in os.listdir(f"/proc/{pid}/task"):
    if int(tid) == pid:
        continue
    with open(f"/proc/{pid}/task/{tid}/schedstat") as file:
        run, wait = (int(value) for value in file.read().split()[:2])
    handler_run, handler_wait = handler_run + run, handler_wait + wait
with open(f"/proc/{pid}/stat") as file:
    fields = file.read().rsplit(")", 1)[1].split()
cpu_ns = (int(fields[11]) + int(fields[12])) * 1_000_000_000 // os.sysconf("SC_CLK_TCK")
connection.close()
os.kill(pid, signal.SIGINT)

off = max(0, latency - handler_run - handler_wait)
print(
    f"requests={count} bytes={received} latency_ns={latency} cpu_ns={cpu_ns} "
    f"handler_run_ns={handler_run} handler_wait_ns={handler_wait} handler_off_ns={off} "
    f"handler_off_share={off / latency if latency else 0:.4f} paced_ns={paced} "
    f"resident={left} poll_ns={poll_ns}",
    flush=True,
)
