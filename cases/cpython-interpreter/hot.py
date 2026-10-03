"""A pure-Python program whose time one function spends, by its input.

    python3 hot.py INPUT

Case 3's on-CPU workload (`case.yml`, `one-function-dominates`). INPUT is a
text file of `weight word` lines, written by the case's setup. The program
reads it (`load`), scores every line (`score`: a pure-Python loop of `weight`
steps per line), and keeps the ten best (`rank`). The input is built so that
the weights sum to about 12 million steps over 2 000 lines, which makes
`score` the program's time by construction: `load` and `rank` touch each line
once.

It prints its own account on stdout, one line, from its own clocks around the
three functions (the interpreter's start and exit are outside it):

    best=V cpu_ns=N real_ns=R

`best` is the highest score, printed so the work cannot be skipped and two
runs on the same input can be compared; `cpu_ns` is
`time.process_time_ns()` (user plus system time of the process), `real_ns`
is `time.monotonic_ns()`, CLOCK_MONOTONIC.
"""

import sys
import time


def load(path):
    rows = []
    with open(path) as file:
        for line in file:
            weight, word = line.split()
            rows.append((int(weight), word))
    return rows


def score(rows):
    scored = []
    for weight, word in rows:
        value = 0
        for step in range(weight):
            value = (value * 31 + step + len(word)) % 1_000_003
        scored.append((value, word))
    return scored


def rank(scored):
    return sorted(scored, reverse=True)[:10]


def main():
    cpu, real = time.process_time_ns(), time.monotonic_ns()
    best = rank(score(load(sys.argv[1])))
    cpu, real = time.process_time_ns() - cpu, time.monotonic_ns() - real
    print(f"best={best[0][0]} cpu_ns={cpu} real_ns={real}", flush=True)


main()
