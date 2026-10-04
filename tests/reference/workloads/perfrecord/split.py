"""Spend a run's cpu time in two C libraries, hashing then compressing, in
shares fixed by wall time: perf record and omn see the same leaf symbols."""
import hashlib
import sys
import time
import zlib

seconds = float(sys.argv[1]) if len(sys.argv) > 1 else 2.0
buf = bytes(range(256)) * 4096
end = time.monotonic() + seconds * 0.6
while time.monotonic() < end:
    hashlib.sha256(buf).digest()
end = time.monotonic() + seconds * 0.4
while time.monotonic() < end:
    zlib.compress(buf, 9)
