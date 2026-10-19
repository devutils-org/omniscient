#!/bin/sh
# Build companions without changing native host packages or granting capabilities.
set -eu
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
export CMAKE_BUILD_PARALLEL_LEVEL=${CMAKE_BUILD_PARALLEL_LEVEL:-2}
cd "$(dirname "$0")/../.."
chain_root=$PWD
xpac_repo=${XPAC_REPO:-../xpac}
atfq_repo=${ATFQ_REPO:-../atfq}
vmanyware_repo=${VMANYWARE_REPO:-../VMAnyware}
chain_target=${DEVUTILS_TARGET_DIR:-/tmp/devutils-chain-build}
# Existing Omniscient release + reference runner are snapshotted by bundle.py.
[ -f target/dp-reference/reference ] || { echo 'run scripts/distroproof/reference-build.sh first' >&2; exit 1; }
(cd "$xpac_repo" && CARGO_TARGET_DIR=/tmp/xpac-target sh scripts/distroproof/build.sh)
for chain_libc in gnu musl; do
    chain_arch=x86_64-unknown-linux-$chain_libc
    chain_build_target=$chain_arch
    [ "$chain_libc" != gnu ] || chain_build_target=$chain_arch.2.17
    (cd "$atfq_repo" && CARGO_TARGET_DIR="$chain_target" cargo +stable zigbuild --release --locked --offline --target "$chain_build_target")
    (cd "$vmanyware_repo" && CARGO_TARGET_DIR="$chain_target" cargo +stable zigbuild --release --locked --offline --target "$chain_build_target" -p vmanyware-server)
done
python3 "$chain_root/scripts/distroproof/devutils/bundle.py" \
    --xpac "$xpac_repo" --atfq "$atfq_repo" --vmanyware "$vmanyware_repo" --build-dir "$chain_target"
