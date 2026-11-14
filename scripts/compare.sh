#!/usr/bin/env bash
# Compares two preserved benchmark binaries in alternating pairs on the same input and CPU.
#
#   scripts/compare.sh [-n PAIRS] [-c CPU] [-l LIMIT] [-t] [-f FLOOR] BASELINE CANDIDATE [FILE]
#
# BASELINE and CANDIDATE are bench binaries kept by scripts/bench-build.sh. Pair 1 runs the
# baseline then the candidate, pair 2 the candidate then the baseline, and so on. PAIRS defaults
# to 5, CPU to 5, FILE to $ITCH_DIR/12302019.NASDAQ_ITCH50 (ITCH_DIR defaults to /data/itch).
# LIMIT stops each run after that many messages. Each run is the latency pass only; -t adds the
# throughput pass and compares throughput too.
#
# For every pair and statistic, d = (candidate - baseline) / baseline * 100. Verdict per statistic:
# "improved" if there are at least 5 pairs, every d is better (lower latency, higher throughput)
# and the median d is better by more than the noise floor; "regressed" the same way in the worse
# direction; otherwise "inconclusive". The noise floor of a statistic is the largest |d| an A/A
# comparison showed for it: pass the same binary as BASELINE and CANDIDATE, keep stdout, and give
# that file to later comparisons with -f FLOOR (same FILE, LIMIT, CPU and -t). Without -f the
# floor is 0 and the verdicts rest on sign agreement alone.
#
# stdout: one JSON line per run (the benchmark's line with "pair", "position" and "role" added),
# one line per pair with "pair_diff":true and every d, and one line with "summary":true. A
# readable table goes to stderr. Control-loop times are shown next to each run so a loaded run
# is visible; they do not adjust any result.
#
# Linux only: results of record come from Linux under WSL2, never native Windows timings.
set -euo pipefail

pairs=5
cpu=5
limit=""
tput=0
floor=""
while getopts "n:c:l:tf:h" opt; do
    case "$opt" in
        n) pairs="$OPTARG" ;;
        c) cpu="$OPTARG" ;;
        l) limit="$OPTARG" ;;
        t) tput=1 ;;
        f) floor="$OPTARG" ;;
        h | *)
            sed -n '2,25p' "$0"
            exit 2
            ;;
    esac
done
shift $((OPTIND - 1))
if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
    sed -n '2,25p' "$0"
    exit 2
fi
base="$1"
cand="$2"
file="${3:-${ITCH_DIR:-/data/itch}/12302019.NASDAQ_ITCH50}"

die() { echo "compare.sh: $*" >&2; exit 2; }
[ "$(uname -s)" = "Linux" ] ||
    die "refusing to run on $(uname -s); benchmarks of record run on Linux (WSL2)."
case "$pairs" in '' | *[!0-9]*) die "bad -n $pairs" ;; esac
[ "$pairs" -ge 1 ] || die "-n must be at least 1"
for b in "$base" "$cand"; do [ -x "$b" ] || die "not an executable: $b"; done
[ -f "$file" ] || die "no ITCH file at $file (see scripts/fetch-itch.sh)"
command -v taskset >/dev/null || die "taskset not found (util-linux)"

stats="all_mean_ns all_p50_ns all_p99_ns all_p999_ns book_mean_ns book_p50_ns book_p99_ns book_p999_ns"
[ "$tput" = 1 ] && stats="$stats throughput_msgs_per_s"
fname="$(basename "$file")"
limit_label="${limit:-none}"

# A floor only applies to comparisons made the same way as the A/A run that produced it.
floors=""
if [ -n "$floor" ]; then
    [ -f "$floor" ] || die "no floor file at $floor"
    fline="$(grep '"summary":true' "$floor" | tail -n 1)"
    [ -n "$fline" ] || die "$floor has no summary line from compare.sh"
    case "$fline" in *'"aa":true'*) ;; *) die "$floor is not an A/A comparison" ;; esac
    for want in "\"file\":\"$fname\"" "\"limit\":\"$limit_label\"" "\"cpu\":\"$cpu\""; do
        case "$fline" in *"$want"*) ;; *) die "$floor was not measured with $want" ;; esac
    done
    for s in $stats; do
        v="$(printf '%s\n' "$fline" | grep -o "\"$s\":{[^}]*}" | grep -o '"max_abs_pct":[0-9.]*' | cut -d: -f2 || true)"
        [ -n "$v" ] || die "$floor has no floor for $s (an A/A run with -t covers throughput)"
        floors="$floors$s=$v,"
    done
fi

sha_b="$(sha256sum "$base" | cut -d' ' -f1)"
sha_c="$(sha256sum "$cand" | cut -d' ' -f1)"
aa=false
[ "$sha_b" = "$sha_c" ] && aa=true
label() {
    local d
    d="$(dirname "$1")"
    if [ -f "$d/commit.txt" ]; then head -n 1 "$d/commit.txt"; else echo unknown; fi
}
label_b="$(label "$base")"
label_c="$(label "$cand")"

echo "compare.sh: baseline $base ($label_b)" >&2
echo "compare.sh: candidate $cand ($label_c)$([ "$aa" = true ] && echo ', identical binary: A/A run')" >&2
echo "compare.sh: cpu $cpu, $pairs pairs, file $file${limit:+, limit $limit}$([ "$tput" = 1 ] && echo ', with throughput pass')" >&2
echo "compare.sh: kernel $(uname -r), load $(cut -d' ' -f1-3 /proc/loadavg)" >&2
[ -n "$floor" ] || echo "compare.sh: no noise floor given (-f): verdicts rest on sign agreement alone" >&2

runs="$(mktemp)"
trap 'rm -f "$runs"' EXIT
tflag="--no-throughput"
[ "$tput" = 1 ] && tflag=""
for p in $(seq 1 "$pairs"); do
    if [ $((p % 2)) -eq 1 ]; then order="baseline candidate"; else order="candidate baseline"; fi
    pos=1
    for role in $order; do
        if [ "$role" = baseline ]; then bin="$base" lab="$label_b"; else bin="$cand" lab="$label_c"; fi
        echo "compare.sh: pair $p/$pairs, run $pos: $role" >&2
        line="$(taskset -c "$cpu" "$bin" --commit "$lab" ${limit:+--limit "$limit"} $tflag "$file")"
        printf '{"pair":%d,"position":%d,"role":"%s",%s\n' "$p" "$pos" "$role" "${line#\{}" | tee -a "$runs"
        pos=$((pos + 1))
    done
done

# The benchmark's lines are flat JSON objects whose string values contain no commas or colons,
# so splitting on them is safe.
awk -v stats="$stats" -v floors="$floors" -v pairs="$pairs" -v aa="$aa" -v cpu="$cpu" \
    -v fname="$fname" -v limit="$limit_label" -v tput="$tput" \
    -v label_b="$label_b" -v label_c="$label_c" -v sha_b="$sha_b" -v sha_c="$sha_c" '
function sort(a, n,    i, j, t) {
    for (i = 2; i <= n; i++) {
        t = a[i]
        for (j = i - 1; j >= 1 && a[j] > t; j--) a[j + 1] = a[j]
        a[j + 1] = t
    }
}
function abs(x) { return x < 0 ? -x : x }
BEGIN {
    ns = split(stats, stat, " ")
    nf = split(floors, fl, ",")
    for (i = 1; i <= nf; i++) if (fl[i] != "") { split(fl[i], kv, "="); floor[kv[1]] = kv[2] + 0; have_floor = 1 }
    err = "/dev/stderr"
    printf "\n%-4s %-3s %-9s %10s %10s %9s %10s %10s %10s %10s\n", "pair", "pos", "role", \
        "ctl_ms_0", "ctl_ms_1", "wall_s", "all_p50", "book_p50", "book_p99", "book_mean" > err
}
{
    line = $0
    gsub(/[{}"]/, "", line)
    n = split(line, fields, ",")
    delete r
    for (f = 1; f <= n; f++) { split(fields[f], kv, ":"); r[kv[1]] = kv[2] }
    p = r["pair"] + 0; role = r["role"]
    for (i = 1; i <= ns; i++) v[p, role, stat[i]] = r[stat[i]] + 0
    wall[++runs] = r["latency_wall_s"] + 0
    wall_desc[runs] = sprintf("pair %d %s", p, role)
    printf "%-4d %-3d %-9s %10.1f %10.1f %9.1f %10.1f %10.1f %10.1f %10.2f\n", p, r["position"], role, \
        r["control_ms_start"], r["control_ms_end"], r["latency_wall_s"], r["all_p50_ns"], \
        r["book_p50_ns"], r["book_p99_ns"], r["book_mean_ns"] > err
}
END {
    printf "\nd = (candidate - baseline) / baseline * 100, per pair (order AB: baseline ran first)\n%-4s %-5s", "pair", "order" > err
    for (i = 1; i <= ns; i++) printf " %12s", stat[i] > err
    printf "\n" > err
    for (p = 1; p <= pairs; p++) {
        ord = (p % 2) ? "AB" : "BA"
        out = sprintf("{\"pair_diff\":true,\"pair\":%d,\"order\":\"%s\"", p, ord)
        printf "%-4d %-5s", p, ord > err
        for (i = 1; i <= ns; i++) {
            s = stat[i]
            b = v[p, "baseline", s]; c = v[p, "candidate", s]
            d[s, p] = (b != 0) ? (c - b) / b * 100 : 0
            out = out sprintf(",\"%s_pct\":%.3f", s, d[s, p])
            printf " %12.2f", d[s, p] > err
        }
        print out "}"
        printf "\n" > err
    }

    out = sprintf("{\"summary\":true,\"aa\":%s,\"pairs\":%d,\"cpu\":\"%s\",\"file\":\"%s\",\"limit\":\"%s\",\"throughput_pass\":%s,\"baseline\":\"%s\",\"candidate\":\"%s\",\"baseline_sha256\":\"%s\",\"candidate_sha256\":\"%s\",\"floor_source\":\"%s\"", \
        aa, pairs, cpu, fname, limit, tput ? "true" : "false", label_b, label_c, sha_b, sha_c, have_floor ? "A/A file" : "none")
    printf "\n%-22s %9s %9s %9s %9s %7s %7s  %s\n", "statistic", "median%", "min%", "max%", "floor%", "better", "worse", "verdict" > err
    for (i = 1; i <= ns; i++) {
        s = stat[i]
        lower_is_better = (s !~ /^throughput/)
        better = 0; worse = 0; maxabs = 0
        for (p = 1; p <= pairs; p++) {
            x = d[s, p]; a[p] = x
            if (abs(x) > maxabs) maxabs = abs(x)
            if ((lower_is_better && x < 0) || (!lower_is_better && x > 0)) better++
            if ((lower_is_better && x > 0) || (!lower_is_better && x < 0)) worse++
        }
        sort(a, pairs)
        med = (pairs % 2) ? a[(pairs + 1) / 2] : (a[pairs / 2] + a[pairs / 2 + 1]) / 2
        f = (s in floor) ? floor[s] : 0
        verdict = "inconclusive"
        if (pairs >= 5 && abs(med) > f) {
            if (better == pairs) verdict = "improved"
            if (worse == pairs) verdict = "regressed"
        }
        out = out sprintf(",\"%s\":{\"median_pct\":%.3f,\"min_pct\":%.3f,\"max_pct\":%.3f,\"max_abs_pct\":%.3f,\"floor_pct\":%.3f,\"better\":%d,\"worse\":%d,\"verdict\":\"%s\"}", \
            s, med, a[1], a[pairs], maxabs, f, better, worse, verdict)
        printf "%-22s %9.2f %9.2f %9.2f %9.2f %7d %7d  %s\n", s, med, a[1], a[pairs], f, better, worse, verdict > err
    }
    print out "}"

    printf "\nRule: improved (or regressed) needs at least 5 pairs, every pair better (or worse), and the\n" > err
    printf "median d beyond the noise floor (largest |d| of an A/A run, given with -f; 0 without it).\n" > err
    printf "Anything else is inconclusive.\n" > err
    if (aa == "true")
        printf "This was an A/A run: keep stdout and pass it with -f; max_abs_pct is the floor.\n" > err

    for (i = 1; i <= runs; i++) w[i] = wall[i]
    sort(w, runs)
    mw = (runs % 2) ? w[(runs + 1) / 2] : (w[runs / 2] + w[runs / 2 + 1]) / 2
    for (i = 1; i <= runs; i++)
        if (wall[i] > 1.5 * mw)
            printf "WARNING: %s took %.1f s against a median of %.1f s; the host may have slept or been busy. Rerun the comparison.\n", \
                wall_desc[i], wall[i], mw > err
}' "$runs"
