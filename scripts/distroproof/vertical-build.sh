#!/bin/sh
# The vertical server, static, for distroproof-vertical.toml: a guest needs no
# compiler and no <sys/sdt.h>. Build the release first (scripts/release.sh).
set -eu
arch=$(uname -m)
mkdir -p "target/dp-vertical/$arch"
cc -O0 -pthread -static -o "target/dp-vertical/$arch/server" examples/vertical/server.c
echo "vertical-build: target/dp-vertical/$arch/server"
