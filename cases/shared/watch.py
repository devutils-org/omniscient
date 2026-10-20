"""Watches a program's process tree from outside, and optionally competes with it.

    python3 watch.py PROGRAM MARKER [CPU COMPETITORS]

A reference for a case whose program is many processes (`case.yml`), kept
here once for every case that calls it (`{case}/../shared/watch.py`). It
waits for a process whose program is PROGRAM and whose command line names
MARKER -- the target, once the tool has released it from its gate -- and
from then on reads, every millisecond, the `/proc/<pid>/task/*/schedstat` of
the target and of every process under it (`/proc/<pid>/task/*/children`):
the kernel's own account of each task's time on a cpu and time waiting for
one. It never touches any of them.

Before anything else it ends what an earlier run left behind: every process
whose command line names MARKER, or whose working directory is under it,
other than this one and its ancestors. Until the eighteenth wave a run the
tool's budget stopped ended the target's main process and left its children
running (`ps -C cc1` after the stop); `omn` now ends the process group, and
this stays as the guard for a program whose children left it: they would
compete for the contended cpu, write the objects and reports the next run
writes, and be found here as the next run's target. MARKER is the case's
scratch directory, which holds the test's pid, so nothing of another run or
another checkout is touched.

PROGRAM is the name the target's first word goes by when it is found. A
target that execs from a shell (`sh -c '... && exec sqlite3 ...'`) is
watched by the program it execs: a shell that lives a few milliseconds can
come and go between two passes over /proc on a loaded machine, and a watcher
waiting for it would never find its target.

With CPU and COMPETITORS, it then starts COMPETITORS processes that spin on
cpu CPU and nothing else, and keeps itself off that cpu: the injection of a
case whose cause is a cpu shared with others. The target is launched on that
cpu by the case (`taskset -c CPU`), an affinity given at exec, the program
unchanged. The competitors die with this process. It refuses to run where
CPU is the only cpu it may use: it would be a third competitor, uncounted.

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
`run_delay_ns` add up every task seen, the target's own included: for a
target that runs one child at a time (a compiler driver), `run_delay_ns` is
the whole chain's wait for a cpu. `swept` is how many processes of an
earlier run were ended first. `poll_ns` is the longest gap between two
reads: a figure here is short of the truth by at most what a task did in its
last such gap, and a process that lived less than one may not be here at all.
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



def ancestors(pid):
    found = set()
    while pid > 1:
        found.add(pid)
        try:
            with open(f"/proc/{pid}/stat") as file:
                pid = int(file.read().rpartition(")")[2].split()[1])
        except (OSError, IndexError, ValueError):
            break
    return found


def leftover(pid):
    """A process of an earlier run: it names MARKER, or works under it."""
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as file:
            if marker.encode() in file.read():
                return True
        cwd = os.readlink(f"/proc/{pid}/cwd")
    except OSError:
        return False
    return cwd == marker or cwd.startswith(marker.rstrip("/") + "/")


def alive(pid):
    try:
        with open(f"/proc/{pid}/stat") as file:
            return file.read().rpartition(")")[2].split()[0] != "Z"
    except (OSError, IndexError):
        return False


mine = ancestors(me)
swept = [
    int(entry)
    for entry in os.listdir("/proc")
    if entry.isdigit() and int(entry) not in mine and leftover(entry)
]
for pid in swept:
    try:
        os.kill(pid, signal.SIGKILL)
    except OSError:
        pass
gone_by = time.monotonic() + 10
while any(alive(pid) for pid in swept):
    if time.monotonic() > gone_by:
        sys.exit(f"processes of an earlier run outlived SIGKILL: {swept}")
    time.sleep(0.01)

spinning = []
if cpu is not None:
    elsewhere = os.sched_getaffinity(0) - {cpu}
    if not elsewhere:
        sys.exit(f"cpu {cpu} is the only cpu this process may use: watching from it would compete")
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


def still_running(pid):
    """Neither gone nor a zombie."""
    try:
        with open(f"/proc/{pid}/stat") as file:
            return file.read().rpartition(")")[2].split()[0] not in ("Z", "X")
    except (OSError, IndexError):
        return False


def schedstat(pid):
    """The process's tasks' time on a cpu and waiting for one, summed.

    None when no task's account could be read: a process exiting between the
    listing of its tasks and the read of their files answers with nothing,
    and a nothing taken for zero once erased a 3.8 s compile from its job's
    account (`cases/make-sqlite/case.yml`, the refiled finding).
    """
    cpu_ns = delay_ns = 0
    read = 0
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
            read += 1
        except (OSError, IndexError, ValueError):
            pass
    return (cpu_ns, delay_ns) if read else None


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
        # A zombie is gone for this account: its parent may reap it late, or,
        # once the tool that launched it has exited, whoever inherits it.
        if alive is None or not still_running(target):
            break
        walk = [(target, None)]
        while walk:
            pid, job = walk.pop()
            stat = alive if pid == target else schedstat(pid)
            if stat is None:
                continue
            entry = seen.setdefault(pid, [job, now, now, 0, 0])
            # A process's account only grows: a read that finds less is one
            # that lost a task on its way out, and the larger stands.
            entry[2] = now
            entry[3], entry[4] = max(entry[3], stat[0]), max(entry[4], stat[1])
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
    "swept": len(swept),
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
