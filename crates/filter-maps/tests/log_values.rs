//! Searchable log-value stream tests.

use alloy_primitives::{Address, B256};
use reth_filter_maps::{
    address_value, topic_value, BatchContinuation, BlockInput, BlockPointer, LogInput,
    LogValueKind, LogValueSlot, LogValueStream, LogValueStreamCompletion, LogValueStreamError,
    LogValueStreamEvent, LogValueStreamItem, LogValueStreamTermination, PendingDelimiter,
    ValueSpaceAnchor, DEFAULT_PARAMS,
};

const fn value(index: u64, hash: B256, kind: LogValueKind) -> LogValueStreamItem {
    LogValueStreamItem::Event(LogValueStreamEvent::Slot(LogValueSlot::Value {
        index,
        value: hash,
        kind,
    }))
}

#[test]
fn a_log_without_topics_emits_one_typed_address_value() {
    let block_hash = B256::repeat_byte(0x11);
    let address = Address::repeat_byte(0xaa);
    let anchor = ValueSpaceAnchor::new(0, block_hash, 0);
    let block = BlockInput::new(0, block_hash, [LogInput::new(address, [])]);

    let items = LogValueStream::new(
        DEFAULT_PARAMS,
        anchor,
        [block],
        LogValueStreamTermination::ReachedHead,
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();

    assert_eq!(
        items,
        vec![
            LogValueStreamItem::Event(LogValueStreamEvent::BlockPointer(BlockPointer::new(
                0, block_hash, 0,
            ))),
            LogValueStreamItem::Event(LogValueStreamEvent::Slot(LogValueSlot::Value {
                index: 0,
                value: address_value(address),
                kind: LogValueKind::Address,
            })),
            LogValueStreamItem::Complete(LogValueStreamCompletion::ReachedHead {
                head: BlockPointer::new(0, block_hash, 0),
                pending_delimiter: PendingDelimiter::new(0, block_hash, 1),
            }),
        ]
    );
}

#[test]
fn a_log_with_more_than_four_topics_returns_a_typed_error_before_its_block() {
    let block_hash = B256::repeat_byte(0x11);
    let topics = (0..5).map(B256::repeat_byte);
    let anchor = ValueSpaceAnchor::new(0, block_hash, 0);
    let block = BlockInput::new(0, block_hash, [LogInput::new(Address::repeat_byte(0xaa), topics)]);
    let mut stream = LogValueStream::new(
        DEFAULT_PARAMS,
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
fn an_invalid_later_log_emits_no_events_from_its_block() {
    let block_hash = B256::repeat_byte(0x11);
    let valid = LogInput::new(Address::repeat_byte(0xaa), []);
    let invalid = LogInput::new(Address::repeat_byte(0xbb), (0..5).map(B256::repeat_byte));
    let block = BlockInput::new(0, block_hash, [valid, invalid]);
    let anchor = ValueSpaceAnchor::new(0, block_hash, 0);
    let mut stream = LogValueStream::new(
        DEFAULT_PARAMS,
        anchor,
        [block],
        LogValueStreamTermination::ReachedHead,
    );

    assert_eq!(
        stream.next(),
        Some(Err(LogValueStreamError::InvalidTopicCount {
            block_number: 0,
            log_index: 1,
            actual: 5,
        }))
    );
    assert_eq!(stream.next(), None);
}

#[test]
fn logs_with_zero_through_four_topics_preserve_topic_order_and_ordinals() {
    for topic_count in 0..=4 {
        let block_hash = B256::repeat_byte(topic_count as u8);
        let address = Address::repeat_byte(0xaa);
        let topics = (0..topic_count)
            .map(|ordinal| B256::repeat_byte(ordinal as u8 + 1))
            .collect::<Vec<_>>();
        let block = BlockInput::new(0, block_hash, [LogInput::new(address, topics.clone())]);
        let anchor = ValueSpaceAnchor::new(0, block_hash, 0);

        let actual = LogValueStream::new(
            DEFAULT_PARAMS,
            anchor,
            [block],
            LogValueStreamTermination::ReachedHead,
        )
        .filter_map(|item| match item.unwrap() {
            LogValueStreamItem::Event(LogValueStreamEvent::Slot(LogValueSlot::Value {
                index,
                value,
                kind,
            })) => Some((index, value, kind)),
            _ => None,
        })
        .collect::<Vec<_>>();

        let mut expected = vec![(0, address_value(address), LogValueKind::Address)];
        expected.extend(topics.into_iter().enumerate().map(|(ordinal, topic)| {
            (ordinal as u64 + 1, topic_value(topic), LogValueKind::Topic { ordinal: ordinal as u8 })
        }));
        assert_eq!(actual, expected, "topic count {topic_count}");
    }
}

#[test]
fn a_block_with_only_empty_receipts_behaves_like_an_empty_block() {
    let block_hash = B256::repeat_byte(0x11);
    let receipts =
        [(B256::repeat_byte(0xf0), Vec::<LogInput>::new()), (B256::repeat_byte(0xf1), Vec::new())];
    let flattened_logs = receipts.into_iter().flat_map(|(_transaction_hash, logs)| logs);
    let block = BlockInput::new(0, block_hash, flattened_logs);
    let anchor = ValueSpaceAnchor::new(0, block_hash, 10);

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
                0, block_hash, 10,
            ))),
            LogValueStreamItem::Complete(LogValueStreamCompletion::ReachedHead {
                head: BlockPointer::new(0, block_hash, 10),
                pending_delimiter: PendingDelimiter::new(0, block_hash, 10),
            }),
        ]
    );
}

#[test]
fn flattened_receipts_emit_only_log_values_in_canonical_order() {
    let block_hash = B256::repeat_byte(0x11);
    let addresses =
        [Address::repeat_byte(0xa0), Address::repeat_byte(0xb0), Address::repeat_byte(0xc0)];
    let topics = [B256::repeat_byte(0x01), B256::repeat_byte(0x02), B256::repeat_byte(0x03)];
    let receipts = [
        (
            B256::repeat_byte(0xf0),
            vec![LogInput::new(addresses[0], [topics[0]]), LogInput::new(addresses[1], [])],
        ),
        (B256::repeat_byte(0xf1), vec![]),
        (B256::repeat_byte(0xf2), vec![LogInput::new(addresses[2], [topics[1], topics[2]])]),
    ];
    let flattened_logs = receipts.into_iter().flat_map(|(_transaction_hash, logs)| logs);
    let block = BlockInput::new(0, block_hash, flattened_logs);
    let anchor = ValueSpaceAnchor::new(0, block_hash, 10);

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
                0, block_hash, 10,
            ))),
            value(10, address_value(addresses[0]), LogValueKind::Address),
            value(11, topic_value(topics[0]), LogValueKind::Topic { ordinal: 0 }),
            value(12, address_value(addresses[1]), LogValueKind::Address),
            value(13, address_value(addresses[2]), LogValueKind::Address),
            value(14, topic_value(topics[1]), LogValueKind::Topic { ordinal: 0 }),
            value(15, topic_value(topics[2]), LogValueKind::Topic { ordinal: 1 }),
            LogValueStreamItem::Event(LogValueStreamEvent::Slot(LogValueSlot::BlockDelimiter {
                index: 16,
                block_number: 0,
                block_hash,
            },)),
            LogValueStreamItem::Complete(LogValueStreamCompletion::BatchExhausted {
                last_block: BlockPointer::new(0, block_hash, 10),
                continuation: BatchContinuation::new(1, 17),
            }),
        ]
    );
}
