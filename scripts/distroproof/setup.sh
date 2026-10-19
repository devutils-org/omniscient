#!/bin/sh
# distroproof's `setup`, as root in each guest (distroproof.toml): installs
# the release archive the way a user does. The guest gets the archives
# alone (target/dist), never the build tree.
set -eu

# This machine's architecture, and musl or glibc: the archive whose loader
# this system has.
# A system with no working glibc loader where the glibc archive's binaries
# look for it gets the static musl archive, which needs none: NixOS keeps its
# loader in the store and puts a stub there that refuses every program
# ("Could not start dynamically linked executable"), so the loader is asked
# to run, not only looked for.
arch=$(uname -m)
case $arch in
    aarch64) glibc_loader=/lib/ld-linux-aarch64.so.1 ;;
    *) glibc_loader=/lib64/ld-linux-x86-64.so.2 ;;
esac
if [ -e "/lib/ld-musl-$arch.so.1" ] || ! "$glibc_loader" --version >/dev/null 2>&1; then
    libc=musl
else
    libc=gnu
fi
archive=$(ls target/dist/omn-*-"$arch"-linux-$libc.tar.gz 2>/dev/null | head -n 1)
[ -n "$archive" ] || { echo "setup: no $arch $libc archive under target/dist; run ARCH=$arch scripts/release.sh" >&2; exit 1; }
mkdir -p /opt/omn
tar -C /opt/omn --strip-components=1 -xzf "$archive"
echo "setup: installed $(basename "$archive") in /opt/omn"

# The capabilities are distroproof's to grant (`file_caps` in
# distroproof.toml), on the installed file, as docs/install.md says to. A
# kernel before 5.8 has neither CAP_PERFMON nor CAP_BPF, and distroproof's
# agent then warns that setcap was refused; cells.py reads that.
