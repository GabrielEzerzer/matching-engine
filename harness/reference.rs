//! A slow reference book for the tests: every resting order in one flat Vec, each match found by a full scan.
//! It shares the crate's public command and event types but none of its internals.

use matching_engine::{
    Command, Event, LevelSnapshot, OrderId, Price, Qty, RejectReason, Side, Snapshot,
};

struct RefOrder {
    id: OrderId,
    side: Side,
    price: Price,
    qty: Qty,
    seq: u64,
}

#[derive(Default)]
pub struct ReferenceBook {
    orders: Vec<RefOrder>,
    next_seq: u64,
}

impl ReferenceBook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, cmd: Command, out: &mut Vec<Event>) {
        match cmd {
            Command::Limit {
                id,
                side,
                price,
                qty,
            } => {
                if qty == 0 {
                    out.push(rejected(id, RejectReason::ZeroQuantity));
                } else if self.find(id).is_some() {
                    out.push(rejected(id, RejectReason::DuplicateOrderId));
                } else {
                    out.push(Event::Accepted { id });
                    self.trade_then_rest(id, side, Some(price), qty, out);
                }
            }
            Command::Market { id, side, qty } => {
                if qty == 0 {
                    out.push(rejected(id, RejectReason::ZeroQuantity));
                } else if self.find(id).is_some() {
                    out.push(rejected(id, RejectReason::DuplicateOrderId));
                } else {
                    out.push(Event::Accepted { id });
                    let left = self.trade_then_rest(id, side, None, qty, out);
                    if left > 0 {
                        out.push(Event::Cancelled {
                            id,
                            remaining: left,
                        });
                    }
                }
            }
            Command::Cancel { id } => match self.find(id) {
                None => out.push(rejected(id, RejectReason::UnknownOrder)),
                Some(i) => {
                    let order = self.orders.remove(i);
                    out.push(Event::Cancelled {
                        id,
                        remaining: order.qty,
                    });
                }
            },
            Command::Reduce { id, by } => match self.find(id) {
                None => out.push(rejected(id, RejectReason::UnknownOrder)),
                Some(_) if by == 0 => out.push(rejected(id, RejectReason::ZeroQuantity)),
                Some(i) if by > self.orders[i].qty => {
                    out.push(rejected(id, RejectReason::ReduceTooLarge))
                }
                Some(i) => {
                    self.orders[i].qty -= by;
                    let remaining = self.orders[i].qty;
                    if remaining == 0 {
                        self.orders.remove(i);
                    }
                    out.push(Event::Reduced { id, by, remaining });
                }
            },
            Command::Replace {
                id,
                new_id,
                price,
                qty,
            } => match self.find(id) {
                None => out.push(rejected(id, RejectReason::UnknownOrder)),
                Some(_) if qty == 0 => out.push(rejected(id, RejectReason::ZeroQuantity)),
                Some(_) if new_id != id && self.find(new_id).is_some() => {
                    out.push(rejected(id, RejectReason::DuplicateOrderId))
                }
                Some(i) => {
                    let old = self.orders.remove(i);
                    out.push(Event::Replaced { old_id: id, new_id });
                    self.trade_then_rest(new_id, old.side, Some(price), qty, out);
                }
            },
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            bids: self.side_snapshot(Side::Buy),
            asks: self.side_snapshot(Side::Sell),
        }
    }

    fn find(&self, id: OrderId) -> Option<usize> {
        self.orders.iter().position(|o| o.id == id)
    }

    fn best_counterparty(&self, side: Side, limit: Option<Price>) -> Option<usize> {
        self.orders
            .iter()
            .enumerate()
            .filter(|(_, o)| o.side != side)
            .filter(|(_, o)| match (side, limit) {
                (_, None) => true,
                (Side::Buy, Some(limit)) => o.price <= limit,
                (Side::Sell, Some(limit)) => o.price >= limit,
            })
            .min_by_key(|(_, o)| {
                let price_rank = match side {
                    Side::Buy => i64::from(o.price),
                    Side::Sell => -i64::from(o.price),
                };
                (price_rank, o.seq)
            })
            .map(|(i, _)| i)
    }

    fn trade_then_rest(
        &mut self,
        id: OrderId,
        side: Side,
        limit: Option<Price>,
        mut qty: Qty,
        out: &mut Vec<Event>,
    ) -> Qty {
        while qty > 0 {
            let Some(i) = self.best_counterparty(side, limit) else {
                break;
            };
            let maker = &mut self.orders[i];
            let fill = qty.min(maker.qty);
            qty -= fill;
            maker.qty -= fill;
            out.push(Event::Trade {
                taker_id: id,
                maker_id: maker.id,
                price: maker.price,
                qty: fill,
            });
            if maker.qty == 0 {
                let maker_id = maker.id;
                self.orders.remove(i);
                out.push(Event::Filled { id: maker_id });
            }
        }
        if qty == 0 {
            out.push(Event::Filled { id });
        } else if let Some(price) = limit {
            self.orders.push(RefOrder {
                id,
                side,
                price,
                qty,
                seq: self.next_seq,
            });
            self.next_seq += 1;
        }
        qty
    }

    fn side_snapshot(&self, side: Side) -> Vec<LevelSnapshot> {
        let mut resting: Vec<&RefOrder> = self.orders.iter().filter(|o| o.side == side).collect();
        resting.sort_by_key(|o| {
            let price_rank = match side {
                Side::Buy => -i64::from(o.price),
                Side::Sell => i64::from(o.price),
            };
            (price_rank, o.seq)
        });
        let mut levels: Vec<LevelSnapshot> = Vec::new();
        for o in resting {
            match levels.last_mut() {
                Some(level) if level.price == o.price => level.orders.push((o.id, o.qty)),
                _ => levels.push(LevelSnapshot {
                    price: o.price,
                    orders: vec![(o.id, o.qty)],
                }),
            }
        }
        levels
    }
}

fn rejected(id: OrderId, reason: RejectReason) -> Event {
    Event::Rejected { id, reason }
}
