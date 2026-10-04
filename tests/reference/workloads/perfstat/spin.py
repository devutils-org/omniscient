"""A fixed amount of integer work: the instruction count depends on the
argument only, so two separate runs retire nearly the same number."""

import sys

n = int(sys.argv[1]) if len(sys.argv) > 1 else 5_000_000
acc = 0
for i in range(n):
    acc = (acc + i * 7) & 0xFFFF
