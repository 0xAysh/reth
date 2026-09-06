//! Pull-consumer and failure contracts for the pure stream.

use alloy_eips::BlockNumHash;
use alloy_primitives::B256;
use reth_filter_maps::{
    BatchContinuation, BlockInput, LogInput, LogValueStream, LogValueStreamError,
    LogValueStreamEvent, LogValueStreamItem, LogValueStreamTermination, ValueSpaceAnchor,
    DEFAULT_PARAMS, RANGE_TEST_PARAMS,
};
use std::cell::Cell;

struct NonCloneBlocks<'a> {
    fetched: &'a Cell<usize>,
    next_number: u64,
}

impl Iterator for NonCloneBlocks<'_> {
    type Item = BlockInput;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next_number == 1000 {
            return None
        }
        self.fetched.set(self.fetched.get() + 1);
        let input = block(self.next_number, 0);
        self.next_number += 1;
        Some(input)
    }
}

fn block(number: u64, topics: usize) -> BlockInput {
    BlockInput::new(
        number,
        B256::ZERO,
        [LogInput::new(Default::default(), vec![B256::ZERO; topics])],
    )
}

#[test]
fn construction_and_map_limited_consumer_do_not_exhaust_input() {
    let fetched = Cell::new(0);
    // Borrows local state and is not Clone: consumers must not need 'static or cloneable sources.
    let input = NonCloneBlocks { fetched: &fetched, next_number: 0 };
    let mut stream = LogValueStream::new(
        DEFAULT_PARAMS,
        ValueSpaceAnchor::new(0, B256::ZERO, DEFAULT_PARAMS.values_per_map() - 1),
        input,
        LogValueStreamTermination::ReachedHead,
    );
    assert_eq!(fetched.get(), 0, "construction must not call input.next()");
    assert!(matches!(
        stream.next(),
        Some(Ok(LogValueStreamItem::Event(LogValueStreamEvent::BlockPointer(_))))
    ));
    assert_eq!(fetched.get(), 2, "only current block and successor are needed");
    for item in stream.by_ref() {
        let item = item.unwrap();
        if matches!(item, LogValueStreamItem::Event(LogValueStreamEvent::MapBoundary(_))) {
            break
        }
    }
    assert_eq!(fetched.get(), 2);
    // A renderer can retain the same iterator and continue without re-fetching the active block.
    assert!(stream.next().unwrap().is_ok());
    assert_eq!(fetched.get(), 2);
    drop(stream);
    assert_eq!(fetched.get(), 2, "dropping a consumer must not drain input");
}

#[test]
fn continuation_construction_is_lazy() {
    let fetched = Cell::new(0);
    let input = (10..1010).inspect(|_| fetched.set(fetched.get() + 1)).map(|n| block(n, 0));
    let mut stream = LogValueStream::continue_from(
        DEFAULT_PARAMS,
        BatchContinuation::new(BlockNumHash::new(10, B256::ZERO), 7),
        input,
        LogValueStreamTermination::ReachedHead,
    );
    assert_eq!(fetched.get(), 0);
    assert!(stream.next().unwrap().is_ok());
    assert_eq!(fetched.get(), 2);
}

#[test]
fn a_paused_consumer_and_its_clone_preserve_the_exact_suffix() {
    let blocks = [block(10, 0), block(11, 2)];
    let anchor = ValueSpaceAnchor::new(10, B256::ZERO, DEFAULT_PARAMS.values_per_map() - 2);
    let expected = LogValueStream::new(
        DEFAULT_PARAMS,
        anchor,
        blocks.clone(),
        LogValueStreamTermination::ReachedHead,
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    let mut stream =
        LogValueStream::new(DEFAULT_PARAMS, anchor, blocks, LogValueStreamTermination::ReachedHead);
    for (position, expected_item) in expected.iter().enumerate() {
        assert_eq!(stream.next(), Some(Ok(*expected_item)));
        // This includes a pending boundary, the lookahead block and the active log/topic cursor.
        let resumed = stream.clone().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(resumed, expected[position + 1..]);
    }
    assert_eq!(stream.next(), None);
}

#[test]
fn anchored_first_log_cannot_require_padding() {
    let index = DEFAULT_PARAMS.values_per_map() - 1;
    let mut stream = LogValueStream::new(
        DEFAULT_PARAMS,
        ValueSpaceAnchor::new(0, B256::ZERO, index),
        [block(0, 1)],
        LogValueStreamTermination::ReachedHead,
    );
    assert_eq!(
        stream.next(),
        Some(Err(LogValueStreamError::AnchorRequiresPadding { index, padding: 1 }))
    );
    assert_eq!(stream.next(), None);
    assert_eq!(stream.next(), None);
}

#[test]
fn searchable_value_overflow_is_block_atomic() {
    let mut stream = LogValueStream::new(
        DEFAULT_PARAMS,
        ValueSpaceAnchor::new(0, B256::ZERO, u64::MAX),
        [block(0, 0)],
        LogValueStreamTermination::ReachedHead,
    );
    assert_eq!(stream.next(), Some(Err(LogValueStreamError::LogValueIndexOverflow)));
    assert_eq!(stream.next(), None);
    assert_eq!(stream.next(), None);
}

#[test]
fn continuation_number_mismatch_is_fused() {
    let mut stream = LogValueStream::continue_from(
        DEFAULT_PARAMS,
        BatchContinuation::new(BlockNumHash::new(10, B256::ZERO), 0),
        [block(11, 0)],
        LogValueStreamTermination::ReachedHead,
    );
    assert_eq!(
        stream.next(),
        Some(Err(LogValueStreamError::NonContiguousBlock { expected: 10, actual: 11 }))
    );
    assert_eq!(stream.next(), None);
    assert_eq!(stream.next(), None);
}

#[test]
fn late_arithmetic_failure_prevents_the_blocks_pointer_escaping() {
    let mut input = block(10, 0);
    input.logs.push(LogInput::new(Default::default(), []));
    let mut stream = LogValueStream::new(
        RANGE_TEST_PARAMS,
        ValueSpaceAnchor::new(10, B256::ZERO, u64::from(u32::MAX)),
        [input],
        LogValueStreamTermination::ReachedHead,
    );
    assert_eq!(
        stream.next(),
        Some(Err(LogValueStreamError::MapIndexOverflow { map_index: u64::from(u32::MAX) + 1 }))
    );
    assert_eq!(stream.next(), None);
}

#[test]
fn lookahead_is_not_validated_as_an_active_block() {
    let mut stream = LogValueStream::new(
        RANGE_TEST_PARAMS,
        ValueSpaceAnchor::new(10, B256::ZERO, 0),
        [block(10, 0), block(11, 5)],
        LogValueStreamTermination::ReachedHead,
    );
    // Pointer, value, value boundary, delimiter and delimiter boundary for valid block 10.
    for _ in 0..5 {
        assert!(stream.next().unwrap().is_ok());
    }
    assert!(matches!(
        stream.next(),
        Some(Err(LogValueStreamError::InvalidTopicCount { block_number: 11, .. }))
    ));
    assert_eq!(stream.next(), None);
}

#[test]
fn successor_number_error_timing_is_preserved() {
    for (index, events_before_error) in [(0, 3), (DEFAULT_PARAMS.values_per_map() - 2, 0)] {
        let mut stream = LogValueStream::new(
            DEFAULT_PARAMS,
            ValueSpaceAnchor::new(10, B256::ZERO, index),
            [block(10, 0), block(12, 0)],
            LogValueStreamTermination::ReachedHead,
        );
        for _ in 0..events_before_error {
            assert!(stream.next().unwrap().is_ok());
        }
        assert_eq!(
            stream.next(),
            Some(Err(LogValueStreamError::NonContiguousBlock { expected: 11, actual: 12 }))
        );
        assert_eq!(stream.next(), None);
    }
}
