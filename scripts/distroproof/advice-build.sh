#!/bin/sh
# The advice rules' guest fixture, static, so a guest needs no compiler:
# target/dp-advice/<arch>/fixture, carried by distroproof-advice.toml.
set -eu
arch=$(uname -m)
mkdir -p "target/dp-advice/$arch"
cc -O1 -pthread -static -o "target/dp-advice/$arch/fixture" tests/fixtures/advice/fixture.c
echo "advice-build: target/dp-advice/$arch/fixture"
