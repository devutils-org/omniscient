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
out=target/dp-fixtures
mkdir -p "$out/omn-usdt-fixture" "$out/omn-vertical" "$out/include/sys"
cp /usr/include/sys/sdt.h /usr/include/sys/sdt-config.h "$out/include/sys/"
awk '/^const FIXTURE_SOURCE: &str = r#"$/ {on=1; next} on && /^"#;$/ {exit} on' \
    tests/scenario_runtime.rs > "$out/omn-usdt-fixture/fixture.c"
build() {
    zig cc -target x86_64-linux-musl -static -O0 -pthread -I"$out/include" -o "$1" "$2"
}
build "$out/omn-usdt-fixture/fixture" "$out/omn-usdt-fixture/fixture.c"
build "$out/omn-vertical/server" examples/vertical/server.c
echo "fixtures: $out/omn-usdt-fixture/fixture and $out/omn-vertical/server, static"
