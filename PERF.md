# Performance log

This file records the baseline and every optimisation round, kept or reverted, with its numbers. How the numbers are measured, and the rule that decides whether a change is kept, are described in the README under "Benchmark" and in `BENCHMARKING.md` under "Procedure for optimisation rounds".

## Machine

- Laptop, Intel Core i7-1185G7 (4 cores, 8 threads), 32 GB RAM, on mains power, Windows power scheme "Balanced".
- Linux under WSL2 (kernel 6.6.87.2-microsoft-standard-WSL2, 15 GB given to the VM), `rustc 1.100.0`, release profile with fat LTO and one codegen unit.
- Timer: the TSC, read with `rdtsc`/`rdtscp`; its own cost is about 17 ns at p50 and is included in every latency below.
- Input: the Nasdaq TotalView-ITCH 5.0 sample for 30 December 2019, 268,744,780 messages, 8,251,407,909 bytes.

No timing in this file was taken on native Windows.

## Baseline (stage 2)

The benchmark replays the whole day through one order book per stock and times each message: decoding it and applying it to its book.

### Latency per message, whole day

Twenty runs of one binary, built from commit `c67ded3`. The Rust sources are the same at the stage 2 head; later commits changed only scripts and documentation. Each run was pinned to CPU 5. The runs were taken on 14 November 2026 between 00:55 and 02:51 as two same-binary comparisons of five pairs each, the first on an otherwise idle machine, the second while other builds ran on CPUs 0 to 3.

| Statistic | Median of 20 runs | Fastest run | Slowest run |
|---|---|---|---|
| p50 | 482.1 ns | 427.1 ns | 566.6 ns |
| p99 | 2,690.4 ns | 2,264.0 ns | 3,208.3 ns |
| p99.9 | 11,139.1 ns | 9,446.1 ns | 19,861.3 ns |
| mean | 702.0 ns | 587.9 ns | 847.2 ns |

For book messages only (adds, executions, cancels, deletes and replaces), the medians are 493.4 ns at p50, 2,704.9 ns at p99 and 11,298.4 ns at p99.9.

The slowest single message in a run took between 91 ms and 169 ms. Pauses of that size are the virtual machine being descheduled, not the engine.

### Throughput

Five runs of the first benchmark binary, commit `1b4400c`, on 13 November 2026 at about 23:50 on an idle machine. Throughput is measured in a separate pass with no per-message timing. The median was 1,890,689 messages per second, the slowest run 1,468,119 and the fastest 1,967,711. That binary predates changes to `src/replay.rs` that did not target speed. Throughput has not been measured again on the stage 2 head.

The same five runs gave a p50 of 415.6 ns, a p99 of 2,136.3 ns and a p99.9 of 9,626.6 ns, which is faster than the twenty runs above. The machine was quieter then.

### What the baseline can and cannot show

Runs of the same binary differ by about 15% either side of the median on this machine. Within one sitting the second run of a pair was about 8% faster at p50 in the first comparison and about 6% slower in the second, so there is no stable advantage to correct for. A single full-day run therefore says little about a change of a few percent. Optimisation rounds are judged on short microbenchmarks of one part each, compared in alternating pairs, and whole-day runs are used for reporting.

The figure to trust for the whole exercise is one paired comparison of the stage 2 head against the final commit, reported at the end of this file.

## Rounds

### Where the rounds were measured

The laptop was under load from other work during stage 3, and the same binary differed by up to a quarter between runs there, so no laptop timing decides a round. Every comparison below ran on a Kaggle CPU session: a 4-vCPU virtual machine under KVM, Linux, pinned to CPU 3, with `rustc 1.100.0` and the same release profile. The session of record, `stage3-total-r2` below, wrote its host into `host.txt`: `lscpu` reports "Intel(R) Xeon(R) CPU @ 2.20GHz" with no model number, CPU family 6, model 79 (a Broadwell part), 2 cores with 2 threads each, 55 MiB of L3 cache, 32 GB of RAM and Linux 6.18.55. The timer's own cost there was about 20 ns at p50, included in every latency it reports. The other sessions' hosts were not compared with it. `scripts/kaggle_bench.py` runs one session per group of alternatives and builds the baseline and the candidates inside the session. The section "Running the comparisons on Kaggle" in `BENCHMARKING.md` says what such a session shows and what it does not. In short: a verdict is evidence from one session on a shared machine, the confirming comparison in the same session is a second look and not an independent test, and effects from different sessions or parts are not ranked against each other.

Every candidate was measured as commit `209e9e2` plus one change. `209e9e2` is the microbenchmark setup one review round before it merged: it derives the same operations as the merged setup and checks each pass's end state, and the merged setup also checks what each read returned. Each kept change was then applied to `main` and passed the merged setup's checks there.

The declared statistic for every part comparison is `ns_per_op_min` (the best warmed pass) with 10 pairs. The effect on the median pass is shown beside it. An effect is the percent change of candidate against baseline, with a one-sided 99% interval; negative is faster. The guardrail is the mean time of book messages in the 09:30 to 10:00 window (`book_mean_ns`, 10 pairs) and passes when the interval's upper end is below +5%. A run is flagged when its control loop shows the host was busy; a comparison with flagged runs still counts as run.

Absolute numbers in this section belong to the Kaggle machine and must not be set beside the laptop baseline above.

### Order-id map

Session `stage3-ordermap`, started 14 November 2026 at 04:56 UTC. Baseline `209e9e2`. Measured part: `orders`, 4,978,788 map operations per pass (the inserts, lookups and removals the book made on six busy symbols up to 10:30), run through maps built the way the book builds them.

| Step | Change | Effect on best pass | Interval | Effect on median pass | Verdict |
|---|---|---|---|---|---|
| Same binary twice | none | +0.75% | −0.35% to +1.87% | +1.44% | inconclusive, as it should be (1 flagged run) |
| First comparison | keyed fmix64 hasher in place of SipHash (`0af2714`) | −43.87% | −44.63% to −43.09% | −42.94% | improved |
| First comparison | flat open-addressing table in place of `HashMap` (`182f5cc`) | −28.25% | −29.34% to −27.14% | −27.95% | improved |
| Same binary twice | none | −0.75% | −2.23% to +0.75% | −1.01% | inconclusive, as it should be |
| Confirming comparison | keyed fmix64 hasher | −43.47% | −44.08% to −42.84% | −43.24% | improved (1 flagged run, on the candidate) |

Guardrail for the hasher: `book_mean_ns` −0.51%, interval −4.25% to +3.37%. The upper end is below +5%, so it passes. One baseline run was flagged; in that pair the candidate measured 3.4% slower, so the flagged run did not help the pass. The golden replay hash was unchanged and the tests passed in the session.

**Kept: the keyed fmix64 hasher.** On the Kaggle machine a map operation went from 51.8 ns to 29.1 ns (median over the runs of each run's best pass). The guardrail shows no end-to-end change either way: the map is a small share of a message's cost, and the window's mean moved from 787.6 ns to 796.3 ns between the two builds' medians, well inside the interval.

Not kept: the flat table. It improved once, but only the best alternative of a group goes to the confirming comparison, so it was not confirmed. It also uses more memory per live order than the standard map.

A third alternative, taking the map entry once per command instead of looking the id up twice (`ade272f`), changes how the book calls the map, so it was measured on the `book` part in the session below: +0.77%, interval −0.56% to +2.11%, median pass −2.76% (the two disagree in sign; 1 flagged run). Inconclusive, so not kept.

### Order queue at a price level, round 1

Session `stage3-book`, started 14 November 2026 at 04:56 UTC. Baseline `209e9e2`. Both alternatives add a field to the resting order and change how the book calls the level, so a queue-only microbenchmark would need a different adapter for each and could not rank them. They were measured on `book`: 2,922,290 commands per pass through whole order books.

| Step | Change | Effect on best pass | Interval | Effect on median pass | Verdict |
|---|---|---|---|---|---|
| Same binary twice | none | +0.61% | −2.32% to +3.62% | +0.91% | inconclusive, as it should be |
| First comparison | lazy removal: cancelled orders stay queued until they reach the front (`81c7bd6`) | +2.54% | −6.55% to +12.51% | +2.68% | inconclusive |
| First comparison | doubly linked ring through a per-book slab (`4a2bce7`) | −9.48% | −10.15% to −8.82% | −9.49% | improved |
| Same binary twice | none | +0.23% | −0.75% to +1.23% | +0.64% | inconclusive, as it should be |
| Confirming comparison | linked ring | −6.80% | −16.76% to +4.37% | −5.92% | inconclusive (2 flagged runs) |

Not kept in this round: lazy removal (inconclusive) and the linked ring (confirmation inconclusive). The host was busy during the last two pairs of the confirmation; the other eight pairs lay between −9.3% and −11.1%. An inconclusive result is final for its round whatever the cause, so the guardrail and the golden check did not run. The linked ring was measured once more as round 2, below.

### Order queue at a price level, round 2

Session `stage3-book-r2`, started 14 November 2026 at 05:41 UTC. Baseline `209e9e2`, candidate `4a2bce7` (the linked ring alone), part `book`, the same statistic and pair count as round 1. This is the one second round the procedure allows. Its reason was written down before it started: the control loop flagged two runs of round 1's confirming comparison, and the flag comes from the loop's own timing, not from the result.

| Step | Change | Effect on best pass | Interval | Effect on median pass | Verdict |
|---|---|---|---|---|---|
| Same binary twice | none | +0.12% | −0.81% to +1.06% | −0.38% | inconclusive, as it should be |
| First comparison | linked ring | −10.09% | −12.23% to −7.90% | −9.22% | improved (2 flagged runs, on the candidate) |
| Same binary twice | none | −0.54% | −1.76% to +0.70% | −2.12% | inconclusive, as it should be |
| Confirming comparison | linked ring | −10.36% | −11.59% to −9.11% | −12.34% | improved |

Guardrail: `book_mean_ns` −9.87%, interval −13.98% to −5.57%. The upper end is below +5%, so it passes, and the interval lies wholly below zero: this change is visible end to end. Two baseline runs were flagged (pairs 8 and 9, at −14.2% and −3.5%); all ten pairs were negative, from −3.5% to −15.8%. The golden replay hash was unchanged and the tests passed in the session.

**Kept on its second round: the linked ring.** Both rounds side by side: round 1's first comparison −9.48%, its confirmation −6.80% with an interval from −16.76% to +4.37% (inconclusive, not kept); round 2's first comparison −10.09%, its confirmation −10.36%. Two rounds are two chances to pass, so this result is less certain than round 2's intervals say on their own.

On the Kaggle machine a book command went from 177.3 ns to 159.7 ns (median over the runs of each run's best pass), and in the guardrail window the mean time of a book message went from 777.7 ns to 701.6 ns and its median from 564.4 ns to 482.6 ns (medians over the ten runs of each build).

The code that merged differs from the measured commit in two places that the `book` measurement does not touch: `Level::check`, which only the tests and the invariant checker call, now also fails for a ring shorter than the level's count, and the `level` microbenchmark's adapter was rewritten for the merged setup's read checks.

### Price-level lookup, round 1

Session `stage3-levels`, started 14 November 2026 at 04:51 UTC. Baseline `209e9e2`. Measured part: `side`, 5,004,039 price-level operations per pass (find, insert, remove and best-price reads on six busy symbols up to 10:30).

| Step | Change | Effect on best pass | Interval | Effect on median pass | Verdict |
|---|---|---|---|---|---|
| Same binary twice | none | −0.29% | −0.84% to +0.27% | +0.05% | inconclusive, as it should be |
| First comparison | sorted `Vec` of levels in place of the `BTreeMap` (`54236c4`) | −6.42% | −6.78% to −6.07% | −6.04% | improved |
| First comparison | `BTreeMap` with the best price cached (`5c22fc1`) | −8.63% | −9.07% to −8.18% | −8.14% | improved |
| First comparison | sorted index of (price, slot) into a slab of levels (`5d2380b`) | −44.16% | −44.44% to −43.87% | −44.27% | improved (1 flagged run, on the baseline) |
| Same binary twice | none | −0.08% | −0.48% to +0.32% | −0.02% | inconclusive, as it should be |
| Confirming comparison | sorted index into a slab | −43.94% | −44.15% to −43.73% | −43.83% | improved |

The golden replay hash was unchanged and the tests passed in the session. The session's window comparison ran with the statistic an earlier draft of the plan declared (`all_p50_ns`, 6 pairs: −9.96%, interval −18.73% to −0.25%; `book_mean_ns` in the same runs −9.80%, interval −17.80% to −1.03%). That is not the declared guardrail, so nothing was kept in this round. The two alternatives that improved less were not confirmed, because only the best of a group goes to the confirming comparison.

### Price-level lookup, round 2

Session `stage3-levels-r2`, started 14 November 2026 at 05:51 UTC. Baseline `209e9e2`, candidate `5d2380b` (the sorted index alone), part `side`, the same statistic and pair count as round 1. This is the one second round the procedure allows. Its reason was written down before it started: round 1's guardrail ran the wrong statistic and too few pairs.

| Step | Change | Effect on best pass | Interval | Effect on median pass | Verdict |
|---|---|---|---|---|---|
| Same binary twice | none | +0.23% | −0.10% to +0.57% | +0.51% | inconclusive, as it should be |
| First comparison | sorted index into a slab | −44.08% | −44.35% to −43.81% | −43.95% | improved |
| Same binary twice | none | +0.12% | −0.08% to +0.31% | −0.07% | inconclusive, as it should be |
| Confirming comparison | sorted index into a slab | −43.88% | −44.05% to −43.70% | −43.98% | improved |

Guardrail: `book_mean_ns` −7.63%, interval −14.78% to +0.11%. The upper end is below +5%, so it passes. The interval reaches just above zero, so this test does not by itself show an end-to-end gain. One candidate run was flagged (pair 2, the only positive pair, at +8.94%); the other nine pairs lay between −2.5% and −16.8%. The window's median book message, reported for information, changed by −8.78% (interval −13.36% to −3.95%). The golden replay hash was unchanged and the tests passed in the session.

**Kept on its second round: the sorted index into a slab of levels.** Both rounds side by side: round 1's first comparison −44.16% and confirmation −43.94%, with no guardrail of record; round 2's first comparison −44.08%, confirmation −43.88% and the guardrail above. Two rounds are two chances to pass, so the guardrail result is less certain than its interval says on its own. The effect on the part itself was the same in all four comparisons.

On the Kaggle machine a price-level operation went from 44.4 ns to 24.9 ns (median over the runs of each run's best pass). In the guardrail window the mean time of a book message was 597.9 ns for the baseline and 562.5 ns for the candidate, and its median 465.1 ns and 431.5 ns (medians over the ten runs of each build). These window numbers are from a different session than the order queue's and are not comparable with them.

The code that merged differs from the measured commit: removing a level checks in release builds too that the level is empty (one comparison per removed level), and it sits on top of the hasher and the linked ring, while the measurement was of the sorted index alone against `209e9e2`. The combination is timed in "Start against final" below.

### ITCH parsing, round 1

Session `stage3-parse`, started 14 November 2026 at 04:51 UTC. Baseline `209e9e2`. Measured part: `parse`, the 8,357,979 messages in the 256 MiB of frames that start at 09:30:00, split and decoded without a book.

| Step | Change | Effect on best pass | Interval | Effect on median pass | Verdict |
|---|---|---|---|---|---|
| Same binary twice | none | −0.86% | −4.61% to +3.03% | −0.68% | inconclusive, as it should be (1 flagged run) |
| First comparison | one length check per message, then fields read from fixed-size arrays (`ef0160d`) | −16.86% | −17.53% to −16.19% | −16.48% | improved (1 flagged run, on the candidate) |
| First comparison | the three rare message types decoded out of line (`985fe7a`) | −9.43% | −10.41% to −8.43% | −8.68% | improved |
| First comparison | the frame splitter and decoder inlined into the consuming loop (`85197a8`) | −7.21% | −8.35% to −6.06% | −7.86% | improved (1 flagged run, on the baseline) |
| Confirming comparison | fields from fixed-size arrays | −17.03% | −18.74% to −15.28% | −16.87% | improved (2 flagged runs, on the baseline) |

Between the first comparisons and the confirmation the session ran the whole-day benchmark of the baseline, not a second same-binary check. The golden replay hash was unchanged and the tests passed in the session. The session's window comparison ran with the statistic an earlier draft of the plan declared (`all_p50_ns`, 6 pairs: −0.34%, interval −4.67% to +4.19%; `book_mean_ns` in the same runs +0.09%, interval −11.13% to +12.72%). That is not the declared guardrail, so nothing was kept in this round. The two alternatives that improved less were not confirmed, because only the best of a group goes to the confirming comparison.

### ITCH parsing, round 2

Session `stage3-parse-r2`, started 14 November 2026 at 06:18 UTC. Baseline `209e9e2`, candidate `ef0160d` (the array decoder alone), part `parse`, the same statistic and pair count as round 1. This is the one second round the procedure allows. Its reason was written down before it started: round 1's guardrail ran the wrong statistic and too few pairs.

| Step | Change | Effect on best pass | Interval | Effect on median pass | Verdict |
|---|---|---|---|---|---|
| Same binary twice | none | +1.62% | −4.34% to +7.95% | +1.64% | inconclusive, as it should be |
| First comparison | fields from fixed-size arrays | −14.52% | −15.67% to −13.36% | −14.36% | improved |
| Same binary twice | none | +0.10% | −1.25% to +1.46% | +1.06% | inconclusive, as it should be (1 flagged run) |
| Confirming comparison | fields from fixed-size arrays | −14.78% | −15.66% to −13.90% | −15.00% | improved |

Guardrail: `book_mean_ns` +0.44%, interval −4.73% to +5.90%. The upper end is not below +5%, so the guardrail does not pass. No run was flagged; the ten pairs lay between −8.33% and +8.55%, and the window's median book message was 384.9 ns for both builds. The golden replay hash was unchanged and the tests passed in the session.

**Not kept, and dropped: the array decoder.** It improved and was confirmed on its own part in both rounds (a parsed message went from 22.3 ns to 19.0 ns in this session), but it did not pass the guardrail in the one round where the guardrail was of record, and a candidate gets no third round. The decoder is a few nanoseconds of a book message that takes about 520 ns in the window, which is less than this comparison can resolve in ten pairs: the result does not show that the change slows the book, only that a slowdown of 5% was not ruled out. The code stays on the unmerged branch `parse-arrays`.

### Allocation and pooling

No candidate was measured for this part, because the two kept structural changes already pool what the baseline allocated. Allocation counts come from the microbenchmark's counting allocator and are exact, so they need no comparison. They are from the sessions above, each change alone against `209e9e2`:

| Workload | Build | Allocations per pass | Per operation | Bytes per operation |
|---|---|---|---|---|
| `book`, 2,922,290 commands | baseline | 380,227 | 0.130 | 12.0 |
| `book` | linked ring (`4a2bce7`) | 27,920 | 0.0096 | 5.9 |
| `side`, 5,004,039 operations | baseline | 27,760 | 0.0055 | 2.8 |
| `side` | sorted index into a slab (`5d2380b`) | 303 | 0.0001 | 0.2 |
| `orders`, 4,978,788 operations | baseline and hasher | 82 | 0.0000 | 1.7 |

The baseline allocated when a level's `VecDeque` was first filled or grew and when the `BTreeMap` of levels added a node. The ring keeps every queue node in one slab per book with a free list, and the index keeps every level in a slab and reuses the slot of a removed level. Together, in the session `stage3-total` below, the merged code allocated 463 times per pass of the `book` workload. What should be left is the growth of the slabs, the index vectors and the id map up to their peak sizes. Sizing them in advance was not tried.

### Start against final

`BENCHMARKING.md` names one comparison of the starting commit against the final commit as the unbiased figure for the whole exercise. It took two sessions, because the first was planned with a baseline that could not be compared.

Session `stage3-total`, started 14 November 2026 at 06:51 UTC, compared `209e9e2` with `2c792ab` and **produced no verdict**. `compare.sh` refuses to compare runs whose workload checksums differ, and they did: `209e9e2` writes workload format 2 and `2c792ab` format 3. That was a mistake in the plan, not a result. Its twenty `book` runs completed and time the same 2,922,290 commands through the same function, so they are kept here for information: 178.5 ns per command for the baseline and 113.4 ns for the final build, the final build faster in every pair by between 35.2% and 37.4%.

Session `stage3-total-r2`, started 14 November 2026 at 08:01 UTC, is the comparison of record. Its reason was written down before it started: the first session's baseline was on another workload format. The size of the effect was already visible in the refused runs, so this session was not started blind, but nothing about the comparison was changed except the baseline commit. Baseline `f84e022`, the commit on `main` just before the first optimisation: its `src/` is identical to `209e9e2`, the baseline of every round above, and it writes the same workload format as the final build. Final `2c792ab` (the hasher, the linked ring and the sorted index together; later commits on `main` change only tests, comments and this file). Part `book`, `ns_per_op_min`, 10 pairs: the plan was `scripts/kaggle-plans/stage3-total-r2.json`, which differs from `stage3-total.json` only in its name and its base label. Its labels name two local branches, `m-start` at `f84e022` and `m-final` at `2c792ab`; the runner embeds those commit ids in the kernel, and the session's output reports them. Every run reported the workload checksum `33743cd103e868b2`.

| Step | Effect on best pass | Interval | Effect on median pass | Verdict |
|---|---|---|---|---|
| Same binary twice | −0.18% | −0.93% to +0.56% | −0.23% | inconclusive, as it should be |
| Start against final | −38.43% | −38.80% to −38.05% | −37.97% | improved |
| Same binary twice | +0.87% | −0.97% to +2.74% | +0.40% | inconclusive, as it should be |
| Confirming comparison | −34.77% | −37.54% to −31.88% | −33.50% | improved |

No run of these four steps was flagged. The final build was faster in all twenty pairs: by 37.9% to 39.3% in the first comparison and by 29.3% to 39.4% in the confirmation, whose runs spread more. A book command took 172.9 ns in the baseline and 106.6 ns in the final build in the first comparison, and 185.5 ns and 122.0 ns in the confirmation (medians over ten runs of each run's best pass). Allocations per pass of that workload went from 380,226 to 463 (0.130 to 0.0002 per command).

Guardrail: `book_mean_ns` −21.23%, interval −25.69% to −16.50%. The upper end is below +5%, so it passes, and the interval lies wholly below zero: the three changes together are visible end to end. One candidate run was flagged (pair 2, at −10.1%, the smallest gain); the other nine pairs lay between −18.5% and −24.1%. In the window the mean time of a book message was 655.3 ns for the baseline and 507.9 ns for the final build, and its median 499.6 ns and 362.5 ns (medians over the ten runs of each build). The golden replay hash of the final build was `58ff0163…f647`, unchanged, and its tests passed in the session.

**Start against final: a book command takes 38.4% less time (interval 38.1% to 38.8%), confirmed at 34.8% (31.9% to 37.5%), and a book message in the 09:30 to 10:00 window takes 21.2% less time in the mean (16.5% to 25.7%).** The whole gain comes from the three kept changes; the array decoder, which was not kept, is not in the final build.

The whole-day benchmark ran three times for each build, the baseline's three runs first and then the final build's. The runs are not paired, so a drift of the machine between the two blocks is not controlled, and the baseline's three runs spread widely; the control loop's median was 228 ms in both blocks. These numbers are for information.

| Whole day, per message | Baseline `f84e022` (median, range of 3 runs) | Final `2c792ab` (median, range of 3 runs) | Change of the median |
|---|---|---|---|
| p50 | 607.8 ns (566.0 to 646.0) | 414.1 ns (400.9 to 431.9) | −31.9% |
| mean | 813.6 ns (749.2 to 883.3) | 573.3 ns (549.8 to 606.0) | −29.5% |
| p99 | 2,867.1 ns (2,523.4 to 3,156.2) | 2,023.4 ns (1,907.0 to 2,216.1) | −29.4% |
| p99.9 | 11,978.8 ns (11,520.5 to 13,120.7) | 7,934.8 ns (7,796.6 to 8,393.0) | −33.8% |
| throughput | 1,422,333 messages/s (1,242,751 to 1,463,893) | 1,775,456 messages/s (1,411,335 to 1,825,881) | +24.8% |

The first session's whole-day runs, also three for each build and unpaired, gave a median p50 of 576.4 ns for `209e9e2` and 423.6 ns for the final build (−26.5%), and a throughput of 1,376,202 and 1,729,979 messages per second (+25.7%). The two sessions' whole-day changes differ by several points, which is the size of the uncertainty of three unpaired runs.

All absolute numbers here belong to the Kaggle machine and are not comparable with the laptop baseline at the top of this file.

### Instruction counts

Instruction counts from `cachegrind` on the laptop are deterministic and are given as supporting numbers only; they are not timings and decide nothing. One pass of the part, candidate against `209e9e2`: on `orders`, the hasher −21.9% and the flat table −25.1%; on `book`, lazy removal +4.2%, the linked ring −4.2% and the map-entry change −0.8%. The flat table executes fewer instructions than the hasher and was still the slower of the two in time.
