"""Maps an anonymous region and writes one byte per page, a few passes over
it: one page fault per page on the first pass, and last-level misses on the
following ones once the region outgrows the cache. Transparent huge pages
are refused for the process: under THP `always` (Arch) a run would fault in
2 MiB pages for some of the region and not others, so separate runs counted
33669 and 37735 faults (distroproof, 2026-10-19)."""

import ctypes
import mmap
import sys

PR_SET_THP_DISABLE = 41
# prctl rather than mmap.madvise, which Python 3.6 (Ubuntu 18.04) lacks.
if ctypes.CDLL(None, use_errno=True).prctl(PR_SET_THP_DISABLE, 1, 0, 0, 0) != 0:
    sys.exit("touch.py: prctl(PR_SET_THP_DISABLE): errno " + str(ctypes.get_errno()))

mib = int(sys.argv[1]) if len(sys.argv) > 1 else 256
passes = int(sys.argv[2]) if len(sys.argv) > 2 else 4
size = mib << 20
page = mmap.PAGESIZE
region = mmap.mmap(-1, size, flags=mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS)
for p in range(passes):
    for off in range(0, size, page):
        region[off] = p & 0xFF
