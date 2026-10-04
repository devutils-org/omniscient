"""Run a counter-reading tool, then the workload, then the tool again.

argv: TOOL... -- WORKLOAD...  The two readings are printed apart by a
marker line, so a parser can take the difference: the increment a tool
that only prints totals (`ip -s link`, `nstat -a`) shows over one run. The
workload's own output goes to stderr, out of the tool's way.
"""

import subprocess
import sys

MARKER = "==== omn-reference: the workload ran here ===="

sep = sys.argv.index("--")
tool, workload = sys.argv[1:sep], sys.argv[sep + 1 :]
sys.stdout.write(subprocess.run(tool, check=True, capture_output=True, text=True).stdout)
print(MARKER, flush=True)
subprocess.run(workload, check=True, stdout=sys.stderr)
sys.stdout.write(subprocess.run(tool, check=True, capture_output=True, text=True).stdout)
