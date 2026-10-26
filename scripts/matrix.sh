#!/usr/bin/env bash
# The portability matrix: docs/ROADMAP.md's "Portability" and "Degradation" rows.
# docs/portability.md says what each cell checks and what the cells found.
#
#   scripts/matrix.sh                    every kernel under every configuration
#   scripts/matrix.sh here               this machine alone, no guest
#   scripts/matrix.sh check              judge the cells already on disk
#   scripts/matrix.sh [KERNEL...] [--configs caps,root,...]
#
# KERNEL is an Ubuntu mainline version (v6.1.188) or `host`, the kernel this
# machine runs. The defaults are the floor's side of docs/DECISIONS.md -- 5.15,
# which is under it, and 6.1, which is it -- then 6.6, the LTS where
# uprobe-multi arrived, then the host's own. The configurations are in
# scripts/vm/cell.sh.
#
# Each guest boots the host's filesystem read-only with virtme-ng and runs the
# host-built binary, so what differs between cells is the kernel and the
# configuration, never the build. The kernels are prebuilt, fetched once into
# target/matrix/kernels/ (about 500 MB each), and nothing is compiled but the
# tool. A cell writes target/matrix/cells/<kernel>-<config>/sweep.json, and
# `cargo test --test portability` judges every cell on disk, here and in any
# later run of the suite.
#
# Needs: /dev/kvm, qemu, virtme-ng (`pipx install virtme-ng`), and a network
# the first time. Run it on a quiet machine if the timings matter to you; the
# judges do not read them.

set -euo pipefail
cd "$(dirname "$0")/.."

KERNELS_DEFAULT=(v5.15.221 v6.1.188 v6.6.157 host)
CONFIGS_DEFAULT=caps,root,nocaps,nobtf,notracefs
# A static busybox for virtme-ng's initramfs, which a host without busybox
# lacks. Pinned by digest: it runs as the guest's init.
BUSYBOX_URL=https://busybox.net/downloads/binaries/1.35.0-x86_64-linux-musl/busybox
BUSYBOX_SHA256=6e123e7f3202a8c1e9b1f94d8941580a25135382b99e8d3e34fb858bba311348

kernels=()
configs=$CONFIGS_DEFAULT
mode=matrix
while [ $# -gt 0 ]; do
    case $1 in
        here) mode=here ;;
        host) kernels+=(host) ;;
        check) mode=check ;;
        --configs) configs=${2:?--configs needs a list}; shift ;;
        --configs=*) configs=${1#--configs=} ;;
        v[0-9]*) kernels+=("$1") ;;
        -h|--help) sed -n '2,27p' "$0"; exit 0 ;;
        *) echo "matrix: unknown argument \`$1\`" >&2; exit 2 ;;
    esac
    shift
done
[ ${#kernels[@]} -eq 0 ] && kernels=("${KERNELS_DEFAULT[@]}")

judge() {
    echo "matrix: -- judging the cells on disk"
    cargo test --test portability -- --nocapture
}

if [ "$mode" = check ]; then
    judge
    exit
fi

make
# The USDT fixture the runtime sources are given, built from the one source
# `tests/scenario_runtime.rs` embeds, with the same flags, so the two cannot
# drift. Never fatal: without it those sources refuse by naming the probes they
# looked for, and the judge says so.
fixture=target/omn-usdt-fixture/fixture
mkdir -p "$(dirname "$fixture")"
awk '/^const FIXTURE_SOURCE: &str = r#"$/ {on=1; next} on && /^"#;$/ {exit} on' \
    tests/scenario_runtime.rs > "$fixture.c"
if ! cc -O0 -pthread -o "$fixture" "$fixture.c" 2>/dev/null; then
    echo "matrix: note: the USDT fixture did not build (no cc, or no <sys/sdt.h>)"
fi

sweep=$(cargo test --test portability --no-run --message-format=json 2>/dev/null \
    | jq -r 'select(.executable != null and .target.name == "portability") | .executable' | tail -n1)
[ -x "$sweep" ] || { echo "matrix: cannot find the portability test binary" >&2; exit 1; }

if [ "$mode" = here ]; then
    release=$(uname -r)
    if getcap target/debug/omn | grep -q cap_bpf; then config=caps; else config=nocaps; fi
    [ "$(id -u)" = 0 ] && config=root
    echo "matrix: -- this machine, $release, as $config, no guest"
    OMN_MATRIX_CELL="here-$config" OMN_MATRIX_CONFIG="$config" \
        "$sweep" --ignored --exact sweep_this_machine --nocapture
    judge
    exit
fi

command -v vng >/dev/null || { echo "matrix: virtme-ng is not installed: pipx install virtme-ng" >&2; exit 1; }
[ -r /dev/kvm ] && [ -w /dev/kvm ] || { echo "matrix: /dev/kvm is not usable by $(id -un)" >&2; exit 1; }
busybox=$(command -v busybox || true)
if [ -z "$busybox" ]; then
    busybox=target/matrix/busybox
    if [ ! -x "$busybox" ]; then
        mkdir -p target/matrix
        curl -fsSL -o "$busybox.partial" "$BUSYBOX_URL"
        echo "$BUSYBOX_SHA256  $busybox.partial" | sha256sum -c --quiet \
            || { echo "matrix: busybox digest mismatch" >&2; rm -f "$busybox.partial"; exit 1; }
        chmod +x "$busybox.partial"
        mv "$busybox.partial" "$busybox"
    fi
fi

cells=$PWD/target/matrix/cells
mkdir -p "$cells"
failed=()
for kernel in "${kernels[@]}"; do
    if [ "$kernel" = host ]; then
        image=/boot/vmlinuz-linux
        [ -r "$image" ] || image=$(ls /boot/vmlinuz-"$(uname -r)" 2>/dev/null || true)
        [ -r "$image" ] || { echo "matrix: cannot read the host's kernel image" >&2; failed+=(host); continue; }
        label=$(uname -r | cut -d- -f1)
    else
        image=$(scripts/vm/fetch-kernel.sh "$kernel") || { failed+=("$kernel"); continue; }
        label=${kernel#v}
    fi
    for config in ${configs//,/ }; do
        cell="$label-$config"
        echo "matrix: -- $cell"
        rm -rf "${cells:?}/$cell"
        mkdir -p "$cells/$cell"
        # --disable-microvm: a distribution kernel finds its root device on
        # virtio-pci, and microvm offers only virtio-mmio, which Ubuntu builds
        # as a module the initramfs does not load.
        if ! timeout 900 vng --disable-microvm --busybox "$busybox" --user root \
            --cpus 4 --memory 2G --rwdir "$cells" -r "$image" \
            --exec "OMN_MATRIX_CELLS=$cells scripts/vm/cell.sh $cell $config $(id -u) $sweep" \
            > "$cells/$cell/guest.log" 2>&1; then
            echo "matrix: $cell did not complete; the guest's log:" >&2
            tail -n 20 "$cells/$cell/guest.log" >&2
            failed+=("$cell")
        fi
        grep '^matrix:' "$cells/$cell/guest.log" || true
    done
done

judge
if [ ${#failed[@]} -gt 0 ]; then
    echo "matrix: cells that did not complete: ${failed[*]}" >&2
    exit 1
fi
