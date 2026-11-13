//! Replays an ITCH 5.0 stream through one [`OrderBook`] per stock locate.
//!
//! Each order message becomes at most one book command:
//!
//! | ITCH | Command |
//! |---|---|
//! | `A`, `F` add order | `Limit` with the ITCH reference number as id |
//! | `D` order delete | `Cancel` |
//! | `X` order cancel | `Reduce` by the cancelled shares |
//! | `U` order replace | `Replace` (same side, new reference, price and shares) |
//! | `E`, `C` order executed | an aggressor `Limit`, or `Reduce` as a fallback (below) |
//!
//! An execution names the resting order it executed against. If that order is at the front of
//! the best level of its side, the driver replays the execution as the incoming order it really
//! was: an opposite-side `Limit` at the resting order's price for the executed shares, with a
//! synthetic id at or above [`TAKER_ID_BASE`]. The book's matching then has to pick exactly the
//! named order, and the driver checks that it did (a mismatch is a [`ReplayError`]). If the named
//! order is not at the front of the best level (around the crosses and in other special cases),
//! or the execution is larger than what the book has left, the driver falls back to `Reduce`, so
//! the book stays as Nasdaq's is. The `C` execution price is ignored: the aggressor trades at the
//! resting order's price, and [`Stats::c_price_differs`] counts how often the two differ.
//!
//! Every other message type changes no book. `R` (stock directory) records the symbol of a
//! locate and `H` (stock trading action) its trading state.
//!
//! While a security is halted, paused or in a quotation-only period, Nasdaq accepts and displays
//! orders that lock or cross the book and matches them only in the reopening cross. This book
//! always matches, so such an add trades here although it rested at Nasdaq, and from then on that
//! book differs from Nasdaq's (later deletes of the orders it traded away are rejected). The
//! driver does not hide this: it counts every add or replace that traded as an unexpected trade,
//! split by whether the security was trading at the time, and counts rejections on books that
//! had already diverged this way separately from rejections on books that had not.

use crate::itch::{Locate, Message, Symbol};
use crate::{Command, Event, OrderBook, OrderId, RejectReason, Side};

/// Synthetic ids for replayed aggressors start here. ITCH reference numbers in a day stay far
/// below 2^63; the driver returns an error if one does not, so the two cannot collide.
pub const TAKER_ID_BASE: OrderId = 1 << 63;

/// The kind of command a message was turned into, for counting rejections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandKind {
    /// `A`/`F` as `Limit`.
    Add,
    /// `D` as `Cancel`.
    Delete,
    /// `X` as `Reduce`.
    PartialCancel,
    /// `U` as `Replace`.
    Replace,
    /// `E`/`C` replayed as an aggressor `Limit`.
    ExecutionAggressor,
    /// `E`/`C` replayed as `Reduce`.
    ExecutionFallback,
}

impl CommandKind {
    pub const ALL: [CommandKind; 6] = [
        CommandKind::Add,
        CommandKind::Delete,
        CommandKind::PartialCancel,
        CommandKind::Replace,
        CommandKind::ExecutionAggressor,
        CommandKind::ExecutionFallback,
    ];

    pub fn name(self) -> &'static str {
        match self {
            CommandKind::Add => "add",
            CommandKind::Delete => "delete",
            CommandKind::PartialCancel => "partial_cancel",
            CommandKind::Replace => "replace",
            CommandKind::ExecutionAggressor => "execution_aggressor",
            CommandKind::ExecutionFallback => "execution_fallback",
        }
    }
}

pub const REJECT_REASONS: [RejectReason; 4] = [
    RejectReason::ZeroQuantity,
    RejectReason::DuplicateOrderId,
    RejectReason::UnknownOrder,
    RejectReason::ReduceTooLarge,
];

pub fn reject_reason_name(reason: RejectReason) -> &'static str {
    match reason {
        RejectReason::ZeroQuantity => "zero_quantity",
        RejectReason::DuplicateOrderId => "duplicate_order_id",
        RejectReason::UnknownOrder => "unknown_order",
        RejectReason::ReduceTooLarge => "reduce_too_large",
    }
}

fn reason_index(reason: RejectReason) -> usize {
    match reason {
        RejectReason::ZeroQuantity => 0,
        RejectReason::DuplicateOrderId => 1,
        RejectReason::UnknownOrder => 2,
        RejectReason::ReduceTooLarge => 3,
    }
}

/// Counts kept while replaying.
#[derive(Clone, Debug)]
pub struct Stats {
    /// Messages applied, of every type.
    pub messages: u64,
    /// Messages by type byte.
    pub by_type: [u64; 256],
    /// Messages that became a book command.
    pub book_messages: u64,
    /// Events emitted by all books.
    pub events: u64,
    /// `Trade` events emitted by all books.
    pub trades: u64,
    /// Executions replayed as an aggressor that traded against exactly the named order.
    pub exec_aggressor: u64,
    /// Executions replayed as `Reduce` because the named order rests at a price worse than the
    /// best price of its side.
    pub exec_fallback_not_best_price: u64,
    /// Executions replayed as `Reduce` because the named order rests at the best price of its
    /// side but behind an older order.
    pub exec_fallback_behind_at_best: u64,
    /// Executions replayed as `Reduce` because the named order is not in the book.
    pub exec_fallback_unknown: u64,
    /// Executions replayed as `Reduce` because they are larger than the order's remaining shares.
    pub exec_fallback_too_large: u64,
    /// `C` messages whose execution price differs from the named order's resting price.
    pub c_price_differs: u64,
    /// Rejections by command kind (index in [`CommandKind::ALL`]) and reason (index in
    /// [`REJECT_REASONS`]).
    pub rejections: [[u64; 4]; 6],
    /// Add or replace commands that traded in our book. Nasdaq rests these only while the
    /// security is not trading (see the module docs).
    pub unexpected_trade_messages: u64,
    /// Those of `unexpected_trade_messages` that arrived while the security's last trading
    /// state was `T` (trading).
    pub unexpected_trade_messages_while_trading: u64,
    /// Those that arrived while the security was trading and its book had not diverged yet.
    /// A consistent feed and a correct book make this zero.
    pub unexpected_trade_messages_on_clean_trading_books: u64,
    /// `Trade` events the unexpected trade messages produced.
    pub unexpected_trades: u64,
    /// Rejections on a book that had an unexpected trade earlier, so no longer matches Nasdaq's.
    pub rejections_on_diverged_books: u64,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            messages: 0,
            by_type: [0; 256],
            book_messages: 0,
            events: 0,
            trades: 0,
            exec_aggressor: 0,
            exec_fallback_not_best_price: 0,
            exec_fallback_behind_at_best: 0,
            exec_fallback_unknown: 0,
            exec_fallback_too_large: 0,
            c_price_differs: 0,
            rejections: [[0; 4]; 6],
            unexpected_trade_messages: 0,
            unexpected_trade_messages_while_trading: 0,
            unexpected_trade_messages_on_clean_trading_books: 0,
            unexpected_trades: 0,
            rejections_on_diverged_books: 0,
        }
    }
}

impl Stats {
    pub fn exec_fallback(&self) -> u64 {
        self.exec_fallback_not_best_price
            + self.exec_fallback_behind_at_best
            + self.exec_fallback_unknown
            + self.exec_fallback_too_large
    }

    pub fn total_rejections(&self) -> u64 {
        self.rejections.iter().flatten().sum()
    }

    pub fn rejections_for(&self, kind: CommandKind, reason: RejectReason) -> u64 {
        let k = CommandKind::ALL
            .iter()
            .position(|&c| c == kind)
            .expect("listed");
        self.rejections[k][reason_index(reason)]
    }
}

/// A replay that cannot continue faithfully.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayError {
    /// An ITCH reference number reached the synthetic taker id range.
    ReferenceInTakerSpace { locate: Locate, reference: u64 },
    /// A replayed aggressor did not trade exactly against the named order.
    AggressorMismatch {
        locate: Locate,
        reference: OrderId,
        events: Vec<Event>,
    },
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayError::ReferenceInTakerSpace { locate, reference } => write!(
                f,
                "locate {locate}: reference {reference} is in the synthetic taker id range"
            ),
            ReplayError::AggressorMismatch {
                locate,
                reference,
                events,
            } => write!(
                f,
                "locate {locate}: aggressor for order {reference} did not trade exactly against it: {events:?}"
            ),
        }
    }
}

impl std::error::Error for ReplayError {}

/// What one message did.
#[derive(Debug)]
pub struct Applied<'a> {
    pub locate: Locate,
    /// The command sent to the book of `locate`, if the message was a book message.
    pub command: Option<Command>,
    /// The events that command emitted, in order. Empty for non-book messages.
    pub events: &'a [Event],
}

/// One order book per stock locate, driven by ITCH messages.
#[derive(Debug, Default)]
pub struct Replay {
    books: Vec<OrderBook>,
    symbols: Vec<Option<Symbol>>,
    /// Last `H` trading state byte per locate; 0 before the first `H`.
    trading_states: Vec<u8>,
    /// Whether the book of a locate has had an unexpected trade.
    diverged: Vec<bool>,
    events: Vec<Event>,
    next_taker: OrderId,
    stats: Stats,
}

impl Replay {
    pub fn new() -> Self {
        Self {
            next_taker: TAKER_ID_BASE,
            ..Self::default()
        }
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// The symbol a stock directory message gave `locate`, if one has arrived.
    pub fn symbol(&self, locate: Locate) -> Option<Symbol> {
        self.symbols.get(usize::from(locate)).copied().flatten()
    }

    /// The last trading state (`H`, `P`, `Q` or `T`) an `H` message gave `locate`, if any.
    pub fn trading_state(&self, locate: Locate) -> Option<u8> {
        match self.trading_states.get(usize::from(locate)) {
            Some(&state) if state != 0 => Some(state),
            _ => None,
        }
    }

    /// Whether the book of `locate` has had an unexpected trade and so differs from Nasdaq's.
    pub fn diverged(&self, locate: Locate) -> bool {
        self.diverged
            .get(usize::from(locate))
            .copied()
            .unwrap_or(false)
    }

    pub fn book(&self, locate: Locate) -> Option<&OrderBook> {
        self.books.get(usize::from(locate))
    }

    /// Every book that has received a command, by ascending locate (books never touched by a
    /// command are empty and may be skipped).
    pub fn books(&self) -> impl Iterator<Item = (Locate, &OrderBook)> {
        self.books.iter().enumerate().map(|(i, book)| {
            (
                Locate::try_from(i).expect("books are indexed by a u16"),
                book,
            )
        })
    }

    /// Applies one message to the book of its locate.
    pub fn apply(&mut self, msg: &Message) -> Result<Applied<'_>, ReplayError> {
        let locate = msg.header().locate;
        let kind_byte = match msg {
            Message::SystemEvent { .. } => b'S',
            Message::StockDirectory { .. } => b'R',
            Message::StockTradingAction { .. } => b'H',
            Message::AddOrder { attribution, .. } => {
                if attribution.is_some() {
                    b'F'
                } else {
                    b'A'
                }
            }
            Message::OrderExecuted { with_price, .. } => {
                if with_price.is_some() {
                    b'C'
                } else {
                    b'E'
                }
            }
            Message::OrderCancel { .. } => b'X',
            Message::OrderDelete { .. } => b'D',
            Message::OrderReplace { .. } => b'U',
            Message::Other { kind, .. } => *kind,
        };
        self.stats.messages += 1;
        self.stats.by_type[usize::from(kind_byte)] += 1;
        self.events.clear();

        let (command, kind) = match *msg {
            Message::StockDirectory { stock, .. } => {
                *slot(&mut self.symbols, locate) = Some(stock);
                return Ok(self.applied(locate, None));
            }
            Message::StockTradingAction { state, .. } => {
                *slot(&mut self.trading_states, locate) = state;
                return Ok(self.applied(locate, None));
            }
            Message::AddOrder {
                reference,
                buy,
                shares,
                price,
                ..
            } => {
                check_reference(locate, reference)?;
                let side = if buy { Side::Buy } else { Side::Sell };
                let cmd = Command::Limit {
                    id: reference,
                    side,
                    price,
                    qty: shares,
                };
                (cmd, CommandKind::Add)
            }
            Message::OrderDelete { reference, .. } => {
                (Command::Cancel { id: reference }, CommandKind::Delete)
            }
            Message::OrderCancel {
                reference,
                cancelled_shares,
                ..
            } => (
                Command::Reduce {
                    id: reference,
                    by: cancelled_shares,
                },
                CommandKind::PartialCancel,
            ),
            Message::OrderReplace {
                original_reference,
                new_reference,
                shares,
                price,
                ..
            } => {
                check_reference(locate, new_reference)?;
                let cmd = Command::Replace {
                    id: original_reference,
                    new_id: new_reference,
                    price,
                    qty: shares,
                };
                (cmd, CommandKind::Replace)
            }
            Message::OrderExecuted {
                reference,
                executed_shares,
                with_price,
                ..
            } => self.execution(
                locate,
                reference,
                executed_shares,
                with_price.map(|p| p.price),
            ),
            Message::SystemEvent { .. } | Message::Other { .. } => {
                return Ok(self.applied(locate, None));
            }
        };

        book_at(&mut self.books, locate).apply(command, &mut self.events);
        self.stats.book_messages += 1;
        self.stats.events += self.events.len() as u64;
        let trades = self
            .events
            .iter()
            .filter(|e| matches!(e, Event::Trade { .. }))
            .count() as u64;
        self.stats.trades += trades;
        if let Some(Event::Rejected { reason, .. }) = self.events.first() {
            let k = CommandKind::ALL
                .iter()
                .position(|&c| c == kind)
                .expect("listed");
            self.stats.rejections[k][reason_index(*reason)] += 1;
            if self.diverged(locate) {
                self.stats.rejections_on_diverged_books += 1;
            }
        }
        match kind {
            CommandKind::Add | CommandKind::Replace if trades > 0 => {
                self.stats.unexpected_trade_messages += 1;
                self.stats.unexpected_trades += trades;
                if self.trading_state(locate) == Some(b'T') {
                    self.stats.unexpected_trade_messages_while_trading += 1;
                    if !self.diverged(locate) {
                        self.stats.unexpected_trade_messages_on_clean_trading_books += 1;
                    }
                }
                *slot(&mut self.diverged, locate) = true;
            }
            CommandKind::ExecutionAggressor => {
                let Message::OrderExecuted { reference, .. } = *msg else {
                    unreachable!("only executions become aggressors");
                };
                self.check_aggressor(locate, command, reference)?;
            }
            _ => {}
        }
        Ok(self.applied(locate, Some(command)))
    }

    fn applied(&self, locate: Locate, command: Option<Command>) -> Applied<'_> {
        Applied {
            locate,
            command,
            events: &self.events,
        }
    }

    /// Chooses the command for an execution of `shares` against resting order `reference`.
    fn execution(
        &mut self,
        locate: Locate,
        reference: OrderId,
        shares: u32,
        c_price: Option<u32>,
    ) -> (Command, CommandKind) {
        let fallback = Command::Reduce {
            id: reference,
            by: shares,
        };
        let book = book_at(&mut self.books, locate);
        let Some(order) = book.order(reference) else {
            self.stats.exec_fallback_unknown += 1;
            return (fallback, CommandKind::ExecutionFallback);
        };
        if c_price.is_some_and(|p| p != order.price) {
            self.stats.c_price_differs += 1;
        }
        if shares > order.qty {
            self.stats.exec_fallback_too_large += 1;
            return (fallback, CommandKind::ExecutionFallback);
        }
        if book.front_order(order.side) != Some(reference) {
            let best = match order.side {
                Side::Buy => book.best_bid(),
                Side::Sell => book.best_ask(),
            };
            if best == Some(order.price) {
                self.stats.exec_fallback_behind_at_best += 1;
            } else {
                self.stats.exec_fallback_not_best_price += 1;
            }
            return (fallback, CommandKind::ExecutionFallback);
        }
        let taker = self.next_taker;
        self.next_taker += 1;
        let side = match order.side {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        };
        let cmd = Command::Limit {
            id: taker,
            side,
            price: order.price,
            qty: shares,
        };
        (cmd, CommandKind::ExecutionAggressor)
    }

    /// The events of a replayed aggressor must be exactly: accepted, one trade against the named
    /// order for all the shares at the limit price, the named order's fill if that emptied it,
    /// and the taker's fill.
    fn check_aggressor(
        &mut self,
        locate: Locate,
        command: Command,
        named: OrderId,
    ) -> Result<(), ReplayError> {
        let Command::Limit {
            id: taker,
            price,
            qty,
            ..
        } = command
        else {
            unreachable!("aggressors are limit orders");
        };
        let ok = match self.events.as_slice() {
            [
                Event::Accepted { id: a },
                Event::Trade {
                    taker_id,
                    maker_id,
                    price: p,
                    qty: q,
                },
                rest @ ..,
            ] if *a == taker
                && *taker_id == taker
                && *maker_id == named
                && *p == price
                && *q == qty =>
            {
                match rest {
                    [Event::Filled { id }] => *id == taker,
                    [Event::Filled { id: m }, Event::Filled { id: t }] => {
                        *m == named && *t == taker
                    }
                    _ => false,
                }
            }
            _ => false,
        };
        if ok {
            self.stats.exec_aggressor += 1;
            Ok(())
        } else {
            Err(ReplayError::AggressorMismatch {
                locate,
                reference: named,
                events: self.events.clone(),
            })
        }
    }
}

/// The entry for `locate` in a per-locate table, growing the table with defaults as needed.
fn slot<T: Default + Clone>(table: &mut Vec<T>, locate: Locate) -> &mut T {
    let i = usize::from(locate);
    if table.len() <= i {
        table.resize(i + 1, T::default());
    }
    &mut table[i]
}

fn book_at(books: &mut Vec<OrderBook>, locate: Locate) -> &mut OrderBook {
    let i = usize::from(locate);
    if books.len() <= i {
        books.resize_with(i + 1, OrderBook::new);
    }
    &mut books[i]
}

fn check_reference(locate: Locate, reference: u64) -> Result<(), ReplayError> {
    if reference >= TAKER_ID_BASE {
        Err(ReplayError::ReferenceInTakerSpace { locate, reference })
    } else {
        Ok(())
    }
}
