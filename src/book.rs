//! Matching: applies commands to the two sides and the order map and emits events.

use std::collections::HashSet;

use crate::level::Nodes;
use crate::orders::{OrderMap, RestingOrder};
use crate::side::BookSide;
use crate::types::{
    Command, Event, LevelDepth, LevelSnapshot, OrderId, OrderInfo, Price, Qty, RejectReason, Side,
    Snapshot,
};

#[derive(Debug)]
pub struct OrderBook {
    bids: BookSide,
    asks: BookSide,
    orders: OrderMap,
    /// The queue nodes of every level on both sides.
    nodes: Nodes,
}

impl Default for OrderBook {
    fn default() -> Self {
        Self::new()
    }
}

impl OrderBook {
    pub fn new() -> Self {
        Self {
            bids: BookSide::new(Side::Buy),
            asks: BookSide::new(Side::Sell),
            orders: OrderMap::new(),
            nodes: Nodes::default(),
        }
    }

    /// Applies one command and appends its events to `out`. Never panics on bad input:
    /// invalid commands produce a `Rejected` event and leave the book unchanged.
    pub fn apply(&mut self, cmd: Command, out: &mut Vec<Event>) {
        match cmd {
            Command::Limit {
                id,
                side,
                price,
                qty,
            } => {
                if let Some(reason) = self.new_order_error(id, qty) {
                    out.push(Event::Rejected { id, reason });
                    return;
                }
                out.push(Event::Accepted { id });
                self.submit(id, side, Some(price), qty, out);
            }
            Command::Market { id, side, qty } => {
                if let Some(reason) = self.new_order_error(id, qty) {
                    out.push(Event::Rejected { id, reason });
                    return;
                }
                out.push(Event::Accepted { id });
                let remaining = self.submit(id, side, None, qty, out);
                if remaining > 0 {
                    out.push(Event::Cancelled { id, remaining });
                }
            }
            Command::Cancel { id } => match self.orders.remove(&id) {
                None => out.push(Event::Rejected {
                    id,
                    reason: RejectReason::UnknownOrder,
                }),
                Some(order) => {
                    self.nodes.unlink(order.slot);
                    self.side_mut(order.side)
                        .remove_order(order.price, id, order.qty);
                    out.push(Event::Cancelled {
                        id,
                        remaining: order.qty,
                    });
                }
            },
            Command::Reduce { id, by } => {
                let Some(order) = self.orders.get(&id).copied() else {
                    out.push(Event::Rejected {
                        id,
                        reason: RejectReason::UnknownOrder,
                    });
                    return;
                };
                let reason = if by == 0 {
                    Some(RejectReason::ZeroQuantity)
                } else if by > order.qty {
                    Some(RejectReason::ReduceTooLarge)
                } else {
                    None
                };
                if let Some(reason) = reason {
                    out.push(Event::Rejected { id, reason });
                    return;
                }
                let remaining = order.qty - by;
                if remaining == 0 {
                    self.orders.remove(&id);
                    self.nodes.unlink(order.slot);
                    self.side_mut(order.side).remove_order(order.price, id, by);
                } else {
                    self.orders.get_mut(&id).expect("checked above").qty = remaining;
                    self.side_mut(order.side)
                        .level_mut(order.price)
                        .expect("order's level exists")
                        .reduce(by);
                }
                out.push(Event::Reduced { id, by, remaining });
            }
            Command::Replace {
                id,
                new_id,
                price,
                qty,
            } => {
                let Some(old) = self.orders.get(&id).copied() else {
                    out.push(Event::Rejected {
                        id,
                        reason: RejectReason::UnknownOrder,
                    });
                    return;
                };
                let reason = if qty == 0 {
                    Some(RejectReason::ZeroQuantity)
                } else if new_id != id && self.orders.contains_key(&new_id) {
                    Some(RejectReason::DuplicateOrderId)
                } else {
                    None
                };
                if let Some(reason) = reason {
                    out.push(Event::Rejected { id, reason });
                    return;
                }
                self.orders.remove(&id);
                self.nodes.unlink(old.slot);
                self.side_mut(old.side).remove_order(old.price, id, old.qty);
                out.push(Event::Replaced { old_id: id, new_id });
                self.submit(new_id, old.side, Some(price), qty, out);
            }
        }
    }

    pub fn best_bid(&self) -> Option<Price> {
        self.bids.best_price()
    }

    pub fn best_ask(&self) -> Option<Price> {
        self.asks.best_price()
    }

    /// The side, price and remaining quantity of a resting order, or `None` if `id` is not resting.
    pub fn order(&self, id: OrderId) -> Option<OrderInfo> {
        self.orders.get(&id).map(|o| OrderInfo {
            side: o.side,
            price: o.price,
            qty: o.qty,
        })
    }

    /// The order with time priority at the best price of `side`: the one an incoming order on
    /// the opposite side would trade with first. `None` if the side is empty.
    pub fn front_order(&self, side: Side) -> Option<OrderId> {
        let side = self.side(side);
        let price = side.best_price()?;
        side.level(price)?.front(&self.nodes)
    }

    /// Levels of one side, best price first.
    pub fn depth(&self, side: Side) -> Vec<LevelDepth> {
        self.side(side)
            .levels_best_first()
            .map(|(price, level)| LevelDepth {
                price,
                total_qty: level.total(),
                order_count: level.len(),
            })
            .collect()
    }

    /// Every resting order, by side, level (best first) and time priority.
    pub fn snapshot(&self) -> Snapshot {
        let side = |side: Side| {
            self.side(side)
                .levels_best_first()
                .map(|(price, level)| LevelSnapshot {
                    price,
                    orders: level
                        .ids(&self.nodes)
                        .map(|id| (id, self.orders[&id].qty))
                        .collect(),
                })
                .collect()
        };
        Snapshot {
            bids: side(Side::Buy),
            asks: side(Side::Sell),
        }
    }

    /// Checks the book's internal consistency: it is not crossed, no level is empty, each
    /// level's total equals the sum of its orders, the levels and the id map hold exactly the
    /// same orders at the same side, price and queue node, each with quantity left, each level's
    /// queue links are consistent, and the queue slab has no node in use that no order or level
    /// owns.
    pub fn check_invariants(&self) -> Result<(), String> {
        if let (Some(bid), Some(ask)) = (self.best_bid(), self.best_ask())
            && bid >= ask
        {
            return Err(format!("book is crossed: best bid {bid} >= best ask {ask}"));
        }
        let mut seen = HashSet::new();
        let mut levels = 0;
        for side in [Side::Buy, Side::Sell] {
            for (price, level) in self.side(side).levels_best_first() {
                if level.is_empty() {
                    return Err(format!("{side:?} level {price} is empty"));
                }
                level.check(&self.nodes)?;
                levels += 1;
                let mut sum = 0u64;
                for (slot, id) in level.slots(&self.nodes) {
                    if !seen.insert(id) {
                        return Err(format!("order {id} is queued twice"));
                    }
                    let Some(order) = self.orders.get(&id) else {
                        return Err(format!("order {id} is queued but not in the id map"));
                    };
                    if order.side != side || order.price != price {
                        return Err(format!(
                            "order {id} is queued at {side:?} {price} but mapped to {:?} {}",
                            order.side, order.price
                        ));
                    }
                    if order.slot != slot {
                        return Err(format!(
                            "order {id} is queued in node {slot} but mapped to node {}",
                            order.slot
                        ));
                    }
                    if order.qty == 0 {
                        return Err(format!("order {id} rests with zero quantity"));
                    }
                    sum += u64::from(order.qty);
                }
                if sum != level.total() {
                    return Err(format!(
                        "{side:?} level {price} total {} != sum of orders {sum}",
                        level.total()
                    ));
                }
            }
        }
        if seen.len() != self.orders.len() {
            return Err(format!(
                "{} orders queued but {} in the id map",
                seen.len(),
                self.orders.len()
            ));
        }
        let in_use = self.nodes.in_use()?;
        if in_use != seen.len() + levels {
            return Err(format!(
                "{in_use} queue nodes in use for {} orders on {levels} levels",
                seen.len()
            ));
        }
        Ok(())
    }

    fn side(&self, side: Side) -> &BookSide {
        match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        }
    }

    fn side_mut(&mut self, side: Side) -> &mut BookSide {
        match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        }
    }

    fn new_order_error(&self, id: OrderId, qty: Qty) -> Option<RejectReason> {
        if qty == 0 {
            Some(RejectReason::ZeroQuantity)
        } else if self.orders.contains_key(&id) {
            Some(RejectReason::DuplicateOrderId)
        } else {
            None
        }
    }

    /// Matches an accepted order and rests what is left if it has a limit price.
    /// Emits `Filled` for the taker when nothing is left. Returns the unfilled quantity.
    fn submit(
        &mut self,
        id: OrderId,
        side: Side,
        limit: Option<Price>,
        qty: Qty,
        out: &mut Vec<Event>,
    ) -> Qty {
        let remaining = self.match_order(id, side, limit, qty, out);
        if remaining == 0 {
            out.push(Event::Filled { id });
        } else if let Some(price) = limit {
            let book_side = match side {
                Side::Buy => &mut self.bids,
                Side::Sell => &mut self.asks,
            };
            let slot = book_side
                .level_or_insert(price)
                .push_back(&mut self.nodes, id, remaining);
            self.orders.insert(
                id,
                RestingOrder {
                    side,
                    price,
                    qty: remaining,
                    slot,
                },
            );
        }
        remaining
    }

    /// Trades `qty` of the taker against the opposite side, best price first and oldest order
    /// first within a price, while the limit (if any) crosses. Returns the unfilled quantity.
    fn match_order(
        &mut self,
        taker_id: OrderId,
        side: Side,
        limit: Option<Price>,
        mut qty: Qty,
        out: &mut Vec<Event>,
    ) -> Qty {
        let (opposite, orders, nodes) = match side {
            Side::Buy => (&mut self.asks, &mut self.orders, &mut self.nodes),
            Side::Sell => (&mut self.bids, &mut self.orders, &mut self.nodes),
        };
        while qty > 0 {
            let Some(price) = opposite.best_price() else {
                break;
            };
            if let Some(limit) = limit
                && !crosses(side, limit, price)
            {
                break;
            }
            let level = opposite.level_mut(price).expect("best level exists");
            while qty > 0
                && let Some(maker_id) = level.front(nodes)
            {
                let maker = orders
                    .get_mut(&maker_id)
                    .expect("queued order is in the id map");
                let fill = qty.min(maker.qty);
                qty -= fill;
                maker.qty -= fill;
                level.reduce(fill);
                out.push(Event::Trade {
                    taker_id,
                    maker_id,
                    price,
                    qty: fill,
                });
                if maker.qty == 0 {
                    level.pop_front(nodes);
                    orders.remove(&maker_id);
                    out.push(Event::Filled { id: maker_id });
                }
            }
            if level.is_empty() {
                opposite.remove_level(price);
            }
        }
        qty
    }
}

/// Whether a taker on `side` with limit `limit` can trade against a resting price `resting`.
fn crosses(side: Side, limit: Price, resting: Price) -> bool {
    match side {
        Side::Buy => resting <= limit,
        Side::Sell => resting >= limit,
    }
}
