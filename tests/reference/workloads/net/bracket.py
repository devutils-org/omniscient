"""Start a tool that runs until interrupted, and interrupt it after SECONDS.

argv: SECONDS TOOL...  For a tool that counts while it runs and prints its
tally on SIGINT (`tcpdump`'s "N packets captured"). Run `alongside` omn
over omn's `--duration`; the tool's stdout and stderr pass through. `$SUDO`
(the runner's `sudo -n`, `reference.sudo`) goes in front of the tool alone.
"""

import os
import signal
import subprocess
import sys
import time

seconds, tool = float(sys.argv[1]), os.environ.get("SUDO", "").split() + sys.argv[2:]
watcher = subprocess.Popen(tool)
time.sleep(seconds)
watcher.send_signal(signal.SIGINT)
sys.exit(watcher.wait(timeout=10))
