"""Runs a pid-taking tool on a launched workload, for the `top` family
(tests/reference/top.rs):

    onpid.py [--at LINE] [--omn-at LINE] [--delay SECONDS] TOOL ARGS... -- WORKLOAD...

It launches the workload, waits for the line `--at` names on its stderr
("ready" by default; `hold.py` says "ready" once its memory is touched and
"done" once its cpu time is spent), waits `--delay` seconds more (default
0.5), runs the tool with every `{pid}` in its arguments replaced by the
workload's pid, prints the tool's stdout and stderr as its own, reaps the
workload and exits with the tool's status.

One run observed by both (a pair's `same_run: onpid_attach`): when the
environment carries `ONPID_OMN`, a JSON list holding omn's full argv (its
path first; `{pid}` replaced as in the tool's), omn is started on the same
workload when the line `--omn-at` names is seen ("ready" by default), before
the tool; its stdout goes to the file `ONPID_OMN_OUT` names, its stderr to
onpid.py's. Both are awaited, and the exit status is the first non-zero of
the tool's and omn's."""

import json
import os
import subprocess
import sys
import time

argv = sys.argv[1:]
want, omn_at, delay = "ready", "ready", 0.5
while argv and argv[0] in ("--at", "--omn-at", "--delay"):
    if len(argv) < 2:
        sys.exit(f"{argv[0]} takes a value")
    if argv[0] == "--at":
        want = argv[1]
    elif argv[0] == "--omn-at":
        omn_at = argv[1]
    else:
        delay = float(argv[1])
    argv = argv[2:]
if "--" not in argv:
    sys.exit("usage: onpid.py [--at LINE] [--omn-at LINE] [--delay S] TOOL ARGS... -- WORKLOAD...")
cut = argv.index("--")
tool, workload = argv[:cut], argv[cut + 1 :]
omn_argv = json.loads(os.environ["ONPID_OMN"]) if "ONPID_OMN" in os.environ else None
if omn_argv is not None and "ONPID_OMN_OUT" not in os.environ:
    sys.exit("ONPID_OMN needs ONPID_OMN_OUT, the file omn's stdout goes to")

child = subprocess.Popen(workload, stderr=subprocess.PIPE, text=True)
pid = str(child.pid)
omn = None
seen = set()
for line in child.stderr:
    line = line.strip()
    seen.add(line)
    if omn_argv is not None and omn is None and line == omn_at:
        out = open(os.environ["ONPID_OMN_OUT"], "w")
        omn = subprocess.Popen([a.replace("{pid}", pid) for a in omn_argv], stdout=out)
    if want in seen and (omn_argv is None or omn is not None):
        break
else:
    child.wait()
    if omn is not None:
        omn.wait()
    sys.exit(f"the workload exited without saying {want!r} (and {omn_at!r} for omn)")
time.sleep(delay)
out = subprocess.run([a.replace("{pid}", pid) for a in tool], capture_output=True, text=True)
sys.stdout.write(out.stdout)
sys.stderr.write(out.stderr)
child.stderr.close()
child.wait()
status = out.returncode
if omn is not None:
    status = status or omn.wait()
sys.exit(status)
