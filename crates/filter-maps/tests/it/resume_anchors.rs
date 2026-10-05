//! Stateful renderer-contract tests for Geth-compatible map resume anchors.
//!
//! These tests consume events in order. In particular, they never search later stream output for
//! the block pointer named by a boundary.

use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256};
use reth_filter_maps::{
    BatchContinuation, BlockInput, BlockPointer, LogInput, LogValueSlot, LogValueStream,
    LogValueStreamCompletion, LogValueStreamEvent, LogValueStreamItem, LogValueStreamTermination,
    MapBoundary, Params, ValueSpaceAnchor, DEFAULT_PARAMS, RANGE_TEST_PARAMS,
};

fn block(number: u64, logs: impl IntoIterator<Item = LogInput>) -> BlockInput {
    BlockInput::new(number, B256::repeat_byte(number as u8), logs)
}

fn log(topic_count: usize) -> LogInput {
    LogInput::new(
        Address::repeat_byte(0xaa),
        (0..topic_count).map(|ordinal| B256::repeat_byte(ordinal as u8 + 1)),
    )
}

const fn slot_index(slot: &LogValueSlot) -> u64 {
    match slot {
        LogValueSlot::Value { index, .. } |
        LogValueSlot::BlockDelimiter { index, .. } |
        LogValueSlot::Padding { index } => *index,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DurableMap {
    /// Test representation of the rows rendered from every slot through this map.
    rows: Vec<LogValueSlot>,
    resume_block: BlockNumHash,
    resume_pointer: u64,
    valid_through_map: u32,
}

#[derive(Debug, Default)]
struct TestStorage {
    published: Option<DurableMap>,
    writes: usize,
}

impl TestStorage {
    /// Models the storage transaction that publishes rows and all restart metadata together.
    fn publish_atomically(&mut self, map: DurableMap) {
        assert!(self.published.is_none(), "the first completed map is published only once");
        self.published = Some(map);
        self.writes += 1;
    }
}

#[derive(Debug)]
struct CompletedMap {
    boundary: MapBoundary,
    rows: Vec<LogValueSlot>,
}

/// Minimal stateful renderer used only to validate the stream's consumer contract.
struct StatefulRenderer<'a> {
    storage: &'a mut TestStorage,
    rows: Vec<LogValueSlot>,
    latest_pointer: Option<BlockPointer>,
    completed: Option<CompletedMap>,
}

impl<'a> StatefulRenderer<'a> {
    const fn new(storage: &'a mut TestStorage) -> Self {
        Self { storage, rows: Vec::new(), latest_pointer: None, completed: None }
    }

    /// Returns true only when this event made the completed map durably restartable.
    fn consume(&mut self, event: LogValueStreamEvent) -> bool {
        match event {
            LogValueStreamEvent::Slot(slot) => {
                assert!(self.completed.is_none(), "do not render beyond an unpublished boundary");
                self.rows.push(slot);
                false
            }
            LogValueStreamEvent::BlockPointer(pointer) => {
                self.latest_pointer = Some(pointer);
                self.try_publish()
            }
            LogValueStreamEvent::MapBoundary(boundary) => {
                assert!(self.completed.is_none());
                self.completed =
                    Some(CompletedMap { boundary, rows: std::mem::take(&mut self.rows) });
                self.try_publish()
            }
        }
    }

    fn try_publish(&mut self) -> bool {
        let (Some(completed), Some(pointer)) = (&self.completed, self.latest_pointer) else {
            return false
        };
        if pointer.block_number != completed.boundary.resume_block_number ||
            pointer.block_hash != completed.boundary.resume_block_hash
        {
            return false
        }

        let completed = self.completed.take().expect("checked above");
        self.storage.publish_atomically(DurableMap {
            rows: completed.rows,
            resume_block: BlockNumHash::new(pointer.block_number, pointer.block_hash),
            resume_pointer: pointer.first_log_value_index,
            valid_through_map: completed.boundary.completed_map_index,
        });
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerTiming {
    AvailableAtBoundary,
    OneAdditionalPointer,
}

fn all_slots(
    params: Params,
    anchor: ValueSpaceAnchor,
    blocks: Vec<BlockInput>,
) -> Vec<LogValueSlot> {
    LogValueStream::new(params, anchor, blocks, LogValueStreamTermination::ReachedHead)
        .map(|item| item.unwrap())
        .filter_map(|item| match item {
            LogValueStreamItem::Event(LogValueStreamEvent::Slot(slot)) => Some(slot),
            _ => None,
        })
        .collect()
}

fn assert_durable_restart(
    params: Params,
    anchor: ValueSpaceAnchor,
    blocks: Vec<BlockInput>,
    batch_end: Option<usize>,
    expected_timing: PointerTiming,
) {
    let uninterrupted = all_slots(params, anchor, blocks.clone());
    let mut storage = TestStorage::default();
    let mut renderer = StatefulRenderer::new(&mut storage);
    let mut saw_boundary = false;
    let mut pointer_events_after_boundary = 0;
    let mut published_at_boundary = false;

    let first_end = batch_end.unwrap_or(blocks.len());
    let first_termination = if first_end == blocks.len() {
        LogValueStreamTermination::ReachedHead
    } else {
        let next = &blocks[first_end];
        LogValueStreamTermination::BatchExhausted {
            next_block: BlockNumHash::new(next.number, next.hash),
        }
    };
    let mut continuation = None;
    let first_stream =
        LogValueStream::new(params, anchor, blocks[..first_end].to_vec(), first_termination);

    for item in first_stream {
        match item.unwrap() {
            LogValueStreamItem::Event(event) => {
                if saw_boundary && matches!(event, LogValueStreamEvent::BlockPointer(_)) {
                    pointer_events_after_boundary += 1;
                }
                let is_boundary = matches!(event, LogValueStreamEvent::MapBoundary(_));
                let published = renderer.consume(event);
                if is_boundary {
                    saw_boundary = true;
                    published_at_boundary = published;
                }
                if published {
                    break
                }
            }
            LogValueStreamItem::Complete(LogValueStreamCompletion::BatchExhausted {
                continuation: next,
                ..
            }) => continuation = Some(next),
            LogValueStreamItem::Complete(LogValueStreamCompletion::ReachedHead { .. }) => {}
        }
    }

    if renderer.storage.published.is_none() {
        let continuation = continuation.expect("a delayed bounded boundary needs its next batch");
        let second_stream = LogValueStream::continue_from(
            params,
            continuation,
            blocks[first_end..].to_vec(),
            LogValueStreamTermination::ReachedHead,
        );
        for item in second_stream {
            if let LogValueStreamItem::Event(event) = item.unwrap() {
                if saw_boundary && matches!(event, LogValueStreamEvent::BlockPointer(_)) {
                    pointer_events_after_boundary += 1;
                }
                if renderer.consume(event) {
                    break
                }
            }
        }
    }

    assert!(saw_boundary);
    assert_eq!(renderer.storage.writes, 1);
    match expected_timing {
        PointerTiming::AvailableAtBoundary => {
            assert!(published_at_boundary);
            assert_eq!(pointer_events_after_boundary, 0);
        }
        PointerTiming::OneAdditionalPointer => {
            assert!(!published_at_boundary);
            assert_eq!(pointer_events_after_boundary, 1);
        }
    }

    // Simulate a crash immediately after the atomic publication: no iterator or renderer state is
    // retained. Everything below is reconstructed from the storage transaction alone.
    drop(renderer);
    let persisted = storage.published.clone().expect("the map was published");
    let first_block = blocks
        .iter()
        .position(|block| {
            block.number == persisted.resume_block.number &&
                block.hash == persisted.resume_block.hash
        })
        .expect("persisted canonical identity selects the restart input");
    let restart_anchor = ValueSpaceAnchor::new(
        persisted.resume_block.number,
        persisted.resume_block.hash,
        persisted.resume_pointer,
    );
    let restarted_tail = all_slots(params, restart_anchor, blocks[first_block..].to_vec())
        .into_iter()
        .filter(|slot| {
            slot_index(slot) / params.values_per_map() > u64::from(persisted.valid_through_map)
        });
    let restarted_output = persisted.rows.iter().copied().chain(restarted_tail).collect::<Vec<_>>();

    assert_eq!(restarted_output, uninterrupted);
}

#[test]
fn searchable_value_completion_publishes_with_the_pointer_already_available() {
    let map_size = DEFAULT_PARAMS.values_per_map();
    let blocks = vec![block(10, [log(0)]), block(11, [log(0)])];
    assert_durable_restart(
        DEFAULT_PARAMS,
        ValueSpaceAnchor::new(10, blocks[0].hash, map_size - 1),
        blocks,
        None,
        PointerTiming::AvailableAtBoundary,
    );
}

#[test]
fn delimiter_completion_waits_one_pointer_for_a_non_empty_successor() {
    let map_size = DEFAULT_PARAMS.values_per_map();
    let blocks = vec![block(10, [log(0)]), block(11, [log(0)])];
    assert_durable_restart(
        DEFAULT_PARAMS,
        ValueSpaceAnchor::new(10, blocks[0].hash, map_size - 2),
        blocks,
        None,
        PointerTiming::OneAdditionalPointer,
    );
}

#[test]
fn delimiter_completion_waits_one_pointer_for_an_empty_successor() {
    let map_size = DEFAULT_PARAMS.values_per_map();
    let blocks = vec![block(10, [log(0)]), block(11, []), block(12, [log(0)])];
    assert_durable_restart(
        DEFAULT_PARAMS,
        ValueSpaceAnchor::new(10, blocks[0].hash, map_size - 2),
        blocks,
        None,
        PointerTiming::OneAdditionalPointer,
    );
}

#[test]
fn padding_completion_waits_one_pointer_for_the_upcoming_block() {
    let map_size = DEFAULT_PARAMS.values_per_map();
    let blocks = vec![block(10, []), block(11, [log(1)])];
    assert_durable_restart(
        DEFAULT_PARAMS,
        ValueSpaceAnchor::new(10, blocks[0].hash, map_size - 2),
        blocks,
        None,
        PointerTiming::OneAdditionalPointer,
    );
}

#[test]
fn a_block_spanning_multiple_maps_reuses_its_available_pointer() {
    let blocks = vec![block(10, [log(0), log(0), log(0)])];
    assert_durable_restart(
        RANGE_TEST_PARAMS,
        ValueSpaceAnchor::new(10, blocks[0].hash, 0),
        blocks,
        None,
        PointerTiming::AvailableAtBoundary,
    );
}

#[test]
fn bounded_batch_ending_at_a_boundary_waits_for_the_next_batches_pointer() {
    let map_size = DEFAULT_PARAMS.values_per_map();
    let blocks = vec![block(10, []), block(11, [log(0)])];
    assert_durable_restart(
        DEFAULT_PARAMS,
        ValueSpaceAnchor::new(10, blocks[0].hash, map_size - 1),
        blocks,
        Some(1),
        PointerTiming::OneAdditionalPointer,
    );
}

#[test]
fn a_pending_head_delimiter_at_a_map_boundary_is_not_a_completed_map() {
    let map_size = DEFAULT_PARAMS.values_per_map();
    let head = block(10, []);
    let mut storage = TestStorage::default();
    let mut renderer = StatefulRenderer::new(&mut storage);
    let stream = LogValueStream::new(
        DEFAULT_PARAMS,
        ValueSpaceAnchor::new(10, head.hash, map_size),
        [head],
        LogValueStreamTermination::ReachedHead,
    );

    for item in stream {
        if let LogValueStreamItem::Event(event) = item.unwrap() {
            assert!(!renderer.consume(event));
        }
    }

    assert!(renderer.completed.is_none());
    assert!(renderer.storage.published.is_none());
}

#[test]
fn batch_continuation_retains_identity_and_the_pre_padding_cursor() {
    let map_size = DEFAULT_PARAMS.values_per_map();
    let first = block(10, []);
    let second = block(11, [log(1)]);
    let next_block = BlockNumHash::new(second.number, second.hash);
    let items = LogValueStream::new(
        DEFAULT_PARAMS,
        ValueSpaceAnchor::new(first.number, first.hash, map_size - 2),
        [first],
        LogValueStreamTermination::BatchExhausted { next_block },
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();

    assert!(matches!(
        items.last(),
        Some(LogValueStreamItem::Complete(LogValueStreamCompletion::BatchExhausted {
            continuation: BatchContinuation { next_block: actual, next_log_value_index },
            ..
        })) if *actual == next_block && *next_log_value_index == map_size - 1
    ));
}
