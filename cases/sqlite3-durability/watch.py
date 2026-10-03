"""Watches a program's process tree from outside, and optionally competes with it.

    python3 watch.py PROGRAM MARKER [CPU COMPETITORS]

A reference for a case whose program is many processes (`case.yml`): it
waits for a process whose program is PROGRAM and whose command line names
MARKER -- the target, once the tool has released it from its gate -- and
from then on reads, every millisecond, the `/proc/<pid>/task/*/schedstat` of
the target and of every process under it (`/proc/<pid>/task/*/children`):
the kernel's own account of each task's time on a cpu and time waiting for
one. It never touches any of them.

With CPU and COMPETITORS, it first starts COMPETITORS processes that spin on
cpu CPU and nothing else, and keeps itself off that cpu: the injection of a
case whose cause is a cpu shared with others. The target is launched on that
cpu by the case (`taskset -c CPU`), an affinity given at exec, the program
unchanged. The competitors die with this process.

It says `ready` once the competitors spin, and when the target is gone it
prints its account on one line:

    target_seen=1 jobs=J poll_ns=P target_life_ns=L longest_life_ns=...

Every job is a process the target started itself (a compiler driver, a
recipe), with everything under it: `longest_*` is the job that lived longest,
its life, the cpu its whole subtree used (`longest_cpu_ns`) and the time that
subtree waited for a cpu (`longest_run_delay_ns`); `longest_share` is its life
over the target's, `longest_waited_share` its waiting over its waiting and
running. `target_*` is the target's own process: its life from the first
read to the last, its cpu, its wait for a cpu, and `target_blocked_ns`, the
rest of its life, neither on a cpu nor waiting for one. `cpu_ns` and
`run_delay_ns` add up every task seen, the target's own included. `poll_ns` is the longest gap between two reads: a figure here
is short of the truth by at most what a task did in its last such gap, and a
process that lived less than one may not be here at all.
"""

import ctypes
import os
import signal
import sys
import time

program, marker = sys.argv[1], sys.argv[2]
cpu = int(sys.argv[3]) if len(sys.argv) > 3 else None
competitors = int(sys.argv[4]) if len(sys.argv) > 4 else 0
me = os.getpid()

PR_SET_PDEATHSIG = 1
libc = ctypes.CDLL(None, use_errno=True)

spinning = []
if cpu is not None:
    for _ in range(competitors):
        child = os.fork()
        if child == 0:
            libc.prctl(PR_SET_PDEATHSIG, signal.SIGKILL)
            if os.getppid() != me:
                os._exit(0)
            os.sched_setaffinity(0, {cpu})
            while True:
                pass
        spinning.append(child)
    elsewhere = os.sched_getaffinity(0) - {cpu}
    if elsewhere:
        os.sched_setaffinity(0, elsewhere)
print("ready", flush=True)


def argv_of(pid):
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as file:
            return file.read().split(b"\0")
    except OSError:
        return []


def find_target():
    for entry in os.listdir("/proc"):
        if not entry.isdigit() or int(entry) in (me, *spinning):
            continue
        argv = argv_of(entry)
        if (
            argv
            and os.path.basename(argv[0]).decode(errors="replace") == program
            and any(marker.encode() in arg for arg in argv[1:])
        ):
            return int(entry)
    return None


def children(pid):
    found = []
    try:
        tasks = os.listdir(f"/proc/{pid}/task")
    except OSError:
        return found
    for task in tasks:
        try:
            with open(f"/proc/{pid}/task/{task}/children") as file:
                found.extend(int(child) for child in file.read().split())
        except OSError:
            pass
    return found


def schedstat(pid):
    """The process's tasks' time on a cpu and waiting for one, summed."""
    cpu_ns = delay_ns = 0
    try:
        tasks = os.listdir(f"/proc/{pid}/task")
    except OSError:
        return None
    for task in tasks:
        try:
            with open(f"/proc/{pid}/task/{task}/schedstat") as file:
                fields = file.read().split()
            cpu_ns += int(fields[0])
            delay_ns += int(fields[1])
        except (OSError, IndexError, ValueError):
            pass
    return cpu_ns, delay_ns


deadline = time.monotonic() + 120
target = None
while target is None and time.monotonic() < deadline:
    target = find_target()
    if target is None:
        time.sleep(0.001)

# pid -> [job, first seen, last seen, cpu, delay]; a job is the target's own
# child that the process is, or is under.
seen = {}
poll_ns = 0
if target is not None:
    seen[target] = [None, time.monotonic_ns(), time.monotonic_ns(), 0, 0]
    last = time.monotonic_ns()
    while True:
        now = time.monotonic_ns()
        poll_ns = max(poll_ns, now - last)
        last = now
        alive = schedstat(target)
        if alive is None:
            break
        walk = [(target, None)]
        while walk:
            pid, job = walk.pop()
            stat = alive if pid == target else schedstat(pid)
            if stat is None:
                continue
            entry = seen.setdefault(pid, [job, now, now, 0, 0])
            entry[2], entry[3], entry[4] = now, stat[0], stat[1]
            for child in children(pid):
                walk.append((child, child if pid == target else job))
        time.sleep(0.001)

for child in spinning:
    os.kill(child, signal.SIGKILL)
    os.waitpid(child, 0)

jobs = {}
for pid, (job, first, last_seen, cpu_ns, delay_ns) in seen.items():
    if job is None:
        continue
    life = jobs.setdefault(job, [0, 0, 0])
    if pid == job:
        life[0] = last_seen - first
    life[1] += cpu_ns
    life[2] += delay_ns

account = {
    "target_seen": int(target is not None),
    "jobs": len(jobs),
    "poll_ns": poll_ns,
    "competitors": len(spinning),
    "cpu_ns": sum(entry[3] for entry in seen.values()),
    "run_delay_ns": sum(entry[4] for entry in seen.values()),
}
if target is not None:
    life_ns = seen[target][2] - seen[target][1]
    account["target_life_ns"] = life_ns
    account["target_cpu_ns"] = seen[target][3]
    account["target_run_delay_ns"] = seen[target][4]
    # Neither on a cpu nor waiting for one: blocked, by the kernel's account.
    account["target_blocked_ns"] = max(life_ns - seen[target][3] - seen[target][4], 0)
if jobs:
    life, cpu_ns, delay_ns = max(jobs.values())
    account.update(
        longest_life_ns=life,
        longest_cpu_ns=cpu_ns,
        longest_run_delay_ns=delay_ns,
        longest_share=life / max(account.get("target_life_ns", 0), 1),
        longest_waited_share=delay_ns / max(cpu_ns + delay_ns, 1),
    )
print(" ".join(f"{key}={value}" for key, value in account.items()), flush=True)
