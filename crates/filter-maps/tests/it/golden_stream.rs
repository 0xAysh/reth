//! Whole-stream observations from the actual pinned Geth iterator.
//!
//! The public fork-only generator and regeneration instructions are linked in each fixture.
//! `V` ranges only compress consecutive identical values; every index is expanded and compared.
//! `P`, `M`, `H`, and `B` normalize Geth's observed iterator state into Rust events; Geth does not
//! natively emit the Rust event protocol.

use alloy_eips::BlockNumHash;
use alloy_primitives::B256;
use reth_filter_maps::{
    BatchContinuation, BlockInput, BlockPointer, LogInput, LogValueKind, LogValueSlot,
    LogValueStream, LogValueStreamCompletion, LogValueStreamEvent, LogValueStreamItem,
    LogValueStreamTermination, MapBoundary, Params, PendingDelimiter, ValueSpaceAnchor,
    DEFAULT_PARAMS, RANGE_TEST_PARAMS,
};

const GETH_HEADER: &str = "# Geth af7c0fd8ee09de71b1034dbe6d1112556b49b59f";
const GENERATOR_HEADER: &str = "# Generator https://github.com/0xAysh/reth/blob/84f857a707326ffcbc4fb71d8a53104ed144b125/tools/filtermaps-oracles/stream/gen_stream_test.go";

struct Fixture {
    params: Params,
    continuation: bool,
    start: u64,
    termination: LogValueStreamTermination,
    blocks: Vec<BlockInput>,
    expected: Vec<LogValueStreamItem>,
}

fn number(s: &str) -> u64 {
    s.parse().expect("fixture number")
}

fn hash(s: &str) -> B256 {
    s.parse().expect("fixture hash")
}

const fn event(event: LogValueStreamEvent) -> LogValueStreamItem {
    LogValueStreamItem::Event(event)
}

fn parse(text: &str) -> Fixture {
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some(GETH_HEADER), "fixture Geth provenance");
    assert_eq!(lines.next(), Some(GENERATOR_HEADER), "fixture generator provenance");
    assert_eq!(lines.next(), Some("FORMAT 1"), "fixture format");

    let mut fixture = Fixture {
        params: DEFAULT_PARAMS,
        continuation: false,
        start: 0,
        termination: LogValueStreamTermination::ReachedHead,
        blocks: Vec::new(),
        expected: Vec::new(),
    };
    let mut events = false;
    for line in lines.filter(|line| !line.is_empty()) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields == ["EVENTS"] {
            assert!(!events);
            events = true;
            continue
        }
        if !events {
            match fields.as_slice() {
                ["PARAMS", "DEFAULT"] => fixture.params = DEFAULT_PARAMS,
                ["PARAMS", "RANGE"] => fixture.params = RANGE_TEST_PARAMS,
                ["START", mode @ ("ANCHOR" | "CONTINUATION"), index] => {
                    fixture.continuation = *mode == "CONTINUATION";
                    fixture.start = number(index);
                }
                ["END", "HEAD"] => {
                    fixture.termination = LogValueStreamTermination::ReachedHead;
                }
                ["END", "BATCH", n, h] => {
                    fixture.termination = LogValueStreamTermination::BatchExhausted {
                        next_block: BlockNumHash::new(number(n), hash(h)),
                    };
                }
                ["BLOCK", n, h] => {
                    fixture.blocks.push(BlockInput::new(number(n), hash(h), []));
                }
                ["LOG", count, address, topics @ ..] => {
                    let log = LogInput::new(
                        address.parse().expect("fixture address"),
                        topics.iter().map(|topic| hash(topic)),
                    );
                    fixture
                        .blocks
                        .last_mut()
                        .expect("log belongs to a block")
                        .logs
                        .extend(std::iter::repeat_n(log, usize::try_from(number(count)).unwrap()));
                }
                // Geth needs successor receipts to advance past the batch delimiter. The pure
                // Rust batch interface only receives its identity; these are oracle-only inputs.
                // Receipt markers in the Rust input likewise introduce no value-space slots.
                ["RECEIPT" | "NEXT_RECEIPT"] | ["NEXT_BLOCK", _, _] | ["NEXT_LOG", _, _, ..] => {}
                _ => panic!("unknown fixture input: {line}"),
            }
            continue
        }
        let item = match fields.as_slice() {
            ["P", n, h, i] => event(LogValueStreamEvent::BlockPointer(BlockPointer::new(
                number(n),
                hash(h),
                number(i),
            ))),
            ["V", first, last, h, kind] => {
                let kind = if *kind == "A" {
                    LogValueKind::Address
                } else {
                    LogValueKind::Topic {
                        ordinal: kind.strip_prefix('T').expect("topic kind").parse().unwrap(),
                    }
                };
                assert!(number(first) <= number(last));
                for index in number(first)..=number(last) {
                    fixture.expected.push(event(LogValueStreamEvent::Slot(LogValueSlot::Value {
                        index,
                        value: hash(h),
                        kind,
                    })));
                }
                continue
            }
            ["D", i, n, h] => event(LogValueStreamEvent::Slot(LogValueSlot::BlockDelimiter {
                index: number(i),
                block_number: number(n),
                block_hash: hash(h),
            })),
            ["X", i] => {
                event(LogValueStreamEvent::Slot(LogValueSlot::Padding { index: number(i) }))
            }
            ["M", m, n, h] => event(LogValueStreamEvent::MapBoundary(MapBoundary::new(
                u32::try_from(number(m)).unwrap(),
                number(n),
                hash(h),
            ))),
            ["H", n, h, pointer, pending] => {
                LogValueStreamItem::Complete(LogValueStreamCompletion::ReachedHead {
                    head: BlockPointer::new(number(n), hash(h), number(pointer)),
                    pending_delimiter: PendingDelimiter::new(number(n), hash(h), number(pending)),
                })
            }
            ["B", n, h, pointer, next_n, next_h, cursor] => {
                LogValueStreamItem::Complete(LogValueStreamCompletion::BatchExhausted {
                    last_block: BlockPointer::new(number(n), hash(h), number(pointer)),
                    continuation: BatchContinuation::new(
                        BlockNumHash::new(number(next_n), hash(next_h)),
                        number(cursor),
                    ),
                })
            }
            _ => panic!("unknown fixture event: {line}"),
        };
        fixture.expected.push(item);
    }
    assert!(events && !fixture.blocks.is_empty() && !fixture.expected.is_empty());
    fixture
}

fn check(text: &str) {
    let fixture = parse(text);
    let first = &fixture.blocks[0];
    let mut stream = if fixture.continuation {
        LogValueStream::continue_from(
            fixture.params,
            BatchContinuation::new(BlockNumHash::new(first.number, first.hash), fixture.start),
            fixture.blocks,
            fixture.termination,
        )
    } else {
        LogValueStream::new(
            fixture.params,
            ValueSpaceAnchor::new(first.number, first.hash, fixture.start),
            fixture.blocks,
            fixture.termination,
        )
    };
    for (ordinal, expected) in fixture.expected.into_iter().enumerate() {
        assert_eq!(stream.next(), Some(Ok(expected)), "event {ordinal}");
    }
    assert_eq!(stream.next(), None);
    assert_eq!(stream.next(), None);
}

macro_rules! fixture_tests {
    ($($name:ident => $file:literal),+ $(,)?) => { $(
        #[test]
        fn $name() { check(include_str!(concat!("golden_stream/fixtures/", $file, ".txt"))); }
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
    batch_delimiter_empty => "batch-delimiter-empty",
    batch_delimiter_full => "batch-delimiter-full",
    batch_before_padding => "batch-before-padding",
    continuation_padding => "continuation-padding",
    continuation_empty => "continuation-empty",
    range_maps => "range-maps",
}

#[test]
fn actual_batch_completion_drives_continuation_fixture() {
    let first = parse(include_str!("golden_stream/fixtures/batch-before-padding.txt"));
    let block = &first.blocks[0];
    let completion = LogValueStream::new(
        first.params,
        ValueSpaceAnchor::new(block.number, block.hash, first.start),
        first.blocks,
        first.termination,
    )
    .last()
    .unwrap()
    .unwrap();
    let LogValueStreamItem::Complete(LogValueStreamCompletion::BatchExhausted {
        continuation, ..
    }) = completion
    else {
        panic!("expected batch completion")
    };
    let next = parse(include_str!("golden_stream/fixtures/continuation-padding.txt"));
    let actual =
        LogValueStream::continue_from(next.params, continuation, next.blocks, next.termination)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
    assert_eq!(actual, next.expected);
}
