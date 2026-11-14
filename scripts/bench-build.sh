#!/usr/bin/env bash
# Builds the release benchmark for the current checkout and keeps a copy of the binary, so that
# scripts/compare.sh can later run it against another build.
#
#   scripts/bench-build.sh [LABEL]
#
# The copy goes to $BENCH_BINS/LABEL/bench, with BENCH_BINS defaulting to /root/bench-bins and
# LABEL to the short commit (with "-dirty" when tracked files have uncommitted changes). Beside it,
# commit.txt holds the full commit label (see scripts/commit-label.sh) and build.txt the toolchain
# and build time. An existing LABEL is never overwritten; remove its directory first. The last
# line of stdout is the binary's path.
#
# Linux only, like scripts/bench.sh: the binaries are for benchmarks of record under WSL2.
set -euo pipefail

if [ "$#" -gt 1 ] || [ "${1:-}" = "-h" ]; then
    sed -n '2,13p' "$0"
    exit 2
fi
if [ "$(uname -s)" != "Linux" ]; then
    echo "bench-build.sh: refusing to run on $(uname -s); benchmarks of record run on Linux (WSL2)." >&2
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
dir="${BENCH_BINS:-/root/bench-bins}/$label"
if [ -e "$dir" ]; then
    echo "bench-build.sh: $dir already exists; remove it to rebuild that label" >&2
    exit 2
fi

cargo build --release --example bench >&2
src="${CARGO_TARGET_DIR:-$repo/target}/release/examples/bench"
mkdir -p "$dir"
cp "$src" "$dir/bench"
echo "$commit" > "$dir/commit.txt"
{
    echo "built $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    rustc --version
    echo "sha256 $(sha256sum "$dir/bench" | cut -d' ' -f1)"
} > "$dir/build.txt"
echo "bench-build.sh: commit $commit" >&2
echo "$dir/bench"
