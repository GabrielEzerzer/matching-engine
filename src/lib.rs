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

#![forbid(unsafe_code)]

mod book;
mod level;
mod orders;
mod side;
mod types;

pub use book::OrderBook;
pub use types::{
    Command, Event, LevelDepth, LevelSnapshot, OrderId, Price, Qty, RejectReason, Side, Snapshot,
};
