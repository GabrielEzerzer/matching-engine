//! A single-instrument, price-time priority limit order book.
//!
//! The book is split into parts that are meant to be measured and replaced one at a time:
//! - [`side`]: price-level lookup for one side of the book,
//! - [`level`]: the FIFO queue of orders resting at one price,
//! - [`orders`]: the map from order id to the resting order,
//! - [`book`]: the matching logic that ties them together.
//!
//! All prices and quantities are integers. Commands go in through [`OrderBook::apply`] and the
//! resulting events are appended to a caller-supplied `Vec<Event>`, in a fully deterministic order.
//!
//! [`itch`] parses Nasdaq TotalView-ITCH 5.0 and [`replay`] drives one book per instrument from
//! an ITCH stream.

#![forbid(unsafe_code)]

mod book;
pub mod itch;
mod level;
mod orders;
pub mod replay;
mod side;
mod types;

/// The book's parts, exposed only with the `internals` feature so that `examples/micro.rs` can
/// time each one on its own. Not part of the stable API: these types change between rounds.
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
