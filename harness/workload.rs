//! Logical workloads for the book's parts, derived by watching real commands go through an
//! [`OrderBook`] from the outside.
//!
//! Each part gets its workload as the operations the book needs from that part, not as calls to
//! its current methods, so a later round can change a part's internals and drive the same
//! workload through a thin adapter:
//! - side (price-level lookup): best price, find the level at a price, find or create it, remove it;
//! - level (the queue at one price): push an id, look at the front, take from the front, reduce
//!   the total, remove an id from anywhere; a level that empties is dropped and its slot starts
//!   over as a new empty level;
//! - map (order id to resting order): insert, get, update the quantity, remove;
//! - book: the commands themselves.
//!
//! [`Recorder::record`] derives the operations of one command from the command, the events it
//! emitted, the book's answer to [`OrderBook::order`] for the id it names (asked before the
//! command) and for its own id and the makers it traded with (asked after). Nothing reads the
//! book's internals, so the derivation does not change when a part does.
//!
//! A replay of a part's operations is checked twice: its end state against the orders, shares
//! and levels the book held after the last command, and the results of its reads against
//! [`Workload::final_reads`], so an adapter cannot skip a read without failing.

use std::collections::HashMap;

use matching_engine::{Command, Event, OrderBook, OrderId, OrderInfo, Price, Qty, Side};

/// Index of a book within a workload (the position of its symbol in the symbol list).
pub type BookIdx = u16;

/// One operation on one side of one book. `side` is `book * 2` for bids, `book * 2 + 1` for asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideOp {
    /// The best price of the side, if any.
    Best { side: u32 },
    /// The existing level at `price`.
    Find { side: u32, price: Price },
    /// The level at `price`, created empty if there is none.
    FindOrCreate { side: u32, price: Price },
    /// Drop the level at `price`, which has just emptied.
    Remove { side: u32, price: Price },
}

/// One operation on the queue of one level. A slot holds one live level at a time; slots are
/// reused after a level is dropped, so the number of slots is the most levels alive at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelOp {
    /// Append `id` with `qty` shares at the back.
    Push { slot: u32, id: OrderId, qty: Qty },
    /// The id at the front (the next maker). After a taker with quantity left empties the level,
    /// the book looks once more and finds nothing, just before the level is dropped.
    Front { slot: u32 },
    /// One of the level's orders lost `qty` shares (the level total drops).
    Reduce { slot: u32, qty: Qty },
    /// Take the front order off, after it was reduced to zero.
    PopFront { slot: u32 },
    /// Remove `id`, which has `qty` shares left, from anywhere in the queue.
    Remove { slot: u32, id: OrderId, qty: Qty },
    /// The level emptied and was dropped; the slot starts over as a new empty level.
    Reset { slot: u32 },
}

/// One operation on the order-id map of one book.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapOp {
    Insert {
        book: BookIdx,
        id: OrderId,
        side: Side,
        price: Price,
        qty: Qty,
    },
    /// A lookup, which may miss.
    Get { book: BookIdx, id: OrderId },
    /// Set the remaining quantity of a resting order.
    Update {
        book: BookIdx,
        id: OrderId,
        qty: Qty,
    },
    /// A removal, which may miss (a cancel of an unknown order).
    Remove { book: BookIdx, id: OrderId },
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Workload {
    pub books: usize,
    pub commands: Vec<(BookIdx, Command)>,
    pub side_ops: Vec<SideOp>,
    pub level_ops: Vec<LevelOp>,
    pub level_slots: usize,
    pub map_ops: Vec<MapOp>,
    /// Orders, shares and levels resting in all books after the last command; a replay of the
    /// operations must end with the same totals.
    pub final_orders: usize,
    pub final_shares: u64,
    pub final_levels: usize,
    /// What the reads of each part returned, as the baseline adapters fold it. [`Recorder`]
    /// leaves it zero; the derivation fills it from `parts::final_reads`, which needs the
    /// `internals` feature.
    pub final_reads: Reads,
}

/// One fold (`parts::fold`) per part of the results of that part's reads, in operation order.
/// A replay of the operations must give the same folds, so an adapter that skips a read or
/// returns a wrong value from one fails even when it ends in the right state. The `book` and
/// `parse` parts have no entry: they time the library's own code, not an adapter in `parts`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reads {
    /// Best price and find-a-level.
    pub side: u64,
    /// Front.
    pub level: u64,
    /// Get and remove.
    pub map: u64,
}

/// Derives a [`Workload`] one command at a time.
pub struct Recorder {
    w: Workload,
    /// Live levels by (side index, price): their slot and how many orders rest there.
    levels: HashMap<(u32, Price), (u32, u32)>,
    free_slots: Vec<u32>,
}

fn side_index(book: BookIdx, side: Side) -> u32 {
    u32::from(book) * 2 + u32::from(side == Side::Sell)
}

fn opposite(side: Side) -> Side {
    match side {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    }
}

impl Recorder {
    pub fn new(books: usize) -> Self {
        Self {
            w: Workload {
                books,
                ..Workload::default()
            },
            levels: HashMap::new(),
            free_slots: Vec::new(),
        }
    }

    pub fn finish(mut self) -> Workload {
        self.w.final_levels = self.levels.len();
        self.w.final_orders = self.levels.values().map(|&(_, n)| n as usize).sum();
        // Every share that rests is pushed onto a level and leaves it by a reduce or a removal.
        self.w.final_shares = self.w.level_ops.iter().fold(0, |t, op| match *op {
            LevelOp::Push { qty, .. } => t + u64::from(qty),
            LevelOp::Reduce { qty, .. } | LevelOp::Remove { qty, .. } => t - u64::from(qty),
            _ => t,
        });
        self.w
    }

    /// Records the part operations of `cmd`, which has just been applied to `book` (book number
    /// `b`) and emitted `events`. `named` is what `book.order(id)` returned before the command
    /// for the id a Cancel, Reduce or Replace names; it is ignored for other commands.
    pub fn record(
        &mut self,
        b: BookIdx,
        cmd: Command,
        named: Option<OrderInfo>,
        events: &[Event],
        book: &OrderBook,
    ) {
        self.w.commands.push((b, cmd));
        let rejected = matches!(events.first(), Some(Event::Rejected { .. }));
        match cmd {
            Command::Limit {
                id,
                side,
                price,
                qty,
            } => {
                // The duplicate-id lookup; a zero quantity is rejected before it.
                if qty != 0 {
                    self.w.map_ops.push(MapOp::Get { book: b, id });
                }
                if !rejected {
                    self.submit(b, id, side, Some(price), events, book);
                }
            }
            Command::Market { id, side, qty } => {
                if qty != 0 {
                    self.w.map_ops.push(MapOp::Get { book: b, id });
                }
                if !rejected {
                    self.submit(b, id, side, None, events, book);
                }
            }
            Command::Cancel { id } => {
                self.w.map_ops.push(MapOp::Remove { book: b, id });
                if !rejected {
                    let o = named.expect("a cancel that was not rejected names a resting order");
                    self.remove_resting(b, o.side, o.price, id, o.qty);
                }
            }
            Command::Reduce { id, by } => {
                self.w.map_ops.push(MapOp::Get { book: b, id });
                if rejected {
                    return;
                }
                let o = named.expect("a reduce that was not rejected names a resting order");
                if by == o.qty {
                    self.w.map_ops.push(MapOp::Remove { book: b, id });
                    self.remove_resting(b, o.side, o.price, id, by);
                } else {
                    self.w.map_ops.push(MapOp::Update {
                        book: b,
                        id,
                        qty: o.qty - by,
                    });
                    let s = side_index(b, o.side);
                    self.w.side_ops.push(SideOp::Find {
                        side: s,
                        price: o.price,
                    });
                    let slot = self.levels[&(s, o.price)].0;
                    self.w.level_ops.push(LevelOp::Reduce { slot, qty: by });
                }
            }
            Command::Replace {
                id,
                new_id,
                price,
                qty,
            } => {
                self.w.map_ops.push(MapOp::Get { book: b, id });
                if named.is_some() && qty != 0 && new_id != id {
                    self.w.map_ops.push(MapOp::Get {
                        book: b,
                        id: new_id,
                    });
                }
                if rejected {
                    return;
                }
                let o = named.expect("a replace that was not rejected names a resting order");
                self.w.map_ops.push(MapOp::Remove { book: b, id });
                self.remove_resting(b, o.side, o.price, id, o.qty);
                self.submit(b, new_id, o.side, Some(price), events, book);
            }
        }
    }

    fn take_slot(&mut self) -> u32 {
        self.free_slots.pop().unwrap_or_else(|| {
            self.w.level_slots += 1;
            u32::try_from(self.w.level_slots - 1).expect("fewer than 2^32 live levels")
        })
    }

    /// One fewer order at the level of side `s` at `price`; drops the level if it emptied.
    fn order_left_level(&mut self, s: u32, price: Price) {
        let entry = self.levels.get_mut(&(s, price)).expect("level is live");
        entry.1 -= 1;
        if entry.1 == 0 {
            let slot = entry.0;
            self.levels.remove(&(s, price));
            self.w.side_ops.push(SideOp::Remove { side: s, price });
            self.w.level_ops.push(LevelOp::Reset { slot });
            self.free_slots.push(slot);
        }
    }

    /// Removes a resting order from its level (cancel, full reduce, the old order of a replace).
    fn remove_resting(&mut self, b: BookIdx, side: Side, price: Price, id: OrderId, qty: Qty) {
        let s = side_index(b, side);
        self.w.side_ops.push(SideOp::Find { side: s, price });
        let slot = self.levels[&(s, price)].0;
        self.w.level_ops.push(LevelOp::Remove { slot, id, qty });
        self.order_left_level(s, price);
    }

    /// The matching and resting of an accepted order `id` (a limit, a market order or the new
    /// order of a replace): one trade per maker, best price first, then the rest if it rested.
    fn submit(
        &mut self,
        b: BookIdx,
        id: OrderId,
        side: Side,
        limit: Option<Price>,
        events: &[Event],
        book: &OrderBook,
    ) {
        let o = side_index(b, opposite(side));
        let mut current: Option<Price> = None;
        let mut taker_filled = false;
        for (i, event) in events.iter().enumerate() {
            match *event {
                Event::Trade {
                    maker_id,
                    price,
                    qty,
                    ..
                } => {
                    if current != Some(price) {
                        self.w.side_ops.push(SideOp::Best { side: o });
                        self.w.side_ops.push(SideOp::Find { side: o, price });
                        current = Some(price);
                    }
                    let slot = self.levels[&(o, price)].0;
                    self.w.level_ops.push(LevelOp::Front { slot });
                    self.w.level_ops.push(LevelOp::Reduce { slot, qty });
                    if events.get(i + 1) == Some(&Event::Filled { id: maker_id }) {
                        self.w.level_ops.push(LevelOp::PopFront { slot });
                        // The book looks the maker up to fill it, then removes it.
                        self.w.map_ops.push(MapOp::Get {
                            book: b,
                            id: maker_id,
                        });
                        self.w.map_ops.push(MapOp::Remove {
                            book: b,
                            id: maker_id,
                        });
                        // While the taker has quantity left the book looks at the front again.
                        // After the level's last maker that look finds nothing, and the level is
                        // dropped after it. The taker has quantity left unless its own Filled
                        // comes straight after this maker's.
                        if self.levels[&(o, price)].1 == 1
                            && events.get(i + 2) != Some(&Event::Filled { id })
                        {
                            self.w.level_ops.push(LevelOp::Front { slot });
                        }
                        self.order_left_level(o, price);
                    } else {
                        let left = book
                            .order(maker_id)
                            .expect("a maker not filled still rests");
                        self.w.map_ops.push(MapOp::Update {
                            book: b,
                            id: maker_id,
                            qty: left.qty,
                        });
                    }
                }
                Event::Filled { id: filled } if filled == id => taker_filled = true,
                _ => {}
            }
        }
        if taker_filled {
            return;
        }
        // The look at the best price that stopped the matching.
        self.w.side_ops.push(SideOp::Best { side: o });
        let Some(price) = limit else { return };
        let rested = book.order(id).expect("an unfilled limit order rests");
        let s = side_index(b, side);
        self.w
            .side_ops
            .push(SideOp::FindOrCreate { side: s, price });
        let slot = match self.levels.get_mut(&(s, price)) {
            Some(entry) => {
                entry.1 += 1;
                entry.0
            }
            None => {
                let slot = self.take_slot();
                self.levels.insert((s, price), (slot, 1));
                slot
            }
        };
        self.w.level_ops.push(LevelOp::Push {
            slot,
            id,
            qty: rested.qty,
        });
        self.w.map_ops.push(MapOp::Insert {
            book: b,
            id,
            side,
            price,
            qty: rested.qty,
        });
    }
}

/// Applies the commands to fresh books, reusing one event buffer, and returns the books.
pub fn run_books(w: &Workload) -> Vec<OrderBook> {
    let mut books: Vec<OrderBook> = (0..w.books).map(|_| OrderBook::new()).collect();
    let mut out = Vec::with_capacity(64);
    for &(b, cmd) in &w.commands {
        out.clear();
        books[usize::from(b)].apply(cmd, &mut out);
        std::hint::black_box(out.len());
    }
    books
}

/// Resting orders, levels and shares over all books, for checking a replay against the workload.
pub fn count_books(books: &[OrderBook]) -> (usize, usize, u64) {
    let (mut orders, mut levels, mut shares) = (0, 0, 0);
    for book in books {
        for side in [Side::Buy, Side::Sell] {
            let depth = book.depth(side);
            levels += depth.len();
            orders += depth.iter().map(|l| l.order_count).sum::<usize>();
            shares += depth.iter().map(|l| l.total_qty).sum::<u64>();
        }
    }
    (orders, levels, shares)
}

/// Adapters that drive the current parts with the logical operations. A round that changes a
/// part changes only its adapter here. Each adapter returns the part's end state and the fold
/// of everything its reads returned (see `Reads`).
#[cfg(feature = "internals")]
pub mod parts {
    use std::hint::black_box;

    use matching_engine::internals::{BookSide, Level, Nodes, OrderMap, RestingOrder};
    use matching_engine::{OrderId, Side};

    use super::{BookIdx, LevelOp, MapOp, Reads, SideOp, Workload};

    /// Folded in place of a read that found nothing.
    pub const NONE: u64 = u64::MAX;

    /// Adds one value to a fold that starts at 0: a multiply by an odd constant and an add, so
    /// the fold changes with every value and with their order. It runs inside the timed passes,
    /// a multiply-add per folded value, in the baseline's adapter and in any candidate's.
    #[inline]
    pub fn fold(acc: u64, value: u64) -> u64 {
        acc.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(value)
    }

    /// Folds what the book uses of a looked-up or removed order (its side and price, then its
    /// quantity), or [`NONE`] when there was no such order.
    #[inline]
    pub fn fold_order(acc: u64, order: Option<&RestingOrder>) -> u64 {
        match order {
            Some(o) => {
                let side_price = (u64::from(o.price) << 1) | u64::from(o.side == Side::Sell);
                fold(fold(acc, side_price), u64::from(o.qty))
            }
            None => fold(acc, NONE),
        }
    }

    /// The read folds of these adapters (the baseline's) over the whole workload, which the
    /// derivation stores as [`Workload::final_reads`].
    pub fn final_reads(w: &Workload) -> Reads {
        Reads {
            side: run_sides(w).1,
            level: run_levels(w, &push_books(w)).1,
            map: run_maps(w).1,
        }
    }

    /// Folds the best price, and whether a level was found and its total. Finding or creating a
    /// level is not folded: the level it returns is empty, like every level here, and a level it
    /// creates shows in the end state.
    pub fn run_sides(w: &Workload) -> (Vec<BookSide>, u64) {
        let mut sides: Vec<BookSide> = (0..w.books)
            .flat_map(|_| [BookSide::new(Side::Buy), BookSide::new(Side::Sell)])
            .collect();
        let mut reads = 0;
        for &op in &w.side_ops {
            match op {
                SideOp::Best { side } => {
                    let best = sides[side as usize].best_price();
                    reads = fold(reads, best.map_or(NONE, u64::from));
                }
                SideOp::Find { side, price } => {
                    // shortcut: every Find in the workload hits, and side levels hold no orders,
                    // so each one folds the same 0: the fold proves a fold step ran, not that the
                    // lookup ran or found the right level (the golden hash and the tests catch a
                    // wrong level in the real part); give side levels an identity once `src/` may
                    // change.
                    let level = sides[side as usize].level_mut(price);
                    reads = fold(reads, level.map_or(NONE, |l| l.total()));
                }
                SideOp::FindOrCreate { side, price } => {
                    black_box(sides[side as usize].level_or_insert(price));
                }
                SideOp::Remove { side, price } => sides[side as usize].remove_level(price),
            }
        }
        (sides, reads)
    }

    /// Levels alive in the sides.
    pub fn count_sides(sides: &[BookSide]) -> usize {
        sides.iter().map(|s| s.levels_best_first().count()).sum()
    }

    /// The book of each `Push`, in order: the recorder pairs every push with one map insert, the
    /// order resting in that book. Prepared outside the timed region, since a real book knows
    /// which book it is.
    pub fn push_books(w: &Workload) -> Vec<BookIdx> {
        let books: Vec<BookIdx> = w
            .map_ops
            .iter()
            .filter_map(|op| match *op {
                MapOp::Insert { book, .. } => Some(book),
                _ => None,
            })
            .collect();
        let pushes = w
            .level_ops
            .iter()
            .filter(|op| matches!(op, LevelOp::Push { .. }))
            .count();
        assert_eq!(books.len(), pushes, "one map insert per level push");
        books
    }

    /// The levels and, per book, the queue slab and the order map the book keeps beside them:
    /// a removal finds its order's slab node through the map, so the adapter keeps the map as the
    /// book does (insert on push with the node, remove when the order leaves) and pays for it in
    /// the timed region. Order quantities are not kept up to date, since the queue never reads
    /// them. Level `slot` belongs to the book in `books[slot]`; the side and price stored in the
    /// map are placeholders, since the queue never reads them either.
    pub struct Levels {
        pub levels: Vec<Level>,
        pub books: Vec<BookIdx>,
        pub nodes: Vec<Nodes>,
        pub orders: Vec<OrderMap>,
    }

    impl Levels {
        /// The orders of level `slot`, oldest first.
        pub fn ids(&self, slot: u32) -> impl Iterator<Item = OrderId> + '_ {
            let nodes = &self.nodes[usize::from(self.books[slot as usize])];
            self.levels[slot as usize].ids(nodes)
        }
    }

    /// Folds the id at the front, or [`NONE`] for the look at a level just emptied. The order
    /// map's inserts and removals are not reads of the queue and are not folded.
    pub fn run_levels(w: &Workload, push_books: &[BookIdx]) -> (Levels, u64) {
        let mut levels: Vec<Level> = (0..w.level_slots).map(|_| Level::default()).collect();
        let mut books: Vec<BookIdx> = vec![0; w.level_slots];
        let mut nodes: Vec<Nodes> = (0..w.books).map(|_| Nodes::default()).collect();
        let mut orders: Vec<OrderMap> = (0..w.books).map(|_| OrderMap::new()).collect();
        let mut push_books = push_books.iter();
        let mut reads = 0;
        for &op in &w.level_ops {
            match op {
                LevelOp::Push { slot, id, qty } => {
                    let b = usize::from(*push_books.next().expect("a book for every push"));
                    books[slot as usize] = b as BookIdx;
                    let node = levels[slot as usize].push_back(&mut nodes[b], id, qty);
                    let order = RestingOrder {
                        side: Side::Buy,
                        price: slot,
                        qty,
                        slot: node,
                    };
                    orders[b].insert(id, order);
                }
                LevelOp::Front { slot } => {
                    let b = usize::from(books[slot as usize]);
                    let front = levels[slot as usize].front(&nodes[b]);
                    reads = fold(reads, front.unwrap_or(NONE));
                }
                LevelOp::Reduce { slot, qty } => levels[slot as usize].reduce(qty),
                LevelOp::PopFront { slot } => {
                    // The book pops the maker it has just looked at. The adapter looks at the
                    // front again instead of keeping the id from the last `Front`, so `Front`
                    // stays a read that changes no state, as the read-check tests assume; the
                    // extra look reads the node `pop_front` reads next.
                    let b = usize::from(books[slot as usize]);
                    let level = &mut levels[slot as usize];
                    let maker = level.front(&nodes[b]).expect("a popped level has a front");
                    level.pop_front(&mut nodes[b]);
                    orders[b].remove(&maker);
                }
                LevelOp::Remove { slot, id, qty } => {
                    let b = usize::from(books[slot as usize]);
                    let order = orders[b].remove(&id).expect("removed order is mapped");
                    nodes[b].unlink(order.slot);
                    levels[slot as usize].remove(id, qty);
                }
                LevelOp::Reset { slot } => levels[slot as usize] = Level::default(),
            }
        }
        let state = Levels {
            levels,
            books,
            nodes,
            orders,
        };
        (state, reads)
    }

    /// Orders in the levels' queues, which the order maps must hold too, levels that are not
    /// empty, and the sum of the level totals.
    pub fn count_levels(state: &Levels) -> (usize, usize, u64) {
        let orders = (0..state.levels.len() as u32)
            .map(|slot| state.ids(slot).count())
            .sum();
        let mapped: usize = state.orders.iter().map(OrderMap::len).sum();
        assert_eq!(
            mapped, orders,
            "the order maps hold exactly the queued orders"
        );
        let live = state.levels.iter().filter(|l| !l.is_empty()).count();
        let shares = state.levels.iter().map(Level::total).sum();
        (orders, live, shares)
    }

    /// Folds the order a get found and the order a removal took out ([`fold_order`]).
    pub fn run_maps(w: &Workload) -> (Vec<OrderMap>, u64) {
        let mut maps: Vec<OrderMap> = (0..w.books).map(|_| OrderMap::new()).collect();
        let mut reads = 0;
        for &op in &w.map_ops {
            match op {
                MapOp::Insert {
                    book,
                    id,
                    side,
                    price,
                    qty,
                } => {
                    let order = RestingOrder {
                        side,
                        price,
                        qty,
                        slot: 0,
                    };
                    maps[usize::from(book)].insert(id, order);
                }
                MapOp::Get { book, id } => {
                    reads = fold_order(reads, maps[usize::from(book)].get(&id));
                }
                MapOp::Update { book, id, qty } => {
                    maps[usize::from(book)]
                        .get_mut(&id)
                        .expect("updated order is in the map")
                        .qty = qty;
                }
                MapOp::Remove { book, id } => {
                    let removed = maps[usize::from(book)].remove(&id);
                    reads = fold_order(reads, removed.as_ref());
                }
            }
        }
        (maps, reads)
    }

    /// Orders in the maps and the sum of their quantities.
    pub fn count_maps(maps: &[OrderMap]) -> (usize, u64) {
        let orders = maps.iter().map(|m| m.len()).sum();
        let shares = maps
            .iter()
            .flat_map(|m| m.values())
            .map(|o| u64::from(o.qty))
            .sum();
        (orders, shares)
    }
}

/// Byte encoding of a workload, one section per part, used for the on-disk cache and the
/// workload checksums. Integers are little-endian.
pub mod codec {
    use matching_engine::{Command, Side};

    use super::{LevelOp, MapOp, Reads, SideOp, Workload};

    #[derive(Default)]
    struct W(Vec<u8>);

    impl W {
        fn u8(&mut self, v: u8) {
            self.0.push(v);
        }
        fn u16(&mut self, v: u16) {
            self.0.extend_from_slice(&v.to_le_bytes());
        }
        fn u32(&mut self, v: u32) {
            self.0.extend_from_slice(&v.to_le_bytes());
        }
        fn u64(&mut self, v: u64) {
            self.0.extend_from_slice(&v.to_le_bytes());
        }
        fn side(&mut self, s: Side) {
            self.u8(u8::from(s == Side::Sell));
        }
    }

    struct R<'a> {
        b: &'a [u8],
        at: usize,
    }

    impl R<'_> {
        fn take<const N: usize>(&mut self) -> Result<[u8; N], String> {
            let bytes = self
                .b
                .get(self.at..self.at + N)
                .ok_or("workload section ends early")?;
            self.at += N;
            Ok(bytes.try_into().expect("N bytes"))
        }
        fn u8(&mut self) -> Result<u8, String> {
            Ok(self.take::<1>()?[0])
        }
        fn u16(&mut self) -> Result<u16, String> {
            Ok(u16::from_le_bytes(self.take()?))
        }
        fn u32(&mut self) -> Result<u32, String> {
            Ok(u32::from_le_bytes(self.take()?))
        }
        fn u64(&mut self) -> Result<u64, String> {
            Ok(u64::from_le_bytes(self.take()?))
        }
        fn len(&mut self) -> Result<usize, String> {
            usize::try_from(self.u64()?).map_err(|e| e.to_string())
        }
        fn side(&mut self) -> Result<Side, String> {
            match self.u8()? {
                0 => Ok(Side::Buy),
                1 => Ok(Side::Sell),
                v => Err(format!("bad side byte {v}")),
            }
        }
        fn done(&self) -> Result<(), String> {
            if self.at == self.b.len() {
                Ok(())
            } else {
                Err("trailing bytes in workload section".into())
            }
        }
    }

    /// The sections in a fixed order: header, book commands, side, level, map. The header is
    /// books, level slots, final orders, final shares, final levels, then the read folds of
    /// side, level and map, each a `u64`.
    pub fn encode(w: &Workload) -> [Vec<u8>; 5] {
        let mut h = W::default();
        for v in [w.books, w.level_slots, w.final_orders] {
            h.u64(v as u64);
        }
        h.u64(w.final_shares);
        h.u64(w.final_levels as u64);
        let r = w.final_reads;
        for v in [r.side, r.level, r.map] {
            h.u64(v);
        }

        let mut c = W::default();
        c.u64(w.commands.len() as u64);
        for &(b, cmd) in &w.commands {
            c.u16(b);
            match cmd {
                Command::Limit {
                    id,
                    side,
                    price,
                    qty,
                } => {
                    c.u8(0);
                    c.u64(id);
                    c.side(side);
                    c.u32(price);
                    c.u32(qty);
                }
                Command::Market { id, side, qty } => {
                    c.u8(1);
                    c.u64(id);
                    c.side(side);
                    c.u32(qty);
                }
                Command::Cancel { id } => {
                    c.u8(2);
                    c.u64(id);
                }
                Command::Reduce { id, by } => {
                    c.u8(3);
                    c.u64(id);
                    c.u32(by);
                }
                Command::Replace {
                    id,
                    new_id,
                    price,
                    qty,
                } => {
                    c.u8(4);
                    c.u64(id);
                    c.u64(new_id);
                    c.u32(price);
                    c.u32(qty);
                }
            }
        }

        let mut s = W::default();
        s.u64(w.side_ops.len() as u64);
        for &op in &w.side_ops {
            let (tag, side, price) = match op {
                SideOp::Best { side } => (0, side, 0),
                SideOp::Find { side, price } => (1, side, price),
                SideOp::FindOrCreate { side, price } => (2, side, price),
                SideOp::Remove { side, price } => (3, side, price),
            };
            s.u8(tag);
            s.u32(side);
            s.u32(price);
        }

        let mut l = W::default();
        l.u64(w.level_ops.len() as u64);
        for &op in &w.level_ops {
            let (tag, slot, id, qty) = match op {
                LevelOp::Push { slot, id, qty } => (0, slot, id, qty),
                LevelOp::Front { slot } => (1, slot, 0, 0),
                LevelOp::Reduce { slot, qty } => (2, slot, 0, qty),
                LevelOp::PopFront { slot } => (3, slot, 0, 0),
                LevelOp::Remove { slot, id, qty } => (4, slot, id, qty),
                LevelOp::Reset { slot } => (5, slot, 0, 0),
            };
            l.u8(tag);
            l.u32(slot);
            l.u64(id);
            l.u32(qty);
        }

        let mut m = W::default();
        m.u64(w.map_ops.len() as u64);
        for &op in &w.map_ops {
            match op {
                MapOp::Insert {
                    book,
                    id,
                    side,
                    price,
                    qty,
                } => {
                    m.u8(0);
                    m.u16(book);
                    m.u64(id);
                    m.side(side);
                    m.u32(price);
                    m.u32(qty);
                }
                MapOp::Get { book, id } => {
                    m.u8(1);
                    m.u16(book);
                    m.u64(id);
                }
                MapOp::Update { book, id, qty } => {
                    m.u8(2);
                    m.u16(book);
                    m.u64(id);
                    m.u32(qty);
                }
                MapOp::Remove { book, id } => {
                    m.u8(3);
                    m.u16(book);
                    m.u64(id);
                }
            }
        }
        [h.0, c.0, s.0, l.0, m.0]
    }

    pub fn decode(sections: &[Vec<u8>; 5]) -> Result<Workload, String> {
        let mut h = R {
            b: &sections[0],
            at: 0,
        };
        let mut w = Workload {
            books: h.len()?,
            level_slots: h.len()?,
            final_orders: h.len()?,
            final_shares: h.u64()?,
            final_levels: h.len()?,
            final_reads: Reads {
                side: h.u64()?,
                level: h.u64()?,
                map: h.u64()?,
            },
            ..Workload::default()
        };
        h.done()?;

        let mut c = R {
            b: &sections[1],
            at: 0,
        };
        let n = c.len()?;
        w.commands.reserve_exact(n);
        for _ in 0..n {
            let b = c.u16()?;
            let cmd = match c.u8()? {
                0 => Command::Limit {
                    id: c.u64()?,
                    side: c.side()?,
                    price: c.u32()?,
                    qty: c.u32()?,
                },
                1 => Command::Market {
                    id: c.u64()?,
                    side: c.side()?,
                    qty: c.u32()?,
                },
                2 => Command::Cancel { id: c.u64()? },
                3 => Command::Reduce {
                    id: c.u64()?,
                    by: c.u32()?,
                },
                4 => Command::Replace {
                    id: c.u64()?,
                    new_id: c.u64()?,
                    price: c.u32()?,
                    qty: c.u32()?,
                },
                t => return Err(format!("bad command tag {t}")),
            };
            w.commands.push((b, cmd));
        }
        c.done()?;

        let mut s = R {
            b: &sections[2],
            at: 0,
        };
        let n = s.len()?;
        w.side_ops.reserve_exact(n);
        for _ in 0..n {
            let (tag, side, price) = (s.u8()?, s.u32()?, s.u32()?);
            w.side_ops.push(match tag {
                0 => SideOp::Best { side },
                1 => SideOp::Find { side, price },
                2 => SideOp::FindOrCreate { side, price },
                3 => SideOp::Remove { side, price },
                t => return Err(format!("bad side op tag {t}")),
            });
        }
        s.done()?;

        let mut l = R {
            b: &sections[3],
            at: 0,
        };
        let n = l.len()?;
        w.level_ops.reserve_exact(n);
        for _ in 0..n {
            let (tag, slot, id, qty) = (l.u8()?, l.u32()?, l.u64()?, l.u32()?);
            w.level_ops.push(match tag {
                0 => LevelOp::Push { slot, id, qty },
                1 => LevelOp::Front { slot },
                2 => LevelOp::Reduce { slot, qty },
                3 => LevelOp::PopFront { slot },
                4 => LevelOp::Remove { slot, id, qty },
                5 => LevelOp::Reset { slot },
                t => return Err(format!("bad level op tag {t}")),
            });
        }
        l.done()?;

        let mut m = R {
            b: &sections[4],
            at: 0,
        };
        let n = m.len()?;
        w.map_ops.reserve_exact(n);
        for _ in 0..n {
            let tag = m.u8()?;
            let book = m.u16()?;
            let id = m.u64()?;
            w.map_ops.push(match tag {
                0 => MapOp::Insert {
                    book,
                    id,
                    side: m.side()?,
                    price: m.u32()?,
                    qty: m.u32()?,
                },
                1 => MapOp::Get { book, id },
                2 => MapOp::Update {
                    book,
                    id,
                    qty: m.u32()?,
                },
                3 => MapOp::Remove { book, id },
                t => return Err(format!("bad map op tag {t}")),
            });
        }
        m.done()?;
        Ok(w)
    }
}
