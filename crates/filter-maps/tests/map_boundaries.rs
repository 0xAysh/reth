//! Completed-map resume-anchor tests.

use alloy_primitives::{address, b256};
use reth_filter_maps::{
    BlockInput, LogInput, LogValueSlot, LogValueStream, LogValueStreamError, LogValueStreamEvent,
    LogValueStreamItem, LogValueStreamTermination, MapBoundary, ValueSpaceAnchor, DEFAULT_PARAMS,
    RANGE_TEST_PARAMS,
};

const BLOCK_10_HASH: alloy_primitives::B256 =
    b256!("0x1010101010101010101010101010101010101010101010101010101010101010");
const BLOCK_11_HASH: alloy_primitives::B256 =
    b256!("0x1111111111111111111111111111111111111111111111111111111111111111");

fn log(topic_count: usize) -> LogInput {
    LogInput::new(
        address!("0x1111111111111111111111111111111111111111"),
        (0..topic_count).map(|ordinal| alloy_primitives::B256::with_last_byte(ordinal as u8)),
    )
}

const fn boundary(item: &LogValueStreamItem) -> Option<MapBoundary> {
    match item {
        LogValueStreamItem::Event(LogValueStreamEvent::MapBoundary(boundary)) => Some(*boundary),
        _ => None,
    }
}

#[test]
fn searchable_value_completes_the_absolute_map_before_head_completion() {
    let values_per_map = DEFAULT_PARAMS.values_per_map();
    let index = values_per_map * 2 - 1;
    let block = BlockInput::new(10, BLOCK_10_HASH, [log(0)]);
    let anchor = ValueSpaceAnchor::new(10, BLOCK_10_HASH, index);

    let items = LogValueStream::new(
        DEFAULT_PARAMS,
        anchor,
        [block],
        LogValueStreamTermination::ReachedHead,
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();

    assert!(matches!(
        items[1],
        LogValueStreamItem::Event(LogValueStreamEvent::Slot(LogValueSlot::Value {
            index: value_index,
            ..
        })) if value_index == index
    ));
    assert_eq!(boundary(&items[2]), Some(MapBoundary::new(1, 10, BLOCK_10_HASH)));
    assert!(matches!(items[3], LogValueStreamItem::Complete(_)));
    assert_eq!(items.iter().filter_map(boundary).count(), 1);
}

#[test]
fn partial_map_and_pending_delimiter_at_a_new_map_emit_no_boundary() {
    let partial_block = BlockInput::new(10, BLOCK_10_HASH, [log(0)]);
    let partial = LogValueStream::new(
        DEFAULT_PARAMS,
        ValueSpaceAnchor::new(10, BLOCK_10_HASH, 100),
        [partial_block],
        LogValueStreamTermination::ReachedHead,
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    assert!(partial.iter().filter_map(boundary).next().is_none());

    let boundary_index = DEFAULT_PARAMS.values_per_map();
    let empty_head = BlockInput::new(10, BLOCK_10_HASH, []);
    let pending_at_boundary = LogValueStream::new(
        DEFAULT_PARAMS,
        ValueSpaceAnchor::new(10, BLOCK_10_HASH, boundary_index),
        [empty_head],
        LogValueStreamTermination::ReachedHead,
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    assert!(pending_at_boundary.iter().filter_map(boundary).next().is_none());
}

#[test]
fn delimiter_can_complete_a_map() {
    let index = DEFAULT_PARAMS.values_per_map() - 1;
    let block = BlockInput::new(10, BLOCK_10_HASH, []);
    let items = LogValueStream::new(
        DEFAULT_PARAMS,
        ValueSpaceAnchor::new(10, BLOCK_10_HASH, index),
        [block],
        LogValueStreamTermination::BatchExhausted,
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();

    assert!(matches!(
        items[1],
        LogValueStreamItem::Event(LogValueStreamEvent::Slot(
            LogValueSlot::BlockDelimiter { index: delimiter_index, .. }
        )) if delimiter_index == index
    ));
    assert_eq!(boundary(&items[2]), Some(MapBoundary::new(0, 10, BLOCK_10_HASH)));
    assert!(matches!(items[3], LogValueStreamItem::Complete(_)));
}

#[test]
fn padding_before_a_new_block_uses_the_preceding_block_as_resume_anchor() {
    let values_per_map = DEFAULT_PARAMS.values_per_map();
    let first_index = values_per_map - 2;
    let blocks =
        [BlockInput::new(10, BLOCK_10_HASH, []), BlockInput::new(11, BLOCK_11_HASH, [log(1)])];
    let items = LogValueStream::new(
        DEFAULT_PARAMS,
        ValueSpaceAnchor::new(10, BLOCK_10_HASH, first_index),
        blocks,
        LogValueStreamTermination::ReachedHead,
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();

    let boundary_position = items.iter().position(|item| boundary(item).is_some()).unwrap();
    assert!(matches!(
        items[boundary_position - 1],
        LogValueStreamItem::Event(LogValueStreamEvent::Slot(LogValueSlot::Padding {
            index
        })) if index == values_per_map - 1
    ));
    assert_eq!(boundary(&items[boundary_position]), Some(MapBoundary::new(0, 10, BLOCK_10_HASH)));
    assert!(matches!(
        items[boundary_position + 1],
        LogValueStreamItem::Event(LogValueStreamEvent::BlockPointer(pointer))
            if pointer.block_number == 11 && pointer.first_log_value_index == values_per_map
    ));
}

#[test]
fn one_block_can_complete_multiple_maps_exactly_once_each() {
    let block = BlockInput::new(10, BLOCK_10_HASH, [log(0), log(0), log(0)]);
    let items = LogValueStream::new(
        RANGE_TEST_PARAMS,
        ValueSpaceAnchor::new(10, BLOCK_10_HASH, 5),
        [block],
        LogValueStreamTermination::ReachedHead,
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();

    assert_eq!(
        items.iter().filter_map(boundary).collect::<Vec<_>>(),
        vec![
            MapBoundary::new(5, 10, BLOCK_10_HASH),
            MapBoundary::new(6, 10, BLOCK_10_HASH),
            MapBoundary::new(7, 10, BLOCK_10_HASH),
        ]
    );
}

#[test]
fn map_index_overflow_is_typed_block_atomic_and_fuses() {
    let overflowing_map = u64::from(u32::MAX) + 1;
    let index =
        overflowing_map * DEFAULT_PARAMS.values_per_map() + DEFAULT_PARAMS.values_per_map() - 1;
    let block = BlockInput::new(10, BLOCK_10_HASH, [log(0)]);
    let mut stream = LogValueStream::new(
        DEFAULT_PARAMS,
        ValueSpaceAnchor::new(10, BLOCK_10_HASH, index),
        [block],
        LogValueStreamTermination::ReachedHead,
    );

    assert_eq!(
        stream.next(),
        Some(Err(LogValueStreamError::MapIndexOverflow { map_index: overflowing_map }))
    );
    assert_eq!(stream.next(), None);
}
