#!/usr/bin/env bash
# Runs the latency/throughput benchmark N times pinned to one CPU and summarises the runs.
#
#   scripts/bench.sh [-n RUNS] [-c CPU] [-l LIMIT] [FILE]
#
# RUNS defaults to 5, CPU to 5 (any otherwise quiet vCPU will do), FILE to
# $ITCH_DIR/12302019.NASDAQ_ITCH50 with ITCH_DIR defaulting to /data/itch. LIMIT stops each pass
# after that many messages (for quick checks; results of record use the whole day).
#
# stdout gets one JSON line per run as the benchmark prints it, then one summary JSON line with
# "summary":true holding, for every numeric field, the median, min, max and spread
# ((max - min) / median, in percent) across the runs. A readable table goes to stderr.
# Redirect stdout to a file to keep a baseline that a later run can be compared with.
#
# Linux only: results of record come from Linux under WSL2, never native Windows timings.
set -euo pipefail

runs=5
cpu=5
limit=""
while getopts "n:c:l:h" opt; do
    case "$opt" in
        n) runs="$OPTARG" ;;
        c) cpu="$OPTARG" ;;
        l) limit="$OPTARG" ;;
        h | *)
            sed -n '2,15p' "$0"
            exit 2
            ;;
    esac
done
shift $((OPTIND - 1))
file="${1:-${ITCH_DIR:-/data/itch}/12302019.NASDAQ_ITCH50}"

if [ "$(uname -s)" != "Linux" ]; then
    echo "bench.sh: refusing to run on $(uname -s); benchmarks of record run on Linux (WSL2)." >&2
    exit 2
fi
if [ ! -f "$file" ]; then
    echo "bench.sh: no ITCH file at $file (see scripts/fetch-itch.sh)" >&2
    exit 2
fi
command -v taskset >/dev/null || { echo "bench.sh: taskset not found (util-linux)" >&2; exit 2; }

repo="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo"

# A worktree checked out from Windows has a .git file with a Windows path that Linux git cannot
# follow; WSL can still run the Windows git.
commit="unknown"
for g in git git.exe; do
    if command -v "$g" >/dev/null && sha="$("$g" rev-parse HEAD 2>/dev/null)"; then
        commit="$sha"
        if [ -n "$("$g" status --porcelain --untracked-files=no 2>/dev/null)" ]; then
            commit="$commit-dirty"
        fi
        break
    fi
done
if [ "$commit" = "unknown" ]; then
    echo "bench.sh: WARNING: could not read the git commit; results are labelled \"unknown\"" >&2
fi

cargo build --release --example bench >&2
bin="${CARGO_TARGET_DIR:-$repo/target}/release/examples/bench"

echo "bench.sh: commit $commit, cpu $cpu, $runs runs, file $file${limit:+, limit $limit}" >&2
echo "bench.sh: kernel $(uname -r), load $(cut -d' ' -f1-3 /proc/loadavg)" >&2

results="$(mktemp)"
trap 'rm -f "$results"' EXIT
for i in $(seq 1 "$runs"); do
    echo "bench.sh: run $i/$runs" >&2
    taskset -c "$cpu" "$bin" --commit "$commit" ${limit:+--limit "$limit"} "$file" | tee -a "$results"
done

# Summary over the numeric fields. The benchmark's lines are flat JSON objects whose string
# values contain no commas or colons, so splitting on them is safe.
awk -v runs="$runs" -v commit="$commit" '
function sort(a, n,    i, j, t) {
    for (i = 2; i <= n; i++) {
        t = a[i]
        for (j = i - 1; j >= 1 && a[j] > t; j--) a[j + 1] = a[j]
        a[j + 1] = t
    }
}
{
    line = $0
    gsub(/[{}]/, "", line)
    n = split(line, fields, ",")
    for (f = 1; f <= n; f++) {
        split(fields[f], kv, ":")
        key = kv[1]; gsub(/"/, "", key)
        val = kv[2]
        if (val ~ /^-?[0-9.]+$/) {
            if (!(key in count)) order[++keys] = key
            count[key]++
            values[key, count[key]] = val + 0
        }
    }
}
END {
    out = "{\"summary\":true,\"commit\":\"" commit "\",\"runs\":" runs
    printf "%-26s %14s %14s %14s %9s\n", "field", "median", "min", "max", "spread%" > "/dev/stderr"
    for (k = 1; k <= keys; k++) {
        key = order[k]; c = count[key]
        for (i = 1; i <= c; i++) v[i] = values[key, i]
        sort(v, c)
        med = (c % 2) ? v[(c + 1) / 2] : (v[c / 2] + v[c / 2 + 1]) / 2
        spread = (med != 0) ? (v[c] - v[1]) / med * 100 : 0
        out = out sprintf(",\"%s\":{\"median\":%.4f,\"min\":%.4f,\"max\":%.4f,\"spread_pct\":%.2f}", key, med, v[1], v[c], spread)
        printf "%-26s %14.2f %14.2f %14.2f %9.2f\n", key, med, v[1], v[c], spread > "/dev/stderr"
    }
    print out "}"
}' "$results"
