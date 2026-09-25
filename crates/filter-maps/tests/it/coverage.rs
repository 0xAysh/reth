//! Coverage-contract tests driven by the real log value stream.
//!
//! These tests derive durable anchors from actual stream events and check that the coverage model
//! claims exactly the blocks whose delimiters the stream has materialized inside completed maps.
//! Coverage evolves the way the store drives it, as structurally restored coverage, and becomes
//! visible only through canonical activation.
//! [`RANGE_TEST_PARAMS`] gives one slot per map, so every slot completes a map and every block
//! with logs spans several maps.

use alloy_primitives::{Address, B256};
use reth_filter_maps::{
    coverage::{
        CandidateSource, IndexIdentity, LogQueryTarget, PlannedSubrange, QueryPlan,
        QueryableCoverage, RejectUnrecognizedCheckpoints, SegmentOrigin, StoredCoverageRecord,
        StructurallyRestoredCoverage, STORAGE_FORMAT_V1,
    },
    BlockInput, BlockPointer, IndexedMatchRange, LogInput, LogValueSlot, LogValueStream,
    LogValueStreamCompletion, LogValueStreamEvent, LogValueStreamItem, LogValueStreamTermination,
    MapBoundary, MapResumeAnchor, ParamsId, GETH_V1, RANGE_TEST_PARAMS,
};
use std::{
    collections::{BTreeMap, HashMap},
    convert::Infallible,
};

const fn hash(number: u64) -> B256 {
    B256::repeat_byte(number as u8 + 1)
}

const fn identity() -> IndexIdentity {
    IndexIdentity::new(STORAGE_FORMAT_V1, 1, hash(0), GETH_V1, ParamsId::RangeTest)
}

fn block(number: u64, log_count: usize) -> BlockInput {
    // Every log is topic-free: under the range-test parameters a map holds one slot, so a wider log
    // could never fit.
    BlockInput::new(
        number,
        hash(number),
        (0..log_count).map(|_| LogInput::new(Address::repeat_byte(number as u8), [])),
    )
}

/// Blocks 0..=5: empty blocks, single-log blocks, and multi-log blocks.
///
/// ```text
/// map:   0  1  2  3  4  5  6  7  8  9  10 11 12
/// slot:  d0 a1 d1 d2 a3 a3 d3 a4 a4 a4 d4 a5 d5
/// ```
fn chain() -> Vec<BlockInput> {
    vec![block(0, 0), block(1, 1), block(2, 0), block(3, 2), block(4, 3), block(5, 1)]
}

/// A durable anchor plus the number of block delimiters materialized in maps through its map.
#[derive(Debug, Clone, Copy)]
struct Observed {
    anchor: MapResumeAnchor,
    delimiters: u64,
}

/// Drives a stream from `anchor` and pairs every boundary with its resume block's pointer.
fn observe(anchor: BlockPointer, blocks: Vec<BlockInput>, next_block: u64) -> Vec<Observed> {
    observe_with_pointers(anchor, blocks, next_block).0
}

/// Like [`observe`], also returning every block pointer the stream established, which is what a
/// publication of these maps would store.
fn observe_with_pointers(
    anchor: BlockPointer,
    blocks: Vec<BlockInput>,
    next_block: u64,
) -> (Vec<Observed>, BTreeMap<u64, BlockPointer>) {
    let termination = LogValueStreamTermination::BatchExhausted {
        next_block: alloy_eips::BlockNumHash::new(next_block, hash(next_block)),
    };
    let stream = LogValueStream::new(RANGE_TEST_PARAMS, anchor, blocks, termination);

    let mut pointers: HashMap<u64, BlockPointer> = HashMap::new();
    let mut pending: Option<(MapBoundary, u64)> = None;
    let mut delimiters = 0;
    let mut observed = Vec::new();
    for item in stream {
        let event = match item.unwrap() {
            LogValueStreamItem::Event(event) => event,
            LogValueStreamItem::Complete(LogValueStreamCompletion::BatchExhausted {
                continuation,
                ..
            }) => {
                // A map completed by the batch's final delimiter names the successor block. Its
                // pointer is the continuation index: the cursor sits at a map start, where no
                // padding can precede the next block's first log.
                if let Some((boundary, count)) = pending.take() {
                    assert_eq!(boundary.resume_block_number, continuation.next_block.number);
                    let pointer = BlockPointer::new(
                        continuation.next_block.number,
                        continuation.next_block.hash,
                        continuation.next_log_value_index,
                    );
                    let anchor = MapResumeAnchor::new(boundary, pointer).unwrap();
                    observed.push(Observed { anchor, delimiters: count });
                    pointers.insert(pointer.block_number, pointer);
                }
                break
            }
            LogValueStreamItem::Complete(LogValueStreamCompletion::ReachedHead { .. }) => break,
        };
        match event {
            LogValueStreamEvent::BlockPointer(pointer) => {
                pointers.insert(pointer.block_number, pointer);
                if let Some((boundary, count)) = pending.take() {
                    assert_eq!(boundary.resume_block_number, pointer.block_number);
                    let anchor = MapResumeAnchor::new(boundary, pointer).unwrap();
                    observed.push(Observed { anchor, delimiters: count });
                }
            }
            LogValueStreamEvent::Slot(LogValueSlot::BlockDelimiter { .. }) => delimiters += 1,
            LogValueStreamEvent::Slot(_) => {}
            LogValueStreamEvent::MapBoundary(boundary) => {
                match pointers.get(&boundary.resume_block_number) {
                    Some(&pointer) => {
                        let anchor = MapResumeAnchor::new(boundary, pointer).unwrap();
                        observed.push(Observed { anchor, delimiters });
                    }
                    None => pending = Some((boundary, delimiters)),
                }
            }
        }
    }
    assert!(pending.is_none(), "every boundary must be paired before the batch ends");
    (observed, pointers.into_iter().collect())
}

fn genesis() -> (Vec<Observed>, BTreeMap<u64, BlockPointer>) {
    observe_with_pointers(BlockPointer::new(0, hash(0), 0), chain(), 6)
}

fn genesis_anchors() -> Vec<Observed> {
    genesis().0
}

/// Coverage of a freshly initialized store.
fn empty() -> StructurallyRestoredCoverage {
    StructurallyRestoredCoverage::restore(
        &identity(),
        StoredCoverageRecord { identity: identity(), segments: Vec::new() },
        [],
        &mut RejectUnrecognizedCheckpoints,
    )
    .unwrap()
}

/// Activates `coverage` on the fixture chain with the pointers genesis construction stored. A
/// checkpoint enters the same value space, so those pointers serve every segment.
fn activate(coverage: &StructurallyRestoredCoverage) -> QueryableCoverage {
    let pointers = genesis().1;
    coverage
        .activate(
            |number| Ok::<_, Infallible>(Some(hash(number))),
            |number| Ok::<_, Infallible>(pointers.get(&number).copied()),
        )
        .unwrap()
}

fn genesis_through(
    anchors: impl IntoIterator<Item = MapResumeAnchor>,
) -> StructurallyRestoredCoverage {
    empty().open_segment(SegmentOrigin::Genesis, anchors).unwrap()
}

fn checkpoint(anchor: MapResumeAnchor) -> SegmentOrigin {
    let anchors: Vec<_> = genesis_anchors()
        .into_iter()
        .map(|observed| observed.anchor)
        .take_while(|candidate| candidate.completed_map_index <= anchor.completed_map_index)
        .collect();
    assert_eq!(anchors.last(), Some(&anchor), "checkpoint must have been observed");
    let published = activate(&genesis_through(anchors));
    SegmentOrigin::Checkpoint(published.derived_checkpoint(anchor).unwrap())
}

#[test]
fn anchors_claim_exactly_the_blocks_whose_delimiters_completed_maps() {
    let observed = genesis_anchors();
    assert!(observed.len() > 10, "every slot completes a map under the range-test parameters");
    for Observed { anchor, delimiters } in observed {
        assert_eq!(
            anchor.covered_through(),
            delimiters.checked_sub(1),
            "anchor {anchor:?} claims coverage its maps do not hold"
        );
    }
}

#[test]
fn a_block_spanning_maps_is_not_visible_early() {
    let observed = genesis_anchors();
    let mut coverage = empty();
    let mut previous: Option<MapResumeAnchor> = None;
    for Observed { anchor, delimiters } in observed {
        coverage = match previous {
            None => coverage.open_segment(SegmentOrigin::Genesis, [anchor]).unwrap(),
            Some(from) => coverage.extend(from, [anchor]).unwrap(),
        };
        previous = Some(anchor);
        let visible = activate(&coverage);
        for number in 0..=6 {
            assert_eq!(
                visible.covers(number),
                number < delimiters,
                "after map {}, block {number} visibility is wrong",
                anchor.completed_map_index
            );
        }
    }
    // The bounded batch materialized block 5's delimiter, so all six blocks are covered.
    assert_eq!(coverage.segments()[0].blocks(), Some(0..=5));
}

#[test]
fn a_map_ending_mid_block_does_not_publish_that_block() {
    let observed = genesis_anchors();
    // Block 3 has two logs; the map completed by its first address slot resumes at block 3 itself,
    // which then still holds slots beyond the completed map.
    let mid_block = observed
        .iter()
        .find(|o| {
            o.anchor.pointer.block_number == 3 && !o.anchor.excludes_block(3, &RANGE_TEST_PARAMS)
        })
        .expect("a map completes inside block 3");
    assert_eq!(mid_block.anchor.completed_map_index, 4);
    assert_eq!(mid_block.delimiters, 3);
    let coverage =
        genesis_through(observed.iter().map(|observed| observed.anchor).take_while(|anchor| {
            anchor.completed_map_index <= mid_block.anchor.completed_map_index
        }));
    assert_eq!(coverage.segments()[0].blocks(), Some(0..=2));
    assert!(!activate(&coverage).covers(3));
    assert!(!coverage.segments()[0].terminal().excludes_block(3, &RANGE_TEST_PARAMS));
}

#[test]
fn a_checkpoint_segment_joins_genesis_construction_only_at_the_same_anchor() {
    let observed = genesis_anchors();
    // Checkpoint after map 4, inside block 3, so the join straddles a block.
    let join_at = observed
        .iter()
        .position(|o| {
            o.anchor.pointer.block_number == 3 && !o.anchor.excludes_block(3, &RANGE_TEST_PARAMS)
        })
        .unwrap();
    let join = observed[join_at].anchor;
    assert_eq!(join.completed_map_index, 4);

    // Construction from the checkpoint reproduces the same anchors as genesis construction, the
    // join anchor included: the resumed block re-renders its slot in map 4.
    let from_checkpoint = observe(join.pointer, chain()[3..].to_vec(), 6);
    let expected: Vec<_> = observed[join_at..].iter().map(|o| o.anchor).collect();
    let reproduced: Vec<_> = from_checkpoint.iter().map(|o| o.anchor).collect();
    assert_eq!(reproduced, expected, "a checkpoint enters the same value space");

    let coverage = empty()
        .open_segment(
            checkpoint(join),
            from_checkpoint.iter().skip(1).map(|observed| observed.anchor),
        )
        .unwrap();
    // Block 3 straddles the checkpoint map and is excluded until both halves are published.
    assert_eq!(coverage.segments()[0].blocks(), Some(4..=5));
    let coverage = coverage
        .open_segment(
            SegmentOrigin::Genesis,
            observed[..join_at].iter().map(|observed| observed.anchor),
        )
        .unwrap();
    assert_eq!(coverage.segments().len(), 2, "not adjacent yet");
    assert!(!activate(&coverage).covers(3));

    let coverage = coverage.extend(observed[join_at - 1].anchor, [join]).unwrap();
    assert_eq!(coverage.segments().len(), 1);
    assert_eq!(coverage.segments()[0].blocks(), Some(0..=5));
    assert_eq!(coverage.segments()[0].origin(), &SegmentOrigin::Genesis);
    assert!(activate(&coverage).covers(3));
}

#[test]
fn reorg_contracts_to_a_map_that_excludes_the_changed_block_and_retention_hides_the_tail() {
    let observed = genesis_anchors();
    let coverage = genesis_through(observed.iter().map(|observed| observed.anchor));
    assert_eq!(coverage.segments()[0].blocks(), Some(0..=5));

    // Block 4 changed. Anchors resuming inside block 4 are unsafe; the last anchor whose maps hold
    // only earlier blocks is the one to contract to.
    let safe = observed
        .iter()
        .rev()
        .map(|o| o.anchor)
        .find(|anchor| anchor.excludes_block(4, &RANGE_TEST_PARAMS))
        .unwrap();
    assert_eq!(safe.pointer.block_number, 4, "block 4's first map begins right after it");
    let unsafe_anchor = observed
        .iter()
        .map(|o| o.anchor)
        .find(|a| a.completed_map_index > safe.completed_map_index)
        .unwrap();
    assert!(coverage.contract_for_reorg(4, Some(unsafe_anchor)).is_err());

    let (coverage, outcome) = coverage.contract_for_reorg(4, Some(safe)).unwrap();
    assert_eq!(outcome.rebuild_from, Some(safe));
    assert_eq!(coverage.segments()[0].blocks(), Some(0..=3));
    assert!(!activate(&coverage).covers(4));

    // Retention: drop everything through the map that ends block 1.
    let tail = observed.iter().map(|o| o.anchor).find(|a| a.pointer.block_number == 2).unwrap();
    let coverage = coverage.retain_after(tail).unwrap();
    let visible = activate(&coverage);
    assert!(!visible.covers(1));
    assert!(visible.covers(2));
    assert_eq!(coverage.segments()[0].blocks(), Some(2..=3));
}

#[test]
fn query_plan_partitions_around_canonically_activated_segments() {
    let observed = genesis_anchors();
    let end = observed.iter().find(|o| o.delimiters == 4).unwrap().anchor;
    let coverage = genesis_through(
        observed
            .iter()
            .map(|observed| observed.anchor)
            .take_while(|anchor| anchor.completed_map_index <= end.completed_map_index),
    );
    let queryable = activate(&coverage);
    assert_eq!(queryable.segments()[0].blocks(), Some(0..=3));

    let plan = QueryPlan::new(LogQueryTarget::Range { from: 2, to: 5 }, true, &queryable, "head-a")
        .unwrap();
    let CandidateSource::Partitioned(subranges) = plan.source() else { panic!() };
    assert_eq!(
        subranges,
        &[
            PlannedSubrange::Indexed(IndexedMatchRange::new(
                2..=3,
                0..=end.completed_map_index,
                ParamsId::RangeTest
            )),
            PlannedSubrange::Bloom { blocks: 4..=5 },
        ]
    );
    assert!(plan.revalidate(&"head-b").is_err());
}
