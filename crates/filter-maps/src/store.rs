//! The `RocksDB` store, in Geth's key layout.
//!
//! The keys are Geth's `fm-` key spaces (`core/rawdb/schema.go`) and the row encodings are Geth's
//! (`core/rawdb/accessors_indexes.go`), so a store can be diffed against a Geth database. Only the
//! range record uses a layout of its own. Every write is one atomic `WriteBatch`.

use crate::{
    matcher::{map_of, FilterMapMatchSource},
    render::{BlockRef, FinishedMap},
    FilterMapsError, DEFAULT_PARAMS,
};
use alloy_eips::BlockNumHash;
use alloy_primitives::B256;
use rocksdb::{DBCompressionType, Options, SnapshotWithThreadMode, WriteBatch, DB};
use std::{ops::RangeInclusive, path::Path};

/// The store format version. A store with another version is wiped at open.
const VERSION: u32 = 1;

/// The range record key (`filterMapsRangeKey`).
const RANGE_KEY: &[u8] = b"fm-R";
/// The row key prefix (`filterMapRowPrefix`).
const ROW_PREFIX: &[u8] = b"fm-r";
/// The last-block-of-map key prefix (`filterMapLastBlockPrefix`).
const LAST_BLOCK_PREFIX: &[u8] = b"fm-b";
/// The block pointer key prefix (`filterMapBlockLVPrefix`).
const POINTER_PREFIX: &[u8] = b"fm-p";

/// The bytes of one column: map width is 24 bits.
const COLUMN_BYTES: usize = 3;

/// The `FilterMaps` store: a plain `RocksDB` with one column family.
#[derive(Debug)]
pub(crate) struct Store {
    db: DB,
}

impl Store {
    /// Opens or creates the store at `path`. A store whose range record cannot be decoded, or has
    /// another version, is wiped.
    pub(crate) fn open(path: &Path) -> Result<Self, FilterMapsError> {
        let mut options = Options::default();
        options.create_if_missing(true);
        // Column values are pseudo-random hashes, so compression saves nothing.
        options.set_compression_type(DBCompressionType::None);
        let store = Self { db: DB::open(&options, path)? };
        if let Some(record) = store.db.get(RANGE_KEY)? &&
            RangeRecord::decode(&record).is_none()
        {
            store.wipe()?;
        }
        Ok(store)
    }

    /// Returns a consistent view of the store.
    pub(crate) fn snapshot(&self) -> StoreSnapshot<'_> {
        StoreSnapshot { snapshot: self.db.snapshot() }
    }

    /// Wipes the store and starts it at `origin`. The origin's pointer is stored with it.
    ///
    /// An origin in the middle of a map gets no last-block record for the map before it, which
    /// Geth writes: the indexer starts every origin at pointer 0, and resuming at the origin's map
    /// takes the origin record instead.
    pub(crate) fn init(&self, origin: BlockRef) -> Result<RangeRecord, FilterMapsError> {
        let map = map_of(origin.pointer)?;
        let range = RangeRecord {
            origin,
            first_block: origin.number,
            after_last_block: origin.number,
            first_map: map,
            after_last_map: map,
        };
        let mut batch = WriteBatch::default();
        wipe(&mut batch);
        batch.put(pointer_key(origin.number), origin.pointer.to_be_bytes());
        batch.put(RANGE_KEY, range.encode());
        self.db.write(batch)?;
        Ok(range)
    }

    /// Deletes every record, so the store is empty.
    pub(crate) fn wipe(&self) -> Result<(), FilterMapsError> {
        let mut batch = WriteBatch::default();
        wipe(&mut batch);
        Ok(self.db.write(batch)?)
    }

    /// Replaces the range record. Records outside the new range are left for later writes to
    /// overwrite.
    pub(crate) fn set_range(&self, range: &RangeRecord) -> Result<(), FilterMapsError> {
        Ok(self.db.put(RANGE_KEY, range.encode())?)
    }

    /// Writes the finished maps of one map group and the extended range in one batch.
    ///
    /// `maps` are consecutive and in one map group. They continue the stored maps or rewrite the
    /// latest group from its start.
    pub(crate) fn write_group(
        &self,
        range: &RangeRecord,
        maps: &[FinishedMap],
    ) -> Result<RangeRecord, FilterMapsError> {
        let params = DEFAULT_PARAMS;
        let (Some(first), Some(last)) = (maps.first(), maps.last()) else { return Ok(*range) };
        let group = params.map_group_index(first.index);
        debug_assert!(
            maps.windows(2).all(|pair| pair[0].index + 1 == pair[1].index) &&
                params.map_group_index(last.index) == group &&
                first.index >= range.first_map &&
                first.index <= range.after_last_map,
            "maps continue the range within one group"
        );

        let mut batch = WriteBatch::default();
        // Every row of the group is rewritten from the buffer, so no stale base row survives a
        // trim. Maps that are not in the buffer encode as empty rows.
        let base_row_length = params.base_row_length() as usize;
        // The rows of each map, in ascending row order, that are not written yet.
        let mut remaining_rows =
            maps.iter().map(|map| map.rows.iter().peekable()).collect::<Vec<_>>();
        let mut base = vec![&[][..]; params.base_row_group_size() as usize];
        for row_index in 0..params.map_height() {
            base.fill(&[]);
            for (map, remaining) in maps.iter().zip(&mut remaining_rows) {
                let Some((_, row)) = remaining.next_if(|(row, _)| *row == row_index) else {
                    continue
                };
                base[params.map_group_offset(map.index) as usize] =
                    &row[..row.len().min(base_row_length)];
                // A reader looks at the extended row only behind a full base part, so that is
                // the only case where a stale extended row must go.
                if row.len() >= base_row_length {
                    let key = row_key(map_row_index(map.index, row_index), false);
                    match &row[base_row_length..] {
                        [] => batch.delete(key),
                        overflow => batch.put(key, encode_columns(overflow)),
                    }
                }
            }
            let key = row_key(map_row_index(group, row_index), true);
            match encode_base_rows(&base) {
                Some(value) => batch.put(key, value),
                None => batch.delete(key),
            }
        }
        for map in maps {
            let last_block = map.last_block;
            batch.put(
                last_block_key(map.index),
                [&last_block.number.to_be_bytes()[..], last_block.hash.as_slice()].concat(),
            );
            for start in &map.pointers {
                batch.put(pointer_key(start.block), start.pointer.to_be_bytes());
            }
        }
        let range = RangeRecord {
            after_last_block: last.last_block.number,
            after_last_map: last.index + 1,
            ..*range
        };
        batch.put(RANGE_KEY, range.encode());
        self.db.write(batch)?;
        Ok(range)
    }

    /// Drops epoch `epoch`, the first stored epoch, and returns the shortened range.
    ///
    /// The last block of the epoch's last map and that block's pointer stay: they are the resume
    /// point of the next epoch's first map (Geth's `deleteTailEpoch`).
    pub(crate) fn drop_epoch(
        &self,
        range: &RangeRecord,
        epoch: u32,
    ) -> Result<RangeRecord, FilterMapsError> {
        let params = DEFAULT_PARAMS;
        let first_map = params.first_epoch_map(epoch);
        let last_map = params.last_epoch_map(epoch);
        let next_map = last_map + 1;
        let last_block = self.snapshot().last_block_of_map(last_map)?.number;

        let mut batch = WriteBatch::default();
        // Row keys of an epoch are contiguous and cover both base and extended rows.
        batch.delete_range(
            row_key(map_row_index(first_map, 0), false),
            row_key(map_row_index(next_map, 0), false),
        );
        batch.delete_range(last_block_key(first_map), last_block_key(last_map));
        batch.delete_range(pointer_key(range.first_block), pointer_key(last_block));
        let range = RangeRecord { first_block: last_block + 1, first_map: next_map, ..*range };
        batch.put(RANGE_KEY, range.encode());
        self.db.write(batch)?;
        Ok(range)
    }
}

/// A consistent view of the store.
pub(crate) struct StoreSnapshot<'a> {
    snapshot: SnapshotWithThreadMode<'a, DB>,
}

impl std::fmt::Debug for StoreSnapshot<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreSnapshot").finish_non_exhaustive()
    }
}

impl StoreSnapshot<'_> {
    /// Returns the range record, or `None` for an empty store.
    pub(crate) fn range(&self) -> Result<Option<RangeRecord>, FilterMapsError> {
        Ok(self.snapshot.get(RANGE_KEY)?.as_deref().and_then(RangeRecord::decode))
    }

    /// Returns the last block of map `map`.
    pub(crate) fn last_block_of_map(&self, map: u32) -> Result<BlockNumHash, FilterMapsError> {
        let value = self
            .snapshot
            .get(last_block_key(map))?
            .ok_or_else(|| FilterMapsError::Corrupt(format!("map {map} has no last block")))?;
        let (number, hash) = value
            .split_first_chunk::<8>()
            .filter(|(_, hash)| hash.len() == 32)
            .ok_or_else(|| FilterMapsError::Corrupt(format!("last block of map {map}")))?;
        Ok(BlockNumHash::new(u64::from_be_bytes(*number), B256::from_slice(hash)))
    }
}

impl FilterMapMatchSource for StoreSnapshot<'_> {
    fn row_prefixes(
        &self,
        map_indices: &[u32],
        row_index: u32,
        max_columns: u32,
    ) -> Result<Vec<Vec<u32>>, FilterMapsError> {
        let params = DEFAULT_PARAMS;
        let base_row_length = params.base_row_length() as usize;
        let mut rows = Vec::with_capacity(map_indices.len());
        for group in
            map_indices.chunk_by(|a, b| params.map_group_index(*a) == params.map_group_index(*b))
        {
            let base_key =
                row_key(map_row_index(params.map_group_index(group[0]), row_index), true);
            let base_rows = match self.snapshot.get(base_key)? {
                Some(value) => decode_base_rows(&value, params.base_row_group_size() as usize)?,
                None => vec![Vec::new(); params.base_row_group_size() as usize],
            };
            for &map in group {
                let mut row = base_rows[params.map_group_offset(map) as usize].clone();
                // The extended row is read only behind a full base part, so a stale extended row of
                // a rewritten map is never read.
                if row.len() == base_row_length &&
                    max_columns as usize > base_row_length &&
                    let Some(value) =
                        self.snapshot.get(row_key(map_row_index(map, row_index), false))?
                {
                    row.extend(decode_columns(&value)?);
                }
                row.truncate(max_columns as usize);
                rows.push(row);
            }
        }
        Ok(rows)
    }

    fn block_pointer(&self, block_number: u64) -> Result<u64, FilterMapsError> {
        let value = self.snapshot.get(pointer_key(block_number))?.ok_or_else(|| {
            FilterMapsError::Corrupt(format!("block {block_number} has no pointer"))
        })?;
        let bytes = <[u8; 8]>::try_from(value.as_slice())
            .map_err(|_| FilterMapsError::Corrupt(format!("pointer of block {block_number}")))?;
        Ok(u64::from_be_bytes(bytes))
    }
}

/// The range record (`fm-R`): the maps and blocks the store covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RangeRecord {
    /// The index origin: the block where the value space starts, and its pointer.
    pub(crate) origin: BlockRef,
    /// The first block whose log values are all in stored maps.
    pub(crate) first_block: u64,
    /// The block after the index head: the last block of the last stored map.
    pub(crate) after_last_block: u64,
    /// The first stored map.
    pub(crate) first_map: u32,
    /// The map after the last stored map.
    pub(crate) after_last_map: u32,
}

impl RangeRecord {
    /// The length of the encoded record.
    const ENCODED_LEN: usize = 4 + 8 + 32 + 8 + 8 + 8 + 4 + 4;

    /// Returns the blocks whose log values are all in stored maps.
    pub(crate) fn indexed_blocks(&self) -> Option<RangeInclusive<u64>> {
        (self.first_block < self.after_last_block)
            .then(|| self.first_block..=self.after_last_block - 1)
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::ENCODED_LEN);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&self.origin.number.to_le_bytes());
        out.extend_from_slice(self.origin.hash.as_slice());
        out.extend_from_slice(&self.origin.pointer.to_le_bytes());
        out.extend_from_slice(&self.first_block.to_le_bytes());
        out.extend_from_slice(&self.after_last_block.to_le_bytes());
        out.extend_from_slice(&self.first_map.to_le_bytes());
        out.extend_from_slice(&self.after_last_map.to_le_bytes());
        out
    }

    /// Decodes a record of this version, or returns `None`.
    fn decode(bytes: &[u8]) -> Option<Self> {
        let mut fields = FieldReader(bytes);
        if u32::from_le_bytes(fields.take()?) != VERSION {
            return None
        }
        let range = Self {
            origin: BlockRef {
                number: u64::from_le_bytes(fields.take()?),
                hash: B256::from(fields.take::<32>()?),
                pointer: u64::from_le_bytes(fields.take()?),
            },
            first_block: u64::from_le_bytes(fields.take()?),
            after_last_block: u64::from_le_bytes(fields.take()?),
            first_map: u32::from_le_bytes(fields.take()?),
            after_last_map: u32::from_le_bytes(fields.take()?),
        };
        fields.0.is_empty().then_some(range)
    }
}

/// Reads fixed-size fields off the front of a byte slice.
struct FieldReader<'a>(&'a [u8]);

impl FieldReader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (field, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*field)
    }
}

/// Returns the storage index of row `row` of map `map` (Geth's `mapRowIndex`). Rows of one epoch
/// are contiguous, and within a row the maps of an epoch are contiguous.
fn map_row_index(map: u32, row: u32) -> u64 {
    let params = DEFAULT_PARAMS;
    let epoch = u64::from(params.map_epoch(map));
    let map_in_epoch = u64::from(map - params.first_epoch_map(params.map_epoch(map)));
    (((epoch << params.log_map_height()) + u64::from(row)) << params.log_maps_per_epoch()) +
        map_in_epoch
}

/// Deletes every `fm-` key, the range record included.
fn wipe(batch: &mut WriteBatch) {
    // `.` follows `-` in ASCII, so the range ends right after the last `fm-` key.
    batch.delete_range(b"fm-", b"fm.");
}

/// Returns the key of a row: the base row group key carries a trailing zero byte.
fn row_key(map_row_index: u64, base: bool) -> Vec<u8> {
    let mut key = [ROW_PREFIX, &map_row_index.to_be_bytes()].concat();
    if base {
        key.push(0);
    }
    key
}

fn last_block_key(map: u32) -> Vec<u8> {
    [LAST_BLOCK_PREFIX, &map.to_be_bytes()].concat()
}

fn pointer_key(block: u64) -> Vec<u8> {
    [POINTER_PREFIX, &block.to_be_bytes()].concat()
}

/// Encodes the base parts of one row for every map of a group (Geth's `WriteFilterMapBaseRows`):
/// a header with one set bit per column and one clear bit after every row but the last nonempty
/// one, then the columns. Returns `None` when every row is empty.
fn encode_base_rows(rows: &[&[u32]]) -> Option<Vec<u8>> {
    let last = rows.iter().rposition(|row| !row.is_empty())?;
    let columns = rows.iter().map(|row| row.len()).sum::<usize>();
    let header_len = (last + columns).div_ceil(8);
    let mut out = vec![0; header_len];
    out.reserve(columns * COLUMN_BYTES);
    let mut bit = 0;
    for (index, row) in rows[..=last].iter().enumerate() {
        for column in *row {
            out[bit / 8] |= 1 << (bit % 8);
            bit += 1;
            out.extend_from_slice(&column.to_le_bytes()[..COLUMN_BYTES]);
        }
        // Rows are separated by a clear bit; nothing follows the last nonempty row.
        if index < last {
            bit += 1;
        }
    }
    Some(out)
}

/// Decodes a base row group into `row_count` rows (Geth's `ReadFilterMapBaseRows`).
///
/// The header's length is not stored: header bits are read until the header and the columns they
/// announce fill the value exactly.
fn decode_base_rows(value: &[u8], row_count: usize) -> Result<Vec<Vec<u32>>, FilterMapsError> {
    let corrupt = || FilterMapsError::Corrupt("base row group".to_owned());
    let mut lengths = vec![0usize; row_count];
    let mut columns = 0;
    let mut row = 0;
    let mut header_len = 0;
    let mut header_bits = 0;
    let mut header_byte = 0u8;
    while header_len + COLUMN_BYTES * columns < value.len() {
        if header_bits == 0 {
            header_byte = value[header_len];
            header_len += 1;
            header_bits = 8;
        }
        if header_byte & 1 == 1 {
            *lengths.get_mut(row).ok_or_else(corrupt)? += 1;
            columns += 1;
        } else {
            row += 1;
        }
        header_byte >>= 1;
        header_bits -= 1;
    }
    if header_len + COLUMN_BYTES * columns != value.len() {
        return Err(corrupt())
    }
    let mut entries = decode_columns(&value[header_len..])?.into_iter();
    Ok(lengths.into_iter().map(|length| entries.by_ref().take(length).collect()).collect())
}

/// Decodes 3-byte little-endian columns (Geth's `ReadFilterMapExtRow`).
fn decode_columns(value: &[u8]) -> Result<Vec<u32>, FilterMapsError> {
    if !value.len().is_multiple_of(COLUMN_BYTES) {
        return Err(FilterMapsError::Corrupt("row length".to_owned()))
    }
    Ok(value
        .chunks_exact(COLUMN_BYTES)
        .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]))
        .collect())
}

/// Encodes 3-byte little-endian columns (Geth's `WriteFilterMapExtRow`).
fn encode_columns(columns: &[u32]) -> Vec<u8> {
    columns.iter().flat_map(|column| column.to_le_bytes().into_iter().take(COLUMN_BYTES)).collect()
}

#[cfg(test)]
mod tests;
