//! Coherent structural snapshots and canonically activated matcher sources.

use crate::{
    codec::pointer_from_db,
    error::{FilterMapStorageError, Result},
    store::FilterMapStore,
};
use alloy_primitives::B256;
use reth_db_api::{
    models::StoredMapRowDirectory, tables::FilterMapBlockPointers, transaction::DbTx,
};
use reth_filter_maps::coverage::{
    CanonicalActivationError, CheckpointVerifier, IndexIdentity, QueryableCoverage,
    StructurallyRestoredCoverage,
};
use std::collections::BTreeMap;

/// One coherent read transaction with eagerly validated `FilterMaps` metadata.
#[derive(Debug)]
pub struct FilterMapReadSnapshot<TX> {
    pub(crate) tx: TX,
    pub(crate) identity: IndexIdentity,
    pub(crate) restored: StructurallyRestoredCoverage,
    pub(crate) directories: BTreeMap<u32, StoredMapRowDirectory>,
}

impl<TX: DbTx> FilterMapReadSnapshot<TX> {
    /// Opens the store over an owned read transaction and eagerly validates identity, coverage,
    /// anchors, and directory metadata.
    ///
    /// The snapshot owns its transaction so a segment source can outlive this call. Origin trust
    /// follows [`FilterMapStore::open`].
    pub fn open(
        tx: TX,
        running: &IndexIdentity,
        checkpoints: &mut impl CheckpointVerifier,
    ) -> Result<Self> {
        let store = FilterMapStore::open(&tx, running, checkpoints)?;
        let directories = store.load_directories()?;
        let (identity, restored) = store.into_parts();
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

    /// Activates restored coverage against the current canonical chain.
    ///
    /// The activation rules are [`StructurallyRestoredCoverage::activate`]'s; this snapshot only
    /// supplies the stored block pointers it asks for from this transaction.
    pub fn activate<E>(
        self,
        canonical_hash: impl FnMut(u64) -> std::result::Result<Option<B256>, E>,
    ) -> std::result::Result<
        ActivatedFilterMapSnapshot<TX>,
        CanonicalActivationError<E, FilterMapStorageError>,
    > {
        let queryable = self.restored.activate(canonical_hash, |block_number| {
            Ok(self
                .tx
                .get::<FilterMapBlockPointers>(block_number)?
                .map(|stored| pointer_from_db(block_number, stored)))
        })?;
        Ok(ActivatedFilterMapSnapshot { tx: self.tx, queryable, directories: self.directories })
    }
}

/// A coherent snapshot whose retained segments passed canonical activation.
#[derive(Debug)]
pub struct ActivatedFilterMapSnapshot<TX> {
    pub(crate) tx: TX,
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
