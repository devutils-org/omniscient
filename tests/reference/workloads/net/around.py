"""Read a counter tool, wait, then read it again.

argv: SECONDS TOOL...  The two readings are printed apart by a marker line,
so a parser can take the difference: the increment a tool that only prints
totals (`ip -s link`, `nstat -a`) shows over SECONDS. The pair runs this
`alongside` omn: both start together, the runner starts the workload
`settle_ms` later, and SECONDS is omn's `--duration`, so the two windows
cover the same run.
"""

import subprocess
import sys
import time

MARKER = "==== omn-reference: the workload ran here ===="

seconds, tool = float(sys.argv[1]), sys.argv[2:]
sys.stdout.write(subprocess.run(tool, check=True, capture_output=True, text=True).stdout)
print(MARKER, flush=True)
time.sleep(seconds)
sys.stdout.write(subprocess.run(tool, check=True, capture_output=True, text=True).stdout)
