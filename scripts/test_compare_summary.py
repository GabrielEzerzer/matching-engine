#!/usr/bin/env python3
# Checks the arithmetic and the verdict rule of scripts/compare_summary.py on small hand-made
# comparisons with known answers, without running a benchmark.
#
#   python3 scripts/test_compare_summary.py
import atexit
import contextlib
import io
import json
import math
import os
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import compare_summary as cs  # noqa: E402

SHA_A, SHA_B = "a" * 64, "b" * 64


def comparison(ab, ba, stat="all_mean_ns", sha_c=SHA_B, tput=False, flat=0.0, second=1.0):
    """Lines of a comparison in which `stat` has the given d = 100 * ln(candidate / baseline) on
    the baseline-first and candidate-first pairs, every other statistic has d = `flat`, and the
    second run of every pair is multiplied by `second` (an order effect)."""
    stats = cs.stats_of({"throughput_pass": tput})
    meta = {"meta": True, "cpu": "5", "file": "f", "limit": "none", "throughput_pass": tput,
            "baseline": "base", "candidate": "cand", "baseline_sha256": SHA_A,
            "candidate_sha256": sha_c}
    lines = [meta]
    ds = [(x, True) for x in ab] + [(x, False) for x in ba]
    for p, (x, base_first) in enumerate(ds, 1):
        roles = ("baseline", "candidate") if base_first else ("candidate", "baseline")
        for pos, role in enumerate(roles, 1):
            r = {"pair": p, "position": pos, "role": role, "latency_wall_s": 100.0,
                 "control_ms_start": 300.0, "control_ms_end": 300.0}
            for s in stats:
                dd = x if s == stat else flat
                v = 1000.0 if role == "baseline" else 1000.0 * math.exp(dd / 100)
                r[s] = v * (second if pos == 2 else 1.0)
            lines.append(r)
    return lines


def write(lines):
    f = tempfile.NamedTemporaryFile("w", suffix=".jsonl", delete=False)
    f.write("".join(json.dumps(x) + "\n" for x in lines))
    f.close()
    atexit.register(os.unlink, f.name)
    return f.name


def summarise(*args):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = cs.main(list(args))
    return code, json.loads(out.getvalue().splitlines()[-1]), err.getvalue()


def refused(*args):
    with contextlib.redirect_stderr(io.StringIO()) as err:
        try:
            cs.main(list(args))
        except SystemExit as e:
            return e.code, err.getvalue()
    return 0, err.getvalue()


class Arithmetic(unittest.TestCase):
    def test_known_effect_se_and_bound(self):
        # AB: mean -5, variance 1; BA: mean -2, variance 1. Equal variances: Welch df = 4.
        e = cs.estimate([-6, -4, -5], [-1, -3, -2], higher_is_better=False)
        self.assertAlmostEqual(e["effect"], -3.5)
        self.assertAlmostEqual(e["order"], 1.5)
        self.assertAlmostEqual(e["se"], math.sqrt(1 / 3 + 1 / 3) / 2)  # 0.40825
        self.assertEqual(e["df"], 4)
        self.assertEqual(e["t"], 3.747)
        self.assertAlmostEqual(e["bound"], 3.747 * 0.4082483, places=5)  # 1.5297
        self.assertEqual(cs.rule(e, False), "improved")
        # The same d on a throughput statistic, where higher is better, is a regression.
        self.assertEqual(cs.rule(cs.estimate([-6, -4, -5], [-1, -3, -2], True), True), "regressed")

    def test_welch_df_with_unequal_variances(self):
        # AB variance 1, BA variance 4: a = 1/3, b = 4/3,
        # df = (5/3)^2 / ((1/3)^2 / 2 + (4/3)^2 / 2) = 50/17 = 2.94, rounded down to 2.
        e = cs.estimate([-6, -4, -5], [0, -4, -2], False)
        self.assertAlmostEqual(e["se"], math.sqrt(5 / 3) / 2)
        self.assertEqual(e["df"], 2)
        self.assertEqual(e["t"], 6.965)
        # Two candidate-first pairs give df at least 1.
        self.assertEqual(cs.estimate([-6, -4, -5, -3], [0, -40], False)["df"], 1)

    def test_no_noise_gives_zero_bound(self):
        e = cs.estimate([-5, -5, -5], [-2, -2, -2], False)
        self.assertEqual((e["se"], e["bound"], e["df"]), (0.0, 0.0, None))
        self.assertEqual(cs.rule(e, False), "improved")

    def test_noise_makes_it_inconclusive(self):
        # Same means, variance 25 in each order: se 2.0412, bound 7.649 > |effect| 3.5.
        e = cs.estimate([-10, 0, -5], [3, -7, -2], False)
        self.assertAlmostEqual(e["se"], math.sqrt(25 / 3 + 25 / 3) / 2)
        self.assertEqual(cs.rule(e, False), "inconclusive")

    def test_order_effect_alone_is_not_an_effect(self):
        e = cs.estimate([-9.2, -8.8, -9.0], [9.1, 8.9, 9.0], False)
        self.assertAlmostEqual(e["effect"], 0.0)
        self.assertAlmostEqual(e["order"], 9.0)
        self.assertEqual(cs.rule(e, False), "inconclusive")

    def test_throughput_order_sign(self):
        # Throughput: the second run is faster, so d is +9 when the candidate runs second (AB) and
        # -9 when the baseline does (BA). Running second helps: the order effect is positive.
        self.assertAlmostEqual(cs.estimate([9, 9, 9], [-9, -9, -9], True)["order"], 9.0)
        self.assertAlmostEqual(cs.estimate([-9, -9, -9], [9, 9, 9], False)["order"], 9.0)

    def test_floor_raises_the_bar_for_improved(self):
        e = cs.estimate([-6, -4, -5], [-1, -3, -2], False)  # better by 3.5 - 1.53 = 1.97 at least
        self.assertEqual(cs.rule(e, False, floor=1.9), "improved")
        self.assertEqual(cs.rule(e, False, floor=2.0), "inconclusive")

    def test_floor_raises_the_bar_for_regressed(self):
        e = cs.estimate([6, 4, 5], [1, 3, 2], False)  # worse by 3.5 - 1.53 = 1.97 at least
        self.assertEqual(cs.rule(e, False, floor=1.9), "regressed")
        self.assertEqual(cs.rule(e, False, floor=2.0), "inconclusive")

    def test_one_order_empty(self):
        e = cs.estimate([-6, -4, -5], [], False)
        self.assertIsNone(e["effect"])
        self.assertIsNone(e["order"])
        self.assertIsNone(cs.rule(e, False))

    def test_t_table_rounds_df_down(self):
        self.assertEqual(cs.t99(10), 2.764)
        self.assertEqual(cs.t99(22), 2.528)  # uses df 20

    def test_pure_multiplicative_order_effect_cancels(self):
        # The same binary twice, the second run 10% faster, no noise. Log ratios give exactly 0;
        # percentage differences would give (-10 + 11.1) / 2 = +0.56%.
        lines = comparison([0, 0, 0], [0, 0, 0], sha_c=SHA_A, second=0.9)
        meta, runs, pairs = cs.load(write(lines))
        e = cs.estimate(*cs.diffs(runs, pairs, "all_mean_ns"), False)
        self.assertAlmostEqual(e["effect"], 0.0, places=9)
        self.assertAlmostEqual(cs.pct(e["order"]), 100 * (1 / 0.9 - 1), places=6)  # 11.1%: first run vs second


class EndToEnd(unittest.TestCase):
    def test_declared_statistic_only_gets_a_verdict(self):
        _, s, _ = summarise("-s", "all_mean_ns", write(comparison([-6, -4, -5], [-1, -3, -2])))
        self.assertEqual(s["target"], "all_mean_ns")
        self.assertEqual(s["verdict"], "improved")
        m = s["all_mean_ns"]
        self.assertEqual(m["effect_ln_x100"], -3.5)
        self.assertEqual(m["bound_ln_x100"], 1.53)
        self.assertEqual(m["effect_pct"], round(100 * math.expm1(-0.035), 3))  # -3.439
        b = 3.747 * math.sqrt(2 / 3) / 2 / 100
        self.assertEqual(m["low_pct"], round(100 * math.expm1(-0.035 - b), 3))
        self.assertEqual(m["high_pct"], round(100 * math.expm1(-0.035 + b), 3))
        self.assertNotIn("verdict", s["all_p50_ns"])

    def test_no_declared_statistic_no_verdict(self):
        _, s, err = summarise(write(comparison([-6, -4, -5], [-1, -3, -2])))
        self.assertIsNone(s["verdict"])
        self.assertIn("no verdict", err)

    def test_too_few_pairs(self):
        # Welch df 2 (t 6.965): bound 2.04 against an effect of -3.5, so the interval alone improves.
        _, s, _ = summarise("-s", "all_mean_ns", write(comparison([-6, -4, -5], [-2.1, -1.9])))
        self.assertEqual(s["verdict"], "inconclusive (too few pairs)")
        self.assertEqual(s["rule_verdict"], "improved")

    def test_single_pair_order_effect_not_measured(self):
        _, s, err = summarise(write(comparison([-6], [])))
        self.assertIsNone(s["all_mean_ns"]["order_effect_pct"])
        self.assertIn("not measured", err)

    def test_aa_is_labelled_a_method_check(self):
        lines = comparison([-9.2, -8.8, -9.0], [9.1, 8.9, 9.0], sha_c=SHA_A)
        _, s, err = summarise("-s", "all_mean_ns", write(lines))
        self.assertTrue(s["aa"])
        self.assertEqual(s["verdict"], "inconclusive")
        self.assertIn("A/A check of the method", err)

    def test_floor_from_aa_file(self):
        # A/A all_mean_ns: effect 0, d -1,1,0 / 1,-1,0: floor = 3.747 * sqrt(2/3) / 2 = 1.53.
        aa = write(comparison([-1, 1, 0], [1, -1, 0], sha_c=SHA_A, flat=0.5))
        _, s, _ = summarise("-s", "all_mean_ns", "-f", aa, write(comparison([-6, -4, -5], [-1, -3, -2])))
        self.assertEqual(s["all_mean_ns"]["floor_ln_x100"], 1.53)
        self.assertEqual(s["verdict"], "improved")  # 3.5 - 1.53 = 1.97 > 1.53

    def test_floor_must_match_throughput_setting(self):
        aa = write(comparison([-1, 1, 0], [1, -1, 0], sha_c=SHA_A, tput=True))
        code, err = refused("-f", aa, write(comparison([-6, -4, -5], [-1, -3, -2])))
        self.assertEqual(code, 2)
        self.assertIn("throughput_pass", err)

    def test_floor_must_be_aa(self):
        ab = write(comparison([-6, -4, -5], [-1, -3, -2]))
        self.assertEqual(refused("-f", ab, ab)[0], 2)

    def test_floor_without_sha_is_not_aa(self):
        lines = comparison([-1, 1, 0], [1, -1, 0], sha_c=SHA_A)
        del lines[0]["baseline_sha256"], lines[0]["candidate_sha256"]
        code, err = refused("-f", write(lines), write(comparison([-6, -4, -5], [-1, -3, -2])))
        self.assertEqual(code, 2)
        self.assertIn("not an A/A comparison", err)

    def test_unknown_statistic_refused(self):
        ab = write(comparison([-6, -4, -5], [-1, -3, -2]))
        code, err = refused("--check", "-s", "throughput_msgs_per_s", ab)
        self.assertEqual(code, 2)
        self.assertIn("throughput needs -t", err)


if __name__ == "__main__":
    unittest.main()
