//! Filter-map boundary padding tests.

use alloy_primitives::{Address, B256};
use reth_filter_maps::{
    address_value, topic_value, BatchContinuation, BlockInput, BlockPointer, LogInput,
    LogValueKind, LogValueSlot, LogValueStream, LogValueStreamCompletion, LogValueStreamError,
    LogValueStreamEvent, LogValueStreamItem, LogValueStreamTermination, PendingDelimiter,
    ValueSpaceAnchor, DEFAULT_PARAMS, RANGE_TEST_PARAMS,
};

const fn value(index: u64, hash: B256, kind: LogValueKind) -> LogValueStreamItem {
    LogValueStreamItem::Event(LogValueStreamEvent::Slot(LogValueSlot::Value {
        index,
        value: hash,
        kind,
    }))
}

#[test]
fn a_log_that_exactly_fits_the_map_remainder_is_not_padded() {
    let block_hash = B256::repeat_byte(0x11);
    let address = Address::repeat_byte(0xaa);
    let topic = B256::repeat_byte(0x01);
    let block = BlockInput::new(0, block_hash, [LogInput::new(address, [topic])]);
    let anchor = ValueSpaceAnchor::new(0, block_hash, 65_534);

    let actual = LogValueStream::new(
        DEFAULT_PARAMS,
        anchor,
        [block],
        LogValueStreamTermination::BatchExhausted,
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();

    assert_eq!(
        actual,
        vec![
            LogValueStreamItem::Event(LogValueStreamEvent::BlockPointer(BlockPointer::new(
                0, block_hash, 65_534,
            ))),
            value(65_534, address_value(address), LogValueKind::Address),
            value(65_535, topic_value(topic), LogValueKind::Topic { ordinal: 0 }),
            LogValueStreamItem::Event(LogValueStreamEvent::Slot(LogValueSlot::BlockDelimiter {
                index: 65_536,
                block_number: 0,
                block_hash,
            },)),
            LogValueStreamItem::Complete(LogValueStreamCompletion::BatchExhausted {
                last_block: BlockPointer::new(0, block_hash, 65_534),
                continuation: BatchContinuation::new(1, 65_537),
            }),
        ]
    );
}

#[test]
fn a_log_one_slot_wider_than_the_remainder_moves_intact_to_the_next_map() {
    let block_hash = B256::repeat_byte(0x11);
    let first_address = Address::repeat_byte(0xaa);
    let second_address = Address::repeat_byte(0xbb);
    let topics = [B256::repeat_byte(0x01), B256::repeat_byte(0x02), B256::repeat_byte(0x03)];
    let block = BlockInput::new(
        0,
        block_hash,
        [LogInput::new(first_address, []), LogInput::new(second_address, topics)],
    );
    let anchor = ValueSpaceAnchor::new(0, block_hash, 65_532);

    let actual = LogValueStream::new(
        DEFAULT_PARAMS,
        anchor,
        [block],
        LogValueStreamTermination::ReachedHead,
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();

    assert_eq!(
        actual,
        vec![
            LogValueStreamItem::Event(LogValueStreamEvent::BlockPointer(BlockPointer::new(
                0, block_hash, 65_532,
            ))),
            value(65_532, address_value(first_address), LogValueKind::Address),
            LogValueStreamItem::Event(LogValueStreamEvent::Slot(LogValueSlot::Padding {
                index: 65_533,
            })),
            LogValueStreamItem::Event(LogValueStreamEvent::Slot(LogValueSlot::Padding {
                index: 65_534,
            })),
            LogValueStreamItem::Event(LogValueStreamEvent::Slot(LogValueSlot::Padding {
                index: 65_535,
            })),
            value(65_536, address_value(second_address), LogValueKind::Address),
            value(65_537, topic_value(topics[0]), LogValueKind::Topic { ordinal: 0 }),
            value(65_538, topic_value(topics[1]), LogValueKind::Topic { ordinal: 1 }),
            value(65_539, topic_value(topics[2]), LogValueKind::Topic { ordinal: 2 }),
            LogValueStreamItem::Complete(LogValueStreamCompletion::ReachedHead {
                head: BlockPointer::new(0, block_hash, 65_532),
                pending_delimiter: PendingDelimiter::new(0, block_hash, 65_540),
            }),
        ]
    );
}

#[test]
fn padding_before_a_blocks_first_log_places_its_pointer_after_the_padding() {
    let first_hash = B256::repeat_byte(0x11);
    let second_hash = B256::repeat_byte(0x22);
    let first_address = Address::repeat_byte(0xaa);
    let second_address = Address::repeat_byte(0xbb);
    let topic = B256::repeat_byte(0x01);
    let blocks = [
        BlockInput::new(0, first_hash, [LogInput::new(first_address, [])]),
        BlockInput::new(1, second_hash, [LogInput::new(second_address, [topic])]),
    ];
    let anchor = ValueSpaceAnchor::new(0, first_hash, 65_533);

    let actual =
        LogValueStream::new(DEFAULT_PARAMS, anchor, blocks, LogValueStreamTermination::ReachedHead)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

    assert_eq!(
        actual,
        vec![
            LogValueStreamItem::Event(LogValueStreamEvent::BlockPointer(BlockPointer::new(
                0, first_hash, 65_533,
            ))),
            value(65_533, address_value(first_address), LogValueKind::Address),
            LogValueStreamItem::Event(LogValueStreamEvent::Slot(LogValueSlot::BlockDelimiter {
                index: 65_534,
                block_number: 0,
                block_hash: first_hash,
            },)),
            LogValueStreamItem::Event(LogValueStreamEvent::Slot(LogValueSlot::Padding {
                index: 65_535,
            })),
            LogValueStreamItem::Event(LogValueStreamEvent::BlockPointer(BlockPointer::new(
                1,
                second_hash,
                65_536,
            ))),
            value(65_536, address_value(second_address), LogValueKind::Address),
            value(65_537, topic_value(topic), LogValueKind::Topic { ordinal: 0 }),
            LogValueStreamItem::Complete(LogValueStreamCompletion::ReachedHead {
                head: BlockPointer::new(1, second_hash, 65_536),
                pending_delimiter: PendingDelimiter::new(1, second_hash, 65_538),
            }),
        ]
    );
}

#[test]
fn a_delimiter_occupies_the_last_map_slot_without_padding() {
    let first_hash = B256::repeat_byte(0x11);
    let second_hash = B256::repeat_byte(0x22);
    let address = Address::repeat_byte(0xaa);
    let blocks = [
        BlockInput::new(0, first_hash, [LogInput::new(address, [])]),
        BlockInput::new(1, second_hash, []),
    ];
    let anchor = ValueSpaceAnchor::new(0, first_hash, 65_534);

    let actual =
        LogValueStream::new(DEFAULT_PARAMS, anchor, blocks, LogValueStreamTermination::ReachedHead)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

    assert_eq!(
        actual,
        vec![
            LogValueStreamItem::Event(LogValueStreamEvent::BlockPointer(BlockPointer::new(
                0, first_hash, 65_534,
            ))),
            value(65_534, address_value(address), LogValueKind::Address),
            LogValueStreamItem::Event(LogValueStreamEvent::Slot(LogValueSlot::BlockDelimiter {
                index: 65_535,
                block_number: 0,
                block_hash: first_hash,
            },)),
            LogValueStreamItem::Event(LogValueStreamEvent::BlockPointer(BlockPointer::new(
                1,
                second_hash,
                65_536,
            ))),
            LogValueStreamItem::Complete(LogValueStreamCompletion::ReachedHead {
                head: BlockPointer::new(1, second_hash, 65_536),
                pending_delimiter: PendingDelimiter::new(1, second_hash, 65_536),
            }),
        ]
    );
}

#[test]
fn an_oversized_later_log_emits_no_events_from_its_block() {
    let block_hash = B256::repeat_byte(0x11);
    let block = BlockInput::new(
        0,
        block_hash,
        [
            LogInput::new(Address::repeat_byte(0xaa), []),
            LogInput::new(Address::repeat_byte(0xbb), [B256::repeat_byte(0x01)]),
        ],
    );
    let anchor = ValueSpaceAnchor::new(0, block_hash, 0);
    let mut stream = LogValueStream::new(
        RANGE_TEST_PARAMS,
        anchor,
        [block],
        LogValueStreamTermination::ReachedHead,
    );

    assert_eq!(
        stream.next(),
        Some(Err(LogValueStreamError::LogTooWide {
            block_number: 0,
            log_index: 1,
            log_width: 2,
            values_per_map: 1,
        }))
    );
    assert_eq!(stream.next(), None);
}

#[test]
fn an_oversized_later_block_emits_no_events_from_the_failing_block() {
    let first_hash = B256::repeat_byte(0x11);
    let second_hash = B256::repeat_byte(0x22);
    let first_address = Address::repeat_byte(0xaa);
    let blocks = [
        BlockInput::new(0, first_hash, [LogInput::new(first_address, [])]),
        BlockInput::new(
            1,
            second_hash,
            [LogInput::new(Address::repeat_byte(0xbb), [B256::repeat_byte(0x01)])],
        ),
    ];
    let anchor = ValueSpaceAnchor::new(0, first_hash, 0);
    let mut stream = LogValueStream::new(
        RANGE_TEST_PARAMS,
        anchor,
        blocks,
        LogValueStreamTermination::ReachedHead,
    );

    assert_eq!(
        stream.by_ref().collect::<Vec<_>>(),
        vec![
            Ok(LogValueStreamItem::Event(LogValueStreamEvent::BlockPointer(BlockPointer::new(
                0, first_hash, 0
            ),))),
            Ok(value(0, address_value(first_address), LogValueKind::Address)),
            Ok(LogValueStreamItem::Event(LogValueStreamEvent::Slot(
                LogValueSlot::BlockDelimiter { index: 1, block_number: 0, block_hash: first_hash },
            ))),
            Err(LogValueStreamError::LogTooWide {
                block_number: 1,
                log_index: 0,
                log_width: 2,
                values_per_map: 1,
            }),
        ]
    );
    assert_eq!(stream.next(), None);
}

#[test]
fn invalid_topic_count_takes_precedence_over_impossible_map_geometry() {
    let block_hash = B256::repeat_byte(0x11);
    let topics = (0..5).map(B256::repeat_byte);
    let block = BlockInput::new(0, block_hash, [LogInput::new(Address::repeat_byte(0xaa), topics)]);
    let anchor = ValueSpaceAnchor::new(0, block_hash, 0);
    let mut stream = LogValueStream::new(
        RANGE_TEST_PARAMS,
        anchor,
        [block],
        LogValueStreamTermination::ReachedHead,
    );

    assert_eq!(
        stream.next(),
        Some(Err(LogValueStreamError::InvalidTopicCount {
            block_number: 0,
            log_index: 0,
            actual: 5,
        }))
    );
    assert_eq!(stream.next(), None);
}

#[test]
fn a_log_wider_than_a_map_returns_a_typed_error_before_its_block() {
    let block_hash = B256::repeat_byte(0x11);
    let block = BlockInput::new(
        0,
        block_hash,
        [LogInput::new(Address::repeat_byte(0xaa), [B256::repeat_byte(0x01)])],
    );
    let anchor = ValueSpaceAnchor::new(0, block_hash, 0);
    let mut stream = LogValueStream::new(
        RANGE_TEST_PARAMS,
        anchor,
        [block],
        LogValueStreamTermination::ReachedHead,
    );

    assert_eq!(
        stream.next(),
        Some(Err(LogValueStreamError::LogTooWide {
            block_number: 0,
            log_index: 0,
            log_width: 2,
            values_per_map: 1,
        }))
    );
    assert_eq!(stream.next(), None);
}
