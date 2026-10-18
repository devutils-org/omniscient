#!/bin/sh
# distroproof's `setup`, as root in each guest (distroproof.toml): installs
# the release archive the way a user does. The guest gets the archives
# alone (target/dist), never the build tree.
set -eu

# musl or glibc: the archive whose loader this system has.
if [ -e /lib/ld-musl-x86_64.so.1 ]; then libc=musl; else libc=gnu; fi
archive=$(ls target/dist/omn-*-x86_64-linux-$libc.tar.gz 2>/dev/null | head -n 1)
[ -n "$archive" ] || { echo "setup: no $libc archive under target/dist; run scripts/release.sh" >&2; exit 1; }
mkdir -p /opt/omn
tar -C /opt/omn --strip-components=1 -xzf "$archive"
echo "setup: installed $(basename "$archive") in /opt/omn"

# The capabilities are distroproof's to grant (`file_caps` in
# distroproof.toml), on the installed file, as docs/install.md says to. A
# kernel before 5.8 has neither CAP_PERFMON nor CAP_BPF, and distroproof's
# agent then warns that setcap was refused; cells.py reads that.
