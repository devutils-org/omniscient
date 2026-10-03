"""Reads gcc's own account of a compile, and of the same compile at -O0.

    python3 ftime.py REPORT -- GCC_ARGS...

The reference of case 5's cause on the cpu (`case.yml`). REPORT is what the
measured compile printed under `-ftime-report` (the case hands its stderr to
that file); GCC_ARGS is the same compile at `-O0`, which this script runs
itself, with `-ftime-report`, as the second of the two builds whose
difference is the cause. It prints one line:

    o2_total_ns=T o2_opt_ns=O o2_parsing_ns=P o0_total_ns=T0 o0_opt_ns=O0 ratio=R

`total` is the report's `TOTAL` line, cc1's own wall clock from its first
phase to its last; `opt` its `phase opt and generate`, the passes and the code
generation; `parsing` its `phase parsing`. `ratio` is the -O2 compile's total
over the -O0 one's. gcc 16 prints each to the hundredth of a second, so each
figure is good to 10 ms per line read.
"""

import subprocess
import sys

report_path = sys.argv[1]
o0 = sys.argv[sys.argv.index("--") + 1 :]


def read(text):
    figures = {}
    for line in text.splitlines():
        name, colon, rest = line.partition(":")
        if not colon:
            continue
        words = rest.split()
        if not words:
            continue
        try:
            seconds = float(words[0])
        except ValueError:
            continue
        key = {
            "TOTAL": "total_ns",
            "phase opt and generate": "opt_ns",
            "phase parsing": "parsing_ns",
        }.get(name.strip())
        if key:
            figures[key] = round(seconds * 1e9)
    if "total_ns" not in figures:
        sys.exit(f"no `TOTAL` line in gcc's -ftime-report: {text!r}")
    return figures


with open(report_path) as file:
    o2 = read(file.read())
baseline = subprocess.run(o0, capture_output=True, text=True, check=True)
o0_figures = read(baseline.stderr)

account = {f"o2_{key}": value for key, value in o2.items()}
account.update({f"o0_{key}": value for key, value in o0_figures.items()})
account["ratio"] = o2["total_ns"] / max(o0_figures["total_ns"], 1)
print(" ".join(f"{key}={value}" for key, value in account.items()))
