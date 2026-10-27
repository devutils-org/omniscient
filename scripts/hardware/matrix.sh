#!/bin/sh
# The hardware cells: the portability matrix's sweep on machines already
# running -- bare metal a VM cannot stand in for (Intel PT, topdown, uncore,
# resctrl, thermal_throttle, a GPU) -- through distroproof's ssh targets.
#
#   scripts/release.sh && scripts/distroproof/fixtures.sh
#   scripts/hardware/matrix.sh               # target/hardware/hosts, see hosts.example
#   cargo test --test portability            # the judges, the hardware one included
#
# The matrix is written from target/hardware/hosts and distroproof.toml's
# own [run], so a hardware cell runs exactly what a guest runs; the cells
# land beside the guests' in target/matrix/cells/<name>/. The host keeps what
# setup installs (/opt/omn) and nothing else: distroproof removes its work
# directory and the test user it created.
set -eu
cd "$(dirname "$0")/../.."
hosts=${OMN_HARDWARE_HOSTS:-target/hardware/hosts}
distroproof=${DISTROPROOF:-$(command -v distroproof || echo ../distroproof/target/release/distroproof)}
if [ ! -s "$hosts" ]; then
    echo "no hosts: write $hosts (see scripts/hardware/hosts.example)" >&2
    exit 2
fi
mkdir -p target/hardware target/matrix
matrix=target/hardware/distroproof.toml
{
    echo "# Written by scripts/hardware/matrix.sh from $hosts; do not edit."
    echo "[defaults]"
    echo 'timeout = "40m"'
    grep -v '^[[:space:]]*\(#\|$\)' "$hosts" | while read -r name host port identity; do
        echo
        echo "[[target]]"
        echo "name = \"$name\""
        echo "ssh = \"$host\""
        if [ -n "${port:-}" ]; then echo "port = $port"; fi
        if [ -n "${identity:-}" ]; then echo "identity = \"$identity\""; fi
    done
    echo
    # The guests' own [run], from the checkout's root.
    sed -n '/^\[run\]$/,$p' distroproof.toml | sed '1a workdir = "../.."'
} >"$matrix"
"$distroproof" check -f "$matrix"
status=0
"$distroproof" up --no-tui -f "$matrix" --json target/matrix/distroproof-hardware.json || status=$?
scripts/distroproof/cells.py target/matrix/distroproof-hardware.json
exit $status
