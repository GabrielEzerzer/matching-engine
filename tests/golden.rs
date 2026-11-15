//! Pins the golden replay hash encoding with digests computed independently from the byte layout in README.md.

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
        let mut want = vec![body[0], 0xCD, 0xAB];
        want.extend_from_slice(&body[1..]);
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
        "3add35e0dbe18fe685957a34a23250fd17bfdd88fee7e5daf1517e5f5acbddab"
    );
}

fn small_replay_digest(period: u64) -> String {
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
        add(2, false, 40),
        Message::SystemEvent {
            header: h(0),
            event: b'Q',
        },
    ];
    let mut replay = Replay::new();
    let mut hasher = ReplayHasher::with_period(period);
    for msg in &messages {
        let applied = replay.apply(msg).unwrap();
        hasher.events(applied.locate, applied.events);
        hasher.after_message(msg, &replay);
    }
    hex(&hasher.finish(&replay))
}

#[test]
fn small_replay_digest_is_pinned() {
    assert_eq!(
        small_replay_digest(golden::PERIOD),
        "7c8f8d0426a642d12848ca057385a5b7066f418a30ffb58888a8ddd418fb7f65"
    );
}

#[test]
fn periodic_checkpoints_are_pinned() {
    assert_eq!(
        small_replay_digest(2),
        "0a9b77eabda6461092eab67d36902c6bcfda52f7d5a99b509b53e6893653a6d6"
    );
    assert_eq!(
        small_replay_digest(1),
        "49e2089db6dbf6ec43147808dc0c7b266e21c55df32b8be121236b1e92665404"
    );
}

#[test]
fn the_checkpoint_period_is_two_to_the_twenty() {
    assert_eq!(golden::PERIOD, 1 << 20);
}
