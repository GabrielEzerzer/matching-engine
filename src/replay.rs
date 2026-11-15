//! Replays an ITCH 5.0 stream through one order book per stock locate.

use crate::itch::{Locate, Message, Symbol};
use crate::{Command, Event, OrderBook, OrderId, RejectReason, Side};

pub const TAKER_ID_BASE: OrderId = 1 << 63;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandKind {
    Add,
    Delete,
    PartialCancel,
    Replace,
    ExecutionAggressor,
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
}

fn reason_index(reason: RejectReason) -> usize {
    match reason {
        RejectReason::ZeroQuantity => 0,
        RejectReason::DuplicateOrderId => 1,
        RejectReason::UnknownOrder => 2,
        RejectReason::ReduceTooLarge => 3,
    }
}

#[derive(Clone, Debug)]
pub struct Stats {
    pub messages: u64,
    pub by_type: [u64; 256],
    pub book_messages: u64,
    pub events: u64,
    pub trades: u64,
    pub exec_aggressor: u64,
    pub exec_fallback_not_best_price: u64,
    pub exec_fallback_behind_at_best: u64,
    pub exec_fallback_unknown: u64,
    pub exec_fallback_too_large: u64,
    pub exec_fallback_zero_shares: u64,
    pub c_price_differs: u64,
    pub rejections: [[u64; 4]; 6],
    pub unexpected_trade_messages: u64,
    pub unexpected_trade_messages_while_trading: u64,
    pub unexpected_trade_messages_on_clean_trading_books: u64,
    pub unexpected_trades: u64,
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
            exec_fallback_zero_shares: 0,
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
            + self.exec_fallback_zero_shares
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayError {
    ReferenceInTakerSpace {
        locate: Locate,
        reference: u64,
    },
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

#[derive(Debug)]
pub struct Applied<'a> {
    pub locate: Locate,
    pub command: Option<Command>,
    pub events: &'a [Event],
}

#[derive(Debug)]
pub struct Replay {
    books: Vec<OrderBook>,
    symbols: Vec<Option<Symbol>>,
    trading_states: Vec<u8>,
    diverged: Vec<bool>,
    events: Vec<Event>,
    next_taker: OrderId,
    stats: Stats,
}

impl Default for Replay {
    fn default() -> Self {
        Self::new()
    }
}

impl Replay {
    pub fn new() -> Self {
        Self {
            books: Vec::new(),
            symbols: Vec::new(),
            trading_states: Vec::new(),
            diverged: Vec::new(),
            events: Vec::new(),
            next_taker: TAKER_ID_BASE,
            stats: Stats::default(),
        }
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    pub fn symbol(&self, locate: Locate) -> Option<Symbol> {
        self.symbols.get(usize::from(locate)).copied().flatten()
    }

    pub fn trading_state(&self, locate: Locate) -> Option<u8> {
        match self.trading_states.get(usize::from(locate)) {
            Some(&state) if state != 0 => Some(state),
            _ => None,
        }
    }

    pub fn diverged(&self, locate: Locate) -> bool {
        self.diverged
            .get(usize::from(locate))
            .copied()
            .unwrap_or(false)
    }

    pub fn book(&self, locate: Locate) -> Option<&OrderBook> {
        self.books.get(usize::from(locate))
    }

    pub fn books(&self) -> impl Iterator<Item = (Locate, &OrderBook)> {
        self.books.iter().enumerate().map(|(i, book)| {
            (
                Locate::try_from(i).expect("books are indexed by a u16"),
                book,
            )
        })
    }

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
                if !matches!(self.trading_state(locate), Some(b'H' | b'P' | b'Q')) {
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
        if shares == 0 {
            self.stats.exec_fallback_zero_shares += 1;
            return (fallback, CommandKind::ExecutionFallback);
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
