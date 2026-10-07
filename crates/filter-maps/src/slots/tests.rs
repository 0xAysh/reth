//! Whole-stream observations from the pinned Geth iterator.
//!
//! The fork-only generator and regeneration instructions are linked in each fixture. `V` ranges
//! only compress consecutive identical values; every index is expanded and compared. `M` lines
//! record where Geth finished a map; the renderer owns that, and the pipeline fixtures pin it.

use super::*;
use alloy_primitives::{Address, Bytes};

const GETH_HEADER: &str = "# Geth af7c0fd8ee09de71b1034dbe6d1112556b49b59f";
const GENERATOR_HEADER: &str = "# Generator https://github.com/0xAysh/reth/blob/84f857a707326ffcbc4fb71d8a53104ed144b125/tools/filtermaps-oracles/stream/gen_stream_test.go";

/// One observable step of the value space, as both Geth and the assigner report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Observation {
    Pointer { block: u64, index: u64 },
    Value { index: u64, value: B256 },
    Delimiter { index: u64, block: u64 },
    Padding { index: u64 },
}

struct Fixture {
    start: u64,
    blocks: Vec<(u64, Vec<Log>)>,
    expected: Vec<Observation>,
    head: (u64, u64, u64),
}

fn number(s: &str) -> u64 {
    s.parse().expect("fixture number")
}

fn hash(s: &str) -> B256 {
    s.parse().expect("fixture hash")
}

fn parse(text: &str) -> Fixture {
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some(GETH_HEADER), "fixture Geth provenance");
    assert_eq!(lines.next(), Some(GENERATOR_HEADER), "fixture generator provenance");
    assert_eq!(lines.next(), Some("FORMAT 1"), "fixture format");

    let mut fixture =
        Fixture { start: 0, blocks: Vec::new(), expected: Vec::new(), head: (0, 0, 0) };
    let mut events = false;
    let mut head = None;
    for line in lines.filter(|line| !line.is_empty()) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields == ["EVENTS"] {
            assert!(!events);
            events = true;
            continue
        }
        if !events {
            match fields.as_slice() {
                ["START", "ANCHOR", index] => fixture.start = number(index),
                ["BLOCK", n, _] => fixture.blocks.push((number(n), Vec::new())),
                ["LOG", count, address, topics @ ..] => {
                    let address = address.parse::<Address>().expect("fixture address");
                    let log = Log::new_unchecked(
                        address,
                        topics.iter().map(|topic| hash(topic)).collect(),
                        Bytes::new(),
                    );
                    let (_, logs) = fixture.blocks.last_mut().expect("log belongs to a block");
                    logs.extend(std::iter::repeat_n(log, usize::try_from(number(count)).unwrap()));
                }
                // Every kept fixture uses mainnet parameters and ends at the head. Receipt
                // boundaries introduce no value-space slots.
                ["PARAMS", "DEFAULT"] | ["END", "HEAD"] | ["RECEIPT"] => {}
                _ => panic!("unknown fixture input: {line}"),
            }
            continue
        }
        let observation = match fields.as_slice() {
            ["P", n, _, i] => Observation::Pointer { block: number(n), index: number(i) },
            ["V", first, last, h, _kind] => {
                assert!(number(first) <= number(last));
                for index in number(first)..=number(last) {
                    fixture.expected.push(Observation::Value { index, value: hash(h) });
                }
                continue
            }
            ["D", i, n, _] => Observation::Delimiter { index: number(i), block: number(n) },
            ["X", i] => Observation::Padding { index: number(i) },
            ["M", ..] => continue,
            ["H", n, _, pointer, pending] => {
                head = Some((number(n), number(pointer), number(pending)));
                continue
            }
            _ => panic!("unknown fixture event: {line}"),
        };
        fixture.expected.push(observation);
    }
    assert!(events && !fixture.blocks.is_empty() && !fixture.expected.is_empty());
    fixture.head = head.expect("every kept fixture ends at the head");
    fixture
}

fn check(text: &str) {
    let fixture = parse(text);
    let mut assigner = SlotAssigner::new(fixture.start);
    let mut actual = Vec::new();
    let mut previous = None;
    for (number, logs) in &fixture.blocks {
        assigner.push_block(*number, logs, |event| {
            actual.push(match event {
                Event::BlockStart(start) => {
                    Observation::Pointer { block: start.block, index: start.pointer }
                }
                Event::Value { index, value } => Observation::Value { index, value },
                Event::Delimiter { index } => Observation::Delimiter {
                    index,
                    block: previous.expect("a delimiter closes a block"),
                },
                Event::Padding { index } => Observation::Padding { index },
            })
        });
        previous = Some(*number);
    }
    assert_eq!(actual, fixture.expected);

    let (head, head_pointer, pending) = fixture.head;
    let last_pointer = actual.iter().rev().find_map(|observation| match observation {
        Observation::Pointer { block, index } => Some((*block, *index)),
        _ => None,
    });
    assert_eq!(last_pointer, Some((head, head_pointer)), "head pointer");
    assert_eq!(assigner.next_index(), pending, "pending head delimiter");
}

macro_rules! fixture_tests {
    ($($name:ident => $file:literal),+ $(,)?) => { $(
        #[test]
        fn $name() {
            check(include_str!(concat!("../../tests/it/golden_stream/fixtures/", $file, ".txt")));
        }
    )+ };
}

fixture_tests! {
    genesis => "genesis",
    empty_blocks_receipts => "empty-blocks-receipts",
    nonzero_topics => "nonzero-topics",
    exact_fit => "exact-fit",
    first_log_padding => "first-log-padding",
    later_log_padding => "later-log-padding",
    multi_slot_padding => "multi-slot-padding",
    multiple_maps => "multiple-maps",
    delimiter_empty_successor => "delimiter-empty-successor",
    delimiter_full_successor => "delimiter-full-successor",
    pending_head_boundary => "pending-head-boundary",
    absolute_map => "absolute-map",
}
