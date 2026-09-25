//! Checked cleanup range calculations for records hidden by coverage contraction.
//!
//! Contraction itself is a [`FilterMapStore`](crate::FilterMapStore) operation; these ranges only
//! describe what a later cleanup scheduler may delete.

use crate::error::{FilterMapStorageError, Result};
use reth_db_api::models::FilterMapRowKeyRange;
use std::ops::RangeInclusive;

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
