//! The opened `FilterMaps` store: restored metadata and every coverage mutation over one
//! caller-owned transaction.

use crate::{
    codec::{anchor_from_db, catalog_from_db, catalog_to_db, identity_from_db, identity_to_db},
    error::{FilterMapStorageError, Result},
    validation::{build_publication, PublicationProposal, PublicationWrites},
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
        CheckpointVerifier, IndexIdentity, MapResumeAnchor, PersistedCoverageError,
        ReorgContraction, SegmentOrigin, StructurallyRestoredCoverage,
    },
    AnchoredCompletedMap,
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
        self.publish_observed(start, maps, |_| Ok(()))
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
    /// A tail inside a segment leaves the remainder under a retained origin, which later openings
    /// trust because only this operation mints it.
    pub fn retain_after(&mut self, tail: MapResumeAnchor) -> Result<()> {
        let retained = self.coverage.retain_after(tail)?;
        self.write_coverage(retained)
    }

    fn publish_observed(
        &mut self,
        start: PublicationStart,
        maps: &[AnchoredCompletedMap],
        mut after_phase: impl FnMut(PublicationPhase) -> Result<()>,
    ) -> Result<()> {
        let PublicationProposal::Write(writes) = build_publication(self, &start, maps)? else {
            return Ok(())
        };
        let PublicationWrites { base_groups, extensions, directories, pointers, anchors, coverage } =
            writes;
        for (key, group) in base_groups {
            self.tx.put::<FilterMapBaseRows>(key, group)?;
            after_phase(PublicationPhase::BaseRowWrite)?;
        }
        after_phase(PublicationPhase::BaseRows)?;
        for (key, extension) in extensions {
            self.tx.delete::<FilterMapExtendedRows>(key, None)?;
            if let Some(extension) = extension {
                self.tx.put::<FilterMapExtendedRows>(key, extension)?;
            }
            after_phase(PublicationPhase::ExtensionWrite)?;
        }
        after_phase(PublicationPhase::Extensions)?;
        for (map_index, directory) in directories {
            self.tx.put::<FilterMapDirectories>(map_index, directory)?;
        }
        after_phase(PublicationPhase::Directories)?;
        for (block_number, pointer) in pointers {
            self.tx.put::<FilterMapBlockPointers>(block_number, pointer)?;
        }
        after_phase(PublicationPhase::Pointers)?;
        for (map_index, anchor) in anchors {
            self.tx.put::<FilterMapAnchors>(map_index, anchor)?;
        }
        after_phase(PublicationPhase::Anchors)?;
        after_phase(PublicationPhase::BeforeCoverage)?;
        self.write_coverage(coverage)
    }

    /// Writes the coverage visibility fence and adopts it only once the write succeeded.
    fn write_coverage(&mut self, coverage: StructurallyRestoredCoverage) -> Result<()> {
        self.tx.put::<FilterMapCoverage>(SINGLETON_KEY, catalog_to_db(coverage.stored_record()))?;
        self.coverage = coverage;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn publish_with_fault(
        &mut self,
        start: PublicationStart,
        maps: &[AnchoredCompletedMap],
        fault_after: PublicationPhase,
    ) -> Result<()> {
        self.publish_observed(start, maps, |phase| {
            if phase == fault_after {
                Err(FilterMapStorageError::InjectedPublicationFailure)
            } else {
                Ok(())
            }
        })
    }
}

/// Trusted starting point for one contiguous publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicationStart {
    /// Opens a separately trusted segment.
    Open {
        /// Genesis, verified checkpoint, or retained anchor.
        origin: SegmentOrigin,
    },
    /// Extends the segment whose current terminal equals `from`.
    Extend {
        /// Current durable terminal.
        from: MapResumeAnchor,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicationPhase {
    BaseRowWrite,
    BaseRows,
    ExtensionWrite,
    Extensions,
    Directories,
    Pointers,
    Anchors,
    BeforeCoverage,
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
