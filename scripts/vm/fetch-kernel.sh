#!/usr/bin/env bash
# Fetches one prebuilt kernel from Ubuntu's mainline builds into
# target/matrix/kernels/<version>/, and prints the path of its image.
#
#   scripts/vm/fetch-kernel.sh v6.1.188
#
# Prebuilt on purpose: building a kernel takes an hour and several gigabytes,
# and a kernel built here would be configured by whoever built it. A
# distribution's generic build is the configuration a user actually runs, and
# Ubuntu's carries BTF, which is what the matrix is asking about.
#
# Only the image and the modules package are taken -- no headers -- and the
# .deb files are deleted once extracted: the machine this was written on had
# nine gigabytes free. Extraction uses bsdtar rather than dpkg, which an Arch
# host does not have.

set -euo pipefail
cd "$(dirname "$0")/../.."

version=${1:?usage: fetch-kernel.sh vX.Y.Z}
base=https://kernel.ubuntu.com/mainline/$version/amd64
dir=target/matrix/kernels/$version

image() { ls "$dir"/boot/vmlinuz-*-generic 2>/dev/null | head -n1; }

if [ -n "$(image)" ]; then
    image
    exit 0
fi

mkdir -p "$dir"
index=$(curl -fsSL "$base/") || {
    echo "fetch-kernel: $base/ is not reachable" >&2
    exit 1
}
debs=$(printf '%s\n' "$index" | grep -oE 'href="linux-(image-unsigned|modules)-[^"]+_amd64\.deb"' \
    | sed 's/^href="//; s/"$//' | sort -u)
[ -n "$debs" ] || { echo "fetch-kernel: no image or modules package under $base/" >&2; exit 1; }

for deb in $debs; do
    echo "fetch-kernel: $deb" >&2
    curl -fsSL -o "$dir/$deb" "$base/$deb"
    # A .deb is an ar archive around data.tar.*; bsdtar reads the ar and the
    # inner tar in two passes.
    (cd "$dir" && bsdtar -xOf "$deb" 'data.tar*' | bsdtar -xf -)
    rm -f "$dir/$deb"
done

# The image is root-only in the package (0600); qemu runs as this user.
chmod a+r "$dir"/boot/vmlinuz-* 2>/dev/null || true
[ -n "$(image)" ] || { echo "fetch-kernel: no image was extracted into $dir" >&2; exit 1; }
image
