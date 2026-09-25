//! Coverage-contract tests driven by the real renderer.
//!
//! These tests publish the renderer's anchored completed maps and check that the coverage model
//! claims exactly the blocks whose delimiters the stream has materialized inside completed maps.
//! Coverage evolves the way the store drives it, through publication proposals over structurally
//! restored coverage, and becomes visible only through canonical activation.
//! [`RANGE_TEST_PARAMS`] gives one slot per map, so every slot completes a map and every block
//! with logs spans several maps.

use alloy_primitives::{Address, B256};
use reth_filter_maps::{
    coverage::{
        CandidateSource, IndexIdentity, LogQueryTarget, PlannedSubrange, PublicationProposal,
        PublicationStart, QueryPlan, QueryableCoverage, RejectUnrecognizedCheckpoints,
        SegmentOrigin, StoredCoverageRecord, StructurallyRestoredCoverage, STORAGE_FORMAT_V1,
    },
    AnchoredCompletedMap, BlockInput, BlockPointer, FilterMapRenderer, IndexedMatchRange, LogInput,
    LogValueSlot, LogValueStream, LogValueStreamEvent, LogValueStreamItem,
    LogValueStreamTermination, MapResumeAnchor, ParamsId, RendererCompletion, RendererOutput,
    GETH_V1, RANGE_TEST_PARAMS,
};
use std::{collections::BTreeMap, convert::Infallible};

/// Empty head block that follows the bounded batch of [`chain`].
const HEAD: u64 = 6;

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

/// Streams `blocks` from `start` as one bounded batch that ends before [`HEAD`].
fn batch(
    start: BlockPointer,
    blocks: Vec<BlockInput>,
) -> LogValueStream<std::vec::IntoIter<BlockInput>> {
    let termination = LogValueStreamTermination::BatchExhausted {
        next_block: alloy_eips::BlockNumHash::new(HEAD, hash(HEAD)),
    };
    LogValueStream::new(RANGE_TEST_PARAMS, start, blocks, termination)
}

/// Renders the batch, then continues it over the head block.
///
/// The map completed by block 5's delimiter names the head as its resume block, so the renderer
/// anchors it only once the continuation supplies the head's pointer.
fn render(
    mut renderer: FilterMapRenderer<std::vec::IntoIter<BlockInput>>,
) -> Vec<AnchoredCompletedMap> {
    let mut maps = Vec::new();
    loop {
        match renderer.render_next().unwrap().unwrap() {
            RendererOutput::Map(map) => maps.push(map),
            RendererOutput::Complete(RendererCompletion::BatchExhausted(continuation)) => {
                renderer = continuation
                    .continue_with(vec![block(HEAD, 0)], LogValueStreamTermination::ReachedHead)
                    .unwrap();
            }
            RendererOutput::Complete(RendererCompletion::ReachedHead { .. }) => return maps,
        }
    }
}

fn genesis_maps() -> Vec<AnchoredCompletedMap> {
    let stream = batch(BlockPointer::new(0, hash(0), 0), chain());
    render(FilterMapRenderer::from_genesis(stream).unwrap())
}

/// Counts the block delimiters the stream materializes in maps through `map_index`.
fn delimiters_through(map_index: u32) -> u64 {
    let maps_end = (u64::from(map_index) + 1) * RANGE_TEST_PARAMS.values_per_map();
    let delimiters = batch(BlockPointer::new(0, hash(0), 0), chain()).filter(|item| {
        matches!(
            item.as_ref().unwrap(),
            LogValueStreamItem::Event(LogValueStreamEvent::Slot(
                LogValueSlot::BlockDelimiter { index, .. }
            )) if *index < maps_end
        )
    });
    delimiters.count() as u64
}

/// Every block pointer the rendered maps established, which is what publishing them stores.
fn pointers(maps: &[AnchoredCompletedMap]) -> BTreeMap<u64, BlockPointer> {
    maps.iter()
        .flat_map(|anchored| {
            let map = anchored.map().block_pointers().iter().copied();
            map.chain(std::iter::once(anchored.resume_anchor().pointer))
        })
        .map(|pointer| (pointer.block_number, pointer))
        .collect()
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

/// Returns the coverage after publishing `maps` from `start`.
fn publish(
    coverage: &StructurallyRestoredCoverage,
    start: &PublicationStart,
    maps: &[AnchoredCompletedMap],
) -> StructurallyRestoredCoverage {
    match coverage.propose(start, maps).unwrap() {
        PublicationProposal::Next { coverage, .. } => coverage,
        PublicationProposal::AlreadyPublished { .. } => panic!("expected new coverage"),
    }
}

/// Activates `coverage` on the fixture chain with the pointers genesis construction stored. A
/// checkpoint enters the same value space, so those pointers serve every segment.
fn activate(coverage: &StructurallyRestoredCoverage) -> QueryableCoverage {
    let pointers = pointers(&genesis_maps());
    coverage
        .activate(
            |number| Ok::<_, Infallible>(Some(hash(number))),
            |number| Ok::<_, Infallible>(pointers.get(&number).copied()),
        )
        .unwrap()
}

fn genesis_through(maps: &[AnchoredCompletedMap]) -> StructurallyRestoredCoverage {
    publish(&empty(), &PublicationStart::Open { origin: SegmentOrigin::Genesis }, maps)
}

fn checkpoint(anchor: MapResumeAnchor) -> SegmentOrigin {
    let maps = genesis_maps();
    let through = maps
        .iter()
        .position(|map| map.resume_anchor() == anchor)
        .expect("checkpoint must have been rendered");
    let published = activate(&genesis_through(&maps[..=through]));
    SegmentOrigin::Checkpoint(published.derived_checkpoint(anchor).unwrap())
}

/// Returns the position of the map that completes inside block 3, which then still holds slots
/// beyond that map.
fn mid_block_3(maps: &[AnchoredCompletedMap]) -> usize {
    maps.iter()
        .position(|map| {
            let anchor = map.resume_anchor();
            anchor.pointer.block_number == 3 && !anchor.excludes_block(3, &RANGE_TEST_PARAMS)
        })
        .expect("a map completes inside block 3")
}

#[test]
fn anchors_claim_exactly_the_blocks_whose_delimiters_completed_maps() {
    let maps = genesis_maps();
    assert_eq!(maps.len(), 13, "every slot completes a map under the range-test parameters");
    for map in &maps {
        let anchor = map.resume_anchor();
        assert_eq!(
            anchor.covered_through(),
            delimiters_through(anchor.completed_map_index).checked_sub(1),
            "anchor {anchor:?} claims coverage its maps do not hold"
        );
    }
}

#[test]
fn a_block_spanning_maps_is_not_visible_early() {
    let mut coverage = empty();
    let mut start = PublicationStart::Open { origin: SegmentOrigin::Genesis };
    for map in &genesis_maps() {
        coverage = publish(&coverage, &start, std::slice::from_ref(map));
        let anchor = map.resume_anchor();
        start = PublicationStart::Extend { from: anchor };
        let delimiters = delimiters_through(anchor.completed_map_index);
        let visible = activate(&coverage);
        for number in 0..=HEAD {
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
    let maps = genesis_maps();
    let mid_block = mid_block_3(&maps);
    assert_eq!(maps[mid_block].resume_anchor().completed_map_index, 4);
    assert_eq!(delimiters_through(4), 3);
    let coverage = genesis_through(&maps[..=mid_block]);
    assert_eq!(coverage.segments()[0].blocks(), Some(0..=2));
    assert!(!activate(&coverage).covers(3));
    assert!(!coverage.segments()[0].terminal().excludes_block(3, &RANGE_TEST_PARAMS));
}

#[test]
fn a_checkpoint_segment_joins_genesis_construction_only_at_the_same_anchor() {
    let maps = genesis_maps();
    // Checkpoint after map 4, inside block 3, so the join straddles a block.
    let join_at = mid_block_3(&maps);
    let join = maps[join_at].resume_anchor();
    assert_eq!(join.completed_map_index, 4);

    // Resuming at the checkpoint re-renders block 3's slot in map 4 without publishing it again,
    // then reproduces exactly the maps genesis construction rendered after the join.
    let stream = batch(join.pointer, chain()[3..].to_vec());
    let from_checkpoint = render(FilterMapRenderer::resume(stream, join).unwrap());
    assert_eq!(from_checkpoint, maps[join_at + 1..], "a checkpoint enters the same value space");

    let coverage =
        publish(&empty(), &PublicationStart::Open { origin: checkpoint(join) }, &from_checkpoint);
    // Block 3 straddles the checkpoint map and is excluded until both halves are published.
    assert_eq!(coverage.segments()[0].blocks(), Some(4..=5));
    let coverage = publish(
        &coverage,
        &PublicationStart::Open { origin: SegmentOrigin::Genesis },
        &maps[..join_at],
    );
    assert_eq!(coverage.segments().len(), 2, "not adjacent yet");
    assert!(!activate(&coverage).covers(3));

    let from = maps[join_at - 1].resume_anchor();
    let coverage = publish(
        &coverage,
        &PublicationStart::Extend { from },
        std::slice::from_ref(&maps[join_at]),
    );
    assert_eq!(coverage.segments().len(), 1);
    assert_eq!(coverage.segments()[0].blocks(), Some(0..=5));
    assert_eq!(coverage.segments()[0].origin(), &SegmentOrigin::Genesis);
    assert!(activate(&coverage).covers(3));
}

#[test]
fn reorg_contracts_to_a_map_that_excludes_the_changed_block_and_retention_hides_the_tail() {
    let maps = genesis_maps();
    let anchors = maps.iter().map(AnchoredCompletedMap::resume_anchor).collect::<Vec<_>>();
    let coverage = genesis_through(&maps);
    assert_eq!(coverage.segments()[0].blocks(), Some(0..=5));

    // Block 4 changed. Anchors resuming inside block 4 are unsafe; the last anchor whose maps hold
    // only earlier blocks is the one to contract to.
    let safe = anchors
        .iter()
        .rev()
        .copied()
        .find(|anchor| anchor.excludes_block(4, &RANGE_TEST_PARAMS))
        .unwrap();
    assert_eq!(safe.pointer.block_number, 4, "block 4's first map begins right after it");
    let unsafe_anchor = anchors
        .iter()
        .copied()
        .find(|anchor| anchor.completed_map_index > safe.completed_map_index)
        .unwrap();
    assert!(coverage.contract_for_reorg(4, Some(unsafe_anchor)).is_err());

    let (coverage, outcome) = coverage.contract_for_reorg(4, Some(safe)).unwrap();
    assert_eq!(outcome.rebuild_from, Some(safe));
    assert_eq!(coverage.segments()[0].blocks(), Some(0..=3));
    assert!(!activate(&coverage).covers(4));

    // Retention: drop everything through the map that ends block 1.
    let tail = anchors.iter().copied().find(|anchor| anchor.pointer.block_number == 2).unwrap();
    let coverage = coverage.retain_after(tail).unwrap();
    let visible = activate(&coverage);
    assert!(!visible.covers(1));
    assert!(visible.covers(2));
    assert_eq!(coverage.segments()[0].blocks(), Some(2..=3));
}

#[test]
fn query_plan_partitions_around_canonically_activated_segments() {
    let maps = genesis_maps();
    let end = maps
        .iter()
        .position(|map| delimiters_through(map.resume_anchor().completed_map_index) == 4)
        .unwrap();
    let end_map = maps[end].resume_anchor().completed_map_index;
    let queryable = activate(&genesis_through(&maps[..=end]));
    assert_eq!(queryable.segments()[0].blocks(), Some(0..=3));

    let plan = QueryPlan::new(LogQueryTarget::Range { from: 2, to: 5 }, true, &queryable, "head-a")
        .unwrap();
    let CandidateSource::Partitioned(subranges) = plan.source() else { panic!() };
    assert_eq!(
        subranges,
        &[
            PlannedSubrange::Indexed(IndexedMatchRange::new(
                2..=3,
                0..=end_map,
                ParamsId::RangeTest
            )),
            PlannedSubrange::Bloom { blocks: 4..=5 },
        ]
    );
    assert!(plan.revalidate(&"head-b").is_err());
}
