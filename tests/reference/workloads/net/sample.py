"""Run the workload, and a snapshot tool every INTERVAL seconds while it lives.

argv: INTERVAL TOOL... -- WORKLOAD...  For a tool that shows the state of
the moment (`ss -ti`): every reading is printed, one after the other, and
the parser makes one value of them. The workload's output goes to stderr.
"""

import subprocess
import sys
import time

sep = sys.argv.index("--")
interval = float(sys.argv[1])
tool, workload = sys.argv[2:sep], sys.argv[sep + 1 :]
proc = subprocess.Popen(workload, stdout=sys.stderr)
while True:
    time.sleep(interval)
    if proc.poll() is not None:
        break
    sys.stdout.write(subprocess.run(tool, capture_output=True, text=True).stdout)
    sys.stdout.flush()
sys.exit(proc.returncode)
