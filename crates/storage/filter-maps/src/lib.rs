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
mod snapshot;
mod store;
mod validation;

pub use contraction::{contract_for_reorg, retain_after, CleanupRanges};
pub use error::{FilterMapStorageError, Result};
pub use matcher::FilterMapSegmentSource;
pub use snapshot::{ActivatedFilterMapSnapshot, FilterMapReadSnapshot};
pub use store::{initialize_identity, publish, PublicationStart};

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256};
    use reth_db::test_utils::create_test_rw_db;
    use reth_db_api::{
        database::Database,
        tables::{FilterMapBlockPointers, FilterMapCoverage, FilterMapDirectories},
        transaction::{DbTx, DbTxMut},
    };
    use reth_filter_maps::{
        coverage::{IndexIdentity, SegmentOrigin, STORAGE_FORMAT_V1},
        BlockInput, FilterMapMatchSource, FilterMapRenderer, LogInput, LogValueStream,
        LogValueStreamTermination, ParamsId, RendererOutput, ValueSpaceAnchor, GETH_V1,
        RANGE_TEST_PARAMS,
    };

    fn identity() -> IndexIdentity {
        IndexIdentity::new(STORAGE_FORMAT_V1, 1, B256::ZERO, GETH_V1, ParamsId::RangeTest)
    }

    fn maps() -> Vec<reth_filter_maps::AnchoredCompletedMap> {
        maps_with_address(1)
    }

    fn maps_with_address(address: u8) -> Vec<reth_filter_maps::AnchoredCompletedMap> {
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
        while maps.len() < 2 {
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
    fn publish_reopen_and_matcher_reads_exact_rows() {
        let db = create_test_rw_db();
        let maps = maps();
        let row = &maps[0].map().rows()[0];
        let row_index = row.row_index();
        let expected = row.columns().to_vec();

        let tx = db.tx_mut().unwrap();
        initialize_identity(&tx, &identity()).unwrap();
        publish(&tx, &identity(), PublicationStart::Open { origin: SegmentOrigin::Genesis }, &maps)
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
    fn aborted_publication_exposes_no_progress() {
        let db = create_test_rw_db();
        let tx = db.tx_mut().unwrap();
        initialize_identity(&tx, &identity()).unwrap();
        tx.commit().unwrap();

        let tx = db.tx_mut().unwrap();
        publish(
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
        publish(&tx, &identity(), PublicationStart::Open { origin: SegmentOrigin::Genesis }, &maps)
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
            publish(
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
        publish(&tx, &identity(), PublicationStart::Open { origin: SegmentOrigin::Genesis }, &maps)
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
        publish(&tx, &identity(), PublicationStart::Open { origin: SegmentOrigin::Genesis }, &maps)
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
        publish(&tx, &identity(), PublicationStart::Open { origin: SegmentOrigin::Genesis }, &maps)
            .unwrap();
        tx.commit().unwrap();

        let tx = db.tx_mut().unwrap();
        publish(&tx, &identity(), PublicationStart::Open { origin: SegmentOrigin::Genesis }, &maps)
            .unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn old_and_new_transactions_observe_coherent_coverage() {
        let db = create_test_rw_db();
        let tx = db.tx_mut().unwrap();
        initialize_identity(&tx, &identity()).unwrap();
        tx.commit().unwrap();
        let old = FilterMapReadSnapshot::new(db.tx().unwrap(), &identity()).unwrap();

        let tx = db.tx_mut().unwrap();
        publish(
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
        let original = maps_with_address(1);
        let replacement = maps_with_address(2);
        let old_row = original[0].map().rows()[0].row_index();
        let new_row = replacement[0].map().rows()[0].row_index();
        let expected = replacement[0].map().rows()[0].columns().to_vec();
        let tx = db.tx_mut().unwrap();
        initialize_identity(&tx, &identity()).unwrap();
        publish(
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
        publish(
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
        publish(&tx, &identity(), PublicationStart::Open { origin: SegmentOrigin::Genesis }, &maps)
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
    fn contraction_only_changes_visibility() {
        let db = create_test_rw_db();
        let maps = maps();
        let tx = db.tx_mut().unwrap();
        initialize_identity(&tx, &identity()).unwrap();
        publish(&tx, &identity(), PublicationStart::Open { origin: SegmentOrigin::Genesis }, &maps)
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
