//! Unit tests for the order book's edge cases, through its public API.

use matching_engine::{
    Command, Event, LevelDepth, LevelSnapshot, OrderBook, OrderId, Price, Qty, RejectReason,
    Side::{self, Buy, Sell},
};

fn limit(id: OrderId, side: Side, price: Price, qty: Qty) -> Command {
    Command::Limit {
        id,
        side,
        price,
        qty,
    }
}

fn market(id: OrderId, side: Side, qty: Qty) -> Command {
    Command::Market { id, side, qty }
}

fn cancel(id: OrderId) -> Command {
    Command::Cancel { id }
}

fn reduce(id: OrderId, by: Qty) -> Command {
    Command::Reduce { id, by }
}

fn replace(id: OrderId, new_id: OrderId, price: Price, qty: Qty) -> Command {
    Command::Replace {
        id,
        new_id,
        price,
        qty,
    }
}

fn trade(taker_id: OrderId, maker_id: OrderId, price: Price, qty: Qty) -> Event {
    Event::Trade {
        taker_id,
        maker_id,
        price,
        qty,
    }
}

fn accepted(id: OrderId) -> Event {
    Event::Accepted { id }
}

fn filled(id: OrderId) -> Event {
    Event::Filled { id }
}

fn rejected(id: OrderId, reason: RejectReason) -> Event {
    Event::Rejected { id, reason }
}

fn level(price: Price, orders: &[(OrderId, Qty)]) -> LevelSnapshot {
    LevelSnapshot {
        price,
        orders: orders.to_vec(),
    }
}

/// Applies the setup commands, then returns only the events of the last command.
fn run(book: &mut OrderBook, setup: &[Command], cmd: Command) -> Vec<Event> {
    let mut out = Vec::new();
    for &c in setup {
        book.apply(c, &mut out);
        book.check_invariants().unwrap();
    }
    out.clear();
    book.apply(cmd, &mut out);
    book.check_invariants().unwrap();
    out
}

#[test]
fn partial_fill_of_maker_leaves_its_remainder_resting() {
    let mut book = OrderBook::new();
    let events = run(&mut book, &[limit(1, Sell, 100, 10)], limit(2, Buy, 100, 4));
    assert_eq!(events, [accepted(2), trade(2, 1, 100, 4), filled(2)]);
    let s = book.snapshot();
    assert_eq!(s.asks, [level(100, &[(1, 6)])]);
    assert!(s.bids.is_empty());
}

#[test]
fn partial_fill_of_taker_rests_its_remainder() {
    let mut book = OrderBook::new();
    let events = run(&mut book, &[limit(1, Sell, 100, 4)], limit(2, Buy, 101, 10));
    assert_eq!(events, [accepted(2), trade(2, 1, 100, 4), filled(1)]);
    let s = book.snapshot();
    assert!(s.asks.is_empty());
    assert_eq!(s.bids, [level(101, &[(2, 6)])]);
}

#[test]
fn trade_price_is_the_makers_price() {
    let mut book = OrderBook::new();
    let events = run(&mut book, &[limit(1, Buy, 105, 5)], limit(2, Sell, 99, 5));
    assert_eq!(
        events,
        [accepted(2), trade(2, 1, 105, 5), filled(1), filled(2)]
    );
}

#[test]
fn aggressive_limit_sweeps_several_levels_and_rests_the_remainder() {
    let mut book = OrderBook::new();
    let setup = [
        limit(1, Sell, 100, 3),
        limit(2, Sell, 101, 4),
        limit(3, Sell, 102, 5),
        limit(4, Sell, 104, 7),
    ];
    let events = run(&mut book, &setup, limit(9, Buy, 102, 20));
    assert_eq!(
        events,
        [
            accepted(9),
            trade(9, 1, 100, 3),
            filled(1),
            trade(9, 2, 101, 4),
            filled(2),
            trade(9, 3, 102, 5),
            filled(3),
        ]
    );
    let s = book.snapshot();
    assert_eq!(s.bids, [level(102, &[(9, 8)])]);
    assert_eq!(s.asks, [level(104, &[(4, 7)])]);
}

#[test]
fn time_priority_within_a_level_fills_the_oldest_order_first() {
    let mut book = OrderBook::new();
    let setup = [
        limit(1, Buy, 100, 5),
        limit(2, Buy, 100, 5),
        limit(3, Buy, 100, 5),
    ];
    let events = run(&mut book, &setup, limit(9, Sell, 100, 7));
    assert_eq!(
        events,
        [
            accepted(9),
            trade(9, 1, 100, 5),
            filled(1),
            trade(9, 2, 100, 2),
            filled(9),
        ]
    );
    assert_eq!(book.snapshot().bids, [level(100, &[(2, 3), (3, 5)])]);
}

#[test]
fn better_price_fills_before_older_order_at_a_worse_price() {
    let mut book = OrderBook::new();
    let setup = [limit(1, Buy, 99, 5), limit(2, Buy, 100, 5)];
    let events = run(&mut book, &setup, market(9, Sell, 5));
    assert_eq!(
        events,
        [accepted(9), trade(9, 2, 100, 5), filled(2), filled(9)]
    );
}

#[test]
fn market_order_on_an_empty_book_is_cancelled_in_full() {
    let mut book = OrderBook::new();
    let events = run(&mut book, &[limit(1, Buy, 100, 5)], market(2, Buy, 10));
    assert_eq!(
        events,
        [
            accepted(2),
            Event::Cancelled {
                id: 2,
                remaining: 10
            }
        ]
    );
    assert_eq!(book.snapshot().bids, [level(100, &[(1, 5)])]);
}

#[test]
fn market_order_on_a_thin_book_fills_what_exists_and_cancels_the_rest() {
    let mut book = OrderBook::new();
    let setup = [limit(1, Sell, 100, 3), limit(2, Sell, 150, 2)];
    let events = run(&mut book, &setup, market(9, Buy, 10));
    assert_eq!(
        events,
        [
            accepted(9),
            trade(9, 1, 100, 3),
            filled(1),
            trade(9, 2, 150, 2),
            filled(2),
            Event::Cancelled {
                id: 9,
                remaining: 5
            },
        ]
    );
    assert_eq!(book.snapshot(), Default::default());
}

#[test]
fn market_order_never_rests() {
    let mut book = OrderBook::new();
    run(&mut book, &[limit(1, Sell, 100, 3)], market(9, Buy, 3));
    let events = run(&mut book, &[], cancel(9));
    assert_eq!(events, [rejected(9, RejectReason::UnknownOrder)]);
}

#[test]
fn cancel_removes_a_resting_order_and_reports_its_remainder() {
    let mut book = OrderBook::new();
    let setup = [limit(1, Sell, 100, 10), limit(2, Buy, 100, 4)];
    let events = run(&mut book, &setup, cancel(1));
    assert_eq!(
        events,
        [Event::Cancelled {
            id: 1,
            remaining: 6
        }]
    );
    assert_eq!(book.snapshot(), Default::default());
}

#[test]
fn cancel_of_a_filled_order_is_rejected() {
    let mut book = OrderBook::new();
    let setup = [limit(1, Sell, 100, 5), limit(2, Buy, 100, 5)];
    assert_eq!(
        run(&mut book, &setup, cancel(1)),
        [rejected(1, RejectReason::UnknownOrder)]
    );
}

#[test]
fn cancelling_twice_rejects_the_second_cancel() {
    let mut book = OrderBook::new();
    let events = run(&mut book, &[limit(1, Buy, 100, 5), cancel(1)], cancel(1));
    assert_eq!(events, [rejected(1, RejectReason::UnknownOrder)]);
}

#[test]
fn cancel_of_an_unknown_id_is_rejected() {
    let mut book = OrderBook::new();
    let events = run(&mut book, &[limit(1, Buy, 100, 5)], cancel(77));
    assert_eq!(events, [rejected(77, RejectReason::UnknownOrder)]);
    assert_eq!(book.snapshot().bids, [level(100, &[(1, 5)])]);
}

#[test]
fn empty_level_is_removed_when_its_last_order_fills() {
    let mut book = OrderBook::new();
    let setup = [limit(1, Sell, 100, 5), limit(2, Sell, 101, 5)];
    run(&mut book, &setup, limit(3, Buy, 100, 5));
    assert_eq!(book.best_ask(), Some(101));
    assert_eq!(book.depth(Sell).len(), 1);
}

#[test]
fn empty_level_is_removed_when_its_last_order_is_cancelled() {
    let mut book = OrderBook::new();
    let setup = [limit(1, Buy, 100, 5), limit(2, Buy, 99, 5)];
    run(&mut book, &setup, cancel(1));
    assert_eq!(book.best_bid(), Some(99));
    assert_eq!(book.depth(Buy).len(), 1);
}

#[test]
fn empty_level_is_removed_when_its_last_order_is_reduced_to_zero() {
    let mut book = OrderBook::new();
    let setup = [limit(1, Buy, 100, 5), limit(2, Buy, 99, 5)];
    let events = run(&mut book, &setup, reduce(1, 5));
    assert_eq!(
        events,
        [Event::Reduced {
            id: 1,
            by: 5,
            remaining: 0
        }]
    );
    assert_eq!(book.best_bid(), Some(99));
    assert_eq!(
        run(&mut book, &[], cancel(1)),
        [rejected(1, RejectReason::UnknownOrder)]
    );
}

#[test]
fn reduce_keeps_time_priority() {
    let mut book = OrderBook::new();
    let setup = [limit(1, Sell, 100, 10), limit(2, Sell, 100, 10)];
    let events = run(&mut book, &setup, reduce(1, 7));
    assert_eq!(
        events,
        [Event::Reduced {
            id: 1,
            by: 7,
            remaining: 3
        }]
    );
    assert_eq!(book.snapshot().asks, [level(100, &[(1, 3), (2, 10)])]);
    assert_eq!(
        book.depth(Sell),
        [LevelDepth {
            price: 100,
            total_qty: 13,
            order_count: 2
        }]
    );
    let events = run(&mut book, &[], market(9, Buy, 4));
    assert_eq!(
        events,
        [
            accepted(9),
            trade(9, 1, 100, 3),
            filled(1),
            trade(9, 2, 100, 1),
            filled(9),
        ]
    );
}

#[test]
fn reduce_by_more_than_remaining_is_rejected() {
    let mut book = OrderBook::new();
    let events = run(&mut book, &[limit(1, Sell, 100, 5)], reduce(1, 6));
    assert_eq!(events, [rejected(1, RejectReason::ReduceTooLarge)]);
    assert_eq!(book.snapshot().asks, [level(100, &[(1, 5)])]);
}

#[test]
fn replace_loses_time_priority() {
    let mut book = OrderBook::new();
    let setup = [limit(1, Buy, 100, 5), limit(2, Buy, 100, 5)];
    let events = run(&mut book, &setup, replace(1, 1, 100, 5));
    assert_eq!(
        events,
        [Event::Replaced {
            old_id: 1,
            new_id: 1
        }]
    );
    assert_eq!(book.snapshot().bids, [level(100, &[(2, 5), (1, 5)])]);
}

#[test]
fn replace_with_a_new_id_and_price_moves_the_order() {
    let mut book = OrderBook::new();
    let setup = [limit(1, Buy, 100, 5), limit(2, Buy, 99, 5)];
    let events = run(&mut book, &setup, replace(1, 3, 99, 8));
    assert_eq!(
        events,
        [Event::Replaced {
            old_id: 1,
            new_id: 3
        }]
    );
    assert_eq!(book.snapshot().bids, [level(99, &[(2, 5), (3, 8)])]);
    assert_eq!(
        run(&mut book, &[], cancel(1)),
        [rejected(1, RejectReason::UnknownOrder)]
    );
}

#[test]
fn replace_that_crosses_trades_at_the_makers_price() {
    let mut book = OrderBook::new();
    let setup = [limit(1, Sell, 101, 3), limit(2, Buy, 99, 10)];
    let events = run(&mut book, &setup, replace(2, 5, 102, 10));
    assert_eq!(
        events,
        [
            Event::Replaced {
                old_id: 2,
                new_id: 5
            },
            trade(5, 1, 101, 3),
            filled(1),
        ]
    );
    assert_eq!(book.snapshot().bids, [level(102, &[(5, 7)])]);
    assert!(book.snapshot().asks.is_empty());
}

#[test]
fn replace_onto_another_live_id_is_rejected_and_keeps_the_original() {
    let mut book = OrderBook::new();
    let setup = [limit(1, Buy, 100, 5), limit(2, Buy, 99, 5)];
    let events = run(&mut book, &setup, replace(1, 2, 98, 5));
    assert_eq!(events, [rejected(1, RejectReason::DuplicateOrderId)]);
    assert_eq!(
        book.snapshot().bids,
        [level(100, &[(1, 5)]), level(99, &[(2, 5)])]
    );
}

#[test]
fn replace_of_an_unknown_id_is_rejected() {
    let mut book = OrderBook::new();
    let events = run(&mut book, &[], replace(1, 2, 100, 5));
    assert_eq!(events, [rejected(1, RejectReason::UnknownOrder)]);
}

#[test]
fn duplicate_live_id_is_rejected() {
    let mut book = OrderBook::new();
    let setup = [limit(1, Buy, 100, 5)];
    assert_eq!(
        run(&mut book, &setup, limit(1, Sell, 105, 5)),
        [rejected(1, RejectReason::DuplicateOrderId)]
    );
    assert_eq!(
        run(&mut book, &[], market(1, Sell, 5)),
        [rejected(1, RejectReason::DuplicateOrderId)]
    );
    assert_eq!(book.snapshot().bids, [level(100, &[(1, 5)])]);
    assert!(book.snapshot().asks.is_empty());
}

#[test]
fn id_of_a_finished_order_can_be_used_again() {
    let mut book = OrderBook::new();
    let setup = [limit(1, Buy, 100, 5), cancel(1)];
    assert_eq!(
        run(&mut book, &setup, limit(1, Sell, 101, 2)),
        [accepted(1)]
    );
}

#[test]
fn zero_quantity_is_rejected() {
    let mut book = OrderBook::new();
    let setup = [limit(1, Buy, 100, 5)];
    let zero = RejectReason::ZeroQuantity;
    assert_eq!(
        run(&mut book, &setup, limit(2, Sell, 100, 0)),
        [rejected(2, zero)]
    );
    assert_eq!(run(&mut book, &[], market(3, Sell, 0)), [rejected(3, zero)]);
    assert_eq!(run(&mut book, &[], reduce(1, 0)), [rejected(1, zero)]);
    assert_eq!(
        run(&mut book, &[], replace(1, 4, 100, 0)),
        [rejected(1, zero)]
    );
    assert_eq!(book.snapshot().bids, [level(100, &[(1, 5)])]);
}

#[test]
fn best_prices_and_depth_report_the_resting_orders() {
    let mut book = OrderBook::new();
    let setup = [
        limit(1, Buy, 99, 5),
        limit(2, Buy, 100, 3),
        limit(3, Buy, 100, 4),
        limit(4, Sell, 102, 6),
    ];
    run(&mut book, &setup, limit(5, Sell, 103, 1));
    assert_eq!(book.best_bid(), Some(100));
    assert_eq!(book.best_ask(), Some(102));
    assert_eq!(
        book.depth(Buy),
        [
            LevelDepth {
                price: 100,
                total_qty: 7,
                order_count: 2
            },
            LevelDepth {
                price: 99,
                total_qty: 5,
                order_count: 1
            },
        ]
    );
    assert_eq!(
        book.depth(Sell),
        [
            LevelDepth {
                price: 102,
                total_qty: 6,
                order_count: 1
            },
            LevelDepth {
                price: 103,
                total_qty: 1,
                order_count: 1
            },
        ]
    );
}

#[test]
fn level_total_does_not_overflow_with_many_max_size_orders() {
    let mut book = OrderBook::new();
    let setup = [limit(1, Buy, 100, Qty::MAX), limit(2, Buy, 100, Qty::MAX)];
    run(&mut book, &setup, limit(3, Buy, 100, Qty::MAX));
    assert_eq!(book.depth(Buy)[0].total_qty, 3 * u64::from(Qty::MAX));
}
