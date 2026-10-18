#!/bin/sh
# distroproof's `setup`, as root in each guest (distroproof.toml): installs
# the release archive the way a user does and grants the capabilities on the
# installed file, as docs/install.md says to. The guest gets the archives
# alone (target/dist), never the build tree.
set -eu

# musl or glibc: the archive whose loader this system has.
if [ -e /lib/ld-musl-x86_64.so.1 ]; then libc=musl; else libc=gnu; fi
archive=$(ls target/dist/omn-*-x86_64-linux-$libc.tar.gz 2>/dev/null | head -n 1)
[ -n "$archive" ] || { echo "setup: no $libc archive under target/dist; run scripts/release.sh" >&2; exit 1; }
mkdir -p /opt/omn
tar -C /opt/omn --strip-components=1 -xzf "$archive"
echo "setup: installed $(basename "$archive") in /opt/omn"

# CAP_PERFMON (38) and CAP_BPF (39) arrived in Linux 5.8: a kernel that
# predates them has nothing of the deployment to grant -- its setcap does not
# know the names (Debian 10, Ubuntu 18.04 and 20.04) -- and omn runs without
# them, as it does for a user there. cells.py reads this line.
last=$(cat /proc/sys/kernel/cap_last_cap 2>/dev/null || echo 0)
if [ "$last" -lt 39 ]; then
    echo "setup: nothing to grant: this kernel predates CAP_PERFMON and CAP_BPF (cap_last_cap $last)"
    exit 0
fi

# setcap, from whatever this distribution packages it in.
if ! command -v setcap >/dev/null 2>&1; then
    if command -v apk >/dev/null 2>&1; then apk add -q libcap-utils
    elif command -v apt-get >/dev/null 2>&1; then apt-get install -qq -y libcap2-bin >/dev/null
    elif command -v dnf >/dev/null 2>&1; then dnf install -q -y libcap >/dev/null
    elif command -v pacman >/dev/null 2>&1; then pacman -Sq --noconfirm libcap >/dev/null
    fi
fi
# Granted on the installed file, never ambient: what `make` and
# docs/install.md do, and what distroproof's `caps =` would not be (every
# program omn launches would inherit them).
setcap cap_bpf,cap_perfmon+ep /opt/omn/bin/omn
echo "setup: $(getcap /opt/omn/bin/omn)"
