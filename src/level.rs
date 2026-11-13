//! The queue of orders resting at one price, oldest first.
//!
//! The level stores ids only; each order's remaining quantity lives in the order map. The level
//! keeps a running total of its orders' quantity, which the caller keeps in step through
//! `push_back`, `reduce`, `remove` and `pop_front`.

use std::collections::VecDeque;

use crate::types::{OrderId, Qty};

#[derive(Debug, Default)]
pub(crate) struct Level {
    total: u64,
    queue: VecDeque<OrderId>,
}

impl Level {
    pub(crate) fn push_back(&mut self, id: OrderId, qty: Qty) {
        self.queue.push_back(id);
        self.total += u64::from(qty);
    }

    pub(crate) fn front(&self) -> Option<OrderId> {
        self.queue.front().copied()
    }

    /// Removes the front order, which must already have been reduced to zero.
    pub(crate) fn pop_front(&mut self) {
        self.queue.pop_front();
    }

    /// Lowers the total after one of this level's orders lost `qty` shares.
    pub(crate) fn reduce(&mut self, qty: Qty) {
        self.total -= u64::from(qty);
    }

    /// Removes `id`, which has `remaining` shares, from anywhere in the queue.
    /// shortcut: linear scan of the queue; stage 3 measures and replaces it.
    pub(crate) fn remove(&mut self, id: OrderId, remaining: Qty) {
        let pos = self
            .queue
            .iter()
            .position(|&queued| queued == id)
            .expect("order is queued at its level");
        self.queue.remove(pos);
        self.reduce(remaining);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub(crate) fn total(&self) -> u64 {
        self.total
    }

    pub(crate) fn len(&self) -> usize {
        self.queue.len()
    }

    pub(crate) fn ids(&self) -> impl Iterator<Item = OrderId> + '_ {
        self.queue.iter().copied()
    }
}
