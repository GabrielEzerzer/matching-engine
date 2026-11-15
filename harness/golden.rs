//! The golden replay hash: SHA-256 over every event and periodic full-state checkpoints of every book.
//! The byte encoding is specified in README.md under "Checking the golden hash".
//! `with_period` lets tests reach the periodic checkpoints in a few messages; the golden value always uses `PERIOD`.

use matching_engine::itch::{Locate, Message};
use matching_engine::replay::Replay;
use matching_engine::{Event, OrderBook, RejectReason};
use sha2::{Digest, Sha256};

pub const DOMAIN: &[u8] = b"matching-engine/replay-hash/v2\n";

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
