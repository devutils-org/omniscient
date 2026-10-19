#!/bin/sh
# Build the frozen companion payload first: sh scripts/distroproof/devutils-build.sh
# Every installation, unavailable optional package and explicit DNF4 fallback is
# retained in witnesses.json; an actual installation failure fails setup.
exec sh scripts/distroproof/devutils/setup.sh
