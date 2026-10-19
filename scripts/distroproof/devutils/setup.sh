#!/bin/sh
# Guest bootstrap only. Native commands refresh metadata / bootstrap Python;
# installed reference packages are managed by xpac except the explicit DNF4 case.
set -eu
[ "$PWD" = /distroproof/work ] && [ -n "${DP_TARGET:-}" ] && [ -n "${DP_IMAGE:-}" ] || {
    echo 'requires a disposable distroproof guest' >&2; exit 1;
}
[ -f target/devutils-input/builds.json ] || { echo "run sh scripts/distroproof/devutils-build.sh before starting the matrix" >&2; exit 1; }
export HOME=/root DEBIAN_FRONTEND=noninteractive
if ! command -v python3 >/dev/null 2>&1; then
    if [ -x /usr/libexec/platform-python ]; then
        mkdir -p /usr/local/bin; ln -sf /usr/libexec/platform-python /usr/local/bin/python3
    elif command -v apk >/dev/null 2>&1; then apk add --no-cache python3
    else echo 'Python is required to bootstrap the integration' >&2; exit 1
    fi
fi
python3 - <<'PY'
import hashlib,json
from pathlib import Path
root=Path('target/devutils-input')
m=json.loads((root/'builds.json').read_text())
for name, expected in m['files'].items():
    if hashlib.sha256((root/name).read_bytes()).hexdigest() != expected:
        raise ValueError('payload checksum mismatch: '+name)
PY
mkdir -p /opt/xpac /opt/omn /opt/devutils /usr/local/bin
tar -xzf target/devutils-input/xpac.tar.gz -C /opt/xpac --strip-components=1
if [ -e /lib/ld-musl-x86_64.so.1 ]; then chain_libc=musl; else chain_libc=gnu; fi
tar -xzf "target/devutils-input/omn-$chain_libc.tar.gz" -C /opt/omn --strip-components=1
cp "target/devutils-input/atfq-$chain_libc" /opt/devutils/atfq
cp "target/devutils-input/vmanyware-server-$chain_libc" /opt/devutils/vmanyware-server
chmod 755 /opt/devutils/*
ln -sf /opt/xpac/xpac /usr/local/bin/xpac
printf '%s\n' "$DP_TARGET" > /etc/devutils-distroproof-guest
exec python3 scripts/distroproof/devutils/provision.py
