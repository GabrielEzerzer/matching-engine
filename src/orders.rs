//! The map from order id to resting order. Only orders resting in the book are in it.

use std::collections::HashMap;

use crate::types::{OrderId, Price, Qty, Side};

#[derive(Clone, Copy, Debug)]
pub(crate) struct RestingOrder {
    pub(crate) side: Side,
    pub(crate) price: Price,
    pub(crate) qty: Qty,
}

/// Iteration order of this map never reaches the output: every query walks the levels instead.
pub(crate) type OrderMap = HashMap<OrderId, RestingOrder>;
