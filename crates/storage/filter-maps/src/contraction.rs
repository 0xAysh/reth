//! Atomic coverage contraction and checked cleanup range calculations.

use crate::{
    codec::catalog_to_db,
    error::{FilterMapStorageError, Result},
    validation::{load_coverage, load_identity},
};
use reth_db_api::{
    models::FilterMapRowKeyRange,
    tables::FilterMapCoverage,
    transaction::{DbTx, DbTxMut},
};
use reth_filter_maps::coverage::{IndexIdentity, MapResumeAnchor, ReorgContraction};
use std::ops::RangeInclusive;

const SINGLETON_KEY: u8 = 0;

/// Contracts query visibility before rebuilding a changed canonical branch.
pub fn contract_for_reorg<TX>(
    tx: &TX,
    identity: &IndexIdentity,
    earliest_changed: u64,
    safe_anchor: Option<MapResumeAnchor>,
) -> Result<ReorgContraction>
where
    TX: DbTx + DbTxMut,
{
    let stored = load_identity(tx, identity)?;
    let mut coverage = load_coverage(tx, &stored)?;
    let outcome = coverage.contract_for_reorg(earliest_changed, safe_anchor)?;
    tx.put::<FilterMapCoverage>(SINGLETON_KEY, catalog_to_db(coverage.stored_record()))?;
    Ok(outcome)
}

/// Atomically drops visibility through a published tail anchor.
pub fn retain_after<TX>(tx: &TX, identity: &IndexIdentity, tail: MapResumeAnchor) -> Result<()>
where
    TX: DbTx + DbTxMut,
{
    let stored = load_identity(tx, identity)?;
    let mut coverage = load_coverage(tx, &stored)?;
    coverage.retain_after(tail)?;
    tx.put::<FilterMapCoverage>(SINGLETON_KEY, catalog_to_db(coverage.stored_record()))?;
    Ok(())
}

/// Checked physical key ranges that a later cleanup scheduler may consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupRanges {
    /// Complete row-key epochs safe for bounded row-table traversal.
    pub row_epochs: Vec<FilterMapRowKeyRange>,
    /// Directory keys.
    pub directories: RangeInclusive<u32>,
    /// Anchor keys.
    pub anchors: RangeInclusive<u32>,
    /// Pointer keys.
    pub pointers: RangeInclusive<u64>,
}

impl CleanupRanges {
    /// Computes cleanup ranges without deleting any record.
    pub fn checked(
        params_id: reth_filter_maps::ParamsId,
        first_epoch: u32,
        last_epoch: u32,
        maps: RangeInclusive<u32>,
        pointers: RangeInclusive<u64>,
    ) -> Result<Self> {
        if first_epoch > last_epoch || maps.is_empty() || pointers.is_empty() {
            return Err(FilterMapStorageError::Arithmetic)
        }
        let mut row_epochs = Vec::with_capacity(
            usize::try_from(last_epoch - first_epoch).unwrap_or(usize::MAX).saturating_add(1),
        );
        for epoch in first_epoch..=last_epoch {
            row_epochs.push(FilterMapRowKeyRange::complete_epoch(params_id.into(), epoch)?);
        }
        Ok(Self { row_epochs, directories: maps.clone(), anchors: maps, pointers })
    }
}
