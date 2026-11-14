//! A deterministic deep-book differential test. It builds hundreds of price levels on each side
//! and long queues at a few of them, all with unique ids, then cancels, reduces, replaces,
//! sweeps, drains and repopulates the book. After every command the real book must emit the same
//! events as the slow reference book and hold the same orders in the same priority, and its
//! invariants must hold.
//!
//! The property test (`tests/prop.rs`) uses short streams over a dozen prices; this one reaches
//! the shapes an optimised book could get wrong only at depth: many levels, queues hundreds
//! long, levels emptied and reused, and the id map growing and shrinking repeatedly.

#[path = "../harness/reference.rs"]
mod reference;

use matching_engine::{Command, Event, OrderBook, OrderId, Price, Qty, Side, Snapshot};
use reference::ReferenceBook;

/// A small deterministic generator (SplitMix64), so the stream is identical on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn range(&mut self, lo: u32, hi: u32) -> u32 {
        lo + self.below(u64::from(hi - lo + 1)) as u32
    }
}

struct Harness {
    book: OrderBook,
    reference: ReferenceBook,
    next_id: OrderId,
    commands: u64,
    events: u64,
    trades: u64,
    max_orders: usize,
    max_levels: usize,
    max_queue: usize,
    /// The snapshot after the last command, used to pick live orders.
    snapshot: Snapshot,
}

impl Harness {
    fn new() -> Self {
        Self {
            book: OrderBook::new(),
            reference: ReferenceBook::new(),
            next_id: 1,
            commands: 0,
            events: 0,
            trades: 0,
            max_orders: 0,
            max_levels: 0,
            max_queue: 0,
            snapshot: Snapshot::default(),
        }
    }

    fn fresh_id(&mut self) -> OrderId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn apply(&mut self, cmd: Command) {
        let mut got = Vec::new();
        let mut want = Vec::new();
        self.book.apply(cmd, &mut got);
        self.reference.apply(cmd, &mut want);
        assert_eq!(
            got, want,
            "command {} {cmd:?}: events differ",
            self.commands
        );
        if let Err(e) = self.book.check_invariants() {
            panic!("command {} {cmd:?}: {e}", self.commands);
        }
        let snapshot = self.book.snapshot();
        assert_eq!(
            snapshot,
            self.reference.snapshot(),
            "command {} {cmd:?}: snapshots differ",
            self.commands
        );
        self.commands += 1;
        self.events += got.len() as u64;
        self.trades += got
            .iter()
            .filter(|e| matches!(e, Event::Trade { .. }))
            .count() as u64;
        let levels = snapshot.bids.len() + snapshot.asks.len();
        let orders: usize = snapshot
            .bids
            .iter()
            .chain(&snapshot.asks)
            .map(|l| l.orders.len())
            .sum();
        let queue = snapshot
            .bids
            .iter()
            .chain(&snapshot.asks)
            .map(|l| l.orders.len())
            .max()
            .unwrap_or(0);
        self.max_levels = self.max_levels.max(levels);
        self.max_orders = self.max_orders.max(orders);
        self.max_queue = self.max_queue.max(queue);
        self.snapshot = snapshot;
    }

    fn limit(&mut self, side: Side, price: Price, qty: Qty) {
        let id = self.fresh_id();
        self.apply(Command::Limit {
            id,
            side,
            price,
            qty,
        });
    }

    fn live(&self) -> Vec<(OrderId, Qty)> {
        self.snapshot
            .bids
            .iter()
            .chain(&self.snapshot.asks)
            .flat_map(|l| l.orders.iter().copied())
            .collect()
    }

    fn live_on(&self, side: Side) -> Vec<(OrderId, Qty)> {
        let levels = match side {
            Side::Buy => &self.snapshot.bids,
            Side::Sell => &self.snapshot.asks,
        };
        levels
            .iter()
            .flat_map(|l| l.orders.iter().copied())
            .collect()
    }

    fn pick(&self, rng: &mut Rng) -> Option<(OrderId, Qty)> {
        let live = self.live();
        (!live.is_empty()).then(|| live[rng.below(live.len() as u64) as usize])
    }

    fn total_qty(&self, side: Side) -> u64 {
        self.live_on(side).iter().map(|&(_, q)| u64::from(q)).sum()
    }
}

/// Bids rest at 10_000..10_000+LEVELS, asks above them, so building never crosses.
const LEVELS: u32 = 300;
const BID_LO: Price = 10_000;
const ASK_LO: Price = BID_LO + LEVELS;
const ASK_HI: Price = ASK_LO + LEVELS - 1;

fn populate(h: &mut Harness, rng: &mut Rng, bid_lo: Price, ask_lo: Price) {
    // One or two orders on every level of both sides.
    for i in 0..LEVELS {
        for _ in 0..1 + rng.below(2) {
            let q = rng.range(1, 500);
            h.limit(Side::Buy, bid_lo + i, q);
            let q = rng.range(1, 500);
            h.limit(Side::Sell, ask_lo + i, q);
        }
    }
    // Deep queues at the touch and at one level in the middle of each side.
    for _ in 0..250 {
        let q = rng.range(1, 300);
        h.limit(Side::Buy, bid_lo + LEVELS - 1, q);
        let q = rng.range(1, 300);
        h.limit(Side::Sell, ask_lo, q);
    }
    for _ in 0..120 {
        let q = rng.range(1, 300);
        h.limit(Side::Buy, bid_lo + LEVELS / 2, q);
        let q = rng.range(1, 300);
        h.limit(Side::Sell, ask_lo + LEVELS / 2, q);
    }
}

/// Random cancels, reduces, replaces and passive adds, with an occasional crossing order.
fn churn(h: &mut Harness, rng: &mut Rng, steps: usize) {
    for _ in 0..steps {
        let Some((id, qty)) = h.pick(rng) else {
            return;
        };
        match rng.below(100) {
            0..=24 => h.apply(Command::Cancel { id }),
            25..=44 => {
                // Partial reduce, or exactly what is left (removes the order).
                let by = if rng.below(4) == 0 {
                    qty
                } else {
                    rng.range(1, qty)
                };
                h.apply(Command::Reduce { id, by });
            }
            45..=64 => {
                // Replace to a random price on either side's range: usually passive, sometimes
                // crossing (it then trades). Half keep the id, half take a fresh one.
                let new_id = if rng.below(2) == 0 { id } else { h.fresh_id() };
                let price = rng.range(BID_LO, ASK_HI);
                let qty = rng.range(1, 400);
                h.apply(Command::Replace {
                    id,
                    new_id,
                    price,
                    qty,
                });
            }
            65..=94 => {
                let side = if rng.below(2) == 0 {
                    Side::Buy
                } else {
                    Side::Sell
                };
                let price = rng.range(BID_LO, ASK_HI);
                let qty = rng.range(1, 400);
                h.limit(side, price, qty);
            }
            _ => {
                let side = if rng.below(2) == 0 {
                    Side::Buy
                } else {
                    Side::Sell
                };
                let qty = rng.range(1, 3_000);
                let id = h.fresh_id();
                h.apply(Command::Market { id, side, qty });
            }
        }
    }
}

/// Large orders that each take out many levels, then one that clears the rest of a side.
fn sweep(h: &mut Harness, side: Side) {
    let opposite = match side {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    };
    for _ in 0..4 {
        let qty = (h.total_qty(opposite) / 5).max(1);
        let price = match side {
            Side::Buy => u32::MAX,
            Side::Sell => 1,
        };
        h.limit(side, price, u32::try_from(qty).unwrap_or(u32::MAX));
    }
    let rest = h.total_qty(opposite);
    if rest > 0 {
        let id = h.fresh_id();
        h.apply(Command::Market {
            id,
            side,
            qty: u32::try_from(rest).expect("fits"),
        });
    }
    assert!(h.live_on(opposite).is_empty(), "sweep cleared {opposite:?}");
}

/// Cancels every order on one side in random order, then reduces every order of the other side
/// to nothing, so both sides end empty without trading.
fn drain(h: &mut Harness, rng: &mut Rng) {
    let mut bids = h.live_on(Side::Buy);
    while !bids.is_empty() {
        let (id, _) = bids.swap_remove(rng.below(bids.len() as u64) as usize);
        h.apply(Command::Cancel { id });
    }
    let mut asks = h.live_on(Side::Sell);
    while !asks.is_empty() {
        let (id, qty) = asks.swap_remove(rng.below(asks.len() as u64) as usize);
        h.apply(Command::Reduce { id, by: qty });
    }
    assert!(h.live().is_empty());
}

#[test]
fn deep_book_matches_reference_through_build_churn_sweep_drain_and_repopulate() {
    let mut rng = Rng(0x5EED_0000_0001);
    let mut h = Harness::new();

    populate(&mut h, &mut rng, BID_LO, ASK_LO);
    churn(&mut h, &mut rng, 1_500);
    sweep(&mut h, Side::Buy);
    churn(&mut h, &mut rng, 500);
    sweep(&mut h, Side::Sell);
    drain(&mut h, &mut rng);

    // Repopulate on prices shifted so old and new levels overlap, with ids continuing upwards.
    populate(&mut h, &mut rng, BID_LO + LEVELS / 3, ASK_LO + LEVELS / 3);
    churn(&mut h, &mut rng, 1_500);
    drain(&mut h, &mut rng);
    populate(&mut h, &mut rng, BID_LO, ASK_LO);
    sweep(&mut h, Side::Sell);
    sweep(&mut h, Side::Buy);

    eprintln!(
        "deep book: {} commands, {} events, {} trades, up to {} orders on {} levels, longest queue {}",
        h.commands, h.events, h.trades, h.max_orders, h.max_levels, h.max_queue
    );
    // The stream must actually reach the depths it is meant to cover.
    assert!(
        h.max_levels >= 2 * LEVELS as usize,
        "levels {}",
        h.max_levels
    );
    assert!(h.max_queue >= 250, "queue {}", h.max_queue);
    assert!(h.max_orders >= 1_500, "orders {}", h.max_orders);
    assert!(h.trades >= 1_000, "trades {}", h.trades);
}
