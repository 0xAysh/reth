//! The single owner of the logical-map <-> physical row layout.
//!
//! A completed map is stored as three kinds of record:
//!
//! ```text
//! logical row r of map m ──┬─► directory[m]: nonempty bit, extended bit, exact length of r
//!                          ├─► base group (m's epoch-local group, r)[slot of m]: first base_row_length columns
//!                          └─► extension (m, r): remaining columns, only when the row is extended
//! ```
//!
//! Encoding and decoding live together so the split point, the group/slot arithmetic, and the
//! per-row length check cannot drift apart between publication, retry comparison, and queries.
//! The directory is authoritative: every decoded row is checked against its declared length, so a
//! payload that disagrees with the directory is corruption even when the map's total mark count
//! happens to agree.

use crate::error::{FilterMapStorageError, Result};
use reth_db_api::{
    models::{
        FilterMapBaseRowKey, FilterMapExtendedRowKey, StoredBaseRowGroup, StoredExtendedRow,
        StoredMapRowDirectory,
    },
    tables::{FilterMapBaseRows, FilterMapDirectories, FilterMapExtendedRows},
    transaction::DbTx,
};
use reth_filter_maps::ParamsId;
use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};

/// One completed map's logical rows together with the directory that describes them durably.
#[derive(Debug)]
pub(crate) struct EncodedMap {
    params: ParamsId,
    map_index: u32,
    directory: StoredMapRowDirectory,
    rows: BTreeMap<u32, Vec<u32>>,
}

impl EncodedMap {
    /// Validates sparse logical rows and derives their directory.
    ///
    /// Rows must be strictly ascending, nonempty, and inside the map's height and width.
    pub(crate) fn new<'a>(
        params: ParamsId,
        map_index: u32,
        rows: impl IntoIterator<Item = (u32, &'a [u32])>,
    ) -> Result<Self> {
        let geometry = params.params();
        let mut nonempty = vec![0u8; geometry.map_height() as usize / 8];
        let mut extended = vec![0u8; nonempty.len()];
        let mut logical = BTreeMap::new();
        let mut row_lengths = Vec::new();
        let mut marks = 0u64;
        let mut previous = None;
        for (row_index, columns) in rows {
            if row_index >= geometry.map_height() ||
                previous.is_some_and(|previous| row_index <= previous) ||
                columns.is_empty() ||
                columns.iter().any(|&column| column >= geometry.map_width())
            {
                return Err(FilterMapStorageError::MalformedRow { map_index, row_index })
            }
            previous = Some(row_index);
            let row_length =
                u32::try_from(columns.len()).map_err(|_| FilterMapStorageError::Arithmetic)?;
            marks = marks
                .checked_add(u64::from(row_length))
                .ok_or(FilterMapStorageError::Arithmetic)?;
            row_lengths.push(row_length);
            set_bit(&mut nonempty, row_index);
            if row_length > geometry.base_row_length() {
                set_bit(&mut extended, row_index);
            }
            logical.insert(row_index, columns.to_vec());
        }
        let directory = StoredMapRowDirectory::new(
            params.into(),
            nonempty,
            extended,
            u32::try_from(logical.len()).map_err(|_| FilterMapStorageError::Arithmetic)?,
            marks,
            row_lengths,
        )?;
        Ok(Self { params, map_index, directory, rows: logical })
    }

    /// Returns the encoded map's index.
    pub(crate) const fn map_index(&self) -> u32 {
        self.map_index
    }

    /// Returns the map's durable directory.
    pub(crate) const fn directory(&self) -> &StoredMapRowDirectory {
        &self.directory
    }

    /// Stages the base-slot and extension edits that replace whatever `tx` holds for this map.
    ///
    /// Rows declared by the map's previous directory are cleared even when the new map leaves them
    /// empty, so stale payload never survives a replacement. Other maps' slots in a shared base
    /// group are preserved byte for byte.
    pub(crate) fn stage<TX: DbTx>(&self, tx: &TX, writes: &mut RowWrites) -> Result<()> {
        let params_id = u8::from(self.params);
        let geometry = self.params.params();
        let map_index = self.map_index;
        let old_directory = tx.get::<FilterMapDirectories>(map_index)?;
        if old_directory.as_ref().is_some_and(|directory| directory.params_id != params_id) {
            return Err(FilterMapStorageError::KeyValueMismatch {
                kind: "stale directory parameter identity",
                key: u64::from(map_index),
            })
        }
        let mut affected = BTreeSet::new();
        if let Some(directory) = &old_directory {
            affected.extend((0..geometry.map_height()).filter(|&row| directory.is_nonempty(row)));
        }
        affected.extend(self.rows.keys().copied());

        let slot = FilterMapBaseRowKey::slot(params_id, map_index)?;
        let split_at = geometry.base_row_length() as usize;
        for row_index in affected {
            let base_key = FilterMapBaseRowKey::new(params_id, map_index, row_index)?;
            let group = match writes.base_groups.entry(base_key) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let group = tx
                        .get::<FilterMapBaseRows>(base_key)?
                        .map_or_else(|| StoredBaseRowGroup::empty(params_id), Ok)?;
                    if group.params_id != params_id {
                        return Err(FilterMapStorageError::KeyValueMismatch {
                            kind: "base group parameter identity",
                            key: base_key.get(),
                        })
                    }
                    entry.insert(group)
                }
            };
            let extension_key = FilterMapExtendedRowKey::new(params_id, map_index, row_index)?;
            let columns = self.rows.get(&row_index).map_or(&[][..], Vec::as_slice);
            let (base, extension) = columns.split_at(columns.len().min(split_at));
            group.slots[slot] = base.to_vec();
            let extension = if extension.is_empty() {
                None
            } else {
                Some(StoredExtendedRow::new(params_id, extension.to_vec())?)
            };
            writes.extensions.insert(extension_key, extension);
        }
        Ok(())
    }

    /// Returns whether `tx` already holds exactly this map's directory and rows.
    ///
    /// Stale payload in a row the directory declares empty or base-only means the prior state is
    /// not this map, so it compares unequal. Payload that contradicts the directory's own row
    /// lengths is corruption and is returned as an error.
    pub(crate) fn is_stored<TX: DbTx>(&self, tx: &TX) -> Result<bool> {
        if tx.get::<FilterMapDirectories>(self.map_index)?.as_ref() != Some(&self.directory) {
            return Ok(false)
        }
        match RowReader::new(tx, self.params).read_map(self.map_index, &self.directory) {
            Ok(rows) => Ok(rows == self.rows),
            Err(FilterMapStorageError::ContradictedPayload { .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }
}

/// Staged base-group and extension records for one publication.
///
/// An extension staged as `None` must be deleted.
#[derive(Debug, Default)]
pub(crate) struct RowWrites {
    pub base_groups: BTreeMap<FilterMapBaseRowKey, StoredBaseRowGroup>,
    pub extensions: BTreeMap<FilterMapExtendedRowKey, Option<StoredExtendedRow>>,
}

/// How much of a row a [`RowReader`] decodes and how much stale payload it tolerates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowRead {
    /// The complete row; payload in a directory-empty row or an extension on a base-only row is
    /// [`FilterMapStorageError::ContradictedPayload`].
    Strict,
    /// At most this many leading columns. Rows the directory declares empty are not read, and an
    /// extension is read only when the prefix reaches past the base slot.
    Prefix(usize),
}

/// Decodes rows of maps under one parameter set from one transaction.
///
/// Base groups are cached, so reading the same row of several maps in one group costs one lookup.
#[derive(Debug)]
pub(crate) struct RowReader<'tx, TX> {
    tx: &'tx TX,
    params: ParamsId,
    groups: BTreeMap<FilterMapBaseRowKey, Option<StoredBaseRowGroup>>,
}

impl<'tx, TX: DbTx> RowReader<'tx, TX> {
    /// Creates a reader with an empty base-group cache.
    pub(crate) const fn new(tx: &'tx TX, params: ParamsId) -> Self {
        Self { tx, params, groups: BTreeMap::new() }
    }

    /// Decodes one row of `map_index`, checked against the length `directory` declares for it.
    pub(crate) fn read_row(
        &mut self,
        map_index: u32,
        directory: &StoredMapRowDirectory,
        row_index: u32,
        mode: RowRead,
    ) -> Result<Vec<u32>> {
        let base_row_length = self.params.params().base_row_length() as usize;
        let (strict, limit) = match mode {
            RowRead::Strict => (true, usize::MAX),
            RowRead::Prefix(limit) => (false, limit),
        };
        let Some(expected) = directory.row_length(row_index) else {
            if strict &&
                (self.base_slot(map_index, row_index)?.is_some_and(|base| !base.is_empty()) ||
                    self.extension(map_index, row_index)?.is_some())
            {
                return Err(FilterMapStorageError::ContradictedPayload { map_index, row_index })
            }
            return Ok(Vec::new())
        };
        let expected = usize::try_from(expected).map_err(|_| FilterMapStorageError::Arithmetic)?;
        let extended = directory.is_extended(row_index);

        let (base_length, mut row) = match self.base_slot(map_index, row_index)? {
            Some(base) if !base.is_empty() => {
                (base.len(), base.iter().copied().take(limit).collect::<Vec<_>>())
            }
            _ => return Err(FilterMapStorageError::MissingBaseRow { map_index, row_index }),
        };
        if !extended {
            if base_length != expected {
                return Err(FilterMapStorageError::PayloadCountMismatch(map_index))
            }
            if strict && self.extension(map_index, row_index)?.is_some() {
                return Err(FilterMapStorageError::ContradictedPayload { map_index, row_index })
            }
            return Ok(row)
        }
        if base_length != base_row_length {
            return Err(FilterMapStorageError::MissingBaseRow { map_index, row_index })
        }
        // The directory and a full base slot prove every base-layer column. Deferring the
        // extension avoids unrelated I/O and lets a base-layer query succeed even if a
        // higher-layer payload needs repair.
        if limit <= base_length {
            return Ok(row)
        }
        let extension = self
            .extension(map_index, row_index)?
            .ok_or(FilterMapStorageError::MissingExtension { map_index, row_index })?;
        if base_length.checked_add(extension.columns.len()) != Some(expected) {
            return Err(FilterMapStorageError::PayloadCountMismatch(map_index))
        }
        row.extend(extension.columns.into_iter().take(limit - row.len()));
        Ok(row)
    }

    /// Strictly decodes every row of `map_index`, returning the nonempty ones.
    pub(crate) fn read_map(
        &mut self,
        map_index: u32,
        directory: &StoredMapRowDirectory,
    ) -> Result<BTreeMap<u32, Vec<u32>>> {
        let mut rows = BTreeMap::new();
        for row_index in 0..self.params.params().map_height() {
            let row = self.read_row(map_index, directory, row_index, RowRead::Strict)?;
            if !row.is_empty() {
                rows.insert(row_index, row);
            }
        }
        Ok(rows)
    }

    fn base_slot(&mut self, map_index: u32, row_index: u32) -> Result<Option<&Vec<u32>>> {
        let params_id = u8::from(self.params);
        let key = FilterMapBaseRowKey::new(params_id, map_index, row_index)?;
        let slot = FilterMapBaseRowKey::slot(params_id, map_index)?;
        let group = match self.groups.entry(key) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let group = self.tx.get::<FilterMapBaseRows>(key)?;
                if group.as_ref().is_some_and(|group| group.params_id != params_id) {
                    return Err(FilterMapStorageError::KeyValueMismatch {
                        kind: "base row parameter identity",
                        key: key.get(),
                    })
                }
                entry.insert(group)
            }
        };
        Ok(group.as_ref().and_then(|group| group.slots.get(slot)))
    }

    fn extension(&self, map_index: u32, row_index: u32) -> Result<Option<StoredExtendedRow>> {
        let params_id = u8::from(self.params);
        let key = FilterMapExtendedRowKey::new(params_id, map_index, row_index)?;
        let extension = self.tx.get::<FilterMapExtendedRows>(key)?;
        if extension.as_ref().is_some_and(|extension| extension.params_id != params_id) {
            return Err(FilterMapStorageError::KeyValueMismatch {
                kind: "extension parameter identity",
                key: key.get(),
            })
        }
        Ok(extension)
    }
}

fn set_bit(bitmap: &mut [u8], row: u32) {
    bitmap[row as usize / 8] |= 1 << (row % 8);
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_db::test_utils::create_test_rw_db;
    use reth_db_api::{database::Database, transaction::DbTxMut};

    const PARAMS: ParamsId = ParamsId::Default;
    /// Slot 1 of the base group starting at map 32, next to [`NEIGHBOUR`] in slot 0.
    const MAP: u32 = 33;
    const NEIGHBOUR: u32 = 32;
    const BASE_ONLY: u32 = 5;
    const SHORT: u32 = 9;
    const EMPTY: u32 = 6;
    const EXTENDED: u32 = 700;
    const FULL_BASE: u32 = 65_535;

    fn logical_rows() -> Vec<(u32, Vec<u32>)> {
        vec![
            (BASE_ONLY, vec![1, 2, 3]),
            (SHORT, vec![4, 4]),
            (EXTENDED, (0..20).map(|column| column % 7).collect()),
            (FULL_BASE, vec![7; 8]),
        ]
    }

    fn encode(map_index: u32, rows: &[(u32, Vec<u32>)]) -> EncodedMap {
        EncodedMap::new(PARAMS, map_index, rows.iter().map(|(row, columns)| (*row, &columns[..])))
            .unwrap()
    }

    fn store<TX: DbTx + DbTxMut>(tx: &TX, maps: &[&EncodedMap]) {
        let mut writes = RowWrites::default();
        for map in maps {
            map.stage(tx, &mut writes).unwrap();
        }
        for (key, group) in writes.base_groups {
            tx.put::<FilterMapBaseRows>(key, group).unwrap();
        }
        for (key, extension) in writes.extensions {
            tx.delete::<FilterMapExtendedRows>(key, None).unwrap();
            if let Some(extension) = extension {
                tx.put::<FilterMapExtendedRows>(key, extension).unwrap();
            }
        }
        for map in maps {
            tx.put::<FilterMapDirectories>(map.map_index(), map.directory().clone()).unwrap();
        }
    }

    fn base_key(row_index: u32) -> FilterMapBaseRowKey {
        FilterMapBaseRowKey::new(PARAMS.into(), MAP, row_index).unwrap()
    }

    fn extension_key(row_index: u32) -> FilterMapExtendedRowKey {
        FilterMapExtendedRowKey::new(PARAMS.into(), MAP, row_index).unwrap()
    }

    fn edit_slot<TX: DbTx + DbTxMut>(tx: &TX, row_index: u32, edit: impl FnOnce(&mut Vec<u32>)) {
        let key = base_key(row_index);
        let mut group = tx
            .get::<FilterMapBaseRows>(key)
            .unwrap()
            .unwrap_or_else(|| StoredBaseRowGroup::empty(PARAMS.into()).unwrap());
        edit(&mut group.slots[FilterMapBaseRowKey::slot(PARAMS.into(), MAP).unwrap()]);
        tx.put::<FilterMapBaseRows>(key, group).unwrap();
    }

    #[test]
    fn encoded_maps_round_trip_through_shared_base_groups() {
        let db = create_test_rw_db();
        let rows = logical_rows();
        let map = encode(MAP, &rows);
        let neighbour_rows = vec![(BASE_ONLY, vec![9])];
        let neighbour = encode(NEIGHBOUR, &neighbour_rows);
        assert!(map.directory().is_extended(EXTENDED));
        assert!(!map.directory().is_extended(FULL_BASE));

        let tx = db.tx_mut().unwrap();
        store(&tx, &[&neighbour, &map]);
        tx.commit().unwrap();

        let tx = db.tx().unwrap();
        assert!(map.is_stored(&tx).unwrap());
        assert!(neighbour.is_stored(&tx).unwrap());
        let mut reader = RowReader::new(&tx, PARAMS);
        assert_eq!(reader.read_map(MAP, map.directory()).unwrap(), rows.into_iter().collect());
        let extended = &map.rows[&EXTENDED];
        for (limit, expected) in [(3, &extended[..3]), (8, &extended[..8]), (100, &extended[..])] {
            assert_eq!(
                reader.read_row(MAP, map.directory(), EXTENDED, RowRead::Prefix(limit)).unwrap(),
                expected,
                "prefix {limit}"
            );
        }
        assert!(reader
            .read_row(MAP, map.directory(), EMPTY, RowRead::Prefix(8))
            .unwrap()
            .is_empty());
        drop(tx);

        // Replacement clears rows the new map no longer declares, drops an obsolete extension,
        // and leaves the neighbouring slot in the shared group untouched.
        let replacement_rows = vec![(EXTENDED, vec![1])];
        let replacement = encode(MAP, &replacement_rows);
        let tx = db.tx_mut().unwrap();
        store(&tx, &[&replacement]);
        tx.commit().unwrap();

        let tx = db.tx().unwrap();
        assert!(replacement.is_stored(&tx).unwrap());
        assert!(neighbour.is_stored(&tx).unwrap());
        assert!(tx.get::<FilterMapExtendedRows>(extension_key(EXTENDED)).unwrap().is_none());
    }

    #[test]
    fn decoding_rejects_row_lengths_permuted_against_the_directory() {
        let db = create_test_rw_db();
        let map = encode(MAP, &logical_rows());
        let tx = db.tx_mut().unwrap();
        store(&tx, &[&map]);
        // Rows BASE_ONLY and SHORT swap declared lengths; the map's total mark count is unchanged.
        let directory = map.directory().clone();
        let mut row_lengths = directory.row_lengths;
        row_lengths.swap(0, 1);
        let permuted = StoredMapRowDirectory::new(
            directory.params_id,
            directory.nonempty,
            directory.extended,
            directory.nonempty_row_count,
            directory.logical_mark_count,
            row_lengths,
        )
        .unwrap();
        tx.put::<FilterMapDirectories>(MAP, permuted.clone()).unwrap();
        tx.commit().unwrap();

        let tx = db.tx().unwrap();
        let mut reader = RowReader::new(&tx, PARAMS);
        assert!(matches!(
            reader.read_map(MAP, &permuted),
            Err(FilterMapStorageError::PayloadCountMismatch(MAP))
        ));
        assert!(matches!(
            reader.read_row(MAP, &permuted, SHORT, RowRead::Prefix(1)),
            Err(FilterMapStorageError::PayloadCountMismatch(MAP))
        ));
    }

    #[test]
    fn decoding_rejects_payload_that_contradicts_the_directory() {
        type Tx = <reth_db::DatabaseEnv as Database>::TXMut;
        struct Case {
            name: &'static str,
            row: u32,
            corrupt: fn(&Tx),
            rejected: fn(&FilterMapStorageError) -> bool,
            /// A prefix read that must reject the row too, or `None` when prefix reads trust the
            /// directory's empty or base-only declaration and never look at the stale payload.
            prefix: Option<usize>,
        }
        let cases = [
            Case {
                name: "truncated base slot",
                row: BASE_ONLY,
                corrupt: |tx| {
                    edit_slot(tx, BASE_ONLY, |slot| {
                        slot.pop();
                    })
                },
                rejected: |error| matches!(error, FilterMapStorageError::PayloadCountMismatch(MAP)),
                prefix: Some(1),
            },
            Case {
                name: "missing base group",
                row: BASE_ONLY,
                corrupt: |tx| {
                    tx.delete::<FilterMapBaseRows>(base_key(BASE_ONLY), None).unwrap();
                },
                rejected: |error| {
                    matches!(error, FilterMapStorageError::MissingBaseRow { map_index: MAP, .. })
                },
                prefix: Some(1),
            },
            Case {
                name: "stale payload in an empty row",
                row: EMPTY,
                corrupt: |tx| edit_slot(tx, EMPTY, |slot| slot.push(1)),
                rejected: |error| {
                    matches!(
                        error,
                        FilterMapStorageError::ContradictedPayload { map_index: MAP, .. }
                    )
                },
                prefix: None,
            },
            Case {
                name: "stale extension on a base-only row",
                row: SHORT,
                corrupt: |tx| {
                    let stale = StoredExtendedRow::new(PARAMS.into(), vec![1]).unwrap();
                    tx.put::<FilterMapExtendedRows>(extension_key(SHORT), stale).unwrap();
                },
                rejected: |error| {
                    matches!(
                        error,
                        FilterMapStorageError::ContradictedPayload { map_index: MAP, .. }
                    )
                },
                prefix: None,
            },
            Case {
                name: "missing extension",
                row: EXTENDED,
                corrupt: |tx| {
                    tx.delete::<FilterMapExtendedRows>(extension_key(EXTENDED), None).unwrap();
                },
                rejected: |error| {
                    matches!(error, FilterMapStorageError::MissingExtension { map_index: MAP, .. })
                },
                prefix: Some(9),
            },
            Case {
                name: "short extension",
                row: EXTENDED,
                corrupt: |tx| {
                    let key = extension_key(EXTENDED);
                    let mut extension = tx.get::<FilterMapExtendedRows>(key).unwrap().unwrap();
                    extension.columns.pop();
                    tx.put::<FilterMapExtendedRows>(key, extension).unwrap();
                },
                rejected: |error| matches!(error, FilterMapStorageError::PayloadCountMismatch(MAP)),
                prefix: Some(9),
            },
        ];

        for case in cases {
            let db = create_test_rw_db();
            let map = encode(MAP, &logical_rows());
            let tx = db.tx_mut().unwrap();
            store(&tx, &[&map]);
            (case.corrupt)(&tx);
            tx.commit().unwrap();

            let tx = db.tx().unwrap();
            let name = case.name;
            let strict = RowReader::new(&tx, PARAMS).read_map(MAP, map.directory());
            assert!(strict.as_ref().is_err_and(case.rejected), "{name}: strict gave {strict:?}");
            let mut reader = RowReader::new(&tx, PARAMS);
            let lazy = reader.read_row(
                MAP,
                map.directory(),
                case.row,
                RowRead::Prefix(case.prefix.unwrap_or(8)),
            );
            match case.prefix {
                Some(_) => {
                    assert!(lazy.as_ref().is_err_and(case.rejected), "{name}: prefix gave {lazy:?}")
                }
                None => assert_eq!(
                    lazy.unwrap(),
                    map.rows.get(&case.row).cloned().unwrap_or_default(),
                    "{name}"
                ),
            }
        }
    }

    #[test]
    fn base_layer_prefix_does_not_need_the_extension() {
        let db = create_test_rw_db();
        let map = encode(MAP, &logical_rows());
        let tx = db.tx_mut().unwrap();
        store(&tx, &[&map]);
        tx.delete::<FilterMapExtendedRows>(extension_key(EXTENDED), None).unwrap();
        tx.commit().unwrap();

        // The directory and a full base slot prove the whole base layer on their own.
        let tx = db.tx().unwrap();
        assert_eq!(
            RowReader::new(&tx, PARAMS)
                .read_row(MAP, map.directory(), EXTENDED, RowRead::Prefix(8))
                .unwrap(),
            map.rows[&EXTENDED][..8]
        );
    }
}
