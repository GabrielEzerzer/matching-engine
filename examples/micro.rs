//! Microbenchmarks of the book's parts, one part per run, on workloads derived from the real day.
//!
//! ```text
//! micro PART [--passes N] [--commit LABEL] [--allow-non-linux] FILE
//! ```
//!
//! PART is one of:
//! - `side`: price-level lookup (`BookSide`): best price, find, find or create, remove a level;
//! - `level`: the queue at one price (`Level`): push, front, reduce, pop front, remove anywhere;
//! - `orders`: the order-id map (`OrderMap`): insert, get, update, remove;
//! - `book`: the commands applied to `OrderBook`, with allocations counted per command;
//! - `parse`: ITCH frames split and decoded to `Message`, no book.
//!
//! The workload is fixed: the book commands of [`SYMBOLS`] from the start of the day until
//! [`WORKLOAD_END`] (the opening hour of regular trading included), and for `parse` the
//! [`PARSE_BYTES`] of frames that start at [`PARSE_START`]. The operations each part sees are
//! derived by replaying those commands through the real book and watching it from outside
//! (`harness/workload.rs`), so the same workload can drive a rewritten part through its adapter.
//! The first run derives the workload from FILE and caches it in `micro-cache/` beside FILE,
//! keyed by its inputs; later runs load the cache and refuse it unless it encodes back to the
//! same bytes. Preparation is never timed.
//!
//! The timed region is one pass over the part's whole workload from empty state. One untimed
//! pass runs first, then `--passes` timed ones (a default per part keeps a run near fifteen
//! seconds in all), then two more untimed ones. The JSON line on stdout reports ns per
//! operation as the median over the timed passes, the min, the max and the spread
//! ((max - min) / median), the operation counts, the allocations per pass from a counting
//! global allocator, and `workload_checksum`, a SHA-256 prefix of the part's workload: two runs
//! with the same checksum ran the same operations. Allocations are counted in the three untimed
//! passes only, so the timed passes pay nothing for the counting; the line gives their median,
//! min and max (the order map's default hasher is seeded randomly per map, which moves where
//! hashbrown rehashes in place instead of growing, so the count can differ by a few between
//! passes). After each pass the part's final state is checked against the orders, levels and
//! resting shares the derivation recorded.

#[path = "../harness/host.rs"]
mod host;
#[path = "../harness/stream.rs"]
mod stream;
#[path = "../harness/workload.rs"]
mod workload;

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::File;
use std::hint::black_box;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use matching_engine::Command;
use matching_engine::itch::{Frames, ItchError, Message};
use matching_engine::replay::Replay;
use sha2::{Digest, Sha256};

use host::{Json, control_ms, cpus_allowed, loadavg};
use stream::{ChunkReader, DEFAULT_CHUNK};
use workload::{BookIdx, LevelOp, MapOp, Recorder, SideOp, Workload, codec, parts};

/// The busy symbols the book workloads come from, the same set as `examples/diff_slice.rs`.
const SYMBOLS: [&str; 6] = ["AAPL", "MSFT", "AMD", "TSLA", "QQQ", "SPY"];
/// The book workloads stop at the first message at or after 10:30:00 Eastern.
const WORKLOAD_END: u64 = hms(10, 30, 0);
/// The parse workload starts at the first frame at or after the 09:30:00 open.
const PARSE_START: u64 = hms(9, 30, 0);
/// Whole frames up to this many bytes, far more than the caches hold, as in the full replay.
const PARSE_BYTES: u64 = 256 << 20;
/// Bump when the derivation or the encoding changes, so old caches are not reused.
const WORKLOAD_VERSION: u32 = 2;

const fn hms(h: u64, m: u64, s: u64) -> u64 {
    ((h * 60 + m) * 60 + s) * 1_000_000_000
}

const PARTS: [(&str, usize); 5] = [
    ("side", 60),
    ("level", 120),
    ("orders", 60),
    ("book", 20),
    ("parse", 30),
];

/// Counts every allocation and reallocation and the bytes they request while [`COUNTING`] is
/// set, so the benchmark can report allocations per operation exactly. Counting is off during
/// the timed passes, which then pay one relaxed load per allocation and nothing more.
struct Counting;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

fn count(bytes: usize) {
    if COUNTING.load(Relaxed) {
        ALLOCS.fetch_add(1, Relaxed);
        ALLOC_BYTES.fetch_add(bytes as u64, Relaxed);
    }
}

// SAFETY: every method forwards to the system allocator with the caller's arguments unchanged;
// the counters are plain atomics.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        // SAFETY: forwarded unchanged; the caller upholds `alloc`'s contract.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size.saturating_sub(layout.size()));
        // SAFETY: forwarded unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

struct Args {
    part: &'static str,
    passes: usize,
    commit: String,
    allow_non_linux: bool,
    file: PathBuf,
}

const USAGE: &str = "usage: micro side|level|orders|book|parse [--passes N] [--commit LABEL] [--allow-non-linux] FILE";

fn parse_args() -> Result<Args, String> {
    let mut part = None;
    let mut passes = None;
    let mut commit = "unknown".to_string();
    let mut allow_non_linux = false;
    let mut file = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--passes" => {
                let v = args.next().ok_or("--passes needs a number")?;
                passes = Some(
                    v.parse()
                        .ok()
                        .filter(|&n| n > 0)
                        .ok_or(format!("bad --passes {v}"))?,
                );
            }
            "--commit" => commit = args.next().ok_or("--commit needs a label")?,
            "--allow-non-linux" => allow_non_linux = true,
            _ if arg.starts_with("--") => return Err(format!("unknown option {arg}")),
            _ if part.is_none() => {
                part = Some(
                    PARTS
                        .iter()
                        .find(|(name, _)| *name == arg)
                        .ok_or(format!("unknown part {arg}\n{USAGE}"))?,
                );
            }
            _ => file = Some(PathBuf::from(arg)),
        }
    }
    let &(name, default_passes) = part.ok_or(USAGE)?;
    Ok(Args {
        part: name,
        passes: passes.unwrap_or(default_passes),
        commit,
        allow_non_linux,
        file: file.ok_or(USAGE)?,
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
    if !cfg!(target_os = "linux") && !args.allow_non_linux {
        eprintln!(
            "refusing to benchmark on {}: results of record come from Linux (WSL2). \
             Pass --allow-non-linux to run anyway; the output is then labelled.",
            std::env::consts::OS
        );
        return ExitCode::from(2);
    }
    match run(&args) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("micro failed: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &Args) -> Result<String, String> {
    let wall = Instant::now();
    let mut out = Json(Vec::new());
    out.text("commit", &args.commit);
    out.text("part", args.part);
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
    out.text("symbols", &SYMBOLS.join("+"));
    out.text("workload_end", &dotted(WORKLOAD_END));

    let prepared = Instant::now();
    let cached = load_or_derive(&args.file)?;
    out.text("workload_key", &cached.key);
    out.text("cache", cached.source);
    let w = &cached.workload;
    let s = &cached.sections;

    let (checksum, timing, ops) = match args.part {
        "side" => {
            count_side_ops(&mut out, &w.side_ops);
            let t = time_passes(
                args.passes,
                w.side_ops.len(),
                || parts::run_sides(w),
                // Side operations carry no quantities: the levels a side holds stay empty.
                |sides| expect("levels", parts::count_sides(sides), w.final_levels),
            )?;
            (digest(&[&s[0], &s[2]]), t, w.side_ops.len())
        }
        "level" => {
            count_level_ops(&mut out, &w.level_ops);
            out.num("level_slots", w.level_slots);
            let books = parts::push_books(w);
            let t = time_passes(
                args.passes,
                w.level_ops.len(),
                || parts::run_levels(w, &books),
                |levels| {
                    let (orders, live, shares) = parts::count_levels(levels);
                    expect("orders", orders, w.final_orders)?;
                    expect("levels", live, w.final_levels)?;
                    expect("shares", shares, w.final_shares)
                },
            )?;
            (digest(&[&s[0], &s[3]]), t, w.level_ops.len())
        }
        "orders" => {
            count_map_ops(&mut out, &w.map_ops);
            let t = time_passes(
                args.passes,
                w.map_ops.len(),
                || parts::run_maps(w),
                |maps| {
                    let (orders, shares) = parts::count_maps(maps);
                    expect("orders", orders, w.final_orders)?;
                    expect("shares", shares, w.final_shares)
                },
            )?;
            (digest(&[&s[0], &s[4]]), t, w.map_ops.len())
        }
        "book" => {
            count_commands(&mut out, &w.commands);
            let t = time_passes(
                args.passes,
                w.commands.len(),
                || workload::run_books(w),
                |books| {
                    let (orders, levels, shares) = workload::count_books(books);
                    expect("orders", orders, w.final_orders)?;
                    expect("levels", levels, w.final_levels)?;
                    expect("shares", shares, w.final_shares)
                },
            )?;
            (digest(&[&s[0], &s[1]]), t, w.commands.len())
        }
        "parse" => {
            let (offset, len, messages) = cached.parse;
            let bytes = read_range(&args.file, offset, len)?;
            out.num("parse_offset", offset);
            out.num("parse_bytes", len);
            let messages = usize::try_from(messages).map_err(|e| e.to_string())?;
            let t = time_passes(
                args.passes,
                messages,
                || run_parse(&bytes),
                |n| expect("messages", n.clone()?, messages),
            )?;
            let mut range = Vec::new();
            range.extend_from_slice(&offset.to_le_bytes());
            range.extend_from_slice(&len.to_le_bytes());
            (digest(&[&range, &bytes]), t, messages)
        }
        _ => unreachable!("parse_args checks the part"),
    };
    out.float("prepare_s", (timing.started - prepared).as_secs_f64(), 3);
    out.text("workload_checksum", &checksum);
    out.num("ops", ops);
    out.num("passes", timing.ns_per_op.len());

    let mut sorted = timing.ns_per_op.clone();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    let median = if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    };
    out.float("ns_per_op_median", median, 3);
    out.float("ns_per_op_min", sorted[0], 3);
    out.float("ns_per_op_max", sorted[n - 1], 3);
    out.float(
        "spread_pct",
        (sorted[n - 1] - sorted[0]) / median * 100.0,
        2,
    );
    let mut counts: Vec<u64> = timing.allocs.iter().map(|&(a, _)| a).collect();
    let mut bytes: Vec<u64> = timing.allocs.iter().map(|&(_, b)| b).collect();
    counts.sort_unstable();
    bytes.sort_unstable();
    let (allocs, alloc_bytes) = (counts[counts.len() / 2], bytes[bytes.len() / 2]);
    out.num("allocs_per_pass", allocs);
    out.num("allocs_per_pass_min", counts[0]);
    out.num("allocs_per_pass_max", counts[counts.len() - 1]);
    out.num("alloc_bytes_per_pass", alloc_bytes);
    out.float("allocs_per_op", allocs as f64 / ops.max(1) as f64, 4);
    out.float(
        "alloc_bytes_per_op",
        alloc_bytes as f64 / ops.max(1) as f64,
        2,
    );
    out.float("timed_s", timing.timed.as_secs_f64(), 3);
    out.float("loadavg_start", timing.loadavg_start, 2);
    out.float("control_ms_start", timing.control_ms_start, 2);
    out.float("control_ms_end", control_ms(), 2);
    out.float("loadavg_end", loadavg(), 2);
    out.float("wall_s", wall.elapsed().as_secs_f64(), 3);
    Ok(out.render())
}

fn expect<T: PartialEq + std::fmt::Display>(what: &str, got: T, want: T) -> Result<(), String> {
    if got == want {
        Ok(())
    } else {
        Err(format!(
            "after a pass the part holds {got} {what}, the workload ends with {want}"
        ))
    }
}

struct Timing {
    /// When the control loop before the passes started: preparation ends here.
    started: Instant,
    loadavg_start: f64,
    control_ms_start: f64,
    ns_per_op: Vec<f64>,
    timed: Duration,
    /// Allocations and allocated bytes of each counted pass (the untimed ones).
    allocs: Vec<(u64, u64)>,
}

/// Untimed passes that count allocations: the warm pass before the timed ones and the rest after.
const COUNTED_PASSES: usize = 3;

/// One untimed pass, then `passes` timed ones, then [`COUNTED_PASSES`] - 1 more untimed ones.
/// Only the untimed passes count allocations. Each pass starts from empty state; the state it
/// returns is checked and dropped outside the timed region.
fn time_passes<S>(
    passes: usize,
    ops: usize,
    mut pass: impl FnMut() -> S,
    mut check: impl FnMut(&S) -> Result<(), String>,
) -> Result<Timing, String> {
    let started = Instant::now();
    let loadavg_start = loadavg();
    let control_ms_start = control_ms();
    let mut allocs = Vec::with_capacity(COUNTED_PASSES);
    let (warm, a) = counted(&mut pass);
    check(&warm)?;
    drop(warm);
    allocs.push(a);
    let mut ns_per_op = Vec::with_capacity(passes);
    let mut timed = Duration::ZERO;
    for _ in 0..passes {
        let t0 = Instant::now();
        let state = pass();
        let dt = t0.elapsed();
        check(&state)?;
        drop(state);
        timed += dt;
        ns_per_op.push(dt.as_nanos() as f64 / ops.max(1) as f64);
    }
    for _ in 1..COUNTED_PASSES {
        let (state, a) = counted(&mut pass);
        check(&state)?;
        drop(state);
        allocs.push(a);
    }
    Ok(Timing {
        started,
        loadavg_start,
        control_ms_start,
        ns_per_op,
        timed,
        allocs,
    })
}

/// Runs one pass with allocation counting on; returns its state and the allocations and bytes
/// it made.
fn counted<S>(pass: &mut impl FnMut() -> S) -> (S, (u64, u64)) {
    let (a0, b0) = (ALLOCS.load(Relaxed), ALLOC_BYTES.load(Relaxed));
    COUNTING.store(true, Relaxed);
    let state = pass();
    COUNTING.store(false, Relaxed);
    let (a1, b1) = (ALLOCS.load(Relaxed), ALLOC_BYTES.load(Relaxed));
    (state, (a1 - a0, b1 - b0))
}

fn run_parse(bytes: &[u8]) -> Result<usize, String> {
    let mut n = 0;
    for item in Frames::new(bytes) {
        black_box(item.map_err(|e| e.to_string())?);
        n += 1;
    }
    Ok(n)
}

fn count_side_ops(out: &mut Json, ops: &[SideOp]) {
    let mut c = [0usize; 4];
    for op in ops {
        c[match op {
            SideOp::Best { .. } => 0,
            SideOp::Find { .. } => 1,
            SideOp::FindOrCreate { .. } => 2,
            SideOp::Remove { .. } => 3,
        }] += 1;
    }
    for (name, n) in ["best", "find", "find_or_create", "remove"].iter().zip(c) {
        out.num(&format!("ops_{name}"), n);
    }
}

fn count_level_ops(out: &mut Json, ops: &[LevelOp]) {
    let mut c = [0usize; 6];
    for op in ops {
        c[match op {
            LevelOp::Push { .. } => 0,
            LevelOp::Front { .. } => 1,
            LevelOp::Reduce { .. } => 2,
            LevelOp::PopFront { .. } => 3,
            LevelOp::Remove { .. } => 4,
            LevelOp::Reset { .. } => 5,
        }] += 1;
    }
    let names = ["push", "front", "reduce", "pop_front", "remove", "reset"];
    for (name, n) in names.iter().zip(c) {
        out.num(&format!("ops_{name}"), n);
    }
}

fn count_map_ops(out: &mut Json, ops: &[MapOp]) {
    let mut c = [0usize; 4];
    for op in ops {
        c[match op {
            MapOp::Insert { .. } => 0,
            MapOp::Get { .. } => 1,
            MapOp::Update { .. } => 2,
            MapOp::Remove { .. } => 3,
        }] += 1;
    }
    for (name, n) in ["insert", "get", "update", "remove"].iter().zip(c) {
        out.num(&format!("ops_{name}"), n);
    }
}

fn count_commands(out: &mut Json, commands: &[(BookIdx, Command)]) {
    let mut c = [0usize; 5];
    for (_, cmd) in commands {
        c[match cmd {
            Command::Limit { .. } => 0,
            Command::Market { .. } => 1,
            Command::Cancel { .. } => 2,
            Command::Reduce { .. } => 3,
            Command::Replace { .. } => 4,
        }] += 1;
    }
    let names = ["limit", "market", "cancel", "reduce", "replace"];
    for (name, n) in names.iter().zip(c) {
        out.num(&format!("ops_{name}"), n);
    }
}

/// `HH.MM.SS`; colons would break the scripts' split of the flat JSON line.
fn dotted(ns: u64) -> String {
    let s = ns / 1_000_000_000;
    format!("{:02}.{:02}.{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn digest(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    h.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn read_range(file: &Path, offset: u64, len: u64) -> Result<Vec<u8>, String> {
    let mut f = File::open(file).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    let mut bytes = vec![0u8; usize::try_from(len).map_err(|e| e.to_string())?];
    f.read_exact(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes)
}

struct Cached {
    key: String,
    source: &'static str,
    workload: Workload,
    sections: [Vec<u8>; 5],
    /// Offset, length and message count of the parse workload's byte range in FILE.
    parse: (u64, u64, u64),
}

const MAGIC: &[u8; 4] = b"MEWL";

/// Loads the derived workload from `micro-cache/<key>.bin` beside `file`, or derives it and
/// writes the cache. The key covers the file's name and size and every workload constant.
fn load_or_derive(file: &Path) -> Result<Cached, String> {
    let size = std::fs::metadata(file)
        .map_err(|e| format!("{}: {e}", file.display()))?
        .len();
    let inputs = format!(
        "v{WORKLOAD_VERSION} {} {size} {} {WORKLOAD_END} {PARSE_START} {PARSE_BYTES}",
        file.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default(),
        SYMBOLS.join("+"),
    );
    let key = digest(&[inputs.as_bytes()]);
    let dir = file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("micro-cache");
    let path = dir.join(format!("{key}.bin"));
    if let Ok(bytes) = std::fs::read(&path) {
        let (sections, parse) =
            read_cache(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        let workload = codec::decode(&sections)?;
        // A changed codec could otherwise decode other operations under the same checksum.
        if codec::encode(&workload) != sections {
            return Err(format!(
                "{}: the cached workload does not encode back to the same bytes; \
                 delete it or bump WORKLOAD_VERSION",
                path.display()
            ));
        }
        return Ok(Cached {
            key,
            source: "loaded",
            workload,
            sections,
            parse,
        });
    }
    eprintln!(
        "micro: deriving the workload from {} (cached afterwards)",
        file.display()
    );
    let (workload, parse) = derive(file)?;
    let sections = codec::encode(&workload);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&WORKLOAD_VERSION.to_le_bytes());
    for v in [parse.0, parse.1, parse.2] {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    for s in &sections {
        bytes.extend_from_slice(&(s.len() as u64).to_le_bytes());
        bytes.extend_from_slice(s);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!("{key}.bin.tmp{}", std::process::id()));
    std::fs::write(&tmp, &bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Cached {
        key,
        source: "derived",
        workload,
        sections,
        parse,
    })
}

type CacheParts = ([Vec<u8>; 5], (u64, u64, u64));

fn read_cache(bytes: &[u8]) -> Result<CacheParts, String> {
    let mut at = 0usize;
    let mut take = |n: usize| -> Result<&[u8], String> {
        let s = bytes.get(at..at + n).ok_or("cache file ends early")?;
        at += n;
        Ok(s)
    };
    if take(4)? != MAGIC || take(4)? != WORKLOAD_VERSION.to_le_bytes() {
        return Err("not a workload cache of this version".into());
    }
    let mut u64s = [0u64; 3];
    for v in &mut u64s {
        *v = u64::from_le_bytes(take(8)?.try_into().expect("8 bytes"));
    }
    let parse = (u64s[0], u64s[1], u64s[2]);
    let mut sections: [Vec<u8>; 5] = Default::default();
    for s in &mut sections {
        let n = u64::from_le_bytes(take(8)?.try_into().expect("8 bytes"));
        *s = take(usize::try_from(n).map_err(|e| e.to_string())?)?.to_vec();
    }
    if at != bytes.len() {
        return Err("trailing bytes in cache file".into());
    }
    Ok((sections, parse))
}

/// Replays the book messages of [`SYMBOLS`] up to [`WORKLOAD_END`] through one book per symbol
/// and records each part's operations; finds the parse byte range on the way.
fn derive(file: &Path) -> Result<(Workload, (u64, u64, u64)), String> {
    let started = Instant::now();
    let mut reader = ChunkReader::open(file, DEFAULT_CHUNK).map_err(|e| e.to_string())?;
    let symbols: Vec<[u8; 8]> = SYMBOLS
        .iter()
        .map(|s| {
            let mut padded = *b"        ";
            padded[..s.len()].copy_from_slice(s.as_bytes());
            padded
        })
        .collect();
    let mut replay = Replay::new();
    let mut recorder = Recorder::new(SYMBOLS.len());
    let mut book_of: Vec<Option<BookIdx>> = vec![None; 1 << 16];
    let mut events = Vec::new();
    let mut consumed = 0u64;
    let mut recording = true;
    let (mut parse_start, mut parse_end, mut parse_messages) = (None, None, 0u64);
    'chunks: loop {
        reader.fill().map_err(|e| e.to_string())?;
        if reader.data().is_empty() {
            break;
        }
        let eof = reader.eof();
        let mut frames = Frames::new(reader.data());
        loop {
            let at = consumed + frames.offset() as u64;
            let msg = match frames.next() {
                None => break,
                Some(Ok(msg)) => msg,
                Some(Err(ItchError::Truncated { .. })) if !eof => break,
                Some(Err(e)) => return Err(e.to_string()),
            };
            let next = consumed + frames.offset() as u64;
            let ts = msg.header().timestamp;
            if parse_start.is_none() && ts >= PARSE_START {
                parse_start = Some(at);
            }
            if let Some(start) = parse_start
                && parse_end.is_none()
            {
                if next - start > PARSE_BYTES {
                    parse_end = Some(at);
                } else {
                    parse_messages += 1;
                }
            }
            if recording && ts >= WORKLOAD_END {
                recording = false;
            }
            if !recording {
                if parse_end.is_some() {
                    break 'chunks;
                }
                continue;
            }
            let locate = msg.header().locate;
            if let Message::StockDirectory { stock, .. } = msg
                && let Some(i) = symbols.iter().position(|s| *s == stock)
            {
                book_of[usize::from(locate)] = Some(i as BookIdx);
            }
            let Some(b) = book_of[usize::from(locate)] else {
                continue;
            };
            let named_ref = match msg {
                Message::OrderDelete { reference, .. }
                | Message::OrderCancel { reference, .. }
                | Message::OrderExecuted { reference, .. } => Some(reference),
                Message::OrderReplace {
                    original_reference, ..
                } => Some(original_reference),
                _ => None,
            };
            let named = named_ref.and_then(|r| replay.book(locate).and_then(|bk| bk.order(r)));
            let applied = replay.apply(&msg).map_err(|e| e.to_string())?;
            let Some(cmd) = applied.command else {
                continue;
            };
            events.clear();
            events.extend_from_slice(applied.events);
            let book = replay
                .book(locate)
                .expect("a book exists after its first command");
            recorder.record(b, cmd, named, &events, book);
        }
        let used = frames.offset();
        reader.consume(used);
        consumed += used as u64;
    }
    if book_of.iter().flatten().count() != SYMBOLS.len() {
        return Err("not every symbol has a stock directory message".into());
    }
    let start = parse_start.ok_or("the file has no message after the parse start")?;
    let end = parse_end.unwrap_or(consumed);
    let w = recorder.finish();
    eprintln!(
        "micro: derived {} commands, {} side, {} level and {} map operations in {:.1} s",
        w.commands.len(),
        w.side_ops.len(),
        w.level_ops.len(),
        w.map_ops.len(),
        started.elapsed().as_secs_f64()
    );
    Ok((w, (start, end - start, parse_messages)))
}
