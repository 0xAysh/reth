//! Atomic coverage contraction and checked cleanup range calculations.

use crate::{
    codec::catalog_to_db,
    error::{FilterMapStorageError, Result},
    restore::load_metadata,
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
    let mut coverage = load_metadata(tx, identity)?.coverage;
    let outcome = coverage.contract_for_reorg(earliest_changed, safe_anchor)?;
    tx.put::<FilterMapCoverage>(SINGLETON_KEY, catalog_to_db(coverage.stored_record()))?;
    Ok(outcome)
}

/// Atomically drops visibility through a published tail anchor.
pub fn retain_after<TX>(tx: &TX, identity: &IndexIdentity, tail: MapResumeAnchor) -> Result<()>
where
    TX: DbTx + DbTxMut,
{
    let mut coverage = load_metadata(tx, identity)?.coverage;
    coverage.retain_after(tail)?;
    tx.put::<FilterMapCoverage>(SINGLETON_KEY, catalog_to_db(coverage.stored_record()))?;
    Ok(())
}

/// Checked physical key ranges that a later cleanup scheduler may consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupRanges {
    params_id: reth_filter_maps::ParamsId,
    /// Complete epochs safe for bounded row-table traversal.
    pub row_epochs: RangeInclusive<u32>,
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
        // Validate both endpoints now; intermediate epochs use the same checked formula.
        FilterMapRowKeyRange::complete_epoch(params_id.into(), first_epoch)?;
        FilterMapRowKeyRange::complete_epoch(params_id.into(), last_epoch)?;
        Ok(Self {
            params_id,
            row_epochs: first_epoch..=last_epoch,
            directories: maps.clone(),
            anchors: maps,
            pointers,
        })
    }

    /// Returns the checked physical row-key interval for a selected complete epoch.
    pub fn row_epoch_range(&self, epoch: u32) -> Result<FilterMapRowKeyRange> {
        if !self.row_epochs.contains(&epoch) {
            return Err(FilterMapStorageError::Arithmetic)
        }
        Ok(FilterMapRowKeyRange::complete_epoch(self.params_id.into(), epoch)?)
    }
}
