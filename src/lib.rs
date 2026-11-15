//! A single-instrument, price-time priority limit order book, an ITCH 5.0 parser and a replay driver.

#![forbid(unsafe_code)]

mod book;
pub mod itch;
mod level;
mod orders;
pub mod replay;
mod side;
mod types;

#[cfg(feature = "internals")]
pub mod internals {
    pub use crate::level::{Level, Nodes};
    pub use crate::orders::{OrderMap, RestingOrder};
    pub use crate::side::BookSide;
}

pub use book::OrderBook;
pub use types::{
    Command, Event, LevelDepth, LevelSnapshot, OrderId, OrderInfo, Price, Qty, RejectReason, Side,
    Snapshot,
};
