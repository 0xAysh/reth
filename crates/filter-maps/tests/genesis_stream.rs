//! Lifecycle tests for the public log value stream.

use alloy_primitives::B256;
use reth_filter_maps::{
    BatchContinuation, BlockInput, BlockPointer, LogValueSlot, LogValueStream,
    LogValueStreamCompletion, LogValueStreamError, LogValueStreamEvent, LogValueStreamItem,
    LogValueStreamTermination, PendingDelimiter, UnknownValueSpaceVersion, ValueSpaceAnchor,
    ValueSpaceVersion, DEFAULT_PARAMS, GETH_V1,
};

fn empty_block(number: u64, hash_byte: u8) -> BlockInput {
    BlockInput::new(number, B256::repeat_byte(hash_byte), [])
}

const fn pointer(number: u64, hash_byte: u8, index: u64) -> LogValueStreamItem {
    LogValueStreamItem::Event(LogValueStreamEvent::BlockPointer(BlockPointer::new(
        number,
        B256::repeat_byte(hash_byte),
        index,
    )))
}

const fn delimiter(number: u64, hash_byte: u8, index: u64) -> LogValueStreamItem {
    LogValueStreamItem::Event(LogValueStreamEvent::Slot(LogValueSlot::BlockDelimiter {
        index,
        block_number: number,
        block_hash: B256::repeat_byte(hash_byte),
    }))
}

#[test]
fn nonzero_anchor_reaches_head_across_contiguous_empty_blocks() {
    let anchor = ValueSpaceAnchor::new(100, B256::repeat_byte(0x64), 42);
    let mut stream = LogValueStream::new(
        DEFAULT_PARAMS,
        anchor,
        [empty_block(100, 0x64), empty_block(101, 0x65), empty_block(102, 0x66)],
        LogValueStreamTermination::ReachedHead,
    );

    assert_eq!(
        stream.by_ref().collect::<Vec<_>>(),
        vec![
            Ok(pointer(100, 0x64, 42)),
            Ok(delimiter(100, 0x64, 42)),
            Ok(pointer(101, 0x65, 43)),
            Ok(delimiter(101, 0x65, 43)),
            Ok(pointer(102, 0x66, 44)),
            Ok(LogValueStreamItem::Complete(LogValueStreamCompletion::ReachedHead {
                head: BlockPointer::new(102, B256::repeat_byte(0x66), 44),
                pending_delimiter: PendingDelimiter::new(102, B256::repeat_byte(0x66), 44,),
            })),
        ]
    );
    assert_eq!(stream.next(), None);
    assert_eq!(stream.next(), None);
}

#[test]
fn nonzero_anchor_exhausts_batch_across_contiguous_empty_blocks() {
    let anchor = ValueSpaceAnchor::new(100, B256::repeat_byte(0x64), 42);
    let mut stream = LogValueStream::new(
        DEFAULT_PARAMS,
        anchor,
        [empty_block(100, 0x64), empty_block(101, 0x65), empty_block(102, 0x66)],
        LogValueStreamTermination::BatchExhausted,
    );

    assert_eq!(
        stream.by_ref().collect::<Vec<_>>(),
        vec![
            Ok(pointer(100, 0x64, 42)),
            Ok(delimiter(100, 0x64, 42)),
            Ok(pointer(101, 0x65, 43)),
            Ok(delimiter(101, 0x65, 43)),
            Ok(pointer(102, 0x66, 44)),
            Ok(delimiter(102, 0x66, 44)),
            Ok(LogValueStreamItem::Complete(LogValueStreamCompletion::BatchExhausted {
                last_block: BlockPointer::new(102, B256::repeat_byte(0x66), 44),
                continuation: BatchContinuation::new(103, 45),
            })),
        ]
    );
    assert_eq!(stream.next(), None);
    assert_eq!(stream.next(), None);
}

#[test]
fn single_non_genesis_head_leaves_its_delimiter_pending() {
    let block_hash = B256::repeat_byte(0x64);
    let anchor = ValueSpaceAnchor::new(100, block_hash, 42);
    let mut stream = LogValueStream::new(
        DEFAULT_PARAMS,
        anchor,
        [BlockInput::new(100, block_hash, [])],
        LogValueStreamTermination::ReachedHead,
    );

    assert_eq!(
        stream.by_ref().collect::<Vec<_>>(),
        vec![
            Ok(pointer(100, 0x64, 42)),
            Ok(LogValueStreamItem::Complete(LogValueStreamCompletion::ReachedHead {
                head: BlockPointer::new(100, block_hash, 42),
                pending_delimiter: PendingDelimiter::new(100, block_hash, 42),
            })),
        ]
    );
}

#[test]
fn genesis_with_no_logs_leaves_its_delimiter_pending() {
    let genesis_hash = B256::repeat_byte(0x11);
    let anchor = ValueSpaceAnchor::new(0, genesis_hash, 0);
    let genesis = BlockInput::new(0, genesis_hash, []);
    let mut stream = LogValueStream::new(
        DEFAULT_PARAMS,
        anchor,
        [genesis],
        LogValueStreamTermination::ReachedHead,
    );

    assert_eq!(
        stream.next(),
        Some(Ok(LogValueStreamItem::Event(LogValueStreamEvent::BlockPointer(BlockPointer::new(
            0,
            genesis_hash,
            0
        )))))
    );
    assert_eq!(
        stream.next(),
        Some(Ok(LogValueStreamItem::Complete(LogValueStreamCompletion::ReachedHead {
            head: BlockPointer::new(0, genesis_hash, 0),
            pending_delimiter: PendingDelimiter::new(0, genesis_hash, 0),
        })))
    );

    // Completion is fused, and the pending delimiter was metadata rather than a materialized slot.
    assert_eq!(stream.next(), None);
    assert_eq!(stream.next(), None);
}

#[test]
fn batch_exhaustion_materializes_the_last_delimiter_and_returns_continuation() {
    let genesis_hash = B256::repeat_byte(0x11);
    let anchor = ValueSpaceAnchor::new(0, genesis_hash, 0);
    let genesis = BlockInput::new(0, genesis_hash, []);
    let mut stream = LogValueStream::new(
        DEFAULT_PARAMS,
        anchor,
        [genesis],
        LogValueStreamTermination::BatchExhausted,
    );

    assert_eq!(
        stream.next(),
        Some(Ok(LogValueStreamItem::Event(LogValueStreamEvent::BlockPointer(BlockPointer::new(
            0,
            genesis_hash,
            0,
        )))))
    );
    assert_eq!(
        stream.next(),
        Some(Ok(LogValueStreamItem::Event(LogValueStreamEvent::Slot(
            LogValueSlot::BlockDelimiter { index: 0, block_number: 0, block_hash: genesis_hash },
        ))))
    );
    assert_eq!(
        stream.next(),
        Some(Ok(LogValueStreamItem::Complete(LogValueStreamCompletion::BatchExhausted {
            last_block: BlockPointer::new(0, genesis_hash, 0),
            continuation: BatchContinuation::new(1, 1),
        })))
    );
    assert_eq!(stream.next(), None);
}

#[test]
fn block_gap_errors_once_then_fuses() {
    assert_non_contiguous_error([empty_block(100, 0x64), empty_block(102, 0x66)], 102);
}

#[test]
fn duplicate_block_errors_once_then_fuses() {
    assert_non_contiguous_error([empty_block(100, 0x64), empty_block(100, 0x65)], 100);
}

#[test]
fn descending_block_errors_once_then_fuses() {
    assert_non_contiguous_error([empty_block(100, 0x64), empty_block(99, 0x63)], 99);
}

fn assert_non_contiguous_error(blocks: [BlockInput; 2], actual: u64) {
    let anchor = ValueSpaceAnchor::new(100, B256::repeat_byte(0x64), 42);
    let mut stream =
        LogValueStream::new(DEFAULT_PARAMS, anchor, blocks, LogValueStreamTermination::ReachedHead);

    assert_eq!(stream.next(), Some(Ok(pointer(100, 0x64, 42))));
    assert_eq!(stream.next(), Some(Ok(delimiter(100, 0x64, 42))));
    assert_eq!(
        stream.next(),
        Some(Err(LogValueStreamError::NonContiguousBlock { expected: 101, actual }))
    );
    assert_eq!(stream.next(), None);
    assert_eq!(stream.next(), None);
}

#[test]
fn empty_input_errors_once_then_fuses() {
    let anchor = ValueSpaceAnchor::new(100, B256::repeat_byte(0x64), 42);
    let mut stream =
        LogValueStream::new(DEFAULT_PARAMS, anchor, [], LogValueStreamTermination::ReachedHead);

    assert_eq!(stream.next(), Some(Err(LogValueStreamError::EmptyInput)));
    assert_eq!(stream.next(), None);
    assert_eq!(stream.next(), None);
}

#[test]
fn delimiter_index_overflow_discards_the_failed_blocks_events() {
    let anchor = ValueSpaceAnchor::new(100, B256::repeat_byte(0x64), u64::MAX);
    let mut stream = LogValueStream::new(
        DEFAULT_PARAMS,
        anchor,
        [empty_block(100, 0x64)],
        LogValueStreamTermination::BatchExhausted,
    );

    assert_eq!(stream.next(), Some(Err(LogValueStreamError::LogValueIndexOverflow)));
    assert_eq!(stream.next(), None);
    assert_eq!(stream.next(), None);
}

#[test]
fn maximum_block_number_is_valid_at_canonical_head() {
    let block_hash = B256::repeat_byte(0xff);
    let anchor = ValueSpaceAnchor::new(u64::MAX, block_hash, 42);
    let mut stream = LogValueStream::new(
        DEFAULT_PARAMS,
        anchor,
        [BlockInput::new(u64::MAX, block_hash, [])],
        LogValueStreamTermination::ReachedHead,
    );

    assert_eq!(
        stream.by_ref().collect::<Vec<_>>(),
        vec![
            Ok(pointer(u64::MAX, 0xff, 42)),
            Ok(LogValueStreamItem::Complete(LogValueStreamCompletion::ReachedHead {
                head: BlockPointer::new(u64::MAX, block_hash, 42),
                pending_delimiter: PendingDelimiter::new(u64::MAX, block_hash, 42),
            })),
        ]
    );
    assert_eq!(stream.next(), None);
}

#[test]
fn maximum_block_number_at_batch_boundary_errors_once_then_fuses() {
    let block_hash = B256::repeat_byte(0xff);
    let anchor = ValueSpaceAnchor::new(u64::MAX, block_hash, 42);
    let mut stream = LogValueStream::new(
        DEFAULT_PARAMS,
        anchor,
        [BlockInput::new(u64::MAX, block_hash, [])],
        LogValueStreamTermination::BatchExhausted,
    );

    assert_eq!(stream.next(), Some(Err(LogValueStreamError::BlockNumberOverflow)));
    assert_eq!(stream.next(), None);
    assert_eq!(stream.next(), None);
}

#[test]
fn value_space_version_has_a_stable_rejecting_encoding() {
    let _: ValueSpaceVersion = GETH_V1;
    let _ = DEFAULT_PARAMS;

    assert_eq!(LogValueStream::VALUE_SPACE_VERSION, GETH_V1);
    assert_eq!(u8::from(GETH_V1), 1);
    assert_eq!(ValueSpaceVersion::try_from(1), Ok(GETH_V1));
    assert_eq!(ValueSpaceVersion::try_from(2), Err(UnknownValueSpaceVersion::new(2)),);
}

#[test]
fn an_anchor_block_number_mismatch_errors_once_then_fuses() {
    let anchor = ValueSpaceAnchor::new(100, B256::repeat_byte(0x64), 42);
    let mut stream = LogValueStream::new(
        DEFAULT_PARAMS,
        anchor,
        [empty_block(101, 0x65)],
        LogValueStreamTermination::ReachedHead,
    );

    assert_eq!(
        stream.next(),
        Some(Err(LogValueStreamError::AnchorBlockNumberMismatch { expected: 100, actual: 101 }))
    );
    assert_eq!(stream.next(), None);
    assert_eq!(stream.next(), None);
}

#[test]
fn an_anchor_hash_mismatch_errors_once_then_fuses() {
    let anchor = ValueSpaceAnchor::new(0, B256::repeat_byte(0x22), 0);
    let genesis = BlockInput::new(0, B256::repeat_byte(0x33), []);
    let mut stream = LogValueStream::new(
        DEFAULT_PARAMS,
        anchor,
        [genesis],
        LogValueStreamTermination::ReachedHead,
    );

    assert_eq!(
        stream.next(),
        Some(Err(LogValueStreamError::AnchorBlockHashMismatch {
            expected: B256::repeat_byte(0x22),
            actual: B256::repeat_byte(0x33),
        }))
    );
    assert_eq!(stream.next(), None);
    assert_eq!(stream.next(), None);
}
