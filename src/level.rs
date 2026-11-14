//! The queue of orders resting at one price, oldest first.
//!
//! The level stores ids only; each order's remaining quantity lives in the order map. The level
//! keeps a running total of its orders' quantity, which the caller keeps in step through
//! `push_back`, `reduce`, `remove` and `pop_front`.
//!
//! Removal is lazy. Most orders are cancelled from the middle of their queue, so `remove` does
//! not touch the queue: the order's entry stays behind as a dead entry and only the live count
//! and the total change. Each entry carries the sequence number the level gave that resting of
//! the order, which the order map also records; an entry is live while the map holds its id at
//! this side and price with the same sequence number. Readers skip dead entries, `pop_dead_front`
//! drops them when they reach the front, and `push_back` compacts the queue once dead entries
//! outnumber live ones by `SLACK`, so the queue never holds more than twice the level's peak number
//! of live orders plus `SLACK` entries. The level itself is dropped once no live order is left.
//!
//! Sequence numbers increase from the front of the queue to the back and are unique within it.
//! When they would wrap, `push_back` drops the dead entries and renumbers the live ones, so an
//! order that rests at the same level again can never be mistaken for its old dead entry.

use std::collections::VecDeque;

use crate::orders::{OrderMap, RestingOrder};
use crate::types::{OrderId, Price, Qty, Side};

/// `push_back` compacts the queue when it holds at least this many more dead entries than live.
const SLACK: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: OrderId,
    pub seq: u32,
}

impl Entry {
    /// Whether this entry is the order's current resting at the level `side`, `price`.
    pub fn is_live(self, orders: &OrderMap, side: Side, price: Price) -> bool {
        orders
            .get(&self.id)
            .is_some_and(|order| self.matches(order, side, price))
    }

    /// Whether `order`, the mapped order with this entry's id, is this entry's resting at the
    /// level `side`, `price`.
    pub fn matches(self, order: &RestingOrder, side: Side, price: Price) -> bool {
        order.seq == self.seq && order.price == price && order.side == side
    }
}

#[derive(Debug, Default)]
pub struct Level {
    total: u64,
    /// Number of live entries in `queue`.
    live: u32,
    /// The sequence number the next pushed order gets.
    next_seq: u32,
    queue: VecDeque<Entry>,
}

impl Level {
    /// Appends `id` to this level at `side`, `price` and returns the sequence number the caller
    /// must record with the order in `orders`. Compacts or renumbers the queue first when due,
    /// which updates the sequence numbers of the orders already resting here.
    pub fn push_back(
        &mut self,
        orders: &mut OrderMap,
        side: Side,
        price: Price,
        id: OrderId,
        qty: Qty,
    ) -> u32 {
        let live = self.live as usize;
        if self.next_seq == u32::MAX {
            self.queue.retain(|e| e.is_live(orders, side, price));
            for (seq, entry) in (0..).zip(self.queue.iter_mut()) {
                entry.seq = seq;
                orders.get_mut(&entry.id).expect("live entry is mapped").seq = seq;
            }
            self.next_seq = self.live;
        } else if self.queue.len() - live >= live + SLACK {
            self.queue.retain(|e| e.is_live(orders, side, price));
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.queue.push_back(Entry { id, seq });
        self.live = self
            .live
            .checked_add(1)
            .expect("fewer than 2^32 orders at one level");
        self.total += u64::from(qty);
        seq
    }

    /// The front entry, live or dead. `None` only when the queue holds no entries at all.
    pub fn front_entry(&self) -> Option<Entry> {
        self.queue.front().copied()
    }

    /// The oldest live order at this level, which sits at `side`, `price`.
    /// shortcut: takes `&self`, so it skips dead entries at the front without dropping them, and
    /// repeats that scan on every call until a fill or a compaction drops them; upgrade if
    /// `front_order` shows in a profile, by trimming the front in `remove` with the order map.
    pub fn front(&self, orders: &OrderMap, side: Side, price: Price) -> Option<OrderId> {
        if self.live == 0 {
            return None;
        }
        self.queue
            .iter()
            .find(|e| e.is_live(orders, side, price))
            .map(|e| e.id)
    }

    /// Removes the front entry, which must be live and already reduced to zero.
    pub fn pop_front(&mut self) {
        self.queue.pop_front();
        self.live -= 1;
    }

    /// Removes the front entry, which must be dead.
    pub fn pop_dead_front(&mut self) {
        self.queue.pop_front();
    }

    /// Lowers the total after one of this level's orders lost `qty` shares.
    pub fn reduce(&mut self, qty: Qty) {
        self.total -= u64::from(qty);
    }

    /// Removes `id`, which has `remaining` shares, from anywhere in the queue. The caller must
    /// already have taken it out of the order map, which is what makes its entry dead. The entry
    /// is dropped at once only if it is at the front; otherwise readers skip it until it reaches
    /// the front or a compaction drops it.
    pub fn remove(&mut self, id: OrderId, remaining: Qty) {
        self.live -= 1;
        self.reduce(remaining);
        // The front entry with this id is either the order's own entry or an older dead one.
        if self.queue.front().is_some_and(|e| e.id == id) {
            self.queue.pop_front();
        }
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn len(&self) -> usize {
        self.live as usize
    }

    /// The live orders at this level, which sits at `side`, `price`, oldest first.
    pub fn ids<'a>(
        &'a self,
        orders: &'a OrderMap,
        side: Side,
        price: Price,
    ) -> impl Iterator<Item = OrderId> + 'a {
        self.queue
            .iter()
            .filter(move |e| e.is_live(orders, side, price))
            .map(|e| e.id)
    }

    /// Checks the queue's own invariants: the live count matches the live entries, and sequence
    /// numbers increase strictly from front to back and stay below `next_seq`.
    pub fn check(&self, orders: &OrderMap, side: Side, price: Price) -> Result<(), String> {
        let live = self.ids(orders, side, price).count();
        if live != self.live as usize {
            return Err(format!(
                "{side:?} level {price} counts {} live orders but holds {live}",
                self.live
            ));
        }
        let mut bound = self.next_seq;
        for entry in self.queue.iter().rev() {
            if entry.seq >= bound {
                return Err(format!(
                    "{side:?} level {price} sequence numbers are out of order at order {}",
                    entry.id
                ));
            }
            bound = entry.seq;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIDE: Side = Side::Buy;
    const PRICE: Price = 100;

    #[derive(Default)]
    struct Fixture {
        level: Level,
        orders: OrderMap,
    }

    impl Fixture {
        fn push(&mut self, id: OrderId, qty: Qty) {
            let seq = self.level.push_back(&mut self.orders, SIDE, PRICE, id, qty);
            let order = RestingOrder {
                side: SIDE,
                price: PRICE,
                qty,
                seq,
            };
            assert!(self.orders.insert(id, order).is_none());
        }

        fn remove(&mut self, id: OrderId) {
            let order = self.orders.remove(&id).expect("order rests");
            self.level.remove(id, order.qty);
        }

        /// Takes the front order off as a fill would, dropping dead entries on the way.
        fn fill_front(&mut self) -> OrderId {
            loop {
                let entry = self.level.front_entry().expect("an order rests");
                if entry.is_live(&self.orders, SIDE, PRICE) {
                    let qty = self.orders.remove(&entry.id).unwrap().qty;
                    self.level.reduce(qty);
                    self.level.pop_front();
                    return entry.id;
                }
                self.level.pop_dead_front();
            }
        }

        fn ids(&self) -> Vec<OrderId> {
            self.level.ids(&self.orders, SIDE, PRICE).collect()
        }

        fn assert_state(&self, ids: &[OrderId]) {
            assert_eq!(self.ids(), ids);
            assert_eq!(self.level.len(), ids.len());
            assert_eq!(self.level.is_empty(), ids.is_empty());
            assert_eq!(
                self.level.front(&self.orders, SIDE, PRICE),
                ids.first().copied()
            );
            let total: u64 = ids.iter().map(|id| u64::from(self.orders[id].qty)).sum();
            assert_eq!(self.level.total(), total);
            self.level.check(&self.orders, SIDE, PRICE).unwrap();
        }
    }

    fn filled(ids: impl IntoIterator<Item = OrderId>) -> Fixture {
        let mut f = Fixture::default();
        for id in ids {
            f.push(id, id as Qty);
        }
        f
    }

    #[test]
    fn remove_front_middle_and_back() {
        let mut f = filled(1..=5);
        f.remove(1);
        f.assert_state(&[2, 3, 4, 5]);
        f.remove(3);
        f.assert_state(&[2, 4, 5]);
        f.remove(5);
        f.assert_state(&[2, 4]);
        f.remove(2);
        f.assert_state(&[4]);
        f.remove(4);
        f.assert_state(&[]);
    }

    #[test]
    fn removing_the_front_drops_its_entry_at_once() {
        let mut f = filled(1..=3);
        f.remove(1);
        assert_eq!(f.level.front_entry().map(|e| e.id), Some(2));
    }

    #[test]
    fn fills_skip_dead_entries_in_priority_order() {
        let mut f = filled(1..=6);
        f.remove(2);
        f.remove(3);
        f.remove(5);
        assert_eq!(f.fill_front(), 1);
        assert_eq!(f.fill_front(), 4);
        f.assert_state(&[6]);
        assert_eq!(f.fill_front(), 6);
        f.assert_state(&[]);
    }

    #[test]
    fn remove_then_refill_queues_at_the_back() {
        let mut f = filled(1..=4);
        f.remove(2);
        f.remove(4);
        f.push(7, 7);
        f.push(8, 8);
        f.assert_state(&[1, 3, 7, 8]);
    }

    #[test]
    fn an_id_that_rests_again_is_not_its_old_dead_entry() {
        let mut f = filled(1..=3);
        f.remove(2);
        f.push(2, 9);
        f.assert_state(&[1, 3, 2]);
        f.remove(1);
        f.remove(3);
        f.assert_state(&[2]);
        assert_eq!(f.fill_front(), 2);
        f.assert_state(&[]);
    }

    #[test]
    fn long_queue_cancelled_from_the_middle_stays_bounded() {
        let mut f = filled(1..=500);
        for id in 2..=499 {
            f.remove(id);
        }
        f.assert_state(&[1, 500]);
        // The next push compacts the dead entries away.
        f.push(501, 1);
        assert_eq!(f.level.queue.len(), 3);
        f.assert_state(&[1, 500, 501]);
        // Steady churn behind a long-lived front order never grows the queue past the bound.
        for id in 502..5_000 {
            f.push(id, 1);
            f.remove(id - 1);
            assert!(f.level.queue.len() <= 2 * f.level.len() + SLACK + 1);
        }
        f.assert_state(&[1, 500, 4_999]);
    }

    #[test]
    fn sequence_numbers_renumber_before_they_wrap() {
        let mut f = filled(1..=4);
        f.remove(2);
        f.level.next_seq = u32::MAX - 1;
        f.push(5, 5);
        f.remove(3);
        f.push(3, 3);
        assert_eq!(f.level.next_seq, 4);
        assert_eq!(f.level.queue.len(), 4);
        f.assert_state(&[1, 4, 5, 3]);
        f.remove(4);
        f.push(4, 4);
        f.assert_state(&[1, 5, 3, 4]);
    }
}
