#!/bin/sh
# distroproof's `setup` for distroproof-reference.toml, as root: the release
# installed as for the matrix (setup.sh), then the reference pairs' witnesses
# from this distribution's own packages, and a sudo for the user the pairs run
# as -- the development machine's /etc/sudoers.d/omniscient-witnesses, made
# wide since the guest is thrown away. A package a distribution does not have
# is left out; its pairs skip, saying which tool they lack.
set -u
sh scripts/distroproof/setup.sh || exit 1

install_each() {
    for package in "$@"; do
        $installer "$package" >/dev/null 2>&1 && echo "witnesses: $package" \
            || echo "witnesses: no $package here"
    done
}
if command -v apt-get >/dev/null 2>&1; then
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq >/dev/null 2>&1
    installer="apt-get install -qq -y --no-install-recommends"
    if grep -qi ubuntu /etc/os-release; then
        install_each "linux-tools-$(uname -r)" linux-tools-generic
    else
        install_each linux-perf
    fi
    install_each bpftrace bpfcc-tools strace sysstat tcpdump python3 gcc libc6-dev sudo iproute2
elif command -v dnf >/dev/null 2>&1; then
    installer="dnf install -q -y"
    install_each perf bpftrace bcc-tools strace sysstat tcpdump python3 gcc sudo iproute
elif command -v pacman >/dev/null 2>&1; then
    pacman -Sy --noconfirm >/dev/null 2>&1
    installer="pacman -S --noconfirm --needed"
    install_each perf bpftrace bcc-tools strace sysstat tcpdump python gcc sudo iproute2
fi
# bcc's tools are runqlat-bpfcc on Debian and Ubuntu, under
# /usr/share/bcc/tools on Fedora: the pairs ask for the bare names.
for dir in /usr/share/bcc/tools /usr/sbin; do
    for tool in runqlat tcplife tcpaccept memleak opensnoop vfsstat biolatency execsnoop; do
        for candidate in "$dir/$tool" "$dir/$tool-bpfcc"; do
            [ -x "$candidate" ] && [ ! -e "/usr/local/bin/$tool" ] && ln -s "$candidate" "/usr/local/bin/$tool"
        done
    done
done
mkdir -p /etc/sudoers.d
echo "${DP_USER:-test} ALL=(ALL) NOPASSWD: ALL" > /etc/sudoers.d/omn-witnesses
chmod 0440 /etc/sudoers.d/omn-witnesses
