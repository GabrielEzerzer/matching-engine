# Performance log

This file records the baseline and every optimisation round, kept or reverted, with its numbers. How the numbers are measured, and the rule that decides whether a change is kept, are described in the README under "Benchmark" and "Procedure for optimisation rounds".

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

None yet.
