"""Sleeps a fixed number of times: each sleep is one voluntary switch away
from the task, so the context-switch count follows the argument."""

import sys
import time

n = int(sys.argv[1]) if len(sys.argv) > 1 else 2000
for _ in range(n):
    time.sleep(0.0001)
