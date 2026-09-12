use super::*;
use crate::{
    coverage::MapResumeAnchor, LogInput, LogValueKind, RendererError, ValueSpaceAnchor,
    DEFAULT_PARAMS, RANGE_TEST_PARAMS,
};
use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256};

fn hash(byte: u8) -> B256 {
    B256::repeat_byte(byte)
}

fn block(number: u64, logs: impl IntoIterator<Item = LogInput>) -> BlockInput {
    BlockInput::new(number, hash(number as u8), logs)
}

fn log(byte: u8, topics: impl IntoIterator<Item = B256>) -> LogInput {
    LogInput::new(Address::repeat_byte(byte), topics)
}

fn genesis_stream(
    blocks: Vec<BlockInput>,
    termination: LogValueStreamTermination,
) -> LogValueStream<std::vec::IntoIter<BlockInput>> {
    let genesis_hash = blocks[0].hash;
    LogValueStream::new(
        RANGE_TEST_PARAMS,
        ValueSpaceAnchor::new(0, genesis_hash, 0),
        blocks,
        termination,
    )
}

fn next_map<I: Iterator<Item = BlockInput>>(
    renderer: &mut FilterMapRenderer<I>,
) -> AnchoredCompletedMap {
    match renderer.render_next().expect("renderer output").expect("valid output") {
        RendererOutput::Map(map) => map,
        RendererOutput::Complete(_) => panic!("expected map"),
    }
}

#[test]
fn completed_maps_are_anchored_and_returned_one_per_pull() {
    let blocks = vec![block(0, [log(1, [])]), block(1, [])];
    let mut renderer = FilterMapRenderer::from_genesis(genesis_stream(
        blocks,
        LogValueStreamTermination::ReachedHead,
    ))
    .unwrap();

    let first = next_map(&mut renderer);
    assert_eq!(first.map().params_id(), ParamsId::RangeTest);
    assert_eq!(first.map().map_index(), 0);
    assert_eq!(first.map().epoch(), 0);
    assert_eq!(first.map().mark_count(), 1);
    assert_eq!(first.map().block_pointers()[0].block_number, 0);
    assert_eq!(first.resume_anchor().completed_map_index, 0);
    assert_eq!(first.map().last_block(), BlockNumHash::new(0, hash(0)));

    // The delimiter completes an empty map before block 1's pointer exists. That delayed pointer
    // anchors the older map and belongs exclusively to it.
    let second = next_map(&mut renderer);
    assert_eq!(second.map().map_index(), 1);
    assert!(second.map().rows().is_empty());
    assert_eq!(
        second.map().block_pointers().iter().map(|p| p.block_number).collect::<Vec<_>>(),
        [1]
    );
    assert_eq!(second.resume_anchor().resume_anchor(), ValueSpaceAnchor::new(1, hash(1), 2));

    assert!(matches!(
        renderer.render_next(),
        Some(Ok(RendererOutput::Complete(RendererCompletion::ReachedHead { .. })))
    ));
    assert!(renderer.render_next().is_none());
}

#[test]
fn every_typed_value_is_marked_regardless_of_kind_or_zero_bytes() {
    let input = block(0, []);
    let stream = LogValueStream::new(
        DEFAULT_PARAMS,
        ValueSpaceAnchor::new(0, input.hash, 0),
        vec![input],
        LogValueStreamTermination::ReachedHead,
    );
    let mut renderer = FilterMapRenderer::from_genesis(stream).unwrap();
    renderer
        .process_event(LogValueStreamEvent::BlockPointer(BlockPointer::new(0, hash(0), 0)))
        .unwrap();
    for (index, kind) in [
        (0, LogValueKind::Address),
        (1, LogValueKind::Topic { ordinal: 0 }),
        (2, LogValueKind::Topic { ordinal: 3 }),
    ] {
        renderer
            .process_event(LogValueStreamEvent::Slot(LogValueSlot::Value {
                index,
                value: B256::ZERO,
                kind,
            }))
            .unwrap();
    }
    assert_eq!(renderer.active.as_ref().unwrap().rows.mark_count(), 3);
}

#[test]
fn durable_resume_suppresses_the_published_prefix() {
    let genesis = block(0, [log(1, [])]);
    let child = block(1, []);
    let anchor = MapResumeAnchor::new(
        MapBoundary::new(0, 0, genesis.hash),
        BlockPointer::new(0, genesis.hash, 0),
    )
    .unwrap();
    let stream = LogValueStream::new(
        RANGE_TEST_PARAMS,
        anchor.resume_anchor(),
        vec![genesis, child],
        LogValueStreamTermination::ReachedHead,
    );
    let mut renderer = FilterMapRenderer::resume(stream, anchor).unwrap();
    let map = next_map(&mut renderer);
    assert_eq!(map.map().map_index(), 1);
    assert!(map.map().rows().is_empty());
    assert_eq!(map.map().block_pointers().iter().map(|p| p.block_number).collect::<Vec<_>>(), [1]);
}

#[test]
fn durable_resume_rejects_mismatched_starts_and_replay_jumps() {
    let pointer = BlockPointer::new(0, hash(0), 0);
    let anchor = MapResumeAnchor::new(MapBoundary::new(0, 0, hash(0)), pointer).unwrap();
    let stream = LogValueStream::new(
        RANGE_TEST_PARAMS,
        ValueSpaceAnchor::new(0, hash(0), 1),
        vec![block(0, [])],
        LogValueStreamTermination::ReachedHead,
    );
    assert!(matches!(
        FilterMapRenderer::resume(stream, anchor),
        Err(RendererError::StartAnchorMismatch { .. })
    ));

    let jumped_pointer = BlockPointer::new(0, hash(0), 2);
    let jumped_anchor =
        MapResumeAnchor::new(MapBoundary::new(0, 0, hash(0)), jumped_pointer).unwrap();
    let stream = LogValueStream::new(
        RANGE_TEST_PARAMS,
        jumped_anchor.resume_anchor(),
        vec![block(0, [log(1, [])])],
        LogValueStreamTermination::ReachedHead,
    );
    let mut renderer = FilterMapRenderer::resume(stream, jumped_anchor).unwrap();
    assert!(matches!(
        renderer.render_next(),
        Some(Err(RendererError::UnexpectedSlotIndex { expected: 1, actual: 2 }))
    ));
    assert!(renderer.render_next().is_none());
}

#[test]
fn bounded_continuation_preserves_active_state() {
    let first = block(0, [log(1, [])]);
    let successor = block(1, [log(2, [])]);
    let termination = LogValueStreamTermination::BatchExhausted {
        next_block: BlockNumHash::new(successor.number, successor.hash),
    };
    let mut renderer =
        FilterMapRenderer::from_genesis(genesis_stream(vec![first], termination)).unwrap();
    let _value_map = next_map(&mut renderer);
    let continuation = match renderer.render_next().unwrap().unwrap() {
        RendererOutput::Complete(RendererCompletion::BatchExhausted(continuation)) => continuation,
        other => panic!("unexpected output: {other:?}"),
    };
    assert_eq!(continuation.next_block(), BlockNumHash::new(1, hash(1)));
    let mut continued =
        continuation.continue_with([successor], LogValueStreamTermination::ReachedHead).unwrap();
    let delayed = next_map(&mut continued);
    assert_eq!(delayed.map().map_index(), 1);
    assert!(delayed.map().rows().is_empty());
    let map = next_map(&mut continued);
    assert_eq!(map.map().map_index(), 2);
    assert_eq!(map.map().mark_count(), 1);
    assert!(matches!(
        continued.render_next(),
        Some(Ok(RendererOutput::Complete(RendererCompletion::ReachedHead { .. })))
    ));
}

#[derive(Debug, PartialEq, Eq)]
struct MapSummary {
    index: u32,
    rows: Vec<(u32, Vec<u32>)>,
    pointers: Vec<BlockPointer>,
    boundary: MapBoundary,
    anchor: MapResumeAnchor,
}

fn summarize(map: AnchoredCompletedMap) -> MapSummary {
    MapSummary {
        index: map.map().map_index(),
        rows: map
            .map()
            .rows()
            .iter()
            .map(|row| (row.row_index(), row.columns().to_vec()))
            .collect(),
        pointers: map.map().block_pointers().to_vec(),
        boundary: map.map().boundary(),
        anchor: map.resume_anchor(),
    }
}

#[test]
fn splitting_at_every_block_boundary_matches_one_shot_rendering() {
    let blocks = (0..4).map(|number| block(number, [log(number as u8, [])])).collect::<Vec<_>>();
    let mut one_shot = FilterMapRenderer::from_genesis(genesis_stream(
        blocks.clone(),
        LogValueStreamTermination::ReachedHead,
    ))
    .unwrap();
    let mut expected = Vec::new();
    loop {
        match one_shot.render_next().unwrap().unwrap() {
            RendererOutput::Map(map) => expected.push(summarize(map)),
            RendererOutput::Complete(RendererCompletion::ReachedHead { .. }) => break,
            RendererOutput::Complete(RendererCompletion::BatchExhausted(_)) => unreachable!(),
        }
    }

    let mut split = FilterMapRenderer::from_genesis(genesis_stream(
        vec![blocks[0].clone()],
        LogValueStreamTermination::BatchExhausted {
            next_block: BlockNumHash::new(blocks[1].number, blocks[1].hash),
        },
    ))
    .unwrap();
    let mut actual = Vec::new();
    for position in 0..blocks.len() {
        let continuation = loop {
            match split.render_next().unwrap().unwrap() {
                RendererOutput::Map(map) => actual.push(summarize(map)),
                RendererOutput::Complete(RendererCompletion::BatchExhausted(continuation)) => {
                    break Some(continuation)
                }
                RendererOutput::Complete(RendererCompletion::ReachedHead { .. }) => break None,
            }
        };
        let Some(continuation) = continuation else { break };
        let next = position + 1;
        let termination = if next + 1 == blocks.len() {
            LogValueStreamTermination::ReachedHead
        } else {
            LogValueStreamTermination::BatchExhausted {
                next_block: BlockNumHash::new(blocks[next + 1].number, blocks[next + 1].hash),
            }
        };
        split = continuation.continue_with(vec![blocks[next].clone()], termination).unwrap();
    }
    assert_eq!(actual, expected);
}

#[test]
fn wrong_continuation_successor_errors_once_and_fuses() {
    let first = block(0, []);
    let next = block(1, []);
    let mut renderer = FilterMapRenderer::from_genesis(genesis_stream(
        vec![first],
        LogValueStreamTermination::BatchExhausted {
            next_block: BlockNumHash::new(next.number, next.hash),
        },
    ))
    .unwrap();
    let continuation = match renderer.render_next().unwrap().unwrap() {
        RendererOutput::Complete(RendererCompletion::BatchExhausted(continuation)) => continuation,
        other => panic!("unexpected output: {other:?}"),
    };
    let wrong = BlockInput::new(1, hash(9), []);
    let mut continued =
        continuation.continue_with([wrong], LogValueStreamTermination::ReachedHead).unwrap();
    assert!(matches!(continued.render_next(), Some(Err(RendererError::Stream(_)))));
    assert!(continued.render_next().is_none());
}

#[test]
fn many_maps_are_returned_incrementally_in_strict_order() {
    let blocks = (0..100).map(|number| block(number, [log(number as u8, [])])).collect::<Vec<_>>();
    let mut renderer = FilterMapRenderer::from_genesis(genesis_stream(
        blocks,
        LogValueStreamTermination::ReachedHead,
    ))
    .unwrap();
    for expected in 0..199 {
        let map = next_map(&mut renderer);
        assert_eq!(map.map().map_index(), expected);
    }
    assert!(matches!(
        renderer.render_next(),
        Some(Ok(RendererOutput::Complete(RendererCompletion::ReachedHead { .. })))
    ));
    assert!(renderer.render_next().is_none());
}

#[test]
fn retained_state_stays_bounded_across_many_batches() {
    let first = block(0, []);
    let next = block(1, []);
    let mut renderer = FilterMapRenderer::from_genesis(genesis_stream(
        vec![first],
        LogValueStreamTermination::BatchExhausted {
            next_block: BlockNumHash::new(next.number, next.hash),
        },
    ))
    .unwrap();
    let mut continuation = match renderer.render_next().unwrap().unwrap() {
        RendererOutput::Complete(RendererCompletion::BatchExhausted(continuation)) => continuation,
        other => panic!("unexpected output: {other:?}"),
    };

    for number in 1..20 {
        assert!(continuation.state.pending.is_some());
        assert!(continuation.state.active.as_ref().unwrap().block_pointers.is_empty());
        let current = block(number, []);
        let successor = block(number + 1, []);
        let mut renderer = continuation
            .continue_with(
                [current],
                LogValueStreamTermination::BatchExhausted {
                    next_block: BlockNumHash::new(successor.number, successor.hash),
                },
            )
            .unwrap();
        let map = next_map(&mut renderer);
        assert_eq!(map.map().map_index(), u32::try_from(number - 1).unwrap());
        continuation = match renderer.render_next().unwrap().unwrap() {
            RendererOutput::Complete(RendererCompletion::BatchExhausted(continuation)) => {
                continuation
            }
            other => panic!("unexpected output: {other:?}"),
        };
    }
}

#[test]
fn malformed_event_errors_follow_protocol_precedence() {
    let mut renderer = FilterMapRenderer::from_genesis(genesis_stream(
        vec![block(0, [])],
        LogValueStreamTermination::ReachedHead,
    ))
    .unwrap();
    let pointer = BlockPointer::new(0, hash(0), 0);
    assert!(renderer.process_event(LogValueStreamEvent::BlockPointer(pointer)).unwrap().is_none());
    assert!(matches!(
        renderer.process_event(LogValueStreamEvent::BlockPointer(pointer)),
        Err(RendererError::DuplicatePointer { .. })
    ));

    let mut renderer = FilterMapRenderer::from_genesis(genesis_stream(
        vec![block(0, [])],
        LogValueStreamTermination::ReachedHead,
    ))
    .unwrap();
    let slot = LogValueSlot::Value { index: 1, value: B256::ZERO, kind: LogValueKind::Address };
    assert!(matches!(
        renderer.process_event(LogValueStreamEvent::Slot(slot)),
        Err(RendererError::UnexpectedSlotIndex { expected: 0, actual: 1 })
    ));

    let mut renderer = FilterMapRenderer::from_genesis(genesis_stream(
        vec![block(0, [log(1, [])])],
        LogValueStreamTermination::ReachedHead,
    ))
    .unwrap();
    renderer
        .process_event(LogValueStreamEvent::BlockPointer(BlockPointer::new(0, hash(0), 0)))
        .unwrap();
    renderer
        .process_event(LogValueStreamEvent::Slot(LogValueSlot::Value {
            index: 0,
            value: B256::ZERO,
            kind: LogValueKind::Address,
        }))
        .unwrap();
    assert!(matches!(
        renderer.process_event(LogValueStreamEvent::MapBoundary(MapBoundary::new(0, 0, hash(9),))),
        Err(RendererError::BoundaryPointerMismatch { .. })
    ));
}

#[test]
fn constructors_reject_invalid_starts_without_advancing_input() {
    let actual = ValueSpaceAnchor::new(1, hash(1), 0);
    let stream = LogValueStream::new(
        RANGE_TEST_PARAMS,
        actual,
        vec![block(1, [])],
        LogValueStreamTermination::ReachedHead,
    );
    assert!(matches!(
        FilterMapRenderer::from_genesis(stream),
        Err(RendererError::InvalidGenesisStart { actual: found }) if found == actual
    ));

    let continuation = BatchContinuation::new(BlockNumHash::new(1, hash(1)), 1);
    let stream = LogValueStream::continue_from(
        RANGE_TEST_PARAMS,
        continuation,
        vec![block(1, [])],
        LogValueStreamTermination::ReachedHead,
    );
    assert!(matches!(
        FilterMapRenderer::from_genesis(stream),
        Err(RendererError::GenesisFromContinuation { actual }) if actual == continuation
    ));
}

fn fixture_blocks(fixture: &crate::golden_pipeline::parser::Fixture) -> Vec<BlockInput> {
    fixture
        .blocks
        .iter()
        .map(|block| {
            let logs = block
                .receipts
                .iter()
                .flat_map(|receipt| &receipt.logs)
                .map(|log| LogInput::new(log.address, log.topics.iter().copied()));
            BlockInput::new(block.number, block.hash, logs)
        })
        .collect()
}

fn fixture_stream(
    fixture: &crate::golden_pipeline::parser::Fixture,
) -> LogValueStream<std::vec::IntoIter<BlockInput>> {
    use crate::golden_pipeline::parser::{Origin, Termination};

    let params = fixture.params_name.params();
    let blocks = fixture_blocks(fixture);
    let termination = match fixture.termination {
        Termination::Head => LogValueStreamTermination::ReachedHead,
        Termination::Batch { next_block, next_hash } => LogValueStreamTermination::BatchExhausted {
            next_block: BlockNumHash::new(next_block, next_hash),
        },
    };
    match fixture.origin {
        Origin::Genesis(origin) | Origin::Checkpoint(origin) => LogValueStream::new(
            params,
            ValueSpaceAnchor::new(origin.block, origin.hash, origin.index),
            blocks,
            termination,
        ),
        Origin::Continuation { block, hash, cursor, .. } => LogValueStream::continue_from(
            params,
            BatchContinuation::new(BlockNumHash::new(block, hash), cursor),
            blocks,
            termination,
        ),
    }
}

pub(crate) fn fixture_renderer(
    fixture: &crate::golden_pipeline::parser::Fixture,
) -> FilterMapRenderer<std::vec::IntoIter<BlockInput>> {
    use crate::golden_pipeline::parser::Origin;

    let stream = fixture_stream(fixture);
    let params = stream.params();
    let expected_slot_index = stream.initial_cursor();
    let map_index = u32::try_from(expected_slot_index / params.values_per_map()).unwrap();
    let previous = match fixture.origin {
        Origin::Continuation { previous, .. } => {
            Some(BlockPointer::new(previous.block, previous.hash, previous.index))
        }
        _ => None,
    };
    FilterMapRenderer {
        params_id: ParamsId::of(&params).unwrap(),
        params,
        stream,
        expected_slot_index,
        phase: Phase::Active,
        active: Some(ActiveMap::new(map_index)),
        pending: None,
        pending_replay_boundary: None,
        last_pointer: previous,
        latest_pointer: previous,
        next_output_map_index: map_index,
    }
}

#[test]
fn every_format_two_fixture_matches_the_production_event_machine() {
    use crate::golden_pipeline::parser::{Origin, Termination};

    for (entry, fixture) in crate::golden_pipeline::manifest::load_and_validate_corpus().unwrap() {
        let mut renderer = fixture_renderer(&fixture);
        let mut maps = Vec::new();
        let mut first_pointer = true;
        let completion = loop {
            let item = renderer.stream.next().expect("fixture stream completion").unwrap();
            match item {
                LogValueStreamItem::Event(event) => {
                    let suppress_checkpoint_pointer = first_pointer &&
                        matches!(fixture.origin, Origin::Checkpoint(origin) if origin.index != 0);
                    let output = if suppress_checkpoint_pointer {
                        let LogValueStreamEvent::BlockPointer(pointer) = event else {
                            panic!("{}: checkpoint stream did not begin with a pointer", entry.path)
                        };
                        first_pointer = false;
                        renderer.accept_pointer(pointer, false).unwrap()
                    } else {
                        if matches!(event, LogValueStreamEvent::BlockPointer(_)) {
                            first_pointer = false;
                        }
                        renderer.process_event(event).unwrap()
                    };
                    if let Some((map, anchor)) = output {
                        maps.push((map, anchor));
                    }
                }
                LogValueStreamItem::Complete(completion) => break completion,
            }
        };

        assert_eq!(maps.len(), fixture.completed_maps.len(), "{}", entry.path);
        for ((actual, anchor), expected) in maps.iter().zip(&fixture.completed_maps) {
            assert_eq!(
                actual.params_id(),
                ParamsId::of(&fixture.params_name.params()).unwrap(),
                "{}",
                entry.path
            );
            assert_eq!(actual.map_index(), expected.index, "{}", entry.path);
            assert_eq!(actual.epoch(), expected.epoch, "{}", entry.path);
            assert_eq!(actual.mark_count(), expected.mark_count, "{}", entry.path);
            assert_eq!(
                actual.last_block(),
                BlockNumHash::new(expected.last_block, expected.last_hash),
                "{}",
                entry.path
            );
            assert_eq!(actual.boundary().completed_map_index, expected.index, "{}", entry.path);
            assert_eq!(
                actual
                    .block_pointers()
                    .iter()
                    .map(|pointer| pointer.block_number)
                    .collect::<Vec<_>>(),
                expected.pointer_blocks,
                "{}",
                entry.path,
            );
            assert_eq!(anchor.pointer.block_number, expected.boundary.block, "{}", entry.path);
            assert_eq!(anchor.pointer.block_hash, expected.boundary.hash, "{}", entry.path);
            assert_eq!(
                anchor.pointer.first_log_value_index, expected.boundary.index,
                "{}",
                entry.path
            );
            assert_eq!(actual.rows().len(), expected.rows.len(), "{}", entry.path);
            for (actual, expected) in actual.rows().iter().zip(&expected.rows) {
                assert_eq!(actual.row_index(), expected.index, "{}", entry.path);
                assert_eq!(actual.columns(), expected.columns, "{}", entry.path);
            }
        }

        match fixture.partial_maps.as_slice() {
            [] => assert!(
                renderer.expected_slot_index.is_multiple_of(renderer.params.values_per_map()),
                "{}",
                entry.path
            ),
            [expected] => {
                let active = renderer.active.as_mut().expect("fixture partial active map");
                assert_eq!(active.map_index, expected.index, "{}", entry.path);
                let rows = std::mem::replace(&mut active.rows, ActiveRows::new()).finish();
                assert_eq!(
                    rows.iter().map(|row| row.columns().len()).sum::<usize>(),
                    expected.mark_count,
                    "{}",
                    entry.path
                );
                assert_eq!(rows.len(), expected.rows.len(), "{}", entry.path);
                for (actual, expected) in rows.iter().zip(&expected.rows) {
                    assert_eq!(actual.row_index(), expected.index, "{}", entry.path);
                    assert_eq!(actual.columns(), expected.columns, "{}", entry.path);
                }
                let (last_block, last_hash, cursor) = match completion {
                    LogValueStreamCompletion::ReachedHead { head, pending_delimiter } => {
                        (head.block_number, head.block_hash, pending_delimiter.index)
                    }
                    LogValueStreamCompletion::BatchExhausted { last_block, continuation } => (
                        last_block.block_number,
                        last_block.block_hash,
                        continuation.next_log_value_index,
                    ),
                };
                assert_eq!(
                    (last_block, last_hash, cursor),
                    (expected.last_block, expected.last_hash, expected.pending_delimiter),
                    "{}",
                    entry.path
                );
            }
            _ => panic!("{}: several private partial maps", entry.path),
        }

        // Exercise terminal validation after observing private state. Partial rows were moved only
        // for test comparison; terminal handling never publishes them.
        if fixture.partial_maps.is_empty() {
            let result = renderer.process_completion(completion);
            assert!(result.is_ok(), "{}: {result:?}", entry.path);
        } else {
            assert!(matches!(fixture.termination, Termination::Head | Termination::Batch { .. }));
        }
    }
}
