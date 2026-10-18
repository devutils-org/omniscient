#!/bin/sh
# distroproof's `command` (distroproof.toml): the portability matrix's sweep
# on this guest, as one JSON document on standard output. The host turns the
# matrix's report into cells (scripts/distroproof/cells.py) for
# tests/portability.rs to judge.
# The program-probe sources run on the static fixtures
# (scripts/distroproof/fixtures.sh), the payload carried beside the archives.
exec /opt/omn/bin/omn selftest --format json --fixtures "$PWD/target/dp-fixtures"
