#!/usr/bin/env python3
"""Tallies the reference pairs each distroproof guest ran.

    scripts/distroproof/reference-tally.py target/matrix/reference.json

Each target of distroproof-reference.toml ran every pair (tests/reference.rs)
with its own distribution's witnesses and brought reference.log back
(`collect`). This prints, per target, the pairs that held, did not hold and
skipped -- the skips grouped by their reason, which names the tool or the
setting the guest lacked -- and every pair that did not hold, with its line.
"""

import collections
import json
import re
import sys


def tally(log):
    held, failed, skipped = [], [], collections.Counter()
    for line in log.splitlines():
        if m := re.match(r"reference (\S+) (\S+): same numbers", line):
            held.append(f"{m.group(1)} {m.group(2)}")
        elif m := re.match(r"skipped: reference (\S+) (\S+): (.*)", line):
            reason = re.sub(r"`[^`]*`", "`…`", m.group(3))[:90]
            skipped[reason] += 1
        elif m := re.match(r"\s+(\S+) \((.*?)\): (.*)", line):
            if "did not hold" not in line:
                failed.append(f"{m.group(1)}: {m.group(3)[:160]}")
    return held, failed, skipped


def main(path):
    report = json.load(open(path))
    for target in report["targets"]:
        logs = [p for p in target.get("collected") or [] if p.endswith("reference.log")]
        if not logs:
            print(f"{target['name']}: no log ({target.get('outcome')}, {target.get('error')})")
            continue
        held, failed, skipped = tally(open(logs[0]).read())
        print(f"{target['name']}: {len(held)} held, {len(failed)} did not hold, "
              f"{sum(skipped.values())} skipped")
        for line in failed:
            print(f"    did not hold  {line}")
        for reason, count in skipped.most_common():
            print(f"    {count:3d} skipped  {reason}")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
