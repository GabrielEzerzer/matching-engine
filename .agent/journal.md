# Journal — matching-engine, stage 1 (offloaded 2026-11-13)

## Goal
A price-time priority order-matching engine in Rust, correctness first, then optimised. Three stages,
strictly in order; a stage starts only after the previous one is merged:
1. A correct order book plus a slow reference book, compared by property tests. **This branch.**
2. The measuring rig (Nasdaq ITCH 5.0 full-day replay, golden output hash, latency benchmark). Not started.
3. Hillclimb per part. Not started.

reading: away · metric (latency, gated on correctness) · feature · logic

## State at handoff
- Branch `order-book`, head `ddb84bd` at handoff: the whole stage 1 crate, built by an Opus worker.
- The review gate has NOT run. A laptop reviewer was started and stopped at handoff; it produced nothing.
- GitHub Actions cannot run: the account's billing blocks job start (run 43495719051, no steps executed).
  So the suite runs in this sandbox as a named exception (CLAUDE.md "Test suites"): run each leg alone,
  never chained, and record the result with the SHA:
  `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings`; `cargo test --all-targets`.
  If Rust is missing, install it with rustup (stable, with rustfmt and clippy).
- The worker reports all three legs green on `ddb84bd` on the laptop, and five seeded bugs each caught
  (LIFO in a level, trade at taker price, empty level left behind, `<` for `<=` on crossing, partial
  reduce not lowering the level total).

## What the cloud session does
Only the stage 1 review gate. Do not start stage 2, do not download market data, do not optimise.
1. `reviewer` reviews `git diff origin/main...HEAD` against the acceptance criteria below. Verify each
   finding yourself, fix the real ones with the smallest change (a focused test for each bug fix),
   and have `reviewer` re-read the change since the last round until it approves.
2. Then `sol-loop` on the final diff. If `sol` returns SEAT UNAVAILABLE or TIMEOUT, a second separate
   `reviewer` stands in with the brief "attack the intent and the assumptions; leave settled findings
   alone", and the summary names the stand-in.
3. Run the three legs on the final head, push, and write `.agent/summary.md`: what changed, each leg's
   result with its SHA, the review ledger (seats, rounds, findings fixed or declined with reasons),
   open decisions. Stop at Waiting. Never merge, never touch `main`, do not use `gh pr`.

## Acceptance criteria (stage 1)
1. Limit, market and cancel orders with price-time priority; modify as reduce-in-place (keeps
   priority) and cancel-replace (loses priority, may trade). Integer prices and quantities, no floats.
2. Trades execute at the maker's resting price; an aggressive limit matches while crossing and rests
   its remainder; a market order never rests.
3. Invalid commands (zero qty, duplicate live id, cancel/modify of an unknown, filled or cancelled id)
   are rejection events, never panics. No panic or overflow on any command sequence over the full
   integer ranges.
4. The event stream is fully deterministic (no HashMap iteration order reaching output). Stage 2 hashes it.
5. A reference book under `harness/`, obviously correct by inspection and structurally independent of
   the main book, never imported by production code.
6. proptest streams fed to both books; every event and the final snapshot match exactly; generators
   hit crossing, cancels/modifies of live, filled, cancelled and unknown ids, duplicates, zero qty,
   markets into empty and thin books, reduce-to-zero, replace-that-crosses. Invariants after each command.
7. Unit tests for the edge cases: partial fills, self-crossing, cancelling a filled order, empty levels.
8. CI workflow: fmt, clippy, test as separate steps; push to non-main branches and workflow_dispatch
   only; `permissions: contents: read`; a timeout; no secrets.

## Where reviewers should look hardest
This stage decides whether every later optimisation round can be trusted. The most expensive mistake is
a bug shared by both books (same author, same understanding) or a property test blind to a class of
divergence.
- Shared-semantics errors: replace (same-id reuse, replace of a partially filled order, replace with an
  invalid new qty or price: is the old order left intact?), reduce edge cases, market remainder
  reporting, event order within a command.
- Whether the property test can see divergence: compared after every command or only at the end;
  snapshot order and quantity; shrinking; whether generated cancels mostly miss.
- Fit for ITCH replay in stage 2: Add (A/F), Execute (E/C: executes a resting order by reference
  number, C carries a price), Cancel (X, partial), Delete (D), Replace (U: new reference number, new
  price and shares, same side). Anything that will force a redesign is a design note, not a blocker,
  unless it is a bug now.
- Arithmetic: u32 quantity sums, u64 level totals, casts, underflow.
- Smallest-change discipline: no speculative abstraction or dead code.

## Decisions so far
- Types: Price u32 (ITCH ticks of $0.0001), Qty u32, OrderId u64; level totals u64.
- `ReduceTooLarge` is rejected, not clamped (worker's choice; reviewers may question it).
- No self-trade prevention (orders carry no owner). Finished ids are not tracked, so an id can be reused.
- CI tests run in debug so overflow checks stay on.

## Needs the laptop (skip in the cloud)
- The PR itself (already opened as a draft from the laptop), the merge, and everything in stages 2 and 3
  (the ITCH file and all benchmark timings live on the laptop's WSL2).

## Rounds
(none yet)
