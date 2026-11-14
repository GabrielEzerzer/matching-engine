//! Price-level lookup for one side of the book.

use std::collections::BTreeMap;

use crate::level::Level;
use crate::types::{OrderId, Price, Qty, Side};

#[derive(Debug)]
pub(crate) struct BookSide {
    side: Side,
    levels: BTreeMap<Price, Level>,
}

impl BookSide {
    pub(crate) fn new(side: Side) -> Self {
        Self {
            side,
            levels: BTreeMap::new(),
        }
    }

    /// Highest bid or lowest ask.
    pub(crate) fn best_price(&self) -> Option<Price> {
        match self.side {
            Side::Buy => self.levels.last_key_value(),
            Side::Sell => self.levels.first_key_value(),
        }
        .map(|(&price, _)| price)
    }

    pub(crate) fn level(&self, price: Price) -> Option<&Level> {
        self.levels.get(&price)
    }

    pub(crate) fn level_mut(&mut self, price: Price) -> Option<&mut Level> {
        self.levels.get_mut(&price)
    }

    pub(crate) fn level_or_insert(&mut self, price: Price) -> &mut Level {
        self.levels.entry(price).or_default()
    }

    pub(crate) fn remove_level(&mut self, price: Price) {
        self.levels.remove(&price);
    }

    /// Removes a resting order and drops its level if that left it empty.
    pub(crate) fn remove_order(&mut self, price: Price, id: OrderId, remaining: Qty) {
        let level = self.level_mut(price).expect("order's level exists");
        level.remove(id, remaining);
        if level.is_empty() {
            self.remove_level(price);
        }
    }

    /// Levels from the best price outwards.
    pub(crate) fn levels_best_first(&self) -> Box<dyn Iterator<Item = (Price, &Level)> + '_> {
        let iter = self.levels.iter().map(|(&price, level)| (price, level));
        match self.side {
            Side::Buy => Box::new(iter.rev()),
            Side::Sell => Box::new(iter),
        }
    }
}
