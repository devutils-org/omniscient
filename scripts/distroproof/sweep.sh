#!/bin/sh
# distroproof's `command` (distroproof.toml): the portability matrix's sweep
# on this guest, written to sweep.json, which distroproof brings back
# (`collect`) for scripts/distroproof/cells.py to turn into a cell. The
# program-probe sources run on the static fixtures carried beside the
# archives (scripts/distroproof/fixtures.sh). Standard output keeps the
# summary a person reads.
set -e
/opt/omn/bin/omn selftest --format json --cell "${DP_TARGET:-guest}" \
    --fixtures "$PWD/target/dp-fixtures/$(uname -m)" > sweep.json
/opt/omn/bin/omn list 2>&1 | grep 'sources:'
