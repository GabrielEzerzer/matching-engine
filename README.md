# Matching engine

A price-time priority order-matching engine in Rust, built for correctness first and then optimised down to the nanosecond. A slow reference book checks it through property tests, and a full Nasdaq ITCH 5.0 day replayed through it measures latency per message. Built out of interest.

## What exists

Stage 1 is a correct, unoptimised baseline: a library crate with a single-instrument `OrderBook`.

- Commands: limit and market orders, cancel, reduce (keeps time priority) and replace (new id, price and quantity; loses time priority and may trade). Invalid commands are rejected with a reason and leave the book unchanged.
- Each command appends its events (accepted, trades, fills, cancels, reductions, replacements, rejections) to a `Vec<Event>` the caller owns, in a deterministic order documented on `Event`. A trade always prints at the resting order's price.
- Queries: best bid and ask, depth per level, a full snapshot of every resting order in priority order, one resting order's side, price and quantity (`order`), and the order with time priority at the best price of a side (`front_order`).
- Prices are `u32` ticks of $0.0001 and quantities are `u32` shares, matching ITCH 5.0. Level totals are `u64`.
- There is no self-trade prevention: orders carry no owner.

The book is split into modules that later stages measure and replace one at a time: `side` (price-level lookup, a `BTreeMap`), `level` (the FIFO queue at one price, a `VecDeque`), `orders` (the id map, a `HashMap`) and `book` (matching).

`harness/reference.rs` is a deliberately slow reference book that keeps every order in one flat list and scans it for each match. Only the tests and the differential replay compile it.

Stage 2 is the rig that makes optimisation rounds trustworthy: an ITCH 5.0 parser, a replay driver, a golden hash of the full day's output, differential checks against the reference book at depth, and a latency benchmark.

- `src/itch.rs` parses ITCH 5.0 without allocating. It fully decodes the order messages (A, F, E, C, X, D, U) and the system event (S), stock directory (R) and stock trading action (H) messages. Every other 5.0 message type is recognised and length-checked. An unknown type, a wrong length or a cut-off frame is an error.
- `src/replay.rs` drives one `OrderBook` per stock locate from the messages (see "How ITCH maps to book commands").
- `examples/replay.rs` replays a whole day, prints the summary counts and the golden hash, and records or checks them against `harness/golden/12302019.txt`. The hash encoding is documented in `harness/golden.rs`.
- `examples/diff_slice.rs` replays busy symbols through the real book and the reference book and compares every event.
- `examples/bench.rs` and `scripts/bench.sh` measure latency per message and throughput.

## Running the tests

```sh
cargo test --all-targets
```

- `tests/book.rs` holds the unit tests for the edge cases.
- `tests/prop.rs` sends random command streams to both books and requires identical events and identical snapshots after every command, along with the book's internal invariants. It runs 2,000 streams by default; set `PROPTEST_CASES` to run more, for example `PROPTEST_CASES=50000 cargo test --release --test prop`. It also checks that the generators reach the cases they are meant to cover.
- `tests/deep.rs` is a deterministic deep-book differential test. It builds 300 price levels per side with queues of up to 252 orders, then cancels, reduces, replaces, sweeps, drains and repopulates, comparing events, snapshots and invariants with the reference after each of its 8,684 commands.
- `tests/replay.rs` covers the ITCH-to-command mapping and the counters; `tests/golden.rs` pins the hash encoding with digests computed independently from the documented byte layout; the parser's tests are in `src/itch.rs`.

CI runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test --all-targets` on every push to a branch other than `main`. CI never sees the ITCH file. The golden check, the differential slice and the benchmark are local runs under Linux.

## The ITCH data

The replay uses the public Nasdaq TotalView-ITCH 5.0 sample for 30 December 2019 (3,524,013,057 bytes compressed, 8,251,407,909 bytes and 268,744,780 messages decompressed). Under Linux or WSL2:

```sh
scripts/fetch-itch.sh            # into $ITCH_DIR, default /data/itch
```

It downloads with resume, checks the pinned size and SHA-256 of the `.gz`, decompresses it and skips work already done. `VERIFY=1` also hashes an existing decompressed file. The data never goes into git.

All replay, golden, differential and benchmark runs below are Linux runs (WSL2 on Windows), from the repository root, in release mode. On WSL2 with the checkout on a Windows drive, keep the build output on the Linux filesystem, for example `export CARGO_TARGET_DIR=/root/cargo-target/matching-engine`.

## How ITCH maps to book commands

| ITCH | Command |
|---|---|
| A, F (add order) | `Limit` with the ITCH reference number as the id |
| D (order delete) | `Cancel` |
| X (order cancel) | `Reduce` by the cancelled shares |
| U (order replace) | `Replace` with the new reference, price and shares |
| E, C (order executed) | an aggressor `Limit`, or `Reduce` as a fallback |

An execution names the resting order it executed. When that order is at the front of the best level of its side, the driver replays the execution as the incoming order it really was: an opposite-side `Limit` at the resting order's price for the executed shares, with a synthetic id at or above 2^63 that cannot collide with ITCH reference numbers. The book's matching then has to trade exactly against the named order, and the driver stops with an error if it does not. That way the matching path runs throughout the replay and the golden hash covers it. When the named order is not at the front (C executions at a price other than the displayed one, the opening and closing crosses), or the execution is for zero shares or more than the order has left, the driver falls back to `Reduce`, so the book stays faithful to Nasdaq's. On the sample day, 98.38% of executions replay as aggressors, and 99.8% of plain E executions hit the order our book has at the front, which checks the book's price-time priority against Nasdaq's.

While a security is halted, paused or in a quotation-only period, Nasdaq accepts orders that cross the book and matches them only in the reopening cross. This book always matches, so on the sample day 1,037 adds and replaces trade here that rested at Nasdaq, all on 15 books whose security was not trading (halted, paused or quotation only, for example IPO halts and securities halted all morning) when the book first diverged. Once a book has diverged, later deletes of orders it already traded away are rejected: all 1,881 rejections of the day fall on those books. The replay deliberately does not rest such orders without matching, so these 15 books stay different from Nasdaq's; the replay is deterministic either way, which is what the golden hash needs. The summary counts these openly (`unexpected_trade_messages*`, `rejections_on_diverged_books`, `rejections_on_clean_books`, `diverged_books`), and the two counts that a correct book and a consistent feed keep at zero are `unexpected_trade_messages_on_clean_trading_books` and `rejections_on_clean_books`.

## Checking the golden hash

```sh
cargo build --release --example replay
"${CARGO_TARGET_DIR:-target}"/release/examples/replay \
    --check harness/golden/12302019.txt /data/itch/12302019.NASDAQ_ITCH50
```

The run takes about five minutes. It recomputes every line of the golden file (the input's size and SHA-256, message counts by type, execution, rejection and unexpected-trade counts, and the replay hash) and exits with status 1, printing the recorded and computed value of each differing line, if anything changed. `--write FILE` records a new golden file; `--limit N` stops after N messages for quick looks; `--diagnose` prints each unexpected trade and rejection with its time and the book's best prices, and breaks down the fallback executions.

The replay hash is SHA-256 over every event of every book in message order, each tagged with its stock locate, plus the full state of every book (every level and order in priority order) every 2^20 messages (256 times over the sample day), after each system event and at the end. Events alone do not show where an order rests, since accepts, cancels and reductions carry no price and most orders are deleted without trading; the state checkpoints do. A change that alters any event, or the price level or queue position of any order resting at one of the checkpoints, changes the hash.

## Differential replay against the reference book

```sh
cargo build --release --example diff_slice
"${CARGO_TARGET_DIR:-target}"/release/examples/diff_slice /data/itch/12302019.NASDAQ_ITCH50
```

By default it takes AAPL, MSFT, AMD, TSLA, QQQ and SPY from the start of the day and stops after 5,000,000 of their book commands, which takes about two and a half minutes. It compares every event with the reference book, and snapshots and invariants every 200 commands per book and at the end. The default slice reaches books of up to 22,282 orders on 4,769 levels with queues of up to 1,336 orders. `--symbols A,B`, `--limit N` (0 for the whole day) and `--snapshot-every K` change the slice.

## Benchmark

```sh
scripts/bench.sh > bench.jsonl          # 5 runs, pinned to CPU 5
scripts/bench.sh -n 3 -c 2 -l 20000000  # 3 quick runs of 20M messages on CPU 2
```

Each run makes two passes over the whole day with a fresh replay each time. The throughput pass times each 64 MiB chunk's parse-and-apply loop as a whole and reports messages per second. The latency pass times every message (parse plus apply, end to end) with the TSC and reports the mean, p50, p99, p99.9 and max for all messages and for book messages only, from HDR histograms. The input is read into a resident buffer before any timing; histogram recording happens after each chunk; nothing is hashed. Each run also reports the timer's own cost (included in the latency figures), the TSC frequency, the load average and a fixed CPU-bound control loop timed before and after, which shows when a run was slowed by other work on the host. `bench.sh` prints the git commit, one JSON line per run and a summary line with the median, min, max and spread of every field, and refuses to run anywhere but Linux. `bench --no-throughput` skips the throughput pass.

The release profile uses one codegen unit, fat LTO and full debug info (for `perf`; it costs nothing at run time).

## Comparing two builds

On a shared laptop, runs of one build differ by more than a typical optimisation gains, so a candidate is never judged against numbers recorded earlier. Both binaries run in the same session instead, in pairs, on the same input and CPU, and the result comes from the difference within each pair.

```sh
scripts/bench-build.sh       # on the baseline commit: prints /root/bench-bins/<short commit>/bench
scripts/bench-build.sh       # again on the candidate commit
scripts/compare.sh -s all_p50_ns /root/bench-bins/BASE/bench /root/bench-bins/BASE/bench > aa.jsonl  # A/A method check
scripts/compare.sh -s all_p50_ns /root/bench-bins/BASE/bench /root/bench-bins/CAND/bench > ab.jsonl
scripts/compare.sh -r ab.jsonl -s all_p50_ns   # summarise saved runs again without running
```

`bench-build.sh [LABEL]` builds the release benchmark and copies it to `$BENCH_BINS/LABEL/bench` (default `/root/bench-bins`; the default label is the short commit, with `-dirty` when tracked files have uncommitted changes). `commit.txt` (the full commit), `build.txt` (toolchain, build time, SHA-256) and `settings.txt` go beside it. `settings.txt` records how the binary was compiled apart from the source: `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, the other Cargo environment variables that set flags or profile options, any Cargo config files, the target triple, the release profile from `Cargo.toml` and `rustc -vV`. It never overwrites an existing label.

`compare.sh [-s STAT] [-n PAIRS] [-c CPU] [-l LIMIT] [-t] [-f FLOOR] BASELINE CANDIDATE [FILE]` runs 6 pairs by default, pinned to CPU 5. Pair 1 runs the baseline first, pair 2 the candidate first, and so on. The pair count must be even and at least 6, so that each order has at least 3 pairs. Each run is the latency pass only (`bench --no-throughput`). A full-day run takes about RUN_TIME, so one pair takes about PAIR_TIME and a 6-pair comparison about COMPARE_TIME. `-t` adds the throughput pass and compares throughput too. `compare.sh` refuses two different binaries whose `settings.txt` files differ, and prints the difference, so that a change of compiler, flags or profile cannot pass as a change to the source. Binaries kept before `settings.txt` existed have no record and are refused, except as an A/A comparison of one binary with itself.

For each pair, and for each of the mean, p50, p99 and p99.9 of all messages and of book messages, it computes d = (candidate − baseline) / baseline × 100. The effect is the order-balanced mean of d: the mean over baseline-first pairs and the mean over candidate-first pairs, averaged. That cancels any constant advantage of running first or second in a pair, which is large on this machine: in the first A/A comparison the second run was about 9% faster at p50 and 13% faster on the mean. The output shows that advantage as `order`, or `not measured` when one order has no pairs.

The uncertainty of the effect comes from the spread of d within each order, which leaves the constant order advantage out of it. With s1² and n1 the sample variance and count of d over baseline-first pairs and s2² and n2 over candidate-first pairs, the standard error is sqrt(s1²/n1 + s2²/n2) / 2, and the bound is the one-sided 99% Student-t value for n1 + n2 − 2 degrees of freedom times the standard error. The t values are 3.747 for 4 degrees of freedom (6 pairs), 3.143 for 6 (8 pairs), 2.896 for 8 (10 pairs) and 2.764 for 10 (12 pairs).

`-s STAT` declares, before the run, the one statistic that gets a verdict, for example `all_p50_ns`. Every other statistic is printed with its effect and bound as information only, and without `-s` there is no verdict. The verdict is:

- `improved` when the effect plus or minus its bound lies wholly on the better side of zero (lower latency, higher throughput) and, when `-f` is given, beyond the floor;
- `regressed` in the mirror case;
- `inconclusive` otherwise. A saved comparison re-read with `-r` that has unequal or fewer than 3 pairs per order is `inconclusive (too few pairs)`, and the summary shows what the interval alone would give.

An A/A comparison (the same binary passed twice) checks the method: its effect should sit inside its bound, so it should come out `inconclusive`, and the summary says so when the two binaries are the same. If an A/A comparison comes out `improved` or `regressed`, no verdict from that host and session can be trusted. `-f FLOOR` takes such an A/A comparison's output and additionally requires the candidate's interval to clear that comparison's absolute effect plus its bound for the declared statistic. The floor file must have the same input, limit, CPU and `-t` setting, and `compare.sh` refuses a mismatched one. `-f` raises the bar considerably on a noisy day, so it is a choice made before the run like the statistic.

stdout gets every run's JSON line (with its pair and role), one line of d values per pair, and a summary line with the declared statistic, the verdict and every statistic's effect, standard error, bound and order advantage. stderr shows the same as tables, with each run's control-loop times beside its numbers so a loaded run is visible; the control loop never adjusts a result. A run whose wall time or control loop took more than 1.5 times the comparison's median is flagged, since the host may have slept or been busy. The summary is computed by `scripts/compare_summary.py`, and `python3 scripts/test_compare_summary.py` checks its arithmetic and the verdict rule on small hand-made comparisons.

### Stopping rule for optimisation rounds

A round keeps a change only when its comparison says `improved`, so the rule is fixed before any numbers are seen:

- The declared statistic, the pair count and whether `-f` is used are chosen before the run and do not change after it.
- Each round runs one comparison.
- `inconclusive` is final for that round: the change is not kept.
- Running the same comparison again is a new round, and it is logged as one, with its own result.
- A comparison with flagged runs counts as run; repeating it is also a new round.

Why this rule: an earlier version accepted a change when every pair agreed and the effect was larger than one A/A comparison's effect. That floor could be near zero by luck, agreement in five pairs happens by chance one time in 32 per statistic, eight statistics gave eight chances, and a round could be rerun until it agreed. The order advantage also made a real gain look worse in baseline-first pairs and better in candidate-first pairs, so it could never pass. The interval now comes from the repeated pairs themselves, the target is named in advance, and rerunning is counted.
