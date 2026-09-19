//! Crash-safe MDBX persistence for completed `FilterMaps` output.
//!
//! Writes use caller-owned transactions. Coverage is the visibility fence, structural restoration
//! is distinct from canonical activation, and the existing pure matcher seam remains unchanged.

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
mod restore;
mod snapshot;
mod store;
mod validation;

pub use contraction::{contract_for_reorg, retain_after, CleanupRanges};
pub use error::{FilterMapStorageError, Result};
pub use matcher::FilterMapSegmentSource;
pub use snapshot::{ActivatedFilterMapSnapshot, FilterMapActivationError, FilterMapReadSnapshot};
pub use store::{initialize_identity, publish, PublicationStart};

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256};
    use reth_db::{init_db, mdbx::DatabaseArguments, test_utils::create_test_rw_db};
    use reth_db_api::{
        database::Database,
        models::{FilterMapBaseRowKey, StoredBaseRowGroup, StoredCoverageCatalog},
        tables::{
            FilterMapBaseRows, FilterMapBlockPointers, FilterMapCoverage, FilterMapDirectories,
            FilterMapIdentity,
        },
        transaction::{DbTx, DbTxMut},
    };
    use reth_filter_maps::{
        coverage::{
            CheckpointProvenance, IndexIdentity, SegmentOrigin, StoredCoverageRecord,
            StoredSegmentOrigin, StoredSegmentRecord, StructurallyRestoredCoverage,
            STORAGE_FORMAT_V1,
        },
        BlockInput, FilterMapMatchSource, FilterMapMatcher, FilterMapRenderer, IndexedMatchRange,
        LogInput, LogValueStream, LogValueStreamTermination, MatchPattern, ParamsId,
        RendererOutput, ValueSpaceAnchor, DEFAULT_PARAMS, GETH_V1, RANGE_TEST_PARAMS,
    };

    fn identity() -> IndexIdentity {
        IndexIdentity::new(STORAGE_FORMAT_V1, 1, B256::ZERO, GETH_V1, ParamsId::RangeTest)
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
        for map in maps {
            publish(tx, identity, start, std::slice::from_ref(map))?;
            start = PublicationStart::Extend { from: map.resume_anchor() };
        }
        Ok(())
    }

    fn maps_with_address(address: u8, count: usize) -> Vec<reth_filter_maps::AnchoredCompletedMap> {
        let blocks = vec![
            BlockInput::new(0, B256::ZERO, [LogInput::new(Address::repeat_byte(address), [])]),
            BlockInput::new(1, B256::repeat_byte(1), []),
        ];
        let stream = LogValueStream::new(
            RANGE_TEST_PARAMS,
            ValueSpaceAnchor::new(0, B256::ZERO, 0),
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
    fn identity_initialization_is_explicit_and_idempotent() {
        let db = create_test_rw_db();
        let tx = db.tx_mut().unwrap();
        initialize_identity(&tx, &identity()).unwrap();
        initialize_identity(&tx, &identity()).unwrap();
        tx.commit().unwrap();

        let snapshot = FilterMapReadSnapshot::new(db.tx().unwrap(), &identity()).unwrap();
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
            initialize_identity(&tx, &identity()),
            Err(FilterMapStorageError::IncompleteStore)
        ));
        tx.abort();
    }

    #[test]
    fn singleton_rows_under_nonzero_keys_fail_closed() {
        let db = create_test_rw_db();
        let tx = db.tx_mut().unwrap();
        tx.put::<FilterMapIdentity>(1, crate::codec::identity_to_db(&identity())).unwrap();
        tx.commit().unwrap();
        assert!(matches!(
            FilterMapReadSnapshot::new(db.tx().unwrap(), &identity()),
            Err(FilterMapStorageError::IncompleteStore)
        ));

        let db = create_test_rw_db();
        let tx = db.tx_mut().unwrap();
        tx.put::<FilterMapIdentity>(0, crate::codec::identity_to_db(&identity())).unwrap();
        tx.put::<FilterMapCoverage>(1, StoredCoverageCatalog { segments: Vec::new() }).unwrap();
        tx.commit().unwrap();
        assert!(matches!(
            FilterMapReadSnapshot::new(db.tx().unwrap(), &identity()),
            Err(FilterMapStorageError::IncompleteStore)
        ));
    }

    #[test]
    fn publish_reopen_and_matcher_reads_exact_rows() {
        let db = create_test_rw_db();
        let maps = maps();
        let row = &maps[0].map().rows()[0];
        let row_index = row.row_index();
        let expected = row.columns().to_vec();

        let tx = db.tx_mut().unwrap();
        initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps,
        )
        .unwrap();
        tx.commit().unwrap();

        let snapshot = FilterMapReadSnapshot::new(db.tx().unwrap(), &identity()).unwrap();
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
        assert_eq!(source.read_row_prefixes(&[0], row_index, 10).unwrap(), vec![expected]);
        assert_eq!(source.read_row_prefixes(&[0], row_index, 0).unwrap(), vec![Vec::<u32>::new()]);
        assert_eq!(source.block_pointer(0).unwrap(), 0);
        assert_eq!(source.block_pointer(1).unwrap(), 2);
        assert!(source.block_pointer(2).is_err());
    }

    #[test]
    fn committed_state_restores_after_database_reopen() {
        let directory = tempfile::tempdir().unwrap();
        {
            let db = init_db(directory.path(), DatabaseArguments::test()).unwrap();
            let tx = db.tx_mut().unwrap();
            initialize_identity(&tx, &identity()).unwrap();
            publish_maps(
                &tx,
                &identity(),
                PublicationStart::Open { origin: SegmentOrigin::Genesis },
                &maps(),
            )
            .unwrap();
            tx.commit().unwrap();
        }
        {
            let db = init_db(directory.path(), DatabaseArguments::test()).unwrap();
            let snapshot = FilterMapReadSnapshot::new(db.tx().unwrap(), &identity()).unwrap();
            assert_eq!(snapshot.restored().segments().len(), 1);
            assert_eq!(snapshot.restored().segments()[0].terminal().completed_map_index, 1);
        }
    }

    #[test]
    fn aborted_publication_exposes_no_progress() {
        let db = create_test_rw_db();
        let tx = db.tx_mut().unwrap();
        initialize_identity(&tx, &identity()).unwrap();
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

        let snapshot = FilterMapReadSnapshot::new(db.tx().unwrap(), &identity()).unwrap();
        assert!(snapshot.restored().segments().is_empty());
    }

    #[test]
    fn covered_pointer_conflicts_fail_before_mutation() {
        let db = create_test_rw_db();
        let maps = maps();
        let tx = db.tx_mut().unwrap();
        initialize_identity(&tx, &identity()).unwrap();
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
    fn missing_covered_directory_is_corruption() {
        let db = create_test_rw_db();
        let maps = maps();
        let tx = db.tx_mut().unwrap();
        initialize_identity(&tx, &identity()).unwrap();
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
            FilterMapReadSnapshot::new(db.tx().unwrap(), &identity()),
            Err(FilterMapStorageError::MissingDirectory(0))
        ));

        // Metadata-only contraction remains available to hide corrupt payload state.
        let tx = db.tx_mut().unwrap();
        contract_for_reorg(&tx, &identity(), 0, None).unwrap();
        tx.commit().unwrap();
        assert!(FilterMapReadSnapshot::new(db.tx().unwrap(), &identity())
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
        initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps,
        )
        .unwrap();
        tx.commit().unwrap();

        let activated = FilterMapReadSnapshot::new(db.tx().unwrap(), &identity())
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
        initialize_identity(&tx, &identity()).unwrap();
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
    fn retries_survive_adjacent_segment_merges() {
        let maps = maps();
        let checkpoint =
            checkpoint_origin(identity(), maps[0].resume_anchor(), maps[1].resume_anchor());

        // Opening an adjacent checkpoint segment merges it into its predecessor. Retrying the
        // original open publication still compares the exact batch subrange.
        let db = create_test_rw_db();
        let tx = db.tx_mut().unwrap();
        initialize_identity(&tx, &identity()).unwrap();
        publish(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps[..1],
        )
        .unwrap();
        publish(
            &tx,
            &identity(),
            PublicationStart::Open { origin: checkpoint.clone() },
            &maps[1..],
        )
        .unwrap();
        publish(&tx, &identity(), PublicationStart::Open { origin: checkpoint }, &maps[1..])
            .unwrap();
        tx.commit().unwrap();
        let snapshot = FilterMapReadSnapshot::new(db.tx().unwrap(), &identity()).unwrap();
        assert_eq!(snapshot.restored().segments().len(), 1);

        // Publishing the predecessor after the following checkpoint segment also merges. An
        // equivalent extension retry recognizes its predecessor anchor inside the merged segment.
        let checkpoint =
            checkpoint_origin(identity(), maps[0].resume_anchor(), maps[1].resume_anchor());
        let db = create_test_rw_db();
        let tx = db.tx_mut().unwrap();
        initialize_identity(&tx, &identity()).unwrap();
        publish(&tx, &identity(), PublicationStart::Open { origin: checkpoint }, &maps[1..])
            .unwrap();
        publish(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps[..1],
        )
        .unwrap();
        publish(
            &tx,
            &identity(),
            PublicationStart::Extend { from: maps[0].resume_anchor() },
            &maps[1..],
        )
        .unwrap();
        tx.commit().unwrap();
        let snapshot = FilterMapReadSnapshot::new(db.tx().unwrap(), &identity()).unwrap();
        assert_eq!(snapshot.restored().segments().len(), 1);
    }

    #[test]
    fn retry_rejects_directory_contradicted_payload() {
        let db = create_test_rw_db();
        let maps = maps();
        let tx = db.tx_mut().unwrap();
        initialize_identity(&tx, &identity()).unwrap();
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
    fn lazy_row_access_rejects_inexact_directory_mark_count() {
        let db = create_test_rw_db();
        let maps = maps();
        let tx = db.tx_mut().unwrap();
        initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps,
        )
        .unwrap();
        tx.commit().unwrap();

        let tx = db.tx_mut().unwrap();
        let mut directory = tx.get::<FilterMapDirectories>(0).unwrap().unwrap();
        directory.logical_mark_count += 1;
        tx.put::<FilterMapDirectories>(0, directory).unwrap();
        tx.commit().unwrap();
        let snapshot = FilterMapReadSnapshot::new(db.tx().unwrap(), &identity()).unwrap();
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
        let row = maps[0].map().rows()[0].row_index();
        assert!(matches!(
            source.read_row_prefixes(&[0], row, 1),
            Err(FilterMapStorageError::PayloadCountMismatch(0))
        ));
    }

    #[test]
    fn lazy_access_requires_declared_extension_for_short_prefixes() {
        let db = create_test_rw_db();
        let maps = maps();
        let row = maps[0].map().rows()[0].row_index();
        let tx = db.tx_mut().unwrap();
        initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps,
        )
        .unwrap();
        tx.commit().unwrap();

        let tx = db.tx_mut().unwrap();
        let mut directory = tx.get::<FilterMapDirectories>(0).unwrap().unwrap();
        directory.extended[row as usize / 8] |= 1 << (row % 8);
        directory.logical_mark_count = 2;
        tx.put::<FilterMapDirectories>(0, directory).unwrap();
        tx.commit().unwrap();

        let snapshot = FilterMapReadSnapshot::new(db.tx().unwrap(), &identity()).unwrap();
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
            Err(FilterMapStorageError::MissingExtension { map_index: 0, row_index })
                if row_index == row
        ));
    }

    #[test]
    fn snapshot_defers_missing_required_payload_until_row_access() {
        let db = create_test_rw_db();
        let maps = maps();
        let row = maps[0].map().rows()[0].row_index();
        let key = FilterMapBaseRowKey::new(ParamsId::RangeTest.into(), 0, row).unwrap();
        let tx = db.tx_mut().unwrap();
        initialize_identity(&tx, &identity()).unwrap();
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
        let snapshot = FilterMapReadSnapshot::new(db.tx().unwrap(), &identity()).unwrap();
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
    fn activation_rejects_pointer_hash_and_index_corruption() {
        for corrupt_hash in [true, false] {
            let db = create_test_rw_db();
            let maps = maps();
            let tx = db.tx_mut().unwrap();
            initialize_identity(&tx, &identity()).unwrap();
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
            if corrupt_hash {
                pointer.block_hash = B256::repeat_byte(0xff);
            } else {
                pointer.first_log_value_index = 1;
            }
            tx.put::<FilterMapBlockPointers>(0, pointer).unwrap();
            tx.commit().unwrap();

            let result = FilterMapReadSnapshot::new(db.tx().unwrap(), &identity())
                .unwrap()
                .activate(|number| {
                    Ok::<_, std::convert::Infallible>(Some(if number == 0 {
                        B256::ZERO
                    } else {
                        B256::repeat_byte(number as u8)
                    }))
                });
            assert!(matches!(
                result,
                Err(FilterMapActivationError::Storage(FilterMapStorageError::PointerMismatch(0)))
            ));
        }
    }

    #[test]
    fn old_and_new_transactions_observe_coherent_coverage() {
        let db = create_test_rw_db();
        let tx = db.tx_mut().unwrap();
        initialize_identity(&tx, &identity()).unwrap();
        tx.commit().unwrap();
        let old = FilterMapReadSnapshot::new(db.tx().unwrap(), &identity()).unwrap();

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
        let new = FilterMapReadSnapshot::new(db.tx().unwrap(), &identity()).unwrap();
        assert_eq!(new.restored().segments().len(), 1);
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
        initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &original,
        )
        .unwrap();
        tx.commit().unwrap();

        let tx = db.tx_mut().unwrap();
        contract_for_reorg(&tx, &identity(), 0, None).unwrap();
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

        let activated = FilterMapReadSnapshot::new(db.tx().unwrap(), &identity())
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
    fn retention_contraction_persists_without_deletion() {
        let db = create_test_rw_db();
        let maps = maps();
        let terminal = maps[1].resume_anchor();
        let tx = db.tx_mut().unwrap();
        initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps,
        )
        .unwrap();
        tx.commit().unwrap();

        let tx = db.tx_mut().unwrap();
        retain_after(&tx, &identity(), terminal).unwrap();
        tx.commit().unwrap();
        let tx = db.tx().unwrap();
        assert!(tx.get::<FilterMapDirectories>(0).unwrap().is_some());
        assert!(FilterMapReadSnapshot::new(tx, &identity())
            .unwrap()
            .restored()
            .segments()
            .is_empty());
    }

    #[test]
    fn production_renderer_survives_publication_reopen_and_matcher_query() {
        let directory = tempfile::tempdir().unwrap();
        let mut searched = [0u8; 20];
        searched[12..].copy_from_slice(&42u64.to_be_bytes());
        let address = Address::from(searched);
        let logs = (0u64..65_535).map(|index| {
            let mut bytes = [0u8; 20];
            bytes[12..].copy_from_slice(&index.to_be_bytes());
            LogInput::new(Address::from(bytes), [])
        });
        let blocks = vec![
            BlockInput::new(0, B256::ZERO, logs),
            BlockInput::new(1, B256::repeat_byte(1), []),
        ];
        let stream = LogValueStream::new(
            DEFAULT_PARAMS,
            ValueSpaceAnchor::new(0, B256::ZERO, 0),
            blocks,
            LogValueStreamTermination::ReachedHead,
        );
        let mut renderer = FilterMapRenderer::from_genesis(stream).unwrap();
        let rendered = match renderer.render_next().unwrap().unwrap() {
            RendererOutput::Map(map) => map,
            RendererOutput::Complete(_) => panic!("expected a completed production map"),
        };
        assert_eq!(rendered.map().map_index(), 0);
        let production_identity =
            IndexIdentity::new(STORAGE_FORMAT_V1, 1, B256::ZERO, GETH_V1, ParamsId::Default);

        {
            let db = init_db(directory.path(), DatabaseArguments::test()).unwrap();
            let tx = db.tx_mut().unwrap();
            initialize_identity(&tx, &production_identity).unwrap();
            publish(
                &tx,
                &production_identity,
                PublicationStart::Open { origin: SegmentOrigin::Genesis },
                std::slice::from_ref(&rendered),
            )
            .unwrap();
            tx.commit().unwrap();
        }
        {
            let db = init_db(directory.path(), DatabaseArguments::test()).unwrap();
            let source = FilterMapReadSnapshot::new(db.tx().unwrap(), &production_identity)
                .unwrap()
                .activate(|number| {
                    Ok::<_, std::convert::Infallible>(Some(if number == 0 {
                        B256::ZERO
                    } else {
                        B256::repeat_byte(number as u8)
                    }))
                })
                .unwrap()
                .into_segment_source(0)
                .unwrap();
            let candidates = FilterMapMatcher::new(source)
                .match_subrange(
                    &MatchPattern::new(vec![address], vec![]).unwrap(),
                    IndexedMatchRange::new(0..=0, 0..=0, ParamsId::Default),
                )
                .unwrap();
            assert_eq!(candidates.candidate_blocks(), &[0]);
        }
    }

    #[test]
    fn contraction_only_changes_visibility() {
        let db = create_test_rw_db();
        let maps = maps();
        let tx = db.tx_mut().unwrap();
        initialize_identity(&tx, &identity()).unwrap();
        publish_maps(
            &tx,
            &identity(),
            PublicationStart::Open { origin: SegmentOrigin::Genesis },
            &maps,
        )
        .unwrap();
        tx.commit().unwrap();

        let tx = db.tx_mut().unwrap();
        contract_for_reorg(&tx, &identity(), 0, None).unwrap();
        tx.commit().unwrap();

        let tx = db.tx().unwrap();
        assert!(tx.get::<FilterMapCoverage>(0).unwrap().is_some());
        assert!(tx.get::<FilterMapDirectories>(0).unwrap().is_some());
        assert!(tx.get::<FilterMapBlockPointers>(0).unwrap().is_some());
        let snapshot = FilterMapReadSnapshot::new(tx, &identity()).unwrap();
        assert!(snapshot.restored().segments().is_empty());
    }
}
