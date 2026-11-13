//! Replays an ITCH 5.0 file through one book per instrument, prints the summary counts and the
//! golden replay hash, and optionally records them or checks them against a recorded file.
//!
//! ```text
//! replay [--write GOLDEN | --check GOLDEN] [--limit N] FILE
//! ```
//!
//! `--check` exits with status 1 and prints the recorded and the computed value of every line
//! that differs. The encoding behind `replay_hash` is documented in `harness/golden.rs`.

#[path = "../harness/golden.rs"]
mod golden;
#[path = "../harness/stream.rs"]
mod stream;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use matching_engine::itch::{Frames, ItchError, Message};
use matching_engine::replay::{CommandKind, REJECT_REASONS, Replay, reject_reason_name};
use sha2::{Digest, Sha256};

use golden::{ReplayHasher, hex};
use stream::{ChunkReader, DEFAULT_CHUNK};

enum Mode {
    Print,
    Write(PathBuf),
    Check(PathBuf),
}

struct Args {
    mode: Mode,
    limit: Option<u64>,
    diagnose: bool,
    file: PathBuf,
}

fn parse_args() -> Result<Args, String> {
    let mut mode = Mode::Print;
    let mut limit = None;
    let mut diagnose = false;
    let mut file = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--write" => mode = Mode::Write(args.next().ok_or("--write needs a path")?.into()),
            "--check" => mode = Mode::Check(args.next().ok_or("--check needs a path")?.into()),
            "--limit" => {
                let n = args.next().ok_or("--limit needs a number")?;
                limit = Some(n.parse().map_err(|_| format!("bad --limit {n}"))?);
            }
            "--diagnose" => diagnose = true,
            _ if arg.starts_with("--") => return Err(format!("unknown option {arg}")),
            _ => file = Some(PathBuf::from(arg)),
        }
    }
    Ok(Args {
        mode,
        limit,
        diagnose,
        file: file.ok_or(
            "usage: replay [--write GOLDEN | --check GOLDEN] [--limit N] [--diagnose] FILE",
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
    let started = Instant::now();
    let lines = match run(&args.file, args.limit, args.diagnose) {
        Ok(lines) => lines,
        Err(e) => {
            eprintln!("replay failed: {e}");
            return ExitCode::from(2);
        }
    };
    eprintln!("replay took {:.1} s", started.elapsed().as_secs_f64());
    let text: String = lines.iter().map(|(k, v)| format!("{k} {v}\n")).collect();
    print!("{text}");
    match args.mode {
        Mode::Print => ExitCode::SUCCESS,
        Mode::Write(path) => match std::fs::write(&path, &text) {
            Ok(()) => {
                eprintln!("wrote {}", path.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("cannot write {}: {e}", path.display());
                ExitCode::from(2)
            }
        },
        Mode::Check(path) => check(&path, &lines),
    }
}

/// Compares every recorded line with the computed one. `input_file` is informational only.
fn check(path: &Path, computed: &[(String, String)]) -> ExitCode {
    let recorded = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("cannot read {}: {e}", path.display());
            return ExitCode::from(2);
        }
    };
    let recorded: Vec<(String, String)> = recorded
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| match l.split_once(' ') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (l.to_string(), String::new()),
        })
        .collect();
    let mut mismatches = 0;
    let lookup = |list: &[(String, String)], key: &str| {
        list.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    };
    let mut keys: Vec<&str> = recorded.iter().map(|(k, _)| k.as_str()).collect();
    for (k, _) in computed {
        if !keys.contains(&k.as_str()) {
            keys.push(k);
        }
    }
    for key in keys {
        if key == "input_file" {
            continue;
        }
        let want = lookup(&recorded, key);
        let got = lookup(computed, key);
        if want != got {
            mismatches += 1;
            eprintln!(
                "MISMATCH {key}: recorded {} computed {}",
                want.as_deref().unwrap_or("(missing)"),
                got.as_deref().unwrap_or("(missing)")
            );
        }
    }
    if mismatches == 0 {
        eprintln!("golden check passed against {}", path.display());
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "golden check FAILED against {}: {mismatches} line(s) differ",
            path.display()
        );
        ExitCode::from(1)
    }
}

fn run(file: &Path, limit: Option<u64>, diagnose: bool) -> Result<Vec<(String, String)>, String> {
    let mut reader = ChunkReader::open(file, DEFAULT_CHUNK).map_err(|e| e.to_string())?;
    let mut input_sha = Sha256::new();
    let mut input_bytes = 0u64;
    let mut consumed_bytes = 0u64;
    let mut replay = Replay::new();
    let mut hasher = ReplayHasher::new();
    let mut next_progress = 50_000_000u64;
    let mut diag = Diagnostics::default();
    'file: loop {
        let new = reader.fill().map_err(|e| e.to_string())?;
        let data = reader.data();
        input_sha.update(&data[data.len() - new..]);
        input_bytes += new as u64;
        if data.is_empty() {
            break;
        }
        let mut frames = Frames::new(data);
        while let Some(item) = frames.next() {
            let msg = match item {
                Ok(msg) => msg,
                Err(ItchError::Truncated { .. }) if !reader.eof() => break,
                Err(e) => {
                    return Err(format!(
                        "bad frame at byte {}: {e}",
                        consumed_bytes + frames.offset() as u64
                    ));
                }
            };
            let before = diagnose.then(|| Before::capture(&replay, &msg));
            let applied = replay.apply(&msg).map_err(|e| e.to_string())?;
            hasher.events(applied.locate, applied.events);
            if let Message::SystemEvent { event, .. } = msg {
                hasher.checkpoint(event, &replay);
            }
            if let Some(before) = before {
                diag.record(&before, &msg, &replay);
            }
            let n = replay.stats().messages;
            if n >= next_progress {
                eprintln!(
                    "  {n} messages, {:.1} GB",
                    (consumed_bytes + frames.offset() as u64) as f64 / 1e9
                );
                next_progress += 50_000_000;
            }
            if limit.is_some_and(|l| n >= l) {
                break 'file;
            }
        }
        let used = frames.offset();
        consumed_bytes += used as u64;
        reader.consume(used);
    }
    if diagnose {
        diag.print();
    }
    let complete = limit.is_none();
    let hash = hasher.finish(&replay);
    Ok(summary(
        file,
        &replay,
        complete,
        input_bytes,
        input_sha,
        hash,
    ))
}

fn summary(
    file: &Path,
    replay: &Replay,
    complete: bool,
    input_bytes: u64,
    input_sha: Sha256,
    hash: [u8; 32],
) -> Vec<(String, String)> {
    let s = replay.stats();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut put = |k: &str, v: String| out.push((k.to_string(), v));
    put("format", "matching-engine-golden-v1".into());
    put(
        "input_file",
        file.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    if complete {
        put("input_bytes", input_bytes.to_string());
        put("input_sha256", hex(&input_sha.finalize()));
    } else {
        put("input_bytes", "partial-run".into());
        put("input_sha256", "partial-run".into());
    }
    put("messages", s.messages.to_string());
    for (kind, &count) in s.by_type.iter().enumerate() {
        if count > 0 {
            put(
                &format!("messages_{}", kind as u8 as char),
                count.to_string(),
            );
        }
    }
    put("book_messages", s.book_messages.to_string());
    put("events", s.events.to_string());
    put("trades", s.trades.to_string());
    let executions = s.exec_aggressor + s.exec_fallback();
    put("executions", executions.to_string());
    put("executions_aggressor", s.exec_aggressor.to_string());
    put("executions_fallback", s.exec_fallback().to_string());
    put(
        "executions_fallback_not_best_price",
        s.exec_fallback_not_best_price.to_string(),
    );
    put(
        "executions_fallback_behind_at_best",
        s.exec_fallback_behind_at_best.to_string(),
    );
    put(
        "executions_fallback_unknown_order",
        s.exec_fallback_unknown.to_string(),
    );
    put(
        "executions_fallback_too_large",
        s.exec_fallback_too_large.to_string(),
    );
    let pct = if executions == 0 {
        0.0
    } else {
        100.0 * s.exec_aggressor as f64 / executions as f64
    };
    put("executions_aggressor_percent", format!("{pct:.4}"));
    put("c_price_differs", s.c_price_differs.to_string());
    put("rejections", s.total_rejections().to_string());
    put(
        "rejections_on_diverged_books",
        s.rejections_on_diverged_books.to_string(),
    );
    put(
        "rejections_on_clean_books",
        (s.total_rejections() - s.rejections_on_diverged_books).to_string(),
    );
    for kind in CommandKind::ALL {
        for reason in REJECT_REASONS {
            let n = s.rejections_for(kind, reason);
            if n > 0 {
                put(
                    &format!("rejections_{}_{}", kind.name(), reject_reason_name(reason)),
                    n.to_string(),
                );
            }
        }
    }
    put(
        "unexpected_trade_messages",
        s.unexpected_trade_messages.to_string(),
    );
    put(
        "unexpected_trade_messages_while_trading",
        s.unexpected_trade_messages_while_trading.to_string(),
    );
    put(
        "unexpected_trade_messages_on_clean_trading_books",
        s.unexpected_trade_messages_on_clean_trading_books
            .to_string(),
    );
    put("unexpected_trades", s.unexpected_trades.to_string());
    let diverged = replay.books().filter(|&(l, _)| replay.diverged(l)).count();
    put("diverged_books", diverged.to_string());
    let mut books = 0u64;
    let mut orders = 0u64;
    for (_, book) in replay.books() {
        let n: usize = [matching_engine::Side::Buy, matching_engine::Side::Sell]
            .into_iter()
            .flat_map(|side| book.depth(side))
            .map(|l| l.order_count)
            .sum();
        if n > 0 {
            books += 1;
            orders += n as u64;
        }
    }
    put("final_books_with_orders", books.to_string());
    put("final_resting_orders", orders.to_string());
    put("replay_hash_algorithm", "sha256".into());
    put("replay_hash", hex(&hash));
    out
}

/// State captured before a message, for `--diagnose`.
struct Before {
    best_bid: Option<u32>,
    best_ask: Option<u32>,
    stats: matching_engine::replay::Stats,
}

impl Before {
    fn capture(replay: &Replay, msg: &matching_engine::itch::Message) -> Self {
        let book = replay.book(msg.header().locate);
        Self {
            best_bid: book.and_then(|b| b.best_bid()),
            best_ask: book.and_then(|b| b.best_ask()),
            stats: replay.stats().clone(),
        }
    }
}

/// Explains unexpected trades, rejections and fallback executions: each unexpected trade and
/// rejection is printed with its time, symbol and the book's best prices before the message;
/// fallbacks are counted per 5-minute bucket of the trading day.
#[derive(Default)]
struct Diagnostics {
    printed: u64,
    fallback_buckets: std::collections::BTreeMap<u64, u64>,
    /// Fallbacks by (E or C, reason, trading state, book diverged).
    fallback_kinds: std::collections::BTreeMap<(char, &'static str, char, bool), u64>,
    unexpected_buckets: std::collections::BTreeMap<u64, u64>,
}

impl Diagnostics {
    fn record(&mut self, before: &Before, msg: &matching_engine::itch::Message, replay: &Replay) {
        let after = replay.stats();
        let ts = msg.header().timestamp;
        let bucket = ts / (300 * 1_000_000_000);
        let fallback = after.exec_fallback() - before.stats.exec_fallback();
        if fallback > 0 {
            *self.fallback_buckets.entry(bucket).or_default() += 1;
            let kind = match msg {
                matching_engine::itch::Message::OrderExecuted {
                    with_price: Some(_),
                    ..
                } => 'C',
                _ => 'E',
            };
            let b = &before.stats;
            let reason = if after.exec_fallback_not_best_price > b.exec_fallback_not_best_price {
                "not_best_price"
            } else if after.exec_fallback_behind_at_best > b.exec_fallback_behind_at_best {
                "behind_at_best"
            } else if after.exec_fallback_unknown > b.exec_fallback_unknown {
                "unknown"
            } else {
                "too_large"
            };
            let locate = msg.header().locate;
            let state = replay.trading_state(locate).map_or('?', char::from);
            *self
                .fallback_kinds
                .entry((kind, reason, state, replay.diverged(locate)))
                .or_default() += 1;
        }
        let unexpected = after.unexpected_trade_messages - before.stats.unexpected_trade_messages;
        if unexpected > 0 {
            *self.unexpected_buckets.entry(bucket).or_default() += 1;
        }
        let rejected = after.total_rejections() - before.stats.total_rejections();
        if (unexpected > 0 || rejected > 0) && self.printed < 200 {
            self.printed += 1;
            let locate = msg.header().locate;
            let symbol = replay
                .symbol(locate)
                .map(|s| String::from_utf8_lossy(&s).trim().to_string())
                .unwrap_or_default();
            eprintln!(
                "{} {} {symbol} (locate {locate}) bid {:?} ask {:?} before; {:?}",
                if unexpected > 0 {
                    "UNEXPECTED"
                } else {
                    "REJECTED"
                },
                clock(ts),
                before.best_bid,
                before.best_ask,
                msg
            );
        }
    }

    fn print(&self) {
        eprintln!("fallback executions by (type, reason, trading state, book diverged):");
        for (key, n) in &self.fallback_kinds {
            eprintln!("  {key:?} {n}");
        }
        for (name, buckets) in [
            ("fallback executions", &self.fallback_buckets),
            ("unexpected trade messages", &self.unexpected_buckets),
        ] {
            eprintln!("{name} per 5 minutes:");
            for (bucket, n) in buckets {
                eprintln!("  {} {n}", clock(bucket * 300 * 1_000_000_000));
            }
        }
    }
}

fn clock(ns: u64) -> String {
    let s = ns / 1_000_000_000;
    format!(
        "{:02}:{:02}:{:02}.{:09}",
        s / 3600,
        s / 60 % 60,
        s % 60,
        ns % 1_000_000_000
    )
}
