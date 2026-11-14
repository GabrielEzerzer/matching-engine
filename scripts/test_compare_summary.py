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


def comparison(ab, ba, stat="all_mean_ns", sha_c=SHA_B, tput=False, flat=None):
    """Lines of a comparison in which `stat` has the given d (percent) on the baseline-first and
    candidate-first pairs, and every other statistic has d = `flat` (default 0)."""
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
                dd = x if s == stat else (flat or 0.0)
                r[s] = 1000.0 if role == "baseline" else 1000.0 * (1 + dd / 100)
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


class Arithmetic(unittest.TestCase):
    def test_known_effect_se_and_bound(self):
        # AB: mean -5, variance 1; BA: mean -2, variance 1.
        e = cs.estimate([-6, -4, -5], [-1, -3, -2], higher_is_better=False)
        self.assertAlmostEqual(e["effect_pct"], -3.5)
        self.assertAlmostEqual(e["order_effect_pct"], 1.5)
        self.assertAlmostEqual(e["se_pct"], math.sqrt(1 / 3 + 1 / 3) / 2)  # 0.40825
        self.assertEqual(e["df"], 4)
        self.assertEqual(e["t"], 3.747)
        self.assertAlmostEqual(e["bound_pct"], 3.747 * 0.4082483, places=5)  # 1.5297
        self.assertEqual(cs.rule(e, False), "improved")
        # The same d on a throughput statistic, where higher is better, is a regression.
        self.assertEqual(cs.rule(cs.estimate([-6, -4, -5], [-1, -3, -2], True), True), "regressed")

    def test_noise_makes_it_inconclusive(self):
        # Same means, variance 25 in each order: se 2.0412, bound 7.649 > |effect| 3.5.
        e = cs.estimate([-10, 0, -5], [3, -7, -2], False)
        self.assertAlmostEqual(e["se_pct"], math.sqrt(25 / 3 + 25 / 3) / 2)
        self.assertEqual(cs.rule(e, False), "inconclusive")

    def test_order_effect_alone_is_not_an_effect(self):
        # Running second is 9% faster and the binaries are equal: d is -9 on AB, +9 on BA.
        e = cs.estimate([-9.2, -8.8, -9.0], [9.1, 8.9, 9.0], False)
        self.assertAlmostEqual(e["effect_pct"], 0.0)
        self.assertAlmostEqual(e["order_effect_pct"], 9.0)
        self.assertEqual(cs.rule(e, False), "inconclusive")

    def test_floor_raises_the_bar(self):
        e = cs.estimate([-6, -4, -5], [-1, -3, -2], False)  # better by 3.5 - 1.53 = 1.97 at least
        self.assertEqual(cs.rule(e, False, floor=1.9), "improved")
        self.assertEqual(cs.rule(e, False, floor=2.0), "inconclusive")

    def test_one_order_empty(self):
        e = cs.estimate([-6, -4, -5], [], False)
        self.assertIsNone(e["effect_pct"])
        self.assertIsNone(e["order_effect_pct"])
        self.assertIsNone(cs.rule(e, False))

    def test_t_table_rounds_df_down(self):
        self.assertEqual(cs.t99(10), 2.764)
        self.assertEqual(cs.t99(22), 2.528)  # uses df 20


class EndToEnd(unittest.TestCase):
    def test_declared_statistic_only_gets_a_verdict(self):
        path = write(comparison([-6, -4, -5], [-1, -3, -2]))
        code, s, err = summarise("-s", "all_mean_ns", path)
        self.assertEqual(code, 0)
        self.assertEqual(s["target"], "all_mean_ns")
        self.assertEqual(s["verdict"], "improved")
        self.assertEqual(s["all_mean_ns"]["effect_pct"], -3.5)
        self.assertEqual(s["all_mean_ns"]["bound_pct"], 1.53)
        self.assertNotIn("verdict", s["all_p50_ns"])

    def test_no_declared_statistic_no_verdict(self):
        _, s, err = summarise(write(comparison([-6, -4, -5], [-1, -3, -2])))
        self.assertIsNone(s["verdict"])
        self.assertIn("no verdict", err)

    def test_too_few_pairs(self):
        _, s, err = summarise("-s", "all_mean_ns", write(comparison([-6, -4, -5], [-1, -3])))
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
        # A/A all_mean_ns: effect 0, d -1,1,0 / 1,-1,0: se sqrt(1/3+1/3)/2, floor 3.747 * 0.40825.
        aa = write(comparison([-1, 1, 0], [1, -1, 0], sha_c=SHA_A, flat=0.5))
        _, s, _ = summarise("-s", "all_mean_ns", "-f", aa, write(comparison([-6, -4, -5], [-1, -3, -2])))
        self.assertEqual(s["all_mean_ns"]["floor_pct"], 1.53)
        self.assertEqual(s["verdict"], "improved")  # 3.5 - 1.53 = 1.97 > 1.53

    def test_floor_must_match_throughput_setting(self):
        aa = write(comparison([-1, 1, 0], [1, -1, 0], sha_c=SHA_A, tput=True))
        ab = write(comparison([-6, -4, -5], [-1, -3, -2]))
        with self.assertRaises(SystemExit) as cm, contextlib.redirect_stderr(io.StringIO()) as err:
            cs.main(["-f", aa, ab])
        self.assertEqual(cm.exception.code, 2)
        self.assertIn("throughput_pass", err.getvalue())

    def test_floor_must_be_aa(self):
        ab = write(comparison([-6, -4, -5], [-1, -3, -2]))
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
            cs.main(["-f", ab, ab])

    def test_unknown_statistic_refused(self):
        ab = write(comparison([-6, -4, -5], [-1, -3, -2]))
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()) as err:
            cs.main(["--check", "-s", "throughput_msgs_per_s", ab])
        self.assertIn("throughput needs -t", err.getvalue())


def micro_comparison(ab, ba, part="level", sha_c=SHA_B, checksums=("c0ffee",)):
    """A compare.sh -m comparison: ns_per_op_median has the given d, and the runs carry the
    microbenchmark's fields, cycling through `checksums` for workload_checksum."""
    lines = [{"meta": True, "cpu": "5", "file": "f", "limit": "none", "throughput_pass": False,
              "part": part, "baseline": "base", "candidate": "cand", "baseline_sha256": SHA_A,
              "candidate_sha256": sha_c}]
    ds = [(x, True) for x in ab] + [(x, False) for x in ba]
    n = 0
    for p, (x, base_first) in enumerate(ds, 1):
        roles = ("baseline", "candidate") if base_first else ("candidate", "baseline")
        for pos, role in enumerate(roles, 1):
            v = 10.0 if role == "baseline" else 10.0 * (1 + x / 100)
            lines.append({"pair": p, "position": pos, "role": role, "timed_s": 9.0,
                          "control_ms_start": 300.0, "control_ms_end": 300.0,
                          "ns_per_op_median": v, "ns_per_op_min": v, "spread_pct": 1.0,
                          "allocs_per_op": 0.1, "workload_checksum": checksums[n % len(checksums)]})
            n += 1
    return lines


class Micro(unittest.TestCase):
    def test_micro_statistics(self):
        _, s, err = summarise("-s", "ns_per_op_median", write(micro_comparison([-6, -4, -5], [-1, -3, -2])))
        self.assertEqual(s["part"], "level")
        self.assertEqual(s["verdict"], "improved")
        self.assertNotIn("all_p50_ns", s)
        self.assertIn("ns_per_op_min", err)

    def test_workload_checksums_must_agree(self):
        lines = micro_comparison([-6, -4, -5], [-1, -3, -2], checksums=("aa", "bb"))
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()) as err:
            cs.main([write(lines)])
        self.assertIn("workload checksums", err.getvalue())

    def test_floor_must_be_the_same_part(self):
        aa = write(micro_comparison([-1, 1, 0], [1, -1, 0], part="side", sha_c=SHA_A))
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()) as err:
            cs.main(["-f", aa, write(micro_comparison([-6, -4, -5], [-1, -3, -2]))])
        self.assertIn("part", err.getvalue())


if __name__ == "__main__":
    unittest.main()
