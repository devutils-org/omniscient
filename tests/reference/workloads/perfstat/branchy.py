"""Branches on a seeded pseudo-random stream, so the predictor misses about
half of them and the miss count depends on the argument only."""

import sys

n = int(sys.argv[1]) if len(sys.argv) > 1 else 2_000_000
x = 12345
taken = 0
for _ in range(n):
    x = (x * 1103515245 + 12345) & 0x7FFFFFFF
    if x & 0x10000:
        taken += 1
