//! `FORMAT 2` conformance of the MDBX [`FilterMapMatchSource`].
//!
//! Every pinned fixture is rendered from the Geth oracle's start, with empty blocks appended until
//! the map Geth left incomplete completes as well. A node publishes only maps it rendered from
//! their first slot, so a map the oracle began mid-map is never published; its resume anchor
//! becomes the recognized checkpoint origin of the published maps instead. After a close and
//! reopen, the canonically activated MDBX segment source must answer exactly like
//! [`InMemoryMatchSource`] over the same maps: every fixture query over its own blocks and over the
//! whole segment, and one query per fixture log over the whole segment. Where the plan indexes a
//! query's blocks completely, both must also return Geth's candidates.
//!
//! Geth pins candidates only for the maps it rendered, and most fixtures start mid-map with a
//! single completed map, so only fixtures whose checkpoint starts a map keep pinned queries.

use alloy_primitives::B256;
use reth_db::{init_db, mdbx::DatabaseArguments};
use reth_db_api::{database::Database, transaction::DbTx};
use reth_filter_maps::{
    coverage::{
        CandidateSource, IndexIdentity, LogQueryTarget, PlannedSubrange, PublicationStart,
        QueryPlan, QueryableCoverage, STORAGE_FORMAT_V1,
    },
    test_utils::{
        golden::{
            self, manifest,
            parser::{Fixture, Origin, QueryResult, Termination},
        },
        recognized_checkpoint, AcceptAllCheckpoints, InMemoryMatchSource,
    },
    AnchoredCompletedMap, BlockInput, BlockPointer, CandidateSet, FilterMapMatchSource,
    FilterMapMatcher, IndexedMatchRange, MapBoundary, MapResumeAnchor, MatchPattern,
    RendererOutput, TopicSelection, GETH_V1,
};
use reth_filter_maps_storage::{FilterMapReadSnapshot, FilterMapStore};
use std::{collections::BTreeMap, convert::Infallible, fmt::Debug};

#[test]
fn mdbx_segment_source_matches_the_in_memory_source_on_every_fixture() {
    let mut published = 0;
    let mut fixtures_with_candidates = 0;
    let mut comparisons = 0;
    let mut geth_pinned = 0;
    for (entry, fixture) in manifest::load_and_validate_corpus().unwrap() {
        let path = entry.path.as_str();
        let mut chain = RenderedChain::new(&fixture);
        let (origin, maps) = chain.take_publication(&fixture);
        assert!(!maps.is_empty(), "{path}: nothing to publish");
        published += 1;

        let params_id = golden::params_id(fixture.params_name);
        let identity = IndexIdentity::new(STORAGE_FORMAT_V1, 1, B256::ZERO, GETH_V1, params_id);
        let directory = tempfile::tempdir().unwrap();
        {
            let db = init_db(directory.path(), DatabaseArguments::test()).unwrap();
            let tx = db.tx_mut().unwrap();
            FilterMapStore::initialize_identity(&tx, &identity).unwrap();
            let mut store =
                FilterMapStore::open(&tx, &identity, &mut AcceptAllCheckpoints).unwrap();
            let origin = recognized_checkpoint(identity, origin, maps[0].resume_anchor());
            let mut start = PublicationStart::Open { origin };
            for map in &maps {
                store.publish(start, std::slice::from_ref(map)).unwrap();
                start = PublicationStart::Extend { from: map.resume_anchor() };
            }
            tx.commit().unwrap();
        }

        let db = init_db(directory.path(), DatabaseArguments::test()).unwrap();
        let activated =
            FilterMapReadSnapshot::open(db.tx().unwrap(), &identity, &mut AcceptAllCheckpoints)
                .unwrap()
                .activate(|number| Ok::<_, Infallible>(Some(chain.hash(number))))
                .unwrap();
        let coverage = activated.coverage().clone();
        assert_eq!(coverage.segments().len(), 1, "{path}");
        let segment_blocks = coverage.segments()[0].blocks().expect("published maps cover blocks");
        let mut mdbx = FilterMapMatcher::new(activated.into_segment_source(0).unwrap());
        let mut memory =
            FilterMapMatcher::new(InMemoryMatchSource::new(maps, chain.pointers()).unwrap());

        // Every pattern over the whole segment, then every query over its own blocks.
        let segment =
            LogQueryTarget::Range { from: *segment_blocks.start(), to: *segment_blocks.end() };
        let mut checks = patterns(path, &fixture)
            .into_iter()
            .map(|(id, pattern)| (id, pattern, segment, None))
            .collect::<Vec<_>>();
        checks.extend(fixture.queries.iter().map(|query| {
            let target = LogQueryTarget::Range { from: query.first_block, to: query.last_block };
            (format!("{path}:{}", query.id), golden::pattern(query).unwrap(), target, Some(query))
        }));
        let mut found_candidates = false;
        for (id, pattern, target, query) in checks {
            let Some(indexed) = indexed_ranges(&coverage, &pattern, target) else {
                let result = query.map(|query| query.result);
                assert_eq!(result, Some(QueryResult::ErrMatchAll), "{id}: scans every block");
                continue
            };
            for range in &indexed {
                let candidates = conform(&id, &mut memory, &mut mdbx, &pattern, range);
                comparisons += 1;
                found_candidates |= !candidates.potential_indices().is_empty();
                // Geth pins candidates for the query's whole block range only.
                if let Some(query) = query &&
                    *range.blocks() == (query.first_block..=query.last_block)
                {
                    assert_eq!(candidates.potential_indices(), query.potential_indices, "{id}");
                    assert_eq!(candidates.candidate_blocks(), query.candidate_blocks, "{id}");
                    geth_pinned += 1;
                }
            }
        }
        fixtures_with_candidates += usize::from(found_candidates);
    }
    assert_eq!(
        (published, fixtures_with_candidates, comparisons, geth_pinned),
        (30, 29, 419, 3),
        "the pinned corpus changed the conformance coverage"
    );
}

/// A fixture's blocks, the empty blocks appended after them, and every map rendered over both.
struct RenderedChain {
    maps: Vec<AnchoredCompletedMap>,
    hashes: BTreeMap<u64, B256>,
    pointers: BTreeMap<u64, BlockPointer>,
}

impl RenderedChain {
    fn new(fixture: &Fixture) -> Self {
        let last = fixture.blocks.last().expect("fixtures feed at least one block");
        let (first_filler, first_hash) = match fixture.termination {
            Termination::Batch { next_block, next_hash } => (next_block, next_hash),
            Termination::Head => (last.number + 1, filler_hash(last.number + 1)),
        };
        let fillers = (first_filler..).map(move |number| {
            let hash = if number == first_filler { first_hash } else { filler_hash(number) };
            BlockInput::new(number, hash, [])
        });
        let mut hashes = fixture
            .blocks
            .iter()
            .map(|block| (block.number, block.hash))
            .collect::<BTreeMap<_, _>>();
        hashes.insert(first_filler, first_hash);

        // The filler input never ends, so the renderer never completes; it is pulled until the
        // map Geth left incomplete is published too.
        let mut renderer = golden::oracle_renderer(
            fixture,
            golden::blocks(fixture).into_iter().chain(fillers),
            golden::termination(fixture),
        );
        let maps = (0..fixture.completed_maps.len() + fixture.partial_maps.len())
            .map(|_| match renderer.render_next().unwrap().unwrap() {
                RendererOutput::Map(map) => map,
                RendererOutput::Complete(completion) => panic!("completed early: {completion:?}"),
            })
            .collect::<Vec<_>>();

        let mut pointers = BTreeMap::new();
        let rendered = maps.iter().flat_map(AnchoredCompletedMap::established_pointers);
        for pointer in fixture.pointers.iter().copied().map(golden::pointer).chain(rendered) {
            let known = *pointers.entry(pointer.block_number).or_insert(pointer);
            assert_eq!(known, pointer, "the fixture and the renderer disagree on a pointer");
        }
        Self { maps, hashes, pointers }
    }

    /// Splits the rendered maps into the origin anchor a node could trust and the maps it could
    /// publish from there.
    ///
    /// A checkpoint at the first slot of a map is the resume anchor of the map before it, so every
    /// rendered map is complete. Any other oracle start begins the first map mid-map, which leaves
    /// it without the marks of earlier slots.
    fn take_publication(
        &mut self,
        fixture: &Fixture,
    ) -> (MapResumeAnchor, Vec<AnchoredCompletedMap>) {
        let mut maps = std::mem::take(&mut self.maps);
        let values_per_map = golden::params(fixture.params_name).values_per_map();
        if let Origin::Checkpoint(checkpoint) = fixture.origin &&
            checkpoint.index > 0 &&
            checkpoint.index.is_multiple_of(values_per_map)
        {
            let previous_map = u32::try_from(checkpoint.index / values_per_map - 1).unwrap();
            let boundary = MapBoundary::new(previous_map, checkpoint.block, checkpoint.hash);
            let origin = MapResumeAnchor::new(boundary, golden::pointer(checkpoint)).unwrap();
            return (origin, maps)
        }
        let published = maps.split_off(1);
        (maps[0].resume_anchor(), published)
    }

    fn hash(&self, number: u64) -> B256 {
        self.hashes.get(&number).copied().unwrap_or_else(|| filler_hash(number))
    }

    fn pointers(&self) -> Vec<BlockPointer> {
        self.pointers.values().copied().collect()
    }
}

/// Plans `target` for `pattern` and returns its indexed subranges, or `None` when the plan scans
/// every block because the pattern has nothing searchable.
fn indexed_ranges(
    coverage: &QueryableCoverage,
    pattern: &MatchPattern,
    target: LogQueryTarget,
) -> Option<Vec<IndexedMatchRange>> {
    let plan = QueryPlan::new(target, pattern.has_searchable_values(), coverage, ()).unwrap();
    match plan.source() {
        CandidateSource::Partitioned(subranges) => Some(
            subranges
                .iter()
                .filter_map(|subrange| match subrange {
                    PlannedSubrange::Indexed(range) => Some(range.clone()),
                    PlannedSubrange::Bloom { .. } => None,
                })
                .collect(),
        ),
        CandidateSource::EveryBlock(_) => None,
        CandidateSource::ResolveBlockHash(_) => unreachable!("fixtures query block ranges"),
    }
}

/// Every searchable fixture query and, to exercise rows the queries miss, one query per fixture log
/// for its address and all of its topics.
fn patterns(path: &str, fixture: &Fixture) -> Vec<(String, MatchPattern)> {
    let queries = fixture
        .queries
        .iter()
        .map(|query| (format!("{path}:{}", query.id), golden::pattern(query).unwrap()));
    let logs = fixture.blocks.iter().flat_map(|block| {
        block.receipts.iter().flat_map(|receipt| &receipt.logs).enumerate().map(
            move |(position, log)| {
                let topics = log.topics.iter().map(|topic| TopicSelection::OneOf(vec![*topic]));
                let pattern = MatchPattern::new(vec![log.address], topics.collect()).unwrap();
                (format!("{path}:block {} log {position}", block.number), pattern)
            },
        )
    });
    queries.chain(logs).filter(|(_, pattern)| pattern.has_searchable_values()).collect()
}

/// Matches `range` through both sources, requires identical results, and returns them.
fn conform(
    id: &str,
    memory: &mut FilterMapMatcher<InMemoryMatchSource>,
    mdbx: &mut FilterMapMatcher<impl FilterMapMatchSource<Error: Debug>>,
    pattern: &MatchPattern,
    range: &IndexedMatchRange,
) -> CandidateSet {
    let expected = memory
        .match_subrange(pattern, range.clone())
        .unwrap_or_else(|error| panic!("{id}: {range:?}: {error}"));
    let actual = mdbx
        .match_subrange(pattern, range.clone())
        .unwrap_or_else(|error| panic!("{id}: {range:?}: {error:?}"));
    assert_eq!(actual, expected, "{id}: {range:?}");
    actual
}

fn filler_hash(number: u64) -> B256 {
    B256::left_padding_from(&number.to_be_bytes())
}
