//! Coherent structural snapshots and canonically activated matcher sources.

use crate::{
    error::{FilterMapStorageError, Result},
    restore::{load_directories, load_metadata},
};
use reth_db_api::{
    models::{StoredBlockPointer, StoredMapRowDirectory},
    tables::FilterMapBlockPointers,
    transaction::DbTx,
};
use reth_filter_maps::coverage::{
    CanonicalActivationError, IndexIdentity, QueryableCoverage, SegmentOrigin,
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
    /// Eagerly validates identity, coverage, anchors, and directory metadata only.
    pub fn new(tx: TX, running: &IndexIdentity) -> Result<Self> {
        let metadata = load_metadata(&tx, running)?;
        let directories = load_directories(&tx, &metadata)?;
        Ok(Self { tx, identity: metadata.identity, restored: metadata.coverage, directories })
    }

    /// Returns the snapshot identity.
    pub const fn identity(&self) -> &IndexIdentity {
        &self.identity
    }

    /// Returns structurally restored coverage, which is not queryable until activation.
    pub const fn restored(&self) -> &StructurallyRestoredCoverage {
        &self.restored
    }

    /// Checks current canonical hashes and validates every permitted pointer record.
    pub fn activate<E>(
        self,
        mut canonical_hash: impl FnMut(u64) -> std::result::Result<Option<alloy_primitives::B256>, E>,
    ) -> std::result::Result<ActivatedFilterMapSnapshot<TX>, FilterMapActivationError<E>> {
        let queryable = self
            .restored
            .activate(&mut canonical_hash)
            .map_err(FilterMapActivationError::Canonical)?;
        let pointers =
            validate_pointers(&self.tx, &self.identity, &queryable, &mut canonical_hash)?;
        Ok(ActivatedFilterMapSnapshot {
            tx: self.tx,
            identity: self.identity,
            queryable,
            directories: self.directories,
            pointers,
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
    pub(crate) pointers: BTreeMap<u64, StoredBlockPointer>,
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

/// Failure while activating a structurally restored snapshot.
#[derive(Debug, thiserror::Error)]
pub enum FilterMapActivationError<E> {
    /// Segment endpoint canonical verification failed.
    #[error(transparent)]
    Canonical(#[from] CanonicalActivationError<E>),
    /// Canonical lookup failed while validating normalized pointers.
    #[error("canonical hash lookup failed while validating FilterMaps pointers")]
    Lookup(#[source] E),
    /// Pointer storage or cross-table validation failed.
    #[error(transparent)]
    Storage(#[from] FilterMapStorageError),
}

fn validate_pointers<TX: DbTx, E>(
    tx: &TX,
    identity: &IndexIdentity,
    queryable: &QueryableCoverage,
    canonical_hash: &mut impl FnMut(u64) -> std::result::Result<Option<alloy_primitives::B256>, E>,
) -> std::result::Result<BTreeMap<u64, StoredBlockPointer>, FilterMapActivationError<E>> {
    let mut expected = BTreeMap::new();
    for segment in queryable.coverage().segments() {
        match segment.origin() {
            SegmentOrigin::Genesis => {
                insert_expected(
                    &mut expected,
                    0,
                    StoredBlockPointer {
                        block_hash: identity.genesis_hash,
                        first_log_value_index: 0,
                    },
                )?;
            }
            origin => {
                let anchor = origin.anchor().expect("non-genesis origins have anchors");
                insert_expected(
                    &mut expected,
                    anchor.pointer.block_number,
                    StoredBlockPointer {
                        block_hash: anchor.pointer.block_hash,
                        first_log_value_index: anchor.pointer.first_log_value_index,
                    },
                )?;
            }
        }
        for anchor in segment.anchors() {
            insert_expected(
                &mut expected,
                anchor.pointer.block_number,
                StoredBlockPointer {
                    block_hash: anchor.pointer.block_hash,
                    first_log_value_index: anchor.pointer.first_log_value_index,
                },
            )?;
        }
    }

    let mut pointers = BTreeMap::new();
    for segment in queryable.coverage().segments() {
        let Some(blocks) = segment.blocks() else { continue };
        let successor = blocks.end().checked_add(1).ok_or(FilterMapStorageError::Arithmetic)?;
        let mut previous_index = None;
        for block_number in *blocks.start()..=successor {
            let pointer = tx
                .get::<FilterMapBlockPointers>(block_number)
                .map_err(FilterMapStorageError::from)?
                .ok_or(FilterMapStorageError::MissingPointer(block_number))?;
            if canonical_hash(block_number).map_err(FilterMapActivationError::Lookup)? !=
                Some(pointer.block_hash) ||
                previous_index.is_some_and(|index| index >= pointer.first_log_value_index)
            {
                return Err(FilterMapStorageError::PointerMismatch(block_number).into())
            }
            if expected.get(&block_number).is_some_and(|expected| *expected != pointer) {
                return Err(FilterMapStorageError::PointerMismatch(block_number).into())
            }
            previous_index = Some(pointer.first_log_value_index);
            pointers.insert(block_number, pointer);
        }
    }
    for (block_number, expected) in expected {
        let stored = tx
            .get::<FilterMapBlockPointers>(block_number)
            .map_err(FilterMapStorageError::from)?
            .ok_or(FilterMapStorageError::MissingPointer(block_number))?;
        if stored != expected {
            return Err(FilterMapStorageError::PointerMismatch(block_number).into())
        }
    }
    Ok(pointers)
}

fn insert_expected<E>(
    pointers: &mut BTreeMap<u64, StoredBlockPointer>,
    block_number: u64,
    pointer: StoredBlockPointer,
) -> std::result::Result<(), FilterMapActivationError<E>> {
    if let Some(existing) = pointers.insert(block_number, pointer) &&
        existing != pointer
    {
        return Err(FilterMapStorageError::PointerMismatch(block_number).into())
    }
    Ok(())
}
