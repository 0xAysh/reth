//! Segment-scoped implementation of the pure matcher read seam.

use crate::{error::FilterMapStorageError, snapshot::ActivatedFilterMapSnapshot};
use reth_db_api::{
    models::{FilterMapBaseRowKey, FilterMapExtendedRowKey},
    tables::{FilterMapBaseRows, FilterMapExtendedRows},
    transaction::DbTx,
};
use reth_filter_maps::{FilterMapMatchSource, ParamsId};
use std::{collections::BTreeMap, ops::RangeInclusive};

/// MDBX-backed matcher source pinned to one activated segment and one read transaction.
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
        let segment = snapshot.queryable.coverage().segments().get(segment_index).ok_or(
            FilterMapStorageError::OutsideSource { kind: "segment", key: segment_index as u64 },
        )?;
        let maps = segment.maps();
        let blocks = segment.blocks().ok_or(FilterMapStorageError::OutsideSource {
            kind: "empty segment",
            key: segment_index as u64,
        })?;
        let successor = blocks.end().checked_add(1).ok_or(FilterMapStorageError::Arithmetic)?;
        let pointers = *blocks.start()..=successor;
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
        self.snapshot.identity.params
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

        let params_id = u8::from(self.params_id());
        let mut groups = BTreeMap::new();
        for &map_index in map_indices {
            let directory = &self.snapshot.directories[&map_index];
            if directory.is_nonempty(row_index) {
                let key = FilterMapBaseRowKey::new(params_id, map_index, row_index)?;
                let coordinates = key.validate(params_id)?;
                let slot = FilterMapBaseRowKey::slot(params_id, map_index)?;
                if coordinates.group_start.checked_add(
                    u32::try_from(slot).map_err(|_| FilterMapStorageError::Arithmetic)?,
                ) != Some(map_index) ||
                    coordinates.row_index != row_index
                {
                    return Err(FilterMapStorageError::KeyValueMismatch {
                        kind: "base row key",
                        key: key.get(),
                    })
                }
                if let std::collections::btree_map::Entry::Vacant(entry) = groups.entry(key) {
                    let group =
                        self.snapshot.tx.get::<FilterMapBaseRows>(key)?.ok_or(
                            FilterMapStorageError::MissingBaseRow { map_index, row_index },
                        )?;
                    if group.params_id != params_id {
                        return Err(FilterMapStorageError::KeyValueMismatch {
                            kind: "base row parameter identity",
                            key: key.get(),
                        })
                    }
                    entry.insert(group);
                }
            }
        }

        let limit = usize::try_from(max_columns).unwrap_or(usize::MAX);
        let mut rows = Vec::with_capacity(map_indices.len());
        for &map_index in map_indices {
            let directory = &self.snapshot.directories[&map_index];
            if !directory.is_nonempty(row_index) {
                rows.push(Vec::new());
                continue
            }
            let extended = directory.is_extended(row_index);
            let expected_length = usize::try_from(
                directory
                    .row_length(row_index)
                    .ok_or(FilterMapStorageError::PayloadCountMismatch(map_index))?,
            )
            .map_err(|_| FilterMapStorageError::Arithmetic)?;
            let key = FilterMapBaseRowKey::new(params_id, map_index, row_index)?;
            let group = &groups[&key];
            let slot_index = FilterMapBaseRowKey::slot(params_id, map_index)?;
            let base = group
                .slots
                .get(slot_index)
                .ok_or(FilterMapStorageError::MissingBaseRow { map_index, row_index })?;
            if base.is_empty() || (extended && base.len() != params.base_row_length() as usize) {
                return Err(FilterMapStorageError::MissingBaseRow { map_index, row_index })
            }

            let mut row = base.iter().copied().take(limit).collect::<Vec<_>>();
            if !extended {
                if base.len() != expected_length {
                    return Err(FilterMapStorageError::PayloadCountMismatch(map_index))
                }
                rows.push(row);
                continue
            }
            // The directory and full base slot prove every requested base-layer column. Deferring
            // the extension avoids unrelated I/O and lets a base-layer query succeed even if a
            // higher-layer payload needs repair.
            if limit <= base.len() {
                rows.push(row);
                continue
            }

            let extension_key = FilterMapExtendedRowKey::new(params_id, map_index, row_index)?;
            let coordinates = extension_key.validate(params_id)?;
            if coordinates.map_index != map_index || coordinates.row_index != row_index {
                return Err(FilterMapStorageError::KeyValueMismatch {
                    kind: "extension row key",
                    key: extension_key.get(),
                })
            }
            let extension = self
                .snapshot
                .tx
                .get::<FilterMapExtendedRows>(extension_key)?
                .ok_or(FilterMapStorageError::MissingExtension { map_index, row_index })?;
            if extension.params_id != params_id {
                return Err(FilterMapStorageError::KeyValueMismatch {
                    kind: "extension parameter identity",
                    key: extension_key.get(),
                })
            }
            let row_length = base
                .len()
                .checked_add(extension.columns.len())
                .ok_or(FilterMapStorageError::Arithmetic)?;
            if row_length != expected_length {
                return Err(FilterMapStorageError::PayloadCountMismatch(map_index))
            }
            row.extend(extension.columns.into_iter().take(limit - row.len()));
            rows.push(row);
        }
        Ok(rows)
    }

    fn block_pointer(&mut self, block_number: u64) -> Result<u64, Self::Error> {
        if !self.pointers.contains(&block_number) {
            return Err(FilterMapStorageError::OutsideSource {
                kind: "block pointer",
                key: block_number,
            })
        }
        self.snapshot
            .pointers
            .get(&block_number)
            .map(|pointer| pointer.first_log_value_index)
            .ok_or(FilterMapStorageError::MissingPointer(block_number))
    }
}
