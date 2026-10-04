#!/usr/bin/env python3
"""Run every pair of catalog/references/perfstat.yaml, interpreting the
family's own fields (same_run, setup, compare.omn_jq) exactly as the YAML's
header documents them. A stopgap until tests/reference.rs runs pairs, and
the reading of those fields the runner is asked to match; not part of the
suite.

    python3 tests/reference/workloads/perfstat/run_pairs.py . [ROUNDS]

Needs perf, jq, python3-yaml and a built target/debug/omn (`make`). The
perf stat CSV reading is a copy of tests/reference/perfstat.rs's
perf_stat_csv, reduced to what the pairs key on."""
import json, os, subprocess, sys, time, yaml

repo = sys.argv[1]
rounds = int(sys.argv[2]) if len(sys.argv) > 2 else 1
omn = os.path.join(repo, "target/debug/omn")
os.chdir(repo)
pairs = yaml.safe_load(open("catalog/references/perfstat.yaml"))


def key(raw):
    raw = raw.strip()
    if "/" in raw:
        raw = raw.split("/", 1)[1].rstrip("/")
    return raw.split(":")[0]


def parse_csv(stderr):
    v = {}
    for line in stderr.splitlines():
        f = line.split(",")
        if len(f) < 3 or f[0].strip().startswith("<"):
            continue
        try:
            x = float(f[0])
        except ValueError:
            continue
        k = key(f[2])
        v[k] = v.get(k, 0.0) + x
    if "instructions" in v and v.get("cycles"):
        v["ipc"] = v["instructions"] / v["cycles"]
    if v.get("task-clock"):
        for k in list(v):
            if k not in ("task-clock", "ipc") and "." not in k:
                v[k + ".per_cpu_ns"] = v[k] / (v["task-clock"] * 1e6)
    return v


def jq(expr, doc):
    out = subprocess.run(["jq", expr], input=doc, capture_output=True, text=True, check=True)
    return float(out.stdout)


def setup(kind):
    """-> (substitutions, teardown)"""
    if kind is None:
        return {}, lambda: None
    spin = subprocess.Popen(["python3", "-S", "tests/reference/workloads/perfstat/spin.py", "1000000000"],
                            env={**os.environ, "PYTHONHASHSEED": "0"})
    if kind == "spin_pid":
        return {"{pid}": str(spin.pid)}, lambda: (spin.kill(), spin.wait())
    if kind == "cgroup_spin":
        own = open("/proc/self/cgroup").read().strip().split("::", 1)[1]
        rel = own.rstrip("/").rsplit("/", 1)[0].lstrip("/") + f"/omn-ref-{os.getpid()}"
        path = "/sys/fs/cgroup/" + rel
        os.mkdir(path)
        open(path + "/cgroup.procs", "w").write(str(spin.pid))

        def down():
            spin.kill(); spin.wait(); time.sleep(0.2); os.rmdir(path)
        return {"{cgroup}": rel, "{cgroup_abs}": path}, down
    raise SystemExit(f"unknown setup {kind}")


def sub(argv, s):
    out = []
    for a in argv:
        for k, v in s.items():
            a = a.replace(k, v)
        out.append(a)
    return out


fails = 0
for i, p in enumerate(pairs):
    c = p["compare"]
    for r in range(rounds):
        s, down = setup(p.get("setup"))
        try:
            oargs = [omn] + sub(p["omn"], s)
            rcmd = sub(p["reference"]["command"], s)
            if p.get("same_run") == "tool_wraps_omn":
                run = subprocess.run(rcmd + oargs, capture_output=True, text=True)
                odoc, rerr = run.stdout, run.stderr
            else:
                o = subprocess.run(oargs + ["--"] + p["workload"], capture_output=True, text=True)
                odoc = o.stdout
                rerr = subprocess.run(rcmd + p["workload"], capture_output=True, text=True).stderr
        finally:
            down()
        ref = parse_csv(rerr).get(c["key"])
        try:
            got = jq(c["omn_jq"], odoc)
        except Exception as e:
            got = None
            print(odoc[:400], file=sys.stderr)
        tol = c["tolerance"]["rel"]
        if ref is None or got is None:
            ok, d = False, float("nan")
        else:
            d = (got - ref) / ref
            ok = abs(d) <= tol
        fails += not ok
        print(f"{i:2} {p['source']:22} {p['use'][:52]:52} omn={got!s:>22} ref={ref!s:>22} "
              f"d={d*100:+7.2f}% tol={tol*100:.0f}% {'ok' if ok else 'FAIL'}", flush=True)
print(f"{fails} failing")
sys.exit(1 if fails else 0)
