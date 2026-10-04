"""Run a snapshot tool every INTERVAL seconds for SECONDS.

argv: INTERVAL SECONDS TOOL...  For a tool that shows the state of the
moment (`ss -ti`): every reading is printed, one after the other, and the
parser makes one value of them. Run `alongside` omn, over omn's
`--duration`: the readings taken before the workload starts or after it
ends find nothing, and print nothing.
"""

import subprocess
import sys
import time

interval, seconds, tool = float(sys.argv[1]), float(sys.argv[2]), sys.argv[3:]
end = time.monotonic() + seconds
while True:
    time.sleep(interval)
    if time.monotonic() > end:
        break
    sys.stdout.write(subprocess.run(tool, capture_output=True, text=True).stdout)
    sys.stdout.flush()
