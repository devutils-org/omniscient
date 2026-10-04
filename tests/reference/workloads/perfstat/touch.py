"""Maps an anonymous region and writes one byte per page, a few passes over
it: one page fault per page on the first pass, and last-level misses on the
following ones once the region outgrows the cache."""

import mmap
import sys

mib = int(sys.argv[1]) if len(sys.argv) > 1 else 256
passes = int(sys.argv[2]) if len(sys.argv) > 2 else 4
size = mib << 20
page = mmap.PAGESIZE
region = mmap.mmap(-1, size, flags=mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS)
for p in range(passes):
    for off in range(0, size, page):
        region[off] = p & 0xFF
