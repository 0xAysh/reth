//! The opened `FilterMaps` store: restored metadata and every coverage mutation over one
//! caller-owned transaction.

use crate::{
    codec::{anchor_from_db, catalog_from_db, catalog_to_db, identity_from_db, identity_to_db},
    error::{FilterMapStorageError, Result},
    publication::{build_publication, PublicationPlan, PublicationWrite},
};
use reth_db_api::{
    models::{StoredCoverageCatalog, StoredMapRowDirectory},
    tables::{
        FilterMapAnchors, FilterMapBaseRows, FilterMapBlockPointers, FilterMapCoverage,
        FilterMapDirectories, FilterMapExtendedRows, FilterMapIdentity,
    },
    transaction::{DbTx, DbTxMut},
};
use reth_filter_maps::{
    coverage::{
        CheckpointVerifier, IndexIdentity, PersistedCoverageError, PublicationStart,
        ReorgContraction, StructurallyRestoredCoverage,
    },
    AnchoredCompletedMap, MapResumeAnchor,
};
use std::collections::BTreeMap;

/// Key of the identity and coverage singleton records.
const SINGLETON_KEY: u8 = 0;

/// A `FilterMaps` store opened over one caller-owned transaction.
///
/// Opening restores the identity, coverage catalog, and every covered anchor once. Mutations
/// validate their complete proposal before the first write, write coverage last, and then adopt the
/// written coverage in memory, so later operations in the same transaction observe it without
/// reloading. The handle borrows the transaction, so the caller still decides when to commit and
/// the handle cannot outlive that commit or abort.
#[derive(Debug)]
pub struct FilterMapStore<'tx, TX> {
    tx: &'tx TX,
    identity: IndexIdentity,
    coverage: StructurallyRestoredCoverage,
}

impl<'tx, TX: DbTx> FilterMapStore<'tx, TX> {
    /// Restores the store's identity and structurally valid coverage.
    ///
    /// Origins the store minted itself restore directly. `checkpoints` is consulted only for
    /// externally recognized or derived checkpoints; pass
    /// [`RejectUnrecognizedCheckpoints`](reth_filter_maps::coverage::RejectUnrecognizedCheckpoints)
    /// when no checkpoint registry is available.
    pub fn open(
        tx: &'tx TX,
        running: &IndexIdentity,
        checkpoints: &mut impl CheckpointVerifier,
    ) -> Result<Self> {
        let identity_entries = tx.entries::<FilterMapIdentity>()?;
        let identity = tx.get::<FilterMapIdentity>(SINGLETON_KEY)?;
        if identity_entries == 0 && identity.is_none() {
            return Err(if has_records(tx)? {
                FilterMapStorageError::IncompleteStore
            } else {
                FilterMapStorageError::MissingIdentity
            })
        }
        let Some(identity) = identity.filter(|_| identity_entries == 1) else {
            return Err(FilterMapStorageError::IncompleteStore)
        };
        let identity = identity_from_db(identity)?;
        running.check_compatible(&identity)?;

        let coverage_entries = tx.entries::<FilterMapCoverage>()?;
        let catalog = tx.get::<FilterMapCoverage>(SINGLETON_KEY)?;
        if coverage_entries > 1 || (coverage_entries == 1 && catalog.is_none()) {
            return Err(FilterMapStorageError::IncompleteStore)
        }
        let catalog = catalog.unwrap_or(StoredCoverageCatalog { segments: Vec::new() });
        let record = catalog_from_db(identity, catalog)?;
        let mut anchors = Vec::new();
        for segment in &record.segments {
            for map_index in segment.first_map..=segment.terminal_map {
                let stored = tx
                    .get::<FilterMapAnchors>(map_index)?
                    .ok_or(PersistedCoverageError::MissingAnchor { map_index })?;
                if stored.completed_map_index != map_index {
                    return Err(FilterMapStorageError::KeyValueMismatch {
                        kind: "anchor",
                        key: u64::from(map_index),
                    })
                }
                anchors.push(anchor_from_db(stored)?);
            }
        }
        let coverage =
            StructurallyRestoredCoverage::restore(running, record, anchors, checkpoints)?;
        Ok(Self { tx, identity, coverage })
    }

    /// Returns the stored identity.
    pub const fn identity(&self) -> &IndexIdentity {
        &self.identity
    }

    /// Returns structurally restored coverage, including this transaction's own mutations.
    pub const fn restored(&self) -> &StructurallyRestoredCoverage {
        &self.coverage
    }

    pub(crate) const fn tx(&self) -> &'tx TX {
        self.tx
    }

    /// Loads and validates every covered directory without touching row payload tables.
    pub(crate) fn load_directories(&self) -> Result<BTreeMap<u32, StoredMapRowDirectory>> {
        let mut directories = BTreeMap::new();
        for segment in self.coverage.segments() {
            for map_index in segment.maps() {
                let directory = self
                    .tx
                    .get::<FilterMapDirectories>(map_index)?
                    .ok_or(FilterMapStorageError::MissingDirectory(map_index))?;
                if directory.params_id != u8::from(self.identity.params) {
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

    pub(crate) fn into_parts(self) -> (IndexIdentity, StructurallyRestoredCoverage) {
        (self.identity, self.coverage)
    }
}

impl<'tx, TX: DbTx + DbTxMut> FilterMapStore<'tx, TX> {
    /// Initializes the authoritative identity in an otherwise empty `FilterMaps` store.
    ///
    /// Re-initializing with a compatible identity succeeds without writing. Any other record
    /// without an identity is an incomplete store.
    pub fn initialize_identity(tx: &'tx TX, identity: &IndexIdentity) -> Result<()> {
        if let Some(stored) = tx.get::<FilterMapIdentity>(SINGLETON_KEY)? {
            if tx.entries::<FilterMapIdentity>()? != 1 {
                return Err(FilterMapStorageError::IncompleteStore)
            }
            identity.check_compatible(&identity_from_db(stored)?)?;
            return Ok(())
        }
        if tx.entries::<FilterMapIdentity>()? != 0 || has_records(tx)? {
            return Err(FilterMapStorageError::IncompleteStore)
        }
        tx.put::<FilterMapIdentity>(SINGLETON_KEY, identity_to_db(identity))?;
        Ok(())
    }

    /// Validates and stages one complete `FilterMaps` publication.
    ///
    /// All reads and proposal construction finish before the first mutation. Coverage is written
    /// last; only the caller's transaction commit makes the publication visible. An exact retry of
    /// already published maps succeeds without writing.
    pub fn publish(
        &mut self,
        start: PublicationStart,
        maps: &[AnchoredCompletedMap],
    ) -> Result<()> {
        let PublicationPlan::Write(writes) = build_publication(self, &start, maps)? else {
            return Ok(())
        };
        writes.into_iter().try_for_each(|write| self.apply(write))
    }

    /// Contracts query visibility before rebuilding a changed canonical branch.
    pub fn contract_for_reorg(
        &mut self,
        earliest_changed: u64,
        safe_anchor: Option<MapResumeAnchor>,
    ) -> Result<ReorgContraction> {
        let (contracted, outcome) =
            self.coverage.contract_for_reorg(earliest_changed, safe_anchor)?;
        self.write_coverage(contracted)?;
        Ok(outcome)
    }

    /// Atomically drops visibility through a published tail anchor.
    ///
    /// A tail inside a segment leaves the remainder under a retained origin. Later openings trust
    /// it by kind, without a checkpoint verifier, because this operation writes it in the same
    /// transaction as the coverage it retains.
    pub fn retain_after(&mut self, tail: MapResumeAnchor) -> Result<()> {
        let retained = self.coverage.retain_after(tail)?;
        self.write_coverage(retained)
    }

    /// Applies one publication write; the coverage write also adopts the published coverage.
    pub(crate) fn apply(&mut self, write: PublicationWrite) -> Result<()> {
        match write {
            PublicationWrite::BaseRowGroup(key, group) => {
                self.tx.put::<FilterMapBaseRows>(key, group)?
            }
            PublicationWrite::ExtendedRow(key, row) => {
                self.tx.delete::<FilterMapExtendedRows>(key, None)?;
                if let Some(row) = row {
                    self.tx.put::<FilterMapExtendedRows>(key, row)?;
                }
            }
            PublicationWrite::Directory(map_index, directory) => {
                self.tx.put::<FilterMapDirectories>(map_index, directory)?
            }
            PublicationWrite::BlockPointer(block_number, pointer) => {
                self.tx.put::<FilterMapBlockPointers>(block_number, pointer)?
            }
            PublicationWrite::Anchor(map_index, anchor) => {
                self.tx.put::<FilterMapAnchors>(map_index, anchor)?
            }
            PublicationWrite::Coverage(coverage) => self.write_coverage(coverage)?,
        }
        Ok(())
    }

    /// Writes the coverage visibility fence and adopts it only once the write succeeded.
    fn write_coverage(&mut self, coverage: StructurallyRestoredCoverage) -> Result<()> {
        self.tx.put::<FilterMapCoverage>(SINGLETON_KEY, catalog_to_db(coverage.stored_record()))?;
        self.coverage = coverage;
        Ok(())
    }
}

/// Returns whether any table other than identity holds a record.
fn has_records<TX: DbTx>(tx: &TX) -> Result<bool> {
    Ok(tx.entries::<FilterMapCoverage>()? != 0 ||
        tx.entries::<FilterMapAnchors>()? != 0 ||
        tx.entries::<FilterMapBlockPointers>()? != 0 ||
        tx.entries::<FilterMapDirectories>()? != 0 ||
        tx.entries::<FilterMapBaseRows>()? != 0 ||
        tx.entries::<FilterMapExtendedRows>()? != 0)
}
