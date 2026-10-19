#!/bin/sh
# The portability matrix's container cells: omn installed from the release
# archive inside rootless podman containers on this machine, and swept as in
# a distroproof guest (`omn selftest`), with envprobe's report of the same
# container beside it. A container is not a kernel: it is the same kernel
# seen through a user namespace, a seccomp filter, a mount namespace that
# hides tracefs and debugfs, and capabilities that are the namespace's own.
# Each refusal omn gives there must name that, and agree with envprobe.
#
#   scripts/release.sh && scripts/distroproof/fixtures.sh
#   scripts/containers.sh
#   cargo test --test portability          # the judges, over every cell
#
# Cells land in target/matrix/cells/<cell>/ (sweep.json, envprobe.json), the
# configuration named `container`. The configurations:
#
#   <image>-root        root in the container (the namespace's root, not the
#                       machine's): what `podman run` gives by default
#   <image>-user        an unprivileged uid in the container
#   <image>-capadd      root with CAP_BPF, CAP_PERFMON and CAP_SYS_ADMIN
#                       added, which a user namespace holds for itself alone
#   <image>-privileged  `--privileged`: every capability of the namespace, no
#                       seccomp filter, /sys writable
#
# Rootless on purpose: it is what a developer's podman is. A rootful
# container (sudo podman) holds the machine's capabilities and is a root cell.
set -eu
cd "$(dirname "$0")/.."
cells=${OMN_MATRIX_CELLS:-target/matrix/cells}
envprobe=${ENVPROBE:-../distroproof/target/x86_64-unknown-linux-musl/release/envprobe}
[ -x "$envprobe" ] || { echo "containers: no static envprobe at $envprobe (ENVPROBE=...)" >&2; exit 1; }
command -v podman >/dev/null || { echo "containers: podman is not installed" >&2; exit 1; }
arch=$(uname -m)

run_cell() { # NAME IMAGE LIBC PODMAN_ARGS...
    name=$1 image=$2 libc=$3
    shift 3
    work=$(mktemp -d)
    chmod 777 "$work"
    archive=$(ls target/dist/omn-*-"$arch"-linux-"$libc".tar.gz | head -n 1)
    # Installed as root (the archive, as a user would), then swept as the
    # container's user: `--user` applies to the whole run, so the install is
    # a separate first step into a volume both share.
    podman run --rm -q -v "$PWD/target/dist:/dist:ro" -v "$work:/out" "$image" \
        sh -c "mkdir -p /out/omn && tar -C /out/omn --strip-components=1 -xzf /dist/$(basename "$archive") && chmod -R a+rX /out/omn"
    podman run --rm -q "$@" \
        -v "$work:/out" -v "$PWD/target/dp-fixtures/$arch:/fixtures:ro" \
        -v "$(realpath "$envprobe"):/envprobe:ro" "$image" \
        sh -c "/out/omn/bin/omn selftest --format json --cell $name --fixtures /fixtures > /out/sweep.json; /envprobe --json > /out/envprobe.json"
    mkdir -p "$cells/$name"
    python3 - "$work/sweep.json" "$cells/$name/sweep.json" <<'EOF'
import json, sys
sweep = json.load(open(sys.argv[1]))
sweep["cell"]["config"] = "container"
json.dump(sweep, open(sys.argv[2], "w"), indent=2)
usable = sum(1 for s in sweep["sources"] if s["here"] == "yes")
print(f"{sweep['cell']['name']}: {sweep['cell']['kernel_release']}, container, "
      f"{len(sweep['sources'])} sources, {usable} listed usable")
EOF
    cp "$work/envprobe.json" "$cells/$name/envprobe.json"
    rm -rf "$work"
}

for spec in "debian docker.io/library/debian:13 gnu" "alpine docker.io/library/alpine:3 musl"; do
    set -- $spec
    short=$1 image=$2 libc=$3
    run_cell "podman-$short-root" "$image" "$libc"
    run_cell "podman-$short-user" "$image" "$libc" --user 1000:1000
    run_cell "podman-$short-capadd" "$image" "$libc" --cap-add CAP_BPF,CAP_PERFMON,CAP_SYS_ADMIN
    run_cell "podman-$short-privileged" "$image" "$libc" --privileged
done
