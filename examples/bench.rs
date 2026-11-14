//! Latency and throughput benchmark: the full ITCH day through one book per instrument.
//!
//! ```text
//! bench [--limit N] [--commit LABEL] [--no-throughput] [--allow-non-linux] FILE
//! ```
//!
//! Prints one JSON line. Run it through `scripts/bench.sh`, which pins it to one CPU, repeats it
//! and summarises the runs, or through `scripts/compare.sh`, which compares two builds in pairs.
//!
//! What is timed, per ITCH message: splitting its frame off the buffer, decoding it, and applying
//! it to the replay (the driver's command choice, the book's work, the driver's counters). The
//! file is read in 64 MiB chunks into a buffer that was written once before any timing, so no
//! file I/O, decompression or page fault of the input happens inside a timed region. Nothing is
//! hashed. The events the book emits go into a `Vec` the driver reuses, so the sink allocates
//! only while it first grows. The book's own allocations are part of what is measured.
//!
//! Two passes, each over the whole file with a fresh replay (`--no-throughput` skips the first,
//! and the output then has no `throughput_*` fields):
//! - Throughput: no per-message timer. Each chunk's parse-and-apply loop is timed as a whole with
//!   `Instant`, and throughput is messages over the summed time.
//! - Latency: each message is timed with the TSC (`lfence; rdtsc; lfence` before,
//!   `rdtscp; lfence` after). The raw tick counts go into a preallocated array; they are added
//!   to the HDR histograms after the chunk, outside any timed region. Reported values include
//!   the timer's own cost, which is measured and printed next to them (`timer_overhead_ns_*`).
//!
//! Under WSL2 the hypervisor does not report steal time, so a run slowed by other work on the
//! host cannot be seen from inside. Each run therefore times a fixed CPU-bound loop before and
//! after its passes (`control_ms_start`, `control_ms_end`); a run whose control time is out of
//! line with the others ran on a contended or slower CPU.

#[path = "../harness/stream.rs"]
mod stream;

use std::hint::black_box;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use matching_engine::itch::{Frames, ItchError};
use matching_engine::replay::Replay;
use stream::{ChunkReader, DEFAULT_CHUNK};

/// The smallest ITCH 5.0 frame: a 2-byte length and a 12-byte message.
const MIN_FRAME: usize = 14;
/// Per-message tick counts are stored in 31 bits; the top bit marks book messages.
const TICK_MASK: u32 = (1 << 31) - 1;
const BOOK_BIT: u32 = 1 << 31;

#[cfg(target_arch = "x86_64")]
mod tsc {
    use std::arch::x86_64::{__rdtscp, _mm_lfence, _rdtsc};

    /// Reads the TSC after all earlier instructions have completed and before later ones start.
    #[inline(always)]
    pub fn start() -> u64 {
        // SAFETY: lfence and rdtsc are available on every x86_64 CPU and touch no memory.
        unsafe {
            _mm_lfence();
            let t = _rdtsc();
            _mm_lfence();
            t
        }
    }

    /// Reads the TSC after all earlier instructions have completed (rdtscp waits for them),
    /// and keeps later instructions from starting before the read.
    #[inline(always)]
    pub fn stop() -> u64 {
        let mut aux = 0u32;
        // SAFETY: rdtscp is checked for at startup (`supported`); it writes only `aux`.
        unsafe {
            let t = __rdtscp(&mut aux);
            _mm_lfence();
            t
        }
    }

    pub fn supported() -> bool {
        std::arch::is_x86_feature_detected!("sse2")
            && std::fs::read_to_string("/proc/cpuinfo")
                .map(|c| c.split_whitespace().any(|w| w == "rdtscp"))
                .unwrap_or(true)
    }
}

#[cfg(not(target_arch = "x86_64"))]
mod tsc {
    pub fn start() -> u64 {
        unimplemented!("the benchmark's timer needs x86_64")
    }
    pub fn stop() -> u64 {
        unimplemented!("the benchmark's timer needs x86_64")
    }
    pub fn supported() -> bool {
        false
    }
}

struct Args {
    limit: Option<u64>,
    commit: String,
    throughput: bool,
    allow_non_linux: bool,
    file: PathBuf,
}

fn parse_args() -> Result<Args, String> {
    let mut limit = None;
    let mut commit = "unknown".to_string();
    let mut throughput = true;
    let mut allow_non_linux = false;
    let mut file = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--limit" => {
                let v = args.next().ok_or("--limit needs a number")?;
                limit = Some(v.parse().map_err(|_| format!("bad --limit {v}"))?);
            }
            "--commit" => commit = args.next().ok_or("--commit needs a label")?,
            "--no-throughput" => throughput = false,
            "--allow-non-linux" => allow_non_linux = true,
            _ if arg.starts_with("--") => return Err(format!("unknown option {arg}")),
            _ => file = Some(PathBuf::from(arg)),
        }
    }
    Ok(Args {
        limit,
        commit,
        throughput,
        allow_non_linux,
        file: file.ok_or(
            "usage: bench [--limit N] [--commit LABEL] [--no-throughput] [--allow-non-linux] FILE"
                .to_string(),
        )?,
    })
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let linux = cfg!(target_os = "linux");
    if !linux && !args.allow_non_linux {
        eprintln!(
            "refusing to benchmark on {}: results of record come from Linux (WSL2). \
             Pass --allow-non-linux to run anyway; the output is then labelled.",
            std::env::consts::OS
        );
        return ExitCode::from(2);
    }
    if !tsc::supported() {
        eprintln!("this benchmark needs an x86_64 CPU with rdtscp");
        return ExitCode::from(2);
    }
    match run(&args) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("bench failed: {e}");
            ExitCode::from(2)
        }
    }
}

/// Fields of the output line, in order.
struct Json(Vec<(String, String)>);

impl Json {
    fn num(&mut self, k: &str, v: impl std::fmt::Display) {
        self.0.push((k.to_string(), v.to_string()));
    }
    fn float(&mut self, k: &str, v: f64, decimals: usize) {
        self.0.push((k.to_string(), format!("{v:.decimals$}")));
    }
    fn text(&mut self, k: &str, v: &str) {
        let escaped: String = v
            .chars()
            .flat_map(|c| match c {
                '"' => vec!['\\', '"'],
                '\\' => vec!['\\', '\\'],
                c if c.is_control() => vec![],
                c => vec![c],
            })
            .collect();
        self.0.push((k.to_string(), format!("\"{escaped}\"")));
    }
    fn render(&self) -> String {
        let body: Vec<String> = self.0.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect();
        format!("{{{}}}", body.join(","))
    }
}

fn run(args: &Args) -> Result<String, String> {
    let mut out = Json(Vec::new());
    out.text("commit", &args.commit);
    out.text("os", std::env::consts::OS);
    out.text(
        "result_of_record",
        if cfg!(target_os = "linux") {
            "eligible"
        } else {
            "NOT-LINUX"
        },
    );
    out.text("cpus_allowed", &cpus_allowed());
    out.text(
        "file",
        &args
            .file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    out.text(
        "limit",
        &args.limit.map_or("none".to_string(), |l| l.to_string()),
    );
    out.float("loadavg_start", loadavg(), 2);
    out.float("control_ms_start", control_ms(), 2);

    let tsc_hz = calibrate_tsc();
    let ns_per_tick = 1e9 / tsc_hz;
    out.float("tsc_ghz", tsc_hz / 1e9, 4);
    out.text("tsc_invariant", &tsc_invariant());
    let timer = timer_cost();
    out.float("tsc_tick_ns", ns_per_tick, 4);
    out.float(
        "timer_overhead_ns_min",
        timer.tsc_min as f64 * ns_per_tick,
        2,
    );
    out.float(
        "timer_overhead_ns_p50",
        timer.tsc_p50 as f64 * ns_per_tick,
        2,
    );
    out.float(
        "timer_overhead_ns_p99",
        timer.tsc_p99 as f64 * ns_per_tick,
        2,
    );
    out.float("instant_overhead_ns_min", timer.instant_min, 2);
    out.float("instant_overhead_ns_p50", timer.instant_p50, 2);
    out.float("instant_resolution_ns", timer.instant_resolution, 2);

    let throughput = if args.throughput {
        Some(throughput_pass(args)?)
    } else {
        None
    };
    let latency = latency_pass(args)?;
    out.num("messages", latency.messages);
    out.num("book_messages", latency.book_messages);
    if let Some(throughput) = throughput {
        if (throughput.messages, throughput.book_messages)
            != (latency.messages, latency.book_messages)
        {
            return Err(format!(
                "passes disagree on the message counts: {} and {} vs {} and {}",
                throughput.messages,
                throughput.book_messages,
                latency.messages,
                latency.book_messages
            ));
        }
        out.float(
            "throughput_msgs_per_s",
            throughput.messages as f64 / throughput.busy.as_secs_f64(),
            0,
        );
        out.float("throughput_busy_s", throughput.busy.as_secs_f64(), 3);
        out.float("throughput_wall_s", throughput.wall.as_secs_f64(), 3);
    }
    for (name, hist, sum) in [
        ("all", &latency.all, latency.all_ticks),
        ("book", &latency.book, latency.book_ticks),
    ] {
        let ns = |ticks: u64| ticks as f64 * ns_per_tick;
        out.num(&format!("{name}_count"), hist.len());
        out.float(
            &format!("{name}_mean_ns"),
            sum as f64 / hist.len().max(1) as f64 * ns_per_tick,
            2,
        );
        out.float(
            &format!("{name}_p50_ns"),
            ns(hist.value_at_quantile(0.5)),
            1,
        );
        out.float(
            &format!("{name}_p99_ns"),
            ns(hist.value_at_quantile(0.99)),
            1,
        );
        out.float(
            &format!("{name}_p999_ns"),
            ns(hist.value_at_quantile(0.999)),
            1,
        );
        out.float(&format!("{name}_max_ns"), ns(hist.max()), 1);
    }
    out.num("latency_saturated", latency.saturated);
    out.float("latency_wall_s", latency.wall.as_secs_f64(), 3);
    out.float("control_ms_end", control_ms(), 2);
    out.float("loadavg_end", loadavg(), 2);
    Ok(out.render())
}

/// Measures TSC ticks per second against the monotonic clock over three 100 ms windows and
/// takes the median.
fn calibrate_tsc() -> f64 {
    let mut rates: Vec<f64> = (0..3)
        .map(|_| {
            let t0 = Instant::now();
            let c0 = tsc::start();
            while t0.elapsed() < Duration::from_millis(100) {
                std::hint::spin_loop();
            }
            let c1 = tsc::stop();
            let elapsed = t0.elapsed().as_secs_f64();
            (c1 - c0) as f64 / elapsed
        })
        .collect();
    rates.sort_by(f64::total_cmp);
    rates[1]
}

struct TimerCost {
    tsc_min: u64,
    tsc_p50: u64,
    tsc_p99: u64,
    instant_min: f64,
    instant_p50: f64,
    instant_resolution: f64,
}

/// The cost of an empty timed region with each timer, from a million back-to-back pairs, and
/// the smallest non-zero step `Instant` reports.
fn timer_cost() -> TimerCost {
    const N: usize = 1_000_000;
    let mut tsc: Vec<u64> = (0..N)
        .map(|_| {
            let a = tsc::start();
            let b = tsc::stop();
            b - a
        })
        .collect();
    tsc.sort_unstable();
    let mut instant: Vec<f64> = (0..N)
        .map(|_| {
            let a = Instant::now();
            let b = black_box(Instant::now());
            (b - a).as_nanos() as f64
        })
        .collect();
    instant.sort_by(f64::total_cmp);
    let resolution = instant
        .iter()
        .copied()
        .find(|&d| d > 0.0)
        .unwrap_or(f64::NAN);
    TimerCost {
        tsc_min: tsc[0],
        tsc_p50: tsc[N / 2],
        tsc_p99: tsc[N * 99 / 100],
        instant_min: instant[0],
        instant_p50: instant[N / 2],
        instant_resolution: resolution,
    }
}

struct Throughput {
    messages: u64,
    book_messages: u64,
    busy: Duration,
    wall: Duration,
}

fn throughput_pass(args: &Args) -> Result<Throughput, String> {
    let wall = Instant::now();
    let mut reader = ChunkReader::open(&args.file, DEFAULT_CHUNK).map_err(|e| e.to_string())?;
    let mut replay = Replay::new();
    let mut busy = Duration::ZERO;
    let limit = args.limit.unwrap_or(u64::MAX);
    loop {
        reader.fill().map_err(|e| e.to_string())?;
        if reader.data().is_empty() {
            break;
        }
        let eof = reader.eof();
        let mut frames = Frames::new(reader.data());
        let mut done = false;
        let t0 = Instant::now();
        for item in frames.by_ref() {
            let msg = match item {
                Ok(msg) => msg,
                Err(ItchError::Truncated { .. }) if !eof => break,
                Err(e) => return Err(e.to_string()),
            };
            let applied = replay.apply(&msg).map_err(|e| e.to_string())?;
            black_box(applied.events.len());
            if replay.stats().messages >= limit {
                done = true;
                break;
            }
        }
        busy += t0.elapsed();
        let used = frames.offset();
        reader.consume(used);
        if done {
            break;
        }
    }
    Ok(Throughput {
        messages: replay.stats().messages,
        book_messages: replay.stats().book_messages,
        busy,
        wall: wall.elapsed(),
    })
}

struct Latency {
    messages: u64,
    book_messages: u64,
    all: Histogram<u64>,
    book: Histogram<u64>,
    all_ticks: u64,
    book_ticks: u64,
    saturated: u64,
    wall: Duration,
}

fn latency_pass(args: &Args) -> Result<Latency, String> {
    let wall = Instant::now();
    let mut reader = ChunkReader::open(&args.file, DEFAULT_CHUNK).map_err(|e| e.to_string())?;
    let mut replay = Replay::new();
    let new_hist =
        || Histogram::<u64>::new_with_bounds(1, u64::from(u32::MAX), 3).expect("valid bounds");
    let mut all = new_hist();
    let mut book = new_hist();
    let (mut all_ticks, mut book_ticks, mut saturated) = (0u64, 0u64, 0u64);
    // One slot per possible frame in a chunk. vec! of zeros is mapped lazily, so write every
    // slot now to fault the pages in before the timed loop; black_box keeps the write.
    let mut timings: Vec<u32> = vec![0; DEFAULT_CHUNK / MIN_FRAME + 1];
    timings.fill(1);
    std::hint::black_box(&mut timings);
    let limit = args.limit.unwrap_or(u64::MAX);
    let mut messages = 0u64;
    loop {
        reader.fill().map_err(|e| e.to_string())?;
        if reader.data().is_empty() {
            break;
        }
        let eof = reader.eof();
        let mut frames = Frames::new(reader.data());
        let mut n = 0usize;
        let mut done = false;
        loop {
            let t0 = tsc::start();
            let Some(item) = frames.next() else { break };
            let msg = match item {
                Ok(msg) => msg,
                Err(ItchError::Truncated { .. }) if !eof => break,
                Err(e) => return Err(e.to_string()),
            };
            let applied = replay.apply(&msg).map_err(|e| e.to_string())?;
            let is_book = applied.command.is_some();
            black_box(applied.events.len());
            let t1 = tsc::stop();
            let ticks = t1.wrapping_sub(t0);
            let stored = if ticks > u64::from(TICK_MASK) {
                saturated += 1;
                TICK_MASK
            } else {
                ticks as u32
            };
            timings[n] = stored | if is_book { BOOK_BIT } else { 0 };
            n += 1;
            if messages + n as u64 >= limit {
                done = true;
                break;
            }
        }
        messages += n as u64;
        for &t in &timings[..n] {
            let ticks = u64::from(t & TICK_MASK);
            all.saturating_record(ticks);
            all_ticks += ticks;
            if t & BOOK_BIT != 0 {
                book.saturating_record(ticks);
                book_ticks += ticks;
            }
        }
        let used = frames.offset();
        reader.consume(used);
        if done {
            break;
        }
    }
    if messages != replay.stats().messages {
        return Err("latency pass lost count of messages".into());
    }
    Ok(Latency {
        messages,
        book_messages: replay.stats().book_messages,
        all,
        book,
        all_ticks,
        book_ticks,
        saturated,
        wall: wall.elapsed(),
    })
}

/// Time for a fixed chain of 100 million dependent xorshift steps: a CPU-only workload with no
/// memory traffic, so it varies only with the CPU's clock and with contention for the core.
fn control_ms() -> f64 {
    let t = Instant::now();
    let mut x = black_box(0x9E37_79B9_7F4A_7C15u64);
    for _ in 0..100_000_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
    }
    black_box(x);
    t.elapsed().as_secs_f64() * 1e3
}

fn cpus_allowed() -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix("Cpus_allowed_list:")
                    .map(|v| v.trim().to_string())
            })
        })
        .unwrap_or_else(|| "unknown".into())
}

fn loadavg() -> f64 {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok()))
        .unwrap_or(f64::NAN)
}

/// Whether the kernel reports an invariant TSC (`constant_tsc` and `nonstop_tsc`).
fn tsc_invariant() -> String {
    match std::fs::read_to_string("/proc/cpuinfo") {
        Ok(c) => {
            let flag = |f: &str| c.split_whitespace().any(|w| w == f);
            (flag("constant_tsc") && flag("nonstop_tsc")).to_string()
        }
        Err(_) => "unknown".into(),
    }
}
