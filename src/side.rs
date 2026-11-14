//! Price-level lookup for one side of the book.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use crate::level::Level;
use crate::types::{OrderId, Price, Qty, Side};

#[derive(Debug)]
pub struct BookSide {
    side: Side,
    levels: BTreeMap<Price, Level>,
    /// The best price in `levels`, kept in step by every insert and removal so reading it walks
    /// no tree.
    best: Option<Price>,
}

impl BookSide {
    pub fn new(side: Side) -> Self {
        Self {
            side,
            levels: BTreeMap::new(),
            best: None,
        }
    }

    /// Highest bid or lowest ask.
    pub fn best_price(&self) -> Option<Price> {
        self.best
    }

    pub fn level(&self, price: Price) -> Option<&Level> {
        self.levels.get(&price)
    }

    pub fn level_mut(&mut self, price: Price) -> Option<&mut Level> {
        self.levels.get_mut(&price)
    }

    pub fn level_or_insert(&mut self, price: Price) -> &mut Level {
        let better = match (self.side, self.best) {
            (_, None) => true,
            (Side::Buy, Some(best)) => price > best,
            (Side::Sell, Some(best)) => price < best,
        };
        if better {
            self.best = Some(price);
        }
        self.levels.entry(price).or_default()
    }

    pub fn remove_level(&mut self, price: Price) {
        if self.levels.remove(&price).is_some() {
            self.dropped(price);
        }
    }

    /// Removes a resting order and drops its level if that left it empty, finding the level once.
    pub fn remove_order(&mut self, price: Price, id: OrderId, remaining: Qty) {
        let Entry::Occupied(mut entry) = self.levels.entry(price) else {
            panic!("order's level exists");
        };
        entry.get_mut().remove(id, remaining);
        if entry.get().is_empty() {
            entry.remove();
            self.dropped(price);
        }
    }

    /// Updates the cached best after the level at `price` left the map.
    fn dropped(&mut self, price: Price) {
        if self.best == Some(price) {
            self.best = match self.side {
                Side::Buy => self.levels.last_key_value(),
                Side::Sell => self.levels.first_key_value(),
            }
            .map(|(&price, _)| price);
        }
    }

    /// Levels from the best price outwards.
    pub fn levels_best_first(&self) -> Box<dyn Iterator<Item = (Price, &Level)> + '_> {
        let iter = self.levels.iter().map(|(&price, level)| (price, level));
        match self.side {
            Side::Buy => Box::new(iter.rev()),
            Side::Sell => Box::new(iter),
        }
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
