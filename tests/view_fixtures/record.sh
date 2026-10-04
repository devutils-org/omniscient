#!/bin/sh
# Records the fixture runs every view is drawn on, through `omn top` itself.
#
#   make && tests/view_fixtures/record.sh [NAME...]
#
# Each fixture is one real run of a workload from `cases/`, live, headless,
# with the sources every view asks for (`omn top --headless -o`): the run's
# results.json, and the view's view.json (what the catalog says of each source
# and what was refused, with why). resolved.json and cost.json are dropped:
# no view reads them, and they would double the size of what is committed.
#
# Each run lasts seconds, not a fraction of one: the views that draw over time
# -- the system view's sparklines, the heatmap's latency over time, the lanes
# -- need a few dozen of a 250 ms poll's points and a spread of events to be
# judged on. `vertical-requests` and `sqlite-latency` are the exceptions: each
# is a decomposition of a few requests or of one program's time, which is
# what they are drawn for.
#
# Recorded once and committed, so every view, every test and every
# demonstration draws the same data. Re-recording changes every snapshot in
# docs/view/gallery/: the view streams' tests then fail until each is looked at
# and blessed (OMN_VIEW_BLESS=1).
#
# Needs the capabilities `make` grants (the eBPF sources), python3 and
# sqlite3. A source this machine cannot serve is recorded as refused, which is
# itself what the views draw it as.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
omn="$root/target/debug/omn"
# On a disk rather than in memory, so a write has a device under it.
work="$root/target/omn-view-fixtures"
rm -rf "$work"
mkdir -p "$work"

# The vertical case's server (`examples/vertical-one-request.yml`): five
# requests whose causes the server injects -- a lock, a cold read, both, a
# client that pauses, a client that reads slowly -- each declared by USDT
# probes, so `net.request` follows each one, and the session's two
# decompositions split each request's time. What the request view draws.
record_vertical_requests() {
    rm -rf "$here/vertical-requests"
    (cd "$root" && "$omn" top --headless -d 20s -f examples/vertical-one-request.yml \
        -o "$here/vertical-requests") >/dev/null
    keep vertical-requests
}

# Whether fixture $1 is among the rest of the arguments, or there are none.
want() {
    wanted=$1
    shift
    [ $# -eq 0 ] && return 0
    for name in "$@"; do [ "$name" = "$wanted" ] && return 0; done
    return 1
}

keep() {
    name=$1
    rm -f "$here/$name/resolved.json" "$here/$name/cost.json"
    # The recording's paths are this checkout's, which say nothing a view
    # draws and differ on every machine.
    sed -i "s|$work|/work|g; s|$root|/omniscient|g" "$here/$name/results.json" "$here/$name/view.json"
    # Compact: the same document without the indentation, a third the size.
    python3 -c 'import json, sys; json.dump(json.load(open(sys.argv[1])), open(sys.argv[1], "w"), separators=(",", ":"))' \
        "$here/$name/results.json"
    echo "recorded $name: $(du -sh "$here/$name" | cut -f1)"
}

# A pure-Python program whose time one function spends (case 3,
# `one-function-dominates`), on a smaller input: on the cpu, for the flame and
# the timebar. Under `-X perf`, so the interpreter writes its perf map and each
# Python function has a name in the profile.
record_cpython_hot() {
    python3 -c "
import random
random.seed(3)
with open('$work/input.txt', 'w') as file:
    for i in range(30000):
        file.write(f'{random.randint(1, 3000)} word{i}\n')
"
    rm -rf "$here/cpython-hot"
    "$omn" top --headless -d 20s -o "$here/cpython-hot" -- \
        python3 -X perf "$root/cases/cpython-interpreter/hot.py" "$work/input.txt" >/dev/null
    keep cpython-hot
}

# The same program on an input whose weights are twice as heavy: the second
# run of a comparison (`omn top --replay cpython-hot --against
# cpython-hot-heavier`), slower where `score` is.
record_cpython_hot_heavier() {
    python3 -c "
import random
random.seed(3)
with open('$work/heavier.txt', 'w') as file:
    for i in range(30000):
        file.write(f'{2 * random.randint(1, 3000)} word{i}\n')
"
    rm -rf "$here/cpython-hot-heavier"
    "$omn" top --headless -d 20s -o "$here/cpython-hot-heavier" -- \
        python3 -X perf "$root/cases/cpython-interpreter/hot.py" "$work/heavier.txt" >/dev/null
    keep cpython-hot-heavier
}

# CPython's http.server under the case's own client (case 4,
# `a-large-listing`), with fewer entries and requests: one connection, a
# thread per connection, the network on loopback.
record_http_server() {
    python3 -c "
import os
os.makedirs('$work/www/big', exist_ok=True)
for i in range(15000):
    open(f'$work/www/big/entry-{i:05d}-<&>.txt', 'w').close()
"
    rm -rf "$here/http-server"
    "$omn" top --headless -d 20s -o "$here/http-server" -- sh -c "
        python3 '$root/cases/http-server-stdlib/client.py' '$work/www' /big/ 25 >/dev/null &
        exec python3 -m http.server -p HTTP/1.1 --bind 127.0.0.1 --directory '$work/www' 0
    " >/dev/null 2>&1
    keep http-server
}

# A script of 40 commits, one row each, in rollback-journal mode with full
# synchronization: every commit waits for the disk. With a second argument,
# each commit is followed by a query that keeps the shell on a cpu for about a
# tenth of a second, so the commits spread over seconds of a program that
# computes between them, without a process or a write more.
commits() {
    {
        echo 'pragma journal_mode=delete; pragma synchronous=full; create table t(x);'
        i=0
        while [ $i -lt 40 ]; do
            echo "insert into t values($i);"
            if [ $# -gt 1 ]; then
                echo "select count(*) from (with recursive c(x) as (select 1 union all select x + 1 from c where x < $2) select x from c);"
            fi
            i=$((i + 1))
        done
    } >"$1"
}

# The sqlite3 shell committing one row at a time in rollback-journal mode
# (case 5, `every-commit-waits-for-the-disk`), fewer commits: each commit
# waits for the disk.
record_sqlite_commits() {
    rm -rf "$here/sqlite-commits"
    commits "$work/commits.sql" 600000
    "$omn" top --headless -d 20s -o "$here/sqlite-commits" -- \
        sqlite3 -init "$work/commits.sql" "$work/commits.db" .quit >/dev/null
    keep sqlite-commits
}

# The same commits, asked the way `omn latency` asks: its session, with the
# decomposition the timebar draws.
record_sqlite_latency() {
    rm -rf "$here/sqlite-latency"
    rm -f "$work/latency.db" "$work/latency.db-journal"
    commits "$work/latency.sql"
    "$omn" latency --objective latency --dump-session --format yaml -- \
        sqlite3 -init "$work/latency.sql" "$work/latency.db" .quit >"$work/latency.yml"
    "$omn" top --headless -d 20s -f "$work/latency.yml" -o "$here/sqlite-latency" >/dev/null
    keep sqlite-latency
}

# The machine itself, no target: what the system view is first for. Twenty
# seconds of whatever this machine is doing, through the system view's own
# needs -- the rows of `docs/view/README.md`'s table of needs, which a test
# holds to the code. Every view's needs machine-wide would be 60 MB for eight
# seconds (the traced sources see every task), and the system view draws
# none of them.
record_machine() {
    rm -rf "$here/machine"
    "$omn" top --dump-session -d 20s >"$work/machine-all.yml"
    python3 - "$root/docs/view/README.md" "$work/machine-all.yml" "$work/machine.yml" <<'PY'
import re, sys
needs = set(re.findall(r"^\| system \| `([^`]+)` \|", open(sys.argv[1]).read(), re.M))
text = open(sys.argv[2]).read()
head, _, body = text.partition("requests:\n")
requests = re.split(r"(?m)^(?=- )", body)
kept = [r for r in requests if r.strip() and re.search(r"(?m)^(?:- |  )source: (\S+)$", r).group(1) in needs]
open(sys.argv[3], "w").write(head + "requests:\n" + "".join(kept))
PY
    "$omn" top --headless -f "$work/machine.yml" -o "$here/machine" >/dev/null
    keep machine
}

# The vertical server (`examples/vertical/server.c`), built here with room for
# a plan longer than its 64 requests, into $1.
build_server() {
    sed 's/^#define MAX_PLAN 64$/#define MAX_PLAN 1024/' "$root/examples/vertical/server.c" >"$work/server.c"
    cc -O0 -pthread -o "$1" "$work/server.c"
}

# The vertical case's abandoned requests (`examples/vertical-abandoned.yml`):
# one request declared and never ended, one ended, one whose answer never
# leaves the stack. What the request view draws as a loss.
record_vertical_abandoned() {
    mkdir -p "$root/target/omn-vertical"
    [ -x "$root/target/omn-vertical/server" ] || build_server "$root/target/omn-vertical/server"
    rm -rf "$here/vertical-abandoned"
    (cd "$root" && "$omn" top --headless -d 20s -f examples/vertical-abandoned.yml \
        -o "$here/vertical-abandoned") >/dev/null
    keep vertical-abandoned
}

# Three hundred requests on the vertical server, its four causes in turn --
# none, a 2 ms lock, 2 cold pages, a client pausing 2 ms -- so the request
# view draws a population rather than a handful: its slowest, its spread, the
# cause of each.
record_many_requests() {
    build_server "$work/server"
    plan=$(python3 -c 'print(" ".join(["0:0", "2:0", "0:2", "0:0:2"][i % 4] for i in range(300)))')
    rm -rf "$here/many-requests"
    # shellcheck disable=SC2086
    "$omn" top --headless -d 30s -o "$here/many-requests" -- \
        "$work/server" "$work/many-data" $plan >/dev/null 2>&1
    keep many-requests
}

# The file server the flows view was judged on (`docs/view/gallery/flows-run/`),
# as a fixture every view is drawn on: bytes per file and per socket asked as
# `raw`, beside the machine's devices and interfaces.
record_file_serving() {
    rm -rf "$here/file-serving"
    "$root/docs/view/gallery/flows-run/record.sh" "$here/file-serving" >/dev/null
    keep file-serving
}

# Every wakeup on the machine for a second, no target: the wake view's
# machine-wide chains, and the lanes it rebuilds threads from.
record_system_wakeup() {
    cat >"$work/system-wakeup.yml" <<YML
name: system wakeups
duration: 1s
requests:
- {source: sched.wakeup, label: sched.wakeup, mode: trace, emit: [raw, summary], target: system}
YML
    rm -rf "$here/system-wakeup"
    "$omn" top --headless -f "$work/system-wakeup.yml" -o "$here/system-wakeup" >/dev/null
    keep system-wakeup
}

# A view's own workload (`view/workloads/<view>/`), launched as its manifest
# says, with every view's needs: the recording `tests/view_truth.rs` holds
# that view to, within each figure's trust.
record_workload() {
    view=$1
    rm -rf "$here/workload-$view"
    if [ "$view" = request ]; then
        mkdir -p "$root/target/omn-view-workloads/request"
        build_server "$root/target/omn-view-workloads/request/server"
    fi
    # The manifest's command, the request server's plan spelled out from its
    # session.
    run=$(python3 -c '
import json, re, shlex, sys
run = json.load(open(sys.argv[1]))["run"]
if "<plan...>" in run:
    plan = re.findall(r"^        - \"([0-9:-]+)\"$", open(sys.argv[2]).read(), re.M)
    run = run[: run.index("<plan...>")] + plan
print(" ".join(shlex.quote(word) for word in run))
' "$root/view/workloads/$view/manifest.json" "$root/view/workloads/$view/session.yml")
    (cd "$root" && eval "\"\$omn\" top --headless -d 30s -o \"\$here/workload-\$view\" -- $run") >/dev/null 2>&1
    keep "workload-$view"
}

WORKLOADS="flame timebar wake flows heatmap ledger system lanes request"

want cpython-hot "$@" && record_cpython_hot
want cpython-hot-heavier "$@" && record_cpython_hot_heavier
want http-server "$@" && record_http_server
want sqlite-commits "$@" && record_sqlite_commits
want sqlite-latency "$@" && record_sqlite_latency
want vertical-requests "$@" && record_vertical_requests
want machine "$@" && record_machine
want vertical-abandoned "$@" && record_vertical_abandoned
want many-requests "$@" && record_many_requests
want file-serving "$@" && record_file_serving
want system-wakeup "$@" && record_system_wakeup
for view in $WORKLOADS; do
    want "workload-$view" "$@" && record_workload "$view"
done
true
