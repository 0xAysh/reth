//! Shared restoration and cross-table integrity checks.

use crate::{
    codec::{anchor_from_db, catalog_from_db, identity_from_db},
    error::{FilterMapStorageError, Result},
};
use reth_db_api::{
    models::StoredMapRowDirectory,
    tables::{
        FilterMapAnchors, FilterMapBaseRows, FilterMapBlockPointers, FilterMapCoverage,
        FilterMapDirectories, FilterMapExtendedRows, FilterMapIdentity,
    },
    transaction::DbTx,
};
use reth_filter_maps::coverage::{
    IndexIdentity, StoredOriginVerifier, StructurallyRestoredCoverage,
};
use std::collections::BTreeMap;

const SINGLETON_KEY: u8 = 0;

/// Structurally restored metadata loaded from one transaction.
#[derive(Debug)]
pub(crate) struct RestoredMetadata {
    pub identity: IndexIdentity,
    pub coverage: StructurallyRestoredCoverage,
}

/// Restores the authoritative singleton identity, catalog, and covered anchor ranges.
pub(crate) fn load_metadata<TX: DbTx>(
    tx: &TX,
    running: &IndexIdentity,
    verifier: &mut impl StoredOriginVerifier,
) -> Result<RestoredMetadata> {
    let identity_entries = tx.entries::<FilterMapIdentity>()?;
    let identity = tx.get::<FilterMapIdentity>(SINGLETON_KEY)?;
    if identity_entries == 0 && identity.is_none() {
        let has_records = tx.entries::<FilterMapCoverage>()? != 0 ||
            tx.entries::<FilterMapAnchors>()? != 0 ||
            tx.entries::<FilterMapBlockPointers>()? != 0 ||
            tx.entries::<FilterMapDirectories>()? != 0 ||
            tx.entries::<FilterMapBaseRows>()? != 0 ||
            tx.entries::<FilterMapExtendedRows>()? != 0;
        return Err(if has_records {
            FilterMapStorageError::IncompleteStore
        } else {
            FilterMapStorageError::MissingIdentity
        })
    }
    if identity_entries != 1 || identity.is_none() {
        return Err(FilterMapStorageError::IncompleteStore)
    }
    let identity = identity_from_db(identity.expect("checked as present"))?;
    running.check_compatible(&identity)?;

    let coverage_entries = tx.entries::<FilterMapCoverage>()?;
    let catalog = tx.get::<FilterMapCoverage>(SINGLETON_KEY)?;
    if coverage_entries > 1 || (coverage_entries == 1 && catalog.is_none()) {
        return Err(FilterMapStorageError::IncompleteStore)
    }
    let catalog =
        catalog.unwrap_or(reth_db_api::models::StoredCoverageCatalog { segments: Vec::new() });
    let record = catalog_from_db(identity, catalog)?;
    let mut anchors = Vec::new();
    for segment in &record.segments {
        for map_index in segment.first_map..=segment.terminal_map {
            let stored =
                tx.get::<FilterMapAnchors>(map_index)?.ok_or(FilterMapStorageError::Coverage(
                    reth_filter_maps::coverage::PersistedCoverageError::MissingAnchor { map_index },
                ))?;
            if stored.completed_map_index != map_index {
                return Err(FilterMapStorageError::KeyValueMismatch {
                    kind: "anchor",
                    key: u64::from(map_index),
                })
            }
            anchors.push(anchor_from_db(stored)?);
        }
    }
    let coverage = StructurallyRestoredCoverage::restore(running, record, anchors, verifier)?;
    Ok(RestoredMetadata { identity, coverage })
}

/// Loads and validates every covered directory without touching row payload tables.
pub(crate) fn load_directories<TX: DbTx>(
    tx: &TX,
    metadata: &RestoredMetadata,
) -> Result<BTreeMap<u32, StoredMapRowDirectory>> {
    let mut directories = BTreeMap::new();
    for segment in metadata.coverage.segments() {
        for map_index in segment.maps() {
            let directory = tx
                .get::<FilterMapDirectories>(map_index)?
                .ok_or(FilterMapStorageError::MissingDirectory(map_index))?;
            if directory.params_id != u8::from(metadata.identity.params) {
                return Err(FilterMapStorageError::KeyValueMismatch {
                    kind: "directory parameter identity",
                    key: u64::from(map_index),
                })
            }
            directories.insert(map_index, directory);
        }
    }
    Ok(directories)
}
