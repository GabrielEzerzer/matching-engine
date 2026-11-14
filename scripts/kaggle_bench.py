#!/usr/bin/env python3
# Runs a plan of benchmark comparisons on a Kaggle CPU session. One file is both the local
# packager and the kernel: --write-kernel writes the dataset and kernel folders that the kaggle
# CLI uploads, and the kernel's copy (this file with the plan embedded) runs the plan on Kaggle.
#
#   python scripts/kaggle_bench.py --write-kernel PLAN DIR [--owner NAME]
#   python scripts/kaggle_bench.py --dry-run PLAN
#   python scripts/kaggle_bench.py --self-test
#
# --write-kernel writes DIR/src (a git bundle with dataset-metadata.json) and DIR/kernel (the
# kernel's code with the plan and the commit ids embedded, with kernel-metadata.json: a private
# script kernel, CPU only, internet on, DIR/src's dataset attached). The bundle holds no history:
# for each branch the plan names, one commit of that branch's tree with no parent, author and
# committer "bench <bench@localhost>", a fixed date and a fixed message, under
# refs/kaggle-bundle/<branch>. The real commit ids travel only in the plan, so STATUS.json and
# SUMMARY.md report them. The Kaggle owner is --owner or $KAGGLE_USERNAME.
#
# On Kaggle (run with no arguments under /kaggle) the kernel installs Rust with rustup into
# /kaggle/tmp, fetches each label's branch from the bundle and checks it is at the embedded bundle
# commit, fetches the ITCH day with the base checkout's scripts/fetch-itch.sh into /kaggle/tmp/itch,
# builds every label with that label's scripts/bench-build.sh (then writes the real commit into the
# build's commit.txt, which compare.sh passes to the binaries), and only then runs the plan's steps
# one at a time with the base checkout's scripts/compare.sh (bench.sh for `bench` steps, whose
# output gets the real commit in place of the bundle commit). Every command stops at a hard limit
# of 700 minutes from kernel start. Each step's stdout goes to /kaggle/working/results/<step>.jsonl
# and stderr to <step>.err; STATUS.json and SUMMARY.md are rewritten every time the status changes.
# Before setup the kernel writes plan.json (the plan as it runs, defaults filled in); STATUS.json and
# SUMMARY.md carry the session id (<plan name>-<UTC start as YYYYMMDDTHHMMSSZ>) and the SHA-256 of
# this file as it runs. Each label that built gets its settings.txt and build.txt copied to
# build-<label>-settings.txt and build-<label>-build.txt. Each step that runs a comparison or bench.sh
# records the pinned CPU's steal and total jiffies from /proc/stat across the step and its wall
# seconds; the readings are informational and never change a state, a verdict or a winner.
# BENCHMARKING.md, "Running the comparisons on Kaggle", describes the plan format and the step kinds.
#
# KAGGLE_BENCH_ROOT (default /kaggle) moves the kernel's root, for trying the kernel elsewhere.
# Standard library only.
import argparse
import ast
import contextlib
import datetime as dt
import hashlib
import io
import json
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
import unittest.mock
from pathlib import Path, PurePath, PurePosixPath
from types import SimpleNamespace

PLAN = None  # --write-kernel replaces this line in the kernel's copy

KINDS = ("aa", "compare", "confirm_best", "window", "golden", "bench")
PARTS = ("side", "level", "orders", "book", "parse")
MICRO_STATS = ("ns_per_op_median", "ns_per_op_min")
WINDOW_STATS = tuple(f"{s}_{q}_ns" for s in ("all", "book") for q in ("mean", "p50", "p99", "p999"))
DEFAULT_CPU = 3
DEFAULT_STOP_MIN = 630  # 10 h 30 min; Kaggle ends a session at 12 h
HARD_STOP_MIN = 700  # no command runs past this many minutes from kernel start
SEPARATING_KINDS = ("aa", "bench", "window")  # with a compare outside the group: see separators()
BUNDLE_REF = "refs/kaggle-bundle/"
BUNDLE_ENV = {"GIT_AUTHOR_NAME": "bench", "GIT_AUTHOR_EMAIL": "bench@localhost",
              "GIT_AUTHOR_DATE": "2000-01-01T00:00:00+0000", "GIT_COMMITTER_NAME": "bench",
              "GIT_COMMITTER_EMAIL": "bench@localhost", "GIT_COMMITTER_DATE": "2000-01-01T00:00:00+0000"}
BUNDLE_MESSAGE = "benchmark snapshot"
MIN_FREE_GB = 40  # ITCH (12 GB with the .gz), a release target per label, test builds
ITCH_NAME = "12302019.NASDAQ_ITCH50"
GOLDEN = "harness/golden/12302019.txt"
NAME_RE = re.compile(r"[a-z0-9][a-z0-9-]{0,40}")
LABEL_RE = re.compile(r"[A-Za-z0-9_-][A-Za-z0-9._-]*")
WINDOW_RE = re.compile(r"\d\d:\d\d:\d\d-\d\d:\d\d:\d\d")

PROC_STAT = Path("/proc/stat")  # the self-test points this at a temporary file
UNAVAILABLE = "unavailable"

T0 = time.monotonic()


def log(msg):
    print(f"[{(time.monotonic() - T0) / 60:7.1f} min] {msg}", flush=True)


def now_iso():
    return dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def slug(plan):
    return f"me-bench-{plan['name']}"


# ---------------------------------------------------------------- the plan

def check_pairs(n, where, err):
    if not isinstance(n, int) or isinstance(n, bool) or n < 6 or n % 2:
        err(f"{where}: pairs must be an even number of at least 6, not {n!r}")


def separators(steps, i):
    """The steps between the last of confirm_best steps[i]'s comparisons and the confirmation that
    separate the two measurements: an aa, bench or window step, or a compare of a candidate outside
    the group."""
    ids = [x.get("id") if isinstance(x, dict) else None for x in steps]
    members = [m for m in steps[i]["steps"] if m in ids[:i]]
    if not members:
        return []
    last = max(ids.index(m) for m in members)
    cands = {steps[ids.index(m)].get("cand") for m in members}
    return [x for x in steps[last + 1:i] if isinstance(x, dict) and (
        x.get("kind") in SEPARATING_KINDS or (x.get("kind") == "compare" and x.get("cand") not in cands))]


def validate(plan):
    """Every problem with the plan, as messages; empty when it can run."""
    errs = []
    err = errs.append
    if not isinstance(plan, dict):
        return ["the plan is not a JSON object"]
    if not isinstance(plan.get("name"), str) or not NAME_RE.fullmatch(plan["name"]):
        err(f"name must be lowercase letters, digits and dashes, not {plan.get('name')!r}")
    labels = plan.get("labels")
    if not isinstance(labels, dict) or not labels:
        err("labels must map each label to its branch")
        labels = {}
    for lab, br in labels.items():
        if not LABEL_RE.fullmatch(lab):
            err(f"bad label {lab!r}")
        if not isinstance(br, str) or not br or br.startswith("-") or ".." in br or " " in br:
            err(f"label {lab!r} has no usable branch ({br!r})")
    base = plan.get("base")
    if base not in labels:
        err(f"the base label {base!r} has no branch in labels")
    cpu = plan.get("cpu", DEFAULT_CPU)
    if not isinstance(cpu, int) or isinstance(cpu, bool) or cpu < 0:
        err(f"cpu must be a CPU number, not {cpu!r}")
    stop = plan.get("stop_after_min", DEFAULT_STOP_MIN)
    if not isinstance(stop, int) or isinstance(stop, bool) or stop <= 0 or stop >= HARD_STOP_MIN:
        err(f"stop_after_min must be a positive number of minutes below the hard limit of "
            f"{HARD_STOP_MIN}, not {stop!r}")
    steps = plan.get("steps")
    if not isinstance(steps, list):
        err("steps must be a list")
        steps = []
    seen, groups, compared = {}, set(), set()
    for i, s in enumerate(steps, 1):
        if not isinstance(s, dict):
            err(f"step {i} is not an object")
            continue
        sid, kind = s.get("id"), s.get("kind")
        where = f"step {i} ({sid})"
        if not isinstance(sid, str) or not LABEL_RE.fullmatch(sid) or sid in ("STATUS", "SUMMARY"):
            err(f"step {i}: bad id {sid!r}")
        elif sid in seen:
            err(f"{where}: the id is used twice")
        if kind not in KINDS:
            err(f"{where}: unknown kind {kind!r}; kinds are {', '.join(KINDS)}")
            continue
        if kind in ("aa", "compare"):
            if s.get("part") not in PARTS:
                err(f"{where}: part must be one of {', '.join(PARTS)}, not {s.get('part')!r}")
            if s.get("stat") not in MICRO_STATS:
                err(f"{where}: part {s.get('part')} does not report {s.get('stat')!r}; "
                    f"microbenchmarks report {', '.join(MICRO_STATS)}")
            check_pairs(s.get("pairs"), where, err)
            if kind == "compare":
                if s.get("cand") not in labels:
                    err(f"{where}: candidate label {s.get('cand')!r} has no branch in labels")
                elif s.get("cand") == base:
                    err(f"{where}: the candidate is the base; an A/A comparison is kind aa")
                elif (s.get("part"), s.get("cand")) in compared:
                    err(f"{where}: {s['cand']} is compared twice on part {s.get('part')}; "
                        f"each candidate gets exactly one first comparison")
                compared.add((s.get("part"), s.get("cand")))
        elif kind == "confirm_best":
            group, members = s.get("group"), s.get("steps")
            if not isinstance(group, str) or not group:
                err(f"{where}: confirm_best needs a group name")
            elif group in groups:
                err(f"{where}: group {group!r} is confirmed twice")
            if not isinstance(members, list) or not members:
                err(f"{where}: confirm_best needs the list of its compare steps")
                members = []
            for m in members:
                if m not in seen:
                    err(f"{where}: {m!r} is not an earlier step")
                elif seen[m]["kind"] != "compare":
                    err(f"{where}: {m!r} is a {seen[m]['kind']} step, not a compare step")
            known = [seen[m] for m in members if m in seen and seen[m]["kind"] == "compare"]
            for key in ("part", "stat", "pairs"):
                if len({json.dumps(m.get(key)) for m in known}) > 1:
                    err(f"{where}: the alternatives of one group must share their {key}")
            if known and len(known) == len(members) and not separators(steps, i - 1):
                err(f"{where}: no other measurement (an aa, bench or window step, or a compare of a "
                    f"candidate outside the group) between the group's last comparison and its confirmation")
            groups.add(group)
        elif kind in ("window", "golden"):
            if s.get("group") not in groups:
                err(f"{where}: no earlier confirm_best step for group {s.get('group')!r}")
            if kind == "window":
                if not isinstance(s.get("window"), str) or not WINDOW_RE.fullmatch(s["window"]):
                    err(f"{where}: window must be HH:MM:SS-HH:MM:SS, not {s.get('window')!r}")
                if s.get("stat") not in WINDOW_STATS:
                    err(f"{where}: the window comparison does not report {s.get('stat')!r}; "
                        f"it reports {', '.join(WINDOW_STATS)}")
                check_pairs(s.get("pairs"), where, err)
        elif kind == "bench":
            if s.get("label") not in labels:
                err(f"{where}: label {s.get('label')!r} has no branch in labels")
            runs = s.get("runs")
            if not isinstance(runs, int) or isinstance(runs, bool) or runs < 1:
                err(f"{where}: runs must be at least 1, not {runs!r}")
        if isinstance(sid, str):
            seen[sid] = s
    return errs


def load_plan(path):
    try:
        plan = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError) as e:
        sys.exit(f"kaggle_bench: cannot read the plan {path}: {e}")
    errs = validate(plan)
    if errs:
        sys.exit("kaggle_bench: the plan is not valid:\n  " + "\n  ".join(errs))
    return plan


# ---------------------------------------------------------------- paths and commands

def layout(root):
    root = root if isinstance(root, PurePath) else Path(root)
    tmp = root / "tmp"
    return SimpleNamespace(
        root=root, input=root / "input", work=root / "working", results=root / "working" / "results",
        tmp=tmp, rustup=tmp / "rustup", cargo=tmp / "cargo", src=tmp / "src", bins=tmp / "bins",
        target=tmp / "target", itch=tmp / "itch", itch_file=tmp / "itch" / ITCH_NAME)


def env_for(L, label=None):
    env = dict(os.environ, RUSTUP_HOME=str(L.rustup), CARGO_HOME=str(L.cargo),
               BENCH_BINS=str(L.bins), ITCH_DIR=str(L.itch))
    env["PATH"] = f"{L.cargo / 'bin'}{os.pathsep}{env.get('PATH', '')}"
    if label:
        env["CARGO_TARGET_DIR"] = str(L.target / label)
    return env


def compare_argv(plan, L, s, cand):
    """compare.sh for an aa, compare or window step against candidate label `cand`."""
    base = plan["base"]
    cmd = [L.src / base / "scripts" / "compare.sh"]
    if s["kind"] == "window":
        cmd += ["-w", s["window"]]
        b, c = L.bins / base / "bench", L.bins / cand / "bench"
    else:
        cmd += ["-m", s["part"]]
        b, c = L.bins / base / "micro", L.bins / cand / "micro"
    cmd += ["-n", s["pairs"], "-s", s["stat"], "-c", plan.get("cpu", DEFAULT_CPU), b, c]
    return [str(x) for x in cmd]


def golden_argvs(L, label):
    """(cwd, argv) of the golden check and the tests in label's checkout."""
    co = L.src / label
    replay = L.target / label / "release" / "examples" / "replay"
    return [(co, ["cargo", "build", "--release", "--example", "replay"]),
            (co, [str(replay), "--check", GOLDEN, str(L.itch_file)]),
            (co, ["cargo", "test", "--all-targets"])]


def checkout_argvs(bundle, br, dest):
    """Branch br from the bundle as a checkout at dest, on a local branch of the same name."""
    return [["git", "init", "-q", str(dest)],
            ["git", "-C", str(dest), "fetch", "-q", str(bundle), f"{BUNDLE_REF}{br}"],
            ["git", "-C", str(dest), "checkout", "-q", "-B", br, "FETCH_HEAD"]]


def bench_argv(plan, L, s):
    return [str(L.src / s["label"] / "scripts" / "bench.sh"), "-n", str(s["runs"]),
            "-c", str(plan.get("cpu", DEFAULT_CPU))]


def setup_lines(plan, L):
    rustup_sh = L.tmp / "rustup-init.sh"
    lines = [f"curl --proto =https --tlsv1.2 -sSf -o {rustup_sh} https://sh.rustup.rs",
             f"sh {rustup_sh} -y --no-modify-path --profile minimal --default-toolchain stable",
             "rustc -vV"]
    for lab, br in plan["labels"].items():
        lines += [shlex.join(a) for a in checkout_argvs("<bundle>", br, L.src / lab)]
    lines.append(f"{L.src / plan['base'] / 'scripts' / 'fetch-itch.sh'} {L.itch}")
    for lab in plan["labels"]:
        lines.append(f"(cd {L.src / lab} && CARGO_TARGET_DIR={L.target / lab} BENCH_BINS={L.bins} "
                     f"scripts/bench-build.sh {lab})")
    return lines


def dry_run(plan_path):
    plan = load_plan(plan_path)
    L = layout(PurePosixPath("/kaggle"))  # the kernel's paths, also when shown on Windows
    print(f"# plan {plan['name']}: base {plan['base']} ({plan['labels'][plan['base']]}), "
          f"cpu {plan.get('cpu', DEFAULT_CPU)}, stop after {plan.get('stop_after_min', DEFAULT_STOP_MIN)} min")
    print("# setup, once")
    for line in setup_lines(plan, L):
        print(line)
    print(f"# steps, one at a time, from {L.src / plan['base']} with ITCH_DIR={L.itch}")
    steps = {s["id"]: s for s in plan["steps"]}
    for s in plan["steps"]:
        out = L.results / s["id"]
        k = s["kind"]
        if k in ("aa", "compare"):
            cand = plan["base"] if k == "aa" else s["cand"]
            print(f"{shlex.join(compare_argv(plan, L, s, cand))} > {out}.jsonl 2> {out}.err")
        elif k == "confirm_best":
            print(f"# {s['id']}: among {', '.join(s['steps'])}, the improved one with the best effect "
                  f"is compared again; when that also says improved it is the winner of {s['group']}. None improved: "
                  f"nothing runs. No winner when an earlier aa step of the part did not come out inconclusive; "
                  f"skipped when no step between the last comparison and this one finished")
            for m in s["steps"]:
                print(f"#   {shlex.join(compare_argv(plan, L, steps[m], steps[m]['cand']))}")
        elif k == "window":
            cmd = compare_argv(plan, L, s, f"<winner of {s['group']}>")
            print(f"{shlex.join(cmd)} > {out}.jsonl 2> {out}.err   # skipped without a winner")
        elif k == "golden":
            for cwd, argv in golden_argvs(L, f"<winner of {s['group']}>"):
                print(f"(cd {cwd} && {shlex.join(argv)}) >> {out}.out 2>> {out}.err   # skipped without a winner")
        elif k == "bench":
            print(f"(cd {L.src / s['label']} && CARGO_TARGET_DIR={L.target / s['label']} "
                  f"{shlex.join(bench_argv(plan, L, s))}) > {out}.jsonl 2> {out}.err")
    return 0


# ---------------------------------------------------------------- results

def read_summary(path):
    """The last {"summary":true,...} line of a compare.sh or bench.sh output, or None."""
    try:
        lines = Path(path).read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return None
    for line in reversed(lines):
        try:
            rec = json.loads(line)
        except ValueError:
            continue
        if isinstance(rec, dict) and rec.get("summary") is True:
            return rec
    return None


def comparison_fields(summary, stat):
    s = summary.get(stat) or {}
    return {"verdict": summary.get("verdict"), "effect_pct": s.get("effect_pct"),
            "low_pct": s.get("low_pct"), "high_pct": s.get("high_pct"),
            "effect_ln_x100": s.get("effect_ln_x100"), "flagged_runs": summary.get("flagged_runs")}


def choose_best(candidates):
    """candidates: (step id, label, fields or None) in the group's order. The improved one with the
    lowest effect (every statistic here is lower-is-better); a tie goes to the earlier step. None
    when no alternative improved."""
    improved = [c for c in candidates if c[2] and c[2].get("verdict") == "improved"
                and c[2].get("effect_ln_x100") is not None]
    if not improved:
        return None
    return min(improved, key=lambda c: c[2]["effect_ln_x100"])  # min keeps the first of equals


def past_stop(elapsed_s, stop_min):
    return elapsed_s >= stop_min * 60


def write_text(path, text):
    path = Path(path)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(text, encoding="utf-8")
    os.replace(tmp, path)


def write_json(path, obj):
    write_text(path, json.dumps(obj, indent=1) + "\n")


def runner_sha256():
    """SHA-256 of this file as it runs (the kernel's copy has the plan embedded)."""
    try:
        return hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    except (OSError, NameError):  # NameError: a host that runs the script without __file__
        return UNAVAILABLE


def cpu_jiffies(cpu):
    """(steal, total of every field) from /proc/stat's line for cpu, or None when there is none."""
    try:
        lines = PROC_STAT.read_text(encoding="ascii", errors="replace").splitlines()
    except OSError:
        return None
    for line in lines:
        f = line.split()
        if f and f[0] == f"cpu{cpu}":
            try:
                vals = [int(x) for x in f[1:]]
            except ValueError:
                return None
            return (vals[7], sum(vals)) if len(vals) > 7 else None  # steal is the 8th field
    return None


def readings(before, after, wall_s):
    """A step record's scheduling readings, informational only."""
    rec = {"steal_jiffies": UNAVAILABLE, "total_jiffies": UNAVAILABLE, "wall_s": round(wall_s, 3)}
    if before and after:
        rec.update(steal_jiffies=after[0] - before[0], total_jiffies=after[1] - before[1])
    return rec


def steal_pct(rec):
    steal, total = rec.get("steal_jiffies"), rec.get("total_jiffies")
    if isinstance(steal, int) and isinstance(total, int) and total > 0:
        return f"{100 * steal / total:.1f}"
    return "n/a"


def fmt(x):
    return "" if x is None else f"{x:+.2f}"


def write_summary(L, status):
    rows = [f"| step (session {status['session']}) | kind | state | verdict | effect % | interval % | steal % | "
            f"label | note |", "|---|---|---|---|---|---|---|---|---|"]
    for s in status["steps"]:
        interval = f"{fmt(s.get('low_pct'))} .. {fmt(s.get('high_pct'))}" if s.get("low_pct") is not None else ""
        rows.append(f"| {s['id']} | {s['kind']} | {s['state']} | {s.get('verdict') or ''} | "
                    f"{fmt(s.get('effect_pct'))} | {interval} | {steal_pct(s)} | {s.get('label') or ''} | "
                    f"{s.get('note') or ''} |")
    labels = [f"- {lab}: branch {v['branch']}, commit {v.get('commit') or '?'}, build {v.get('build') or 'not run'}"
              for lab, v in status["labels"].items()]
    setup = [f"- {k}: {v['state']}{' (' + v['note'] + ')' if v.get('note') else ''}"
             for k, v in status["setup"].items()]
    text = "\n".join([
        f"# Plan {status['plan']}, session {status['session']}: {status['state']}", "",
        "Timings from a shared Kaggle cloud VM under Linux (see host.txt). The comparisons are relative: "
        "both binaries ran in this session on the same CPU. A verdict is evidence from this session, not a "
        "property of the code on every machine, and a passing A/A check does not certify that the host was "
        "stable throughout. Absolute numbers belong to this machine only. A steal reading of 0 does not show "
        "that the core was uncontended.", "",
        f"Session {status['session']}, runner SHA-256 {status['runner_sha256']}.", "",
        f"Started {status['started']}, last update {status['updated']}, cpu {status['cpu']}, "
        f"stop time {status['stop_at']}.", "", "Setup:", *setup, "", "Labels:", *labels, "",
        *rows, ""])
    write_text(L.results / "SUMMARY.md", text)


# ---------------------------------------------------------------- the kernel

class Kernel:
    def __init__(self, plan, L):
        self.plan, self.L = plan, L
        self.cpu = plan.get("cpu", DEFAULT_CPU)
        self.stop_min = plan.get("stop_after_min", DEFAULT_STOP_MIN)
        self.deadline = T0 + HARD_STOP_MIN * 60  # monotonic time after which no command runs
        start = dt.datetime.now(dt.timezone.utc).replace(microsecond=0)
        self.status = {
            "plan": plan["name"], "session": f"{plan['name']}-{start:%Y%m%dT%H%M%SZ}",
            "runner_sha256": runner_sha256(), "state": "running",
            "started": start.strftime("%Y-%m-%dT%H:%M:%SZ"), "updated": now_iso(),
            "stop_at": (start + dt.timedelta(minutes=self.stop_min)).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "cpu": self.cpu, "rustc": None, "setup": {},
            "labels": {lab: {"branch": br, "commit": None, "bundle_commit": None, "build": None}
                       for lab, br in plan["labels"].items()},
            "steps": [{"id": s["id"], "kind": s["kind"], "state": "pending",
                       **({"group": s["group"]} if "group" in s else {})} for s in plan["steps"]]}

    def save(self):
        self.status["updated"] = now_iso()
        write_json(self.L.results / "STATUS.json", self.status)
        write_summary(self.L, self.status)  # current also when the session is killed

    def write_plan(self):
        """results/plan.json: the plan as the kernel runs it, with cpu and stop time filled in."""
        write_json(self.L.results / "plan.json", {**self.plan, "cpu": self.cpu, "stop_after_min": self.stop_min})

    def stopped(self):
        return past_stop(time.monotonic() - T0, self.stop_min)

    def measured_sh(self, rec, argv, out, err, **kw):
        """self.sh, with the pinned CPU's /proc/stat readings and the wall time put in rec."""
        before, t = cpu_jiffies(self.cpu), time.monotonic()
        try:
            return self.sh(argv, out, err, **kw)
        finally:
            rec.update(readings(before, cpu_jiffies(self.cpu), time.monotonic() - t))

    def sh(self, argv, out, err, cwd=None, env=None, mode="w"):
        """Exit code of argv. Raises TimeoutError at the hard limit, after killing the command's
        whole process group (a pinned grandchild would otherwise keep the CPU busy)."""
        log("$ " + shlex.join(str(a) for a in argv))
        why = f"stopped at the hard limit of {HARD_STOP_MIN} min from kernel start"
        left = self.deadline - time.monotonic()
        if left <= 0:
            raise TimeoutError(f"{why}; not started")
        with open(out, mode) as fo, open(err, mode) as fe:
            p = subprocess.Popen([str(a) for a in argv], stdout=fo, stderr=fe, cwd=cwd,
                                 env=env or env_for(self.L), start_new_session=True)
            try:
                return p.wait(timeout=left)
            except subprocess.TimeoutExpired:
                os.killpg(p.pid, signal.SIGKILL)
                p.wait()
                raise TimeoutError(f"{why}; its process group was killed") from None

    def phase(self, name, fn):
        rec = self.status["setup"][name] = {"state": "running", "start": now_iso()}
        self.save()
        try:
            note = fn()
            rec.update(state="done", note=note)
        except Exception as e:  # recorded, and the run ends cleanly with STATUS written
            rec.update(state="failed", note=str(e))
        rec["end"] = now_iso()
        self.save()
        log(f"{name}: {rec['state']} {rec.get('note') or ''}")
        return rec["state"] == "done"

    # setup phases

    def host(self):
        self.L.tmp.mkdir(parents=True, exist_ok=True)
        lines = []
        for argv in (["nproc"], ["lscpu"], ["lscpu", "-e"], ["sh", "-c", "head -n 5 /proc/meminfo"],
                     ["uname", "-r"], ["df", "-h", str(self.L.tmp), str(self.L.work)]):
            r = subprocess.run(argv, capture_output=True, text=True)
            lines += [f"$ {shlex.join(argv)}", r.stdout.rstrip(), r.stderr.rstrip()]
        for name in ("scaling_governor", "scaling_driver"):
            f = Path(f"/sys/devices/system/cpu/cpu{self.cpu}/cpufreq/{name}")
            try:
                lines.append(f"{f}: {f.read_text().strip()}")
            except OSError:
                lines.append(f"{f}: {UNAVAILABLE}")
        (self.L.results / "host.txt").write_text("\n".join(x for x in lines if x) + "\n", encoding="utf-8")
        cpus = sorted(os.sched_getaffinity(0))
        if self.cpu not in cpus:
            raise RuntimeError(f"the plan pins to CPU {self.cpu}, but this session has CPUs {cpus} "
                               f"(nproc {os.cpu_count()}); set cpu in the plan to one of them")
        free = shutil.disk_usage(self.L.tmp).free / 2**30
        if free < MIN_FREE_GB:
            raise RuntimeError(f"{free:.0f} GB free under {self.L.tmp}, need {MIN_FREE_GB}")
        for tool in ("git", "curl", "taskset", "python3", "sha256sum", "gzip"):
            if not shutil.which(tool):
                raise RuntimeError(f"{tool} not found")
        return f"cpus {cpus}, {free:.0f} GB free under {self.L.tmp}"

    def rust(self):
        script = self.L.tmp / "rustup-init.sh"
        out, err = self.L.results / "setup-rust.log", self.L.results / "setup-rust.err"
        for argv in (["curl", "--proto", "=https", "--tlsv1.2", "-sSf", "-o", script, "https://sh.rustup.rs"],
                     ["sh", script, "-y", "--no-modify-path", "--profile", "minimal",
                      "--default-toolchain", "stable"]):
            if self.sh(argv, out, err, mode="a"):
                raise RuntimeError(f"{argv[0]} failed; see {err.name}")
        r = subprocess.run(["rustc", "-vV"], capture_output=True, text=True, env=env_for(self.L))
        if r.returncode:
            raise RuntimeError("rustc -vV failed after rustup")
        self.status["rustc"] = r.stdout.strip()
        with open(self.L.results / "host.txt", "a", encoding="utf-8") as f:
            f.write("$ rustc -vV\n" + r.stdout)
        return r.stdout.splitlines()[0]

    def clone(self):
        bundles = sorted(self.L.input.rglob("*.bundle"))
        if len(bundles) != 1:
            raise RuntimeError(f"expected one .bundle under {self.L.input}, found {bundles}")
        out, err = self.L.results / "setup-clone.log", self.L.results / "setup-clone.err"
        failed = []
        for lab, br in self.plan["labels"].items():
            dest = self.L.src / lab
            shutil.rmtree(dest, ignore_errors=True)
            want = self.plan["bundle_commits"][br]
            if any(self.sh(a, out, err, mode="a") for a in checkout_argvs(bundles[0], br, dest)):
                failed.append(f"{lab}: checkout from the bundle failed")
                continue
            head = subprocess.run(["git", "-C", dest, "rev-parse", "HEAD"], capture_output=True, text=True)
            got = head.stdout.strip()
            if got != want:
                failed.append(f"{lab}: at {got}, the plan names bundle commit {want}")
                continue
            self.status["labels"][lab].update(commit=self.plan["commits"][br], bundle_commit=got)
        if any(f.startswith(f"{self.plan['base']}:") for f in failed):
            raise RuntimeError("; ".join(failed))
        for f in failed:
            self.status["labels"][f.split(":")[0]]["build"] = "failed (" + f.split(": ", 1)[1] + ")"
        return "; ".join(failed) or f"{len(self.plan['labels'])} checkouts at their commits"

    def fetch(self):
        out, err = self.L.results / "setup-fetch.log", self.L.results / "setup-fetch.err"
        script = self.L.src / self.plan["base"] / "scripts" / "fetch-itch.sh"
        if self.sh([script, self.L.itch], out, err):
            raise RuntimeError(f"fetch-itch.sh failed; see {err.name}")
        if not self.L.itch_file.is_file():
            raise RuntimeError(f"no {self.L.itch_file} after fetch-itch.sh")
        return f"{self.L.itch_file.stat().st_size} bytes"

    def build(self, lab):
        rec = self.status["labels"][lab]
        log_ = self.L.results / f"build-{lab}.log"
        try:
            code = self.sh([self.L.src / lab / "scripts" / "bench-build.sh", lab], log_,
                           log_.with_suffix(".err"), cwd=self.L.src / lab, env=env_for(self.L, lab))
            ok = code == 0 and (self.L.bins / lab / "bench").is_file()
            rec["build"] = "ok" if ok else f"failed (exit {code}; see build-{lab}.err)"
            if ok:  # bench-build.sh wrote the bundle commit; compare.sh passes this file to the binaries
                (self.L.bins / lab / "commit.txt").write_text(rec["commit"] + "\n", encoding="utf-8")
                rec["build_files"] = {}
                for name in ("settings.txt", "build.txt"):
                    copy = f"build-{lab}-{name}"
                    try:
                        shutil.copyfile(self.L.bins / lab / name, self.L.results / copy)
                        rec["build_files"][name] = copy
                    except OSError:
                        rec["build_files"][name] = "missing"
        except Exception as e:  # recorded; the other labels still build
            rec["build"] = f"failed ({type(e).__name__}: {e})"
        self.save()
        log(f"build {lab}: {rec['build']}")

    def built(self, lab):
        return self.status["labels"][lab]["build"] == "ok"

    # steps

    def run_compare(self, s, rec, cand, out_id=None):
        missing = [x for x in (self.plan["base"], cand) if not self.built(x)]
        if missing:
            return rec.update(state="failed", note=f"no build of {', '.join(missing)}")
        out = self.L.results / (out_id or s["id"])
        rec["label"] = cand
        code = self.measured_sh(rec, compare_argv(self.plan, self.L, s, cand), f"{out}.jsonl", f"{out}.err",
                                cwd=self.L.src / self.plan["base"])
        summary = read_summary(f"{out}.jsonl")
        if summary is None or code:
            return rec.update(state="failed", note=f"exit {code}{'' if summary else ', no summary line'}; "
                                                   f"see {out.name}.err")
        rec.update(state="done", **comparison_fields(summary, s["stat"]))
        notes = [f"{summary['flagged_runs']} flagged run(s)"] if summary.get("flagged_runs") else []
        if s["kind"] == "aa" and rec["verdict"] != "inconclusive":
            notes.append(f"A/A gave {rec['verdict']}: the method check failed for part {s['part']}, "
                         f"so its comparisons cannot be trusted")
        if notes:
            rec["note"] = "; ".join(notes)

    def run_confirm(self, i, rec, by_id, specs):
        steps = self.plan["steps"]
        s = steps[i]
        if not any(by_id[x["id"]]["state"] == "done" for x in separators(steps, i)):
            return rec.update(state="skipped", note="no step between the group's last comparison and this "
                                                    "confirmation finished, so the two would not be separate blocks")
        cands = []
        for m in s["steps"]:
            r = by_id[m]
            fields = r if r["state"] == "done" else None
            cands.append((m, specs[m]["cand"], fields))
        best = choose_best(cands)
        if best is None:
            return rec.update(state="done", verdict="none improved",
                              note="no alternative improved; nothing to confirm")
        sid, lab, _ = best
        part = specs[sid]["part"]
        bad_aa = [f"{x['id']} {by_id[x['id']]['state']} {by_id[x['id']].get('verdict') or ''}".strip()
                  for x in steps[:i] if x["kind"] == "aa" and x["part"] == part
                  and not (by_id[x["id"]]["state"] == "done" and by_id[x["id"]].get("verdict") == "inconclusive")]
        rec["chosen_from"] = sid
        self.run_compare(specs[sid], rec, lab, out_id=s["id"])
        if rec["state"] == "done":
            rec["winner"] = lab if rec["verdict"] == "improved" and not bad_aa else None
            head = f"{lab} confirmed" if rec["winner"] else f"{lab} not confirmed"
            if bad_aa and rec["verdict"] == "improved":
                head = (f"{lab} improved again, but no winner is named: the A/A method check for part "
                        f"{part} did not come out inconclusive ({', '.join(bad_aa)})")
            rec["note"] = head + (f"; {rec['note']}" if rec.get("note") else "")

    def winner(self, by_id, group):
        """(the group's winner or None, the group's confirm_best record)."""
        conf = next(r for r in by_id.values() if r["kind"] == "confirm_best" and r.get("group") == group)
        return conf.get("winner"), conf

    def run_golden(self, rec, lab):
        out = self.L.results / rec["id"]
        open(f"{out}.out", "w").close()
        open(f"{out}.err", "w").close()
        results = []
        for cwd, argv in golden_argvs(self.L, lab):
            results.append(self.sh(argv, f"{out}.out", f"{out}.err", cwd=cwd, env=env_for(self.L, lab), mode="a"))
        err_text = Path(f"{out}.err").read_text(errors="replace")
        out_text = Path(f"{out}.out").read_text(errors="replace")
        golden_ok = results[1] == 0 and "golden check passed against" in err_text
        tests = re.findall(r"^test result: (ok|FAILED)\.", out_text, re.M)
        tests_ok = results[2] == 0 and bool(tests) and all(t == "ok" for t in tests)
        rec.update(state="done", label=lab, verdict="pass" if golden_ok and tests_ok else "fail",
                   note=f"golden {'passed' if golden_ok else 'FAILED'}; "
                        f"tests {'ok' if tests_ok else 'FAILED'} ({tests.count('ok')} test binaries ok)")

    def run_bench(self, s, rec):
        lab = s["label"]
        if not self.built(lab):
            return rec.update(state="failed", note=f"no build of {lab}")
        out = self.L.results / s["id"]
        rec["label"] = lab
        try:
            code = self.measured_sh(rec, bench_argv(self.plan, self.L, s), f"{out}.jsonl", f"{out}.err",
                                    cwd=self.L.src / lab, env=env_for(self.L, lab))
        finally:
            ids = self.status["labels"][lab]
            for f in (Path(f"{out}.jsonl"), Path(f"{out}.err")):  # bench.sh labels its runs with the bundle commit
                if ids.get("bundle_commit") and ids.get("commit") and f.is_file():
                    write_text(f, f.read_text(encoding="utf-8", errors="replace")
                               .replace(ids["bundle_commit"], ids["commit"]))
        summary = read_summary(f"{out}.jsonl")
        if summary is None or code:
            return rec.update(state="failed", note=f"exit {code}{'' if summary else ', no summary line'}; "
                                                   f"see {out.name}.err")
        med = {k: (summary.get(k) or {}).get("median") for k in ("all_p50_ns", "throughput_msgs_per_s")}
        rec.update(state="done", note=f"median all_p50_ns {med['all_p50_ns']}, "
                                      f"throughput_msgs_per_s {med['throughput_msgs_per_s']}")

    def steps(self):
        specs = {s["id"]: s for s in self.plan["steps"]}
        by_id = {r["id"]: r for r in self.status["steps"]}
        for i, (s, rec) in enumerate(zip(self.plan["steps"], self.status["steps"])):
            if self.stopped():
                self.halt("stop time reached")
                return
            k = s["kind"]
            rec.update(state="running", start=now_iso())
            self.save()
            log(f"step {s['id']} ({k})")
            try:
                if k == "aa":
                    self.run_compare(s, rec, self.plan["base"])
                elif k == "compare":
                    self.run_compare(s, rec, s["cand"])
                elif k == "confirm_best":
                    self.run_confirm(i, rec, by_id, specs)
                elif k in ("window", "golden"):
                    lab, conf = self.winner(by_id, s["group"])
                    if lab is None:
                        rec.update(state="skipped", note=f"no confirmed winner for {s['group']} ({conf['id']} "
                                                         f"{conf['state']}: {conf.get('note') or ''})")
                    elif k == "window":
                        self.run_compare(s, rec, lab)
                    else:
                        self.run_golden(rec, lab)
                elif k == "bench":
                    self.run_bench(s, rec)
            except Exception as e:  # recorded; later steps that do not need this one still run
                rec.update(state="failed", note=f"{type(e).__name__}: {e}")
            rec["end"] = now_iso()
            self.save()
            log(f"step {s['id']}: {rec['state']} {rec.get('verdict') or ''} {rec.get('note') or ''}")

    def halt(self, why):
        for rec in self.status["steps"]:
            if rec["state"] in ("pending", "running"):
                rec.update(state="not started", note=why)
        self.status["state"] = f"stopped: {why}"
        log(f"stopped: {why}")

    def run(self):
        self.L.results.mkdir(parents=True, exist_ok=True)
        log(f"plan {json.dumps(self.plan)}")
        try:
            self.write_plan()
            self.save()
            ok = (self.phase("host", self.host) and self.phase("rust", self.rust)
                  and self.phase("clone", self.clone) and self.phase("fetch", self.fetch))
            if not ok:
                self.halt("setup failed")
            else:
                for lab in self.plan["labels"]:
                    if self.stopped():
                        self.halt("stop time reached during builds")
                        break
                    if self.status["labels"][lab]["build"] is None:
                        self.build(lab)
                else:
                    self.steps()
        except Exception as e:  # STATUS.json and SUMMARY.md are written below either way
            self.halt(f"error {type(e).__name__}: {e}")
        finally:
            if self.status["state"] == "running":
                self.status["state"] = "done"
            self.save()
            log(f"STATUS {self.status['state']}")
        return 0  # the output is kept either way; STATUS.json says what ran


# ---------------------------------------------------------------- packaging

def embed(source, plan):
    marker = "PLAN = None" + "  # --write-kernel replaces this line in the kernel's copy"  # split: matches once
    if source.count(marker) != 1:
        sys.exit("kaggle_bench: cannot find the PLAN line to fill in")
    return source.replace(marker, f"PLAN = {plan!r}", 1)


def git(repo, *args, env=None):
    r = subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True, env=env)
    if r.returncode:
        sys.exit(f"kaggle_bench: git {' '.join(args)} failed: {r.stderr.strip()}")
    return r.stdout.strip()


def make_bundle(repo, commits, bundle):
    """Writes a bundle of one history-free commit per branch in commits (that commit's tree, the neutral
    BUNDLE_ENV identity and dates, BUNDLE_MESSAGE) under refs/kaggle-bundle/<branch>, so no name or
    address from the real history leaves the machine. Returns {branch: bundle commit}. The temporary
    refs are deleted afterwards, also when bundling fails."""
    env = {**os.environ, **BUNDLE_ENV}
    made = {}
    try:
        for br, commit in commits.items():
            sha = git(repo, "commit-tree", "--no-gpg-sign", "-m", BUNDLE_MESSAGE,
                      f"{commit}^{{tree}}", env=env)
            git(repo, "update-ref", f"{BUNDLE_REF}{br}", sha)
            made[br] = sha
        git(repo, "bundle", "create", str(Path(bundle).resolve()), *[f"{BUNDLE_REF}{b}" for b in made])
    finally:
        for br in made:
            subprocess.run(["git", "-C", str(repo), "update-ref", "-d", f"{BUNDLE_REF}{br}"],
                           capture_output=True)
    return made


def write_kernel(plan_path, out_dir, owner):
    plan = load_plan(plan_path)
    owner = owner or os.environ.get("KAGGLE_USERNAME")
    if not owner:
        sys.exit("kaggle_bench: no Kaggle owner; pass --owner or set KAGGLE_USERNAME")
    if not re.fullmatch(r"[A-Za-z0-9_-]+", owner):
        sys.exit(f"kaggle_bench: bad Kaggle owner {owner!r}")
    here = Path(__file__).resolve()
    repo = git(here.parent, "rev-parse", "--show-toplevel")
    branches = sorted(set(plan["labels"].values()))
    commits = {br: git(repo, "rev-parse", "--verify", f"refs/heads/{br}^{{commit}}") for br in branches}
    out = Path(out_dir)
    src, kern = out / "src", out / "kernel"
    for d in (src, kern):
        if d.exists() and any(d.iterdir()):
            sys.exit(f"kaggle_bench: {d} is not empty; remove it or pick another DIR")
        d.mkdir(parents=True, exist_ok=True)
    name = slug(plan)
    bundle = src / "matching-engine.bundle"
    bundle_commits = make_bundle(repo, commits, bundle)
    write_json(src / "dataset-metadata.json",
               {"id": f"{owner}/{name}-src", "title": f"{name}-src", "licenses": [{"name": "other"}]})
    code = embed(here.read_text(encoding="utf-8"),
                 {**plan, "commits": commits, "bundle_commits": bundle_commits})
    (kern / f"{name}.py").write_text(code, encoding="utf-8", newline="\n")
    write_json(kern / "kernel-metadata.json", {
        "id": f"{owner}/{name}", "title": name, "code_file": f"{name}.py", "language": "python",
        "kernel_type": "script", "is_private": True, "enable_gpu": False, "enable_tpu": False,
        "enable_internet": True, "dataset_sources": [f"{owner}/{name}-src"],
        "competition_sources": [], "kernel_sources": [], "model_sources": []})
    for br, sha in commits.items():
        print(f"{br} {sha} (bundle commit {bundle_commits[br]})")
    print(f"wrote {bundle} ({bundle.stat().st_size} bytes), {src / 'dataset-metadata.json'}")
    print(f"wrote {kern / (name + '.py')}, {kern / 'kernel-metadata.json'}")
    return 0


# ---------------------------------------------------------------- self-test

def fixture_plan():
    def cmp(i, cand, part="side"):
        return {"id": i, "kind": "compare", "part": part, "cand": cand, "pairs": 10, "stat": "ns_per_op_min"}
    return {"name": "t", "base": "base", "labels": {"base": "b0", "x": "bx", "y": "by"},
            "steps": [{"id": "aa", "kind": "aa", "part": "side", "pairs": 10, "stat": "ns_per_op_min"},
                      cmp("cx", "x"), cmp("cy", "y"),
                      {"id": "bench", "kind": "bench", "label": "base", "runs": 3},
                      {"id": "conf", "kind": "confirm_best", "group": "g", "steps": ["cx", "cy"]},
                      {"id": "win", "kind": "window", "group": "g", "window": "09:30:00-10:00:00",
                       "pairs": 6, "stat": "all_p50_ns"},
                      {"id": "gold", "kind": "golden", "group": "g"}]}


def summary_line(verdict, effect):
    return json.dumps({"summary": True, "verdict": verdict, "flagged_runs": 0,
                       "ns_per_op_min": {"effect_pct": effect, "low_pct": effect - 1, "high_pct": effect + 1,
                                         "effect_ln_x100": effect}})


class SelfTest(unittest.TestCase):
    def errs(self, edit):
        plan = fixture_plan()
        edit(plan)
        return " | ".join(validate(plan))

    def test_valid_plan(self):
        self.assertEqual(validate(fixture_plan()), [])

    def test_shipped_plans_are_valid(self):
        for p in sorted((Path(__file__).resolve().parent / "kaggle-plans").glob("*.json")):
            self.assertEqual(validate(json.loads(p.read_text(encoding="utf-8"))), [], p.name)

    def test_refusals(self):
        cases = [
            (lambda p: p["steps"][0].update(kind="nope"), "unknown kind"),
            (lambda p: p["steps"][1].update(cand="z"), "no branch"),
            (lambda p: p.update(base="z"), "base label"),
            (lambda p: p["steps"][4].update(steps=["cx", "gone"]), "not an earlier step"),
            (lambda p: p["steps"][4].update(steps=["cx", "aa"]), "not a compare step"),
            (lambda p: p["steps"][1].update(stat="all_p50_ns"), "does not report"),
            (lambda p: p["steps"][5].update(stat="ns_per_op_min"), "does not report"),
            (lambda p: p["steps"][1].update(pairs=7), "even number"),
            (lambda p: p["steps"][1].update(pairs=4), "even number"),
            (lambda p: p["steps"][2].update(pairs=12), "share their pairs"),
            (lambda p: p["steps"][5].update(group="h"), "no earlier confirm_best"),
            (lambda p: p["steps"].insert(0, p["steps"].pop(4)), "not an earlier step"),
            (lambda p: p["steps"][2].update(id="cx"), "used twice"),
            (lambda p: p["steps"][1].update(cand="base"), "kind aa"),
            (lambda p: p["steps"][5].update(window="9:30-10:00"), "HH:MM:SS"),
            (lambda p: p["steps"].pop(3), "no other measurement"),  # nothing between cy and conf
            (lambda p: p["steps"][2].update(cand="x"), "compared twice"),
            (lambda p: p.update(stop_after_min=HARD_STOP_MIN), "below the hard limit"),
        ]
        for edit, want in cases:
            self.assertIn(want, self.errs(edit))

    def run_steps(self, canned, built=("base", "x", "y"), stat=True):
        """Kernel.steps() on the fixture plan with the shell-out replaced. canned maps a step id (the
        output file's name) to (verdict, effect, exit code); a missing id prints nothing and exits 1.
        With stat, PROC_STAT is a file whose CPU 3 line gains 5 steal and 97 total jiffies across each
        command; without, it is missing. Returns ({step id: record}, the step ids that ran a command)."""
        def proc_stat(n):  # user nice system idle iowait irq softirq steal guest guest_nice
            return f"cpu  1 2 3 4 5 6 7 8 9 10\ncpu3 {10 * n} 0 {2 * n} {80 * n} 0 0 0 {5 * n} 0 0\n"
        with tempfile.TemporaryDirectory() as d, \
                unittest.mock.patch(f"{__name__}.PROC_STAT", Path(d) / "stat"):
            L = layout(d)
            L.results.mkdir(parents=True)
            k = Kernel({**fixture_plan(), "commits": {}, "bundle_commits": {}}, L)
            for lab in built:
                k.status["labels"][lab]["build"] = "ok"
            ran = []
            if stat:
                PROC_STAT.write_text(proc_stat(1))

            def sh(argv, out, err, cwd=None, env=None, mode="w"):
                sid = Path(out).stem
                ran.append(sid)
                if stat:
                    PROC_STAT.write_text(proc_stat(len(ran) + 1))
                with open(out, mode) as fo, open(err, mode) as fe:
                    if sid == "gold":
                        fe.write("golden check passed against x\n")
                        fo.write("test result: ok. 3 passed\n")
                        return 0
                    if sid not in canned:
                        return 1
                    verdict, effect, code = canned[sid]
                    fo.write(summary_line(verdict, effect) + "\n")
                    return code
            k.sh = sh
            with contextlib.redirect_stdout(io.StringIO()):
                k.steps()
            self.summary = (L.results / "SUMMARY.md").read_text()
            self.assertIn("| conf | confirm_best |", self.summary)
            return {r["id"]: r for r in k.status["steps"]}, ran

    def test_step_wiring(self):
        ok = {"aa": ("inconclusive", 0.1, 0), "bench": ("", 0.0, 0)}
        better = {"cx": ("improved", -3.0, 0), "cy": ("improved", -5.0, 0), "conf": ("improved", -4.0, 0),
                  "win": ("improved", -1.0, 0)}

        r, _ = self.run_steps({**ok, **better})  # the best of two improved is confirmed
        self.assertEqual((r["conf"]["chosen_from"], r["conf"]["winner"]), ("cy", "y"))
        self.assertEqual((r["win"]["state"], r["win"]["label"]), ("done", "y"))
        self.assertEqual((r["gold"]["state"], r["gold"]["verdict"]), ("done", "pass"))
        for x in ("aa", "cx", "cy", "bench", "conf", "win"):  # each command's scheduling readings
            self.assertEqual((r[x]["steal_jiffies"], r[x]["total_jiffies"]), (5, 97), x)
            self.assertGreaterEqual(r[x]["wall_s"], 0)
        self.assertNotIn("steal_jiffies", r["gold"])
        self.assertIn("| -5.00 .. -3.00 | 5.2 | y |", self.summary)  # the conf row: 5 / 97

        r, _ = self.run_steps({**ok, **better}, stat=False)  # no /proc/stat: unavailable, never 0
        self.assertEqual((r["aa"]["steal_jiffies"], r["aa"]["total_jiffies"]), ("unavailable",) * 2)
        self.assertIsInstance(r["aa"]["wall_s"], float)
        self.assertEqual(r["conf"]["winner"], "y")  # the readings change nothing
        self.assertIn("| -5.00 .. -3.00 | n/a | y |", self.summary)

        r, ran = self.run_steps({**ok, "cx": ("inconclusive", -1.0, 0), "cy": ("regressed", 2.0, 0)})
        self.assertEqual(r["conf"]["verdict"], "none improved")  # none improved: no confirmation runs
        self.assertNotIn("conf", ran)
        for x in ("win", "gold"):
            self.assertEqual(r[x]["state"], "skipped")
            self.assertIn("no confirmed winner for g", r[x]["note"])

        for cx in ({}, {"cx": ("improved", -9.0, 1)}):  # no summary line; a summary but exit 1
            canned = {**ok, **better}
            del canned["cx"]
            r, _ = self.run_steps({**canned, **cx})
            self.assertEqual(r["cx"]["state"], "failed")
            self.assertEqual(r["conf"]["chosen_from"], "cy")

        r, _ = self.run_steps({**ok, **better, "conf": ("inconclusive", -1.0, 0)})
        self.assertEqual((r["conf"]["state"], r["conf"]["winner"]), ("done", None))  # not confirmed
        self.assertIn("y not confirmed", r["conf"]["note"])
        self.assertEqual(r["win"]["state"], "skipped")

        for aa in ({"aa": ("regressed", 3.0, 0)}, {"aa": ("inconclusive", 0.1, 1)}):  # A/A check failed
            r, _ = self.run_steps({**ok, **better, **aa})
            self.assertEqual((r["conf"]["verdict"], r["conf"]["winner"]), ("improved", None))
            self.assertIn("A/A method check", r["conf"]["note"])
            self.assertEqual(r["win"]["state"], "skipped")
            self.assertIn("A/A method check", r["win"]["note"])
        r, _ = self.run_steps({**ok, **better, "aa": ("regressed", 3.0, 0)})
        self.assertIn("A/A gave regressed", r["aa"]["note"])

        r, ran = self.run_steps({"aa": ok["aa"], **better})  # the separating bench step failed
        self.assertEqual(r["bench"]["state"], "failed")
        self.assertEqual(r["conf"]["state"], "skipped")
        self.assertNotIn("conf", ran)
        self.assertIn("separate blocks", r["gold"]["note"])

        r, _ = self.run_steps({**ok, **better}, built=("base", "x"))  # y did not build
        self.assertEqual((r["cy"]["state"], r["cy"]["note"]), ("failed", "no build of y"))
        self.assertEqual(r["conf"]["chosen_from"], "cx")

    def test_proc_stat_without_the_cpu(self):
        with tempfile.TemporaryDirectory() as d, \
                unittest.mock.patch(f"{__name__}.PROC_STAT", Path(d) / "stat"):
            PROC_STAT.write_text("cpu  1 2 3 4 5 6 7 8 9 10\ncpu0 1 2 3 4 5 6 7 8 9 10\ncpu3 1 2 3 4\n")
            self.assertEqual(cpu_jiffies(0), (8, 55))
            self.assertIsNone(cpu_jiffies(3))  # no steal field
            self.assertIsNone(cpu_jiffies(2))  # no line
            self.assertEqual(readings(None, (8, 55), 1.0)["steal_jiffies"], "unavailable")

    def test_session_record(self):
        with tempfile.TemporaryDirectory() as d:
            L = layout(d)
            L.results.mkdir(parents=True)
            plan = {**fixture_plan(), "commits": {"b0": "0" * 40}, "bundle_commits": {"b0": "1" * 40}}
            k = Kernel(plan, L)
            k.write_plan()
            self.assertEqual(json.loads((L.results / "plan.json").read_text()),
                             {**plan, "cpu": DEFAULT_CPU, "stop_after_min": DEFAULT_STOP_MIN})
            session = k.status["session"]  # the plan name and the UTC start, nothing else
            self.assertRegex(session, r"\At-\d{8}T\d{6}Z\Z")
            self.assertEqual(session, "t-" + k.status["started"].replace("-", "").replace(":", ""))
            self.assertEqual(k.status["runner_sha256"], hashlib.sha256(Path(__file__).read_bytes()).hexdigest())
            k.save()
            text = (L.results / "SUMMARY.md").read_text()
            self.assertIn(f"# Plan t, session {session}: running", text)
            self.assertIn(f"runner SHA-256 {k.status['runner_sha256']}", text)
            self.assertIn(f"| step (session {session}) | kind |", text)

            def sh(argv, out, err, cwd=None, env=None, mode="w"):  # a build that writes only settings.txt
                (L.bins / "x").mkdir(parents=True)
                for f in ("bench", "settings.txt"):
                    (L.bins / "x" / f).write_text(f)
                return 0
            k.sh = sh
            k.status["labels"]["x"]["commit"] = "2" * 40
            with contextlib.redirect_stdout(io.StringIO()):
                k.build("x")
            rec = k.status["labels"]["x"]
            self.assertEqual(rec["build"], "ok")
            self.assertEqual(rec["build_files"], {"settings.txt": "build-x-settings.txt", "build.txt": "missing"})
            self.assertEqual((L.results / "build-x-settings.txt").read_text(), "settings.txt")

    def test_hard_limit(self):
        with tempfile.TemporaryDirectory() as d:
            L = layout(d)
            L.results.mkdir(parents=True)
            k = Kernel({**fixture_plan(), "commits": {}, "bundle_commits": {}}, L)
            for lab in k.status["labels"].values():
                lab["build"] = "ok"
            k.deadline = time.monotonic() - 1
            with contextlib.redirect_stdout(io.StringIO()):
                k.steps()
            self.assertEqual(k.status["steps"][0]["state"], "failed")
            self.assertIn("hard limit of 700 min", k.status["steps"][0]["note"])

    @unittest.skipUnless(hasattr(os, "killpg"), "process groups are POSIX only")
    def test_hard_limit_kills_the_process_group(self):
        with tempfile.TemporaryDirectory() as d:
            L = layout(d)
            L.results.mkdir(parents=True)
            k = Kernel({**fixture_plan(), "commits": {}, "bundle_commits": {}}, L)
            k.deadline = time.monotonic() + 2
            out = Path(d) / "x.out"
            child = ("import subprocess, sys, time; p = subprocess.Popen([sys.executable, '-c', "
                     "'import time; time.sleep(60)']); print(p.pid, flush=True); time.sleep(60)")
            with self.assertRaises(TimeoutError):
                k.sh([sys.executable, "-c", child], out, Path(d) / "x.err")
            grandchild = int(out.read_text().split()[0])
            for _ in range(50):
                try:
                    os.kill(grandchild, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.1)
            else:
                self.fail(f"grandchild {grandchild} still runs")

    def test_bundle_carries_no_identity(self):
        name, mail = "Someone Private", "private@example.invalid"
        with tempfile.TemporaryDirectory() as d:
            repo, bundle = Path(d) / "repo", Path(d) / "t.bundle"
            env = {**os.environ, "GIT_AUTHOR_NAME": name, "GIT_AUTHOR_EMAIL": mail,
                   "GIT_COMMITTER_NAME": name, "GIT_COMMITTER_EMAIL": mail}

            def g(*args, cwd=repo):
                return subprocess.run(["git", "-c", "commit.gpgsign=false", *args], cwd=cwd, env=env,
                                      check=True, capture_output=True, text=True).stdout.strip()
            repo.mkdir()
            g("init", "-q")
            g("symbolic-ref", "HEAD", "refs/heads/master")  # git's usual initial branch names: the
            (repo / "f.txt").write_text("one\n")
            g("add", "f.txt")
            g("commit", "-q", "-m", f"first, by {name}")
            g("checkout", "-q", "-b", "main")  # kernel's checkout must not collide with either
            (repo / "f.txt").write_text("two\n")
            g("commit", "-q", "-am", f"second <{mail}>")
            commits = {br: g("rev-parse", br) for br in ("master", "main")}
            made = make_bundle(repo, commits, bundle)
            self.assertEqual(made, make_bundle(repo, commits, bundle))  # reproducible
            self.assertEqual(g("for-each-ref", BUNDLE_REF), "")  # temporary refs gone
            data = bundle.read_bytes()
            for x in (name, mail):
                self.assertNotIn(x.encode(), data)
            for br, sha in made.items():
                dest = Path(d) / f"clone-{br}"
                for argv in checkout_argvs(bundle, br, dest):
                    subprocess.run(argv, check=True, capture_output=True)
                self.assertEqual(g("rev-parse", "HEAD", cwd=dest), sha)
                self.assertEqual(g("rev-parse", "HEAD^{tree}", cwd=dest), g("rev-parse", f"{br}^{{tree}}"))
                history = g("log", "--all", "--format=%an %ae %cn %ce %B", cwd=dest)
                for x in (name, mail):
                    self.assertNotIn(x, history)
                self.assertEqual(history, f"bench bench@localhost bench bench@localhost {BUNDLE_MESSAGE}")

    def test_choose_best(self):
        def f(v, e):
            return comparison_fields(json.loads(summary_line(v, e)), "ns_per_op_min")
        cands = [("a", "A", f("inconclusive", -9.0)), ("b", "B", f("improved", -2.0)),
                 ("c", "C", f("improved", -5.0)), ("d", "D", f("regressed", 4.0)), ("e", "E", None)]
        self.assertEqual(choose_best(cands)[1], "C")
        tie = [("a", "A", f("improved", -3.0)), ("b", "B", f("improved", -3.0))]
        self.assertEqual(choose_best(tie)[1], "A")
        self.assertIsNone(choose_best([("a", "A", f("inconclusive", -1.0)), ("b", "B", f("regressed", 2.0)),
                                       ("c", "C", f("inconclusive (too few pairs)", -4.0)), ("d", "D", None)]))

    def test_read_summary_and_status(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "x.jsonl"
            p.write_text('{"meta":true}\n{"pair":1}\nnot json\n' + summary_line("improved", -4.0) + "\n")
            self.assertEqual(read_summary(p)["verdict"], "improved")
            self.assertIsNone(read_summary(Path(d) / "missing.jsonl"))
            L = layout(d)
            L.results.mkdir(parents=True)
            k = Kernel({**fixture_plan(), "commits": {}}, L)
            k.status["steps"][0].update(state="done", verdict="inconclusive", effect_pct=0.1,
                                        low_pct=-1.0, high_pct=1.2)
            k.save()
            k.halt("stop time reached")
            k.save()
            write_summary(L, k.status)
            st = json.loads((L.results / "STATUS.json").read_text())
            self.assertEqual([s["state"] for s in st["steps"]][:2], ["done", "not started"])
            self.assertTrue(st["state"].startswith("stopped"))
            self.assertIn("| aa | aa | done | inconclusive | +0.10 | -1.00 .. +1.20 |",
                          (L.results / "SUMMARY.md").read_text())

    def test_stop_time(self):
        self.assertFalse(past_stop(629 * 60, 630))
        self.assertTrue(past_stop(630 * 60, 630))
        self.assertTrue(past_stop(11 * 3600, DEFAULT_STOP_MIN))

    def test_embed(self):
        plan = {**fixture_plan(), "commits": {"b0": "0" * 40}}
        code = embed(Path(__file__).read_text(encoding="utf-8"), plan)
        line = next(x for x in code.splitlines() if x.startswith("PLAN = "))
        self.assertEqual(ast.literal_eval(line[len("PLAN = "):]), plan)
        compile(code, "kernel.py", "exec")


def main():
    ap = argparse.ArgumentParser(prog="kaggle_bench.py", description="Run benchmark comparisons on Kaggle.")
    g = ap.add_mutually_exclusive_group()
    g.add_argument("--write-kernel", nargs=2, metavar=("PLAN", "DIR"))
    g.add_argument("--dry-run", metavar="PLAN")
    g.add_argument("--self-test", action="store_true")
    ap.add_argument("--owner", help="Kaggle user that owns the dataset and kernel (default $KAGGLE_USERNAME)")
    a = ap.parse_args()
    if a.self_test:
        sys.dont_write_bytecode = True
        r = unittest.main(argv=["kaggle_bench"], module=__name__, exit=False, verbosity=1).result
        return 0 if r.wasSuccessful() else 1
    if a.dry_run:
        return dry_run(a.dry_run)
    if a.write_kernel:
        return write_kernel(*a.write_kernel, a.owner)
    root = Path(os.environ.get("KAGGLE_BENCH_ROOT", "/kaggle"))
    if PLAN is not None and (root / "working").is_dir():
        errs = validate(PLAN)
        if errs:
            sys.exit("kaggle_bench: the embedded plan is not valid:\n  " + "\n  ".join(errs))
        return Kernel(PLAN, layout(root)).run()
    ap.print_help()
    return 2


if __name__ == "__main__":
    sys.exit(main())
