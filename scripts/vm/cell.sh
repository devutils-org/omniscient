#!/usr/bin/env bash
# One cell of the portability matrix, run inside a guest as root.
#
#   scripts/vm/cell.sh <cell> <config> <uid> <sweep binary>
#
# `scripts/matrix.sh` boots the guest and calls this; it is not meant to be run
# on the host, where it would setcap and unmount things for real -- which is
# why it refuses to run outside a virtme-ng guest.
#
# The configuration is made here, inside the guest, rather than found: a
# kernel without BTF does not have to be built when the file can be hidden,
# and the tool cannot tell the difference -- it reads the same path.
#
#   caps       the deployment `make` produces: uid <uid>, the binary carrying
#              cap_bpf,cap_perfmon. virtiofs drops the host's file
#              capabilities, so they are granted again here, on the guest's
#              copy-on-write view of the binary; the host's file is untouched.
#   root       uid 0.
#   nocaps     uid <uid>, no capability on the binary.
#   nobtf      uid 0, /sys/kernel/btf hidden under an empty tmpfs.
#   notracefs  uid 0, tracefs and debugfs unmounted.
#
# The two removals run as root so that each cell differs from `root` by
# exactly the one thing removed: as uid <uid>, tracefs is already unreadable,
# and hiding it would change nothing the tool can see.

set -euo pipefail
cd "$(dirname "$0")/../.."

cell=${1:?cell} config=${2:?config} uid=${3:?uid} sweep=${4:?sweep binary}

if ! grep -q virtme /proc/cmdline; then
    echo "cell.sh: this is not a virtme-ng guest; it changes the machine it runs on" >&2
    exit 2
fi

omn=target/debug/omn
run_as=0
export OMN_MATRIX_CELL=$cell OMN_MATRIX_CONFIG=$config
case $config in
    caps)
        setcap cap_bpf,cap_perfmon+ep "$omn"
        run_as=$uid
        ;;
    root) ;;
    nocaps)
        setcap -r "$omn" 2>/dev/null || true
        run_as=$uid
        ;;
    nobtf)
        mount -t tmpfs -o mode=0755 none /sys/kernel/btf
        export OMN_MATRIX_BTF_HIDDEN=1
        ;;
    notracefs)
        # debugfs automounts tracefs under itself on the first access, so it
        # goes too; innermost first.
        for mount in /sys/kernel/debug/tracing /sys/kernel/tracing /sys/kernel/debug; do
            umount "$mount" 2>/dev/null || true
        done
        export OMN_MATRIX_TRACEFS_HIDDEN=1
        ;;
    *)
        echo "cell.sh: unknown configuration \`$config\`" >&2
        exit 2
        ;;
esac

# The configuration is checked before anything is measured: a cell that
# believes it hid BTF and did not would pass every judge for the wrong reason.
case $config in
    nobtf)
        [ ! -e /sys/kernel/btf/vmlinux ] || { echo "cell.sh: BTF is still visible" >&2; exit 1; }
        ;;
    notracefs)
        if grep -qE ' (tracefs|debugfs) ' /proc/mounts; then
            echo "cell.sh: tracefs or debugfs is still mounted:" >&2
            grep -E ' (tracefs|debugfs) ' /proc/mounts >&2
            exit 1
        fi
        ;;
    caps)
        getcap "$omn" | grep -q cap_bpf || { echo "cell.sh: the capabilities did not take" >&2; exit 1; }
        ;;
    nocaps)
        ! getcap "$omn" | grep -q cap_ || { echo "cell.sh: the binary still carries capabilities" >&2; exit 1; }
        ;;
esac

echo "cell.sh: $cell: kernel $(uname -r), uid $run_as, perf_event_paranoid $(cat /proc/sys/kernel/perf_event_paranoid)"
if [ "$run_as" = 0 ]; then
    exec "$sweep" --ignored --exact sweep_this_machine --nocapture
fi
gid=$(getent passwd "$run_as" | cut -d: -f4)
exec setpriv --reuid="$run_as" --regid="${gid:-$run_as}" --init-groups \
    env HOME=/tmp "$sweep" --ignored --exact sweep_this_machine --nocapture
