#!/usr/bin/env bash
# Downloads, verifies and decompresses the public Nasdaq TotalView-ITCH 5.0 sample day used by
# the replay, the golden hash and the benchmark.
#
#   scripts/fetch-itch.sh [DIR]
#
# DIR defaults to $ITCH_DIR, then /data/itch. Work already done is skipped: a complete .gz is
# not downloaded again (a partial one is resumed), and an existing decompressed file is kept.
# The .gz is checked against the pinned size and SHA-256 before it is decompressed. The golden
# check (`replay --check`) verifies the decompressed file's own SHA-256 as part of its run.
#
# Set VERIFY=1 to also hash an already decompressed file against the pinned value.
# ITCH_URL, ITCH_GZ_SHA256, ITCH_GZ_SIZE, ITCH_SHA256 and ITCH_SIZE override the pinned values
# (for another day, or to test this script on a small file).
#
# The download is a plain curl request with curl's default user agent and no other headers.
set -euo pipefail

dir="${1:-${ITCH_DIR:-/data/itch}}"
url="${ITCH_URL:-https://emi.nasdaq.com/ITCH/Nasdaq%20ITCH/12302019.NASDAQ_ITCH50.gz}"
gz_sha256="${ITCH_GZ_SHA256:-ef03df46a27e6bda4dead017f84c2e3979df7211f02c7868b51d53fceb99c689}"
gz_size="${ITCH_GZ_SIZE:-3524013057}"
sha256="${ITCH_SHA256:-5d81c2e14a0f748b29c674b6a342796932702034b4dd341e39e9a9ec5bac610f}"
size="${ITCH_SIZE:-8251407909}"

name="$(basename "${url//%20/ }")"
gz="$dir/$name"
out="$dir/${name%.gz}"

say() { echo "fetch-itch: $*" >&2; }
size_of() { stat -c %s "$1" 2>/dev/null || echo 0; }

mkdir -p "$dir"

if [ -f "$out" ]; then
    if [ "$(size_of "$out")" != "$size" ]; then
        say "$out has $(size_of "$out") bytes, expected $size; delete it and run again"
        exit 1
    fi
    if [ "${VERIFY:-0}" = 1 ]; then
        say "hashing $out"
        echo "$sha256  $out" | sha256sum --check --quiet || { say "$out does not match the pinned SHA-256"; exit 1; }
        say "$out verified"
    fi
    say "$out already present ($size bytes), nothing to do"
    exit 0
fi

if [ -f "$out.tmp" ] && [ -n "$(find "$out.tmp" -mmin -1 2>/dev/null)" ]; then
    say "$out.tmp changed in the last minute: another decompression seems to be running"
    exit 1
fi

have="$(size_of "$gz")"
if [ "$have" -gt "$gz_size" ]; then
    say "$gz has $have bytes, more than the expected $gz_size; delete it and run again"
    exit 1
fi
if [ "$have" -lt "$gz_size" ]; then
    if [ "$have" -gt 0 ]; then say "resuming download at byte $have"; else say "downloading $url"; fi
    curl --fail --location --continue-at - --output "$gz" "$url"
fi
if [ "$(size_of "$gz")" != "$gz_size" ]; then
    say "$gz has $(size_of "$gz") bytes after download, expected $gz_size"
    exit 1
fi

say "verifying $gz"
if ! echo "$gz_sha256  $gz" | sha256sum --check --quiet; then
    say "$gz does not match the pinned SHA-256; delete it and run again"
    exit 1
fi

say "decompressing to $out"
gzip --decompress --stdout "$gz" > "$out.tmp"
if [ "$(size_of "$out.tmp")" != "$size" ]; then
    say "decompressed $(size_of "$out.tmp") bytes, expected $size"
    exit 1
fi
mv "$out.tmp" "$out"
say "done: $out"
