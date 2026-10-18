#!/bin/sh
# The reference runner (tests/reference.rs) as a binary a distroproof guest
# runs: linked against glibc 2.17 through zig, as the release is, and copied
# to target/dp-reference/reference. In the guest it reads the pairs and
# workloads from $OMN_REFERENCE_ROOT and runs the release at
# $OMN_REFERENCE_BIN (scripts/distroproof/reference.sh).
set -eu
cd "$(dirname "$0")/../.."
target="$(uname -m)-unknown-linux-gnu.2.17"
binary=$(cargo zigbuild --release --locked --test reference --target "$target" \
    --message-format=json 2>/dev/null \
    | jq -r 'select(.executable != null and .target.name == "reference") | .executable' \
    | tail -n 1)
[ -n "$binary" ] || { echo "reference-build: the runner did not build" >&2; exit 1; }
mkdir -p target/dp-reference
cp "$binary" target/dp-reference/reference
echo "reference-build: target/dp-reference/reference ($(objdump -T target/dp-reference/reference \
    | grep -oE 'GLIBC_[0-9.]+' | sort -Vu | tail -n 1))"
