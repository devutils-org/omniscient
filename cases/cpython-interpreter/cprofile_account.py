"""cProfile's account of `hot.py`: which function the time is in.

    python3 profile.py HOT_PY INPUT

Runs HOT_PY's `main` on INPUT under `cProfile`, the profiler CPython ships,
and prints, on one line, each of the workload's three functions' own time
(`tottime`, the time in the function and not in what it calls) and its share
of their sum:

    score_ns=N score_share=S load_ns=... load_share=... rank_ns=... rank_share=...

It is a separate run of the same program on the same input: cProfile's own
cost inflates every function's time, so its figures say *which* function the
time is in, not how long the measured run took.
"""

import cProfile
import pstats
import runpy
import sys

hot, data = sys.argv[1], sys.argv[2]
sys.argv = [hot, data]
profiler = cProfile.Profile()
profiler.enable()
runpy.run_path(hot, run_name="__main__")
profiler.disable()

stats = pstats.Stats(profiler).stats
own = {name: 0.0 for name in ("load", "score", "rank")}
for (file, _line, function), (_cc, _nc, tottime, _ct, _callers) in stats.items():
    if file == hot and function in own:
        own[function] += tottime
total = sum(own.values()) or 1.0
print(
    " ".join(
        f"{name}_ns={int(seconds * 1e9)} {name}_share={seconds / total:.4f}"
        for name, seconds in own.items()
    ),
    flush=True,
)
