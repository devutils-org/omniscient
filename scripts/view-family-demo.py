#!/usr/bin/env python3
"""Open recorded resource-family examples without live collection privileges."""
import argparse
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
SCENARIOS = {
    "cpu": ("workload-flame", "CPU profile and a changing CPU load", "hottest"),
    "memory": ("workload-ledger", "64 MiB allocation and resident-memory growth", "MiB"),
    "disk": ("sqlite-commits", "SQLite writes, fsyncs and latency", "p99"),
    "network": ("workload-flows", "known socket and file transfers", "MiB/s"),
    "gpu": ("workload-lanes", "GPU kernels and memory from a recorded GPU run", "VRAM"),
    "energy": ("workload-system", "machine power during a two-process workload", " W"),
    "time": ("workload-timebar", "CPU, sleep, lock and scheduler waits", "lock"),
}


def command(family, headless=False):
    fixture, _, _ = SCENARIOS[family]
    args = [str(ROOT / "target/debug/omn"), "top", "--replay",
            str(ROOT / "tests/view_fixtures" / fixture), "--view", "family-" + family]
    if headless:
        args += ["--headless", "--size", "120x40"]
    return args


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("family", nargs="?", choices=SCENARIOS)
    parser.add_argument("--check", action="store_true", help="check each recorded family page headlessly")
    args = parser.parse_args()
    binary = ROOT / "target/debug/omn"
    if not binary.is_file():
        parser.error("build the worktree first: make")
    if args.check:
        failed = False
        for family, (fixture, _, expected) in SCENARIOS.items():
            result = subprocess.run(command(family, True), capture_output=True, text=True)
            ok = result.returncode == 0 and expected in result.stdout and "Sources " in result.stdout
            print(f"{'OK' if ok else 'FAIL'} {family:8} {fixture}: expected {expected!r}")
            if not ok:
                failed = True
                print(result.stderr or result.stdout)
        return int(failed)
    family = args.family
    if family is None:
        print("Recorded family examples (no collection permissions needed):")
        names = list(SCENARIOS)
        for i, name in enumerate(names, 1):
            print(f"  {i}. {name:8} {SCENARIOS[name][1]}")
        try:
            selection = input("Family number (q to quit): ").strip()
            if selection.lower() == "q":
                return 0
            number = int(selection)
            if not 1 <= number <= len(names):
                raise ValueError
            family = names[number - 1]
        except (ValueError, EOFError):
            print("Invalid selection", file=sys.stderr)
            return 1
    os.chdir(ROOT)
    os.execv(str(binary), command(family))


if __name__ == "__main__":
    sys.exit(main())
