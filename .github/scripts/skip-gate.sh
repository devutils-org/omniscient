#!/usr/bin/env bash
# Fails when the suite skipped for a reason this cell is not expected to have.
#
#   .github/scripts/skip-gate.sh <test log> <expected skips>...
#
# Several expected files are read as one: a cell's own list is added to the
# common one rather than copied from it.
#
# A skip in this repository is a line that starts `skipped:` or `SKIPPED`
# (after indentation), printed by the test that skipped and saying why. The
# expected file holds one extended regular expression per line, each matched
# against the reason; a line that matches none is an unexpected skip, and the
# gate fails naming it. Each class of reason is counted, so a class that
# suddenly skips three times as often is visible in the log even when it is
# allowed.
#
# The reasons are matched rather than the test names because a test's name
# says what it checks and its skip line says what the machine lacked, and only
# the second is a property of this cell.

set -euo pipefail

log=${1:?usage: skip-gate.sh <test log> <expected skips>...}
shift
[ $# -gt 0 ] || { echo "usage: skip-gate.sh <test log> <expected skips>..." >&2; exit 1; }
[ -r "$log" ] || { echo "skip-gate: no test log at $log" >&2; exit 1; }
expected="$*"

patterns=$(mktemp)
trap 'rm -f "$patterns"' EXIT
cat "$@" | grep -vE '^[[:space:]]*(#|$)' > "$patterns"

# A skip that quotes a refusal puts the refusal's reasons on the indented lines
# under it -- `error: ... has no viable backend`, then `  bpf:raw_tp: bpf:
# missing: ... CAP_BPF` -- so each skip is read with up to eight indented lines
# after it, joined into one line, and the reason is matched against all of it.
skips=$(awk '
    function flush() { if (record != "") print record; record = ""; extra = 0 }
    /^[[:space:]]*(skipped|SKIPPED)([^[:alnum:]_]|$)/ {
        flush(); record = $0; sub(/^[[:space:]]+/, "", record); next
    }
    record != "" && /^[[:space:]]+[^[:space:]]/ && extra < 8 {
        line = $0; sub(/^[[:space:]]+/, "", line); record = record " | " line; extra++; next
    }
    { flush() }
    END { flush() }
' "$log")
total=$(printf '%s' "$skips" | grep -c . || true)
echo "skip-gate: $total skip line(s) in $log"

unexpected=$(printf '%s\n' "$skips" | grep . | grep -vEf "$patterns" || true)

# Per class, the count, in the order the file gives them.
while IFS= read -r pattern; do
    count=$(printf '%s\n' "$skips" | grep -cE -- "$pattern" || true)
    printf '  %5d  %s\n' "$count" "$pattern"
done < "$patterns"

if [ -n "$unexpected" ]; then
    echo
    echo "skip-gate: $(printf '%s\n' "$unexpected" | grep -c .) skip(s) with a reason this cell is not expected to have:"
    printf '%s\n' "$unexpected" | cut -c1-400 | sort | uniq -c | sort -rn | sed 's/^/  /'
    echo
    echo "skip-gate: either the runner lacks something the workflow should install, or a test"
    echo "           skips for a reason that is not a missing privilege or device. Add a pattern"
    echo "           to $expected only for the second kind, with the reason it is expected."
    exit 1
fi
echo "skip-gate: every skip names a privilege or a device this cell does not have"
