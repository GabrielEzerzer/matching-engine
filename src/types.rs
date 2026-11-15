//! Public value types: commands in, events out, and the query results.

pub type OrderId = u64;

pub type Price = u32;

pub type Qty = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Limit {
        id: OrderId,
        side: Side,
        price: Price,
        qty: Qty,
    },
    Market {
        id: OrderId,
        side: Side,
        qty: Qty,
    },
    Cancel {
        id: OrderId,
    },
    Reduce {
        id: OrderId,
        by: Qty,
    },
    Replace {
        id: OrderId,
        new_id: OrderId,
        price: Price,
        qty: Qty,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Accepted {
        id: OrderId,
    },
    Trade {
        taker_id: OrderId,
        maker_id: OrderId,
        price: Price,
        qty: Qty,
    },
    Filled {
        id: OrderId,
    },
    Cancelled {
        id: OrderId,
        remaining: Qty,
    },
    Reduced {
        id: OrderId,
        by: Qty,
        remaining: Qty,
    },
    Replaced {
        old_id: OrderId,
        new_id: OrderId,
    },
    Rejected {
        id: OrderId,
        reason: RejectReason,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    ZeroQuantity,
    DuplicateOrderId,
    UnknownOrder,
    ReduceTooLarge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderInfo {
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelDepth {
    pub price: Price,
    pub total_qty: u64,
    pub order_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LevelSnapshot {
    pub price: Price,
    pub orders: Vec<(OrderId, Qty)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub bids: Vec<LevelSnapshot>,
    pub asks: Vec<LevelSnapshot>,
}
