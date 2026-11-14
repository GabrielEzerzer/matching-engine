//! The golden replay hash: SHA-256 over an explicit byte encoding of every event the replay
//! emits, interleaved with full-state checkpoints of every book, followed by the final state.
//!
//! Encoding, version 2. All integers are little-endian and fixed width. Every record starts with
//! a tag byte, and the tags of the different records are distinct.
//!
//! 1. The ASCII domain string `matching-engine/replay-hash/v2\n` (31 bytes).
//! 2. For every event, in the order the books emitted them (message order, then the event order
//!    documented on `Event`): an event record, which is a tag byte, the stock locate (`u16`),
//!    then the fields:
//!    - `0x01` Accepted: id `u64`
//!    - `0x02` Trade: taker id `u64`, maker id `u64`, price `u32`, qty `u32`
//!    - `0x03` Filled: id `u64`
//!    - `0x04` Cancelled: id `u64`, remaining `u32`
//!    - `0x05` Reduced: id `u64`, by `u32`, remaining `u32`
//!    - `0x06` Replaced: old id `u64`, new id `u64`
//!    - `0x07` Rejected: id `u64`, reason `u8` (1 zero quantity, 2 duplicate order id,
//!      3 unknown order, 4 reduce too large)
//!
//!    After a message's events, and before the next message's, up to two checkpoint records:
//!    - after each `S` (system event) message, the byte `0xF1`, the system event code byte, then
//!      the state of every book as in 3;
//!    - after every message whose 1-based position in the stream is a multiple of the checkpoint
//!      period ([`PERIOD`], 2^20 messages), the byte `0xF2`, that position (`u64`), then the
//!      state of every book as in 3. When both apply to one message, `0xF1` comes first.
//! 3. The final state: the byte `0xF0`, then the state of every book: the number of books that
//!    hold at least one order (`u32`), then each of those books by ascending locate: the locate
//!    (`u16`), then the bid side and then the ask side, each as a level count (`u32`) followed by
//!    every level best price first: price (`u32`), order count (`u32`), then each order in time
//!    priority as id (`u64`) and remaining qty (`u32`).
//! 4. The byte `0xFF`, the number of messages replayed (`u64`) and the number of events (`u64`).
//!
//! Every record starts with its own tag and has a fixed length given by that tag or by a count
//! inside it, so the stream parses one way only.
//!
//! `Accepted`, `Cancelled` and `Reduced` carry no side or price, and most orders are deleted
//! without trading, so the events alone do not show where an order rested. The checkpoints do:
//! a book change that files an order at the wrong price or in the wrong place in a queue changes
//! the next checkpoint that sees the order resting, even if every event is the same. The periodic
//! checkpoints (256 over the sample day) see every order that rests across a multiple of 2^20
//! messages; the `S` checkpoints add the start of market hours at 09:30 and the end at 16:00.
//! The message and event counts catch a stream cut short.
//!
//! Messages that change no book contribute nothing except to the message count and the
//! checkpoints that fall on them.

#![allow(dead_code)]

use matching_engine::itch::{Locate, Message};
use matching_engine::replay::Replay;
use matching_engine::{Event, OrderBook, RejectReason};
use sha2::{Digest, Sha256};

pub const DOMAIN: &[u8] = b"matching-engine/replay-hash/v2\n";

/// Messages between periodic state checkpoints.
pub const PERIOD: u64 = 1 << 20;

const FLUSH_AT: usize = 1 << 16;

pub struct ReplayHasher {
    sha: Sha256,
    buf: Vec<u8>,
    events: u64,
    period: u64,
}

impl Default for ReplayHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplayHasher {
    pub fn new() -> Self {
        Self::with_period(PERIOD)
    }

    /// A hasher with periodic checkpoints every `period` messages instead of [`PERIOD`], so the
    /// tests can reach them with a few messages. The golden value always uses [`PERIOD`].
    pub fn with_period(period: u64) -> Self {
        assert!(period > 0, "checkpoint period must be positive");
        let mut buf = Vec::with_capacity(FLUSH_AT + 1024);
        buf.extend_from_slice(DOMAIN);
        Self {
            sha: Sha256::new(),
            buf,
            events: 0,
            period,
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

    /// Appends the checkpoints due after `msg`, which `replay` has just applied (after its
    /// events went to [`ReplayHasher::events`]).
    pub fn after_message(&mut self, msg: &Message, replay: &Replay) {
        if let Message::SystemEvent { event, .. } = *msg {
            self.buf.push(0xF1);
            self.buf.push(event);
            self.state(replay);
        }
        let position = replay.stats().messages;
        if position.is_multiple_of(self.period) {
            self.buf.push(0xF2);
            self.buf.extend_from_slice(&position.to_le_bytes());
            self.state(replay);
        }
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
    let tag = match event {
        Event::Accepted { .. } => 0x01,
        Event::Trade { .. } => 0x02,
        Event::Filled { .. } => 0x03,
        Event::Cancelled { .. } => 0x04,
        Event::Reduced { .. } => 0x05,
        Event::Replaced { .. } => 0x06,
        Event::Rejected { .. } => 0x07,
    };
    out.push(tag);
    out.extend_from_slice(&locate.to_le_bytes());
    match *event {
        Event::Accepted { id } => {
            out.extend_from_slice(&id.to_le_bytes());
        }
        Event::Trade {
            taker_id,
            maker_id,
            price,
            qty,
        } => {
            out.extend_from_slice(&taker_id.to_le_bytes());
            out.extend_from_slice(&maker_id.to_le_bytes());
            out.extend_from_slice(&price.to_le_bytes());
            out.extend_from_slice(&qty.to_le_bytes());
        }
        Event::Filled { id } => {
            out.extend_from_slice(&id.to_le_bytes());
        }
        Event::Cancelled { id, remaining } => {
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&remaining.to_le_bytes());
        }
        Event::Reduced { id, by, remaining } => {
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&by.to_le_bytes());
            out.extend_from_slice(&remaining.to_le_bytes());
        }
        Event::Replaced { old_id, new_id } => {
            out.extend_from_slice(&old_id.to_le_bytes());
            out.extend_from_slice(&new_id.to_le_bytes());
        }
        Event::Rejected { id, reason } => {
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
