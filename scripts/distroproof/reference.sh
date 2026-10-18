#!/bin/sh
# distroproof's `command` for distroproof-reference.toml: every reference pair
# (tests/reference.rs) on this distribution, one at a time, with its own
# witnesses, against the release a user installed. The log is brought back
# (`collect`); scripts/distroproof/reference-tally.py reads it.
OMN_REFERENCE_ROOT="$PWD" OMN_REFERENCE_BIN=/opt/omn/bin/omn \
    ./target/dp-reference/reference --test-threads=1 --nocapture pairs_ > reference.log 2>&1
grep -E '^test result' reference.log
exit 0
