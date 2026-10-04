#!/bin/sh
# build.sh NAME ARGS...: build NAME.c once into the target directory, then
# replace this shell with it, so the profiled process is the program alone.
# A rebuild happens only when the source is newer than the binary, and is
# then a few tens of milliseconds of cc inside the first run's profile.
set -e
here=$(dirname "$0")
name=$1
shift
out=${CARGO_TARGET_TMPDIR:-$here/../../../../target}/reference
bin=$out/perfrecord-$name
mkdir -p "$out"
if [ ! -x "$bin" ] || [ "$here/$name.c" -nt "$bin" ]; then
    cc -O1 -fno-omit-frame-pointer -fno-inline -pthread -no-pie -o "$bin.tmp.$$" "$here/$name.c"
    mv "$bin.tmp.$$" "$bin"
fi
exec "$bin" "$@"
