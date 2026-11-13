//! Pins the golden replay hash encoding (`harness/golden.rs`). The expected digests were computed
//! independently from the documented byte layout, so a change to the encoding, to the event
//! order or to the hash algorithm fails here before it silently changes the recorded golden value.

#[path = "../harness/golden.rs"]
mod golden;

use golden::{ReplayHasher, encode_event, hex};
use matching_engine::itch::{Header, Message};
use matching_engine::replay::Replay;
use matching_engine::{Event, RejectReason};

#[test]
fn every_event_has_its_documented_bytes() {
    let cases: [(Event, Vec<u8>); 7] = [
        (
            Event::Accepted { id: 0x0102 },
            vec![1, 0x02, 0x01, 0, 0, 0, 0, 0, 0],
        ),
        (
            Event::Trade {
                taker_id: 1,
                maker_id: 2,
                price: 3,
                qty: 4,
            },
            [
                vec![2],
                1u64.to_le_bytes().to_vec(),
                2u64.to_le_bytes().to_vec(),
                3u32.to_le_bytes().to_vec(),
                4u32.to_le_bytes().to_vec(),
            ]
            .concat(),
        ),
        (
            Event::Filled { id: 5 },
            [vec![3], 5u64.to_le_bytes().to_vec()].concat(),
        ),
        (
            Event::Cancelled {
                id: 6,
                remaining: 7,
            },
            [
                vec![4],
                6u64.to_le_bytes().to_vec(),
                7u32.to_le_bytes().to_vec(),
            ]
            .concat(),
        ),
        (
            Event::Reduced {
                id: 8,
                by: 9,
                remaining: 10,
            },
            [
                vec![5],
                8u64.to_le_bytes().to_vec(),
                9u32.to_le_bytes().to_vec(),
                10u32.to_le_bytes().to_vec(),
            ]
            .concat(),
        ),
        (
            Event::Replaced {
                old_id: 11,
                new_id: 12,
            },
            [
                vec![6],
                11u64.to_le_bytes().to_vec(),
                12u64.to_le_bytes().to_vec(),
            ]
            .concat(),
        ),
        (
            Event::Rejected {
                id: 13,
                reason: RejectReason::ReduceTooLarge,
            },
            [vec![7], 13u64.to_le_bytes().to_vec(), vec![4]].concat(),
        ),
    ];
    for (event, body) in cases {
        let mut out = Vec::new();
        encode_event(&mut out, 0xABCD, &event);
        let mut want = vec![0xCD, 0xAB];
        want.extend_from_slice(&body);
        assert_eq!(out, want, "{event:?}");
    }
    for (reason, byte) in [
        (RejectReason::ZeroQuantity, 1),
        (RejectReason::DuplicateOrderId, 2),
        (RejectReason::UnknownOrder, 3),
        (RejectReason::ReduceTooLarge, 4),
    ] {
        let mut out = Vec::new();
        encode_event(&mut out, 0, &Event::Rejected { id: 0, reason });
        assert_eq!(out.last(), Some(&byte));
    }
}

#[test]
fn empty_replay_digest_is_pinned() {
    let replay = Replay::new();
    let digest = ReplayHasher::new().finish(&replay);
    assert_eq!(
        hex(&digest),
        "6415c4da8f9145c5a15161f9d7fb0b97ee3b8568e4e471ddf3c62e190e7b702b"
    );
}

#[test]
fn small_replay_digest_is_pinned() {
    let h = |locate| Header {
        locate,
        tracking: 0,
        timestamp: 0,
    };
    let add = |reference, buy, shares| Message::AddOrder {
        header: h(7),
        reference,
        buy,
        shares,
        stock: *b"TEST    ",
        price: 500,
        attribution: None,
    };
    let messages = [
        add(1, true, 100),
        // Crosses and trades 40 against order 1 (an unexpected trade, which is fine here).
        add(2, false, 40),
        Message::SystemEvent {
            header: h(0),
            event: b'Q',
        },
    ];
    let mut replay = Replay::new();
    let mut hasher = ReplayHasher::new();
    for msg in &messages {
        let applied = replay.apply(msg).unwrap();
        hasher.events(applied.locate, applied.events);
        if let Message::SystemEvent { event, .. } = *msg {
            hasher.checkpoint(event, &replay);
        }
    }
    assert_eq!(
        hex(&hasher.finish(&replay)),
        "c0f830a610f46a0a1c61802f5704477ae0f2323ad2f46ad265ab3da2c8c6cb78"
    );
}
