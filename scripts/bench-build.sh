#!/usr/bin/env bash
# Builds the release benchmark and the microbenchmarks for the current checkout and keeps a copy
# of both binaries, so that scripts/compare.sh can later run them against another build.
#
#   scripts/bench-build.sh [LABEL]
#
# The copies go to $BENCH_BINS/LABEL/bench and LABEL/micro (built with --features internals), with
# BENCH_BINS defaulting to $HOME/bench-bins and LABEL to the short commit (with "-dirty" when
# tracked files have uncommitted changes). Beside them,
# commit.txt holds the full commit label (see scripts/commit-label.sh), build.txt the toolchain,
# build time and SHA-256, and settings.txt every build setting apart from the source: RUSTFLAGS,
# CARGO_ENCODED_RUSTFLAGS, the other Cargo environment variables that change flags or the profile,
# the path and SHA-256 of each Cargo config file, the target triple, the release profile from
# Cargo.toml and rustc -vV. scripts/compare.sh refuses to compare two binaries whose settings.txt
# differ. An existing LABEL is never overwritten; remove its directory first. The last line of
# stdout is the bench binary's path.
#
# Linux only, like scripts/bench.sh: the binaries are for benchmarks of record on Linux.
set -euo pipefail

if [ "$#" -gt 1 ] || [ "${1:-}" = "-h" ]; then
    sed -n '2,18p' "$0"
    exit 2
fi
if [ "$(uname -s)" != "Linux" ]; then
    echo "bench-build.sh: refusing to run on $(uname -s); benchmarks of record run on Linux." >&2
    exit 2
fi

repo="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo"
commit="$(scripts/commit-label.sh)"
if [ "$commit" = "unknown" ] && [ -z "${1:-}" ]; then
    echo "bench-build.sh: could not read the git commit; pass a LABEL" >&2
    exit 2
fi
short="${commit:0:12}"
case "$commit" in *-dirty) short="$short-dirty" ;; esac
label="${1:-$short}"
case "$label" in
    "" | */* | .*) echo "bench-build.sh: bad label \"$label\"" >&2; exit 2 ;;
esac
dir="${BENCH_BINS:-$HOME/bench-bins}/$label"
if [ -e "$dir" ]; then
    echo "bench-build.sh: $dir already exists; remove it to rebuild that label" >&2
    exit 2
fi

cargo build --release --example bench >&2
cargo build --release --features internals --example micro >&2
src="${CARGO_TARGET_DIR:-$repo/target}/release/examples"
mkdir -p "$dir"
cp "$src/bench" "$src/micro" "$dir/"
echo "$commit" > "$dir/commit.txt"
{
    echo "built $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    rustc --version
    echo "sha256 $(sha256sum "$dir/bench" | cut -d' ' -f1)"
    echo "micro sha256 $(sha256sum "$dir/micro" | cut -d' ' -f1)"
} > "$dir/build.txt"
{
    echo "RUSTFLAGS=${RUSTFLAGS-(unset)}"
    echo "CARGO_ENCODED_RUSTFLAGS=${CARGO_ENCODED_RUSTFLAGS-(unset)}"
    env | grep -E '^CARGO_(BUILD_RUSTFLAGS|PROFILE_[A-Z0-9_]+|TARGET_[A-Z0-9_]+_RUSTFLAGS)=' | sort || true
    # The Cargo config files Cargo reads for a build here: every .cargo/config(.toml) from the
    # repository up to /, then CARGO_HOME's. Path and SHA-256 only, never the contents, which can
    # hold registry tokens; a path inside the repository is relative, so worktrees compare equal.
    dirs=() d="$repo"
    while [ "$d" != / ]; do dirs+=("$d/.cargo"); d="$(dirname "$d")"; done
    for c in "${dirs[@]}" /.cargo "${CARGO_HOME:-$HOME/.cargo}"; do
        for f in "$c/config" "$c/config.toml"; do
            if [ -f "$f" ]; then echo "cargo config file ${f#"$repo"/} sha256 $(sha256sum "$f" | cut -d' ' -f1)"; fi
        done
    done
    echo "target ${CARGO_BUILD_TARGET:-$(rustc -vV | sed -n 's/^host: //p')}"
    awk '/^\[/ { on = /^\[profile\.release[].]/ } on' Cargo.toml
    rustc -vV
} > "$dir/settings.txt"
echo "bench-build.sh: commit $commit" >&2
echo "$dir/bench"
