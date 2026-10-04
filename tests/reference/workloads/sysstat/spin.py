"""Spin one cpu for SECONDS, then exit: a known share of one core."""

import sys
import time

seconds = float(sys.argv[1]) if len(sys.argv) > 1 else 3.0
end = time.monotonic() + seconds
n = 0
while time.monotonic() < end:
    n += 1
