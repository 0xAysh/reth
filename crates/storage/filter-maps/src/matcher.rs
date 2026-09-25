//! Segment-scoped implementation of the pure matcher read seam.

use crate::{
    error::FilterMapStorageError,
    rows::{RowRead, RowReader},
    snapshot::ActivatedFilterMapSnapshot,
};
use reth_db_api::{tables::FilterMapBlockPointers, transaction::DbTx};
use reth_filter_maps::{FilterMapMatchSource, ParamsId};
use std::ops::RangeInclusive;

/// MDBX-backed matcher source pinned to one activated segment and one read transaction.
///
/// Only a canonically activated snapshot yields a source:
///
/// ```
/// use reth_db_api::transaction::DbTx;
/// use reth_filter_maps_storage::ActivatedFilterMapSnapshot;
///
/// fn source<TX: DbTx>(snapshot: ActivatedFilterMapSnapshot<TX>) {
///     let _ = snapshot.into_segment_source(0);
/// }
/// ```
///
/// A structurally restored snapshot has not been checked against the current chain:
///
/// ```compile_fail,E0599
/// use reth_db_api::transaction::DbTx;
/// use reth_filter_maps_storage::FilterMapReadSnapshot;
///
/// fn source<TX: DbTx>(snapshot: FilterMapReadSnapshot<TX>) {
///     let _ = snapshot.into_segment_source(0);
/// }
/// ```
#[derive(Debug)]
pub struct FilterMapSegmentSource<TX> {
    snapshot: ActivatedFilterMapSnapshot<TX>,
    maps: RangeInclusive<u32>,
    blocks: RangeInclusive<u64>,
    pointers: RangeInclusive<u64>,
}

impl<TX: DbTx> FilterMapSegmentSource<TX> {
    pub(crate) fn new(
        snapshot: ActivatedFilterMapSnapshot<TX>,
        segment_index: usize,
    ) -> Result<Self, FilterMapStorageError> {
        let segment = snapshot.queryable.segments().get(segment_index).ok_or(
            FilterMapStorageError::OutsideSource { kind: "segment", key: segment_index as u64 },
        )?;
        let maps = segment.maps();
        let blocks = segment.blocks().ok_or(FilterMapStorageError::OutsideSource {
            kind: "empty segment",
            key: segment_index as u64,
        })?;
        // Activation verified exactly the pointer span, which includes the successor of the last
        // covered block that bounds it.
        let pointers = segment.pointer_span();
        Ok(Self { snapshot, maps, blocks, pointers })
    }

    /// Returns the covered block interval pinned by this source.
    pub const fn blocks(&self) -> &RangeInclusive<u64> {
        &self.blocks
    }
}

impl<TX: DbTx + 'static> FilterMapMatchSource for FilterMapSegmentSource<TX> {
    type Error = FilterMapStorageError;

    fn params_id(&self) -> ParamsId {
        self.snapshot.queryable.identity().params
    }

    fn read_row_prefixes(
        &mut self,
        map_indices: &[u32],
        row_index: u32,
        max_columns: u32,
    ) -> Result<Vec<Vec<u32>>, Self::Error> {
        let params = self.params_id().params();
        if row_index >= params.map_height() {
            return Err(FilterMapStorageError::RowOutOfRange(row_index))
        }
        if map_indices.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(FilterMapStorageError::UnorderedMapRequest)
        }
        for &map_index in map_indices {
            if !self.maps.contains(&map_index) {
                return Err(FilterMapStorageError::OutsideSource {
                    kind: "map",
                    key: u64::from(map_index),
                })
            }
            if !self.snapshot.directories.contains_key(&map_index) {
                return Err(FilterMapStorageError::MissingDirectory(map_index))
            }
        }
        if max_columns == 0 {
            return Ok(vec![Vec::new(); map_indices.len()])
        }

        let limit = usize::try_from(max_columns).unwrap_or(usize::MAX);
        let mut reader = RowReader::new(&self.snapshot.tx, self.params_id());
        map_indices
            .iter()
            .map(|&map_index| {
                let directory = &self.snapshot.directories[&map_index];
                reader.read_row(map_index, directory, row_index, RowRead::Prefix(limit))
            })
            .collect()
    }

    fn block_pointer(&mut self, block_number: u64) -> Result<u64, Self::Error> {
        if !self.pointers.contains(&block_number) {
            return Err(FilterMapStorageError::OutsideSource {
                kind: "block pointer",
                key: block_number,
            })
        }
        // Activation verified every pointer of this segment in the same read transaction.
        self.snapshot
            .tx
            .get::<FilterMapBlockPointers>(block_number)?
            .map(|pointer| pointer.first_log_value_index)
            .ok_or(FilterMapStorageError::MissingPointer(block_number))
    }
}
