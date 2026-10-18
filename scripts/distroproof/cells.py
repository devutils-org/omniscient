#!/usr/bin/env python3
"""Turns a distroproof matrix report into the portability matrix's cells.

    scripts/distroproof/cells.py target/matrix/distroproof.json

Each target's command wrote `omn selftest --format json` to sweep.json
(scripts/distroproof/sweep.sh), which distroproof brought back (`collect`).
That document is a cell; this names it
after the distroproof target, says which configuration the target was, and
writes it where tests/portability.rs reads cells
(target/matrix/cells/<target>/sweep.json), with distroproof's envprobe report
of the same guest beside it (envprobe.json), for the judge that holds omn's
reasons against envprobe's facts. A target that did not print a cell is
listed with why, and makes the script exit 1.
"""

import json
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


def config_of(target):
    """What the target was made to be, as the matrix names configurations."""
    variant = target.get("variant") or {}
    name = target["name"]
    if variant.get("user") == "root":
        return "root"
    # distroproof's agent warns when the kernel refused the file capabilities
    # (`file_caps`): one before 5.8 has neither CAP_PERFMON nor CAP_BPF.
    if any("file_caps:" in w for w in target.get("warnings") or []):
        return "nocaps"
    for suffix in ("paranoid3", "kptr2", "lockdown", "nopmu"):
        if name.endswith("-" + suffix):
            return suffix
    if variant.get("pmu") is False:
        return "nopmu"
    return "caps"


def main(path):
    cells = os.environ.get("OMN_MATRIX_CELLS", os.path.join(ROOT, "target/matrix/cells"))
    report = json.load(open(path))
    failed = []
    for target in report["targets"]:
        name = target["name"]
        command = target.get("command") or {}
        collected = [p for p in target.get("collected") or [] if p.endswith("/sweep.json")]
        stdout = open(collected[0]).read() if collected else command.get("stdout") or ""
        try:
            sweep = json.loads(stdout)
        except json.JSONDecodeError:
            setup = (target.get("setup") or {}).get("stderr", "").strip().splitlines()[-3:]
            why = target.get("error") or (command.get("stderr") or "").strip()[-300:] \
                or " | ".join(setup) or f"outcome {target.get('outcome')}, no cell printed"
            failed.append((name, why))
            continue
        sweep["cell"]["name"] = name
        sweep["cell"]["config"] = config_of(target)
        directory = os.path.join(cells, name)
        os.makedirs(directory, exist_ok=True)
        with open(os.path.join(directory, "sweep.json"), "w") as out:
            json.dump(sweep, out, indent=2)
        if target.get("envprobe") is not None:
            with open(os.path.join(directory, "envprobe.json"), "w") as out:
                json.dump(target["envprobe"], out, indent=2)
        usable = sum(1 for s in sweep["sources"] if s["here"] == "yes")
        print(f"{name}: {sweep['cell']['kernel_release']}, {sweep['cell']['config']}, "
              f"{len(sweep['sources'])} sources, {usable} listed usable")
    for name, why in failed:
        print(f"{name}: no cell: {why}", file=sys.stderr)
    return 1 if failed else 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    sys.exit(main(sys.argv[1]))
