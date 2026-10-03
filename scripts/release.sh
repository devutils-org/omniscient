#!/usr/bin/env bash
# A release someone else can install: the binaries, the preload shim and the
# manual page in one archive, with its checksum. docs/install.md is the user's
# side of this script.
#
#   scripts/release.sh                 build target/dist/<name>.tar.gz and its .sha256
#   scripts/release.sh list            the archive's contents, exactly, one path a line
#   scripts/release.sh name            the archive's name, without the extension
#   scripts/release.sh floor [FILE...] the oldest glibc each binary needs, failing
#                                      above the documented $GLIBC_FLOOR
#   scripts/release.sh built           whether target/release is newer than its
#                                      sources (what `sudo make install` checks)
#   scripts/release.sh verify ARCHIVE  the checksum, the contents against `list`,
#                                      the glibc floor, and `check` on the
#                                      extracted tree
#   scripts/release.sh check PREFIX    run PREFIX/bin/omn with the build tree moved
#                                      out of the way (what `make install-check` runs)
#
# The archive is a prefix: extracted with --strip-components=1 into ~/.local or
# /usr/local, it is what `make install PREFIX=...` would have put there. The
# shim sits beside the binary in bin/, because that is where `omn` looks for
# it (`find_shim` in src/collect/mem/heap/mod.rs) and where `omn-mcp` looks for
# `omn`.
#
# It grants no capability. A file capability is an extended attribute of one
# file on one filesystem: tar does not carry it by default, and a user who
# extracts an archive as themselves could not set it anyway. The user grants
# it on the installed path, and docs/install.md says with what -- on `omn`
# alone, since `omn-mcp` collects through the `omn` beside it.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST="$ROOT/target/dist"

# The oldest glibc README.md and docs/install.md promise, as "glibc 2.34 or
# later". `floor` fails when a binary needs a newer one, and `verify` fails
# when the documents shipped in the archive promise something else, so the
# promise and the check cannot drift apart silently.
GLIBC_FLOOR=2.34


version() {
    sed -n 's/^version = "\(.*\)"$/\1/p' "$ROOT/Cargo.toml" | head -n 1
}

name() {
    echo "omn-$(version)-$(uname -m)-linux-gnu"
}

# The contents, in the order they are written. Everything the archive holds is
# on this list and nothing else is: `verify` compares the two.
contents() {
    cat <<'EOF'
bin/omn
bin/omn-mcp
bin/libomn_preload.so
share/man/man1/omn.1
share/doc/omn/README.md
share/doc/omn/install.md
share/doc/omn/LICENSE-MIT
share/doc/omn/LICENSE-APACHE
share/doc/omn/LICENSES/GPL-2.0-only.txt
EOF
}

# Where each archived path comes from, relative to the checkout.
source_of() {
    case "$1" in
        bin/omn) echo target/release/omn ;;
        bin/omn-mcp) echo target/release/omn-mcp ;;
        bin/libomn_preload.so) echo target/release/libomn_preload.so ;;
        share/man/man1/omn.1) echo man/omn.1 ;;
        share/doc/omn/README.md) echo README.md ;;
        share/doc/omn/install.md) echo docs/install.md ;;
        share/doc/omn/LICENSE-MIT) echo LICENSE-MIT ;;
        share/doc/omn/LICENSE-APACHE) echo LICENSE-APACHE ;;
        share/doc/omn/LICENSES/GPL-2.0-only.txt) echo LICENSES/GPL-2.0-only.txt ;;
        *) echo "release.sh: no source for $1" >&2; return 1 ;;
    esac
}

mode_of() {
    case "$1" in
        bin/omn | bin/omn-mcp) echo 755 ;;
        *) echo 644 ;;
    esac
}

build() {
    cd "$ROOT"
    # `--workspace`, or the shim is not built: nothing depends on it.
    # `--locked`, so the archive is built from the dependencies the lock file
    # names rather than whatever resolves today.
    cargo build --workspace --release --locked

    local stage archive top
    top="$(name)"
    stage="$DIST/stage/$top"
    archive="$DIST/$top.tar.gz"
    rm -rf "$DIST/stage"
    mkdir -p "$stage" "$DIST"

    local path
    while read -r path; do
        install -D -m "$(mode_of "$path")" "$ROOT/$(source_of "$path")" "$stage/$path"
    done < <(contents)

    # Reproducible: sorted as listed, owned by nobody in particular, and dated
    # by the commit rather than by the build.
    local epoch
    epoch="${SOURCE_DATE_EPOCH:-$(git -C "$ROOT" log -1 --format=%ct 2>/dev/null || date +%s)}"
    contents | sed "s|^|$top/|" \
        | tar -C "$DIST/stage" --owner=0 --group=0 --numeric-owner --mtime="@$epoch" \
              --no-recursion -T - -cf - \
        | gzip -n -9 > "$archive"
    (cd "$DIST" && sha256sum "$top.tar.gz" > "$top.tar.gz.sha256")
    rm -rf "$DIST/stage"

    echo "$archive"
    echo "$archive.sha256"
    echo "  $(cut -d' ' -f1 "$archive.sha256")"
    glibc_floor
}

# The oldest glibc each binary runs on: the newest symbol version it needs,
# weak references left out (the standard library resolves `pidfd_spawnp` and
# its kind at run time and does without them). A build on a newer distribution
# can raise it, which is why the release workflow builds on the oldest runner
# it can -- and why this fails, rather than prints, when a binary needs more
# than $GLIBC_FLOOR: a dependency linking one newer symbol would otherwise
# break the documented promise with every step green. With no objdump the floor
# is unknown, and unknown is a failure, not a pass.
glibc_floor() {
    local binaries=("$@")
    (( ${#binaries[@]} )) || binaries=("$ROOT"/target/release/{omn,omn-mcp,libomn_preload.so})
    command -v objdump >/dev/null || {
        echo "glibc floor: unknown, there is no objdump to read it with (binutils)" >&2
        return 1
    }
    local binary floor above=0
    for binary in "${binaries[@]}"; do
        [[ -f "$binary" ]] || { echo "glibc floor: no file at $binary" >&2; above=1; continue; }
        floor="$(objdump -T "$binary" \
            | awk '$2 != "w" && $0 ~ /GLIBC_[0-9]/ { match($0, /GLIBC_[0-9.]+/); print substr($0, RSTART + 6, RLENGTH - 6) }' \
            | sort -uV | tail -n 1)"
        if [[ -n "$floor" && "$(printf '%s\n' "$floor" "$GLIBC_FLOOR" | sort -V | tail -n 1)" != "$GLIBC_FLOOR" ]]; then
            echo "glibc floor: $(basename "$binary") needs $floor, above the documented $GLIBC_FLOOR" >&2
            objdump -T "$binary" | awk -v f="GLIBC_$floor" '$2 != "w" && index($0, f) { print "  " $NF " (" f ")" }' >&2
            above=1
        else
            echo "glibc floor: $(basename "$binary") needs ${floor:-nothing versioned}, within $GLIBC_FLOOR"
        fi
    done
    return "$above"
}

# Whether target/release holds a build at least as new as its sources. It is
# what `make install` checks instead of building when it runs as root: cargo
# under sudo is usually not on root's PATH, and when it is, it leaves
# root-owned files in target/ that break the next `make`. So the build is the
# user's (`make release`), and the install only copies it.
#
# The sources of each artifact are the ones cargo itself recorded for it, in
# the dep-info file beside it (target/release/omn.d and its kind): the files
# rustc actually read, catalog included. A wider list -- all of src/, say --
# would name files that are not compiled (src/triage/workload.c), and touching
# one would refuse forever, since `make release` would not relink for it.
built() {
    local artifact depinfo newer
    for artifact in omn omn-mcp libomn_preload.so; do
        depinfo="$ROOT/target/release/${artifact%.so}.d"
        artifact="$ROOT/target/release/$artifact"
        if [[ ! -f "$artifact" || ! -f "$depinfo" ]]; then
            echo "no release build at $artifact; run \`make release\` as yourself first" >&2
            return 1
        fi
        newer="$(cut -d: -f2- "$depinfo" | tr ' ' '\n' | grep . \
            | while read -r source; do [[ ! "$source" -nt "$artifact" ]] || { echo "$source"; break; }; done)"
        if [[ -n "$newer" ]]; then
            echo "$(basename "$artifact") is older than ${newer#"$ROOT/"}; run \`make release\` as yourself first" >&2
            return 1
        fi
    done
    echo "the release build in target/release is newer than every source cargo recorded for it"
}

verify() {
    local archive="${1:?usage: release.sh verify ARCHIVE}"
    archive="$(cd "$(dirname "$archive")" && pwd)/$(basename "$archive")"
    local sum="$archive.sha256"
    [[ -f "$sum" ]] || { echo "verify: no checksum beside it: $sum" >&2; return 1; }

    (cd "$(dirname "$archive")" && sha256sum --check --strict --quiet "$(basename "$sum")")
    echo "checksum: $(basename "$archive") matches $(basename "$sum")"

    local top expected actual
    top="$(basename "$archive" .tar.gz)"
    expected="$(contents | sed "s|^|$top/|")"
    actual="$(tar -tzf "$archive")"
    if [[ "$expected" != "$actual" ]]; then
        echo "verify: the archive does not hold exactly what \`release.sh list\` names" >&2
        diff <(echo "$expected") <(echo "$actual") >&2 || true
        return 1
    fi
    echo "contents: exactly the $(contents | wc -l) paths \`release.sh list\` names"

    # The extracted tree is a prefix of its own, and is checked as one: the
    # floor its binaries need, the floor its documents promise, and then the
    # installed-tree checks.
    local scratch status=0
    scratch="$(mktemp -d "${TMPDIR:-/tmp}/omn-verify.XXXXXX")"
    tar -C "$scratch" -xzf "$archive"
    glibc_floor "$scratch/$top"/bin/{omn,omn-mcp,libomn_preload.so} || status=1
    local doc
    for doc in README.md install.md; do
        if grep -q "glibc $GLIBC_FLOOR or later" "$scratch/$top/share/doc/omn/$doc"; then
            echo "promise: share/doc/omn/$doc says glibc $GLIBC_FLOOR or later"
        else
            echo "verify: share/doc/omn/$doc does not say \"glibc $GLIBC_FLOOR or later\"" >&2
            status=1
        fi
    done
    check "$scratch/$top" || status=1
    rm -rf "$scratch"
    return "$status"
}

# The ground truth for an installed tree. Every check runs PREFIX/bin/omn from
# an empty directory, with OMN_PRELOAD_LIB unset and the build tree renamed, so
# nothing it finds can have come from the checkout's target/.
check() {
    local prefix="${1:?usage: release.sh check PREFIX}"
    prefix="$(cd "$prefix" && pwd)"
    local omn="$prefix/bin/omn"
    [[ -x "$omn" ]] || { echo "check: no executable at $omn" >&2; return 1; }
    case "$prefix/" in
        "$ROOT/target/"*) echo "check: $prefix is inside the build tree it is meant to go without" >&2; return 1 ;;
    esac
    command -v python3 >/dev/null || {
        echo "check: the workloads are python3 programs that allocate a known size, and there is no python3" >&2
        return 1
    }

    local moved=""
    if [[ -d "$ROOT/target" ]]; then
        moved="$ROOT/target.moved-by-release-check"
        [[ ! -e "$moved" ]] || { echo "check: $moved exists; an earlier check did not restore it" >&2; return 1; }
        mv "$ROOT/target" "$moved"
    fi
    local work
    work="$(mktemp -d "${TMPDIR:-/tmp}/omn-check.XXXXXX")"
    # shellcheck disable=SC2064
    trap "rm -rf '$work'; [[ -z '$moved' ]] || mv '$moved' '$ROOT/target'" EXIT

    local failures=0
    fail() { echo "FAIL  $*"; failures=$((failures + 1)); }
    pass() { echo "ok    $*"; }

    run() { (cd "$work" && env -u OMN_PRELOAD_LIB "$@"); }

    echo "checking $prefix, with the build tree at ${moved:-<none>}"

    # 1. The binary is the one built from this tree.
    local reported
    reported="$(run "$omn" --version)"
    [[ "$reported" == "omn $(version)" ]] \
        && pass "omn --version: $reported" \
        || fail "omn --version says '$reported', the checkout is $(version)"

    # 2. The catalog is compiled in: the installed binary lists every entry of
    #    catalog/*.yaml without a catalog anywhere near it.
    local entries listed
    entries="$(cat "$ROOT"/catalog/*.yaml | grep -c '^  family: ')"
    listed="$(run "$omn" list | sed -n 's/^\([0-9]*\) sources:.*/\1/p')"
    [[ "$listed" == "$entries" ]] \
        && pass "omn list: $listed sources, as many as catalog/*.yaml declares" \
        || fail "omn list reports '${listed:-nothing}' sources; catalog/*.yaml declares $entries"

    # 3. mem.rss on a launched program that writes 64 MiB and holds it, against
    #    one that holds nothing: the first peaks at 64 MiB or more, the second
    #    below it. The hold is how long the program runs, not a synchronisation.
    local big=$((64 << 20))
    local rss_big rss_none
    rss_big="$(peak mem.rss "$work/rss-big" -m poll@20ms -- \
        python3 -c "import time; b = b'x' * $big; time.sleep(1.0)")"
    rss_none="$(peak mem.rss "$work/rss-none" -m poll@20ms -- \
        python3 -c "import time; time.sleep(1.0)")"
    if ! numeric "$rss_big" || ! numeric "$rss_none"; then
        fail "mem.rss: $rss_big / $rss_none"
    elif (( ${rss_big%% *} >= big && ${rss_none%% *} < big )); then
        pass "mem.rss: peak $((${rss_big%% *} >> 20)) MiB$(losses "$rss_big") with 64 MiB written, $((${rss_none%% *} >> 20)) MiB$(losses "$rss_none") without"
    else
        fail "mem.rss: peak $rss_big with 64 MiB written, $rss_none without; expected >= $big and < $big"
    fi

    # 4. mem.heap.alloc through the installed shim. Two programs, each making
    #    one allocation of a size it chose; the largest the shim saw is that
    #    size plus at most the interpreter's object header.
    local size seen slack=$((64 << 10))
    for size in $((64 << 20)) $((32 << 20)); do
        seen="$(peak mem.heap.alloc "$work/heap-$size" -m trace -- \
            python3 -c "b = bytearray($size)")"
        if ! numeric "$seen"; then
            fail "mem.heap.alloc of $size B: $seen"
        elif (( ${seen%% *} >= size && ${seen%% *} < size + slack )); then
            pass "mem.heap.alloc: largest allocation ${seen%% *} B$(losses "$seen") for one of $size B, through the preload shim"
        else
            fail "mem.heap.alloc: largest allocation $seen B for one of $size B; expected [$size, $((size + slack)))"
        fi
    done

    # 5. The path it looked at is the installed one: without the shim beside
    #    the binary, the refusal names that path.
    local shim="$prefix/bin/libomn_preload.so" refusal
    if [[ ! -f "$shim" ]]; then
        fail "there is no shim at $shim"
    else
        mv "$shim" "$shim.away"
        refusal="$(run "$omn" mem.heap.alloc -m trace -e summary -- true 2>&1 || true)"
        mv "$shim.away" "$shim"
        [[ "$refusal" == *"$shim"* ]] \
            && pass "without its shim, the installed omn names $shim" \
            || fail "without its shim, the refusal does not name $shim: $(echo "$refusal" | head -n 3)"
    fi

    # 6. omn-mcp answers for the omn beside it.
    local mcp="$prefix/bin/omn-mcp" answer
    answer="$(printf '%s\n' \
        '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"release-check","version":"0"}}}' \
        '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
        '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"capabilities","arguments":{}}}' \
        | (cd "$work" && timeout 120 "$mcp" 2>/dev/null) | tail -n 1 || true)"
    [[ "$answer" == *"$omn"* ]] \
        && pass "omn-mcp's capability sweep was answered by $omn" \
        || fail "omn-mcp's capability sweep does not name $omn: $(echo "$answer" | head -c 300)"

    # 7. One privilege model: the server's sweep is the one `omn` gives, so
    #    whatever was granted to $omn -- and only to it -- serves both ways of
    #    asking. A difference here is two machines in one tree.
    local direct through
    direct="$(run "$omn" list --format json 2>/dev/null | python3 -c '
import json, sys
print(sum(1 for row in json.load(sys.stdin) if row["here"]["availability"] == "ready"))' || true)"
    through="$(echo "$answer" | python3 -c '
import json, sys
print(json.load(sys.stdin)["result"]["structuredContent"]["totals"]["available"])' 2>/dev/null || true)"
    if [[ -n "$direct" && "$direct" == "$through" ]]; then
        pass "omn-mcp serves the $through sources $omn serves: one privilege model, on $omn"
    else
        fail "omn serves ${direct:-?} sources and omn-mcp ${through:-?}: the two ways of asking run on different machines"
    fi

    if (( failures )); then
        echo "$failures check(s) failed on $prefix"
        return 1
    fi
    echo "every check passed on $prefix"
}

# Whether `peak` answered with a number, possibly followed by its losses.
numeric() { [[ "$1" =~ ^[0-9]+( .*)?$ ]]; }

# The losses `peak` put after its number, with a leading space, or nothing.
# Every line that prints a number prints them too, a pass as much as a
# failure: a lossy run in range is still a lossy run, and losses are surfaced
# always.
losses() { [[ "$1" != *" "* ]] || echo " ${1#* }"; }

# The largest value a run's summary saw, in bytes, then the run's losses when
# it had any, or a sentence saying why there is no value. The losses travel
# with the number so that every line printing it says so: on a failure,
# missing the size with events dropped is inconclusive rather than wrong, and
# on a pass the losses are still shown.
peak() {
    local source="$1" out="$2"
    shift 2
    local status=0
    (cd "$work" && env -u OMN_PRELOAD_LIB "$omn" "$source" -e summary --format json --out "$out" "$@") \
        > "$out.json" 2> "$out.err" || status=$?
    if (( status )); then
        echo "exited $status: $(head -n 3 "$out.err" | tr '\n' ' ')"
        return
    fi
    python3 - "$out.json" <<'EOF'
import json, sys
result = json.load(open(sys.argv[1]))["results"][0]
series = [s for o in result["outputs"] for s in o["series"]]
if not series:
    print("no series in the summary")
    sys.exit()
losses = {k: v for k, v in result["losses"].items() if v}
peak = int(max(s["max"] for s in series))
print(peak if not losses else f"{peak} (the run lost {losses})")
EOF
}

case "${1:-build}" in
    build) build ;;
    list) contents ;;
    name) name ;;
    floor) shift; glibc_floor "$@" ;;
    built) built ;;
    verify) shift; verify "$@" ;;
    check) shift; check "$@" ;;
    *) grep '^#   scripts/release.sh\|^#  ' "$0" | head -n 16 >&2; exit 2 ;;
esac
