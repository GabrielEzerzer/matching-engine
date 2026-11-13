//! Feeds real ITCH messages for a set of symbols through both the real book and the slow
//! reference book (`harness/reference.rs`) and requires identical events for every command, plus
//! identical snapshots and the real book's invariants every `--snapshot-every` commands per book
//! and at the end. The reference is O(resting orders) per operation, so the slice is bounded by
//! the symbol set and an optional message limit.
//!
//! ```text
//! diff_slice [--symbols A,B,...] [--limit N] [--snapshot-every K] FILE
//! ```
//!
//! `--limit` counts book commands of the chosen symbols (default 5,000,000, which takes about
//! two and a half minutes in release mode; `--limit 0` runs the whole day). Exits with status 1
//! on the first difference, printing the command and both event lists.

#[path = "../harness/reference.rs"]
mod reference;
#[path = "../harness/stream.rs"]
mod stream;

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use matching_engine::itch::{Frames, ItchError, Locate, Message};
use matching_engine::replay::{Replay, TAKER_ID_BASE};
use matching_engine::{Command, OrderBook};
use reference::ReferenceBook;
use stream::{ChunkReader, DEFAULT_CHUNK};

/// Busy Nasdaq symbols with deep books on the sample day; see the README for the measured
/// coverage and runtime of this default.
const DEFAULT_SYMBOLS: &str = "AAPL,MSFT,AMD,TSLA,QQQ,SPY";
const DEFAULT_SNAPSHOT_EVERY: u64 = 200;
const DEFAULT_LIMIT: u64 = 5_000_000;

struct Args {
    symbols: Vec<[u8; 8]>,
    limit: Option<u64>,
    snapshot_every: u64,
    file: PathBuf,
}

fn parse_args() -> Result<Args, String> {
    let mut symbols = DEFAULT_SYMBOLS.to_string();
    let mut limit = Some(DEFAULT_LIMIT);
    let mut snapshot_every = DEFAULT_SNAPSHOT_EVERY;
    let mut file = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--symbols" => symbols = value("--symbols")?,
            "--limit" => {
                let v = value("--limit")?;
                let n: u64 = v.parse().map_err(|_| format!("bad --limit {v}"))?;
                limit = (n > 0).then_some(n);
            }
            "--snapshot-every" => {
                let v = value("--snapshot-every")?;
                snapshot_every = v
                    .parse()
                    .ok()
                    .filter(|&k| k > 0)
                    .ok_or(format!("bad --snapshot-every {v}"))?;
            }
            _ if arg.starts_with("--") => return Err(format!("unknown option {arg}")),
            _ => file = Some(PathBuf::from(arg)),
        }
    }
    let symbols = symbols
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let s = s.trim().as_bytes();
            if s.len() > 8 {
                return Err(format!("symbol too long: {}", String::from_utf8_lossy(s)));
            }
            let mut padded = *b"        ";
            padded[..s.len()].copy_from_slice(s);
            Ok(padded)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Args {
        symbols,
        limit,
        snapshot_every,
        file: file
            .ok_or("usage: diff_slice [--symbols A,B,...] [--limit N] [--snapshot-every K] FILE")?,
    })
}

/// Per-book counters for the report.
#[derive(Default)]
struct Coverage {
    commands: u64,
    events: u64,
    trades: u64,
    snapshots: u64,
    orders_added: u64,
    max_orders: usize,
    max_levels: usize,
    max_queue: usize,
}

struct Pair {
    reference: ReferenceBook,
    coverage: Coverage,
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Diverged(msg)) => {
            eprintln!("DIFFERENCE: {msg}");
            ExitCode::from(1)
        }
        Err(Failure::Other(msg)) => {
            eprintln!("diff_slice failed: {msg}");
            ExitCode::from(2)
        }
    }
}

enum Failure {
    Diverged(String),
    Other(String),
}

fn run(args: &Args) -> Result<(), Failure> {
    let started = Instant::now();
    let other = |e: std::io::Error| Failure::Other(e.to_string());
    let mut reader = ChunkReader::open(&args.file, DEFAULT_CHUNK).map_err(other)?;
    let mut replay = Replay::new();
    let mut pairs: BTreeMap<Locate, Pair> = BTreeMap::new();
    let mut selected: HashSet<Locate> = HashSet::new();
    let mut scanned = 0u64;
    let mut compared = 0u64;
    let mut reference_events = Vec::new();
    'file: loop {
        reader.fill().map_err(other)?;
        let data = reader.data();
        if data.is_empty() {
            break;
        }
        let mut frames = Frames::new(data);
        for item in frames.by_ref() {
            let msg = match item {
                Ok(msg) => msg,
                Err(ItchError::Truncated { .. }) if !reader.eof() => break,
                Err(e) => return Err(Failure::Other(format!("bad frame: {e}"))),
            };
            scanned += 1;
            let locate = msg.header().locate;
            if let Message::StockDirectory { stock, .. } = msg
                && args.symbols.contains(&stock)
            {
                selected.insert(locate);
                pairs.entry(locate).or_insert_with(|| Pair {
                    reference: ReferenceBook::new(),
                    coverage: Coverage::default(),
                });
            }
            let is_book = matches!(
                msg,
                Message::AddOrder { .. }
                    | Message::OrderExecuted { .. }
                    | Message::OrderCancel { .. }
                    | Message::OrderDelete { .. }
                    | Message::OrderReplace { .. }
            );
            if is_book && !selected.contains(&locate) {
                continue;
            }
            let applied = replay
                .apply(&msg)
                .map_err(|e| Failure::Other(e.to_string()))?;
            let Some(cmd) = applied.command else {
                continue;
            };
            let pair = pairs
                .get_mut(&locate)
                .expect("selected locates have a pair");
            reference_events.clear();
            pair.reference.apply(cmd, &mut reference_events);
            if applied.events != reference_events.as_slice() {
                return Err(Failure::Diverged(format!(
                    "locate {locate} message {scanned} {msg:?}\n  command {cmd:?}\n  book      {:?}\n  reference {:?}",
                    applied.events, reference_events
                )));
            }
            let c = &mut pair.coverage;
            c.commands += 1;
            c.events += applied.events.len() as u64;
            c.trades += applied
                .events
                .iter()
                .filter(|e| matches!(e, matching_engine::Event::Trade { .. }))
                .count() as u64;
            match cmd {
                Command::Limit { id, .. } if id < TAKER_ID_BASE => c.orders_added += 1,
                Command::Replace { .. } => c.orders_added += 1,
                _ => {}
            }
            compared += 1;
            if c.commands.is_multiple_of(args.snapshot_every) {
                let book = replay.book(locate).expect("book exists");
                compare_snapshots(locate, book, pair, scanned)?;
            }
            if args.limit.is_some_and(|l| compared >= l) {
                break 'file;
            }
        }
        let used = frames.offset();
        reader.consume(used);
    }
    for (&locate, pair) in pairs.iter_mut() {
        if let Some(book) = replay.book(locate) {
            compare_snapshots(locate, book, pair, scanned)?;
        }
    }
    report(&replay, &pairs, scanned, compared, started);
    Ok(())
}

fn compare_snapshots(
    locate: Locate,
    book: &OrderBook,
    pair: &mut Pair,
    scanned: u64,
) -> Result<(), Failure> {
    let snapshot = book.snapshot();
    if snapshot != pair.reference.snapshot() {
        return Err(Failure::Diverged(format!(
            "locate {locate}: snapshots differ after message {scanned}"
        )));
    }
    if let Err(e) = book.check_invariants() {
        return Err(Failure::Diverged(format!(
            "locate {locate}: invariant broken after message {scanned}: {e}"
        )));
    }
    let c = &mut pair.coverage;
    c.snapshots += 1;
    let levels = snapshot.bids.iter().chain(&snapshot.asks);
    c.max_levels = c.max_levels.max(snapshot.bids.len() + snapshot.asks.len());
    c.max_orders = c
        .max_orders
        .max(levels.clone().map(|l| l.orders.len()).sum());
    c.max_queue = c
        .max_queue
        .max(levels.map(|l| l.orders.len()).max().unwrap_or(0));
    Ok(())
}

fn report(
    replay: &Replay,
    pairs: &BTreeMap<Locate, Pair>,
    scanned: u64,
    compared: u64,
    started: Instant,
) {
    println!(
        "{:<8} {:>6} {:>10} {:>10} {:>9} {:>10} {:>9} {:>10} {:>10} {:>9}",
        "symbol",
        "locate",
        "commands",
        "events",
        "trades",
        "orders",
        "snapshots",
        "max_orders",
        "max_levels",
        "max_queue"
    );
    let mut total = Coverage::default();
    for (&locate, pair) in pairs {
        let c = &pair.coverage;
        let symbol = replay
            .symbol(locate)
            .map(|s| String::from_utf8_lossy(&s).trim().to_string())
            .unwrap_or_default();
        println!(
            "{symbol:<8} {locate:>6} {:>10} {:>10} {:>9} {:>10} {:>9} {:>10} {:>10} {:>9}",
            c.commands,
            c.events,
            c.trades,
            c.orders_added,
            c.snapshots,
            c.max_orders,
            c.max_levels,
            c.max_queue
        );
        total.commands += c.commands;
        total.events += c.events;
        total.trades += c.trades;
        total.orders_added += c.orders_added;
        total.snapshots += c.snapshots;
    }
    let s = replay.stats();
    println!(
        "total: {} book commands compared ({} events, {} trades, {} orders added, {} snapshot comparisons) over {} messages scanned",
        total.commands, total.events, total.trades, total.orders_added, total.snapshots, scanned
    );
    println!(
        "executions: {} as aggressor, {} as fallback; unexpected trade messages {}",
        s.exec_aggressor,
        s.exec_fallback(),
        s.unexpected_trade_messages
    );
    debug_assert_eq!(total.commands, compared);
    println!(
        "no differences; took {:.1} s",
        started.elapsed().as_secs_f64()
    );
}
