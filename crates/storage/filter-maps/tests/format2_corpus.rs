//! Pinned FORMAT 2 renderer-to-MDBX-to-matcher integration coverage.

use alloy_primitives::B256;
use reth_db::{init_db, mdbx::DatabaseArguments};
use reth_db_api::{database::Database, transaction::DbTx};
use reth_filter_maps::{
    coverage::{
        CheckpointProvenance, CheckpointVerifier, IndexIdentity, MapResumeAnchor, PublicationStart,
        SegmentOrigin, StoredCoverageRecord, StoredSegmentOrigin, StoredSegmentRecord,
        StructurallyRestoredCoverage, STORAGE_FORMAT_V1,
    },
    AnchoredCompletedMap, BlockInput, BlockPointer, FilterMapMatchSource, FilterMapMatcher,
    FilterMapRenderer, IndexedMatchRange, LogInput, LogValueStream, LogValueStreamTermination,
    MapBoundary, MatchPattern, ParamsId, RendererOutput, TopicSelection, ValueSpaceAnchor, GETH_V1,
};
use reth_filter_maps_storage::{FilterMapReadSnapshot, FilterMapStore};
use reth_filter_maps_test_utils::{
    manifest::load_and_validate_corpus,
    parser::{Fixture, Origin, ParamsName, Planner, QueryResult, Termination, TopicConstraint},
};
use std::{collections::BTreeMap, convert::Infallible, path::Path};

#[derive(Default)]
struct TestOriginVerifier;

impl CheckpointVerifier for TestOriginVerifier {
    fn verify_checkpoint(
        &mut self,
        identity: &IndexIdentity,
        anchor: MapResumeAnchor,
        _provenance: CheckpointProvenance,
    ) -> bool {
        anchor.value_space_version == identity.value_space_version
    }
}

#[derive(Clone)]
struct MemorySource {
    params_id: ParamsId,
    rows: BTreeMap<(u32, u32), Vec<u32>>,
    pointers: BTreeMap<u64, u64>,
}

impl FilterMapMatchSource for MemorySource {
    type Error = Infallible;

    fn params_id(&self) -> ParamsId {
        self.params_id
    }

    fn read_row_prefixes(
        &mut self,
        map_indices: &[u32],
        row_index: u32,
        max_columns: u32,
    ) -> Result<Vec<Vec<u32>>, Self::Error> {
        let limit = usize::try_from(max_columns).unwrap_or(usize::MAX);
        Ok(map_indices
            .iter()
            .map(|map| {
                self.rows
                    .get(&(*map, row_index))
                    .map_or_else(Vec::new, |row| row.iter().copied().take(limit).collect())
            })
            .collect())
    }

    fn block_pointer(&mut self, block_number: u64) -> Result<u64, Self::Error> {
        Ok(self.pointers[&block_number])
    }
}

#[test]
fn applicable_format_two_scenarios_roundtrip_through_mdbx() {
    let mut applicable = 0;
    for (entry, fixture) in load_and_validate_corpus().unwrap() {
        if !is_applicable(&fixture) {
            continue
        }
        applicable += 1;
        check_fixture(Path::new(&entry.path), &fixture);
    }
    assert_eq!(applicable, 3, "the pinned corpus changed its applicable scenario set");
}

// The public production renderer can be reconstructed only at a completed-map anchor. Synthetic
// mid-map checkpoints and batch continuations intentionally require private, non-durable renderer
// state, so they are not applicable to a close/reopen storage test.
const fn fixture_params(name: ParamsName) -> reth_filter_maps::Params {
    match name {
        ParamsName::Default => reth_filter_maps::DEFAULT_PARAMS,
        ParamsName::Range => reth_filter_maps::RANGE_TEST_PARAMS,
    }
}

fn is_applicable(fixture: &Fixture) -> bool {
    let Origin::Checkpoint(origin) = fixture.origin else { return false };
    let params = fixture_params(fixture.params_name);
    origin.index > 0 &&
        origin.index.is_multiple_of(params.values_per_map()) &&
        fixture.termination == Termination::Head &&
        !fixture.completed_maps.is_empty() &&
        fixture.queries.iter().all(|query| query.planner == Planner::Matcher)
}

fn check_fixture(path: &Path, fixture: &Fixture) {
    let Origin::Checkpoint(origin) = fixture.origin else { unreachable!() };
    let params = fixture_params(fixture.params_name);
    let params_id = ParamsId::of(&params).unwrap();
    assert_eq!(fixture.params_name, ParamsName::Range);

    let blocks = fixture
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
        .collect::<Vec<_>>();
    let stream = LogValueStream::new(
        params,
        ValueSpaceAnchor::new(origin.block, origin.hash, origin.index),
        blocks,
        LogValueStreamTermination::ReachedHead,
    );
    let first_map = u32::try_from(origin.index / params.values_per_map()).unwrap();
    let prior_anchor = MapResumeAnchor::new(
        MapBoundary::new(first_map - 1, origin.block, origin.hash),
        BlockPointer::new(origin.block, origin.hash, origin.index),
    )
    .unwrap();
    let mut renderer = FilterMapRenderer::resume(stream, prior_anchor).unwrap();
    let mut rendered = Vec::new();
    while let RendererOutput::Map(map) = renderer.render_next().unwrap().unwrap() {
        rendered.push(map);
    }
    assert_rendered_matches(path, fixture, &rendered);

    let identity = IndexIdentity::new(STORAGE_FORMAT_V1, 1, B256::ZERO, GETH_V1, params_id);
    let publication_origin = checkpoint_origin(identity, prior_anchor, rendered[0].resume_anchor());
    let directory = tempfile::tempdir().unwrap();
    {
        let db = init_db(directory.path(), DatabaseArguments::test()).unwrap();
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity).unwrap();
        let mut store = FilterMapStore::open(&tx, &identity, &mut TestOriginVerifier).unwrap();
        let mut start = PublicationStart::Open { origin: publication_origin };
        for map in &rendered {
            store.publish(start, std::slice::from_ref(map)).unwrap();
            start = PublicationStart::Extend { from: map.resume_anchor() };
        }
        tx.commit().unwrap();
    }

    let db = init_db(directory.path(), DatabaseArguments::test()).unwrap();
    let canonical =
        fixture.blocks.iter().map(|block| (block.number, block.hash)).collect::<BTreeMap<_, _>>();
    let memory = MemorySource {
        params_id,
        rows: rendered
            .iter()
            .flat_map(|map| {
                map.map().rows().iter().map(move |row| {
                    ((map.map().map_index(), row.row_index()), row.columns().to_vec())
                })
            })
            .collect(),
        pointers: fixture.pointers.iter().map(|pointer| (pointer.block, pointer.index)).collect(),
    };

    for query in &fixture.queries {
        assert_eq!(query.result, QueryResult::Ok);
        let pattern = MatchPattern::new(
            query.addresses.clone(),
            query
                .topics
                .iter()
                .map(|topic| match topic {
                    TopicConstraint::Any => TopicSelection::Any,
                    TopicConstraint::Values(values) => TopicSelection::OneOf(values.clone()),
                })
                .collect(),
        )
        .unwrap();
        let range = IndexedMatchRange::new(
            query.first_block..=query.last_block,
            query.map_range.0..=query.map_range.1,
            params_id,
        );
        let expected =
            FilterMapMatcher::new(memory.clone()).match_subrange(&pattern, range.clone()).unwrap();
        assert_eq!(expected.potential_indices(), query.potential_indices);
        assert_eq!(expected.candidate_blocks(), query.candidate_blocks);

        let source =
            FilterMapReadSnapshot::open(db.tx().unwrap(), &identity, &mut TestOriginVerifier)
                .unwrap()
                .activate(|number| Ok::<_, Infallible>(canonical.get(&number).copied()))
                .unwrap()
                .into_segment_source(0)
                .unwrap();
        let actual = FilterMapMatcher::new(source).match_subrange(&pattern, range).unwrap();
        assert_eq!(actual, expected, "{}: {}", path.display(), query.id);
        assert_eq!(actual.potential_indices(), query.potential_indices);
        assert_eq!(actual.candidate_blocks(), query.candidate_blocks);
    }
}

fn assert_rendered_matches(path: &Path, fixture: &Fixture, rendered: &[AnchoredCompletedMap]) {
    assert_eq!(rendered.len(), fixture.completed_maps.len(), "{}", path.display());
    for (actual, expected) in rendered.iter().zip(&fixture.completed_maps) {
        assert_eq!(actual.map().map_index(), expected.index, "{}", path.display());
        assert_eq!(actual.map().rows().len(), expected.rows.len(), "{}", path.display());
        for (actual, expected) in actual.map().rows().iter().zip(&expected.rows) {
            assert_eq!(actual.row_index(), expected.index, "{}", path.display());
            assert_eq!(actual.columns(), expected.columns, "{}", path.display());
        }
    }
}

fn checkpoint_origin(
    identity: IndexIdentity,
    origin_anchor: MapResumeAnchor,
    terminal: MapResumeAnchor,
) -> SegmentOrigin {
    let restored = StructurallyRestoredCoverage::restore(
        &identity,
        StoredCoverageRecord {
            identity,
            segments: vec![StoredSegmentRecord {
                origin: StoredSegmentOrigin::Checkpoint {
                    origin_anchor,
                    provenance: CheckpointProvenance::Recognized { id: 1 },
                },
                first_map: terminal.completed_map_index,
                terminal_map: terminal.completed_map_index,
            }],
        },
        [terminal],
        &mut TestOriginVerifier,
    )
    .unwrap();
    restored.segments()[0].origin().clone()
}
