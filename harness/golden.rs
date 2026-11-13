//! The golden replay hash: SHA-256 over an explicit byte encoding of every event the replay
//! emits, followed by the final state of every book.
//!
//! Encoding, version 1. All integers are little-endian and fixed width.
//!
//! 1. The ASCII domain string `matching-engine/replay-hash/v1\n` (31 bytes).
//! 2. For every event, in the order the books emitted them (message order, then the event order
//!    documented on `Event`): the stock locate (`u16`), a tag byte, then the fields:
//!    - `0x01` Accepted: id `u64`
//!    - `0x02` Trade: taker id `u64`, maker id `u64`, price `u32`, qty `u32`
//!    - `0x03` Filled: id `u64`
//!    - `0x04` Cancelled: id `u64`, remaining `u32`
//!    - `0x05` Reduced: id `u64`, by `u32`, remaining `u32`
//!    - `0x06` Replaced: old id `u64`, new id `u64`
//!    - `0x07` Rejected: id `u64`, reason `u8` (1 zero quantity, 2 duplicate order id,
//!      3 unknown order, 4 reduce too large)
//!
//!    After each `S` (system event) message, and so between its events and the next message's,
//!    a state checkpoint: the byte `0xF1`, the system event code byte, then the state of every
//!    book as in 3.
//! 3. The final state: the byte `0xF0`, then the number of books that hold at least one order
//!    (`u32`), then each of those books by ascending locate: the locate (`u16`), then the bid side
//!    and then the ask side, each as a level count (`u32`) followed by every level best price
//!    first: price (`u32`), order count (`u32`), then each order in time priority as id (`u64`)
//!    and remaining qty (`u32`).
//! 4. The byte `0xFF`, the number of messages replayed (`u64`) and the number of events (`u64`).
//!
//! Every record has a fixed length given by its tag or a count before it, so the stream parses
//! one way only. The state checkpoints (the day's file has system events at the start of
//! messages, start of system hours, start of market hours at 09:30, end of market hours at 16:00,
//! end of system hours and end of messages) and the final state catch a divergence that left a
//! book different without emitting a different event yet, such as two orders swapped in a queue;
//! the message and event counts catch a stream cut short.
//!
//! Messages that change no book contribute nothing except to the message count.

#![allow(dead_code)]

use matching_engine::itch::Locate;
use matching_engine::replay::Replay;
use matching_engine::{Event, OrderBook, RejectReason};
use sha2::{Digest, Sha256};

pub const DOMAIN: &[u8] = b"matching-engine/replay-hash/v1\n";

const FLUSH_AT: usize = 1 << 16;

pub struct ReplayHasher {
    sha: Sha256,
    buf: Vec<u8>,
    events: u64,
}

impl Default for ReplayHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplayHasher {
    pub fn new() -> Self {
        let mut buf = Vec::with_capacity(FLUSH_AT + 1024);
        buf.extend_from_slice(DOMAIN);
        Self {
            sha: Sha256::new(),
            buf,
            events: 0,
        }
    }

    pub fn events(&mut self, locate: Locate, events: &[Event]) {
        for event in events {
            encode_event(&mut self.buf, locate, event);
        }
        self.events += events.len() as u64;
        if self.buf.len() >= FLUSH_AT {
            self.flush();
        }
    }

    /// Appends a state checkpoint after the system event `code`.
    pub fn checkpoint(&mut self, code: u8, replay: &Replay) {
        self.buf.push(0xF1);
        self.buf.push(code);
        self.state(replay);
    }

    /// Appends the final state of every book and the counts, and returns the digest.
    pub fn finish(mut self, replay: &Replay) -> [u8; 32] {
        self.buf.push(0xF0);
        self.state(replay);
        self.buf.push(0xFF);
        self.buf
            .extend_from_slice(&replay.stats().messages.to_le_bytes());
        self.buf.extend_from_slice(&self.events.to_le_bytes());
        self.flush();
        self.sha.finalize().into()
    }

    fn state(&mut self, replay: &Replay) {
        let nonempty = |book: &OrderBook| book.best_bid().is_some() || book.best_ask().is_some();
        let books = replay.books().filter(|(_, b)| nonempty(b)).count();
        self.buf.extend_from_slice(&count(books).to_le_bytes());
        for (locate, book) in replay.books() {
            if nonempty(book) {
                encode_book(&mut self.buf, locate, book);
                if self.buf.len() >= FLUSH_AT {
                    self.flush();
                }
            }
        }
    }

    fn flush(&mut self) {
        self.sha.update(&self.buf);
        self.buf.clear();
    }
}

pub fn encode_event(out: &mut Vec<u8>, locate: Locate, event: &Event) {
    out.extend_from_slice(&locate.to_le_bytes());
    match *event {
        Event::Accepted { id } => {
            out.push(0x01);
            out.extend_from_slice(&id.to_le_bytes());
        }
        Event::Trade {
            taker_id,
            maker_id,
            price,
            qty,
        } => {
            out.push(0x02);
            out.extend_from_slice(&taker_id.to_le_bytes());
            out.extend_from_slice(&maker_id.to_le_bytes());
            out.extend_from_slice(&price.to_le_bytes());
            out.extend_from_slice(&qty.to_le_bytes());
        }
        Event::Filled { id } => {
            out.push(0x03);
            out.extend_from_slice(&id.to_le_bytes());
        }
        Event::Cancelled { id, remaining } => {
            out.push(0x04);
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&remaining.to_le_bytes());
        }
        Event::Reduced { id, by, remaining } => {
            out.push(0x05);
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&by.to_le_bytes());
            out.extend_from_slice(&remaining.to_le_bytes());
        }
        Event::Replaced { old_id, new_id } => {
            out.push(0x06);
            out.extend_from_slice(&old_id.to_le_bytes());
            out.extend_from_slice(&new_id.to_le_bytes());
        }
        Event::Rejected { id, reason } => {
            out.push(0x07);
            out.extend_from_slice(&id.to_le_bytes());
            out.push(match reason {
                RejectReason::ZeroQuantity => 1,
                RejectReason::DuplicateOrderId => 2,
                RejectReason::UnknownOrder => 3,
                RejectReason::ReduceTooLarge => 4,
            });
        }
    }
}

pub fn encode_book(out: &mut Vec<u8>, locate: Locate, book: &OrderBook) {
    out.extend_from_slice(&locate.to_le_bytes());
    let snapshot = book.snapshot();
    for side in [&snapshot.bids, &snapshot.asks] {
        out.extend_from_slice(&count(side.len()).to_le_bytes());
        for level in side {
            out.extend_from_slice(&level.price.to_le_bytes());
            out.extend_from_slice(&count(level.orders.len()).to_le_bytes());
            for &(id, qty) in &level.orders {
                out.extend_from_slice(&id.to_le_bytes());
                out.extend_from_slice(&qty.to_le_bytes());
            }
        }
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).expect("fewer than 2^32 levels or orders")
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
