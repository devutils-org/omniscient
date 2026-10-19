"""Start a tool that runs until interrupted, and interrupt it after SECONDS.

argv: SECONDS TOOL...  For a tool that counts while it runs and prints its
tally on SIGINT (`tcpdump`'s "N packets captured"). Run `alongside` omn
over omn's `--duration`; the tool's stdout and stderr pass through. `$SUDO`
(the runner's `sudo -n`, `reference.sudo`) goes in front of the tool alone.
sudo 1.8 (Ubuntu 18.04) runs as real uid 0, so this script cannot signal it
(EPERM) and tcpdump outlived it, holding the runner's pipe open: the
interrupt then goes through `$SUDO kill`, and a sudo that refuses it says so.
The tool has a session of its own, since sudo relays no signal sent from
its own process group.
"""

import os
import signal
import subprocess
import sys
import time

sudo = os.environ.get("SUDO", "").split()
seconds, tool = float(sys.argv[1]), sudo + sys.argv[2:]
watcher = subprocess.Popen(tool, start_new_session=True)
time.sleep(seconds)
try:
    watcher.send_signal(signal.SIGINT)
except PermissionError:
    if not sudo or subprocess.call(sudo + ["kill", "-INT", str(watcher.pid)]) != 0:
        sys.exit("bracket.py: " + sys.argv[2] + " (pid " + str(watcher.pid) + ") cannot be interrupted")
sys.exit(watcher.wait(timeout=10))
