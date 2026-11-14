//! The microbenchmark workload derivation (`harness/workload.rs`) on a small synthetic command
//! stream: replaying each part's derived operations into a plain model must end in exactly the
//! state the real book reached, and the byte encoding must round-trip. With the `internals`
//! feature the same check runs against the real parts through the benchmark's adapters.

#[path = "../harness/workload.rs"]
mod workload;

use std::collections::{BTreeSet, HashMap, VecDeque};

use matching_engine::{Command, OrderBook, OrderId, Price, Qty, Side, Snapshot};
use workload::{BookIdx, LevelOp, MapOp, Recorder, SideOp, Workload, codec};

use Command::{Cancel, Limit, Market, Reduce, Replace};
use Side::{Buy, Sell};

fn lim(id: OrderId, side: Side, price: Price, qty: Qty) -> Command {
    Limit {
        id,
        side,
        price,
        qty,
    }
}

/// Commands for two books covering resting, multi-level sweeps, partial and full fills, market
/// orders, partial and full reduces, replaces (new id, same id, crossing) and every rejection,
/// ending with a partial fill and a partial reduce of orders that still rest.
fn commands() -> Vec<(BookIdx, Command)> {
    vec![
        (0, lim(1, Buy, 10, 100)),
        (0, lim(2, Buy, 10, 100)),
        (1, lim(1, Sell, 500, 40)),
        (0, lim(3, Buy, 9, 50)),
        (0, lim(4, Sell, 12, 70)),
        (0, lim(5, Sell, 13, 30)),
        (0, lim(6, Sell, 9, 150)), // fills 1, half of 2
        (0, lim(7, Sell, 9, 120)), // empties 10 and 9, rests 20 at 9
        (1, lim(2, Buy, 499, 10)),
        (0, Reduce { id: 4, by: 20 }),
        (0, Reduce { id: 5, by: 30 }), // removes 5 and its level
        (0, Cancel { id: 999 }),
        (0, Reduce { id: 4, by: 1000 }),
        (0, Reduce { id: 4, by: 0 }),
        (0, lim(4, Buy, 1, 1)),
        (0, lim(8, Buy, 1, 0)),
        (
            0,
            Replace {
                id: 4,
                new_id: 9,
                price: 11,
                qty: 10,
            },
        ),
        (
            0,
            Replace {
                id: 9,
                new_id: 9,
                price: 8,
                qty: 10,
            },
        ),
        (0, lim(10, Buy, 9, 25)), // fills 9 at 8, part of 7 at 9
        (
            0,
            Market {
                id: 11,
                side: Buy,
                qty: 100,
            },
        ), // takes the rest of 7, cancels the remainder
        (0, lim(12, Buy, 5, 10)),
        (0, lim(13, Buy, 5, 10)),
        (
            0,
            Replace {
                id: 12,
                new_id: 13,
                price: 5,
                qty: 10,
            },
        ),
        (
            0,
            Replace {
                id: 12,
                new_id: 14,
                price: 5,
                qty: 0,
            },
        ),
        (
            0,
            Replace {
                id: 77,
                new_id: 78,
                price: 5,
                qty: 1,
            },
        ),
        (0, Cancel { id: 13 }),
        (0, lim(15, Sell, 20, 5)),  // reuses a freed level slot
        (1, lim(3, Sell, 499, 15)), // fills 2, rests 5 at 499
        (
            1,
            Replace {
                id: 1,
                new_id: 4,
                price: 498,
                qty: 40,
            },
        ),
        (1, Cancel { id: 3 }),
        (1, lim(5, Buy, 498, 15)), // 25 of 4 left
        (0, Reduce { id: 12, by: 4 }),
    ]
}

/// Applies the commands to real books and records the workload, as `examples/micro.rs` does.
fn derive(cmds: &[(BookIdx, Command)]) -> (Workload, Vec<OrderBook>) {
    let mut books = vec![OrderBook::new(), OrderBook::new()];
    let mut rec = Recorder::new(books.len());
    let mut events = Vec::new();
    for &(b, cmd) in cmds {
        let book = &mut books[usize::from(b)];
        let named = match cmd {
            Cancel { id } | Reduce { id, .. } | Replace { id, .. } => book.order(id),
            _ => None,
        };
        events.clear();
        book.apply(cmd, &mut events);
        rec.record(b, cmd, named, &events, book);
    }
    (rec.finish(), books)
}

/// Each side's level prices, each level's queue and total, and the id map, from snapshots.
struct Expected {
    side_levels: BTreeSet<(u32, Price)>,
    queues: Vec<(Vec<OrderId>, u64)>,
    map: HashMap<(BookIdx, OrderId), (Side, Price, Qty)>,
}

fn expected(books: &[OrderBook]) -> Expected {
    let mut e = Expected {
        side_levels: BTreeSet::new(),
        queues: Vec::new(),
        map: HashMap::new(),
    };
    for (b, book) in books.iter().enumerate() {
        let Snapshot { bids, asks } = book.snapshot();
        for (side, levels) in [(Buy, bids), (Sell, asks)] {
            let s = b as u32 * 2 + u32::from(side == Sell);
            for level in levels {
                e.side_levels.insert((s, level.price));
                let total = level.orders.iter().map(|&(_, q)| u64::from(q)).sum();
                e.queues
                    .push((level.orders.iter().map(|&(id, _)| id).collect(), total));
                for (id, qty) in level.orders {
                    e.map.insert((b as BookIdx, id), (side, level.price, qty));
                }
            }
        }
    }
    e.queues.sort();
    e
}

#[test]
fn derived_operations_rebuild_the_books_state() {
    let (w, books) = derive(&commands());
    let e = expected(&books);
    assert_eq!(w.final_orders, e.map.len());
    assert_eq!(w.final_levels, e.side_levels.len());
    assert_eq!(w.final_shares, shares(&e));
    let created = w.final_levels
        + w.level_ops
            .iter()
            .filter(|op| matches!(op, LevelOp::Reset { .. }))
            .count();
    assert!(
        w.level_slots < created,
        "slots of dropped levels are reused"
    );

    let mut sides = BTreeSet::new();
    for op in &w.side_ops {
        match *op {
            SideOp::Best { side } => assert!((side as usize) < 2 * w.books),
            SideOp::Find { side, price } => assert!(sides.contains(&(side, price))),
            SideOp::FindOrCreate { side, price } => {
                sides.insert((side, price));
            }
            SideOp::Remove { side, price } => assert!(sides.remove(&(side, price))),
        }
    }
    assert_eq!(sides, e.side_levels);

    let mut queues: Vec<(VecDeque<OrderId>, u64)> = vec![Default::default(); w.level_slots];
    for op in &w.level_ops {
        match *op {
            LevelOp::Push { slot, id, qty } => {
                let q = &mut queues[slot as usize];
                q.0.push_back(id);
                q.1 += u64::from(qty);
            }
            LevelOp::Front { slot } => assert!(!queues[slot as usize].0.is_empty()),
            LevelOp::Reduce { slot, qty } => queues[slot as usize].1 -= u64::from(qty),
            LevelOp::PopFront { slot } => {
                queues[slot as usize].0.pop_front().expect("front exists");
            }
            LevelOp::Remove { slot, id, qty } => {
                let q = &mut queues[slot as usize];
                let pos = q.0.iter().position(|&x| x == id).expect("queued");
                q.0.remove(pos);
                q.1 -= u64::from(qty);
            }
            LevelOp::Reset { slot } => {
                let q = &mut queues[slot as usize];
                assert!(q.0.is_empty() && q.1 == 0, "only empty levels are dropped");
                *q = Default::default();
            }
        }
    }
    let mut live: Vec<(Vec<OrderId>, u64)> = queues
        .into_iter()
        .filter(|q| !q.0.is_empty())
        .map(|(q, t)| (q.into_iter().collect(), t))
        .collect();
    live.sort();
    assert_eq!(live, e.queues);

    let mut map = HashMap::new();
    for op in &w.map_ops {
        match *op {
            MapOp::Insert {
                book,
                id,
                side,
                price,
                qty,
            } => assert!(map.insert((book, id), (side, price, qty)).is_none()),
            MapOp::Get { .. } => {}
            MapOp::Update { book, id, qty } => {
                map.get_mut(&(book, id)).expect("updated order rests").2 = qty;
            }
            MapOp::Remove { book, id } => {
                map.remove(&(book, id));
            }
        }
    }
    assert_eq!(map, e.map);

    // The book workload is the commands themselves; replaying them reproduces the books.
    let replayed = workload::run_books(&w);
    for (a, b) in replayed.iter().zip(&books) {
        assert_eq!(a.snapshot(), b.snapshot());
    }
    assert_eq!(
        workload::count_books(&replayed),
        (w.final_orders, w.final_levels, w.final_shares)
    );
}

fn shares(e: &Expected) -> u64 {
    e.map.values().map(|&(_, _, qty)| u64::from(qty)).sum()
}

/// One map operation for each lookup `OrderBook::apply` makes in its order map.
#[test]
fn map_operations_follow_the_books_lookups() {
    let cmds = [
        (0, lim(1, Buy, 10, 5)), // duplicate check, insert
        (0, lim(2, Buy, 10, 0)), // rejected for zero quantity before any lookup
        (
            0,
            Market {
                id: 3,
                side: Sell,
                qty: 0,
            },
        ), // likewise
        (0, lim(4, Sell, 10, 7)), // duplicate check, fills maker 1 (get, remove), rests 2
        (0, lim(5, Buy, 10, 1)), // duplicate check, partly fills maker 4 (get and update)
    ];
    let (w, _) = derive(&cmds);
    use MapOp::{Get, Insert, Remove, Update};
    assert_eq!(
        w.map_ops,
        [
            Get { book: 0, id: 1 },
            Insert {
                book: 0,
                id: 1,
                side: Buy,
                price: 10,
                qty: 5
            },
            Get { book: 0, id: 4 },
            Get { book: 0, id: 1 },
            Remove { book: 0, id: 1 },
            Insert {
                book: 0,
                id: 4,
                side: Sell,
                price: 10,
                qty: 2
            },
            Get { book: 0, id: 5 },
            Update {
                book: 0,
                id: 4,
                qty: 1
            },
        ]
    );
}

#[test]
fn derivation_covers_every_operation_kind() {
    let (w, _) = derive(&commands());
    let has = |f: &dyn Fn(&LevelOp) -> bool| w.level_ops.iter().any(f);
    assert!(has(&|op| matches!(op, LevelOp::Push { .. })));
    assert!(has(&|op| matches!(op, LevelOp::Front { .. })));
    assert!(has(&|op| matches!(op, LevelOp::Reduce { .. })));
    assert!(has(&|op| matches!(op, LevelOp::PopFront { .. })));
    assert!(has(&|op| matches!(op, LevelOp::Remove { .. })));
    assert!(has(&|op| matches!(op, LevelOp::Reset { .. })));
    for want in 0..4 {
        assert!(w.side_ops.iter().any(|op| want
            == match op {
                SideOp::Best { .. } => 0,
                SideOp::Find { .. } => 1,
                SideOp::FindOrCreate { .. } => 2,
                SideOp::Remove { .. } => 3,
            }));
        assert!(w.map_ops.iter().any(|op| want
            == match op {
                MapOp::Insert { .. } => 0,
                MapOp::Get { .. } => 1,
                MapOp::Update { .. } => 2,
                MapOp::Remove { .. } => 3,
            }));
    }
}

#[test]
fn encoding_round_trips() {
    let (w, _) = derive(&commands());
    let sections = codec::encode(&w);
    assert_eq!(codec::decode(&sections).expect("decodes"), w);
    let mut cut = sections.clone();
    cut[3].pop();
    assert!(codec::decode(&cut).is_err());
}

#[cfg(feature = "internals")]
#[test]
fn adapters_rebuild_the_books_state() {
    use workload::parts;

    let (w, books) = derive(&commands());
    let e = expected(&books);

    let sides = parts::run_sides(&w);
    assert_eq!(parts::count_sides(&sides), e.side_levels.len());
    let level_parts = parts::run_levels(&w, &parts::push_books(&w));
    assert_eq!(
        parts::count_levels(&level_parts),
        (e.map.len(), e.side_levels.len(), shares(&e))
    );
    let mut levels: Vec<(Vec<OrderId>, u64)> = (0..)
        .zip(&level_parts.levels)
        .filter(|(_, l)| !l.is_empty())
        .map(|(slot, l)| (level_parts.ids(slot).collect(), l.total()))
        .collect();
    levels.sort();
    assert_eq!(levels, e.queues);
    let maps = parts::run_maps(&w);
    assert_eq!(parts::count_maps(&maps), (e.map.len(), shares(&e)));
    let got: HashMap<(BookIdx, OrderId), (Side, Price, Qty)> = maps
        .iter()
        .enumerate()
        .flat_map(|(b, m)| {
            m.iter()
                .map(move |(&id, o)| ((b as BookIdx, id), (o.side, o.price, o.qty)))
        })
        .collect();
    assert_eq!(got, e.map);
}

/// The benchmark's check after each pass compares these share totals with the workload's, so an
/// adapter that skipped quantity changes fails it.
#[cfg(feature = "internals")]
#[test]
fn share_totals_catch_skipped_updates_and_reduces() {
    use workload::parts;

    let (mut w, _) = derive(&commands());
    w.map_ops.retain(|op| !matches!(op, MapOp::Update { .. }));
    assert_ne!(parts::count_maps(&parts::run_maps(&w)).1, w.final_shares);
    w.level_ops
        .retain(|op| !matches!(op, LevelOp::Reduce { .. }));
    assert_ne!(
        parts::count_levels(&parts::run_levels(&w, &parts::push_books(&w))).2,
        w.final_shares
    );
}
