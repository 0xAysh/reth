//! Crash-safe MDBX persistence for completed `FilterMaps` output.
//!
//! ```text
//! AnchoredCompletedMap ──► validate complete proposal ──► stage seven-table write
//!                                                               │
//!                                                        caller commits MDBX
//!                                                               │
//! FilterMapMatcher ◄── segment source ◄── canonical activation ◄─┘
//! ```
//!
//! Writes use caller-owned transactions. Coverage is the visibility fence: publication makes rows,
//! pointers, anchors, directories, and coverage visible together, while contraction hides invalid
//! coverage before later cleanup. Structural restoration is deliberately distinct from canonical
//! activation, and [`FilterMapSegmentSource`] implements the existing pure matcher seam without
//! moving candidate logic into storage.
//!
//! This crate does not acquire receipts, schedule indexing, detect reorgs, execute bloom fallback,
//! or integrate RPC. Later lifecycle code composes those operations around these atomic primitives.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod codec;
mod contraction;
mod error;
mod matcher;
mod rows;
mod snapshot;
mod store;
mod validation;

pub use contraction::CleanupRanges;
pub use error::{FilterMapStorageError, Result};
pub use matcher::FilterMapSegmentSource;
pub use snapshot::{ActivatedFilterMapSnapshot, FilterMapReadSnapshot};
pub use store::FilterMapStore;

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256};
    use reth_db::test_utils::create_test_rw_db;
    use reth_db_api::{
        database::Database,
        models::{
            filter_map_physical_params, FilterMapBaseRowKey, FilterMapExtendedRowKey,
            StoredBaseRowGroup, StoredCoverageCatalog, StoredMapRowDirectory,
        },
        table::{Compress, Table},
        tables::{
            FilterMapBaseRows, FilterMapBlockPointers, FilterMapCoverage, FilterMapDirectories,
            FilterMapExtendedRows, FilterMapIdentity,
        },
        transaction::{DbTx, DbTxMut},
    };
    use reth_filter_maps::{
        address_value,
        coverage::{
            CanonicalActivationError, CheckpointProvenance, CheckpointVerifier, IndexIdentity,
            MapResumeAnchor, PublicationStart, RejectUnrecognizedCheckpoints, SegmentOrigin,
            StoredCoverageRecord, StoredSegmentOrigin, StoredSegmentRecord,
            StructurallyRestoredCoverage, STORAGE_FORMAT_V1,
        },
        BlockInput, BlockPointer, FilterMapMatchSource, FilterMapRenderer, LogInput,
        LogValueStream, LogValueStreamTermination, ParamsId, RendererOutput, DEFAULT_PARAMS,
        GETH_V1, RANGE_TEST_PARAMS,
    };

    #[derive(Debug)]
    struct RawFilterMapDirectories;

    impl Table for RawFilterMapDirectories {
        const NAME: &'static str = <FilterMapDirectories as Table>::NAME;
        const DUPSORT: bool = false;
        type Key = u32;
        type Value = Vec<u8>;
    }

    fn identity() -> IndexIdentity {
        IndexIdentity::new(STORAGE_FORMAT_V1, 1, B256::ZERO, GETH_V1, ParamsId::RangeTest)
    }

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

    fn maps() -> Vec<reth_filter_maps::AnchoredCompletedMap> {
        maps_with_address(1, 2)
    }

    fn checkpoint_origin(
        identity: IndexIdentity,
        origin_anchor: reth_filter_maps::coverage::MapResumeAnchor,
        terminal: reth_filter_maps::coverage::MapResumeAnchor,
    ) -> SegmentOrigin {
        let first_map = origin_anchor.completed_map_index + 1;
        let restored = StructurallyRestoredCoverage::restore(
            &identity,
            StoredCoverageRecord {
                identity,
                segments: vec![StoredSegmentRecord {
                    origin: StoredSegmentOrigin::Checkpoint {
                        origin_anchor,
                        provenance: CheckpointProvenance::Recognized { id: 1 },
                    },
                    first_map,
                    terminal_map: terminal.completed_map_index,
                }],
            },
            [terminal],
            &mut TestOriginVerifier,
        )
        .unwrap();
        restored.segments()[0].origin().clone()
    }

    fn publish_maps<TX>(
        tx: &TX,
        identity: &IndexIdentity,
        mut start: PublicationStart,
        maps: &[reth_filter_maps::AnchoredCompletedMap],
    ) -> Result<()>
    where
        TX: DbTx + DbTxMut,
    {
        let mut store = open_store(tx, identity);
        for map in maps {
            store.publish(start, std::slice::from_ref(map))?;
            start = PublicationStart::Extend { from: map.resume_anchor() };
        }
        Ok(())
    }

    fn open_store<'tx, TX: DbTx>(tx: &'tx TX, identity: &IndexIdentity) -> FilterMapStore<'tx, TX> {
        FilterMapStore::open(tx, identity, &mut RejectUnrecognizedCheckpoints).unwrap()
    }

    fn read_snapshot<TX: DbTx>(
        tx: TX,
        identity: &IndexIdentity,
    ) -> Result<FilterMapReadSnapshot<TX>> {
        FilterMapReadSnapshot::open(tx, identity, &mut RejectUnrecognizedCheckpoints)
    }

    fn render_address_map(
        addresses: impl IntoIterator<Item = Address>,
    ) -> reth_filter_maps::AnchoredCompletedMap {
        let blocks = vec![
            BlockInput::new(
                0,
                B256::ZERO,
                addresses.into_iter().map(|address| LogInput::new(address, [])),
            ),
            BlockInput::new(1, B256::repeat_byte(1), []),
        ];
        let stream = LogValueStream::new(
            DEFAULT_PARAMS,
            BlockPointer::new(0, B256::ZERO, 0),
            blocks,
            LogValueStreamTermination::ReachedHead,
        );
        let mut renderer = FilterMapRenderer::from_genesis(stream).unwrap();
        match renderer.render_next().unwrap().unwrap() {
            RendererOutput::Map(map) => map,
            RendererOutput::Complete(_) => panic!("expected a completed production map"),
        }
    }

    fn replacement_transition_maps(
    ) -> (reth_filter_maps::AnchoredCompletedMap, reth_filter_maps::AnchoredCompletedMap, u32) {
        let repeated = Address::repeat_byte(0x6e);
        let target_row = DEFAULT_PARAMS.row_index(0, 1, address_value(repeated));
        let single = (2_000_000u64..)
            .map(|value| {
                let mut bytes = [0u8; 20];
                bytes[12..].copy_from_slice(&value.to_be_bytes());
                Address::from(bytes)
            })
            .find(|address| DEFAULT_PARAMS.row_index(0, 0, address_value(*address)) == target_row)
            .unwrap();
        let filler = |start: u64, count: u64| {
            (start..start + count).map(|value| {
                let mut bytes = [0u8; 20];
                bytes[12..].copy_from_slice(&value.to_be_bytes());
                Address::from(bytes)
            })
        };
        let overflow =
            render_address_map(std::iter::repeat_n(repeated, 137).chain(filler(4_000_000, 65_398)));
        let base = render_address_map(std::iter::once(single).chain(filler(8_000_000, 65_534)));
        let overflow_length = overflow
            .map()
            .rows()
            .iter()
            .find(|row| row.row_index() == target_row)
            .unwrap()
            .columns()
            .len();
        let base_length = base
            .map()
            .rows()
            .iter()
            .find(|row| row.row_index() == target_row)
            .unwrap()
            .columns()
            .len();
        assert!(overflow_length > DEFAULT_PARAMS.base_row_length() as usize);
        assert!(base_length <= DEFAULT_PARAMS.base_row_length() as usize);
        (overflow, base, target_row)
    }

    /// Renders `count` range-test maps over a chain with one log per block, so every other map
    /// ends on a block boundary.
    fn chain_maps(count: usize) -> Vec<reth_filter_maps::AnchoredCompletedMap> {
        chain_maps_with_logs(count, 1)
    }

    /// Renders `count` range-test maps over [`chain_hash`] blocks that each hold `logs` logs.
    fn chain_maps_with_logs(
        count: usize,
        logs: usize,
    ) -> Vec<reth_filter_maps::AnchoredCompletedMap> {
        let blocks = (0..=count as u64)
            .map(|number| {
                let hash = chain_hash(number).unwrap().unwrap();
                let logs = (0..logs).map(|_| LogInput::new(Address::repeat_byte(0x11), []));
                BlockInput::new(number, hash, logs)
            })
            .collect::<Vec<_>>();
        let stream = LogValueStream::new(
            RANGE_TEST_PARAMS,
            BlockPointer::new(0, B256::ZERO, 0),
            blocks,
            LogValueStreamTermination::ReachedHead,
        );
        let mut renderer = FilterMapRenderer::from_genesis(stream).unwrap();
        let mut maps = Vec::new();
        while maps.len() < count {
            match renderer.render_next().unwrap().unwrap() {
                RendererOutput::Map(map) => maps.push(map),
                RendererOutput::Complete(_) => panic!("expected {count} maps"),
            }
        }
        maps
    }

    fn chain_hash(number: u64) -> std::result::Result<Option<B256>, std::convert::Infallible> {
        Ok(Some(if number == 0 { B256::ZERO } else { B256::repeat_byte(number as u8) }))
    }

    fn maps_with_address(address: u8, count: usize) -> Vec<reth_filter_maps::AnchoredCompletedMap> {
        let blocks = vec![
            BlockInput::new(0, B256::ZERO, [LogInput::new(Address::repeat_byte(address), [])]),
            BlockInput::new(1, B256::repeat_byte(1), []),
        ];
        let stream = LogValueStream::new(
            RANGE_TEST_PARAMS,
            BlockPointer::new(0, B256::ZERO, 0),
            blocks,
            LogValueStreamTermination::ReachedHead,
        );
        let mut renderer = FilterMapRenderer::from_genesis(stream).unwrap();
        let mut maps = Vec::new();
        while maps.len() < count {
            match renderer.render_next().unwrap().unwrap() {
                RendererOutput::Map(map) => maps.push(map),
                RendererOutput::Complete(_) => panic!("expected two maps"),
            }
        }
        maps
    }

    #[test]
    fn durable_parameter_geometry_matches_every_recognized_parameter_id() {
        for id in [ParamsId::Default, ParamsId::RangeTest] {
            let logical = id.params();
            let durable = filter_map_physical_params(id.into()).unwrap();
            assert_eq!(durable.map_height, logical.map_height());
            assert_eq!(durable.map_width, logical.map_width());
            assert_eq!(durable.maps_per_epoch, logical.maps_per_epoch());
            assert_eq!(durable.base_row_length, logical.base_row_length());
            assert_eq!(
                durable.max_row_length,
                logical.max_row_length(logical.log_maps_per_epoch())
            );
            assert_eq!(durable.group_size, logical.base_row_group_size());

            let epoch = logical.maps_per_epoch();
            let group = logical.base_row_group_size();
            for map_index in [0, 1, group - 1, group, group + 1, epoch - 1, epoch, epoch + 1]
                .into_iter()
                .chain([2 * epoch + group + 3, u32::MAX - epoch, u32::MAX])
            {
                let key = FilterMapBaseRowKey::new(id.into(), map_index, 0).unwrap();
                assert_eq!(
                    key.validate(id.into()).unwrap().group_start,
                    logical.map_group_index(map_index),
                    "{id:?} map {map_index}"
                );
                assert_eq!(
                    FilterMapBaseRowKey::slot(id.into(), map_index).unwrap(),
                    logical.map_group_offset(map_index) as usize,
                    "{id:?} map {map_index}"
                );
            }
        }
        assert!(filter_map_physical_params(0).is_err());
        assert!(filter_map_physical_params(3).is_err());
    }

    #[test]
    fn cleanup_epoch_ranges_are_bounded_and_non_overlapping() {
        let ranges = CleanupRanges::checked(ParamsId::Default, 0, 1, 0..=2_047, 0..=100).unwrap();
        let first = ranges.row_epoch_range(0).unwrap();
        let second = ranges.row_epoch_range(1).unwrap();
        assert!(first.end < second.start);
        assert!(ranges.row_epoch_range(2).is_err());

        let ranges = CleanupRanges::checked(ParamsId::RangeTest, 0, 1, 0..=1, 0..=1).unwrap();
        assert!(ranges.row_epoch_range(0).unwrap().end < ranges.row_epoch_range(1).unwrap().start);
    }

    #[test]
    fn pristine_store_is_missing_identity() {
        let db = create_test_rw_db();
        assert!(matches!(
            read_snapshot(db.tx().unwrap(), &identity()),
            Err(FilterMapStorageError::MissingIdentity)
        ));
    }

    #[test]
    fn identity_initialization_is_explicit_and_idempotent() {
        let db = create_test_rw_db();
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        tx.commit().unwrap();

        let snapshot = read_snapshot(db.tx().unwrap(), &identity()).unwrap();
        assert!(snapshot.restored().segments().is_empty());
    }

    #[test]
    fn records_without_identity_fail_closed() {
        let db = create_test_rw_db();
        let tx = db.tx_mut().unwrap();
        tx.put::<FilterMapBlockPointers>(
            0,
            reth_db_api::models::StoredBlockPointer {
                block_hash: B256::ZERO,
                first_log_value_index: 0,
            },
        )
        .unwrap();
        assert!(matches!(
            FilterMapStore::initialize_identity(&tx, &identity()),
            Err(FilterMapStorageError::IncompleteStore)
        ));
        tx.commit().unwrap();
        assert!(matches!(
            read_snapshot(db.tx().unwrap(), &identity()),
            Err(FilterMapStorageError::IncompleteStore)
        ));
    }

    #[test]
    fn singleton_rows_under_nonzero_keys_fail_closed() {
        let db = create_test_rw_db();
        let tx = db.tx_mut().unwrap();
        tx.put::<FilterMapIdentity>(1, crate::codec::identity_to_db(&identity())).unwrap();
        tx.commit().unwrap();
        assert!(matches!(
            read_snapshot(db.tx().unwrap(), &identity()),
            Err(FilterMapStorageError::IncompleteStore)
        ));

        let db = create_test_rw_db();
        let tx = db.tx_mut().unwrap();
        tx.put::<FilterMapIdentity>(0, crate::codec::identity_to_db(&identity())).unwrap();
        tx.put::<FilterMapCoverage>(1, StoredCoverageCatalog { segments: Vec::new() }).unwrap();
        tx.commit().unwrap();
        assert!(matches!(
            read_snapshot(db.tx().unwrap(), &identity()),
            Err(FilterMapStorageError::IncompleteStore)
        ));
    }

    #[test]
    fn every_failed_publication_phase_keeps_new_coverage_invisible() {
        use crate::store::PublicationPhase;

        for phase in [
            PublicationPhase::BaseRows,
            PublicationPhase::Extensions,
            PublicationPhase::Directories,
            PublicationPhase::Pointers,
            PublicationPhase::Anchors,
            PublicationPhase::BeforeCoverage,
        ] {
            let db = create_test_rw_db();
            let tx = db.tx_mut().unwrap();
            FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
            tx.commit().unwrap();

            // Commit the deliberately partial physical writes. Coverage remains the visibility
            // fence, so even this hostile caller behavior cannot make them queryable.
            let tx = db.tx_mut().unwrap();
            let maps = maps();
            let result = open_store(&tx, &identity()).publish_with_fault(
                PublicationStart::Open { origin: SegmentOrigin::Genesis },
                &maps[..1],
                phase,
            );
            assert!(
                matches!(result, Err(FilterMapStorageError::InjectedPublicationFailure)),
                "phase {phase:?}: {result:?}"
            );
            tx.commit().unwrap();

            let snapshot = read_snapshot(db.tx().unwrap(), &identity()).unwrap();
            assert!(snapshot.restored().segments().is_empty(), "phase {phase:?}");
            assert!(snapshot.activate(|_| Ok::<_, std::convert::Infallible>(None)).is_ok());
        }
    }

    #[test]
    fn failures_inside_multi_record_row_loops_keep_coverage_invisible() {
        use crate::store::PublicationPhase;

        let (map, _, _) = replacement_transition_maps();
        let identity =
            IndexIdentity::new(STORAGE_FORMAT_V1, 1, B256::ZERO, GETH_V1, ParamsId::Default);
        for phase in [PublicationPhase::BaseRowWrite, PublicationPhase::ExtensionWrite] {
            let db = create_test_rw_db();
            let tx = db.tx_mut().unwrap();
            FilterMapStore::initialize_identity(&tx, &identity).unwrap();
            tx.commit().unwrap();

            // Commit the deliberately interrupted physical loop. Coverage remains unchanged, so
            // neither the first written base group nor extension is queryable.
            let tx = db.tx_mut().unwrap();
            let result = open_store(&tx, &identity).publish_with_fault(
                PublicationStart::Open { origin: SegmentOrigin::Genesis },
                std::slice::from_ref(&map),
                phase,
            );
            assert!(
                matches!(result, Err(FilterMapStorageError::InjectedPublicationFailure)),
                "phase {phase:?}: {result:?}"
            );
            tx.commit().unwrap();

            let snapshot = read_snapshot(db.tx().unwrap(), &identity).unwrap();
            assert!(snapshot.restored().segments().is_empty(), "phase {phase:?}");
        }
    }

    #[test]
    fn aborted_publication_exposes_no_progress() {
        let db = create_test_rw_db();
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        tx.commit().unwrap();

        let tx = db.tx_mut().unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps(),
        )
        .unwrap();
        tx.abort();

        let snapshot = read_snapshot(db.tx().unwrap(), &identity()).unwrap();
        assert!(snapshot.restored().segments().is_empty());
    }

    #[test]
    fn covered_pointer_conflicts_fail_before_mutation() {
        let db = create_test_rw_db();
        let maps = maps();
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps,
        )
        .unwrap();
        tx.commit().unwrap();

        let tx = db.tx_mut().unwrap();
        tx.put::<FilterMapBlockPointers>(
            0,
            reth_db_api::models::StoredBlockPointer {
                block_hash: B256::repeat_byte(0xff),
                first_log_value_index: 0,
            },
        )
        .unwrap();
        assert!(matches!(
            publish_maps(
                &tx,
                &identity(),
                PublicationStart::Open { origin: SegmentOrigin::Genesis },
                &maps,
            ),
            Err(FilterMapStorageError::ProtectedConflict { kind: "block pointer", key: 0 })
        ));
        tx.abort();
    }

    #[test]
    fn publication_cannot_rewrite_a_pointer_that_activation_verifies() {
        // One log per block: map 4 completes inside block 2, which starts at index 4. A checkpoint
        // segment from that anchor covers only block 3 onwards, but activation still verifies the
        // stored pointer of its start block 2.
        let logged = chain_maps(8);
        let origin = logged[4].resume_anchor();
        assert_eq!((origin.pointer.block_number, origin.pointer.first_log_value_index), (2, 4));
        // Empty blocks place block 2 at index 2 instead, a contradictory value space.
        let empty = chain_maps_with_logs(2, 0);
        assert_eq!(empty[1].resume_anchor().pointer.block_number, 2);

        let db = create_test_rw_db();
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        let mut store = FilterMapStore::open(&tx, &identity(), &mut TestOriginVerifier).unwrap();
        let checkpoint = checkpoint_origin(identity(), origin, logged[5].resume_anchor());
        let mut start = PublicationStart::Open { origin: checkpoint };
        for map in &logged[5..=7] {
            store.publish(start, std::slice::from_ref(map)).unwrap();
            start = PublicationStart::Extend { from: map.resume_anchor() };
        }
        assert_eq!(store.restored().segments()[0].blocks(), Some(3..=3));
        assert_eq!(store.restored().segments()[0].pointer_span(), 2..=4);

        // The genesis segment's blocks and maps are disjoint from the checkpoint segment's, but its
        // terminal pointer names block 2.
        store
            .publish(PublicationStart::Open { origin: SegmentOrigin::Genesis }, &empty[..1])
            .unwrap();
        assert!(matches!(
            store.publish(PublicationStart::Extend { from: empty[0].resume_anchor() }, &empty[1..]),
            Err(FilterMapStorageError::ProtectedConflict { kind: "block pointer", key: 2 })
        ));
        tx.commit().unwrap();

        let snapshot =
            FilterMapReadSnapshot::open(db.tx().unwrap(), &identity(), &mut TestOriginVerifier)
                .unwrap();
        // Both segments still activate: the genesis map before block 2 and the checkpoint segment.
        assert_eq!(snapshot.activate(chain_hash).unwrap().coverage().segments().len(), 2);
    }

    #[test]
    fn publication_stays_inside_one_base_row_group() {
        // Range-test epochs hold one map, so every map has its own base-row group.
        let maps = chain_maps(2);
        let db = create_test_rw_db();
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        assert!(matches!(
            open_store(&tx, &identity())
                .publish(PublicationStart::Open { origin: SegmentOrigin::Genesis }, &maps),
            Err(FilterMapStorageError::MultipleBaseRowGroups { first_map: 0, last_map: 1 })
        ));
        tx.abort();
    }

    #[test]
    fn missing_covered_directory_is_corruption() {
        let db = create_test_rw_db();
        let maps = maps();
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps,
        )
        .unwrap();
        tx.commit().unwrap();

        let tx = db.tx_mut().unwrap();
        tx.delete::<FilterMapDirectories>(0, None).unwrap();
        tx.commit().unwrap();
        assert!(matches!(
            read_snapshot(db.tx().unwrap(), &identity()),
            Err(FilterMapStorageError::MissingDirectory(0))
        ));

        // Metadata-only contraction remains available to hide corrupt payload state.
        let tx = db.tx_mut().unwrap();
        open_store(&tx, &identity()).contract_for_reorg(0, None).unwrap();
        tx.commit().unwrap();
        assert!(read_snapshot(db.tx().unwrap(), &identity())
            .unwrap()
            .restored()
            .segments()
            .is_empty());
    }

    #[test]
    fn canonical_mismatch_exposes_no_segment_source() {
        let db = create_test_rw_db();
        let maps = maps();
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps,
        )
        .unwrap();
        tx.commit().unwrap();

        let activated = read_snapshot(db.tx().unwrap(), &identity())
            .unwrap()
            .activate(|_| Ok::<_, std::convert::Infallible>(Some(B256::repeat_byte(0xff))))
            .unwrap();
        assert!(activated.into_segment_source(0).is_err());
    }

    #[test]
    fn exact_publication_retry_is_a_noop_success() {
        let db = create_test_rw_db();
        let maps = maps();
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps,
        )
        .unwrap();
        tx.commit().unwrap();

        let tx = db.tx_mut().unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps,
        )
        .unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn retry_rejects_directory_contradicted_payload() {
        let db = create_test_rw_db();
        let maps = maps();
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps,
        )
        .unwrap();
        tx.commit().unwrap();

        let map_index = 0;
        let occupied = maps[0].map().rows()[0].row_index();
        let empty_row = (0..RANGE_TEST_PARAMS.map_height())
            .find(|row| {
                *row != occupied &&
                    !maps[0].map().rows().iter().any(|item| item.row_index() == *row)
            })
            .unwrap();
        let key =
            FilterMapBaseRowKey::new(ParamsId::RangeTest.into(), map_index, empty_row).unwrap();
        let tx = db.tx_mut().unwrap();
        let mut group = tx
            .get::<FilterMapBaseRows>(key)
            .unwrap()
            .unwrap_or_else(|| StoredBaseRowGroup::empty(ParamsId::RangeTest.into()).unwrap());
        group.slots[FilterMapBaseRowKey::slot(ParamsId::RangeTest.into(), map_index).unwrap()] =
            vec![0];
        tx.put::<FilterMapBaseRows>(key, group).unwrap();
        tx.commit().unwrap();

        let tx = db.tx_mut().unwrap();
        assert!(matches!(
            publish_maps(
                &tx,
                &identity(),
                PublicationStart::Open { origin: SegmentOrigin::Genesis },
                &maps,
            ),
            Err(FilterMapStorageError::IncompletePriorState)
        ));
        tx.abort();
    }

    #[test]
    fn snapshot_rejects_empty_directory_with_positive_mark_count() {
        let db = create_test_rw_db();
        let maps = maps();
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps,
        )
        .unwrap();
        tx.commit().unwrap();

        let empty = StoredMapRowDirectory::new(
            ParamsId::RangeTest.into(),
            vec![0; RANGE_TEST_PARAMS.map_height() as usize / 8],
            vec![0; RANGE_TEST_PARAMS.map_height() as usize / 8],
            0,
            0,
            vec![],
        )
        .unwrap();
        let mut malformed = empty.compress();
        *malformed.last_mut().unwrap() = 1;
        let tx = db.tx_mut().unwrap();
        tx.put::<RawFilterMapDirectories>(0, malformed).unwrap();
        tx.commit().unwrap();

        assert!(matches!(
            read_snapshot(db.tx().unwrap(), &identity()),
            Err(FilterMapStorageError::Database(reth_db_api::DatabaseError::Decode))
        ));
    }

    #[test]
    fn snapshot_defers_missing_required_payload_until_row_access() {
        let db = create_test_rw_db();
        let maps = maps();
        let row = maps[0].map().rows()[0].row_index();
        let key = FilterMapBaseRowKey::new(ParamsId::RangeTest.into(), 0, row).unwrap();
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps,
        )
        .unwrap();
        tx.commit().unwrap();

        let tx = db.tx_mut().unwrap();
        tx.delete::<FilterMapBaseRows>(key, None).unwrap();
        tx.commit().unwrap();

        // Snapshot construction is metadata-only and must not touch the missing row payload.
        let snapshot = read_snapshot(db.tx().unwrap(), &identity()).unwrap();
        let activated = snapshot
            .activate(|number| {
                Ok::<_, std::convert::Infallible>(Some(if number == 0 {
                    B256::ZERO
                } else {
                    B256::repeat_byte(number as u8)
                }))
            })
            .unwrap();
        let mut source = activated.into_segment_source(0).unwrap();
        assert!(matches!(
            source.read_row_prefixes(&[0], row, 1),
            Err(FilterMapStorageError::MissingBaseRow { map_index: 0, row_index })
                if row_index == row
        ));
    }

    #[test]
    fn activation_checks_stored_pointers_from_the_snapshot_transaction() {
        let db = create_test_rw_db();
        let maps = maps();
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps,
        )
        .unwrap();
        tx.commit().unwrap();

        let tx = db.tx_mut().unwrap();
        let mut pointer = tx.get::<FilterMapBlockPointers>(0).unwrap().unwrap();
        pointer.block_hash = B256::repeat_byte(0xff);
        tx.put::<FilterMapBlockPointers>(0, pointer).unwrap();
        tx.commit().unwrap();

        assert!(matches!(
            read_snapshot(db.tx().unwrap(), &identity()).unwrap().activate(chain_hash),
            Err(CanonicalActivationError::PointerMismatch { block_number: 0 })
        ));
    }

    #[test]
    fn old_and_new_transactions_observe_coherent_coverage() {
        let db = create_test_rw_db();
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        tx.commit().unwrap();
        let old = read_snapshot(db.tx().unwrap(), &identity()).unwrap();

        let tx = db.tx_mut().unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps(),
        )
        .unwrap();
        tx.commit().unwrap();

        assert!(old.restored().segments().is_empty());
        let new = read_snapshot(db.tx().unwrap(), &identity()).unwrap();
        assert_eq!(new.restored().segments().len(), 1);
    }

    #[test]
    fn default_geometry_replacement_covers_row_transitions() {
        let db = create_test_rw_db();
        let (overflow, base, target_row) = replacement_transition_maps();
        let production_identity =
            IndexIdentity::new(STORAGE_FORMAT_V1, 1, B256::ZERO, GETH_V1, ParamsId::Default);
        let key = FilterMapBaseRowKey::new(ParamsId::Default.into(), 0, target_row).unwrap();
        let extension_key =
            FilterMapExtendedRowKey::new(ParamsId::Default.into(), 0, target_row).unwrap();

        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &production_identity).unwrap();
        open_store(&tx, &production_identity)
            .publish(
                PublicationStart::Open { origin: SegmentOrigin::Genesis },
                std::slice::from_ref(&overflow),
            )
            .unwrap();
        tx.commit().unwrap();
        let tx = db.tx_mut().unwrap();
        open_store(&tx, &production_identity).contract_for_reorg(0, None).unwrap();
        let mut group = tx.get::<FilterMapBaseRows>(key).unwrap().unwrap();
        group.slots[1] = vec![7, 8, 9];
        tx.put::<FilterMapBaseRows>(key, group).unwrap();
        tx.commit().unwrap();

        // Extended → base-only also removes obsolete extension data and preserves another map's
        // byte-for-byte slot in the shared group.
        let tx = db.tx_mut().unwrap();
        open_store(&tx, &production_identity)
            .publish(
                PublicationStart::Open { origin: SegmentOrigin::Genesis },
                std::slice::from_ref(&base),
            )
            .unwrap();
        tx.commit().unwrap();
        let tx = db.tx().unwrap();
        assert!(!tx.get::<FilterMapDirectories>(0).unwrap().unwrap().is_extended(target_row));
        assert!(tx.get::<FilterMapExtendedRows>(extension_key).unwrap().is_none());
        assert_eq!(tx.get::<FilterMapBaseRows>(key).unwrap().unwrap().slots[1], [7, 8, 9]);
        drop(tx);

        let tx = db.tx_mut().unwrap();
        open_store(&tx, &production_identity).contract_for_reorg(0, None).unwrap();
        tx.commit().unwrap();
        // Base-only → extended recreates the required extension.
        let tx = db.tx_mut().unwrap();
        open_store(&tx, &production_identity)
            .publish(
                PublicationStart::Open { origin: SegmentOrigin::Genesis },
                std::slice::from_ref(&overflow),
            )
            .unwrap();
        tx.commit().unwrap();
        let tx = db.tx().unwrap();
        assert!(tx.get::<FilterMapDirectories>(0).unwrap().unwrap().is_extended(target_row));
        assert!(tx.get::<FilterMapExtendedRows>(extension_key).unwrap().is_some());

        // Rows present only in the base replacement are removed when overflow is restored.
        let removed_row = base
            .map()
            .rows()
            .iter()
            .find(|row| {
                !overflow
                    .map()
                    .rows()
                    .iter()
                    .any(|candidate| candidate.row_index() == row.row_index())
            })
            .unwrap()
            .row_index();
        assert!(!tx.get::<FilterMapDirectories>(0).unwrap().unwrap().is_nonempty(removed_row));
    }

    #[test]
    fn contraction_and_republication_share_one_opened_store() {
        let db = create_test_rw_db();
        let original = maps_with_address(1, 2);
        let replacement = maps_with_address(2, 2);
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &original,
        )
        .unwrap();
        tx.commit().unwrap();

        // The replacement reuses the original anchors, so a handle still holding the contracted
        // coverage would treat it as a mismatched retry instead of a new publication.
        let tx = db.tx_mut().unwrap();
        let mut store = open_store(&tx, &identity());
        let outcome = store.contract_for_reorg(0, None).unwrap();
        assert_eq!(outcome.disabled.len(), 1);
        assert!(store.restored().segments().is_empty());
        let mut start = PublicationStart::Open { origin: SegmentOrigin::Genesis };
        for map in &replacement {
            store.publish(start, std::slice::from_ref(map)).unwrap();
            start = PublicationStart::Extend { from: map.resume_anchor() };
        }
        let published = store.restored().clone();
        tx.commit().unwrap();

        let snapshot = read_snapshot(db.tx().unwrap(), &identity()).unwrap();
        assert_eq!(snapshot.restored(), &published);
        let row = replacement[0].map().rows()[0].row_index();
        let mut source = snapshot.activate(chain_hash).unwrap().into_segment_source(0).unwrap();
        assert_eq!(
            source.read_row_prefixes(&[0], row, 4).unwrap(),
            vec![replacement[0].map().rows()[0].columns().to_vec()]
        );
    }

    #[test]
    fn recognized_checkpoint_origins_still_require_the_verifier() {
        let db = create_test_rw_db();
        let maps = maps();
        let checkpoint =
            checkpoint_origin(identity(), maps[0].resume_anchor(), maps[1].resume_anchor());
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        open_store(&tx, &identity())
            .publish(PublicationStart::Open { origin: checkpoint }, &maps[1..])
            .unwrap();
        tx.commit().unwrap();

        assert!(matches!(
            read_snapshot(db.tx().unwrap(), &identity()),
            Err(FilterMapStorageError::Coverage(
                reth_filter_maps::coverage::PersistedCoverageError::UnverifiedOrigin
            ))
        ));
        let snapshot =
            FilterMapReadSnapshot::open(db.tx().unwrap(), &identity(), &mut TestOriginVerifier)
                .unwrap();
        assert_eq!(snapshot.restored().segments()[0].maps(), 1..=1);
    }

    #[test]
    fn uncovered_stale_maps_can_be_replaced() {
        let db = create_test_rw_db();
        let original = maps_with_address(1, 2);
        let replacement = maps_with_address(2, 2);
        let old_row = original[0].map().rows()[0].row_index();
        let new_row = replacement[0].map().rows()[0].row_index();
        let expected = replacement[0].map().rows()[0].columns().to_vec();
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &original,
        )
        .unwrap();
        tx.commit().unwrap();

        let tx = db.tx_mut().unwrap();
        open_store(&tx, &identity()).contract_for_reorg(0, None).unwrap();
        tx.commit().unwrap();
        let tx = db.tx_mut().unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &replacement,
        )
        .unwrap();
        tx.commit().unwrap();

        let activated = read_snapshot(db.tx().unwrap(), &identity())
            .unwrap()
            .activate(|number| {
                Ok::<_, std::convert::Infallible>(Some(if number == 0 {
                    B256::ZERO
                } else {
                    B256::repeat_byte(number as u8)
                }))
            })
            .unwrap();
        let mut source = activated.into_segment_source(0).unwrap();
        assert_eq!(source.read_row_prefixes(&[0], new_row, 4).unwrap(), vec![expected]);
        if old_row != new_row {
            assert_eq!(
                source.read_row_prefixes(&[0], old_row, 4).unwrap(),
                vec![Vec::<u32>::new()]
            );
        }
    }

    #[test]
    fn retained_origin_restores_and_extends_without_external_verifier() {
        let db = create_test_rw_db();
        let maps = chain_maps(5);
        let tail = maps[1].resume_anchor();
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps[..4],
        )
        .unwrap();
        tx.commit().unwrap();

        let tx = db.tx_mut().unwrap();
        open_store(&tx, &identity()).retain_after(tail).unwrap();
        tx.commit().unwrap();

        let snapshot = read_snapshot(db.tx().unwrap(), &identity()).unwrap();
        let segment = &snapshot.restored().segments()[0];
        assert!(
            matches!(segment.origin(), SegmentOrigin::Retained(retained) if retained.anchor() == tail)
        );
        assert_eq!(segment.maps(), 2..=3);

        let tx = db.tx_mut().unwrap();
        open_store(&tx, &identity())
            .publish(PublicationStart::Extend { from: maps[3].resume_anchor() }, &maps[4..])
            .unwrap();
        tx.commit().unwrap();

        let activated =
            read_snapshot(db.tx().unwrap(), &identity()).unwrap().activate(chain_hash).unwrap();
        assert_eq!(activated.coverage().segments()[0].maps(), 2..=4);
    }

    #[test]
    fn published_coverage_checkpoint_restores_without_external_verifier() {
        let db = create_test_rw_db();
        let maps = chain_maps(4);
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps[..2],
        )
        .unwrap();
        tx.commit().unwrap();

        // A checkpoint derived from activated published coverage keeps the value space reachable
        // after retention drops every map that produced it.
        let checkpoint = read_snapshot(db.tx().unwrap(), &identity())
            .unwrap()
            .activate(chain_hash)
            .unwrap()
            .coverage()
            .derived_checkpoint(maps[1].resume_anchor())
            .unwrap();
        let tx = db.tx_mut().unwrap();
        open_store(&tx, &identity()).retain_after(maps[1].resume_anchor()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Checkpoint(checkpoint) },
            &maps[2..],
        )
        .unwrap();
        tx.commit().unwrap();

        let activated =
            read_snapshot(db.tx().unwrap(), &identity()).unwrap().activate(chain_hash).unwrap();
        let segment = &activated.coverage().segments()[0];
        assert_eq!(segment.origin(), &SegmentOrigin::Checkpoint(checkpoint));
        assert_eq!(segment.maps(), 2..=3);
    }

    #[test]
    fn retention_contraction_persists_without_deletion() {
        let db = create_test_rw_db();
        let maps = maps();
        let terminal = maps[1].resume_anchor();
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps,
        )
        .unwrap();
        tx.commit().unwrap();

        let tx = db.tx_mut().unwrap();
        open_store(&tx, &identity()).retain_after(terminal).unwrap();
        tx.commit().unwrap();
        let tx = db.tx().unwrap();
        assert!(tx.get::<FilterMapDirectories>(0).unwrap().is_some());
        assert!(read_snapshot(tx, &identity()).unwrap().restored().segments().is_empty());
    }

    #[test]
    fn contraction_only_changes_visibility() {
        let db = create_test_rw_db();
        let maps = maps();
        let tx = db.tx_mut().unwrap();
        FilterMapStore::initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps,
        )
        .unwrap();
        tx.commit().unwrap();

        let tx = db.tx_mut().unwrap();
        open_store(&tx, &identity()).contract_for_reorg(0, None).unwrap();
        tx.commit().unwrap();

        let tx = db.tx().unwrap();
        assert!(tx.get::<FilterMapCoverage>(0).unwrap().is_some());
        assert!(tx.get::<FilterMapDirectories>(0).unwrap().is_some());
        assert!(tx.get::<FilterMapBlockPointers>(0).unwrap().is_some());
        let snapshot = read_snapshot(tx, &identity()).unwrap();
        assert!(snapshot.restored().segments().is_empty());
    }
}
