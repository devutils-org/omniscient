#!/bin/sh
# Runs the bench's GPU program (examples/bench/gpu.c), linked with -lcuda.
#
# gpu.api interposes on calls bound through the dynamic linker, so a ctypes
# workload is invisible to it: the program has to link libcuda the ordinary
# way. It is compiled once into target/reference/gpu and reused; the
# arguments are gpu.c's own (`memory N MIB`, `launches N`, `compute N ITERS`).
set -eu
root=$(cd "$(dirname "$0")/../../../.." && pwd)
binary=$root/target/reference/gpu
if [ ! -x "$binary" ] || [ "$root/examples/bench/gpu.c" -nt "$binary" ]; then
    mkdir -p "$root/target/reference"
    cc -O2 -o "$binary.$$" "$root/examples/bench/gpu.c" -lcuda
    mv "$binary.$$" "$binary"
fi
export BENCH_GPU_WARM_MS=${BENCH_GPU_WARM_MS:-0}
exec "$binary" "$@"
