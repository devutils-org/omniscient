"""Start a tool that runs until interrupted, run the workload, interrupt it.

argv: TOOL... -- WORKLOAD...  For a tool that counts while it runs and
prints its tally on SIGINT (`tcpdump`'s "N packets captured"). The tool is
given a second to open its socket first; its stdout and stderr pass
through, the workload's output goes to stderr.
"""

import signal
import subprocess
import sys
import time

sep = sys.argv.index("--")
tool, workload = sys.argv[1:sep], sys.argv[sep + 1 :]
watcher = subprocess.Popen(tool)
time.sleep(1.0)
subprocess.run(workload, check=True, stdout=sys.stderr)
time.sleep(0.3)
watcher.send_signal(signal.SIGINT)
watcher.wait(timeout=10)
