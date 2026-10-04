"""Runs a pid-taking tool on a launched workload, for the `top` family
(tests/reference/top.rs):

    onpid.py [--at ready|done] [--delay SECONDS] TOOL ARGS... -- WORKLOAD...

It launches the workload, waits for the line `--at` names on its stderr
("ready" by default; `hold.py` says "ready" once its memory is touched and
"done" once its cpu time is spent), waits `--delay` seconds more (default
0.5), runs the tool with every `{pid}` in its arguments replaced by the
workload's pid, prints the tool's stdout and stderr as its own, reaps the
workload and exits with the tool's status."""

import subprocess
import sys
import time

argv = sys.argv[1:]
want, delay = "ready", 0.5
while argv and argv[0] in ("--at", "--delay"):
    if len(argv) < 2:
        sys.exit(f"{argv[0]} takes a value")
    if argv[0] == "--at":
        want = argv[1]
    else:
        delay = float(argv[1])
    argv = argv[2:]
if "--" not in argv:
    sys.exit("usage: onpid.py [--at LINE] [--delay S] TOOL ARGS... -- WORKLOAD...")
cut = argv.index("--")
tool, workload = argv[:cut], argv[cut + 1 :]

child = subprocess.Popen(workload, stderr=subprocess.PIPE, text=True)
for line in child.stderr:
    if line.strip() == want:
        break
else:
    child.wait()
    sys.exit(f"the workload exited without saying {want!r}")
time.sleep(delay)
pid = str(child.pid)
out = subprocess.run([a.replace("{pid}", pid) for a in tool], capture_output=True, text=True)
sys.stdout.write(out.stdout)
sys.stderr.write(out.stderr)
child.stderr.close()
child.wait()
sys.exit(out.returncode)
