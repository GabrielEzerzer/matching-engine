#!/usr/bin/env bash
# Compares two preserved benchmark binaries in alternating pairs on the same input and CPU.
#
#   scripts/compare.sh [-s STAT] [-n PAIRS] [-c CPU] [-l LIMIT] [-t] [-f FLOOR] BASELINE CANDIDATE [FILE]
#   scripts/compare.sh -r EARLIER [-s STAT] [-f FLOOR]
#
# BASELINE and CANDIDATE are bench binaries kept by scripts/bench-build.sh; their recorded build
# settings must match (two copies of one binary need none). Pair 1 runs the baseline then the
# candidate, pair 2 the candidate then the baseline, and so on. PAIRS must be even and at least 6
# (3 in each order) and defaults to 6; CPU defaults to 5, FILE to $ITCH_DIR/12302019.NASDAQ_ITCH50
# (ITCH_DIR defaults to /data/itch). LIMIT stops each run after that many messages. Each run is the
# latency pass only; -t adds the throughput pass and compares throughput too.
#
# STAT, declared before the run, is the one statistic that gets a verdict, for example all_p50_ns.
# Every other statistic is printed as information. Without -s there is no verdict.
#
# For every pair, d = 100 * ln(candidate / baseline), so that a multiplicative advantage of
# running first or second cancels exactly. With m1, s1^2, n1 the mean, sample variance and count
# of d over baseline-first pairs and m2, s2^2, n2 over candidate-first pairs: effect = (m1 + m2) / 2,
# se = sqrt(a + b) / 2 with a = s1^2/n1 and b = s2^2/n2, and bound = t * se, where t is the
# one-sided 99% Student-t value for the Welch-Satterthwaite degrees of freedom
# (a + b)^2 / (a^2/(n1 - 1) + b^2/(n2 - 1)), rounded down and at least 1 (t is 6.965 for 2, 4.541
# for 3, 3.747 for 4, 3.143 for 6, 2.896 for 8, 2.764 for 10). Taking the variance within each
# order keeps the order advantage out of se. The effect and the interval ends are printed as
# percent changes. Verdict for STAT: "improved" when effect +/- bound lies wholly on the better
# side of zero (lower latency, higher throughput) and, with -f, beyond the floor; "regressed" in
# the mirror case; otherwise "inconclusive". The floor is an A/A comparison's (the same binary
# passed twice) absolute effect plus its bound for that statistic, recomputed from the FLOOR file's
# runs; FLOOR must have the same FILE, LIMIT, CPU and -t setting. An A/A comparison is a check of
# the method: a sound method gives "inconclusive" for it.
#
# -r EARLIER summarises the runs saved in an earlier compare.sh output again instead of running. A
# file with unequal or too few pairs per order gets "inconclusive (too few pairs)", with what the
# interval alone gives printed beside it.
#
# stdout: one JSON line per run (the benchmark's line with "pair", "position" and "role" added),
# one line per pair with "pair_diff":true and every d, and one line with "summary":true holding the
# declared statistic, the verdict and every statistic's numbers. Readable tables go to stderr, with
# control-loop times next to each run so a loaded run is visible; they never adjust a result.
# The summary is computed by scripts/compare_summary.py (Python 3).
#
# Linux only: results of record come from Linux under WSL2, never native Windows timings.
set -euo pipefail

pairs=6
cpu=5
limit=""
tput=0
floor=""
rerun=""
target=""
usage() { sed -n '2,42p' "$0"; exit 2; }
while getopts "s:n:c:l:tf:r:h" opt; do
    case "$opt" in
        s) target="$OPTARG" ;;
        n) pairs="$OPTARG" ;;
        c) cpu="$OPTARG" ;;
        l) limit="$OPTARG" ;;
        t) tput=1 ;;
        f) floor="$OPTARG" ;;
        r) rerun="$OPTARG" ;;
        h | *) usage ;;
    esac
done
shift $((OPTIND - 1))
die() { echo "compare.sh: $*" >&2; exit 2; }
[ "$(uname -s)" = "Linux" ] ||
    die "refusing to run on $(uname -s); benchmarks of record run on Linux (WSL2)."
command -v python3 >/dev/null || die "python3 not found"
summarise=(python3 "$(dirname "$0")/compare_summary.py" ${target:+-s "$target"} ${floor:+-f "$floor"})

if [ -n "$rerun" ]; then
    [ "$#" -eq 0 ] || die "-r takes no binaries"
    [ -f "$rerun" ] || die "no file at $rerun"
    echo "compare.sh: summarising the runs saved in $rerun" >&2
    exec "${summarise[@]}" "$rerun"
fi

{ [ "$#" -ge 2 ] && [ "$#" -le 3 ]; } || usage
base="$1"
cand="$2"
file="${3:-${ITCH_DIR:-/data/itch}/12302019.NASDAQ_ITCH50}"
case "$pairs" in '' | *[!0-9]*) die "bad -n $pairs" ;; esac
{ [ $((pairs % 2)) -eq 0 ] && [ "$pairs" -ge 6 ]; } ||
    die "-n must be even and at least 6, so that each order has at least 3 pairs"
for b in "$base" "$cand"; do [ -x "$b" ] || die "not an executable: $b"; done
[ -f "$file" ] || die "no ITCH file at $file (see scripts/fetch-itch.sh)"
command -v taskset >/dev/null || die "taskset not found (util-linux)"

sha_b="$(sha256sum "$base" | cut -d' ' -f1)"
sha_c="$(sha256sum "$cand" | cut -d' ' -f1)"
aa=false
[ "$sha_b" = "$sha_c" ] && aa=true

# Two different binaries are only comparable when they were built the same way.
if [ "$aa" = false ]; then
    set_b="$(dirname "$base")/settings.txt"
    set_c="$(dirname "$cand")/settings.txt"
    for s in "$set_b" "$set_c"; do
        [ -f "$s" ] || die "no build settings recorded at $s; rebuild that binary with scripts/bench-build.sh"
    done
    if ! delta="$(diff "$set_b" "$set_c")"; then
        case "$delta" in *"cargo config file"*) delta="$delta"$'\n'"A Cargo config file differs (paths above)." ;; esac
        die "the two binaries were built with different settings (< baseline, > candidate):"$'\n'"$delta"
    fi
fi

label() {
    local d
    d="$(dirname "$1")"
    if [ -f "$d/commit.txt" ]; then head -n 1 "$d/commit.txt"; else echo unknown; fi
}
label_b="$(label "$base")"
label_c="$(label "$cand")"

runs="$(mktemp)"
trap 'rm -f "$runs"' EXIT
printf '{"meta":true,"cpu":"%s","file":"%s","limit":"%s","throughput_pass":%s,"baseline":"%s","candidate":"%s","baseline_sha256":"%s","candidate_sha256":"%s"}\n' \
    "$cpu" "$(basename "$file")" "${limit:-none}" "$([ "$tput" = 1 ] && echo true || echo false)" \
    "$label_b" "$label_c" "$sha_b" "$sha_c" > "$runs"
# Refuse a bad -s or -f now rather than after the runs.
"${summarise[@]}" --check "$runs"

echo "compare.sh: baseline $base ($label_b)" >&2
echo "compare.sh: candidate $cand ($label_c)$([ "$aa" = true ] && echo ', identical binary: A/A run')" >&2
echo "compare.sh: cpu $cpu, $pairs pairs, file $file${limit:+, limit $limit}$([ "$tput" = 1 ] && echo ', with throughput pass')" >&2
echo "compare.sh: declared statistic ${target:-none (no verdict)}${floor:+, floor from $floor}" >&2
echo "compare.sh: kernel $(uname -r), load $(cut -d' ' -f1-3 /proc/loadavg)" >&2

tflag="--no-throughput"
[ "$tput" = 1 ] && tflag=""
for p in $(seq 1 "$pairs"); do
    if [ $((p % 2)) -eq 1 ]; then order="baseline candidate"; else order="candidate baseline"; fi
    pos=1
    for role in $order; do
        if [ "$role" = baseline ]; then bin="$base" lab="$label_b"; else bin="$cand" lab="$label_c"; fi
        echo "compare.sh: pair $p/$pairs, run $pos: $role" >&2
        line="$(taskset -c "$cpu" "$bin" --commit "$lab" ${limit:+--limit "$limit"} $tflag "$file")" ||
            die "pair $p run $pos ($role) failed"
        case "$line" in "{"*) ;; *) die "pair $p run $pos ($role) printed no result line" ;; esac
        printf '{"pair":%d,"position":%d,"role":"%s",%s\n' "$p" "$pos" "$role" "${line#\{}" | tee -a "$runs"
        pos=$((pos + 1))
    done
done

"${summarise[@]}" "$runs"
