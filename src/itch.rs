//! A zero-copy parser for Nasdaq TotalView-ITCH 5.0 messages and their length-prefixed frames.

use std::fmt;

pub type Locate = u16;

pub type Symbol = [u8; 8];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub locate: Locate,
    pub tracking: u16,
    pub timestamp: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message {
    SystemEvent {
        header: Header,
        event: u8,
    },
    StockDirectory {
        header: Header,
        stock: Symbol,
        market_category: u8,
        financial_status: u8,
        round_lot_size: u32,
        round_lots_only: u8,
        issue_classification: u8,
        issue_subtype: [u8; 2],
        authenticity: u8,
        short_sale_threshold: u8,
        ipo_flag: u8,
        luld_reference_price_tier: u8,
        etp_flag: u8,
        etp_leverage_factor: u32,
        inverse_indicator: u8,
    },
    StockTradingAction {
        header: Header,
        stock: Symbol,
        state: u8,
        reason: [u8; 4],
    },
    AddOrder {
        header: Header,
        reference: u64,
        buy: bool,
        shares: u32,
        stock: Symbol,
        price: u32,
        attribution: Option<[u8; 4]>,
    },
    OrderExecuted {
        header: Header,
        reference: u64,
        executed_shares: u32,
        match_number: u64,
        with_price: Option<ExecutionPrice>,
    },
    OrderCancel {
        header: Header,
        reference: u64,
        cancelled_shares: u32,
    },
    OrderDelete {
        header: Header,
        reference: u64,
    },
    OrderReplace {
        header: Header,
        original_reference: u64,
        new_reference: u64,
        shares: u32,
        price: u32,
    },
    Other {
        kind: u8,
        header: Header,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionPrice {
    pub printable: bool,
    pub price: u32,
}

impl Message {
    pub fn header(&self) -> &Header {
        match self {
            Message::SystemEvent { header, .. }
            | Message::StockDirectory { header, .. }
            | Message::StockTradingAction { header, .. }
            | Message::AddOrder { header, .. }
            | Message::OrderExecuted { header, .. }
            | Message::OrderCancel { header, .. }
            | Message::OrderDelete { header, .. }
            | Message::OrderReplace { header, .. }
            | Message::Other { header, .. } => header,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItchError {
    Truncated {
        needed: usize,
        available: usize,
    },
    Empty,
    UnknownType {
        kind: u8,
    },
    BadLength {
        kind: u8,
        expected: usize,
        actual: usize,
    },
    BadField {
        kind: u8,
        field: &'static str,
        value: u8,
    },
}

impl fmt::Display for ItchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ItchError::Truncated { needed, available } => {
                write!(f, "truncated frame: needs {needed} bytes, {available} left")
            }
            ItchError::Empty => write!(f, "zero-length message"),
            ItchError::UnknownType { kind } => {
                write!(f, "unknown message type 0x{kind:02x} ({:?})", kind as char)
            }
            ItchError::BadLength {
                kind,
                expected,
                actual,
            } => write!(
                f,
                "message type {:?} must be {expected} bytes, got {actual}",
                kind as char
            ),
            ItchError::BadField { kind, field, value } => write!(
                f,
                "message type {:?}: invalid {field} byte 0x{value:02x}",
                kind as char
            ),
        }
    }
}

impl std::error::Error for ItchError {}

pub const fn message_length(kind: u8) -> Option<usize> {
    Some(match kind {
        b'S' => 12,
        b'R' => 39,
        b'H' => 25,
        b'Y' => 20,
        b'L' => 26,
        b'V' => 35,
        b'W' => 12,
        b'K' => 28,
        b'J' => 35,
        b'h' => 21,
        b'A' => 36,
        b'F' => 40,
        b'E' => 31,
        b'C' => 36,
        b'X' => 23,
        b'D' => 19,
        b'U' => 35,
        b'P' => 44,
        b'Q' => 40,
        b'B' => 19,
        b'I' => 50,
        b'N' => 20,
        b'O' => 48,
        _ => return None,
    })
}

const fn len(kind: u8) -> usize {
    match message_length(kind) {
        Some(n) => n,
        None => panic!("not an ITCH 5.0 message type"),
    }
}

#[inline(always)]
pub fn decode(msg: &[u8]) -> Result<Message, ItchError> {
    let Some(&kind) = msg.first() else {
        return Err(ItchError::Empty);
    };
    Ok(match kind {
        b'A' => {
            let m = fixed::<{ len(b'A') }>(kind, msg)?;
            Message::AddOrder {
                header: header(m),
                reference: u64::from_be_bytes(bytes(m, 11)),
                buy: side(kind, m[19])?,
                shares: u32::from_be_bytes(bytes(m, 20)),
                stock: bytes(m, 24),
                price: u32::from_be_bytes(bytes(m, 32)),
                attribution: None,
            }
        }
        b'D' => {
            let m = fixed::<{ len(b'D') }>(kind, msg)?;
            Message::OrderDelete {
                header: header(m),
                reference: u64::from_be_bytes(bytes(m, 11)),
            }
        }
        b'U' => {
            let m = fixed::<{ len(b'U') }>(kind, msg)?;
            Message::OrderReplace {
                header: header(m),
                original_reference: u64::from_be_bytes(bytes(m, 11)),
                new_reference: u64::from_be_bytes(bytes(m, 19)),
                shares: u32::from_be_bytes(bytes(m, 27)),
                price: u32::from_be_bytes(bytes(m, 31)),
            }
        }
        b'E' => {
            let m = fixed::<{ len(b'E') }>(kind, msg)?;
            Message::OrderExecuted {
                header: header(m),
                reference: u64::from_be_bytes(bytes(m, 11)),
                executed_shares: u32::from_be_bytes(bytes(m, 19)),
                match_number: u64::from_be_bytes(bytes(m, 23)),
                with_price: None,
            }
        }
        b'X' => {
            let m = fixed::<{ len(b'X') }>(kind, msg)?;
            Message::OrderCancel {
                header: header(m),
                reference: u64::from_be_bytes(bytes(m, 11)),
                cancelled_shares: u32::from_be_bytes(bytes(m, 19)),
            }
        }
        b'F' => {
            let m = fixed::<{ len(b'F') }>(kind, msg)?;
            Message::AddOrder {
                header: header(m),
                reference: u64::from_be_bytes(bytes(m, 11)),
                buy: side(kind, m[19])?,
                shares: u32::from_be_bytes(bytes(m, 20)),
                stock: bytes(m, 24),
                price: u32::from_be_bytes(bytes(m, 32)),
                attribution: Some(bytes(m, 36)),
            }
        }
        b'C' => {
            let m = fixed::<{ len(b'C') }>(kind, msg)?;
            let printable = match m[31] {
                b'Y' => true,
                b'N' => false,
                value => {
                    return Err(ItchError::BadField {
                        kind,
                        field: "printable",
                        value,
                    });
                }
            };
            Message::OrderExecuted {
                header: header(m),
                reference: u64::from_be_bytes(bytes(m, 11)),
                executed_shares: u32::from_be_bytes(bytes(m, 19)),
                match_number: u64::from_be_bytes(bytes(m, 23)),
                with_price: Some(ExecutionPrice {
                    printable,
                    price: u32::from_be_bytes(bytes(m, 32)),
                }),
            }
        }
        b'S' | b'R' | b'H' => return decode_admin(kind, msg),
        _ => {
            let expected = message_length(kind).ok_or(ItchError::UnknownType { kind })?;
            if msg.len() != expected {
                return Err(bad_length(kind, expected, msg));
            }
            let Some(m) = msg.first_chunk::<11>() else {
                unreachable!("every message type is longer than its header");
            };
            Message::Other {
                kind,
                header: header(m),
            }
        }
    })
}

#[cold]
#[inline(never)]
fn decode_admin(kind: u8, msg: &[u8]) -> Result<Message, ItchError> {
    Ok(match kind {
        b'S' => {
            let m = fixed::<{ len(b'S') }>(kind, msg)?;
            Message::SystemEvent {
                header: header(m),
                event: m[11],
            }
        }
        b'R' => {
            let m = fixed::<{ len(b'R') }>(kind, msg)?;
            Message::StockDirectory {
                header: header(m),
                stock: bytes(m, 11),
                market_category: m[19],
                financial_status: m[20],
                round_lot_size: u32::from_be_bytes(bytes(m, 21)),
                round_lots_only: m[25],
                issue_classification: m[26],
                issue_subtype: bytes(m, 27),
                authenticity: m[29],
                short_sale_threshold: m[30],
                ipo_flag: m[31],
                luld_reference_price_tier: m[32],
                etp_flag: m[33],
                etp_leverage_factor: u32::from_be_bytes(bytes(m, 34)),
                inverse_indicator: m[38],
            }
        }
        b'H' => {
            let m = fixed::<{ len(b'H') }>(kind, msg)?;
            Message::StockTradingAction {
                header: header(m),
                stock: bytes(m, 11),
                state: m[19],
                reason: bytes(m, 21),
            }
        }
        _ => unreachable!("only S, R and H reach decode_admin"),
    })
}

#[inline(always)]
pub fn split_frame(buf: &[u8]) -> Result<Option<(&[u8], usize)>, ItchError> {
    if buf.is_empty() {
        return Ok(None);
    }
    if buf.len() < 2 {
        return Err(ItchError::Truncated {
            needed: 2,
            available: buf.len(),
        });
    }
    let len = usize::from(u16::from_be_bytes([buf[0], buf[1]]));
    let total = 2 + len;
    match buf.get(2..total) {
        Some(body) => Ok(Some((body, total))),
        None => Err(ItchError::Truncated {
            needed: total,
            available: buf.len(),
        }),
    }
}

#[derive(Clone, Debug)]
pub struct Frames<'a> {
    buf: &'a [u8],
    offset: usize,
    failed: bool,
}

impl<'a> Frames<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            offset: 0,
            failed: false,
        }
    }

    pub fn offset(&self) -> usize {
        self.offset
    }
}

impl Iterator for Frames<'_> {
    type Item = Result<Message, ItchError>;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let result = match split_frame(&self.buf[self.offset..]) {
            Ok(None) => return None,
            Ok(Some((body, used))) => decode(body).map(|msg| (msg, used)),
            Err(e) => Err(e),
        };
        match result {
            Ok((msg, used)) => {
                self.offset += used;
                Some(Ok(msg))
            }
            Err(e) => {
                self.failed = true;
                Some(Err(e))
            }
        }
    }
}

#[inline(always)]
fn side(kind: u8, byte: u8) -> Result<bool, ItchError> {
    match byte {
        b'B' => Ok(true),
        b'S' => Ok(false),
        value => Err(ItchError::BadField {
            kind,
            field: "side",
            value,
        }),
    }
}

#[inline(always)]
fn fixed<const N: usize>(kind: u8, msg: &[u8]) -> Result<&[u8; N], ItchError> {
    msg.try_into().map_err(|_| bad_length(kind, N, msg))
}

#[cold]
#[inline(never)]
fn bad_length(kind: u8, expected: usize, msg: &[u8]) -> ItchError {
    ItchError::BadLength {
        kind,
        expected,
        actual: msg.len(),
    }
}

#[inline(always)]
fn header<const N: usize>(m: &[u8; N]) -> Header {
    let tracking_and_time = u64::from_be_bytes(bytes(m, 3));
    Header {
        locate: u16::from_be_bytes(bytes(m, 1)),
        tracking: (tracking_and_time >> 48) as u16,
        timestamp: tracking_and_time & 0xffff_ffff_ffff,
    }
}

#[inline(always)]
fn bytes<const W: usize, const N: usize>(m: &[u8; N], at: usize) -> [u8; W] {
    let mut x = [0u8; W];
    x.copy_from_slice(&m[at..at + W]);
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(kind: u8, locate: u16, body: &[u8]) -> Vec<u8> {
        let tracking = 0x0102u16.to_be_bytes();
        let timestamp = [0x00, 0x00, 0x12, 0x34, 0x56, 0x78];
        let mut m = vec![kind];
        m.extend_from_slice(&locate.to_be_bytes());
        m.extend_from_slice(&tracking);
        m.extend_from_slice(&timestamp);
        m.extend_from_slice(body);
        m
    }

    fn frame(m: &[u8]) -> Vec<u8> {
        let mut f = (m.len() as u16).to_be_bytes().to_vec();
        f.extend_from_slice(m);
        f
    }

    const HEADER: fn(u16) -> Header = |locate| Header {
        locate,
        tracking: 0x0102,
        timestamp: 0x1234_5678,
    };

    #[test]
    fn system_event() {
        assert_eq!(
            decode(&msg(b'S', 0, b"O")),
            Ok(Message::SystemEvent {
                header: HEADER(0),
                event: b'O'
            })
        );
    }

    #[test]
    fn stock_trading_action() {
        let mut body = b"ZWZZT   ".to_vec();
        body.extend_from_slice(b"H IPO1");
        let m = msg(b'H', 8900, &body);
        assert_eq!(m.len(), 25);
        assert_eq!(
            decode(&m),
            Ok(Message::StockTradingAction {
                header: HEADER(8900),
                stock: *b"ZWZZT   ",
                state: b'H',
                reason: *b"IPO1",
            })
        );
    }

    #[test]
    fn stock_directory() {
        let mut body = b"AAPL    ".to_vec();
        body.extend_from_slice(b"QN");
        body.extend_from_slice(&100u32.to_be_bytes());
        body.extend_from_slice(b"NCZ ");
        body.extend_from_slice(b"P");
        body.extend_from_slice(b"N");
        body.extend_from_slice(b"N");
        body.extend_from_slice(b"1");
        body.extend_from_slice(b"N");
        body.extend_from_slice(&3u32.to_be_bytes());
        body.extend_from_slice(b"Y");
        let m = msg(b'R', 7, &body);
        assert_eq!(m.len(), 39);
        assert_eq!(
            decode(&m),
            Ok(Message::StockDirectory {
                header: HEADER(7),
                stock: *b"AAPL    ",
                market_category: b'Q',
                financial_status: b'N',
                round_lot_size: 100,
                round_lots_only: b'N',
                issue_classification: b'C',
                issue_subtype: *b"Z ",
                authenticity: b'P',
                short_sale_threshold: b'N',
                ipo_flag: b'N',
                luld_reference_price_tier: b'1',
                etp_flag: b'N',
                etp_leverage_factor: 3,
                inverse_indicator: b'Y',
            })
        );
    }

    fn add_body(reference: u64, side: u8, shares: u32, price: u32) -> Vec<u8> {
        let mut body = reference.to_be_bytes().to_vec();
        body.push(side);
        body.extend_from_slice(&shares.to_be_bytes());
        body.extend_from_slice(b"MSFT    ");
        body.extend_from_slice(&price.to_be_bytes());
        body
    }

    #[test]
    fn add_order() {
        let m = msg(
            b'A',
            3,
            &add_body(0x0102_0304_0506_0708, b'B', 300, 1_234_500),
        );
        assert_eq!(
            decode(&m),
            Ok(Message::AddOrder {
                header: HEADER(3),
                reference: 0x0102_0304_0506_0708,
                buy: true,
                shares: 300,
                stock: *b"MSFT    ",
                price: 1_234_500,
                attribution: None,
            })
        );
    }

    #[test]
    fn add_order_with_mpid() {
        let mut body = add_body(9, b'S', 1, u32::MAX);
        body.extend_from_slice(b"GSCO");
        assert_eq!(
            decode(&msg(b'F', 65535, &body)),
            Ok(Message::AddOrder {
                header: HEADER(65535),
                reference: 9,
                buy: false,
                shares: 1,
                stock: *b"MSFT    ",
                price: u32::MAX,
                attribution: Some(*b"GSCO"),
            })
        );
    }

    #[test]
    fn add_order_bad_side() {
        assert_eq!(
            decode(&msg(b'A', 3, &add_body(1, b'Z', 1, 1))),
            Err(ItchError::BadField {
                kind: b'A',
                field: "side",
                value: b'Z'
            })
        );
    }

    fn exec_body(reference: u64, shares: u32, match_number: u64) -> Vec<u8> {
        let mut body = reference.to_be_bytes().to_vec();
        body.extend_from_slice(&shares.to_be_bytes());
        body.extend_from_slice(&match_number.to_be_bytes());
        body
    }

    #[test]
    fn order_executed() {
        assert_eq!(
            decode(&msg(b'E', 1, &exec_body(42, 100, 77))),
            Ok(Message::OrderExecuted {
                header: HEADER(1),
                reference: 42,
                executed_shares: 100,
                match_number: 77,
                with_price: None,
            })
        );
    }

    #[test]
    fn order_executed_with_price() {
        let mut body = exec_body(42, 100, 77);
        body.push(b'N');
        body.extend_from_slice(&5_000_000u32.to_be_bytes());
        assert_eq!(
            decode(&msg(b'C', 1, &body)),
            Ok(Message::OrderExecuted {
                header: HEADER(1),
                reference: 42,
                executed_shares: 100,
                match_number: 77,
                with_price: Some(ExecutionPrice {
                    printable: false,
                    price: 5_000_000
                }),
            })
        );
        let mut bad = exec_body(42, 100, 77);
        bad.push(b'?');
        bad.extend_from_slice(&1u32.to_be_bytes());
        assert!(matches!(
            decode(&msg(b'C', 1, &bad)),
            Err(ItchError::BadField {
                field: "printable",
                ..
            })
        ));
    }

    #[test]
    fn order_cancel() {
        let mut body = 5u64.to_be_bytes().to_vec();
        body.extend_from_slice(&25u32.to_be_bytes());
        assert_eq!(
            decode(&msg(b'X', 2, &body)),
            Ok(Message::OrderCancel {
                header: HEADER(2),
                reference: 5,
                cancelled_shares: 25
            })
        );
    }

    #[test]
    fn order_delete() {
        assert_eq!(
            decode(&msg(b'D', 2, &u64::MAX.to_be_bytes())),
            Ok(Message::OrderDelete {
                header: HEADER(2),
                reference: u64::MAX
            })
        );
    }

    #[test]
    fn order_replace() {
        let mut body = 5u64.to_be_bytes().to_vec();
        body.extend_from_slice(&6u64.to_be_bytes());
        body.extend_from_slice(&200u32.to_be_bytes());
        body.extend_from_slice(&999u32.to_be_bytes());
        assert_eq!(
            decode(&msg(b'U', 2, &body)),
            Ok(Message::OrderReplace {
                header: HEADER(2),
                original_reference: 5,
                new_reference: 6,
                shares: 200,
                price: 999
            })
        );
    }

    #[test]
    fn every_other_type_is_recognised_and_length_checked() {
        for &kind in b"YLVWKJhPQBINO" {
            let len = message_length(kind).unwrap();
            let m = msg(kind, 9, &vec![0u8; len - 11]);
            assert_eq!(
                decode(&m),
                Ok(Message::Other {
                    kind,
                    header: HEADER(9)
                })
            );
            assert_eq!(
                decode(&m[..len - 1]),
                Err(ItchError::BadLength {
                    kind,
                    expected: len,
                    actual: len - 1
                })
            );
            let mut long = m.clone();
            long.push(0);
            assert!(matches!(decode(&long), Err(ItchError::BadLength { .. })));
        }
    }

    #[test]
    fn decoded_types_are_length_checked() {
        for &kind in b"SRHAFECXDU" {
            let len = message_length(kind).unwrap();
            for wrong in [11, len - 1, len + 1] {
                let m = msg(kind, 0, &vec![b'B'; wrong - 11]);
                assert_eq!(
                    decode(&m),
                    Err(ItchError::BadLength {
                        kind,
                        expected: len,
                        actual: wrong
                    }),
                    "type {}",
                    kind as char
                );
            }
        }
    }

    #[test]
    fn unknown_and_empty() {
        assert_eq!(decode(&[]), Err(ItchError::Empty));
        for kind in [b'Z', b'a', 0u8, 0xff] {
            assert_eq!(
                decode(&msg(kind, 0, &[0; 20])),
                Err(ItchError::UnknownType { kind })
            );
        }
    }

    #[test]
    fn frames_walk_a_stream_and_report_truncation() {
        let a = msg(b'D', 1, &1u64.to_be_bytes());
        let b = msg(b'S', 0, b"C");
        let mut stream = frame(&a);
        stream.extend_from_slice(&frame(&b));
        let full: Vec<_> = Frames::new(&stream).collect();
        assert_eq!(full.len(), 2);
        assert!(full.iter().all(Result::is_ok));

        let boundary = 2 + a.len();
        for cut in 1..stream.len() {
            let mut frames = Frames::new(&stream[..cut]);
            let items: Vec<_> = frames.by_ref().collect();
            if cut == boundary {
                assert_eq!(items.len(), 1);
                assert!(items[0].is_ok());
                continue;
            }
            let last = items.last().unwrap();
            assert!(
                matches!(last, Err(ItchError::Truncated { .. })),
                "cut {cut}: {last:?}"
            );
            assert_eq!(items.len(), if cut > boundary { 2 } else { 1 });
            assert_eq!(frames.offset(), if cut > boundary { boundary } else { 0 });
        }
    }

    #[test]
    fn frames_stop_at_a_corrupt_frame() {
        let good = frame(&msg(b'S', 0, b"O"));
        let mut stream = good.clone();
        stream.extend_from_slice(&frame(&msg(b'Z', 0, &[0; 4])));
        stream.extend_from_slice(&good);
        let items: Vec<_> = Frames::new(&stream).collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1], Err(ItchError::UnknownType { kind: b'Z' }));

        let mut wrong = vec![0, 13];
        wrong.extend_from_slice(&msg(b'S', 0, b"OX"));
        assert!(matches!(
            Frames::new(&wrong).next(),
            Some(Err(ItchError::BadLength { .. }))
        ));
        assert_eq!(Frames::new(&[0, 0]).next(), Some(Err(ItchError::Empty)));
    }
}
