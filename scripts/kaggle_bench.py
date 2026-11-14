#!/usr/bin/env python3
# Runs a plan of benchmark comparisons on a Kaggle CPU session. One file is both the local
# packager and the kernel: --write-kernel writes the dataset and kernel folders that the kaggle
# CLI uploads, and the kernel's copy (this file with the plan embedded) runs the plan on Kaggle.
#
#   python scripts/kaggle_bench.py --write-kernel PLAN DIR [--owner NAME]
#   python scripts/kaggle_bench.py --dry-run PLAN
#   python scripts/kaggle_bench.py --self-test
#
# --write-kernel writes DIR/src (a git bundle of exactly the branches the plan names, with
# dataset-metadata.json) and DIR/kernel (the kernel's code with the plan and each branch's commit
# embedded, with kernel-metadata.json: a private script kernel, CPU only, internet on, DIR/src's
# dataset attached). The Kaggle owner is --owner or $KAGGLE_USERNAME.
#
# On Kaggle (run with no arguments under /kaggle) the kernel installs Rust with rustup into
# /kaggle/tmp, clones each label's branch from the bundle and checks it is at the embedded commit,
# fetches the ITCH day with the base checkout's scripts/fetch-itch.sh into /kaggle/tmp/itch, builds
# every label with that label's scripts/bench-build.sh, and only then runs the plan's steps one at
# a time with the base checkout's scripts/compare.sh (bench.sh for `bench` steps). Each step's
# stdout goes to /kaggle/working/results/<step>.jsonl and stderr to <step>.err; STATUS.json is
# rewritten after every step and SUMMARY.md at the end. README.md, "Running the comparisons on
# Kaggle", describes the plan format and the step kinds.
#
# KAGGLE_BENCH_ROOT (default /kaggle) moves the kernel's root, for trying the kernel elsewhere.
# Standard library only.
import argparse
import ast
import datetime as dt
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path, PurePath, PurePosixPath
from types import SimpleNamespace

PLAN = None  # --write-kernel replaces this line in the kernel's copy

KINDS = ("aa", "compare", "confirm_best", "window", "golden", "bench")
PARTS = ("side", "level", "orders", "book", "parse")
MICRO_STATS = ("ns_per_op_median", "ns_per_op_min")
WINDOW_STATS = tuple(f"{s}_{q}_ns" for s in ("all", "book") for q in ("mean", "p50", "p99", "p999"))
DEFAULT_CPU = 3
DEFAULT_STOP_MIN = 630  # 10 h 30 min; Kaggle ends a session at 12 h
MIN_FREE_GB = 40  # ITCH (12 GB with the .gz), a release target per label, test builds
ITCH_NAME = "12302019.NASDAQ_ITCH50"
GOLDEN = "harness/golden/12302019.txt"
NAME_RE = re.compile(r"[a-z0-9][a-z0-9-]{0,40}")
LABEL_RE = re.compile(r"[A-Za-z0-9_-][A-Za-z0-9._-]*")
WINDOW_RE = re.compile(r"\d\d:\d\d:\d\d-\d\d:\d\d:\d\d")

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
    if not isinstance(stop, int) or isinstance(stop, bool) or stop <= 0:
        err(f"stop_after_min must be a positive number of minutes, not {stop!r}")
    steps = plan.get("steps")
    if not isinstance(steps, list):
        err("steps must be a list")
        steps = []
    seen, groups = {}, set()
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


def bench_argv(plan, L, s):
    return [str(L.src / s["label"] / "scripts" / "bench.sh"), "-n", str(s["runs"]),
            "-c", str(plan.get("cpu", DEFAULT_CPU))]


def setup_lines(plan, L):
    rustup_sh = L.tmp / "rustup-init.sh"
    lines = [f"curl --proto =https --tlsv1.2 -sSf -o {rustup_sh} https://sh.rustup.rs",
             f"sh {rustup_sh} -y --no-modify-path --profile minimal --default-toolchain stable",
             "rustc -vV"]
    for lab, br in plan["labels"].items():
        lines.append(f"git clone -q --branch {br} <bundle> {L.src / lab}")
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
                  f"is compared again; when that also says improved it is the winner of {s['group']}. None improved: nothing runs")
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


def write_json(path, obj):
    path = Path(path)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(obj, indent=1) + "\n", encoding="utf-8")
    os.replace(tmp, path)


def fmt(x):
    return "" if x is None else f"{x:+.2f}"


def write_summary(L, status):
    rows = ["| step | kind | state | verdict | effect % | interval % | label | note |",
            "|---|---|---|---|---|---|---|---|"]
    for s in status["steps"]:
        interval = f"{fmt(s.get('low_pct'))} .. {fmt(s.get('high_pct'))}" if s.get("low_pct") is not None else ""
        rows.append(f"| {s['id']} | {s['kind']} | {s['state']} | {s.get('verdict') or ''} | "
                    f"{fmt(s.get('effect_pct'))} | {interval} | {s.get('label') or ''} | {s.get('note') or ''} |")
    labels = [f"- {lab}: branch {v['branch']}, commit {v.get('commit') or '?'}, build {v.get('build') or 'not run'}"
              for lab, v in status["labels"].items()]
    setup = [f"- {k}: {v['state']}{' (' + v['note'] + ')' if v.get('note') else ''}"
             for k, v in status["setup"].items()]
    text = "\n".join([
        f"# Plan {status['plan']}: {status['state']}", "",
        "Timings from a shared Kaggle cloud VM under Linux (see host.txt). The comparisons are "
        "relative and valid on that machine; absolute numbers belong to that machine only.", "",
        f"Started {status['started']}, last update {status['updated']}, cpu {status['cpu']}, "
        f"stop time {status['stop_at']}.", "", "Setup:", *setup, "", "Labels:", *labels, "",
        *rows, ""])
    (L.results / "SUMMARY.md").write_text(text, encoding="utf-8")


# ---------------------------------------------------------------- the kernel

class Kernel:
    def __init__(self, plan, L):
        self.plan, self.L = plan, L
        self.cpu = plan.get("cpu", DEFAULT_CPU)
        self.stop_min = plan.get("stop_after_min", DEFAULT_STOP_MIN)
        start = dt.datetime.now(dt.timezone.utc)
        self.status = {
            "plan": plan["name"], "state": "running", "started": now_iso(), "updated": now_iso(),
            "stop_at": (start + dt.timedelta(minutes=self.stop_min)).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "cpu": self.cpu, "rustc": None, "setup": {},
            "labels": {lab: {"branch": br, "commit": None, "build": None}
                       for lab, br in plan["labels"].items()},
            "steps": [{"id": s["id"], "kind": s["kind"], "state": "pending",
                       **({"group": s["group"]} if "group" in s else {})} for s in plan["steps"]]}

    def save(self):
        self.status["updated"] = now_iso()
        write_json(self.L.results / "STATUS.json", self.status)

    def stopped(self):
        return past_stop(time.monotonic() - T0, self.stop_min)

    def sh(self, argv, out, err, cwd=None, env=None, mode="w"):
        log("$ " + shlex.join(str(a) for a in argv))
        with open(out, mode) as fo, open(err, mode) as fe:
            return subprocess.run([str(a) for a in argv], stdout=fo, stderr=fe, cwd=cwd,
                                  env=env or env_for(self.L)).returncode

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
        for argv in (["nproc"], ["lscpu"], ["sh", "-c", "head -n 5 /proc/meminfo"], ["uname", "-r"],
                     ["df", "-h", str(self.L.tmp), str(self.L.work)]):
            r = subprocess.run(argv, capture_output=True, text=True)
            lines += [f"$ {shlex.join(argv)}", r.stdout.rstrip(), r.stderr.rstrip()]
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
            want = self.plan["commits"][br]
            if self.sh(["git", "clone", "-q", "--branch", br, bundles[0], dest], out, err, mode="a"):
                failed.append(f"{lab}: clone failed")
                continue
            head = subprocess.run(["git", "-C", dest, "rev-parse", "HEAD"], capture_output=True, text=True)
            got = head.stdout.strip()
            if got != want:
                failed.append(f"{lab}: at {got}, the plan names {want}")
                continue
            self.status["labels"][lab]["commit"] = got
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
        code = self.sh([self.L.src / lab / "scripts" / "bench-build.sh", lab], log_, log_.with_suffix(".err"),
                       cwd=self.L.src / lab, env=env_for(self.L, lab))
        ok = code == 0 and (self.L.bins / lab / "bench").is_file()
        rec["build"] = "ok" if ok else f"failed (exit {code}; see build-{lab}.err)"
        self.save()
        log(f"build {lab}: {rec['build']}")

    def built(self, lab):
        return self.status["labels"][lab]["build"] == "ok"

    # steps

    def run_compare(self, s, rec, cand, out_id=None):
        missing = [x for x in (self.plan["base"], cand) if not self.built(x)]
        if missing:
            return rec.update(state="skipped", note=f"no build of {', '.join(missing)}")
        out = self.L.results / (out_id or s["id"])
        rec["label"] = cand
        code = self.sh(compare_argv(self.plan, self.L, s, cand), f"{out}.jsonl", f"{out}.err",
                       cwd=self.L.src / self.plan["base"])
        summary = read_summary(f"{out}.jsonl")
        if summary is None:
            return rec.update(state="failed", note=f"exit {code}, no summary line; see {out.name}.err")
        rec.update(state="done", **comparison_fields(summary, s["stat"]))
        if summary.get("flagged_runs"):
            rec["note"] = f"{summary['flagged_runs']} flagged run(s)"
        if s["kind"] == "aa" and rec["verdict"] != "inconclusive":
            rec["note"] = (f"A/A gave {rec['verdict']}: the method check failed for part {s['part']}, "
                           f"so its comparisons cannot be trusted")

    def run_confirm(self, s, rec, by_id, specs):
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
        rec["chosen_from"] = sid
        self.run_compare(specs[sid], rec, lab, out_id=s["id"])
        if rec["state"] == "done":
            rec["winner"] = lab if rec["verdict"] == "improved" else None
            rec["note"] = (f"{lab} confirmed" if rec["winner"] else f"{lab} not confirmed") + \
                (f"; {rec['note']}" if rec.get("note") else "")

    def winner(self, by_id, group):
        conf = next(r for r in by_id.values() if r["kind"] == "confirm_best" and r.get("group") == group)
        return conf.get("winner")

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
            return rec.update(state="skipped", note=f"no build of {lab}")
        out = self.L.results / s["id"]
        rec["label"] = lab
        code = self.sh(bench_argv(self.plan, self.L, s), f"{out}.jsonl", f"{out}.err",
                       cwd=self.L.src / lab, env=env_for(self.L, lab))
        summary = read_summary(f"{out}.jsonl")
        if summary is None:
            return rec.update(state="failed", note=f"exit {code}, no summary line; see {out.name}.err")
        med = {k: (summary.get(k) or {}).get("median") for k in ("all_p50_ns", "throughput_msgs_per_s")}
        rec.update(state="done", note=f"median all_p50_ns {med['all_p50_ns']}, "
                                      f"throughput_msgs_per_s {med['throughput_msgs_per_s']}")

    def steps(self):
        specs = {s["id"]: s for s in self.plan["steps"]}
        by_id = {r["id"]: r for r in self.status["steps"]}
        for s, rec in zip(self.plan["steps"], self.status["steps"]):
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
                    self.run_confirm(s, rec, by_id, specs)
                elif k in ("window", "golden"):
                    lab = self.winner(by_id, s["group"])
                    if lab is None:
                        rec.update(state="skipped", note=f"no confirmed winner for {s['group']}")
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
        if self.status["state"] == "running":
            self.status["state"] = "done"
        self.save()
        write_summary(self.L, self.status)
        log(f"STATUS {self.status['state']}")
        return 0  # the output is kept either way; STATUS.json says what ran


# ---------------------------------------------------------------- packaging

def embed(source, plan):
    marker = "PLAN = None" + "  # --write-kernel replaces this line in the kernel's copy"  # split: matches once
    if source.count(marker) != 1:
        sys.exit("kaggle_bench: cannot find the PLAN line to fill in")
    return source.replace(marker, f"PLAN = {plan!r}", 1)


def git(repo, *args):
    r = subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True)
    if r.returncode:
        sys.exit(f"kaggle_bench: git {' '.join(args)} failed: {r.stderr.strip()}")
    return r.stdout.strip()


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
    git(repo, "bundle", "create", str(bundle.resolve()), *[f"refs/heads/{b}" for b in branches])
    write_json(src / "dataset-metadata.json",
               {"id": f"{owner}/{name}-src", "title": f"{name}-src", "licenses": [{"name": "other"}]})
    code = embed(here.read_text(encoding="utf-8"), {**plan, "commits": commits})
    (kern / f"{name}.py").write_text(code, encoding="utf-8", newline="\n")
    write_json(kern / "kernel-metadata.json", {
        "id": f"{owner}/{name}", "title": name, "code_file": f"{name}.py", "language": "python",
        "kernel_type": "script", "is_private": True, "enable_gpu": False, "enable_tpu": False,
        "enable_internet": True, "dataset_sources": [f"{owner}/{name}-src"],
        "competition_sources": [], "kernel_sources": [], "model_sources": []})
    for br, sha in commits.items():
        print(f"{br} {sha}")
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
                      {"id": "conf", "kind": "confirm_best", "group": "g", "steps": ["cx", "cy"]},
                      {"id": "win", "kind": "window", "group": "g", "window": "09:30:00-10:00:00",
                       "pairs": 6, "stat": "all_p50_ns"},
                      {"id": "gold", "kind": "golden", "group": "g"},
                      {"id": "bench", "kind": "bench", "label": "base", "runs": 3}]}


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
            (lambda p: p["steps"][3].update(steps=["cx", "gone"]), "not an earlier step"),
            (lambda p: p["steps"][3].update(steps=["cx", "aa"]), "not a compare step"),
            (lambda p: p["steps"][1].update(stat="all_p50_ns"), "does not report"),
            (lambda p: p["steps"][4].update(stat="ns_per_op_min"), "does not report"),
            (lambda p: p["steps"][1].update(pairs=7), "even number"),
            (lambda p: p["steps"][1].update(pairs=4), "even number"),
            (lambda p: p["steps"][2].update(pairs=12), "share their pairs"),
            (lambda p: p["steps"][4].update(group="h"), "no earlier confirm_best"),
            (lambda p: p["steps"].insert(0, p["steps"].pop(3)), "not an earlier step"),
            (lambda p: p["steps"][2].update(id="cx"), "used twice"),
            (lambda p: p["steps"][1].update(cand="base"), "kind aa"),
            (lambda p: p["steps"][4].update(window="9:30-10:00"), "HH:MM:SS"),
        ]
        for edit, want in cases:
            self.assertIn(want, self.errs(edit))

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
