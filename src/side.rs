//! Price-level lookup for one side of the book.
//!
//! The levels live in one vector sorted from the worst price to the best, so the best level is
//! the last element. Most activity is within a few levels of the best: a lookup scans a few
//! elements back from the end before it falls back to a binary search, and creating or removing
//! a level near the best moves only the few levels better than it.

use crate::level::Level;
use crate::types::{OrderId, Price, Qty, Side};

/// How many levels back from the best a lookup compares one by one before it binary-searches
/// the rest.
const SCAN: usize = 8;

#[derive(Debug)]
pub struct BookSide {
    /// XORed into a price to make its sort key: zero for bids, all ones for asks. The key grows
    /// as the price gets better on either side, over the whole `u32` range, so the best level is
    /// always last.
    flip: u32,
    /// `(key, level)`, sorted by key, worst price first.
    /// shortcut: creating or removing a level moves every level better than it, 48 bytes each, so
    /// a level created behind thousands of others costs a memmove of that whole tail; if deep
    /// churn on very deep books shows in the profile, keep far levels in a separate tree.
    levels: Vec<(u32, Level)>,
}

impl BookSide {
    pub fn new(side: Side) -> Self {
        Self {
            flip: match side {
                Side::Buy => 0,
                Side::Sell => u32::MAX,
            },
            levels: Vec::new(),
        }
    }

    /// `Ok(index)` of the level at the price with sort key `key`, or `Err(index)` where it
    /// would be inserted.
    fn find(&self, key: u32) -> Result<usize, usize> {
        let split = self.levels.len().saturating_sub(SCAN);
        for i in (split..self.levels.len()).rev() {
            let k = self.levels[i].0;
            if k == key {
                return Ok(i);
            }
            if k < key {
                return Err(i + 1);
            }
        }
        // Every scanned level is better than `key`, so its place is in the rest.
        self.levels[..split].binary_search_by_key(&key, |&(k, _)| k)
    }

    /// Highest bid or lowest ask.
    pub fn best_price(&self) -> Option<Price> {
        self.levels.last().map(|&(key, _)| key ^ self.flip)
    }

    pub fn level(&self, price: Price) -> Option<&Level> {
        let i = self.find(price ^ self.flip).ok()?;
        Some(&self.levels[i].1)
    }

    pub fn level_mut(&mut self, price: Price) -> Option<&mut Level> {
        let i = self.find(price ^ self.flip).ok()?;
        Some(&mut self.levels[i].1)
    }

    pub fn level_or_insert(&mut self, price: Price) -> &mut Level {
        let key = price ^ self.flip;
        let i = match self.find(key) {
            Ok(i) => i,
            Err(i) => {
                self.levels.insert(i, (key, Level::default()));
                i
            }
        };
        &mut self.levels[i].1
    }

    pub fn remove_level(&mut self, price: Price) {
        if let Ok(i) = self.find(price ^ self.flip) {
            self.levels.remove(i);
        }
    }

    /// Removes a resting order and drops its level if that left it empty.
    pub fn remove_order(&mut self, price: Price, id: OrderId, remaining: Qty) {
        let i = self.find(price ^ self.flip).expect("order's level exists");
        let level = &mut self.levels[i].1;
        level.remove(id, remaining);
        if level.is_empty() {
            self.levels.remove(i);
        }
    }

    /// Levels from the best price outwards.
    pub fn levels_best_first(&self) -> impl Iterator<Item = (Price, &Level)> + '_ {
        let flip = self.flip;
        self.levels
            .iter()
            .rev()
            .map(move |(key, level)| (key ^ flip, level))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::BookSide;
    use crate::types::{OrderId, Price, Qty, Side};

    const SIDES: [Side; 2] = [Side::Buy, Side::Sell];

    fn prices(side: &BookSide) -> Vec<Price> {
        side.levels_best_first().map(|(price, _)| price).collect()
    }

    fn add(side: &mut BookSide, price: Price, id: OrderId, qty: Qty) {
        side.level_or_insert(price).push_back(id, qty);
    }

    /// `prices` sorted best first for `side`.
    fn best_first(side: Side, prices: &[Price]) -> Vec<Price> {
        let mut sorted = prices.to_vec();
        sorted.sort_unstable();
        if side == Side::Buy {
            sorted.reverse();
        }
        sorted
    }

    #[test]
    fn empty_side() {
        for s in SIDES {
            let mut side = BookSide::new(s);
            assert_eq!(side.best_price(), None);
            assert!(side.level(100).is_none());
            assert!(side.level_mut(100).is_none());
            assert!(prices(&side).is_empty());
            side.remove_level(100);
            assert_eq!(side.best_price(), None);
        }
    }

    #[test]
    fn one_level() {
        for s in SIDES {
            let mut side = BookSide::new(s);
            add(&mut side, 100, 1, 10);
            add(&mut side, 100, 2, 5);
            assert_eq!(side.best_price(), Some(100));
            assert_eq!(prices(&side), [100]);
            assert_eq!(side.level(100).map(|l| l.total()), Some(15));
            assert!(side.level(99).is_none());
            assert!(side.level(101).is_none());
            side.remove_order(100, 1, 10);
            assert_eq!(side.best_price(), Some(100));
            side.remove_order(100, 2, 5);
            assert_eq!(side.best_price(), None);
            assert!(prices(&side).is_empty());
        }
    }

    #[test]
    fn insert_below_and_above_the_best() {
        for s in SIDES {
            let mut side = BookSide::new(s);
            for (id, price) in [100, 90, 110, 95, 105].into_iter().enumerate() {
                add(&mut side, price, id as OrderId, 1);
            }
            let want = best_first(s, &[100, 90, 110, 95, 105]);
            assert_eq!(side.best_price(), Some(want[0]));
            assert_eq!(prices(&side), want);
        }
    }

    #[test]
    fn remove_best_and_remove_last() {
        for s in SIDES {
            let mut side = BookSide::new(s);
            for price in [100, 101, 102] {
                add(&mut side, price, OrderId::from(price), 1);
            }
            let want = best_first(s, &[100, 101, 102]);
            // Remove the best the way matching does: empty the level, then drop it.
            let best = side.best_price().expect("not empty");
            let level = side.level_mut(best).expect("best level exists");
            level.reduce(1);
            level.pop_front();
            side.remove_level(best);
            assert_eq!(side.best_price(), Some(want[1]));
            assert_eq!(prices(&side), want[1..]);
            // Remove the worst through a cancel.
            side.remove_order(want[2], OrderId::from(want[2]), 1);
            assert_eq!(prices(&side), want[1..2]);
            side.remove_order(want[1], OrderId::from(want[1]), 1);
            assert_eq!(side.best_price(), None);
        }
    }

    #[test]
    fn extreme_prices() {
        let extremes = [0, 1, u32::MAX - 1, u32::MAX, 100];
        for s in SIDES {
            let mut side = BookSide::new(s);
            for (id, price) in extremes.into_iter().enumerate() {
                add(&mut side, price, id as OrderId, 1);
            }
            let want = best_first(s, &extremes);
            assert_eq!(prices(&side), want);
            assert_eq!(side.best_price(), Some(want[0]));
            for price in extremes {
                assert_eq!(side.level(price).map(|l| l.len()), Some(1));
            }
            assert!(side.level(2).is_none());
            assert!(side.level(u32::MAX - 2).is_none());
        }
    }

    /// SplitMix64, so the stream is the same on every run.
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
    }

    /// Mostly a narrow band of penny prices, some anywhere in the `u32` range, and the extremes,
    /// so a side grows to thousands of levels with most of the churn near the best.
    fn price(rng: &mut Rng) -> Price {
        match rng.below(10) {
            0..=6 => 1_000_000 + 100 * rng.below(60) as Price,
            7 | 8 => rng.next() as Price,
            _ => [0, 1, u32::MAX - 1, u32::MAX][rng.below(4) as usize],
        }
    }

    /// Random adds, cancels and best-level drains against a `BTreeMap` model, comparing the best
    /// price and a lookup after each step and the full level list regularly.
    #[test]
    fn matches_a_btreemap_model() {
        for s in SIDES {
            let mut rng = Rng(7);
            let mut side = BookSide::new(s);
            let mut model: BTreeMap<Price, Vec<(OrderId, Qty)>> = BTreeMap::new();
            let mut live: Vec<(Price, OrderId, Qty)> = Vec::new();
            let mut max_levels = 0;
            for step in 0..30_000u64 {
                match rng.below(20) {
                    0..=12 => {
                        let (p, qty) = (price(&mut rng), 1 + rng.below(100) as Qty);
                        add(&mut side, p, step, qty);
                        model.entry(p).or_default().push((step, qty));
                        live.push((p, step, qty));
                    }
                    13..=18 if !live.is_empty() => {
                        let (p, id, qty) = live.swap_remove(rng.below(live.len() as u64) as usize);
                        side.remove_order(p, id, qty);
                        let queue = model.get_mut(&p).expect("model has the level");
                        queue.retain(|&(queued, _)| queued != id);
                        if queue.is_empty() {
                            model.remove(&p);
                        }
                    }
                    _ => {
                        if let Some(best) = side.best_price() {
                            let level = side.level_mut(best).expect("best level exists");
                            for (id, qty) in model.remove(&best).expect("model has the best") {
                                assert_eq!(level.front(), Some(id));
                                level.reduce(qty);
                                level.pop_front();
                            }
                            assert!(level.is_empty());
                            side.remove_level(best);
                            live.retain(|&(p, _, _)| p != best);
                        }
                    }
                }
                let want_best = match s {
                    Side::Buy => model.keys().next_back(),
                    Side::Sell => model.keys().next(),
                };
                assert_eq!(side.best_price(), want_best.copied(), "step {step}");
                let probe = price(&mut rng);
                assert_eq!(
                    side.level(probe).map(|l| l.len()),
                    model.get(&probe).map(Vec::len),
                    "step {step}, price {probe}"
                );
                max_levels = max_levels.max(model.len());
                if step % 256 == 0 {
                    let keys: Vec<Price> = model.keys().copied().collect();
                    let want: Vec<(Price, u64, Vec<OrderId>)> = best_first(s, &keys)
                        .into_iter()
                        .map(|p| {
                            let queue = &model[&p];
                            let total = queue.iter().map(|&(_, q)| u64::from(q)).sum();
                            (p, total, queue.iter().map(|&(id, _)| id).collect())
                        })
                        .collect();
                    let got: Vec<(Price, u64, Vec<OrderId>)> = side
                        .levels_best_first()
                        .map(|(p, l)| (p, l.total(), l.ids().collect()))
                        .collect();
                    assert_eq!(got, want, "step {step}");
                }
            }
            assert!(max_levels > 1_000, "reached only {max_levels} levels");
        }
    }
}
