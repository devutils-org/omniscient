"""Writes N single-row INSERT statements, one commit each.

    python3 commits.py FILE N [WIDTH]

The input of case 2 (`case.yml`): the sqlite3 shell reads FILE with `.read`,
and since no statement opens a transaction, every INSERT is a transaction of
its own -- N commits, each as durable as the database's settings make it.
Each row's text is WIDTH characters (64 when not given).
"""

import sys

path, count = sys.argv[1], int(sys.argv[2])
width = int(sys.argv[3]) if len(sys.argv) > 3 else 64
with open(path, "w") as file:
    for row in range(count):
        file.write(f"INSERT INTO t(v) VALUES (printf('%0{width}d', {row}));\n")
