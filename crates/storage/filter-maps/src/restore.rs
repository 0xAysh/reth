//! Shared restoration and cross-table integrity checks.

use crate::{
    codec::{anchor_from_db, catalog_from_db, identity_from_db},
    error::{FilterMapStorageError, Result},
};
use reth_db_api::{
    models::{FilterMapBaseRowKey, FilterMapExtendedRowKey, StoredMapRowDirectory},
    tables::{
        FilterMapAnchors, FilterMapBaseRows, FilterMapCoverage, FilterMapDirectories,
        FilterMapExtendedRows, FilterMapIdentity,
    },
    transaction::DbTx,
};
use reth_filter_maps::coverage::{IndexIdentity, StructurallyRestoredCoverage};
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
) -> Result<RestoredMetadata> {
    let identity_entries = tx.entries::<FilterMapIdentity>()?;
    let identity = tx.get::<FilterMapIdentity>(SINGLETON_KEY)?;
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
    let coverage = StructurallyRestoredCoverage::restore(running, record, anchors)?;
    Ok(RestoredMetadata { identity, coverage })
}

/// Loads every covered directory and reconciles its exact count with required payloads.
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
            read_map_rows(tx, &metadata.identity, map_index, &directory, false)?;
            directories.insert(map_index, directory);
        }
    }
    Ok(directories)
}

/// Reconstructs every directory-declared row and verifies its exact aggregate mark count.
///
/// When `reject_contradicted` is true, physical payload in directory-empty/base-only states is an
/// integrity error. This stricter mode is used for complete retry comparison.
pub(crate) fn read_map_rows<TX: DbTx>(
    tx: &TX,
    identity: &IndexIdentity,
    map_index: u32,
    directory: &StoredMapRowDirectory,
    reject_contradicted: bool,
) -> Result<BTreeMap<u32, Vec<u32>>> {
    let params = identity.params.params();
    let params_id = u8::from(identity.params);
    let mut rows = BTreeMap::new();
    let mut mark_count = 0u64;

    for row_index in 0..params.map_height() {
        let nonempty = directory.is_nonempty(row_index);
        if !nonempty && !reject_contradicted {
            continue
        }

        let base_key = FilterMapBaseRowKey::new(params_id, map_index, row_index)?;
        let coordinates = base_key.validate(params_id)?;
        let slot = FilterMapBaseRowKey::slot(params_id, map_index)?;
        if coordinates
            .group_start
            .checked_add(u32::try_from(slot).map_err(|_| FilterMapStorageError::Arithmetic)?) !=
            Some(map_index) ||
            coordinates.row_index != row_index
        {
            return Err(FilterMapStorageError::KeyValueMismatch {
                kind: "base row key",
                key: base_key.get(),
            })
        }
        let group = tx.get::<FilterMapBaseRows>(base_key)?;
        if group.as_ref().is_some_and(|group| group.params_id != params_id) {
            return Err(FilterMapStorageError::KeyValueMismatch {
                kind: "base row parameter identity",
                key: base_key.get(),
            })
        }
        let base = group.as_ref().and_then(|group| group.slots.get(slot));

        let extension_key = FilterMapExtendedRowKey::new(params_id, map_index, row_index)?;
        let extension_coordinates = extension_key.validate(params_id)?;
        if extension_coordinates.map_index != map_index ||
            extension_coordinates.row_index != row_index
        {
            return Err(FilterMapStorageError::KeyValueMismatch {
                kind: "extension row key",
                key: extension_key.get(),
            })
        }
        let extension = tx.get::<FilterMapExtendedRows>(extension_key)?;
        if extension.as_ref().is_some_and(|extension| extension.params_id != params_id) {
            return Err(FilterMapStorageError::KeyValueMismatch {
                kind: "extension parameter identity",
                key: extension_key.get(),
            })
        }

        if !nonempty {
            if base.is_some_and(|columns| !columns.is_empty()) || extension.is_some() {
                return Err(FilterMapStorageError::ContradictedPayload { map_index, row_index })
            }
            continue
        }

        let base = base
            .filter(|columns| !columns.is_empty())
            .ok_or(FilterMapStorageError::MissingBaseRow { map_index, row_index })?;
        let mut row = base.clone();
        if directory.is_extended(row_index) {
            if base.len() != params.base_row_length() as usize {
                return Err(FilterMapStorageError::MissingBaseRow { map_index, row_index })
            }
            let extension = extension
                .ok_or(FilterMapStorageError::MissingExtension { map_index, row_index })?;
            row.extend(extension.columns);
        } else if reject_contradicted && extension.is_some() {
            return Err(FilterMapStorageError::ContradictedPayload { map_index, row_index })
        }
        if row.len() > params.max_row_length(params.log_maps_per_epoch()) as usize {
            return Err(FilterMapStorageError::PayloadCountMismatch(map_index))
        }
        mark_count = mark_count
            .checked_add(u64::try_from(row.len()).map_err(|_| FilterMapStorageError::Arithmetic)?)
            .ok_or(FilterMapStorageError::Arithmetic)?;
        rows.insert(row_index, row);
    }

    if mark_count != directory.logical_mark_count {
        return Err(FilterMapStorageError::PayloadCountMismatch(map_index))
    }
    Ok(rows)
}
