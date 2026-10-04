"""Start a tool that runs until interrupted, and interrupt it after SECONDS.

argv: SECONDS TOOL...  For a tool that counts while it runs and prints its
tally on SIGINT (`tcpdump`'s "N packets captured"). Run `alongside` omn
over omn's `--duration`; the tool's stdout and stderr pass through.
"""

import signal
import subprocess
import sys
import time

seconds, tool = float(sys.argv[1]), sys.argv[2:]
watcher = subprocess.Popen(tool)
time.sleep(seconds)
watcher.send_signal(signal.SIGINT)
sys.exit(watcher.wait(timeout=10))
