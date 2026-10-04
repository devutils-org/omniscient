"""Allocate MIB mebibytes, touch every page, hold them SECONDS, then exit.

The resident set is then a known quantity: the touched buffer plus the
interpreter, on whichever node the first touch placed it.
"""

import sys
import time

mib = int(sys.argv[1]) if len(sys.argv) > 1 else 256
seconds = float(sys.argv[2]) if len(sys.argv) > 2 else 3.0
buf = bytearray(mib << 20)
for i in range(0, len(buf), 4096):
    buf[i] = 1
time.sleep(seconds)
