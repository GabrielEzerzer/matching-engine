//! The queue of orders resting at one price, oldest first.
//!
//! Each queue is a doubly linked list threaded through `Nodes`, one slab per book: a node per
//! resting order holds its id and the slab indices of its neighbours, and each level owns a
//! sentinel node that closes its list into a ring. The order map records each order's slab
//! index, so an order leaves its queue from anywhere in O(1): `Nodes::unlink` rewires its two
//! neighbours and frees its node. Freed nodes go on a free list and are reused, so the slab
//! holds at most the book's peak number of resting orders plus levels.
//!
//! The level stores ids only; each order's remaining quantity lives in the order map. The level
//! keeps its order count and a running total of its orders' quantity, which the caller keeps in
//! step through `push_back`, `reduce`, `remove` and `pop_front`.

use crate::types::{OrderId, Qty};

/// The null slab index: no sentinel yet, or the end of the free list.
const NIL: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
struct Node {
    id: OrderId,
    /// The previous node in the level's ring. Unused while the node is free.
    prev: u32,
    /// The next node in the level's ring, or the next free node.
    next: u32,
}

/// The slab every level of one book keeps its queue nodes in.
#[derive(Debug)]
pub struct Nodes {
    nodes: Vec<Node>,
    /// The first free node, or `NIL`.
    free: u32,
}

impl Default for Nodes {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            free: NIL,
        }
    }
}

impl Nodes {
    /// A fresh node holding `id`, linked to itself.
    /// shortcut: `u32` indices cap a book at 2^32 - 1 resting orders plus levels; widen them if a
    /// single instrument ever needs more.
    fn alloc(&mut self, id: OrderId) -> u32 {
        let slot = self.free;
        if slot == NIL {
            let slot = u32::try_from(self.nodes.len())
                .ok()
                .filter(|&slot| slot != NIL)
                .expect("fewer than 2^32 - 1 queue nodes per book");
            self.nodes.push(Node {
                id,
                prev: slot,
                next: slot,
            });
            return slot;
        }
        self.free = self.nodes[slot as usize].next;
        self.nodes[slot as usize] = Node {
            id,
            prev: slot,
            next: slot,
        };
        slot
    }

    fn release(&mut self, slot: u32) {
        self.nodes[slot as usize].next = self.free;
        self.free = slot;
    }

    /// Takes the order at `slot` out of its level's queue and frees its node, and the level's
    /// sentinel too if the queue is now empty. The caller then calls `Level::remove`.
    pub fn unlink(&mut self, slot: u32) {
        let Node { prev, next, .. } = self.nodes[slot as usize];
        self.nodes[prev as usize].next = next;
        self.nodes[next as usize].prev = prev;
        self.release(slot);
        // Only the node of a one-order queue has the same node, the sentinel, on both sides.
        if prev == next {
            self.release(prev);
        }
    }

    /// The number of nodes in use: one per resting order and one per level.
    pub fn in_use(&self) -> Result<usize, String> {
        let mut free = 0;
        let mut slot = self.free;
        while slot != NIL {
            free += 1;
            if free > self.nodes.len() {
                return Err("the queue node free list has a cycle".to_string());
            }
            slot = self.nodes[slot as usize].next;
        }
        Ok(self.nodes.len() - free)
    }
}

#[derive(Debug)]
pub struct Level {
    total: u64,
    /// This level's sentinel node in `Nodes`, allocated by the first `push_back`.
    sentinel: u32,
    len: u32,
}

impl Default for Level {
    fn default() -> Self {
        Self {
            total: 0,
            sentinel: NIL,
            len: 0,
        }
    }
}

impl Level {
    /// Appends `id` and returns its slab index, which the caller records with the order.
    pub fn push_back(&mut self, nodes: &mut Nodes, id: OrderId, qty: Qty) -> u32 {
        if self.sentinel == NIL {
            self.sentinel = nodes.alloc(0);
        }
        let slot = nodes.alloc(id);
        let tail = nodes.nodes[self.sentinel as usize].prev;
        nodes.nodes[slot as usize].prev = tail;
        nodes.nodes[slot as usize].next = self.sentinel;
        nodes.nodes[tail as usize].next = slot;
        nodes.nodes[self.sentinel as usize].prev = slot;
        self.len += 1;
        self.total += u64::from(qty);
        slot
    }

    pub fn front(&self, nodes: &Nodes) -> Option<OrderId> {
        if self.len == 0 {
            return None;
        }
        let head = nodes.nodes[self.sentinel as usize].next;
        Some(nodes.nodes[head as usize].id)
    }

    /// Removes the front order, which must already have been reduced to zero.
    pub fn pop_front(&mut self, nodes: &mut Nodes) {
        let head = nodes.nodes[self.sentinel as usize].next;
        nodes.unlink(head);
        self.len -= 1;
        if self.len == 0 {
            self.sentinel = NIL;
        }
    }

    /// Lowers the total after one of this level's orders lost `qty` shares.
    pub fn reduce(&mut self, qty: Qty) {
        self.total -= u64::from(qty);
    }

    /// Accounts for the removal of an order with `remaining` shares from anywhere in the queue.
    /// The caller must already have unlinked its node with `Nodes::unlink`.
    /// shortcut: removal is split in two because `BookSide::remove_order` passes the id, not the
    /// slab index; fold `unlink` into this method once that signature can carry the index.
    pub fn remove(&mut self, _id: OrderId, remaining: Qty) {
        self.len -= 1;
        self.reduce(remaining);
        if self.len == 0 {
            self.sentinel = NIL;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// The orders at this level, oldest first, with their slab indices.
    pub fn slots<'a>(&self, nodes: &'a Nodes) -> impl Iterator<Item = (u32, OrderId)> + 'a {
        let sentinel = self.sentinel;
        let mut slot = if sentinel == NIL {
            NIL
        } else {
            nodes.nodes[sentinel as usize].next
        };
        std::iter::from_fn(move || {
            (slot != sentinel).then(|| {
                let node = nodes.nodes[slot as usize];
                let item = (slot, node.id);
                slot = node.next;
                item
            })
        })
    }

    pub fn ids<'a>(&self, nodes: &'a Nodes) -> impl Iterator<Item = OrderId> + 'a {
        self.slots(nodes).map(|(_, id)| id)
    }

    /// Checks the queue's own invariants: a non-empty level has a sentinel, its ring's links
    /// agree in both directions and the ring holds exactly `len` orders.
    pub fn check(&self, nodes: &Nodes) -> Result<(), String> {
        let in_slab = |slot: u32| (slot as usize) < nodes.nodes.len();
        if self.len == 0 || !in_slab(self.sentinel) {
            return Err(format!(
                "level with {} orders has sentinel {}",
                self.len, self.sentinel
            ));
        }
        let mut slot = self.sentinel;
        for _ in 0..=self.len {
            let next = nodes.nodes[slot as usize].next;
            if !in_slab(next) || nodes.nodes[next as usize].prev != slot {
                return Err(format!("queue links disagree after node {slot}"));
            }
            slot = next;
        }
        if slot != self.sentinel {
            return Err(format!(
                "level's queue ring does not close after {} orders",
                self.len
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// One level, with each order's slab index and quantity as the order map would hold them.
    #[derive(Default)]
    struct Fixture {
        nodes: Nodes,
        level: Level,
        orders: HashMap<OrderId, (u32, Qty)>,
    }

    impl Fixture {
        fn push(&mut self, id: OrderId, qty: Qty) -> u32 {
            let slot = self.level.push_back(&mut self.nodes, id, qty);
            assert!(self.orders.insert(id, (slot, qty)).is_none());
            slot
        }

        fn remove(&mut self, id: OrderId) {
            let (slot, qty) = self.orders.remove(&id).expect("order rests");
            self.nodes.unlink(slot);
            self.level.remove(id, qty);
        }

        fn fill_front(&mut self) -> OrderId {
            let id = self.level.front(&self.nodes).expect("an order rests");
            let (_, qty) = self.orders.remove(&id).unwrap();
            self.level.reduce(qty);
            self.level.pop_front(&mut self.nodes);
            id
        }

        fn assert_state(&self, ids: &[OrderId]) {
            assert_eq!(self.level.ids(&self.nodes).collect::<Vec<_>>(), ids);
            assert_eq!(self.level.len(), ids.len());
            assert_eq!(self.level.is_empty(), ids.is_empty());
            assert_eq!(self.level.front(&self.nodes), ids.first().copied());
            let total: u64 = ids.iter().map(|id| u64::from(self.orders[id].1)).sum();
            assert_eq!(self.level.total(), total);
            for (slot, id) in self.level.slots(&self.nodes) {
                assert_eq!(self.orders[&id].0, slot);
            }
            // One node per order, plus the sentinel while the level is not empty.
            let sentinel = usize::from(!ids.is_empty());
            assert_eq!(self.nodes.in_use(), Ok(ids.len() + sentinel));
            if !ids.is_empty() {
                self.level.check(&self.nodes).unwrap();
            }
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
    fn fills_take_orders_in_priority_order() {
        let mut f = filled(1..=4);
        f.remove(2);
        assert_eq!(f.fill_front(), 1);
        assert_eq!(f.fill_front(), 3);
        f.assert_state(&[4]);
        assert_eq!(f.fill_front(), 4);
        f.assert_state(&[]);
    }

    #[test]
    fn remove_then_refill_queues_at_the_back() {
        let mut f = filled(1..=4);
        f.remove(2);
        f.remove(4);
        f.push(7, 7);
        f.push(2, 9);
        f.assert_state(&[1, 3, 7, 2]);
    }

    #[test]
    fn freed_slots_are_reused() {
        let mut f = filled(1..=3);
        let freed = f.orders[&2].0;
        f.remove(2);
        assert_eq!(f.push(4, 4), freed);
        f.assert_state(&[1, 3, 4]);
        // Emptying the level frees its sentinel too, and a new queue reuses every node.
        for id in [1, 3, 4] {
            f.remove(id);
        }
        f.assert_state(&[]);
        let slab = f.nodes.nodes.len();
        for id in 5..=7 {
            f.push(id, 1);
        }
        assert_eq!(f.nodes.nodes.len(), slab);
        f.assert_state(&[5, 6, 7]);
    }

    #[test]
    fn long_queue_cancelled_from_the_middle() {
        let mut f = filled(1..=500);
        for id in (2..=499)
            .rev()
            .step_by(2)
            .chain((2..=499).rev().skip(1).step_by(2))
        {
            f.remove(id);
        }
        f.assert_state(&[1, 500]);
        // Steady churn behind a long-lived front order reuses the freed nodes.
        let slab = f.nodes.nodes.len();
        for id in 501..5_000 {
            f.push(id, 1);
            f.remove(id - 1);
        }
        assert_eq!(f.nodes.nodes.len(), slab);
        f.assert_state(&[1, 4_999]);
    }

    #[test]
    fn two_levels_share_one_slab() {
        let mut f = filled(1..=3);
        let mut other = Level::default();
        let a = other.push_back(&mut f.nodes, 10, 10);
        f.remove(2);
        let b = other.push_back(&mut f.nodes, 11, 11);
        assert_eq!(other.ids(&f.nodes).collect::<Vec<_>>(), [10, 11]);
        other.check(&f.nodes).unwrap();
        assert_eq!(f.nodes.in_use(), Ok(2 + 1 + 2 + 1));
        f.nodes.unlink(a);
        other.remove(10, 10);
        f.nodes.unlink(b);
        other.remove(11, 11);
        assert!(other.is_empty());
        f.assert_state(&[1, 3]);
    }
}
