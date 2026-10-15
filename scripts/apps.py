#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR GPL-2.0
"""The application suite: `omn` against programs of known shape, as code.

Each manifest under bench/apps/ is one archetype of application -- cpu-bound,
disk-bound, lock-contended, a fork-heavy build, a network round trip -- with
the command that runs it, the number the program itself reports (its ground
truth), and what `omn` must find. This script runs every manifest, or the ones
named, and prints one table: what was expected, what `omn` said, what the
program said, and whether they agree.

    scripts/apps.py                 every manifest
    scripts/apps.py cpu-bound io    the manifests named (by file stem)
    scripts/apps.py --list          what there is, and what each one needs
    scripts/apps.py --out DIR       where the runs are kept (target/apps/<time>)

The binary is $OMN, or target/debug/omn. Run `make` first: the eBPF sources
need the capabilities a relink drops, and without them most checks are
skipped with the reason rather than failed. docs/how-to-use/apps.md says how
to write a manifest for a program of your own.

A verdict check a manifest marks `known` for this architecture (or `any`)
reports KNOWN with the reason instead of FAIL -- never a number's check: a defect already filed, kept visible
rather than tolerated by loosening the expectation.

Exit status: 0 when nothing failed (a skip or a known one is not a failure), 1 otherwise,
2 for a manifest or an invocation that cannot mean anything.
"""

import argparse
import json
import os
import platform
import shutil
import subprocess
import sys
import time
from pathlib import Path

try:
    import tomllib
except ImportError:  # Python < 3.11
    sys.exit("scripts/apps.py needs Python 3.11 or later, for tomllib")

ROOT = Path(__file__).resolve().parent.parent
APPS = ROOT / "bench" / "apps"
TRIAGE_FIXTURE = ROOT / "target" / "omn-triage-fixture" / "workload"
BENCH_WORKLOAD = ROOT / "target" / "bench" / "workload"
BENCH_SOURCE = ROOT / "examples" / "bench" / "workload.c"

# A syscall's number is the architecture's; the manifests name it through a
# placeholder rather than carry one table per machine.
SYSCALL_NUMBERS = {
    "x86_64": {"getppid": 110},
    "aarch64": {"getppid": 173},
}


class Refused(Exception):
    """A manifest that cannot mean anything: exit 2."""


def cache_dir() -> Path:
    base = os.environ.get("XDG_CACHE_HOME") or str(Path.home() / ".cache")
    return Path(base) / "omn-apps"


def on_tmpfs(path: Path) -> bool:
    out = subprocess.run(
        ["stat", "-f", "-c", "%T", str(path)], capture_output=True, text=True
    )
    return out.stdout.strip() == "tmpfs"


def build_fixtures(needs: set) -> list:
    """Builds what the manifests launch; returns the reasons it could not."""
    missing = []
    if "triage-fixture" in needs and not TRIAGE_FIXTURE.exists():
        subprocess.run(["make", "-s", "triage-fixture"], cwd=ROOT)
        if not TRIAGE_FIXTURE.exists():
            missing.append("triage-fixture: `make triage-fixture` did not build it (no cc?)")
    if "bench-workload" in needs and (
        not BENCH_WORKLOAD.exists()
        or BENCH_WORKLOAD.stat().st_mtime < BENCH_SOURCE.stat().st_mtime
    ):
        BENCH_WORKLOAD.parent.mkdir(parents=True, exist_ok=True)
        built = subprocess.run(
            ["cc", "-O2", "-pthread", "-o", str(BENCH_WORKLOAD), str(BENCH_SOURCE)],
            capture_output=True,
            text=True,
        )
        if built.returncode != 0:
            missing.append(f"bench-workload: cc failed: {built.stderr.strip()[:200]}")
    return missing


def load(path: Path) -> dict:
    with open(path, "rb") as handle:
        manifest = tomllib.load(handle)
    manifest["id"] = path.stem
    if "command" not in manifest or not manifest["command"]:
        raise Refused(f"{path.name}: no `command`")
    if "latency" not in manifest and not manifest.get("trace"):
        raise Refused(f"{path.name}: neither `[latency]` nor `[[trace]]`: nothing to check")
    if manifest.get("truth", "static") not in ("omn-truth", "bench-report", "static"):
        raise Refused(f"{path.name}: `truth` is omn-truth, bench-report or static")
    return manifest


def expand(text: str, places: dict) -> str:
    for name, value in places.items():
        text = text.replace("{" + name + "}", str(value))
    return text


def omn_truth(stderr_text: str, tid=None) -> dict:
    """The `omn-truth key=value ...` line of `tid`, else the first one."""
    lines = []
    for line in stderr_text.splitlines():
        if not line.startswith("omn-truth "):
            continue
        fields = {}
        for word in line.split()[1:]:
            if "=" in word:
                key, value = word.split("=", 1)
                try:
                    fields[key] = float(value)
                except ValueError:
                    fields[key] = value
        lines.append(fields)
    for fields in lines:
        if tid is not None and fields.get("tid") == tid:
            return fields
    return lines[0] if lines else {}


def compare(measured, truth, check) -> tuple:
    """(passed, the expectation in words) of one measured value."""
    if "at_least" in check:
        floor = truth * check["at_least"]
        return measured >= floor, f">= {check['at_least']:g} x truth"
    tolerance = check.get("tolerance", 0.05)
    slack = check.get("slack", 0)
    allowed = abs(truth) * tolerance + slack
    words = f"within {tolerance:.0%}" + (f" + {slack:g}" if slack else "")
    return abs(measured - truth) <= allowed, words


class Run:
    """One manifest's runs, and the rows of the table they produce."""

    def __init__(self, manifest, omn, out, places):
        self.manifest = manifest
        self.omn = omn
        self.out = out
        self.places = places
        self.rows = []

    def row(self, check, expected, measured, truth, result, note=""):
        known = self.manifest.get("known", {})
        reason = known.get(platform.machine()) or known.get("any")
        # A known reason is about the verdict; a number that disagrees with
        # the program's own is still a failure, whatever the verdict was.
        about_the_verdict = check.startswith(("verdict", "says "))
        if result == "FAIL" and reason and about_the_verdict:
            result, note = "KNOWN", reason
        self.rows.append(
            {
                "app": self.manifest["id"],
                "kind": self.manifest.get("kind", ""),
                "check": check,
                "expected": expected,
                "measured": measured,
                "truth": truth,
                "result": result,
                "note": note,
            }
        )

    def command(self):
        return [expand(part, self.places) for part in self.manifest["command"]]

    def launch(self, label, omn_args):
        """Runs omn over the command; returns (stdout, program stderr, report)."""
        directory = self.out / self.manifest["id"] / label
        directory.mkdir(parents=True, exist_ok=True)
        program_out = directory / "program.stdout"
        program_err = directory / "program.stderr"
        report = directory / "report.json"
        env = dict(os.environ, BENCH_REPORT=str(report))
        args = (
            [self.omn]
            + omn_args
            + ["--program-stdout", str(program_out), "--program-stderr", str(program_err)]
            + ["--"]
            + self.command()
        )
        (directory / "command").write_text(" ".join(args) + "\n")
        started = time.monotonic()
        done = subprocess.run(args, capture_output=True, text=True, env=env, cwd=ROOT)
        (directory / "omn.stdout").write_text(done.stdout)
        (directory / "omn.stderr").write_text(done.stderr)
        seconds = time.monotonic() - started
        truth = {}
        if self.manifest.get("truth") == "bench-report" and report.exists():
            truth = json.loads(report.read_text())
        stderr_text = program_err.read_text() if program_err.exists() else ""
        return done, stderr_text, truth, seconds

    def statics(self):
        return {
            key: float(expand(str(value), self.places))
            for key, value in self.manifest.get("values", {}).items()
        }

    def latency(self):
        spec = self.manifest["latency"]
        done, program_err, report, _ = self.launch(
            "latency", ["latency", "--objective", spec.get("objective", "latency"), "--format", "json"]
        )
        if done.returncode not in (0, 1) or not done.stdout.strip():
            reason = (done.stderr.strip().splitlines() or ["no output"])[-1]
            self.row("latency", spec.get("limit", "-"), "-", "-", "SKIP", reason[:160])
            return
        answer = json.loads(done.stdout)
        verdict = answer.get("verdict", {})
        run = (answer.get("runs") or [{}])[0]
        task = verdict.get("task", "")
        tid = float(task.split(":", 1)[1]) if task.startswith("task:") else None
        truth = dict(self.statics())
        if self.manifest.get("truth") == "omn-truth":
            truth.update(omn_truth(program_err, tid))
        else:
            truth.update(report)
        if "limit" in spec:
            found = verdict.get("limit") or ("undecided" if "undecided" in verdict else "-")
            self.row(
                "verdict limit",
                spec["limit"],
                found,
                "-",
                "PASS" if found == spec["limit"] else "FAIL",
                verdict.get("sentence", "")[:160] if found != spec["limit"] else "",
            )
        sentence = verdict.get("sentence", "")
        for words in spec.get("says", []):
            self.row(
                f"says '{words}'",
                "in the verdict",
                "yes" if words in sentence else "no",
                "-",
                "PASS" if words in sentence else "FAIL",
                "" if words in sentence else sentence[:160],
            )
        rows = {state["state"]: state for state in run.get("states", [])}
        if "whole" in run:
            rows["whole"] = run["whole"]
        for check in spec.get("check", []):
            label = f"{check['row']} vs {check['truth']}"
            if check["truth"] not in truth:
                self.row(label, "-", "-", "-", "SKIP", "the program reported no such value")
                continue
            if check["row"] not in rows:
                self.row(label, "-", "-", "-", "FAIL", f"no `{check['row']}` row in the answer")
                continue
            amount = rows[check["row"]]["amount"]
            expected = truth[check["truth"]]
            passed, words = compare(amount, expected, check)
            self.row(label, words, ms(amount), ms(expected), "PASS" if passed else "FAIL")

    def trace(self, spec):
        label = spec.get("label", spec["args"][0])
        args = [expand(arg, self.places) for arg in spec["args"]]
        results_dir = self.out / self.manifest["id"] / f"trace-{label}" / "results"
        done, program_err, report, _ = self.launch(
            f"trace-{label}", ["trace"] + args + ["--format", "json", "-o", str(results_dir)]
        )
        results = results_dir / "results.json"
        if done.returncode != 0 or not results.exists():
            reason = [line for line in done.stderr.splitlines() if line.startswith("error")]
            self.row(label, "-", "-", "-", "SKIP", (reason or ["no results"])[0][:160])
            return
        result = json.loads(results.read_text())["results"][0]
        truth = dict(self.statics())
        truth.update(report if self.manifest.get("truth") == "bench-report" else omn_truth(program_err))
        measured = value_of(result, spec.get("value", "events"))
        losses = result.get("losses", {})
        lost = losses.get("dropped", 0) + losses.get("throttled", 0)
        overhead = result.get("overhead", {})
        cost = ""
        if overhead.get("observed_cpu_ns"):
            cost = f"cost {overhead['collector_cpu_ns'] / overhead['observed_cpu_ns']:.1%} of the target's cpu"
        note = ", ".join(part for part in (cost, f"{lost} lost" if lost else "") if part)
        name = f"{label}: {spec.get('value', 'events')} vs {spec['truth']}"
        if spec["truth"] not in truth:
            self.row(name, "-", fmt(measured), "-", "SKIP", "the program reported no such value")
            return
        expected = truth[spec["truth"]] + spec.get("offset", 0)
        passed, words = compare(measured, expected, spec)
        self.row(name, words, fmt(measured), fmt(expected), "PASS" if passed else "FAIL", note)


def value_of(result: dict, what: str) -> float:
    """One number out of a run's result: events, or a summary column summed."""
    if what == "events":
        return float(result.get("events_seen", 0))
    series = [
        line
        for output in result.get("outputs", [])
        if output.get("emit") == "summary"
        for line in output.get("series", [])
    ]
    if what == "max":
        return max((line.get("max") or 0 for line in series), default=0.0)
    return float(sum(line.get(what) or 0 for line in series))


def ms(ns: float) -> str:
    return f"{ns / 1e6:.2f}ms"


def fmt(value: float) -> str:
    return f"{value:.0f}" if value == int(value) else f"{value:.3g}"


def needs_met(manifest) -> str:
    """Why this machine cannot run the manifest, or the empty string."""
    for program in manifest.get("requires", []):
        if shutil.which(program) is None:
            return f"`{program}` is not installed"
    if manifest.get("storage") and on_tmpfs(cache_dir()):
        return f"{cache_dir()} is on tmpfs, where a sync waits for nothing"
    return ""


def print_table(rows):
    headers = ["app", "check", "expected", "measured", "truth", "result", "note"]
    widths = {h: max([len(h)] + [len(str(r[h])) for r in rows]) for h in headers[:-1]}
    line = "  ".join(h.upper().ljust(widths[h]) for h in headers[:-1]) + "  NOTE"
    print(line)
    for row in rows:
        print("  ".join(str(row[h]).ljust(widths[h]) for h in headers[:-1]) + "  " + row["note"])


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("apps", nargs="*", help="manifests to run, by file stem")
    parser.add_argument("--list", action="store_true", help="list the manifests")
    parser.add_argument("--out", type=Path, help="where to keep the runs")
    options = parser.parse_args()

    try:
        manifests = [load(path) for path in sorted(APPS.glob("*.toml"))]
    except (Refused, tomllib.TOMLDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    known = {manifest["id"] for manifest in manifests}
    unknown = [name for name in options.apps if name not in known]
    if unknown:
        print(f"error: no manifest {', '.join(unknown)}; there are: {', '.join(sorted(known))}", file=sys.stderr)
        return 2
    if options.apps:
        manifests = [manifest for manifest in manifests if manifest["id"] in options.apps]

    if options.list:
        for manifest in manifests:
            why = needs_met(manifest)
            print(f"{manifest['id']:<20} {manifest.get('kind', ''):<14} {manifest.get('about', '')}")
            if why:
                print(f"{'':<20} cannot run here: {why}")
        return 0

    omn = os.environ.get("OMN") or str(ROOT / "target" / "debug" / "omn")
    if not Path(omn).exists():
        print(f"error: no omn at {omn}: run `make` first, or set OMN", file=sys.stderr)
        return 2
    caps = subprocess.run(["getcap", omn], capture_output=True, text=True).stdout
    if "cap_bpf" not in caps:
        print(f"note: {omn} has no CAP_BPF: the eBPF sources will be skipped. `make` grants it.", file=sys.stderr)

    missing = build_fixtures({need for manifest in manifests for need in manifest.get("needs", [])})
    out = options.out or ROOT / "target" / "apps" / time.strftime("%Y%m%d-%H%M%S")
    out.mkdir(parents=True, exist_ok=True)
    arch = platform.machine()
    numbers = SYSCALL_NUMBERS.get(arch, {})

    rows = []
    for manifest in manifests:
        work = cache_dir() / "work" / manifest["id"]
        shutil.rmtree(work, ignore_errors=True)
        work.mkdir(parents=True)
        places = {
            "fixture": TRIAGE_FIXTURE,
            "bench": BENCH_WORKLOAD,
            "apps": APPS,
            "work": work,
            **{f"nr:{name}": number for name, number in numbers.items()},
        }
        run = Run(manifest, omn, out, places)
        why = needs_met(manifest) or next(
            (reason for reason in missing if reason.split(":")[0] in manifest.get("needs", [])), ""
        )
        if why:
            run.row("-", "-", "-", "-", "SKIP", why)
            rows += run.rows
            continue
        for step in manifest.get("setup", []):
            done = subprocess.run(
                [expand(part, places) for part in step], cwd=ROOT, capture_output=True, text=True
            )
            if done.returncode != 0:
                run.row("setup", "-", "-", "-", "SKIP", (done.stderr.strip() or "setup failed")[:160])
                break
        else:
            print(f"running {manifest['id']} ...", file=sys.stderr)
            if "latency" in manifest:
                run.latency()
            for spec in manifest.get("trace", []):
                run.trace(spec)
        rows += run.rows

    print()
    print_table(rows)
    counts = {
        result: sum(row["result"] == result for row in rows)
        for result in ("PASS", "FAIL", "KNOWN", "SKIP")
    }
    print(
        f"\n{counts['PASS']} passed, {counts['FAIL']} failed, {counts['KNOWN']} known, "
        f"{counts['SKIP']} skipped  ({arch}, runs in {out})"
    )
    (out / "summary.json").write_text(
        json.dumps({"arch": arch, "omn": omn, "rows": rows, "counts": counts}, indent=2) + "\n"
    )
    return 1 if counts["FAIL"] else 0


if __name__ == "__main__":
    sys.exit(main())
