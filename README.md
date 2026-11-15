# Matching engine

A price-time priority order-matching engine in Rust. A deliberately slow reference book checks it event by event, a full Nasdaq ITCH 5.0 trading day is replayed through it, and every message of that day is timed. The library has no runtime dependencies.

## Current state

The work ran in three stages, all finished. Stage 1 built a correct, unoptimised order book and the reference book that checks it. Stage 2 built the rig that makes measurements trustworthy: an ITCH 5.0 parser, a replay driver, a golden hash of the whole day's output, differential checks and a latency benchmark. Stage 3 tried optimisations one part at a time under a rule fixed before any numbers were seen, and kept three of them.

### How correctness is checked

- **Differential testing against a reference model.** `harness/reference.rs` keeps every order in one flat list and scans it for each match. Property tests send random command streams to both books and require identical events, and identical snapshots of every resting order, after every command. A deterministic deep-book test does the same over 8,684 commands on 300 price levels per side, and `examples/diff_slice` replays 5,000,000 commands of six busy symbols from the real day through both books and compares every event.
- **Aggressor replay against Nasdaq's own executions.** When an ITCH execution names the order at the front of the best level, the replay sends it as the incoming order it really was, and the book's own matching has to trade against exactly that order or the replay stops. On the sample day 98.4% of executions replay this way, and 99.8% of plain E executions name the order this book has at the front, which checks the book's price-time priority against Nasdaq's.
- **A golden hash of the whole day.** SHA-256 over every event of every book and periodic snapshots of every book's full state, for all 268,744,780 messages of 30 December 2019. Every kept optimisation reproduced it.

### Results

Median of three whole-day runs of the final build (`2c792ab`) on one pinned vCPU of a shared Kaggle cloud VM: 4 vCPUs, Intel Xeon at 2.20 GHz (Broadwell), Linux, `rustc 1.100.0`, fat LTO.

| Measure | Result |
|---|---|
| Latency per message, p50 | 414 ns |
| Latency per message, p99 | 2.0 µs |
| Latency per message, p99.9 | 7.9 µs |
| Throughput, one vCPU | 1.78 million messages per second |
| Book command, final build against the unoptimised stage 2 build | 38.4% less time (98% interval 38.1% to 38.8%) |

What is timed: each message on its own, from splitting its frame off an in-memory buffer through decoding it and applying it to its book, matching included. Messages run back to back; the file is read in 64 MiB chunks outside the timed regions. The timer's own cost, about 20 ns on that machine, is included in every latency. Throughput comes from a separate pass with no per-message timer. The last row compares the final build with the unoptimised stage 2 code (`f84e022`) on 2,922,290 pre-decoded commands of six busy symbols, in 10 alternating pairs; a second comparison in the same session confirmed it at 34.8%.

These numbers are not directly comparable with the messages-per-second figures that ITCH book-building replays publish. Those replays rebuild the book from Nasdaq's executions without matching and divide wall time by the message count. Here matching runs throughout, the latency rows time each message separately, and the throughput pass times only parsing and applying from memory, leaving file reads out. Several published engines report higher throughput than this one.

### How this was built

Built with AI coding agents under the author's direction. Every pull request that changed code was reviewed before it merged by a separate review agent with a fresh context, and most also by a second model (GPT); each pull request's description records its reviews. Documentation-only changes were merged without review.

## What exists

The library crate has a single-instrument `OrderBook`.

- Commands: limit and market orders, cancel, reduce (keeps time priority) and replace (new id, price and quantity; loses time priority and may trade). Invalid commands are rejected with a reason and leave the book unchanged.
- Each command appends its events (accepted, trades, fills, cancels, reductions, replacements, rejections) to a `Vec<Event>` the caller owns, in a deterministic order documented on `Event`. A trade always prints at the resting order's price.
- Queries: best bid and ask, depth per level, a full snapshot of every resting order in priority order, one resting order's side, price and quantity (`order`), and the order with time priority at the best price of a side (`front_order`).
- Prices are `u32` ticks of $0.0001 and quantities are `u32` shares, matching ITCH 5.0. Level totals are `u64`.
- There is no self-trade prevention: orders carry no owner.

The book is split into modules that can be measured and replaced one at a time: `side` (price-level lookup, a sorted index into a slab of levels), `level` (the FIFO queue at one price, a doubly linked list in a slab of nodes per book), `orders` (the id map, a `HashMap` with a keyed integer hasher) and `book` (matching). Stage 3 replaced the first three, which started as a `BTreeMap` of levels, a `VecDeque` per level and a `HashMap` with SipHash. An ITCH decoder that reads fields from fixed-size arrays was faster on its own part but did not pass the end-to-end check, so it was not merged; it stays on the branch `parse-arrays`.

`harness/reference.rs` is the slow reference book. Only the tests and the differential replay compile it.

The replay and measurement rig:

- `src/itch.rs` parses ITCH 5.0 without allocating. It fully decodes the order messages (A, F, E, C, X, D, U) and the system event (S), stock directory (R) and stock trading action (H) messages. Every other 5.0 message type is recognised and length-checked. An unknown type, a wrong length or a cut-off frame is an error.
- `src/replay.rs` drives one `OrderBook` per stock locate from the messages (see "How ITCH maps to book commands").
- `examples/replay.rs` replays a whole day, prints the summary counts and the golden hash, and records or checks them against `harness/golden/12302019.txt`. The hash encoding is documented in `harness/golden.rs`.
- `examples/diff_slice.rs` replays busy symbols through the real book and the reference book and compares every event.
- `examples/bench.rs` and `scripts/bench.sh` measure latency per message and throughput; `examples/micro.rs` times one part of the book at a time (see [BENCHMARKING.md](BENCHMARKING.md)).

## Running the tests

```sh
cargo test --all-targets
```

- `tests/book.rs` holds the unit tests for the edge cases.
- `tests/prop.rs` sends random command streams to both books and requires identical events and identical snapshots after every command, along with the book's internal invariants. It runs 2,000 streams by default; set `PROPTEST_CASES` to run more, for example `PROPTEST_CASES=50000 cargo test --release --test prop`. It also checks that the generators reach the cases they are meant to cover.
- `tests/deep.rs` is a deterministic deep-book differential test. It builds 300 price levels per side with queues of up to 252 orders, then cancels, reduces, replaces, sweeps, drains and repopulates, comparing events, snapshots and invariants with the reference after each of its 8,684 commands.
- `tests/replay.rs` covers the ITCH-to-command mapping and the counters; `tests/golden.rs` pins the hash encoding with digests computed independently from the documented byte layout; the parser's tests are in `src/itch.rs`.
- `tests/workload.rs` checks the microbenchmark workloads (see "Microbenchmarks" in [BENCHMARKING.md](BENCHMARKING.md)) on a hand-made command stream: the derived operations rebuild each book's orders, levels and resting shares, each order-map operation matches a lookup the book makes, and the encoding round-trips. It also checks that the `level` operations contain exactly the looks at the front of a queue that the book makes. Its tests of the adapters that drive the real parts, of the share totals that catch an adapter skipping quantity changes, and of the read check that catches an adapter skipping a lookup or returning the wrong result, need the `internals` feature, so run the suite a second time with it:

```sh
cargo test --all-targets --features internals
```

CI runs `cargo fmt --check`, then `cargo clippy --all-targets -- -D warnings` and `cargo test --all-targets` both without and with `--features internals`, on every push to a branch other than `main`. CI never sees the ITCH file. The golden check, the differential slice and the benchmark are local runs under Linux.

## The ITCH data

The replay uses the public Nasdaq TotalView-ITCH 5.0 sample for 30 December 2019 (3,524,013,057 bytes compressed, 8,251,407,909 bytes and 268,744,780 messages decompressed). On Linux:

```sh
export ITCH_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/itch"   # the default; any folder works
scripts/fetch-itch.sh
```

It downloads with resume, checks the pinned size and SHA-256 of the `.gz`, decompresses it and skips work already done. `VERIFY=1` also hashes an existing decompressed file. The data never goes into git.

All replay, golden, differential and benchmark runs below are Linux runs, from the repository root, in release mode, with `ITCH_DIR` set as above. If the checkout is on a slow filesystem (a Windows drive under WSL2, for example), keep the build output elsewhere: `export CARGO_TARGET_DIR="$HOME/cargo-target/matching-engine"`.

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
    --check harness/golden/12302019.txt "$ITCH_DIR"/12302019.NASDAQ_ITCH50
```

The run takes about five minutes. It recomputes every line of the golden file (the input's size and SHA-256, message counts by type, execution, rejection and unexpected-trade counts, and the replay hash) and exits with status 1, printing the recorded and computed value of each differing line, if anything changed. `--write FILE` records a new golden file; `--limit N` stops after N messages for quick looks; `--diagnose` prints each unexpected trade and rejection with its time and the book's best prices, and breaks down the fallback executions.

The replay hash is SHA-256 over every event of every book in message order, each tagged with its stock locate, plus the full state of every book (every level and order in priority order) every 2^20 messages (256 times over the sample day), after each system event and at the end. Events alone do not show where an order rests, since accepts, cancels and reductions carry no price and most orders are deleted without trading; the state checkpoints do. A change that alters any event, or the price level or queue position of any order resting at one of the checkpoints, changes the hash.

## Differential replay against the reference book

```sh
cargo build --release --example diff_slice
"${CARGO_TARGET_DIR:-target}"/release/examples/diff_slice "$ITCH_DIR"/12302019.NASDAQ_ITCH50
```

By default it takes AAPL, MSFT, AMD, TSLA, QQQ and SPY from the start of the day and stops after 5,000,000 of their book commands, which takes about two and a half minutes. It compares every event with the reference book, and snapshots and invariants every 200 commands per book and at the end. The default slice reaches books of up to 22,282 orders on 4,769 levels with queues of up to 1,336 orders. `--symbols A,B`, `--limit N` (0 for the whole day) and `--snapshot-every K` change the slice.

## Benchmark

```sh
scripts/bench.sh > bench.jsonl          # 5 runs, pinned to the last CPU
scripts/bench.sh -n 3 -c 2 -l 20000000  # 3 quick runs of 20M messages on CPU 2
```

Each run makes two passes over the whole day with a fresh replay each time. The throughput pass times each 64 MiB chunk's parse-and-apply loop as a whole and reports messages per second. The latency pass times every message (parse plus apply, end to end) with the TSC and reports the mean, p50, p99, p99.9 and max for all messages and for book messages only, from HDR histograms. The input is read into a resident buffer before any timing; histogram recording happens after each chunk; nothing is hashed. Each run also reports the timer's own cost (included in the latency figures), the TSC frequency, the load average and a fixed CPU-bound control loop timed before and after, which shows when a run was slowed by other work on the host. `bench.sh` prints the git commit, one JSON line per run and a summary line with the median, min, max and spread of every field, and refuses to run anywhere but Linux. `bench --no-throughput` skips the throughput pass.

The release profile uses one codegen unit, fat LTO and full debug info (for `perf`; it costs nothing at run time).

Comparing two builds, the rule that decides whether an optimisation is kept, the microbenchmarks, the guardrail window and the Kaggle runner are described in [BENCHMARKING.md](BENCHMARKING.md).

## License

Licensed under either of the [MIT license](LICENSE-MIT) or the [Apache License, Version 2.0](LICENSE-APACHE), at your option.
