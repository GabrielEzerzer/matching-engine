//! Latency and throughput benchmark: the full ITCH day through one book per instrument, one JSON line out.
//! Run it through scripts/bench.sh or scripts/compare.sh; README.md and BENCHMARKING.md describe the method.

#[path = "../harness/host.rs"]
mod host;
#[path = "../harness/stream.rs"]
mod stream;

use std::hint::black_box;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use host::{Json, control_ms, cpus_allowed, loadavg};
use matching_engine::itch::{Frames, ItchError};
use matching_engine::replay::Replay;
use stream::{ChunkReader, DEFAULT_CHUNK};

const MIN_FRAME: usize = 14;
const TICK_MASK: u32 = (1 << 31) - 1;
const BOOK_BIT: u32 = 1 << 31;

#[cfg(target_arch = "x86_64")]
mod tsc {
    use std::arch::x86_64::{__rdtscp, _mm_lfence, _rdtsc};

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
    window: Option<(u64, u64)>,
    commit: String,
    throughput: bool,
    allow_non_linux: bool,
    file: PathBuf,
}

fn parse_args() -> Result<Args, String> {
    let mut limit = None;
    let mut window = None;
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
            "--window" => {
                let v = args.next().ok_or("--window needs HH:MM:SS-HH:MM:SS")?;
                window = Some(parse_window(&v).ok_or(format!("bad --window {v}"))?);
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
        window,
        commit,
        throughput,
        allow_non_linux,
        file: file.ok_or(
            "usage: bench [--limit N] [--window HH:MM:SS-HH:MM:SS] [--commit LABEL] \
             [--no-throughput] [--allow-non-linux] FILE"
                .to_string(),
        )?,
    })
}

fn parse_window(v: &str) -> Option<(u64, u64)> {
    let ns = |t: &str| -> Option<u64> {
        let mut parts = t.split(':').map(|p| p.parse::<u64>().ok());
        let (h, m, s) = (parts.next()??, parts.next()??, parts.next()??);
        (parts.next().is_none() && h < 24 && m < 60 && s < 60)
            .then(|| ((h * 60 + m) * 60 + s) * 1_000_000_000)
    };
    let (a, b) = v.split_once('-')?;
    let (start, end) = (ns(a)?, ns(b)?);
    (start < end).then_some((start, end))
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
            "refusing to benchmark on {}: results of record come from Linux. \
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
    out.text(
        "window",
        &args.window.map_or("none".to_string(), |(a, b)| {
            let hms = |ns: u64| {
                let s = ns / 1_000_000_000;
                format!("{:02}.{:02}.{:02}", s / 3600, s / 60 % 60, s % 60)
            };
            format!("{}-{}", hms(a), hms(b))
        }),
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
    out.num("warmup_messages", latency.warmup.messages);
    out.float("warmup_s", latency.warmup.wall.as_secs_f64(), 3);
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
    let warm = warm_up(&mut reader, &mut replay, args.window)?;
    let end = args.window.map_or(u64::MAX, |(_, end)| end);
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
            if msg.header().timestamp >= end {
                done = true;
                break;
            }
            let applied = replay.apply(&msg).map_err(|e| e.to_string())?;
            black_box(applied.events.len());
            if replay.stats().messages - warm.messages >= limit {
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
        messages: replay.stats().messages - warm.messages,
        book_messages: replay.stats().book_messages - warm.book_messages,
        busy,
        wall: wall.elapsed(),
    })
}

struct Latency {
    warmup: Warmup,
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
    let warmup = warm_up(&mut reader, &mut replay, args.window)?;
    let end = args.window.map_or(u64::MAX, |(_, end)| end);
    let new_hist =
        || Histogram::<u64>::new_with_bounds(1, u64::from(u32::MAX), 3).expect("valid bounds");
    let mut all = new_hist();
    let mut book = new_hist();
    let (mut all_ticks, mut book_ticks, mut saturated) = (0u64, 0u64, 0u64);
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
            if msg.header().timestamp >= end {
                done = true;
                break;
            }
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
    if messages + warmup.messages != replay.stats().messages {
        return Err("latency pass lost count of messages".into());
    }
    Ok(Latency {
        book_messages: replay.stats().book_messages - warmup.book_messages,
        warmup,
        messages,
        all,
        book,
        all_ticks,
        book_ticks,
        saturated,
        wall: wall.elapsed(),
    })
}

struct Warmup {
    messages: u64,
    book_messages: u64,
    wall: Duration,
}

fn warm_up(
    reader: &mut ChunkReader,
    replay: &mut Replay,
    window: Option<(u64, u64)>,
) -> Result<Warmup, String> {
    let wall = Instant::now();
    if let Some((start, _)) = window {
        'chunks: loop {
            reader.fill().map_err(|e| e.to_string())?;
            if reader.data().is_empty() {
                break;
            }
            let eof = reader.eof();
            let mut frames = Frames::new(reader.data());
            loop {
                let before = frames.offset();
                match frames.next() {
                    None => break,
                    Some(Ok(msg)) if msg.header().timestamp >= start => {
                        reader.consume(before);
                        break 'chunks;
                    }
                    Some(Ok(msg)) => {
                        replay.apply(&msg).map_err(|e| e.to_string())?;
                    }
                    Some(Err(ItchError::Truncated { .. })) if !eof => break,
                    Some(Err(e)) => return Err(e.to_string()),
                }
            }
            let used = frames.offset();
            reader.consume(used);
        }
    }
    Ok(Warmup {
        messages: replay.stats().messages,
        book_messages: replay.stats().book_messages,
        wall: wall.elapsed(),
    })
}

fn tsc_invariant() -> String {
    match std::fs::read_to_string("/proc/cpuinfo") {
        Ok(c) => {
            let flag = |f: &str| c.split_whitespace().any(|w| w == f);
            (flag("constant_tsc") && flag("nonstop_tsc")).to_string()
        }
        Err(_) => "unknown".into(),
    }
}
