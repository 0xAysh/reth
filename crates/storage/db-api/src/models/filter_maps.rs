//! Strict physical models for the durable `FilterMaps` schema.

use crate::{
    table::{Compress, Decode, Decompress, Encode},
    DatabaseError,
};
use alloy_primitives::B256;
use bytes::BufMut;
use reth_codecs::DecompressError;
use serde::{Deserialize, Serialize};
use std::mem::size_of;

const IDENTITY_BYTES: usize = 44;
const ANCHOR_BYTES: usize = 4 + 8 + 32 + 8 + 1;
const POINTER_BYTES: usize = 32 + 8;
const GROUP_SIZE: usize = 32;
const COLUMN_BYTES: usize = 3;

/// Primitive persisted index identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StoredIndexIdentity {
    /// Storage format version.
    pub storage_format_version: u16,
    /// Chain id.
    pub chain_id: u64,
    /// Genesis hash.
    pub genesis_hash: B256,
    /// Value-space version tag.
    pub value_space_version: u8,
    /// Parameter-set tag.
    pub params_id: u8,
}

/// Compact persisted coverage catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoredCoverageCatalog {
    /// Ordered segment descriptors.
    pub segments: Vec<StoredSegmentDescriptor>,
}

/// Persisted segment descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoredSegmentDescriptor {
    /// Inline origin.
    pub origin: StoredSegmentOrigin,
    /// First completed map.
    pub first_map: u32,
    /// Terminal completed map.
    pub terminal_map: u32,
}

/// Persisted segment origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum StoredSegmentOrigin {
    /// Genesis.
    Genesis,
    /// Checkpoint with durable pointer provenance.
    Checkpoint {
        /// Origin anchor.
        anchor: StoredMapResumeAnchor,
        /// Provenance.
        provenance: StoredCheckpointProvenance,
    },
    /// Anchor retained after contraction.
    Retained {
        /// Origin anchor.
        anchor: StoredMapResumeAnchor,
    },
}

/// Persisted checkpoint provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum StoredCheckpointProvenance {
    /// Produced by published coverage.
    PublishedCoverage,
    /// Recognized registry id.
    Recognized(u64),
    /// Derived from a trusted predecessor.
    DerivedFrom(StoredMapResumeAnchor),
}

/// Persisted map resume anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StoredMapResumeAnchor {
    /// Completed map index, repeated and checked against the table key.
    pub completed_map_index: u32,
    /// Resume block number.
    pub block_number: u64,
    /// Resume block hash.
    pub block_hash: B256,
    /// First log value index.
    pub first_log_value_index: u64,
    /// Value-space version tag.
    pub value_space_version: u8,
}

/// Persisted block pointer record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StoredBlockPointer {
    /// Canonical block hash.
    pub block_hash: B256,
    /// First log value index.
    pub first_log_value_index: u64,
}

/// Per-map proof of logical row presence and extension state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoredMapRowDirectory {
    /// Parameter identity defining bitmap cardinality.
    pub params_id: u8,
    /// One bit per nonempty row.
    pub nonempty: Vec<u8>,
    /// One bit per row requiring extension data.
    pub extended: Vec<u8>,
    /// Declared nonempty row count.
    pub nonempty_row_count: u32,
    /// Complete logical mark count.
    pub logical_mark_count: u64,
    /// Exact logical length of each nonempty row, ordered by ascending row index.
    pub row_lengths: Vec<u32>,
}

impl StoredMapRowDirectory {
    /// Creates and validates a directory.
    pub fn new(
        params_id: u8,
        nonempty: Vec<u8>,
        extended: Vec<u8>,
        nonempty_row_count: u32,
        logical_mark_count: u64,
        row_lengths: Vec<u32>,
    ) -> Result<Self, FilterMapModelError> {
        let value = Self {
            params_id,
            nonempty,
            extended,
            nonempty_row_count,
            logical_mark_count,
            row_lengths,
        };
        value.validate()?;
        Ok(value)
    }

    /// Returns whether a row is nonempty.
    pub fn is_nonempty(&self, row: u32) -> bool {
        bit(&self.nonempty, row)
    }

    /// Returns whether a row requires extension data.
    pub fn is_extended(&self, row: u32) -> bool {
        bit(&self.extended, row)
    }

    /// Returns the exact logical length declared for a nonempty row.
    pub fn row_length(&self, row: u32) -> Option<u32> {
        if !self.is_nonempty(row) {
            return None
        }
        let byte = usize::try_from(row / 8).ok()?;
        let preceding_bytes = self.nonempty.get(..byte)?;
        let preceding =
            preceding_bytes.iter().map(|value| value.count_ones() as usize).sum::<usize>();
        let mask = if row.is_multiple_of(8) { 0 } else { (1u8 << (row % 8)) - 1 };
        let within_byte = (self.nonempty[byte] & mask).count_ones() as usize;
        self.row_lengths.get(preceding + within_byte).copied()
    }

    fn validate(&self) -> Result<(), FilterMapModelError> {
        let params = params(self.params_id)?;
        let bytes =
            usize::try_from(params.map_height / 8).map_err(|_| FilterMapModelError::Arithmetic)?;
        if self.nonempty.len() != bytes || self.extended.len() != bytes {
            return Err(FilterMapModelError::BitmapLength)
        }
        let mut count = 0u32;
        let mut extended_count = 0u32;
        for (&nonempty, &extended) in self.nonempty.iter().zip(&self.extended) {
            if extended & !nonempty != 0 {
                return Err(FilterMapModelError::ImpossibleDirectoryState)
            }
            count =
                count.checked_add(nonempty.count_ones()).ok_or(FilterMapModelError::Arithmetic)?;
            extended_count = extended_count
                .checked_add(extended.count_ones())
                .ok_or(FilterMapModelError::Arithmetic)?;
        }
        if count != self.nonempty_row_count || self.row_lengths.len() != count as usize {
            return Err(FilterMapModelError::CountMismatch)
        }
        let mut length_index = 0usize;
        let mut exact_marks = 0u64;
        for row in 0..params.map_height {
            if !self.is_nonempty(row) {
                continue
            }
            let length = self.row_lengths[length_index];
            length_index += 1;
            let valid = if self.is_extended(row) {
                length > params.base_row_length &&
                    length <= params.max_row_length.min(params.map_width)
            } else {
                length > 0 && length <= params.base_row_length.min(params.map_width)
            };
            if !valid {
                return Err(FilterMapModelError::ImpossibleDirectoryState)
            }
            exact_marks = exact_marks
                .checked_add(u64::from(length))
                .ok_or(FilterMapModelError::Arithmetic)?;
        }
        if exact_marks != self.logical_mark_count || extended_count > count {
            return Err(FilterMapModelError::CountMismatch)
        }
        Ok(())
    }
}

/// Linearized key for one grouped base row.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct FilterMapBaseRowKey(pub u64);

impl FilterMapBaseRowKey {
    /// Computes the epoch-local aligned grouped-row key for `map_index` and `row_index`.
    pub fn new(params_id: u8, map_index: u32, row_index: u32) -> Result<Self, FilterMapModelError> {
        let params = params(params_id)?;
        linearized_key(params, base_group_start(params, map_index)?, row_index).map(Self)
    }

    /// Returns the slot occupied by `map_index` in its epoch-local base group.
    pub fn slot(params_id: u8, map_index: u32) -> Result<usize, FilterMapModelError> {
        let params = params(params_id)?;
        let offset = map_index % params.maps_per_epoch;
        usize::try_from(offset % params.group_size).map_err(|_| FilterMapModelError::Arithmetic)
    }

    /// Validates and decodes this key under a recognized parameter identity.
    pub fn validate(
        self,
        params_id: u8,
    ) -> Result<FilterMapBaseRowCoordinates, FilterMapModelError> {
        let params = params(params_id)?;
        let coordinates = decode_linearized_key(params, self.0)?;
        let epoch_offset = coordinates.map_index % params.maps_per_epoch;
        if !epoch_offset.is_multiple_of(params.group_size) {
            return Err(FilterMapModelError::GroupMisalignment)
        }
        Ok(FilterMapBaseRowCoordinates {
            group_start: coordinates.map_index,
            row_index: coordinates.row_index,
        })
    }

    /// Returns the raw ordered key.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Decoded coordinates of a validated grouped base-row key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterMapBaseRowCoordinates {
    /// First map represented by the 32-slot group.
    pub group_start: u32,
    /// Logical row index.
    pub row_index: u32,
}

/// Linearized key for one individual extended row.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct FilterMapExtendedRowKey(pub u64);

impl FilterMapExtendedRowKey {
    /// Computes the individual map-row key.
    pub fn new(params_id: u8, map_index: u32, row_index: u32) -> Result<Self, FilterMapModelError> {
        linearized_key(params(params_id)?, map_index, row_index).map(Self)
    }

    /// Validates and decodes this key under a recognized parameter identity.
    pub fn validate(self, params_id: u8) -> Result<FilterMapRowCoordinates, FilterMapModelError> {
        decode_linearized_key(params(params_id)?, self.0)
    }

    /// Returns the raw ordered key.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Decoded coordinates of a validated individual row key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterMapRowCoordinates {
    /// Individual map index.
    pub map_index: u32,
    /// Logical row index.
    pub row_index: u32,
}

/// The base portions of one row for 32 adjacent maps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoredBaseRowGroup {
    /// Parameter identity used to validate columns and slot bounds.
    pub params_id: u8,
    /// Exactly 32 slots in map order.
    pub slots: Vec<Vec<u32>>,
}

impl StoredBaseRowGroup {
    /// Creates a canonical group.
    pub fn new(params_id: u8, slots: Vec<Vec<u32>>) -> Result<Self, FilterMapModelError> {
        let value = Self { params_id, slots };
        value.validate()?;
        Ok(value)
    }

    /// Creates an empty 32-slot group.
    pub fn empty(params_id: u8) -> Result<Self, FilterMapModelError> {
        params(params_id)?;
        Ok(Self { params_id, slots: vec![Vec::new(); GROUP_SIZE] })
    }

    fn validate(&self) -> Result<(), FilterMapModelError> {
        let params = params(self.params_id)?;
        if self.slots.len() != GROUP_SIZE {
            return Err(FilterMapModelError::GroupCardinality)
        }
        for slot in &self.slots {
            if slot.len() > params.base_row_length as usize {
                return Err(FilterMapModelError::ExcessRowLength)
            }
            validate_columns(slot, params.map_width)?;
        }
        Ok(())
    }
}

/// Overflow columns for one individual logical row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoredExtendedRow {
    /// Parameter identity used to validate columns.
    pub params_id: u8,
    /// Remaining columns in insertion order, including duplicates.
    pub columns: Vec<u32>,
}

impl StoredExtendedRow {
    /// Creates a canonical nonempty extension.
    pub fn new(params_id: u8, columns: Vec<u32>) -> Result<Self, FilterMapModelError> {
        let value = Self { params_id, columns };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), FilterMapModelError> {
        let params = params(self.params_id)?;
        if self.columns.is_empty() {
            return Err(FilterMapModelError::EmptyExtension)
        }
        let max_extension = params
            .max_row_length
            .checked_sub(params.base_row_length)
            .ok_or(FilterMapModelError::Arithmetic)?;
        if self.columns.len() > max_extension as usize {
            return Err(FilterMapModelError::ExcessRowLength)
        }
        validate_columns(&self.columns, params.map_width)
    }
}

/// Checked cleanup key interval, inclusive at both ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterMapRowKeyRange {
    /// First key.
    pub start: u64,
    /// Last key.
    pub end: u64,
}

impl FilterMapRowKeyRange {
    /// Returns the complete row-key interval for one epoch.
    pub fn complete_epoch(params_id: u8, epoch: u32) -> Result<Self, FilterMapModelError> {
        let params = params(params_id)?;
        let first_map =
            epoch.checked_mul(params.maps_per_epoch).ok_or(FilterMapModelError::Arithmetic)?;
        let last_map = first_map
            .checked_add(params.maps_per_epoch - 1)
            .ok_or(FilterMapModelError::Arithmetic)?;
        Ok(Self {
            start: linearized_key(params, first_map, 0)?,
            end: linearized_key(params, last_map, params.map_height - 1)?,
        })
    }
}

/// Strict model construction or decoding error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FilterMapModelError {
    /// Unknown parameter identity.
    #[error("unknown FilterMaps parameter identity")]
    UnknownParams,
    /// Unknown storage or value-space identity tag.
    #[error("unknown FilterMaps version")]
    UnknownVersion,
    /// A fixed-size record has the wrong length.
    #[error("malformed FilterMaps record length")]
    Length,
    /// Bitmap length does not match the parameter set.
    #[error("malformed FilterMaps bitmap length")]
    BitmapLength,
    /// Extended bits are set for empty rows.
    #[error("impossible FilterMaps directory state")]
    ImpossibleDirectoryState,
    /// Declared and actual counts disagree.
    #[error("FilterMaps count mismatch")]
    CountMismatch,
    /// A row index is outside the map.
    #[error("FilterMaps row index out of range")]
    RowOutOfRange,
    /// A column is outside the map.
    #[error("FilterMaps column out of range")]
    ColumnOutOfRange,
    /// A grouped row does not have 32 explicit slots.
    #[error("FilterMaps base group must have 32 slots")]
    GroupCardinality,
    /// A base-row key does not begin at a 32-map boundary.
    #[error("FilterMaps base-row key is not group aligned")]
    GroupMisalignment,
    /// A base slot exceeds the recognized base-row length.
    #[error("FilterMaps base slot exceeds its bound")]
    ExcessRowLength,
    /// An extension must contain at least one column.
    #[error("FilterMaps extension is empty")]
    EmptyExtension,
    /// A tag is invalid.
    #[error("invalid FilterMaps tag")]
    InvalidTag,
    /// Persisted segments are unordered or malformed.
    #[error("invalid FilterMaps coverage catalog")]
    InvalidCatalog,
    /// Checked arithmetic overflowed.
    #[error("FilterMaps arithmetic overflow")]
    Arithmetic,
}

/// Durable physical geometry mirrored by the storage codecs for a recognized parameter tag.
///
/// This deliberately lives in `reth-db-api`, which cannot depend on the higher-level `FilterMaps`
/// crate. Every recognized tag and derived dimension is exhaustively compared with `ParamsId` by
/// the storage integration tests; changing either side without a storage-format migration fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterMapPhysicalParams {
    /// Number of rows in one map.
    pub map_height: u32,
    /// Number of encoded columns in one map.
    pub map_width: u32,
    /// Number of maps in one epoch.
    pub maps_per_epoch: u32,
    /// Number of columns stored in a grouped base slot.
    pub base_row_length: u32,
    /// Maximum logical row length across all mapping layers.
    pub max_row_length: u32,
    /// Number of maps sharing one grouped base-row record.
    pub group_size: u32,
}

/// Returns the durable physical geometry for a recognized persisted parameter tag.
pub const fn filter_map_physical_params(
    id: u8,
) -> Result<FilterMapPhysicalParams, FilterMapModelError> {
    params(id)
}

const fn params(id: u8) -> Result<FilterMapPhysicalParams, FilterMapModelError> {
    match id {
        1 => Ok(FilterMapPhysicalParams {
            map_height: 65_536,
            map_width: 16_777_216,
            maps_per_epoch: 1_024,
            base_row_length: 8,
            max_row_length: 8_192,
            group_size: 32,
        }),
        2 => Ok(FilterMapPhysicalParams {
            map_height: 16,
            map_width: 16_777_216,
            maps_per_epoch: 1,
            base_row_length: 1,
            max_row_length: 1,
            group_size: 32,
        }),
        _ => Err(FilterMapModelError::UnknownParams),
    }
}

fn base_group_start(
    params: FilterMapPhysicalParams,
    map_index: u32,
) -> Result<u32, FilterMapModelError> {
    let epoch_start = (map_index / params.maps_per_epoch)
        .checked_mul(params.maps_per_epoch)
        .ok_or(FilterMapModelError::Arithmetic)?;
    let epoch_offset = map_index % params.maps_per_epoch;
    epoch_start
        .checked_add(epoch_offset & !(params.group_size - 1))
        .ok_or(FilterMapModelError::Arithmetic)
}

fn linearized_key(
    params: FilterMapPhysicalParams,
    map_index: u32,
    row_index: u32,
) -> Result<u64, FilterMapModelError> {
    if row_index >= params.map_height {
        return Err(FilterMapModelError::RowOutOfRange)
    }
    let epoch = map_index / params.maps_per_epoch;
    let offset = map_index % params.maps_per_epoch;
    u64::from(epoch)
        .checked_mul(u64::from(params.map_height))
        .and_then(|value| value.checked_mul(u64::from(params.maps_per_epoch)))
        .and_then(|value| {
            u64::from(row_index)
                .checked_mul(u64::from(params.maps_per_epoch))
                .and_then(|row| value.checked_add(row))
        })
        .and_then(|value| value.checked_add(u64::from(offset)))
        .ok_or(FilterMapModelError::Arithmetic)
}

fn decode_linearized_key(
    params: FilterMapPhysicalParams,
    key: u64,
) -> Result<FilterMapRowCoordinates, FilterMapModelError> {
    let epoch_span = u64::from(params.map_height)
        .checked_mul(u64::from(params.maps_per_epoch))
        .ok_or(FilterMapModelError::Arithmetic)?;
    let epoch = key / epoch_span;
    let within_epoch = key % epoch_span;
    let row_index = within_epoch / u64::from(params.maps_per_epoch);
    let map_offset = within_epoch % u64::from(params.maps_per_epoch);
    let map_index = epoch
        .checked_mul(u64::from(params.maps_per_epoch))
        .and_then(|first| first.checked_add(map_offset))
        .and_then(|map| u32::try_from(map).ok())
        .ok_or(FilterMapModelError::Arithmetic)?;
    let row_index = u32::try_from(row_index).map_err(|_| FilterMapModelError::Arithmetic)?;
    if row_index >= params.map_height {
        return Err(FilterMapModelError::RowOutOfRange)
    }
    Ok(FilterMapRowCoordinates { map_index, row_index })
}

fn validate_columns(columns: &[u32], map_width: u32) -> Result<(), FilterMapModelError> {
    if columns.iter().any(|&column| column >= map_width) {
        return Err(FilterMapModelError::ColumnOutOfRange)
    }
    Ok(())
}

fn bit(bitmap: &[u8], row: u32) -> bool {
    let byte = usize::try_from(row / 8).ok();
    byte.and_then(|index| bitmap.get(index)).is_some_and(|value| value & (1 << (row % 8)) != 0)
}

fn decode_error(error: FilterMapModelError) -> DecompressError {
    DecompressError::new(error)
}

const fn take<'a>(input: &mut &'a [u8], count: usize) -> Result<&'a [u8], FilterMapModelError> {
    if input.len() < count {
        return Err(FilterMapModelError::Length)
    }
    let (head, tail) = input.split_at(count);
    *input = tail;
    Ok(head)
}

fn take_u8(input: &mut &[u8]) -> Result<u8, FilterMapModelError> {
    Ok(take(input, 1)?[0])
}
fn take_u16(input: &mut &[u8]) -> Result<u16, FilterMapModelError> {
    Ok(u16::from_be_bytes(take(input, 2)?.try_into().map_err(|_| FilterMapModelError::Length)?))
}
fn take_u32(input: &mut &[u8]) -> Result<u32, FilterMapModelError> {
    Ok(u32::from_be_bytes(take(input, 4)?.try_into().map_err(|_| FilterMapModelError::Length)?))
}
fn take_u64(input: &mut &[u8]) -> Result<u64, FilterMapModelError> {
    Ok(u64::from_be_bytes(take(input, 8)?.try_into().map_err(|_| FilterMapModelError::Length)?))
}
fn take_hash(input: &mut &[u8]) -> Result<B256, FilterMapModelError> {
    Ok(B256::from_slice(take(input, 32)?))
}

fn put_column(out: &mut Vec<u8>, column: u32) {
    let bytes = column.to_le_bytes();
    out.extend_from_slice(&bytes[..COLUMN_BYTES]);
}

fn take_column(input: &mut &[u8]) -> Result<u32, FilterMapModelError> {
    let bytes = take(input, COLUMN_BYTES)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]))
}

fn encode_anchor(anchor: &StoredMapResumeAnchor, out: &mut Vec<u8>) {
    out.extend_from_slice(&anchor.completed_map_index.to_be_bytes());
    out.extend_from_slice(&anchor.block_number.to_be_bytes());
    out.extend_from_slice(anchor.block_hash.as_slice());
    out.extend_from_slice(&anchor.first_log_value_index.to_be_bytes());
    out.push(anchor.value_space_version);
}

fn decode_anchor(input: &mut &[u8]) -> Result<StoredMapResumeAnchor, FilterMapModelError> {
    let anchor = StoredMapResumeAnchor {
        completed_map_index: take_u32(input)?,
        block_number: take_u64(input)?,
        block_hash: take_hash(input)?,
        first_log_value_index: take_u64(input)?,
        value_space_version: take_u8(input)?,
    };
    if anchor.value_space_version != 1 {
        return Err(FilterMapModelError::UnknownVersion)
    }
    Ok(anchor)
}

macro_rules! impl_key {
    ($ty:ty) => {
        impl Encode for $ty {
            type Encoded = [u8; 8];
            fn encode(self) -> Self::Encoded {
                self.0.to_be_bytes()
            }
        }
        impl Decode for $ty {
            fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
                Ok(Self(u64::from_be_bytes(value.try_into().map_err(|_| DatabaseError::Decode)?)))
            }
        }
    };
}
impl_key!(FilterMapBaseRowKey);
impl_key!(FilterMapExtendedRowKey);

macro_rules! impl_value {
    ($ty:ty, $encode:expr, $decode:expr) => {
        impl Compress for $ty {
            type Compressed = Vec<u8>;
            fn compress(self) -> Self::Compressed {
                ($encode)(&self)
            }
            fn compress_to_buf<B: BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
                buf.put_slice(&($encode)(self));
            }
        }
        impl Decompress for $ty {
            fn decompress(value: &[u8]) -> Result<Self, DecompressError> {
                ($decode)(value).map_err(decode_error)
            }
        }
    };
}

impl_value!(
    StoredIndexIdentity,
    |value: &StoredIndexIdentity| {
        let mut out = Vec::with_capacity(IDENTITY_BYTES);
        out.extend_from_slice(&value.storage_format_version.to_be_bytes());
        out.extend_from_slice(&value.chain_id.to_be_bytes());
        out.extend_from_slice(value.genesis_hash.as_slice());
        out.push(value.value_space_version);
        out.push(value.params_id);
        out
    },
    |value: &[u8]| {
        if value.len() != IDENTITY_BYTES {
            return Err(FilterMapModelError::Length)
        }
        let mut input = value;
        let decoded = StoredIndexIdentity {
            storage_format_version: take_u16(&mut input)?,
            chain_id: take_u64(&mut input)?,
            genesis_hash: take_hash(&mut input)?,
            value_space_version: take_u8(&mut input)?,
            params_id: take_u8(&mut input)?,
        };
        if decoded.storage_format_version != 1 || decoded.value_space_version != 1 {
            return Err(FilterMapModelError::UnknownVersion)
        }
        params(decoded.params_id)?;
        Ok(decoded)
    }
);

impl_value!(
    StoredMapResumeAnchor,
    |value: &StoredMapResumeAnchor| {
        let mut out = Vec::with_capacity(ANCHOR_BYTES);
        encode_anchor(value, &mut out);
        out
    },
    |value: &[u8]| {
        if value.len() != ANCHOR_BYTES {
            return Err(FilterMapModelError::Length)
        }
        let mut input = value;
        decode_anchor(&mut input)
    }
);

impl_value!(
    StoredBlockPointer,
    |value: &StoredBlockPointer| {
        let mut out = Vec::with_capacity(POINTER_BYTES);
        out.extend_from_slice(value.block_hash.as_slice());
        out.extend_from_slice(&value.first_log_value_index.to_be_bytes());
        out
    },
    |value: &[u8]| {
        if value.len() != POINTER_BYTES {
            return Err(FilterMapModelError::Length)
        }
        let mut input = value;
        Ok(StoredBlockPointer {
            block_hash: take_hash(&mut input)?,
            first_log_value_index: take_u64(&mut input)?,
        })
    }
);

impl_value!(
    StoredMapRowDirectory,
    |value: &StoredMapRowDirectory| {
        value.validate().expect("invalid directory must not be persisted");
        let mut out = Vec::with_capacity(
            17 + value.nonempty.len() + value.extended.len() + value.row_lengths.len() * 4,
        );
        out.push(value.params_id);
        out.extend_from_slice(&(value.nonempty.len() as u32).to_be_bytes());
        out.extend_from_slice(&value.nonempty);
        out.extend_from_slice(&value.extended);
        out.extend_from_slice(&value.nonempty_row_count.to_be_bytes());
        out.extend_from_slice(&value.logical_mark_count.to_be_bytes());
        for &length in &value.row_lengths {
            out.extend_from_slice(&length.to_be_bytes());
        }
        out
    },
    |value: &[u8]| {
        let mut input = value;
        let params_id = take_u8(&mut input)?;
        let limits = params(params_id)?;
        let bitmap_len =
            usize::try_from(take_u32(&mut input)?).map_err(|_| FilterMapModelError::Arithmetic)?;
        let expected_bitmap_len =
            usize::try_from(limits.map_height / 8).map_err(|_| FilterMapModelError::Arithmetic)?;
        if bitmap_len != expected_bitmap_len {
            return Err(FilterMapModelError::BitmapLength)
        }
        let nonempty = take(&mut input, bitmap_len)?.to_vec();
        let extended = take(&mut input, bitmap_len)?.to_vec();
        let nonempty_row_count = take_u32(&mut input)?;
        let logical_mark_count = take_u64(&mut input)?;
        // Bound corruption-controlled counts and prove the exact remaining size before allocating.
        if nonempty_row_count > limits.map_height {
            return Err(FilterMapModelError::CountMismatch)
        }
        let row_count =
            usize::try_from(nonempty_row_count).map_err(|_| FilterMapModelError::Arithmetic)?;
        let row_length_bytes =
            row_count.checked_mul(size_of::<u32>()).ok_or(FilterMapModelError::Arithmetic)?;
        if input.len() != row_length_bytes {
            return Err(FilterMapModelError::Length)
        }
        let mut row_lengths = Vec::with_capacity(row_count);
        for _ in 0..row_count {
            row_lengths.push(take_u32(&mut input)?);
        }
        StoredMapRowDirectory::new(
            params_id,
            nonempty,
            extended,
            nonempty_row_count,
            logical_mark_count,
            row_lengths,
        )
    }
);

impl_value!(
    StoredBaseRowGroup,
    |value: &StoredBaseRowGroup| {
        value.validate().expect("invalid base group must not be persisted");
        let column_count: usize = value.slots.iter().map(Vec::len).sum();
        let mut out = Vec::with_capacity(1 + GROUP_SIZE * 2 + column_count * COLUMN_BYTES);
        out.push(value.params_id);
        for slot in &value.slots {
            out.extend_from_slice(&(slot.len() as u16).to_be_bytes());
        }
        for slot in &value.slots {
            for &column in slot {
                put_column(&mut out, column);
            }
        }
        out
    },
    |value: &[u8]| {
        let mut input = value;
        let params_id = take_u8(&mut input)?;
        let limits = params(params_id)?;
        let mut lengths = [0u16; GROUP_SIZE];
        for length in &mut lengths {
            *length = take_u16(&mut input)?;
            if u32::from(*length) > limits.base_row_length {
                return Err(FilterMapModelError::ExcessRowLength)
            }
        }
        let mut slots = Vec::with_capacity(GROUP_SIZE);
        for length in lengths {
            let mut slot = Vec::with_capacity(length as usize);
            for _ in 0..length {
                slot.push(take_column(&mut input)?);
            }
            slots.push(slot);
        }
        if !input.is_empty() {
            return Err(FilterMapModelError::Length)
        }
        StoredBaseRowGroup::new(params_id, slots)
    }
);

impl_value!(
    StoredExtendedRow,
    |value: &StoredExtendedRow| {
        value.validate().expect("invalid extension must not be persisted");
        let mut out = Vec::with_capacity(5 + value.columns.len() * COLUMN_BYTES);
        out.push(value.params_id);
        out.extend_from_slice(&(value.columns.len() as u32).to_be_bytes());
        for &column in &value.columns {
            put_column(&mut out, column);
        }
        out
    },
    |value: &[u8]| {
        let mut input = value;
        let params_id = take_u8(&mut input)?;
        let count =
            usize::try_from(take_u32(&mut input)?).map_err(|_| FilterMapModelError::Arithmetic)?;
        let expected = count.checked_mul(COLUMN_BYTES).ok_or(FilterMapModelError::Arithmetic)?;
        if input.len() != expected {
            return Err(FilterMapModelError::Length)
        }
        let mut columns = Vec::with_capacity(count);
        for _ in 0..count {
            columns.push(take_column(&mut input)?);
        }
        StoredExtendedRow::new(params_id, columns)
    }
);

impl_value!(
    StoredCoverageCatalog,
    |value: &StoredCoverageCatalog| {
        let mut out = Vec::new();
        out.extend_from_slice(&(value.segments.len() as u32).to_be_bytes());
        for segment in &value.segments {
            match &segment.origin {
                StoredSegmentOrigin::Genesis => out.push(0),
                StoredSegmentOrigin::Checkpoint { anchor, provenance } => {
                    out.push(1);
                    encode_anchor(anchor, &mut out);
                    match provenance {
                        StoredCheckpointProvenance::PublishedCoverage => out.push(0),
                        StoredCheckpointProvenance::Recognized(id) => {
                            out.push(1);
                            out.extend_from_slice(&id.to_be_bytes());
                        }
                        StoredCheckpointProvenance::DerivedFrom(anchor) => {
                            out.push(2);
                            encode_anchor(anchor, &mut out);
                        }
                    }
                }
                StoredSegmentOrigin::Retained { anchor } => {
                    out.push(2);
                    encode_anchor(anchor, &mut out);
                }
            }
            out.extend_from_slice(&segment.first_map.to_be_bytes());
            out.extend_from_slice(&segment.terminal_map.to_be_bytes());
        }
        out
    },
    |value: &[u8]| {
        let mut input = value;
        let count =
            usize::try_from(take_u32(&mut input)?).map_err(|_| FilterMapModelError::Arithmetic)?;
        // Every descriptor has at least an origin tag and two map indices.
        if count > input.len() / 9 {
            return Err(FilterMapModelError::Length)
        }
        let mut segments = Vec::with_capacity(count);
        let mut previous = None;
        for _ in 0..count {
            let origin = match take_u8(&mut input)? {
                0 => StoredSegmentOrigin::Genesis,
                1 => {
                    let anchor = decode_anchor(&mut input)?;
                    let provenance = match take_u8(&mut input)? {
                        0 => StoredCheckpointProvenance::PublishedCoverage,
                        1 => StoredCheckpointProvenance::Recognized(take_u64(&mut input)?),
                        2 => StoredCheckpointProvenance::DerivedFrom(decode_anchor(&mut input)?),
                        _ => return Err(FilterMapModelError::InvalidTag),
                    };
                    StoredSegmentOrigin::Checkpoint { anchor, provenance }
                }
                2 => StoredSegmentOrigin::Retained { anchor: decode_anchor(&mut input)? },
                _ => return Err(FilterMapModelError::InvalidTag),
            };
            let first_map = take_u32(&mut input)?;
            let terminal_map = take_u32(&mut input)?;
            if first_map > terminal_map || previous.is_some_and(|end| first_map <= end) {
                return Err(FilterMapModelError::InvalidCatalog)
            }
            previous = Some(terminal_map);
            segments.push(StoredSegmentDescriptor { origin, first_map, terminal_map });
        }
        if !input.is_empty() {
            return Err(FilterMapModelError::Length)
        }
        Ok(StoredCoverageCatalog { segments })
    }
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_canonical_and_rejecting() {
        let value = StoredIndexIdentity {
            storage_format_version: 1,
            chain_id: 1,
            genesis_hash: B256::repeat_byte(7),
            value_space_version: 1,
            params_id: 1,
        };
        let mut bytes = value.compress();
        assert_eq!(bytes.len(), IDENTITY_BYTES);
        assert_eq!(StoredIndexIdentity::decompress(&bytes).unwrap(), value);
        assert!(StoredIndexIdentity::decompress(&bytes[..43]).is_err());
        bytes.push(0);
        assert!(StoredIndexIdentity::decompress(&bytes).is_err());
    }

    #[test]
    fn row_group_preserves_order_and_duplicates() {
        let mut slots = vec![Vec::new(); GROUP_SIZE];
        slots[3] = vec![7, 2, 7];
        let group = StoredBaseRowGroup::new(1, slots).unwrap();
        assert_eq!(StoredBaseRowGroup::decompress(&group.clone().compress()).unwrap(), group);
    }

    #[test]
    fn directory_rejects_impossible_states_and_counts() {
        assert_eq!(
            StoredMapRowDirectory::new(2, vec![0, 0], vec![1, 0], 0, 0, vec![]),
            Err(FilterMapModelError::ImpossibleDirectoryState)
        );
        assert_eq!(
            StoredMapRowDirectory::new(2, vec![0, 0], vec![0, 0], 0, 1, vec![]),
            Err(FilterMapModelError::CountMismatch)
        );
        assert_eq!(
            StoredMapRowDirectory::new(2, vec![1, 0], vec![0, 0], 1, 2, vec![1]),
            Err(FilterMapModelError::CountMismatch)
        );
        let mut nonempty = vec![0; 65_536 / 8];
        nonempty[0] = 1;
        assert_eq!(
            StoredMapRowDirectory::new(1, nonempty.clone(), nonempty, 1, 8_193, vec![8_193]),
            Err(FilterMapModelError::ImpossibleDirectoryState)
        );

        let empty = StoredMapRowDirectory::new(2, vec![0, 0], vec![0, 0], 0, 0, vec![]).unwrap();
        let mut malformed = empty.clone().compress();
        *malformed.last_mut().unwrap() = 1;
        assert!(StoredMapRowDirectory::decompress(&malformed).is_err());

        let mut oversized_count = empty.compress();
        let count_offset = 1 + 4 + 2 * (16 / 8);
        oversized_count[count_offset..count_offset + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(StoredMapRowDirectory::decompress(&oversized_count).is_err());
    }

    #[test]
    fn row_keys_are_ordered_by_epoch_row_and_map() {
        let row0_map0 = FilterMapExtendedRowKey::new(1, 0, 0).unwrap();
        let row0_map1 = FilterMapExtendedRowKey::new(1, 1, 0).unwrap();
        let row1_map0 = FilterMapExtendedRowKey::new(1, 0, 1).unwrap();
        assert!(row0_map0 < row0_map1);
        assert!(row0_map1 < row1_map0);
        assert_eq!(FilterMapBaseRowKey(1).validate(1), Err(FilterMapModelError::GroupMisalignment));

        for params_id in [1, 2] {
            let epoch0 = FilterMapRowKeyRange::complete_epoch(params_id, 0).unwrap();
            let epoch1 = FilterMapRowKeyRange::complete_epoch(params_id, 1).unwrap();
            assert!(epoch0.end < epoch1.start);
        }
    }

    /// Every reader and writer locates a map's base row as `(key, slot)`; decoding the key must
    /// give back the group that `slot` indexes into, and that group must never span an epoch.
    #[test]
    fn base_row_key_and_slot_recompose_the_map_index() {
        for params_id in [1, 2] {
            let limits = params(params_id).unwrap();
            let epoch = limits.maps_per_epoch;
            let group = limits.group_size;
            let maps = [0, 1, group - 1, group, group + 1, epoch - 1, epoch, epoch + 1]
                .into_iter()
                .chain([2 * epoch + group + 3, u32::MAX - epoch, u32::MAX]);
            for map_index in maps {
                let slot = FilterMapBaseRowKey::slot(params_id, map_index).unwrap();
                assert!(slot < GROUP_SIZE, "params {params_id}, map {map_index}");
                for row_index in [0, 1, limits.map_height - 1] {
                    let key = FilterMapBaseRowKey::new(params_id, map_index, row_index).unwrap();
                    let coordinates = key.validate(params_id).unwrap();
                    assert_eq!(coordinates.row_index, row_index);
                    assert_eq!(
                        u64::from(coordinates.group_start) + slot as u64,
                        u64::from(map_index),
                        "params {params_id}, map {map_index}, row {row_index}"
                    );
                    assert_eq!(coordinates.group_start / epoch, map_index / epoch);
                }
            }
        }
    }

    #[test]
    fn extension_rejects_out_of_range_columns() {
        assert_eq!(
            StoredExtendedRow::new(1, vec![1 << 24]),
            Err(FilterMapModelError::ColumnOutOfRange)
        );
    }

    #[test]
    fn fixed_records_reject_truncation_and_trailing_bytes() {
        let anchor = StoredMapResumeAnchor {
            completed_map_index: 3,
            block_number: 7,
            block_hash: B256::repeat_byte(7),
            first_log_value_index: 99,
            value_space_version: 1,
        };
        let mut encoded = anchor.compress();
        assert_eq!(StoredMapResumeAnchor::decompress(&encoded).unwrap(), anchor);
        assert!(StoredMapResumeAnchor::decompress(&encoded[..encoded.len() - 1]).is_err());
        encoded.push(0);
        assert!(StoredMapResumeAnchor::decompress(&encoded).is_err());

        let pointer =
            StoredBlockPointer { block_hash: B256::repeat_byte(4), first_log_value_index: 42 };
        assert_eq!(StoredBlockPointer::decompress(&pointer.compress()).unwrap(), pointer);
    }

    #[test]
    fn variable_rows_reject_count_disagreement_and_preserve_duplicates() {
        let extension = StoredExtendedRow::new(1, vec![9, 2, 9]).unwrap();
        let encoded = extension.clone().compress();
        assert_eq!(StoredExtendedRow::decompress(&encoded).unwrap(), extension);
        assert!(StoredExtendedRow::decompress(&encoded[..encoded.len() - 1]).is_err());

        let directory =
            StoredMapRowDirectory::new(2, vec![1, 0], vec![0, 0], 1, 1, vec![1]).unwrap();
        assert_eq!(
            StoredMapRowDirectory::decompress(&directory.clone().compress()).unwrap(),
            directory
        );
    }

    #[test]
    fn coverage_catalog_roundtrip_rejects_invalid_tags() {
        let anchor = StoredMapResumeAnchor {
            completed_map_index: 9,
            block_number: 100,
            block_hash: B256::repeat_byte(100),
            first_log_value_index: 1234,
            value_space_version: 1,
        };
        let catalog = StoredCoverageCatalog {
            segments: vec![StoredSegmentDescriptor {
                origin: StoredSegmentOrigin::Checkpoint {
                    anchor,
                    provenance: StoredCheckpointProvenance::DerivedFrom(anchor),
                },
                first_map: 10,
                terminal_map: 12,
            }],
        };
        let encoded = catalog.clone().compress();
        assert_eq!(StoredCoverageCatalog::decompress(&encoded).unwrap(), catalog);
        let mut invalid = 1u32.to_be_bytes().to_vec();
        invalid.push(0xff);
        assert!(StoredCoverageCatalog::decompress(&invalid).is_err());
    }

    #[test]
    fn anchors_with_unknown_value_space_versions_do_not_decode() {
        let unknown = StoredMapResumeAnchor {
            completed_map_index: 9,
            block_number: 100,
            block_hash: B256::repeat_byte(100),
            first_log_value_index: 1234,
            value_space_version: 2,
        };
        assert!(StoredMapResumeAnchor::decompress(&unknown.compress()).is_err());

        let known = StoredMapResumeAnchor { value_space_version: 1, ..unknown };
        let origins = [
            StoredSegmentOrigin::Retained { anchor: unknown },
            StoredSegmentOrigin::Checkpoint {
                anchor: unknown,
                provenance: StoredCheckpointProvenance::Recognized(1),
            },
            StoredSegmentOrigin::Checkpoint {
                anchor: known,
                provenance: StoredCheckpointProvenance::DerivedFrom(unknown),
            },
        ];
        for origin in origins {
            let catalog = StoredCoverageCatalog {
                segments: vec![StoredSegmentDescriptor { origin, first_map: 10, terminal_map: 12 }],
            };
            assert!(StoredCoverageCatalog::decompress(&catalog.compress()).is_err());
        }
    }
}
