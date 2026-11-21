//! Price-level lookup for one side of the book: a sorted vector of inline levels, best price last.

use crate::level::{Level, Nodes};
use crate::types::{Price, Qty, Side};

const SCAN: usize = 8;

#[derive(Debug)]
struct PriceLevel {
    key: u32,
    level: Level,
}

#[derive(Debug)]
pub struct BookSide {
    flip: u32,
    best: Price,
    entries: Vec<PriceLevel>,
}

impl BookSide {
    pub fn new(side: Side) -> Self {
        Self {
            flip: match side {
                Side::Buy => 0,
                Side::Sell => u32::MAX,
            },
            best: 0,
            entries: Vec::new(),
        }
    }

    fn find(&self, key: u32) -> Result<usize, usize> {
        let split = self.entries.len().saturating_sub(SCAN);
        for i in (split..self.entries.len()).rev() {
            let k = self.entries[i].key;
            if k == key {
                return Ok(i);
            }
            if k < key {
                return Err(i + 1);
            }
        }
        self.entries[..split].binary_search_by_key(&key, |e| e.key)
    }

    fn remove_at(&mut self, position: usize) {
        let entry = self.entries.remove(position);
        assert!(
            entry.level.is_empty() && entry.level.total() == 0,
            "only empty levels are freed"
        );
        if position == self.entries.len() {
            self.best = self.entries.last().map_or(0, |e| e.key ^ self.flip);
        }
    }

    pub fn best_price(&self) -> Option<Price> {
        (!self.entries.is_empty()).then_some(self.best)
    }

    pub fn best_level(&self) -> Option<&Level> {
        self.entries.last().map(|e| &e.level)
    }

    pub fn best_level_mut(&mut self) -> Option<(Price, &mut Level)> {
        let flip = self.flip;
        self.entries
            .last_mut()
            .map(|e| (e.key ^ flip, &mut e.level))
    }

    pub fn remove_best(&mut self) {
        if let Some(entry) = self.entries.pop() {
            assert!(
                entry.level.is_empty() && entry.level.total() == 0,
                "only empty levels are freed"
            );
            self.best = self.entries.last().map_or(0, |e| e.key ^ self.flip);
        }
    }

    #[cfg(any(test, feature = "internals"))]
    pub fn level(&self, price: Price) -> Option<&Level> {
        let i = self.find(price ^ self.flip).ok()?;
        Some(&self.entries[i].level)
    }

    pub fn level_mut(&mut self, price: Price) -> Option<&mut Level> {
        let i = self.find(price ^ self.flip).ok()?;
        Some(&mut self.entries[i].level)
    }

    pub fn level_or_insert(&mut self, price: Price) -> &mut Level {
        let key = price ^ self.flip;
        let i = match self.find(key) {
            Ok(i) => i,
            Err(i) => {
                if i == self.entries.len() {
                    self.best = price;
                }
                self.entries.insert(
                    i,
                    PriceLevel {
                        key,
                        level: Level::default(),
                    },
                );
                i
            }
        };
        &mut self.entries[i].level
    }

    #[cfg(any(test, feature = "internals"))]
    pub fn remove_level(&mut self, price: Price) {
        if let Ok(i) = self.find(price ^ self.flip) {
            self.remove_at(i);
        }
    }

    pub fn remove_order(&mut self, nodes: &mut Nodes, price: Price, slot: u32, remaining: Qty) {
        let i = self.find(price ^ self.flip).expect("order's level exists");
        let level = &mut self.entries[i].level;
        level.remove(nodes, slot, remaining);
        if level.is_empty() {
            self.remove_at(i);
        }
    }

    pub fn levels_best_first(&self) -> impl Iterator<Item = (Price, &Level)> + '_ {
        self.entries
            .iter()
            .rev()
            .map(|e| (e.key ^ self.flip, &e.level))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use super::BookSide;
    use crate::level::{Level, Nodes};
    use crate::types::{OrderId, Price, Qty, Side};

    const SIDES: [Side; 2] = [Side::Buy, Side::Sell];

    fn prices(side: &BookSide) -> Vec<Price> {
        side.levels_best_first().map(|(price, _)| price).collect()
    }

    #[derive(Default)]
    struct Queues {
        nodes: Nodes,
        slots: HashMap<OrderId, u32>,
    }

    impl Queues {
        fn add(&mut self, side: &mut BookSide, price: Price, id: OrderId, qty: Qty) {
            let level = side.level_or_insert(price);
            let slot = level.push_back(&mut self.nodes, id, qty);
            self.slots.insert(id, slot);
        }

        fn cancel(&mut self, side: &mut BookSide, price: Price, id: OrderId, remaining: Qty) {
            let slot = self.slots.remove(&id).expect("the order was added");
            side.remove_order(&mut self.nodes, price, slot, remaining);
        }

        fn pop_front(&mut self, level: &mut Level) {
            let id = level.front(&self.nodes).expect("an order rests");
            level.pop_front(&mut self.nodes);
            self.slots.remove(&id).expect("the order was added");
        }
    }

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
            let mut queues = Queues::default();
            queues.add(&mut side, 100, 1, 10);
            queues.add(&mut side, 100, 2, 5);
            assert_eq!(side.best_price(), Some(100));
            assert_eq!(prices(&side), [100]);
            assert_eq!(side.level(100).map(|l| l.total()), Some(15));
            assert!(side.level(99).is_none());
            assert!(side.level(101).is_none());
            queues.cancel(&mut side, 100, 1, 10);
            assert_eq!(side.best_price(), Some(100));
            queues.cancel(&mut side, 100, 2, 5);
            assert_eq!(side.best_price(), None);
            assert!(prices(&side).is_empty());
        }
    }

    #[test]
    fn insert_below_and_above_the_best() {
        for s in SIDES {
            let mut side = BookSide::new(s);
            let mut queues = Queues::default();
            for (id, price) in [100, 90, 110, 95, 105].into_iter().enumerate() {
                queues.add(&mut side, price, id as OrderId, 1);
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
            let mut queues = Queues::default();
            for price in [100, 101, 102] {
                queues.add(&mut side, price, OrderId::from(price), 1);
            }
            let want = best_first(s, &[100, 101, 102]);
            let best = side.best_price().expect("not empty");
            let level = side.level_mut(best).expect("best level exists");
            level.reduce(1);
            queues.pop_front(level);
            side.remove_level(best);
            assert_eq!(side.best_price(), Some(want[1]));
            assert_eq!(prices(&side), want[1..]);
            queues.cancel(&mut side, want[2], OrderId::from(want[2]), 1);
            assert_eq!(prices(&side), want[1..2]);
            queues.cancel(&mut side, want[1], OrderId::from(want[1]), 1);
            assert_eq!(side.best_price(), None);
        }
    }

    #[test]
    fn extreme_prices() {
        let extremes = [0, 1, u32::MAX - 1, u32::MAX, 100];
        for s in SIDES {
            let mut side = BookSide::new(s);
            let mut queues = Queues::default();
            for (id, price) in extremes.into_iter().enumerate() {
                queues.add(&mut side, price, id as OrderId, 1);
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

    #[test]
    fn removed_level_is_dropped_and_reinserted_cleanly() {
        for s in SIDES {
            let mut side = BookSide::new(s);
            let mut queues = Queues::default();
            queues.add(&mut side, 100, 1, 10);
            queues.add(&mut side, 101, 2, 20);
            queues.cancel(&mut side, 100, 1, 10);
            queues.add(&mut side, 99, 3, 7);
            assert_eq!(side.entries.len(), 2);
            let level = side.level(99).expect("level exists");
            assert_eq!(
                (level.total(), level.ids(&queues.nodes).collect::<Vec<_>>()),
                (7, vec![3])
            );
            assert_eq!(prices(&side), best_first(s, &[99, 101]));
        }
    }

    fn check_against(
        side: &BookSide,
        nodes: &Nodes,
        s: Side,
        model: &BTreeMap<Price, OrderId>,
        at: &str,
    ) {
        let keys: Vec<Price> = model.keys().copied().collect();
        let want = best_first(s, &keys);
        assert_eq!(side.best_price(), want.first().copied(), "{at}");
        assert_eq!(prices(side), want, "{at}");
        let probes = keys.iter().flat_map(|&p| [p - 1, p, p + 1]);
        for probe in probes.chain([0, u32::MAX]) {
            assert_eq!(
                side.level(probe).map(|l| l.ids(nodes).collect::<Vec<_>>()),
                model.get(&probe).map(|&id| vec![id]),
                "{at}, price {probe}"
            );
        }
    }

    #[test]
    fn scan_and_binary_search_boundary() {
        for s in SIDES {
            for n in [7, 8, 9, 10, 17] {
                let ascending: Vec<Price> = (0..n).map(|i| 100 + 10 * i as Price).collect();
                let descending: Vec<Price> = ascending.iter().rev().copied().collect();
                let interleaved: Vec<Price> = (0..n)
                    .map(|i| ascending[if i % 2 == 0 { i / 2 } else { n - 1 - i / 2 }])
                    .collect();
                for (name, order) in [
                    ("ascending", &ascending),
                    ("descending", &descending),
                    ("interleaved", &interleaved),
                ] {
                    for position in 0..n {
                        let at = format!("{s:?}, {n} levels, {name}, position {position}");
                        let mut side = BookSide::new(s);
                        let mut queues = Queues::default();
                        let mut model = BTreeMap::new();
                        let mut next_id: OrderId = 0;
                        for &p in order {
                            next_id += 1;
                            queues.add(&mut side, p, next_id, 1);
                            model.insert(p, next_id);
                            check_against(
                                &side,
                                &queues.nodes,
                                s,
                                &model,
                                &format!("{at}, insert {p}"),
                            );
                        }

                        let p = prices(&side)[position];
                        queues.cancel(&mut side, p, model[&p], 1);
                        model.remove(&p);
                        check_against(
                            &side,
                            &queues.nodes,
                            s,
                            &model,
                            &format!("{at}, removed {p}"),
                        );

                        next_id += 1;
                        queues.add(&mut side, p + 5, next_id, 1);
                        model.insert(p + 5, next_id);
                        assert_eq!(side.entries.len(), n, "{at}");
                        check_against(
                            &side,
                            &queues.nodes,
                            s,
                            &model,
                            &format!("{at}, inserted {}", p + 5),
                        );

                        while !model.is_empty() {
                            let p = prices(&side)[position.min(model.len() - 1)];
                            let level = side.level_mut(p).expect("level exists");
                            level.reduce(1);
                            queues.pop_front(level);
                            side.remove_level(p);
                            model.remove(&p);
                            assert_eq!(side.entries.len(), model.len(), "{at}");
                            check_against(
                                &side,
                                &queues.nodes,
                                s,
                                &model,
                                &format!("{at}, drained {p}"),
                            );
                        }
                    }
                }
            }
        }
    }

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

    fn price(rng: &mut Rng) -> Price {
        match rng.below(10) {
            0..=6 => 1_000_000 + 100 * rng.below(60) as Price,
            7 | 8 => rng.next() as Price,
            _ => [0, 1, u32::MAX - 1, u32::MAX][rng.below(4) as usize],
        }
    }

    #[test]
    fn matches_a_btreemap_model() {
        for s in SIDES {
            let mut rng = Rng(7);
            let mut side = BookSide::new(s);
            let mut queues = Queues::default();
            let mut model: BTreeMap<Price, Vec<(OrderId, Qty)>> = BTreeMap::new();
            let mut live: Vec<(Price, OrderId, Qty)> = Vec::new();
            let mut max_levels = 0;
            for step in 0..30_000u64 {
                match rng.below(20) {
                    0..=12 => {
                        let (p, qty) = (price(&mut rng), 1 + rng.below(100) as Qty);
                        queues.add(&mut side, p, step, qty);
                        model.entry(p).or_default().push((step, qty));
                        live.push((p, step, qty));
                    }
                    13..=18 if !live.is_empty() => {
                        let (p, id, qty) = live.swap_remove(rng.below(live.len() as u64) as usize);
                        queues.cancel(&mut side, p, id, qty);
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
                                assert_eq!(level.front(&queues.nodes), Some(id));
                                level.reduce(qty);
                                queues.pop_front(level);
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
                        .map(|(p, l)| (p, l.total(), l.ids(&queues.nodes).collect()))
                        .collect();
                    assert_eq!(got, want, "step {step}");
                }
            }
            assert!(max_levels > 1_000, "reached only {max_levels} levels");
        }
    }
}
