# Matching engine

A price-time priority order-matching engine in Rust, built for correctness first and then optimised down to the nanosecond. A slow reference book checks it through property tests, and a full Nasdaq ITCH 5.0 day replayed through it measures latency per message. Built out of interest.

## What exists

Stage 1 is a correct, unoptimised baseline: a library crate with a single-instrument `OrderBook`.

- Commands: limit and market orders, cancel, reduce (keeps time priority) and replace (new id, price and quantity; loses time priority and may trade). Invalid commands are rejected with a reason and leave the book unchanged.
- Each command appends its events (accepted, trades, fills, cancels, reductions, replacements, rejections) to a `Vec<Event>` the caller owns, in a deterministic order documented on `Event`. A trade always prints at the resting order's price.
- Queries: best bid and ask, depth per level, and a full snapshot of every resting order in priority order.
- Prices are `u32` ticks of $0.0001 and quantities are `u32` shares, matching ITCH 5.0. Level totals are `u64`.
- There is no self-trade prevention: orders carry no owner.

The book is split into modules that later stages measure and replace one at a time: `side` (price-level lookup, a `BTreeMap`), `level` (the FIFO queue at one price, a `VecDeque`), `orders` (the id map, a `HashMap`) and `book` (matching).

`harness/reference.rs` is a deliberately slow reference book that keeps every order in one flat list and scans it for each match. Only the tests compile it.

## Running the tests

```sh
cargo test --all-targets
```

- `tests/book.rs` holds the unit tests for the edge cases.
- `tests/prop.rs` sends random command streams to both books and requires identical events and identical snapshots after every command, along with the book's internal invariants. It runs 2,000 streams by default; set `PROPTEST_CASES` to run more, for example `PROPTEST_CASES=50000 cargo test --release --test prop`. It also checks that the generators reach the cases they are meant to cover.

CI runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test --all-targets` on every push to a branch other than `main`.
