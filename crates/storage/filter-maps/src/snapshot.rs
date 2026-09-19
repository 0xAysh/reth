//! Coherent structural snapshots and canonically activated matcher sources.

use crate::{
    codec::{anchor_from_db, catalog_from_db, identity_from_db},
    error::{FilterMapStorageError, Result},
};
use reth_db_api::{
    models::StoredMapRowDirectory,
    tables::{FilterMapAnchors, FilterMapCoverage, FilterMapDirectories, FilterMapIdentity},
    transaction::DbTx,
};
use reth_filter_maps::coverage::{
    CanonicalActivationError, IndexIdentity, QueryableCoverage, StructurallyRestoredCoverage,
};
use std::collections::BTreeMap;

const SINGLETON_KEY: u8 = 0;

/// One coherent read transaction with eagerly validated `FilterMaps` metadata.
#[derive(Debug)]
pub struct FilterMapReadSnapshot<TX> {
    pub(crate) tx: TX,
    pub(crate) identity: IndexIdentity,
    pub(crate) restored: StructurallyRestoredCoverage,
    pub(crate) directories: BTreeMap<u32, StoredMapRowDirectory>,
}

impl<TX: DbTx> FilterMapReadSnapshot<TX> {
    /// Loads and structurally validates identity, coverage, anchors, and directories from `tx`.
    pub fn new(tx: TX, running: &IndexIdentity) -> Result<Self> {
        if tx.entries::<FilterMapIdentity>()? != 1 || tx.entries::<FilterMapCoverage>()? > 1 {
            return Err(FilterMapStorageError::IncompleteStore)
        }
        let identity = tx
            .get::<FilterMapIdentity>(SINGLETON_KEY)?
            .ok_or(FilterMapStorageError::MissingIdentity)
            .and_then(identity_from_db)?;
        running.check_compatible(&identity)?;

        let catalog = tx
            .get::<FilterMapCoverage>(SINGLETON_KEY)?
            .unwrap_or(reth_db_api::models::StoredCoverageCatalog { segments: Vec::new() });
        let record = catalog_from_db(identity, catalog)?;
        let mut anchors = Vec::new();
        let mut directories = BTreeMap::new();
        for segment in &record.segments {
            for map_index in segment.first_map..=segment.terminal_map {
                let stored = tx.get::<FilterMapAnchors>(map_index)?.ok_or(
                    FilterMapStorageError::Coverage(
                        reth_filter_maps::coverage::PersistedCoverageError::MissingAnchor {
                            map_index,
                        },
                    ),
                )?;
                if stored.completed_map_index != map_index {
                    return Err(FilterMapStorageError::KeyValueMismatch {
                        kind: "anchor",
                        key: u64::from(map_index),
                    })
                }
                anchors.push(anchor_from_db(stored)?);
                let directory = tx
                    .get::<FilterMapDirectories>(map_index)?
                    .ok_or(FilterMapStorageError::MissingDirectory(map_index))?;
                if directory.params_id != u8::from(identity.params) {
                    return Err(FilterMapStorageError::KeyValueMismatch {
                        kind: "directory parameter identity",
                        key: u64::from(map_index),
                    })
                }
                directories.insert(map_index, directory);
            }
        }
        let restored = StructurallyRestoredCoverage::restore(running, record, anchors)?;
        Ok(Self { tx, identity, restored, directories })
    }

    /// Returns the snapshot identity.
    pub const fn identity(&self) -> &IndexIdentity {
        &self.identity
    }

    /// Returns structurally restored coverage, which is not queryable until activation.
    pub const fn restored(&self) -> &StructurallyRestoredCoverage {
        &self.restored
    }

    /// Checks current canonical hashes and returns an activated snapshot.
    pub fn activate<E>(
        self,
        canonical_hash: impl FnMut(u64) -> std::result::Result<Option<alloy_primitives::B256>, E>,
    ) -> std::result::Result<ActivatedFilterMapSnapshot<TX>, CanonicalActivationError<E>> {
        let queryable = self.restored.activate(canonical_hash)?;
        Ok(ActivatedFilterMapSnapshot {
            tx: self.tx,
            identity: self.identity,
            queryable,
            directories: self.directories,
        })
    }
}

/// A coherent snapshot whose retained segments passed canonical activation.
#[derive(Debug)]
pub struct ActivatedFilterMapSnapshot<TX> {
    pub(crate) tx: TX,
    pub(crate) identity: IndexIdentity,
    pub(crate) queryable: QueryableCoverage,
    pub(crate) directories: BTreeMap<u32, StoredMapRowDirectory>,
}

impl<TX: DbTx> ActivatedFilterMapSnapshot<TX> {
    /// Returns activated coverage.
    pub const fn coverage(&self) -> &QueryableCoverage {
        &self.queryable
    }

    /// Pins this transaction to exactly one activated segment.
    pub fn into_segment_source(
        self,
        segment_index: usize,
    ) -> Result<crate::matcher::FilterMapSegmentSource<TX>> {
        crate::matcher::FilterMapSegmentSource::new(self, segment_index)
    }
}
