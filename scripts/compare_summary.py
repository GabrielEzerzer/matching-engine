#!/usr/bin/env python3
# Summarises the runs of a scripts/compare.sh comparison and gives its verdict. compare.sh calls
# it, and compare.sh's header and the README describe the rule. It recomputes everything from the
# run lines, so -r and -f work on any saved comparison, and scripts/test_compare_summary.py checks
# the arithmetic without a benchmark.
#
#   compare_summary.py [--check] [-s STAT] [-f FLOOR] RUNS
#
# RUNS holds the run lines (benchmark JSON lines with "pair", "position" and "role" added) and one
# metadata line: compare.sh's {"meta":true,...} line during a run, or the {"summary":true,...}
# line of an earlier compare.sh output. --check only validates STAT and FLOOR against that
# metadata. stdout: one {"pair_diff":true,...} line per pair, then one {"summary":true,...} line.
# Tables go to stderr. A refusal exits with status 2.
#
# All arithmetic is on d = 100 * ln(candidate / baseline), so that a multiplicative advantage of
# running second cancels exactly between the two orders. Effects, interval ends, order effects and
# floors are converted back to percent changes for display; the JSON keeps both, the log-scale
# values under names ending in _ln_x100.
import argparse
import json
import math
import sys

LATENCY = [f"{scope}_{q}_ns" for scope in ("all", "book") for q in ("mean", "p50", "p99", "p999")]
THROUGHPUT = "throughput_msgs_per_s"
MIN_PER_ORDER = 3

# One-sided 99% critical values of Student's t by degrees of freedom. A df between two rows uses
# the row below it, which gives the larger, more cautious value.
T99 = {1: 31.821, 2: 6.965, 3: 4.541, 4: 3.747, 5: 3.365, 6: 3.143, 7: 2.998, 8: 2.896,
       9: 2.821, 10: 2.764, 11: 2.718, 12: 2.681, 13: 2.650, 14: 2.624, 15: 2.602, 16: 2.583,
       17: 2.567, 18: 2.552, 19: 2.539, 20: 2.528, 25: 2.485, 30: 2.457, 40: 2.423, 60: 2.390,
       120: 2.358}


def die(msg):
    print(f"compare.sh: {msg}", file=sys.stderr)
    sys.exit(2)


def t99(df):
    return T99[max(k for k in T99 if k <= df)]


def mean(xs):
    return sum(xs) / len(xs)


def var(xs):
    m = mean(xs)
    return sum((x - m) ** 2 for x in xs) / (len(xs) - 1)


def pct(x):
    """A difference of 100 * ln(ratio) as a percent change."""
    return None if x is None else 100 * math.expm1(x / 100)


def load(path):
    """The metadata line, the runs by (pair, role), and the pairs that have both runs."""
    meta, runs = None, {}
    with open(path) as fh:
        for line in fh:
            if not line.startswith("{"):
                continue
            rec = json.loads(line)
            if rec.get("meta") or rec.get("summary"):
                meta = rec
            elif "role" in rec:
                runs[rec["pair"], rec["role"]] = rec
    if meta is None:
        die(f"{path} has no summary line from compare.sh")
    pairs = sorted(p for p, role in runs if role == "baseline" and (p, "candidate") in runs)
    return meta, runs, pairs


def same_binary(meta):
    return bool(meta.get("baseline_sha256")) and meta.get("baseline_sha256") == meta.get("candidate_sha256")


def stats_of(meta):
    return LATENCY + ([THROUGHPUT] if meta.get("throughput_pass") else [])


def baseline_first(runs, p):
    return runs[p, "baseline"]["position"] == 1


def d(runs, p, stat):
    b, c = runs[p, "baseline"][stat], runs[p, "candidate"][stat]
    for role, v in (("baseline", b), ("candidate", c)):
        if not v > 0:
            die(f"pair {p} {role} has {stat} = {v}; a log ratio needs positive values")
    return 100 * math.log(c / b)


def diffs(runs, pairs, stat):
    """d over the baseline-first pairs and over the candidate-first pairs."""
    ab = [d(runs, p, stat) for p in pairs if baseline_first(runs, p)]
    ba = [d(runs, p, stat) for p in pairs if not baseline_first(runs, p)]
    return ab, ba


def estimate(ab, ba, higher_is_better):
    """Effect, order effect and the effect's uncertainty, as 100 * ln(ratio); None where not
    measured."""
    e = dict(effect=None, order=None, se=None, df=None, t=None, bound=None)
    if ab and ba:
        e["effect"] = (mean(ab) + mean(ba)) / 2
        # Half the gap between the two orders' means: how much running second helps.
        order = (mean(ba) - mean(ab)) / 2
        e["order"] = -order if higher_is_better else order
    if len(ab) >= 2 and len(ba) >= 2:
        # The variance within each order only, so the constant order effect stays out of it.
        a, b = var(ab) / len(ab), var(ba) / len(ba)
        if a + b == 0:
            e.update(se=0.0, bound=0.0)
        else:
            # Welch-Satterthwaite degrees of freedom: the two orders may be unequally noisy.
            df = max(1, math.floor((a + b) ** 2 / (a * a / (len(ab) - 1) + b * b / (len(ba) - 1))))
            se = math.sqrt(a + b) / 2
            e.update(se=se, df=df, t=t99(df), bound=t99(df) * se)
    return e


def rule(e, higher_is_better, floor=0.0):
    """What the interval says on its own, or None when there is no bound."""
    if e["bound"] is None:
        return None
    gain = e["effect"] if higher_is_better else -e["effect"]
    if gain - e["bound"] > floor:
        return "improved"
    if gain + e["bound"] < -floor:
        return "regressed"
    return "inconclusive"


def floors(path, meta, stats):
    """Per statistic, the A/A comparison's absolute effect plus its own bound (log scale)."""
    fmeta, fruns, fpairs = load(path)
    if not same_binary(fmeta):
        die(f"{path} is not an A/A comparison (the same binary twice, by SHA-256)")
    for key in ("file", "limit", "cpu", "throughput_pass"):
        if fmeta.get(key) != meta.get(key):
            die(f"{path} was measured with {key} {fmeta.get(key)}, this comparison has {meta.get(key)}")
    out = {}
    for s in stats:
        e = estimate(*diffs(fruns, fpairs, s), s == THROUGHPUT)
        if e["bound"] is None:
            die(f"{path} has too few pairs in each order to give a bound for {s}")
        out[s] = abs(e["effect"]) + e["bound"]
    return out


def report(e, floor, higher_is_better):
    """The numbers of one statistic for the summary line: percent changes and the log scale.
    order_effect_pct is how much faster the second run was. floor_pct is the bar the interval's
    better end must clear for improved (latency: high_pct below it; throughput: low_pct above it),
    and floor_regressed_pct the bar its worse end must clear for regressed."""
    lo = hi = None
    if e["bound"] is not None:
        lo, hi = e["effect"] - e["bound"], e["effect"] + e["bound"]
    sign = 1 if higher_is_better else -1
    order = None if e["order"] is None else (pct(e["order"]) if higher_is_better else -pct(-e["order"]))
    return {"effect_pct": pct(e["effect"]), "low_pct": pct(lo), "high_pct": pct(hi),
            "order_effect_pct": order,
            "floor_pct": None if floor is None else pct(sign * floor),
            "floor_regressed_pct": None if floor is None else pct(-sign * floor),
            "df": e["df"], "t": e["t"],
            "effect_ln_x100": e["effect"], "se_ln_x100": e["se"], "bound_ln_x100": e["bound"],
            "order_effect_ln_x100": e["order"], "floor_ln_x100": floor}


def median(xs):
    xs = sorted(xs)
    n = len(xs)
    return xs[n // 2] if n % 2 else (xs[n // 2 - 1] + xs[n // 2]) / 2


def flagged(order):
    """Runs whose wall time or either control-loop time is over 1.5 times the comparison's median:
    the host slept or was busy. Flags never change a number."""
    mw = median([r["latency_wall_s"] for r in order])
    mc = median([r[k] for r in order for k in ("control_ms_start", "control_ms_end")])
    out = []
    for r in order:
        why = []
        if r["latency_wall_s"] > 1.5 * mw:
            why.append(f"took {r['latency_wall_s']:.1f} s against a median of {mw:.1f} s")
        c = max(r["control_ms_start"], r["control_ms_end"])
        if c > 1.5 * mc:
            why.append(f"control loop took {c:.1f} ms against a median of {mc:.1f} ms")
        if why:
            out.append(f"pair {r['pair']} {r['role']} " + "; ".join(why))
    return out


def num(x, missing="-"):
    return missing if x is None else f"{x:.2f}"


def main(argv=None):
    ap = argparse.ArgumentParser(prog="compare_summary.py")
    ap.add_argument("--check", action="store_true")
    ap.add_argument("-s", dest="target")
    ap.add_argument("-f", dest="floor")
    ap.add_argument("runs")
    a = ap.parse_args(argv)

    meta, runs, pairs = load(a.runs)
    stats = stats_of(meta)
    if a.target is not None and a.target not in stats:
        die(f"-s {a.target} is not one of: {' '.join(stats)}"
            + ("" if THROUGHPUT in stats else " (throughput needs -t)"))
    fl = floors(a.floor, meta, stats) if a.floor else {}
    if a.check:
        return 0
    if not pairs:
        die(f"{a.runs} has no complete pairs")

    err = sys.stderr
    order = sorted((runs[p, role] for p in pairs for role in ("baseline", "candidate")),
                   key=lambda r: (r["pair"], r["position"]))
    print(f"\n{'pair':<4} {'pos':<3} {'role':<9} {'ctl_ms_0':>10} {'ctl_ms_1':>10} {'wall_s':>9} "
          f"{'all_p50':>10} {'book_p50':>10} {'book_p99':>10} {'book_mean':>10}", file=err)
    for r in order:
        print(f"{r['pair']:<4} {r['position']:<3} {r['role']:<9} {r['control_ms_start']:>10.1f} "
              f"{r['control_ms_end']:>10.1f} {r['latency_wall_s']:>9.1f} {r['all_p50_ns']:>10.1f} "
              f"{r['book_p50_ns']:>10.1f} {r['book_p99_ns']:>10.1f} {r['book_mean_ns']:>10.2f}", file=err)

    print("\nd = 100 * ln(candidate / baseline), per pair (order AB: baseline ran first)", file=err)
    print(f"{'pair':<4} {'order':<5}" + "".join(f" {s:>12}" for s in stats), file=err)
    for p in pairs:
        o = "AB" if baseline_first(runs, p) else "BA"
        line = {"pair_diff": True, "pair": p, "order": o}
        line.update({f"{s}_ln_x100": round(d(runs, p, s), 3) for s in stats})
        print(json.dumps(line, separators=(",", ":")))
        print(f"{p:<4} {o:<5}" + "".join(f" {d(runs, p, s):>12.2f}" for s in stats), file=err)

    n1 = sum(1 for p in pairs if baseline_first(runs, p))
    n2 = len(pairs) - n1
    balanced = n1 == n2 and n1 >= MIN_PER_ORDER
    aa = same_binary(meta)
    warnings = flagged(order)
    summary = {"summary": True, "aa": aa, "flagged_runs": len(warnings), "pairs": len(pairs),
               "pairs_baseline_first": n1, "pairs_candidate_first": n2}
    for k in ("cpu", "file", "limit", "throughput_pass", "baseline", "candidate",
              "baseline_sha256", "candidate_sha256"):
        summary[k] = meta.get(k)
    summary["floor_source"] = "A/A file" if fl else "none"
    summary["target"] = a.target
    summary["verdict"] = None
    by_rule = None

    print(f"\n{'statistic':<22} {'effect%':>9} {'low%':>9} {'high%':>9} {'df':>3} {'order%':>13} "
          f"{'imp_bar%':>9} {'reg_bar%':>9}  verdict", file=err)
    for s in stats:
        hib = s == THROUGHPUT
        e = estimate(*diffs(runs, pairs, s), hib)
        shown = "information"
        if s == a.target:
            by_rule = rule(e, hib, fl.get(s, 0.0))
            summary["verdict"] = by_rule if balanced else "inconclusive (too few pairs)"
            if not balanced:
                summary["rule_verdict"] = by_rule
            shown = summary["verdict"]
        r = report(e, fl.get(s), hib)
        summary[s] = {k: round(x, 3) if isinstance(x, float) else x for k, x in r.items()}
        print(f"{s:<22} {num(r['effect_pct']):>9} {num(r['low_pct']):>9} {num(r['high_pct']):>9} "
              f"{'-' if e['df'] is None else e['df']:>3} {num(r['order_effect_pct'], 'not measured'):>13} "
              f"{num(r['floor_pct']):>9} {num(r['floor_regressed_pct']):>9}  {shown}", file=err)
    print(json.dumps(summary, separators=(",", ":")))

    print(f"""
{len(pairs)} pairs: {n1} baseline-first (AB), {n2} candidate-first (BA).
d is 100 * ln(candidate / baseline). With m1, v1, n1 the mean, sample variance and count of d over
AB pairs and m2, v2, n2 over BA pairs: effect = (m1 + m2) / 2, se = sqrt(v1/n1 + v2/n2) / 2, and
bound = t * se, with t the one-sided 99% Student-t value for the Welch-Satterthwaite degrees of
freedom (df, rounded down). low and high are effect - bound and effect + bound.
order: how much faster the second run of a pair was (from half the gap between the two orders'
means); negative when the first run was faster.
floor: with -f, the A/A comparison's absolute effect plus its own bound, on the log scale.
imp_bar and reg_bar are the floor as the percent change each end of the interval must clear:
for latency, improved needs high below imp_bar and regressed needs low above reg_bar; for
throughput, improved needs low above imp_bar and regressed needs high below reg_bar.
Effects, interval ends and order are shown as percent changes.
Rule for the declared statistic: improved when the interval lies wholly on the better side of
zero (lower latency, higher throughput) and, with -f, beyond the floor; regressed in the mirror
case; otherwise inconclusive.""", file=err)
    if a.target is None:
        print("No statistic was declared with -s, so there is no verdict; every row is information.",
              file=err)
    elif not balanced:
        alone = by_rule if by_rule else "nothing (no bound)"
        print(f"The rule needs the same number of pairs in each order and at least {MIN_PER_ORDER} of each, "
              f"so the verdict is inconclusive. The interval alone gives {alone}.", file=err)
    if aa:
        print("Both binaries are the same file: this is an A/A check of the method, and a sound method "
              "gives inconclusive.", file=err)
        if by_rule in ("improved", "regressed"):
            print(f"WARNING: this A/A comparison gives {by_rule} for {a.target}, so the setup is not "
                  "measuring cleanly. Find the cause before trusting any comparison; do not rerun the "
                  "A/A comparison in the hope that it passes.", file=err)
    for w in warnings:
        print(f"WARNING: {w}.", file=err)
    if warnings:
        print(f"WARNING: the host slept or was busy during {len(warnings)} run(s); rerun the comparison "
              "as a new round before trusting it.", file=err)
    return 0


if __name__ == "__main__":
    sys.exit(main())
