#!/usr/bin/env python3
"""Run every pair of catalog/references/top.yaml, interpreting the family's
own fields (same_run: onpid_attach, compare.omn_jq) exactly as the YAML's
header documents them. A stopgap until tests/reference.rs runs pairs, and
the reading of those fields the runner is asked to match; not part of the
suite.

    python3 tests/reference/workloads/top/run_pairs.py . [ROUNDS]

Needs top, pmap, jq, python3-yaml and a built target/debug/omn (`make`);
a pair whose `requires` is not on PATH is reported `skip`. The parsing is a
copy of tests/reference/top.rs's parsers, reduced to the keys the pairs
compare."""
import json, os, shutil, subprocess, sys, tempfile, yaml

repo = sys.argv[1]
rounds = int(sys.argv[2]) if len(sys.argv) > 2 else 1
omn = os.path.abspath(os.path.join(repo, "target/debug/omn"))
os.chdir(repo)
pairs = yaml.safe_load(open("catalog/references/top.yaml"))
KIB = 1024.0


def top_size(f):
    scale = {"k": 1, "m": KIB, "g": KIB**2, "t": KIB**3}.get(f[-1])
    return float(f[:-1] if scale else f) * (scale or 1) * KIB


def top_batch_pid(out):
    header = last = None
    for line in out.splitlines():
        w = line.split()
        if w[:1] == ["PID"]:
            header = w
        elif header and w and w[0].isdigit():
            last = w
    col = lambda n: last[header.index(n)]
    m, s = col("TIME+").split(":")
    return {"res": top_size(col("RES")), "time": (float(m) * 60 + float(s)) * 1e9,
            "cpu": float(col("%CPU")) / 100}


def top_batch_cpus(out):
    line = [l for l in out.splitlines() if l.startswith("%Cpu(s):")][-1]
    v = {}
    for part in line[len("%Cpu(s):"):].split(","):
        n, state = part.split()
        v[state] = float(n) / 100
    v["busy"] = 1 - v["id"] - v["wa"]
    return v


def pmap_x_total(out):
    line = next(l for l in out.splitlines() if l.startswith("total kB"))
    return {"": float(line.split()[3]) * KIB}


def pmap_XX_total(out):
    lines = out.splitlines()
    header = next(l for l in lines if l.split()[:1] == ["Address"]).split()
    totals = [float(f) for f in next(l for l in reversed(lines) if l.rstrip().endswith("KB")).split()[:-1]]
    v = {k.lower(): t * KIB for k, t in zip(header[header.index("Size"):], totals)}
    v["uss"] = v["private_clean"] + v["private_dirty"]
    return v


def smem_uss_pss_rss(out):
    rows = [l for l in out.splitlines() if l.strip()]
    if len(rows) != 1:
        raise ValueError(f"smem matched {len(rows)} processes")
    uss, pss, rss = (float(f) * KIB for f in rows[0].split())
    return {"uss": uss, "pss": pss, "rss": rss}


parsers = {f.__name__: f for f in (top_batch_pid, top_batch_cpus, pmap_x_total, pmap_XX_total, smem_uss_pss_rss)}


def jq(expr, doc):
    out = subprocess.run(["jq", expr], input=doc, capture_output=True, text=True, check=True)
    return float(out.stdout)


fails = 0
for i, p in enumerate(pairs):
    c, ref = p["compare"], p["reference"]
    missing = [r for r in ref["requires"] if not shutil.which(r)]
    if missing:
        print(f"{i:2} {p['source']:15} {p['use'][:50]:50} skip: {' '.join(missing)} not on PATH")
        continue
    for r in range(rounds):
        if p.get("same_run") == "onpid_attach":
            with tempfile.NamedTemporaryFile("r", suffix=".json") as f:
                env = {**os.environ, "ONPID_OMN": json.dumps([omn] + p["omn"]), "ONPID_OMN_OUT": f.name}
                run = subprocess.run(ref["command"] + p["workload"], env=env, capture_output=True, text=True)
                odoc = f.read()
            rout = run.stdout
        else:
            odoc = subprocess.run([omn] + p["omn"] + ["--"] + p["workload"], capture_output=True, text=True).stdout
            rout = subprocess.run(ref["command"] + p["workload"], capture_output=True, text=True).stdout
        try:
            want = parsers[ref["parse"]](rout)[c["key"]]
        except Exception as e:
            want = None
            print(f"   parse: {e}: {rout[:300]!r}", file=sys.stderr)
        try:
            got = jq(c["omn_jq"], odoc)
        except Exception:
            got = None
            print(f"   omn: {odoc[:300]!r}", file=sys.stderr)
        tol = c["tolerance"]
        if want is None or got is None:
            ok, d = False, "n/a"
        elif "rel" in tol:
            ok, d = abs(got - want) <= tol["rel"] * abs(want), f"{(got - want) / want * 100:+.2f}%"
        else:
            ok, d = abs(got - want) <= tol["abs"], f"{got - want:+.4g}"
        fails += not ok
        print(f"{i:2} {p['source']:15} {p['use'][:50]:50} omn={got!s:>14.14} ref={want!s:>14.14} "
              f"d={d:>9} tol={tol} {'ok' if ok else 'FAIL'}", flush=True)
print(f"{fails} failing")
sys.exit(1 if fails else 0)
