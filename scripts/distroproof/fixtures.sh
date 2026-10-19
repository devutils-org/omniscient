#!/bin/sh
# The program-probe sources' fixtures, for distroproof's guests: the USDT
# fixture `runtime.*` is run on (from the source tests/scenario_runtime.rs
# embeds, as scripts/matrix.sh builds it) and the vertical case's server
# `net.request` is run on (examples/vertical/server.c). Built here, statically
# against zig's musl, so that one binary starts on every image whatever its
# libc; their probes are notes in the binary, which a static link keeps.
# <sys/sdt.h> is systemtap's, self-contained, and taken from this machine.
set -eu
cd "$(dirname "$0")/../.."
src=target/dp-fixtures/src
mkdir -p "$src/include/sys"
cp /usr/include/sys/sdt.h /usr/include/sys/sdt-config.h "$src/include/sys/"
awk '/^const FIXTURE_SOURCE: &str = r#"$/ {on=1; next} on && /^"#;$/ {exit} on' \
    tests/scenario_runtime.rs > "$src/fixture.c"
# One tree per architecture a guest may be (target/dp-fixtures/<uname -m>/),
# zig cross-compiling: sweep.sh passes the guest's own.
for arch in x86_64 aarch64; do
    out=target/dp-fixtures/$arch
    mkdir -p "$out/omn-usdt-fixture" "$out/omn-vertical"
    build() {
        zig cc -target "$arch-linux-musl" -static -O0 -pthread -I"$src/include" -o "$1" "$2"
    }
    build "$out/omn-usdt-fixture/fixture" "$src/fixture.c"
    build "$out/omn-vertical/server" examples/vertical/server.c
    echo "fixtures: $out/omn-usdt-fixture/fixture and $out/omn-vertical/server, static"
done
