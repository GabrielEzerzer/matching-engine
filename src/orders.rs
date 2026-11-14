//! The map from order id to resting order. Only orders resting in the book are in it.

use std::collections::HashMap;

use crate::types::{OrderId, Price, Qty, Side};

#[derive(Clone, Copy, Debug)]
pub struct RestingOrder {
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
    /// The sequence number its level gave this resting of the order; see `level`.
    pub seq: u32,
}

/// Iteration order of this map never reaches the output: every query walks the levels instead.
pub type OrderMap = HashMap<OrderId, RestingOrder>;
