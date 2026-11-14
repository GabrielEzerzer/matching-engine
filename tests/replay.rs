//! Tests for the ITCH replay driver: the message-to-command mapping, the aggressor replay of
//! executions and its fallback, and the counters.

use matching_engine::itch::{ExecutionPrice, Header, Message};
use matching_engine::replay::{CommandKind, Replay, ReplayError, TAKER_ID_BASE};
use matching_engine::{Command, Event, RejectReason, Side};

const L: u16 = 7;

fn h(locate: u16) -> Header {
    Header {
        locate,
        tracking: 0,
        timestamp: 0,
    }
}

fn add(reference: u64, buy: bool, shares: u32, price: u32) -> Message {
    Message::AddOrder {
        header: h(L),
        reference,
        buy,
        shares,
        stock: *b"TEST    ",
        price,
        attribution: None,
    }
}

fn exec(reference: u64, shares: u32) -> Message {
    Message::OrderExecuted {
        header: h(L),
        reference,
        executed_shares: shares,
        match_number: 1,
        with_price: None,
    }
}

fn exec_at(reference: u64, shares: u32, price: u32) -> Message {
    Message::OrderExecuted {
        header: h(L),
        reference,
        executed_shares: shares,
        match_number: 1,
        with_price: Some(ExecutionPrice {
            printable: true,
            price,
        }),
    }
}

fn trading_action(state: u8) -> Message {
    Message::StockTradingAction {
        header: h(L),
        stock: *b"TEST    ",
        state,
        reason: *b"    ",
    }
}

/// Applies a message and returns its command and events.
fn run(replay: &mut Replay, msg: Message) -> (Option<Command>, Vec<Event>) {
    let applied = replay.apply(&msg).expect("replay ok");
    assert_eq!(applied.locate, msg.header().locate);
    (applied.command, applied.events.to_vec())
}

#[test]
fn adds_deletes_cancels_and_replaces_map_to_book_commands() {
    let mut r = Replay::new();
    let (cmd, events) = run(&mut r, add(1, true, 100, 500));
    assert_eq!(
        cmd,
        Some(Command::Limit {
            id: 1,
            side: Side::Buy,
            price: 500,
            qty: 100
        })
    );
    assert_eq!(events, [Event::Accepted { id: 1 }]);

    let (cmd, events) = run(
        &mut r,
        Message::OrderCancel {
            header: h(L),
            reference: 1,
            cancelled_shares: 40,
        },
    );
    assert_eq!(cmd, Some(Command::Reduce { id: 1, by: 40 }));
    assert_eq!(
        events,
        [Event::Reduced {
            id: 1,
            by: 40,
            remaining: 60
        }]
    );

    let (cmd, events) = run(
        &mut r,
        Message::OrderReplace {
            header: h(L),
            original_reference: 1,
            new_reference: 2,
            shares: 70,
            price: 501,
        },
    );
    assert_eq!(
        cmd,
        Some(Command::Replace {
            id: 1,
            new_id: 2,
            price: 501,
            qty: 70
        })
    );
    assert_eq!(
        events,
        [Event::Replaced {
            old_id: 1,
            new_id: 2
        }]
    );
    assert_eq!(r.book(L).unwrap().best_bid(), Some(501));

    let (cmd, events) = run(
        &mut r,
        Message::OrderDelete {
            header: h(L),
            reference: 2,
        },
    );
    assert_eq!(cmd, Some(Command::Cancel { id: 2 }));
    assert_eq!(
        events,
        [Event::Cancelled {
            id: 2,
            remaining: 70
        }]
    );
    let s = r.stats();
    assert_eq!(s.messages, 4);
    assert_eq!(s.book_messages, 4);
    assert_eq!(s.by_type[usize::from(b'A')], 1);
    assert_eq!(s.by_type[usize::from(b'X')], 1);
    assert_eq!(s.by_type[usize::from(b'U')], 1);
    assert_eq!(s.by_type[usize::from(b'D')], 1);
    assert_eq!(s.total_rejections(), 0);
}

#[test]
fn execution_of_the_front_order_replays_as_an_aggressor() {
    let mut r = Replay::new();
    run(&mut r, add(1, false, 50, 1000));
    run(&mut r, add(2, false, 50, 1000));

    let (cmd, events) = run(&mut r, exec(1, 30));
    let t = TAKER_ID_BASE;
    assert_eq!(
        cmd,
        Some(Command::Limit {
            id: t,
            side: Side::Buy,
            price: 1000,
            qty: 30
        })
    );
    assert_eq!(
        events,
        [
            Event::Accepted { id: t },
            Event::Trade {
                taker_id: t,
                maker_id: 1,
                price: 1000,
                qty: 30
            },
            Event::Filled { id: t },
        ]
    );

    // The rest of order 1: the maker's fill comes before the taker's, and the taker id advances.
    let (_, events) = run(&mut r, exec(1, 20));
    let t2 = TAKER_ID_BASE + 1;
    assert_eq!(
        events,
        [
            Event::Accepted { id: t2 },
            Event::Trade {
                taker_id: t2,
                maker_id: 1,
                price: 1000,
                qty: 20
            },
            Event::Filled { id: 1 },
            Event::Filled { id: t2 },
        ]
    );
    let s = r.stats();
    assert_eq!(s.exec_aggressor, 2);
    assert_eq!(s.exec_fallback(), 0);
    assert_eq!(s.trades, 2);
    assert_eq!(s.unexpected_trade_messages, 0);
    assert_eq!(r.book(L).unwrap().front_order(Side::Sell), Some(2));
}

#[test]
fn default_replay_numbers_takers_like_new() {
    // A default replay must keep synthetic taker ids away from ITCH references: a taker id of 1
    // would collide with resting reference 1 and abort the replay.
    let mut r = Replay::default();
    run(&mut r, add(1, false, 2, 1000));
    let (cmd, _) = run(&mut r, exec(1, 1));
    assert!(matches!(cmd, Some(Command::Limit { id, .. }) if id == TAKER_ID_BASE));
    let (cmd, events) = run(&mut r, exec(1, 1));
    assert!(matches!(cmd, Some(Command::Limit { id, .. }) if id == TAKER_ID_BASE + 1));
    assert!(events.contains(&Event::Filled { id: 1 }));
}

#[test]
fn execution_behind_the_front_falls_back_to_reduce() {
    let mut r = Replay::new();
    run(&mut r, add(1, true, 50, 1000));
    run(&mut r, add(2, true, 50, 1000));
    let (cmd, events) = run(&mut r, exec(2, 50));
    assert_eq!(cmd, Some(Command::Reduce { id: 2, by: 50 }));
    assert_eq!(
        events,
        [Event::Reduced {
            id: 2,
            by: 50,
            remaining: 0
        }]
    );
    assert_eq!(r.stats().exec_fallback_behind_at_best, 1);
    assert_eq!(r.stats().exec_aggressor, 0);
}

#[test]
fn execution_away_from_the_best_price_falls_back_to_reduce() {
    let mut r = Replay::new();
    run(&mut r, add(1, true, 50, 1000));
    run(&mut r, add(2, true, 50, 999));
    let (cmd, _) = run(&mut r, exec(2, 10));
    assert_eq!(cmd, Some(Command::Reduce { id: 2, by: 10 }));
    assert_eq!(r.stats().exec_fallback_not_best_price, 1);
}

#[test]
fn execution_with_price_trades_at_the_resting_price_and_counts_the_difference() {
    let mut r = Replay::new();
    run(&mut r, add(1, false, 50, 1000));
    let (cmd, events) = run(&mut r, exec_at(1, 10, 990));
    assert_eq!(
        cmd,
        Some(Command::Limit {
            id: TAKER_ID_BASE,
            side: Side::Buy,
            price: 1000,
            qty: 10
        })
    );
    assert!(events.contains(&Event::Trade {
        taker_id: TAKER_ID_BASE,
        maker_id: 1,
        price: 1000,
        qty: 10
    }));
    run(&mut r, exec_at(1, 10, 1000));
    let s = r.stats();
    assert_eq!(s.by_type[usize::from(b'C')], 2);
    assert_eq!(s.c_price_differs, 1);
    assert_eq!(s.exec_aggressor, 2);
}

#[test]
fn execution_of_an_unknown_or_too_small_order_is_a_counted_rejection() {
    let mut r = Replay::new();
    let (cmd, events) = run(&mut r, exec(9, 10));
    assert_eq!(cmd, Some(Command::Reduce { id: 9, by: 10 }));
    assert_eq!(
        events,
        [Event::Rejected {
            id: 9,
            reason: RejectReason::UnknownOrder
        }]
    );
    run(&mut r, add(1, false, 5, 1000));
    let (_, events) = run(&mut r, exec(1, 6));
    assert_eq!(
        events,
        [Event::Rejected {
            id: 1,
            reason: RejectReason::ReduceTooLarge
        }]
    );
    let s = r.stats();
    assert_eq!(s.exec_fallback_unknown, 1);
    assert_eq!(s.exec_fallback_too_large, 1);
    assert_eq!(s.total_rejections(), 2);
    assert_eq!(
        s.rejections_for(CommandKind::ExecutionFallback, RejectReason::UnknownOrder),
        1
    );
    assert_eq!(
        s.rejections_for(CommandKind::ExecutionFallback, RejectReason::ReduceTooLarge),
        1
    );
    assert_eq!(s.rejections_on_diverged_books, 0);
}

#[test]
fn execution_of_zero_shares_on_the_front_order_falls_back_to_reduce() {
    let mut r = Replay::new();
    run(&mut r, add(1, false, 5, 1000));
    let (cmd, events) = run(&mut r, exec(1, 0));
    assert_eq!(cmd, Some(Command::Reduce { id: 1, by: 0 }));
    assert_eq!(
        events,
        [Event::Rejected {
            id: 1,
            reason: RejectReason::ZeroQuantity
        }]
    );
    let s = r.stats();
    assert_eq!(s.exec_fallback_zero_shares, 1);
    assert_eq!(s.exec_aggressor, 0);
    assert_eq!(
        s.rejections_for(CommandKind::ExecutionFallback, RejectReason::ZeroQuantity),
        1
    );
    // The order is untouched and a later execution still replays as an aggressor.
    run(&mut r, exec(1, 5));
    assert_eq!(r.stats().exec_aggressor, 1);
}

#[test]
fn crossing_adds_are_counted_by_trading_state_and_mark_the_book_diverged() {
    let mut r = Replay::new();
    run(&mut r, trading_action(b'H'));
    assert_eq!(r.trading_state(L), Some(b'H'));
    run(&mut r, add(1, false, 50, 1000));
    // Halted: Nasdaq would rest this crossing buy; our book trades it.
    let (_, events) = run(&mut r, add(2, true, 20, 1001));
    assert!(matches!(events[1], Event::Trade { maker_id: 1, .. }));
    assert!(r.diverged(L));
    let s = r.stats();
    assert_eq!(s.unexpected_trade_messages, 1);
    assert_eq!(s.unexpected_trade_messages_while_trading, 0);
    assert_eq!(s.unexpected_trades, 1);

    // Nasdaq still has order 2 and deletes it: our book has nothing to delete.
    run(
        &mut r,
        Message::OrderDelete {
            header: h(L),
            reference: 2,
        },
    );
    let s = r.stats();
    assert_eq!(
        s.rejections_for(CommandKind::Delete, RejectReason::UnknownOrder),
        1
    );
    assert_eq!(s.rejections_on_diverged_books, 1);

    // Once trading, a crossing add on the already diverged book is counted as such.
    run(&mut r, trading_action(b'T'));
    run(&mut r, add(3, true, 10, 1000));
    let s = r.stats();
    assert_eq!(s.unexpected_trade_messages, 2);
    assert_eq!(s.unexpected_trade_messages_while_trading, 1);
    assert_eq!(s.unexpected_trade_messages_on_clean_trading_books, 0);
}

#[test]
fn crossing_add_on_a_clean_trading_book_is_counted_separately() {
    let mut r = Replay::new();
    run(&mut r, trading_action(b'T'));
    run(&mut r, add(1, false, 50, 1000));
    run(&mut r, add(2, true, 50, 1000));
    assert_eq!(
        r.stats().unexpected_trade_messages_on_clean_trading_books,
        1
    );
}

#[test]
fn crossing_add_before_any_trading_action_counts_as_trading() {
    let mut r = Replay::new();
    assert_eq!(r.trading_state(L), None);
    run(&mut r, add(1, false, 50, 1000));
    run(&mut r, add(2, true, 50, 1000));
    let s = r.stats();
    assert_eq!(s.unexpected_trade_messages_while_trading, 1);
    assert_eq!(s.unexpected_trade_messages_on_clean_trading_books, 1);
}

#[test]
fn crossing_adds_while_paused_or_quoting_do_not_count_as_trading() {
    for state in *b"PQ" {
        let mut r = Replay::new();
        run(&mut r, trading_action(state));
        run(&mut r, add(1, false, 50, 1000));
        run(&mut r, add(2, true, 50, 1000));
        let s = r.stats();
        assert_eq!(s.unexpected_trade_messages, 1);
        assert_eq!(
            s.unexpected_trade_messages_while_trading, 0,
            "{}",
            state as char
        );
    }
}

#[test]
fn references_in_the_taker_range_are_an_error() {
    let mut r = Replay::new();
    let err = r.apply(&add(TAKER_ID_BASE, true, 1, 1)).unwrap_err();
    assert_eq!(
        err,
        ReplayError::ReferenceInTakerSpace {
            locate: L,
            reference: TAKER_ID_BASE
        }
    );
    run(&mut r, add(1, true, 1, 1));
    let replace = Message::OrderReplace {
        header: h(L),
        original_reference: 1,
        new_reference: u64::MAX,
        shares: 1,
        price: 1,
    };
    assert!(matches!(
        r.apply(&replace),
        Err(ReplayError::ReferenceInTakerSpace { .. })
    ));
}

#[test]
fn non_book_messages_change_no_book_and_books_are_per_locate() {
    let mut r = Replay::new();
    let dir = Message::StockDirectory {
        header: h(3),
        stock: *b"ABCD    ",
        market_category: b'Q',
        financial_status: b'N',
        round_lot_size: 100,
        round_lots_only: b'N',
        issue_classification: b'C',
        issue_subtype: *b"Z ",
        authenticity: b'P',
        short_sale_threshold: b'N',
        ipo_flag: b'N',
        luld_reference_price_tier: b'1',
        etp_flag: b'N',
        etp_leverage_factor: 0,
        inverse_indicator: b'N',
    };
    let (cmd, events) = run(&mut r, dir);
    assert_eq!((cmd, events.len()), (None, 0));
    assert_eq!(r.symbol(3), Some(*b"ABCD    "));
    assert_eq!(r.symbol(4), None);
    let (cmd, _) = run(
        &mut r,
        Message::Other {
            kind: b'P',
            header: h(3),
        },
    );
    assert_eq!(cmd, None);

    // The same reference may not repeat in one book, but books are separate per locate.
    let mut a = add(1, true, 10, 100);
    if let Message::AddOrder { header, .. } = &mut a {
        header.locate = 3;
    }
    run(&mut r, a);
    run(&mut r, add(1, false, 10, 100));
    assert_eq!(r.book(3).unwrap().best_bid(), Some(100));
    assert_eq!(r.book(L).unwrap().best_ask(), Some(100));
    let s = r.stats();
    assert_eq!(s.messages, 4);
    assert_eq!(s.book_messages, 2);
    assert_eq!(s.unexpected_trade_messages, 0);
}
