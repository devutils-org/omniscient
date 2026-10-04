"""Block and wake COUNT times (a 1 ms sleep each), then exit.

Every sleep is one voluntary context switch of the one thread, so the
count a per-process switch counter reads is close to COUNT.
"""

import sys
import time

count = int(sys.argv[1]) if len(sys.argv) > 1 else 1000
for _ in range(count):
    time.sleep(0.001)
