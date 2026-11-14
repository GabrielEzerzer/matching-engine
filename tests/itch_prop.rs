//! Property tests for the ITCH 5.0 decoder: `decode` and `Frames` must return a result, never
//! panic, on any bytes, and every result must agree with an oracle for the wire layout that is
//! written apart from the decoder. For validation the oracle follows the parser's own contract:
//! only the two constrained fields are checked, so any other field value is accepted and must
//! round-trip.
//!
//! The oracle is two pieces written apart from `src/itch.rs`: `expected_error`, which says which
//! error (if any) a message body must produce, from the spec's length table and its two
//! constrained fields; and `encode`, which writes a decoded `Message` back out field by field.
//! A successful decode must re-encode to exactly the bytes it came from, so a field read from the
//! wrong offset, or a message accepted at the wrong length, fails.
//!
//! Runs from a fixed seed. Case count defaults to `DEFAULT_CASES`; set `PROPTEST_CASES` to run
//! more.

use matching_engine::itch::{
    ExecutionPrice, Frames, Header, ItchError, Message, decode, message_length,
};
use proptest::prelude::*;
use proptest::sample::{Index, select};
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, RngSeed, TestRunner};

const DEFAULT_CASES: u32 = 4_096;

fn config() -> Config {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_CASES);
    Config {
        cases,
        rng_seed: RngSeed::Fixed(0x1754_5000),
        // The seed is fixed, so a failure reproduces without a regressions file.
        failure_persistence: None,
        ..Config::default()
    }
}

/// Every ITCH 5.0 message type and its length (type byte included), from the spec's tables.
/// Kept apart from `message_length` so a wrong entry there cannot pass unseen.
const SPEC: [(u8, usize); 23] = [
    (b'S', 12),
    (b'R', 39),
    (b'H', 25),
    (b'Y', 20),
    (b'L', 26),
    (b'V', 35),
    (b'W', 12),
    (b'K', 28),
    (b'J', 35),
    (b'h', 21),
    (b'A', 36),
    (b'F', 40),
    (b'E', 31),
    (b'C', 36),
    (b'X', 23),
    (b'D', 19),
    (b'U', 35),
    (b'P', 44),
    (b'Q', 40),
    (b'B', 19),
    (b'I', 50),
    (b'N', 20),
    (b'O', 48),
];

fn spec_length(kind: u8) -> Option<usize> {
    SPEC.iter().find(|&&(k, _)| k == kind).map(|&(_, n)| n)
}

/// Offsets of the only bytes whose value the decoder checks: the side of an add order and the
/// printable flag of an execution with price.
const SIDE_AT: usize = 19;
const PRINTABLE_AT: usize = 31;

/// The error `decode` must return for `m`, or `None` if it must decode. The order of the checks
/// is the documented precedence: empty, unknown type, bad length, bad field.
fn expected_error(m: &[u8]) -> Option<ItchError> {
    let Some(&kind) = m.first() else {
        return Some(ItchError::Empty);
    };
    let Some(expected) = spec_length(kind) else {
        return Some(ItchError::UnknownType { kind });
    };
    if m.len() != expected {
        return Some(ItchError::BadLength {
            kind,
            expected,
            actual: m.len(),
        });
    }
    let (field, at, allowed) = match kind {
        b'A' | b'F' => ("side", SIDE_AT, b"BS"),
        b'C' => ("printable", PRINTABLE_AT, b"YN"),
        _ => return None,
    };
    (!allowed.contains(&m[at])).then_some(ItchError::BadField {
        kind,
        field,
        value: m[at],
    })
}

/// Writes `msg` back out in wire order. `original` supplies the bytes a `Message` does not carry:
/// the reserved byte of `H` and the undecoded body of `Other`.
fn encode(msg: &Message, original: &[u8]) -> Vec<u8> {
    fn start(kind: u8, h: &Header) -> Vec<u8> {
        let mut out = vec![kind];
        out.extend_from_slice(&h.locate.to_be_bytes());
        out.extend_from_slice(&h.tracking.to_be_bytes());
        out.extend_from_slice(&h.timestamp.to_be_bytes()[2..]);
        out
    }
    match *msg {
        Message::SystemEvent { ref header, event } => {
            let mut out = start(b'S', header);
            out.push(event);
            out
        }
        Message::StockDirectory {
            ref header,
            stock,
            market_category,
            financial_status,
            round_lot_size,
            round_lots_only,
            issue_classification,
            issue_subtype,
            authenticity,
            short_sale_threshold,
            ipo_flag,
            luld_reference_price_tier,
            etp_flag,
            etp_leverage_factor,
            inverse_indicator,
        } => {
            let mut out = start(b'R', header);
            out.extend_from_slice(&stock);
            out.push(market_category);
            out.push(financial_status);
            out.extend_from_slice(&round_lot_size.to_be_bytes());
            out.push(round_lots_only);
            out.push(issue_classification);
            out.extend_from_slice(&issue_subtype);
            out.push(authenticity);
            out.push(short_sale_threshold);
            out.push(ipo_flag);
            out.push(luld_reference_price_tier);
            out.push(etp_flag);
            out.extend_from_slice(&etp_leverage_factor.to_be_bytes());
            out.push(inverse_indicator);
            out
        }
        Message::StockTradingAction {
            ref header,
            stock,
            state,
            reason,
        } => {
            let mut out = start(b'H', header);
            out.extend_from_slice(&stock);
            out.push(state);
            out.push(original[20]); // reserved
            out.extend_from_slice(&reason);
            out
        }
        Message::AddOrder {
            ref header,
            reference,
            buy,
            shares,
            stock,
            price,
            attribution,
        } => {
            let mut out = start(if attribution.is_some() { b'F' } else { b'A' }, header);
            out.extend_from_slice(&reference.to_be_bytes());
            out.push(if buy { b'B' } else { b'S' });
            out.extend_from_slice(&shares.to_be_bytes());
            out.extend_from_slice(&stock);
            out.extend_from_slice(&price.to_be_bytes());
            if let Some(mpid) = attribution {
                out.extend_from_slice(&mpid);
            }
            out
        }
        Message::OrderExecuted {
            ref header,
            reference,
            executed_shares,
            match_number,
            with_price,
        } => {
            let mut out = start(if with_price.is_some() { b'C' } else { b'E' }, header);
            out.extend_from_slice(&reference.to_be_bytes());
            out.extend_from_slice(&executed_shares.to_be_bytes());
            out.extend_from_slice(&match_number.to_be_bytes());
            if let Some(ExecutionPrice { printable, price }) = with_price {
                out.push(if printable { b'Y' } else { b'N' });
                out.extend_from_slice(&price.to_be_bytes());
            }
            out
        }
        Message::OrderCancel {
            ref header,
            reference,
            cancelled_shares,
        } => {
            let mut out = start(b'X', header);
            out.extend_from_slice(&reference.to_be_bytes());
            out.extend_from_slice(&cancelled_shares.to_be_bytes());
            out
        }
        Message::OrderDelete {
            ref header,
            reference,
        } => {
            let mut out = start(b'D', header);
            out.extend_from_slice(&reference.to_be_bytes());
            out
        }
        Message::OrderReplace {
            ref header,
            original_reference,
            new_reference,
            shares,
            price,
        } => {
            let mut out = start(b'U', header);
            out.extend_from_slice(&original_reference.to_be_bytes());
            out.extend_from_slice(&new_reference.to_be_bytes());
            out.extend_from_slice(&shares.to_be_bytes());
            out.extend_from_slice(&price.to_be_bytes());
            out
        }
        Message::Other { kind, ref header } => {
            // The types the decoder reads field by field must never come back undecoded.
            assert!(
                !b"SRHAFECXDU".contains(&kind),
                "type {} must be fully decoded",
                kind as char
            );
            let mut out = start(kind, header);
            out.extend_from_slice(&original[11..]);
            out
        }
    }
}

/// Decodes `m` twice and checks the result against the oracle. Any panic in `decode` fails the
/// calling test.
fn check(m: &[u8]) -> Result<Message, ItchError> {
    let got = decode(m);
    assert_eq!(got, decode(m), "decode is not repeatable on {m:02x?}");
    match (expected_error(m), got) {
        (Some(e), got) => assert_eq!(got, Err(e), "wrong result for {m:02x?}"),
        (None, Ok(msg)) => {
            assert!(msg.header().timestamp < 1 << 48, "timestamp over 48 bits");
            assert_eq!(encode(&msg, m), m, "decoded {msg:?} does not re-encode");
        }
        (None, Err(e)) => panic!("valid message {m:02x?} rejected: {e:?}"),
    }
    got
}

fn frame(m: &[u8]) -> Vec<u8> {
    let mut f = u16::try_from(m.len()).unwrap().to_be_bytes().to_vec();
    f.extend_from_slice(m);
    f
}

/// Walks `buf` with `Frames` and with a separate reading of the framing, and requires the same
/// items, the same offsets, and that the iterator stays finished after its end or first error.
fn check_frames(buf: &[u8]) {
    let mut frames = Frames::new(buf);
    let mut pos = 0;
    loop {
        let rest = &buf[pos..];
        let (want, used) = if rest.is_empty() {
            (None, 0)
        } else if rest.len() < 2 {
            (
                Some(Err(ItchError::Truncated {
                    needed: 2,
                    available: rest.len(),
                })),
                0,
            )
        } else {
            let total = 2 + usize::from(rest[0]) * 256 + usize::from(rest[1]);
            match rest.get(2..total) {
                Some(body) => (Some(check(body)), total),
                None => (
                    Some(Err(ItchError::Truncated {
                        needed: total,
                        available: rest.len(),
                    })),
                    0,
                ),
            }
        };
        let got = frames.next();
        assert_eq!(got, want, "frame at offset {pos} of {buf:02x?}");
        match want {
            Some(Ok(_)) => pos += used,
            _ => break,
        }
        assert_eq!(frames.offset(), pos);
    }
    // The walk above advances on every frame and stops at the first end or error, so it ends.
    // The iterator must end with it and stay ended.
    assert_eq!(frames.offset(), pos);
    for _ in 0..3 {
        assert_eq!(frames.next(), None, "iterator continued after its end");
    }
    assert_eq!(frames.offset(), pos);
}

/// A valid message of type `kind`: random field values, with the side and printable bytes set
/// to allowed values.
fn valid_message_of(kind: u8) -> impl Strategy<Value = Vec<u8>> {
    let len = spec_length(kind).unwrap();
    (
        prop::collection::vec(any::<u8>(), len),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(move |(mut m, buy, printable)| {
            m[0] = kind;
            match kind {
                b'A' | b'F' => m[SIDE_AT] = if buy { b'B' } else { b'S' },
                b'C' => m[PRINTABLE_AT] = if printable { b'Y' } else { b'N' },
                _ => {}
            }
            m
        })
}

fn valid_message() -> impl Strategy<Value = Vec<u8>> {
    select(SPEC.map(|(k, _)| k).to_vec()).prop_flat_map(valid_message_of)
}

/// Message bodies that reach every outcome: plain random bytes, a known type byte followed by
/// random bytes, valid messages with their length changed by up to two bytes and one byte
/// overwritten, and add and execution messages with a random side or printable byte.
fn message_bytes() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::collection::vec(any::<u8>(), 0..64),
        (
            select(vec![b'A', b'F', b'C']).prop_flat_map(valid_message_of),
            any::<u8>()
        )
            .prop_map(|(mut m, value)| {
                let at = if m[0] == b'C' { PRINTABLE_AT } else { SIDE_AT };
                m[at] = value;
                m
            }),
        (
            select(SPEC.map(|(k, _)| k).to_vec()),
            prop::collection::vec(any::<u8>(), 0..56)
        )
            .prop_map(|(kind, rest)| [&[kind][..], &rest].concat()),
        (valid_message(), -2i32..=2, any::<Index>(), any::<u8>()).prop_map(
            |(mut m, delta, at, value)| {
                let len = (m.len() as i32 + delta) as usize;
                m.resize(len, 0);
                let at = at.index(len);
                m[at] = value;
                m
            }
        ),
    ]
}

/// One piece of a stream: a valid frame, a frame around random bytes, a zero-length frame, or a
/// few raw bytes that break the framing.
fn piece() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        6 => valid_message().prop_map(|m| frame(&m)),
        2 => message_bytes().prop_map(|m| frame(&m)),
        1 => Just(vec![0, 0]),
        1 => prop::collection::vec(any::<u8>(), 0..4),
    ]
}

/// Streams of pieces, sometimes cut at a random point, and plain random bytes.
fn stream() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        3 => (prop::collection::vec(piece(), 0..12), any::<Index>(), any::<bool>()).prop_map(
            |(pieces, cut, do_cut)| {
                let mut s = pieces.concat();
                if do_cut {
                    s.truncate(cut.index(s.len() + 1));
                }
                s
            }
        ),
        1 => prop::collection::vec(any::<u8>(), 0..256),
    ]
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn decode_agrees_with_the_spec_on_any_bytes(m in message_bytes()) {
        let _ = check(&m);
    }

    #[test]
    fn frames_agree_with_the_spec_on_any_bytes(buf in stream()) {
        check_frames(&buf);
    }
}

#[test]
fn length_table_matches_the_spec() {
    for kind in 0..=255u8 {
        assert_eq!(message_length(kind), spec_length(kind), "type 0x{kind:02x}");
    }
}

/// A few generated valid messages of every type, from a fixed seed.
fn templates() -> Vec<Vec<u8>> {
    let mut runner = TestRunner::deterministic();
    SPEC.iter()
        .flat_map(|&(kind, _)| {
            let strategy = valid_message_of(kind);
            (0..3)
                .map(|_| strategy.new_tree(&mut runner).unwrap().current())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn every_truncation_of_a_valid_message_is_a_length_error() {
    for m in templates() {
        let kind = m[0];
        assert!(check(&m).is_ok());
        for cut in 0..m.len() {
            let want = if cut == 0 {
                ItchError::Empty
            } else {
                ItchError::BadLength {
                    kind,
                    expected: m.len(),
                    actual: cut,
                }
            };
            assert_eq!(decode(&m[..cut]), Err(want), "{m:02x?} cut at {cut}");
            let _ = check(&m[..cut]);
            // The same truncated body behind a length prefix that matches it.
            check_frames(&frame(&m[..cut]));
        }
        // The full frame cut at every point: whatever is left of it is `Truncated`.
        let f = frame(&m);
        for cut in 0..f.len() {
            let mut frames = Frames::new(&f[..cut]);
            let want = match cut {
                0 => None,
                1 => Some(Err(ItchError::Truncated {
                    needed: 2,
                    available: 1,
                })),
                _ => Some(Err(ItchError::Truncated {
                    needed: f.len(),
                    available: cut,
                })),
            };
            assert_eq!(frames.next(), want, "{f:02x?} cut at {cut}");
            assert_eq!(frames.offset(), 0);
            check_frames(&f[..cut]);
        }
    }
}

#[test]
fn every_single_byte_change_of_a_valid_message_is_decoded_consistently() {
    let mut type_changes = [0u32; 4]; // unknown type, bad length, same length decoded, bad field
    for mut m in templates() {
        let kind = m[0];
        for at in 0..m.len() {
            let original = m[at];
            for value in (0..=255u8).filter(|&v| v != original) {
                m[at] = value;
                let got = check(&m);
                if at == 0 {
                    match got {
                        Err(ItchError::UnknownType { .. }) => type_changes[0] += 1,
                        Err(ItchError::BadLength { .. }) => type_changes[1] += 1,
                        Ok(_) => type_changes[2] += 1,
                        Err(ItchError::BadField { .. }) => type_changes[3] += 1,
                        Err(e) => panic!("type byte 0x{value:02x}: {e:?}"),
                    }
                } else if got.is_err() {
                    // Past the type byte only the side or printable byte can make a valid
                    // message fail, and `check` has already matched the exact error.
                    assert!(
                        matches!((kind, at), (b'A' | b'F', SIDE_AT) | (b'C', PRINTABLE_AT)),
                        "{kind:?} byte {at} = 0x{value:02x}: {got:?}"
                    );
                }
            }
            m[at] = original;
        }
    }
    // A changed type byte reaches each outcome: unknown, wrong length, and a different type of
    // the same length that decodes or fails on its own field.
    assert!(type_changes.iter().all(|&n| n > 0), "{type_changes:?}");
}

#[test]
fn errors_follow_the_documented_precedence() {
    // Empty before anything else.
    assert_eq!(decode(&[]), Err(ItchError::Empty));
    // An unknown type is reported whatever the length, including a lone type byte.
    for kind in (0..=255u8).filter(|&k| spec_length(k).is_none()) {
        for len in [1, 12, 19, 36, 50, 51] {
            let mut m = vec![b'Z'; len];
            m[0] = kind;
            assert_eq!(decode(&m), Err(ItchError::UnknownType { kind }));
        }
    }
    // A known type at the wrong length is a length error even when a checked field is invalid.
    for (kind, at) in [(b'A', SIDE_AT), (b'F', SIDE_AT), (b'C', PRINTABLE_AT)] {
        let expected = spec_length(kind).unwrap();
        for len in [1, at, at + 1, expected - 1, expected + 1] {
            let mut m = vec![b'?'; len];
            m[0] = kind;
            assert_eq!(
                decode(&m),
                Err(ItchError::BadLength {
                    kind,
                    expected,
                    actual: len
                }),
                "type {:?} length {len}",
                kind as char
            );
        }
        // At the right length the same bytes are a field error.
        let mut m = vec![b'?'; expected];
        m[0] = kind;
        assert!(matches!(decode(&m), Err(ItchError::BadField { .. })));
    }
}

#[test]
fn frames_end_on_bad_prefixes_without_looping() {
    let good = frame(&[b'S', 0, 1, 0, 2, 0, 0, 0, 0, 0, 3, b'O']);
    let ok = |buf: &[u8]| Frames::new(buf).next().unwrap().unwrap();
    let first = ok(&good);

    // A length prefix larger than the bytes left, alone and after a whole frame.
    for prefix in [[0u8, 12], [0, 13], [0xff, 0xff]] {
        let mut tail = prefix.to_vec();
        tail.extend_from_slice(&good[2..9]);
        let needed = 2 + usize::from(u16::from_be_bytes(prefix));
        let mut frames = Frames::new(&tail);
        assert_eq!(
            frames.next(),
            Some(Err(ItchError::Truncated {
                needed,
                available: 9
            }))
        );
        assert_eq!(frames.next(), None);
        assert_eq!(frames.offset(), 0);

        let stream = [&good[..], &tail].concat();
        let mut frames = Frames::new(&stream);
        assert_eq!(frames.next(), Some(Ok(first)));
        assert!(matches!(
            frames.next(),
            Some(Err(ItchError::Truncated { .. }))
        ));
        assert_eq!(frames.next(), None);
        assert_eq!(frames.offset(), good.len());
        check_frames(&stream);
    }

    // A zero length is `Empty` and ends the stream; it is not skipped.
    let stream = [&good[..], &[0, 0], &good].concat();
    let items: Vec<_> = Frames::new(&stream).take(10).collect();
    assert_eq!(items, [Ok(first), Err(ItchError::Empty)]);
    check_frames(&stream);

    // A stream cut inside a length prefix.
    let stream = [&good[..], &[0]].concat();
    let mut frames = Frames::new(&stream);
    assert_eq!(frames.next(), Some(Ok(first)));
    assert_eq!(
        frames.next(),
        Some(Err(ItchError::Truncated {
            needed: 2,
            available: 1
        }))
    );
    assert_eq!(frames.next(), None);
    assert_eq!(frames.offset(), good.len());

    // An empty buffer is the end of the stream, not an error.
    assert_eq!(Frames::new(&[]).next(), None);
}

/// Proves the generators reach every outcome the properties are meant to cover.
#[test]
fn generators_hit_every_outcome() {
    let mut runner = TestRunner::deterministic();
    let mut decoded = [0u32; 5]; // ok, empty, unknown type, bad length, bad field
    let strategy = message_bytes();
    for _ in 0..4_000 {
        let m = strategy.new_tree(&mut runner).unwrap().current();
        decoded[match decode(&m) {
            Ok(_) => 0,
            Err(ItchError::Empty) => 1,
            Err(ItchError::UnknownType { .. }) => 2,
            Err(ItchError::BadLength { .. }) => 3,
            Err(ItchError::BadField { .. }) => 4,
            Err(e) => panic!("{e:?}"),
        }] += 1;
    }

    // How each stream ends: clean end, truncated, empty frame, unknown type, bad length, bad
    // field.
    let mut ends = [0u32; 6];
    let strategy = stream();
    for _ in 0..4_000 {
        let buf = strategy.new_tree(&mut runner).unwrap().current();
        let end = Frames::new(&buf).find_map(Result::err);
        ends[match end {
            None => 0,
            Some(ItchError::Truncated { .. }) => 1,
            Some(ItchError::Empty) => 2,
            Some(ItchError::UnknownType { .. }) => 3,
            Some(ItchError::BadLength { .. }) => 4,
            Some(ItchError::BadField { .. }) => 5,
        }] += 1;
    }
    println!("decode outcomes {decoded:?}, stream ends {ends:?}");
    assert!(decoded.iter().all(|&n| n >= 1), "decode: {decoded:?}");
    assert!(ends.iter().all(|&n| n >= 1), "streams: {ends:?}");
}
