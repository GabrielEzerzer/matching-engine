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
- `tests/workload.rs` checks the microbenchmark workloads (see "Microbenchmarks") on a hand-made command stream: the derived operations rebuild each book's orders, levels and resting shares, each order-map operation matches a lookup the book makes, and the encoding round-trips. It also checks that the `level` operations contain exactly the looks at the front of a queue that the book makes. Its tests of the adapters that drive the real parts, of the share totals that catch an adapter skipping quantity changes, and of the read check that catches an adapter skipping a lookup or returning the wrong result, need the `internals` feature, so run the suite a second time with it:

```sh
cargo test --all-targets --features internals
```

CI runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test --all-targets` on every push to a branch other than `main`, without the `internals` feature. CI never sees the ITCH file. The golden check, the differential slice and the benchmark are local runs under Linux.

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

`bench-build.sh [LABEL]` builds the release benchmark and the microbenchmarks and copies them to `$BENCH_BINS/LABEL/bench` and `$BENCH_BINS/LABEL/micro` (default `/root/bench-bins`; the default label is the short commit, with `-dirty` when tracked files have uncommitted changes). `commit.txt` (the full commit), `build.txt` (toolchain, build time, SHA-256) and `settings.txt` go beside it. `settings.txt` records how the binary was compiled apart from the source: `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, the other Cargo environment variables that set flags or profile options, the path and SHA-256 of each Cargo config file (never its contents, which can hold registry tokens), the target triple, the release profile from `Cargo.toml` and `rustc -vV`. It never overwrites an existing label.

`compare.sh [-s STAT] [-n PAIRS] [-c CPU] [-l LIMIT] [-t] [-w WINDOW] [-m PART] [-f FLOOR] BASELINE CANDIDATE [FILE]` runs 6 pairs by default, pinned to CPU 5. Pair 1 runs the baseline first, pair 2 the candidate first, and so on. The pair count must be even and at least 6, so that each order has at least 3 pairs. Each run is the latency pass only (`bench --no-throughput`). A full-day run takes about three and a half minutes, so one pair takes about seven minutes and a 6-pair comparison about 42 minutes. `-t` adds the throughput pass and compares throughput too. `-w` and `-m` select the shorter runs described under "Guardrail window" and "Microbenchmarks" below. `compare.sh` refuses two different binaries whose `settings.txt` files differ, and prints the difference, so that a change of compiler, flags or profile cannot pass as a change to the source. Binaries kept before `settings.txt` existed have no record and are refused, except as an A/A comparison of one binary with itself.

For each pair, and for each of the mean, p50, p99 and p99.9 of all messages and of book messages, it computes d = 100 × ln(candidate / baseline), the log ratio of the two runs, which is close to the percentage difference for small changes. The effect is the order-balanced mean of d: the mean over baseline-first pairs and the mean over candidate-first pairs, averaged. That cancels an advantage of running first or second in a pair, which is large on this machine: in the first A/A comparison the second run was about 8% faster at p50 and 12% faster on the mean, and in the second it was about −6% at p50 and −9% on the mean, so the first run was the faster one. Because the advantage multiplies the second run's time, it cancels exactly on the log scale; plain percentage differences would leave a bias of about 0.5% at a 10% advantage. The output shows the advantage as `order`, the percentage by which the second run was faster (negative when the first run was faster), or `not measured` when one order has no pairs.

The uncertainty of the effect comes from the spread of d within each order, which leaves the order advantage out of it. With s1² and n1 the sample variance and count of d over baseline-first pairs and s2² and n2 over candidate-first pairs, and a = s1²/n1 and b = s2²/n2, the standard error is sqrt(a + b) / 2. The bound is the one-sided 99% Student-t value times the standard error, with the Welch–Satterthwaite degrees of freedom (a + b)² / (a²/(n1 − 1) + b²/(n2 − 1)), rounded down and at least 1. That count is n1 + n2 − 2 when the two orders are equally noisy and falls towards n − 1 of the noisier order when they are not, so one quiet order cannot make the bound narrow. Some t values: 6.965 for 2 degrees of freedom, 4.541 for 3, 3.747 for 4 (6 equally noisy pairs), 3.143 for 6 (8 pairs), 2.896 for 8 (10 pairs) and 2.764 for 10 (12 pairs). The arithmetic stays on the log scale; the effect and the two ends of the interval (effect minus and plus the bound) are converted back to percent changes for the tables and the summary line, which also keeps the log-scale values.

`-s STAT` declares, before the run, the one statistic that gets a verdict, for example `all_p50_ns`. Every other statistic is printed with its effect and interval as information only, and without `-s` there is no verdict. The verdict is:

- `improved` when the whole interval lies on the better side of zero (lower latency, higher throughput) and, when `-f` is given, beyond the floor;
- `regressed` in the mirror case;
- `inconclusive` otherwise. A saved comparison re-read with `-r` that has unequal or fewer than 3 pairs per order is `inconclusive (too few pairs)`, and the summary shows what the interval alone would give.

An A/A comparison (the same binary passed twice) checks the method: its effect should sit inside its bound, so it should come out `inconclusive`, and the summary says so when the two binaries are the same. An A/A comparison that does not come out `inconclusive` on the declared statistic means the setup is not measuring cleanly. Find the cause before trusting any comparison, and do not rerun the A/A comparison in the hope that it passes. `-f FLOOR` takes an A/A comparison's output and additionally requires the candidate's interval to clear that comparison's absolute effect plus its bound for the declared statistic. The tables show that floor as the percent change each end of the interval must clear: `imp_bar` for `improved` (for latency the high end must be below it, for throughput the low end above it) and `reg_bar` for `regressed`. The floor file must be an A/A comparison by SHA-256 and have the same input, limit, CPU and `-t` setting, and `compare.sh` refuses anything else. `-f` raises the bar considerably on a noisy day, so it is a choice made before the run like the statistic.

stdout gets every run's JSON line (with its pair and role), one line of d values per pair, and a summary line with the declared statistic, the verdict and every statistic's effect, interval, degrees of freedom and order advantage. stderr shows the same as tables, with each run's control-loop times beside its numbers so a loaded run is visible; the control loop never adjusts a result. A run whose wall time or control loop took more than 1.5 times the comparison's median is flagged, since the host may have slept or been busy. The summary is computed by `scripts/compare_summary.py`, and `python3 scripts/test_compare_summary.py` checks its arithmetic and the verdict rule on small hand-made comparisons.

### Procedure for optimisation rounds

A round tries one change and keeps it only when the comparisons say `improved`, so the procedure is fixed before any numbers are seen.

The declared statistic, the pair count and whether `-f` is used are chosen before the first comparison of a round and do not change during it.

The pairs within one comparison share the machine's state, such as its temperature and the load from other work on the host, so they are not fully independent and the interval is narrower than the true uncertainty. A change is therefore kept only when a second comparison, with the same declared statistic and pair count, also says `improved`. The second comparison runs as a separate block, not back to back with the first: at least one other build or measurement runs in between. A first `improved` followed by `inconclusive` or `regressed` means the change is not kept.

`inconclusive` is final for that round. Running the same comparisons again is a new round, and it is logged as one with its own result. A comparison with flagged runs counts as run, and repeating it is also a new round.

When there are several alternatives for one part, each is compared once against the baseline with the same declared statistic and pair count, and only the one with the best effect goes on to the confirming comparison. The alternatives are compared on the same microbenchmark part, because effects measured on different parts have different denominators and cannot be ranked against each other. The confirming comparison shows that the chosen alternative beats the baseline again. It does not show that it beats the other alternatives.

A kept change must also pass the guardrail (see "Guardrail window"), reproduce the golden hash and pass the tests.

Every round is logged in `PERF.md`, kept or not, with the part that was changed, the microbenchmark part it was measured on, what that part's adapter includes (the timed fold of read results among it), the effect and interval of each comparison, and the guardrail result.

Over many rounds some kept changes will still be chance, and the effects reported for kept rounds are biased upward, because only the comparisons that came out well were kept. The unbiased figure for the whole exercise is one comparison of the starting commit against the final commit at the end.

Why this procedure: an earlier version accepted a change when every pair agreed and the effect was larger than one A/A comparison's effect. That floor could be near zero by luck, agreement in five pairs happens by chance one time in 32 per statistic, eight statistics gave eight chances, and a round could be rerun until it agreed. The order advantage also made a real gain look worse in baseline-first pairs and better in candidate-first pairs, so it could never pass. The interval now comes from the repeated pairs themselves, the target is named in advance, a gain has to show up twice in separate blocks, and rerunning is counted.

## Microbenchmarks

`examples/micro.rs` times one part of the book at a time, so that a change to one part can be measured in seconds instead of a full-day replay. It needs the `internals` feature, which exposes the parts as `matching_engine::internals`; default builds, the tests and the public API do not change.

```sh
cargo build --release --features internals --example micro
taskset -c 5 target/release/examples/micro side /data/itch/12302019.NASDAQ_ITCH50
```

The parts are:

- `side`: the price levels of one side (`BookSide`): best price, find a level, find or create one, remove one;
- `level`: the queue of orders at one price (`Level`): push, look at the front, reduce, pop the front, remove from anywhere, and reset when a level is emptied;
- `orders`: the order-id map (`OrderMap`): insert, get, update, remove;
- `book`: the book commands applied to `OrderBook`, which includes everything above and reports allocations per command from a counting global allocator;
- `parse`: splitting and decoding ITCH frames into `Message`, without a book.

The workload is the book commands of AAPL, MSFT, AMD, TSLA, QQQ and SPY from the start of the day until 10:30:00, which includes the first hour of regular trading: 2,922,290 commands. `parse` uses the 256 MiB of frames that start at 09:30:00 (8,357,979 messages). The operations of `side`, `level` and `orders` are derived by replaying those commands through the real book and watching it only through its public API, so they are a sequence of logical operations (for example "remove the order at price p on the sell side of book 3") rather than calls into the current code. A rewritten part runs the same workload through a small adapter in `harness/workload.rs`. Each operation stands for one call the book makes: for example, filling a maker is a lookup in the order map followed by a removal. `tests/workload.rs` checks that the derived operations rebuild the book's state on a hand-made command stream.

The first run derives the workloads, which takes about 15 seconds, and caches them in `micro-cache/` beside the ITCH file (`/data/itch/micro-cache/`), keyed by a hash of the inputs, so later runs start in under a second. A run that loads the cache refuses it unless it encodes back to the same bytes, so a changed encoding cannot run different operations under the same checksum. Preparation is never timed. Each run makes one untimed pass, then a fixed number of timed passes, then two more untimed passes over the part's whole workload, each from empty state. After every pass it checks that the part ends with the orders, levels and resting shares the derivation recorded (9,856,654 shares in 71,966 orders at 16,177 levels); `side` checks levels only, because its operations carry no quantities. For `side`, `level` and `orders` it also checks what the reads returned: every best price, level lookup, front of a queue and order lookup is folded into one number, which must equal the number the workload stores. The stored number comes from the adapters of the binary that derived the workload, which in a comparison is the baseline. One limit: every level lookup in `side` finds a level, and those levels hold no orders, so each lookup folds the same value. For lookups the check therefore proves only that the adapter took a fold step, not that it looked the level up or found the right one; an adapter is a few lines and is read in review, and a wrong level in the real part fails the golden hash and the tests. The fold is a multiply-add per value inside the timed passes, in the baseline's adapter and in a candidate's alike. Its cost does not cancel out of a percentage: it dilutes a difference between two builds, most in the parts with many reads, so a result measures the adapter with its fold, not the part's reads alone. A run takes about 15 seconds. It prints one JSON line with the part, the operation counts, `workload_checksum` (two runs with the same checksum ran the same operations), ns per operation as the median, min and max over the timed passes and their spread, allocations per pass and per operation, and the same host readings as `bench`. Allocations are counted in the three untimed passes only, so the timed passes pay nothing for the counting. Allocation counts can differ by one or two between passes, because the order map's hasher is seeded randomly and that changes when the hash table rehashes in place instead of growing; the line reports the median, min and max.

To compare two builds on one part, keep both with `scripts/bench-build.sh` and pass the `micro` binaries and `-m PART` to `compare.sh`:

```sh
scripts/compare.sh -m side -n 10 -s ns_per_op_min /root/bench-bins/BASE/micro /root/bench-bins/CAND/micro > side.jsonl
```

The statistics are `ns_per_op_median` and `ns_per_op_min`, all runs must report the same `workload_checksum`, `passes` and `ops`, and the comparison rule is the one described above. Before the pairs, `compare.sh` runs the baseline once with one timed pass and discards its output, so that a workload derived on first use is derived there and every recorded run loads it from the cache. `-m` cannot be combined with `-l`, `-w` or `-t`.

### What a microbenchmark result means

The microbenchmarks screen changes. They do not measure how much of the real replay's cost a part accounts for, and a percentage measured on one part does not carry over to the replay.

- **The statistics summarise whole passes.** `ns_per_op_min` is the fastest timed pass divided by its operations: the best observed cost of a warmed pass. It is the least disturbed by other work on the host, which is why rounds declare it, but a change that is faster in its best pass and slower in most would also win on it. Each comparison therefore reports the effect on `ns_per_op_median` beside the declared one, and a round whose two effects disagree in sign is logged as such. Neither statistic says anything about the latency of single operations or their tail.
- **Memory is laid out differently from the real book.** `level` keeps every level in one vector created at the start of each pass, where the book keeps levels inside its price map. `side` runs on levels that hold no orders. `orders` runs the map alone. A queue that stores more inline can win on `level` and lose once it sits inside the price map, and a change that improves locality between the order map and the levels cannot show its gain on either part alone.
- **The stream is narrower and more regular than the replay.** It holds six symbols, and the work of all other symbols and of the other parts between two operations is removed. Every pass starts from empty state, reuses the memory the previous pass freed, and repeats the same operations, so caches and the branch predictor are warmer than they are during a day's replay, and the state never grows to what the book holds later in the day.

A change whose gain depends on memory layout or on how two parts work together is therefore compared on `book`, not on its own part. This applies to any change that needs the order map inside the `level` adapter. Its log entry says that the result is the cost of pre-decoded book commands, not the cost of queue operations.

## Guardrail window

A change that wins on its part still has to hold up end to end. `bench --window HH:MM:SS-HH:MM:SS` times only the messages in that part of the day: it first replays everything before the window untimed, so the books are in their real state when timing starts, and stops at the first message at or after the end. `compare.sh -w WINDOW` passes the option through.

```sh
scripts/compare.sh -w 09:30:00-10:00:00 -n 10 -s book_mean_ns /root/bench-bins/BASE/bench /root/bench-bins/CAND/bench > guard.jsonl
```

The guardrail asks a different question from a round's comparisons: not whether the change is faster, but whether it could be meaningfully slower. Its statistic and its margin are declared before it runs. The statistic is `book_mean_ns`, the mean time of the messages that reach a book, because a mean moves when rare operations get slower and a median does not. The margin is 5%: the tolerance chosen for this project, not something the pairs establish. The guardrail passes if and only if the upper end of the comparison's interval is below +5%, whatever the comparison's verdict against zero. Under the comparison's sampling assumptions (see the procedure above for how far they hold), that rules out a regression of that size at the interval's confidence. The verdict alone settles nothing: `inconclusive` with an interval from −2% to +2% passes, and `inconclusive` with an interval from −2% to +8% does not, because it still allows an 8% regression. When the upper end is at or above the margin, the guardrail is unresolved and the change is not kept; it can be measured again as a new comparison with more pairs or on a quieter machine. Ten pairs decide clear cases and leave noisy ones unresolved.

A pass covers this statistic in this window. It does not cover tail latency, or the state the books reach later in the day.

The guardrail window is 09:30:00 to 10:00:00, the first half hour of regular trading. It is the densest part of the day, with 32,817,244 messages (about an eighth of the day) and the largest bursts, so it exercises the book under the heaviest load. The warm-up before it is only the 6,696,719 pre-market messages, and the input up to 10:00 is 1.24 GB, which stays in the page cache between runs. A run takes about 30 seconds on a quiet machine, so a 10-pair comparison takes about ten minutes.
