"""Writes N single-row INSERT statements, one commit each.

    python3 commits.py FILE N

The input of case 2 (`case.yml`): the sqlite3 shell reads FILE with `.read`,
and since no statement opens a transaction, every INSERT is a transaction of
its own -- N commits, each as durable as the database's settings make it.
"""

import sys

path, count = sys.argv[1], int(sys.argv[2])
with open(path, "w") as file:
    for row in range(count):
        file.write(f"INSERT INTO t(v) VALUES (printf('%064d', {row}));\n")
