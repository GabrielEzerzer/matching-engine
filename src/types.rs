//! Public value types: commands in, events out, and the query results.

/// Order id. ITCH order reference numbers are 64-bit.
pub type OrderId = u64;

/// Price in ticks. ITCH prices are `u32` with four implied decimals, so one tick is $0.0001.
pub type Price = u32;

/// Quantity of one order or trade, in shares. ITCH share counts are `u32`.
/// Totals over many orders (a level's depth) are `u64` so they cannot overflow.
pub type Qty = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Buy,
    Sell,
}

/// A command sent to the book.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Match against the opposite side while the price crosses, then rest any remainder.
    Limit {
        id: OrderId,
        side: Side,
        price: Price,
        qty: Qty,
    },
    /// Match against the opposite side at any price; an unfilled remainder is cancelled.
    Market { id: OrderId, side: Side, qty: Qty },
    /// Remove a resting order.
    Cancel { id: OrderId },
    /// Reduce a resting order by `by` shares, keeping its time priority (ITCH "Order Cancel").
    /// Reducing by the full remaining quantity removes the order.
    Reduce { id: OrderId, by: Qty },
    /// Cancel a resting order and enter a new limit order on the same side with `new_id`
    /// (which may equal `id`), `price` and `qty` (ITCH "Order Replace"). The new order loses
    /// time priority and may trade if it crosses.
    Replace {
        id: OrderId,
        new_id: OrderId,
        price: Price,
        qty: Qty,
    },
}

/// An event emitted by the book.
///
/// Order of events per command:
/// - Limit: `Rejected`, or `Accepted` then (`Trade` then `Filled` for each maker it empties)*,
///   then `Filled` for the taker if nothing is left. A remainder rests silently.
/// - Market: `Rejected`, or `Accepted`, the same trades, then `Filled` or `Cancelled` for the
///   taker's remainder.
/// - Cancel: `Rejected` or `Cancelled`.
/// - Reduce: `Rejected` or `Reduced` (with `remaining == 0` when the order was removed).
/// - Replace: `Rejected`, or `Replaced` followed by the new order's events as for Limit,
///   without its `Accepted`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Event {
    Accepted {
        id: OrderId,
    },
    /// `price` is always the maker's resting price.
    Trade {
        taker_id: OrderId,
        maker_id: OrderId,
        price: Price,
        qty: Qty,
    },
    /// The order has no quantity left and is gone from the book.
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
    /// `id` is the id the command named first (for Replace, the old id).
    Rejected {
        id: OrderId,
        reason: RejectReason,
    },
}

/// Why a command was rejected. A rejected command leaves the book unchanged.
///
/// Checks run in this order: Limit and Market check `ZeroQuantity` then `DuplicateOrderId`;
/// Reduce checks `UnknownOrder`, `ZeroQuantity`, then `ReduceTooLarge`; Replace checks
/// `UnknownOrder`, `ZeroQuantity`, then `DuplicateOrderId` (a `new_id` equal to the old id is fine).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RejectReason {
    ZeroQuantity,
    /// The id belongs to an order that is resting in the book.
    DuplicateOrderId,
    /// No resting order has this id: it never existed, or it was filled or cancelled.
    UnknownOrder,
    /// A reduce asked for more shares than the order has left.
    ReduceTooLarge,
}

/// Aggregate view of one price level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelDepth {
    pub price: Price,
    pub total_qty: u64,
    pub order_count: usize,
}

/// One price level with its orders in time priority order, as `(id, remaining qty)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LevelSnapshot {
    pub price: Price,
    pub orders: Vec<(OrderId, Qty)>,
}

/// The whole book. Each side lists its levels best price first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub bids: Vec<LevelSnapshot>,
    pub asks: Vec<LevelSnapshot>,
}
