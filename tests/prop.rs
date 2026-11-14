//! Property tests: random command streams go to both the real book and the slow reference book.
//! After every command the two must have emitted the same events and hold the same orders, and
//! the real book's invariants must hold.
//!
//! Case count defaults to `DEFAULT_CASES`; set `PROPTEST_CASES` to run more.

#[path = "../harness/reference.rs"]
mod reference;

use std::collections::HashSet;

use matching_engine::{
    Command, Event, LevelDepth, OrderBook, OrderId, RejectReason, Side, Snapshot,
};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, FileFailurePersistence, TestRunner};
use reference::ReferenceBook;

const DEFAULT_CASES: u32 = 2_000;

fn config() -> Config {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_CASES);
    Config {
        cases,
        failure_persistence: Some(Box::new(FileFailurePersistence::WithSource("regressions"))),
        ..Config::default()
    }
}

/// A small id pool so that cancels, modifies and new orders keep hitting ids that are live,
/// filled, cancelled or duplicated, plus a rare id from a wide range that is almost never known
/// and a rare id at either end of the integer range.
fn id() -> impl Strategy<Value = OrderId> {
    prop_oneof![
        24 => 1..=16u64,
        2 => 1_000..=1_000_000u64,
        1 => prop_oneof![Just(0u64), Just(u64::MAX)],
    ]
}

/// Mostly small sizes, some zeros, some large sizes that sweep several levels, and a rare size
/// at the top of the integer range.
fn qty() -> impl Strategy<Value = u32> {
    prop_oneof![
        2 => Just(0u32),
        24 => 1..=10u32,
        6 => 11..=60u32,
        1 => prop_oneof![Just(u32::MAX - 1), Just(u32::MAX)],
    ]
}

/// Reduce sizes skewed small, so a reduce often takes exactly what is left (reduce to zero),
/// with some zeros and some that ask for more than is left.
fn reduce_by() -> impl Strategy<Value = u32> {
    prop_oneof![
        1 => Just(0u32),
        6 => 1..=3u32,
        3 => 4..=12u32,
    ]
}

/// A narrow band, so buy and sell prices overlap and orders cross often, plus a rare price at
/// either end of the integer range, so a round that does arithmetic on prices cannot pass unseen.
fn price() -> impl Strategy<Value = u32> {
    prop_oneof![
        30 => 96..=104u32,
        1 => prop_oneof![Just(0u32), Just(1), Just(u32::MAX - 1), Just(u32::MAX)],
    ]
}

fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Buy), Just(Side::Sell)]
}

fn command() -> impl Strategy<Value = Command> {
    prop_oneof![
        10 => (id(), side(), price(), qty())
            .prop_map(|(id, side, price, qty)| Command::Limit { id, side, price, qty }),
        3 => (id(), side(), qty()).prop_map(|(id, side, qty)| Command::Market { id, side, qty }),
        3 => id().prop_map(|id| Command::Cancel { id }),
        3 => (id(), reduce_by()).prop_map(|(id, by)| Command::Reduce { id, by }),
        3 => (id(), prop::option::weighted(0.7, id()), price(), qty()).prop_map(
            |(id, new_id, price, qty)| Command::Replace {
                id,
                new_id: new_id.unwrap_or(id),
                price,
                qty,
            }
        ),
    ]
}

fn commands() -> impl Strategy<Value = Vec<Command>> {
    prop::collection::vec(command(), 1..200)
}

fn depth_from(snapshot: &Snapshot, side: Side) -> Vec<LevelDepth> {
    let levels = match side {
        Side::Buy => &snapshot.bids,
        Side::Sell => &snapshot.asks,
    };
    levels
        .iter()
        .map(|level| LevelDepth {
            price: level.price,
            total_qty: level.orders.iter().map(|&(_, q)| u64::from(q)).sum(),
            order_count: level.orders.len(),
        })
        .collect()
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn book_matches_reference(cmds in commands()) {
        let mut book = OrderBook::new();
        let mut reference = ReferenceBook::new();
        let mut got = Vec::new();
        let mut want = Vec::new();
        for (i, cmd) in cmds.iter().enumerate() {
            got.clear();
            want.clear();
            book.apply(*cmd, &mut got);
            reference.apply(*cmd, &mut want);
            prop_assert_eq!(&got, &want, "events differ at command {}: {:?}", i, cmd);
            if let Err(e) = book.check_invariants() {
                prop_assert!(false, "invariant broken after command {}: {:?}: {}", i, cmd, e);
            }
            let snapshot = reference.snapshot();
            prop_assert_eq!(book.snapshot(), snapshot.clone(), "books differ after command {}: {:?}", i, cmd);
            prop_assert_eq!(book.best_bid(), snapshot.bids.first().map(|l| l.price));
            prop_assert_eq!(book.best_ask(), snapshot.asks.first().map(|l| l.price));
            prop_assert_eq!(book.depth(Side::Buy), depth_from(&snapshot, Side::Buy));
            prop_assert_eq!(book.depth(Side::Sell), depth_from(&snapshot, Side::Sell));
            prop_assert_eq!(book.front_order(Side::Buy), snapshot.bids.first().map(|l| l.orders[0].0));
            prop_assert_eq!(book.front_order(Side::Sell), snapshot.asks.first().map(|l| l.orders[0].0));
        }
    }
}

/// Proves the generators reach the cases the property test is meant to cover. Counts each case
/// over a fixed set of generated streams and requires every one of them to show up often.
#[test]
fn generators_hit_the_interesting_cases() {
    #[derive(Default, Debug)]
    struct Counts {
        zero_qty: u32,
        duplicate_id: u32,
        unknown_id: u32,
        reduce_too_large: u32,
        cancel_live: u32,
        target_filled: u32,
        target_cancelled: u32,
        market_into_empty: u32,
        market_partly_filled: u32,
        reduce_partial: u32,
        reduce_to_zero: u32,
        replace_same_id: u32,
        replace_trades: u32,
        sweep_two_levels: u32,
        partial_fill_then_rest: u32,
        extreme_price: u32,
        extreme_qty: u32,
        extreme_id: u32,
    }

    let mut runner = TestRunner::deterministic();
    let strategy = commands();
    let mut c = Counts::default();
    for _ in 0..500 {
        let cmds = strategy.new_tree(&mut runner).unwrap().current();
        let mut book = OrderBook::new();
        let mut filled: HashSet<OrderId> = HashSet::new();
        let mut cancelled: HashSet<OrderId> = HashSet::new();
        let mut events = Vec::new();
        for cmd in cmds {
            let live = |book: &OrderBook, id| {
                let s = book.snapshot();
                s.bids
                    .iter()
                    .chain(&s.asks)
                    .any(|l| l.orders.iter().any(|&(o, _)| o == id))
            };
            let target = match cmd {
                Command::Cancel { id }
                | Command::Reduce { id, .. }
                | Command::Replace { id, .. } => Some(id),
                _ => None,
            };
            if let Some(id) = target
                && !live(&book, id)
            {
                c.target_filled += u32::from(filled.contains(&id));
                c.target_cancelled += u32::from(cancelled.contains(&id));
            }
            let opposite_empty = match cmd {
                Command::Market {
                    side: Side::Buy, ..
                } => book.best_ask().is_none(),
                Command::Market {
                    side: Side::Sell, ..
                } => book.best_bid().is_none(),
                _ => false,
            };

            events.clear();
            book.apply(cmd, &mut events);

            c.market_into_empty +=
                u32::from(opposite_empty && matches!(events[0], Event::Accepted { .. }));

            let trades: Vec<u32> = events
                .iter()
                .filter_map(|e| match e {
                    Event::Trade { price, .. } => Some(*price),
                    _ => None,
                })
                .collect();
            for e in &events {
                match *e {
                    Event::Rejected { reason, .. } => match reason {
                        RejectReason::ZeroQuantity => c.zero_qty += 1,
                        RejectReason::DuplicateOrderId => c.duplicate_id += 1,
                        RejectReason::UnknownOrder => c.unknown_id += 1,
                        RejectReason::ReduceTooLarge => c.reduce_too_large += 1,
                    },
                    Event::Filled { id } => {
                        filled.insert(id);
                    }
                    Event::Cancelled { id, .. } => {
                        cancelled.insert(id);
                    }
                    Event::Reduced { remaining: 0, .. } => c.reduce_to_zero += 1,
                    Event::Reduced { .. } => c.reduce_partial += 1,
                    _ => {}
                }
            }
            match cmd {
                Command::Cancel { .. } if matches!(events[0], Event::Cancelled { .. }) => {
                    c.cancel_live += 1
                }
                Command::Market { .. }
                    if !trades.is_empty()
                        && matches!(events.last(), Some(Event::Cancelled { .. })) =>
                {
                    c.market_partly_filled += 1
                }
                Command::Replace { id, new_id, .. }
                    if matches!(events[0], Event::Replaced { .. }) =>
                {
                    c.replace_same_id += u32::from(id == new_id);
                    c.replace_trades += u32::from(!trades.is_empty());
                }
                Command::Limit { id, price, qty, .. } => {
                    // Accepted limits at the ends of the integer ranges.
                    if matches!(events[0], Event::Accepted { .. }) {
                        c.extreme_price += u32::from(price <= 1 || price >= u32::MAX - 1);
                        c.extreme_qty += u32::from(qty >= u32::MAX - 1);
                        c.extreme_id += u32::from(id == 0 || id == u64::MAX);
                    }
                    let mut prices = trades.clone();
                    prices.dedup();
                    c.sweep_two_levels += u32::from(prices.len() >= 2);
                    c.partial_fill_then_rest += u32::from(!trades.is_empty() && live(&book, id));
                }
                _ => {}
            }
        }
    }

    println!("{c:#?}");
    let all = [
        c.zero_qty,
        c.duplicate_id,
        c.unknown_id,
        c.reduce_too_large,
        c.cancel_live,
        c.target_filled,
        c.target_cancelled,
        c.market_into_empty,
        c.market_partly_filled,
        c.reduce_partial,
        c.reduce_to_zero,
        c.replace_same_id,
        c.replace_trades,
        c.extreme_price,
        c.extreme_qty,
        c.extreme_id,
        c.sweep_two_levels,
        c.partial_fill_then_rest,
    ];
    assert!(
        all.iter().all(|&n| n >= 100),
        "a case is rarely generated: {c:#?}"
    );
}
