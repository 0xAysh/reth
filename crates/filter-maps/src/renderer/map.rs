use crate::{coverage::MapResumeAnchor, BlockPointer, MapBoundary, ParamsId};
use alloy_eips::BlockNumHash;

/// One nonempty logical row in a completed filter map.
#[derive(Debug, PartialEq, Eq)]
pub struct RenderedRow {
    pub(super) row_index: u32,
    pub(super) columns: Vec<u32>,
}

impl RenderedRow {
    /// Returns the logical row index.
    pub const fn row_index(&self) -> u32 {
        self.row_index
    }

    /// Returns columns in insertion order, including duplicates.
    pub fn columns(&self) -> &[u32] {
        &self.columns
    }
}

/// A storage-independent completed filter map.
#[derive(Debug, PartialEq, Eq)]
pub struct CompletedMap {
    pub(super) params_id: ParamsId,
    pub(super) map_index: u32,
    pub(super) rows: Vec<RenderedRow>,
    pub(super) block_pointers: Vec<BlockPointer>,
    pub(super) boundary: MapBoundary,
}

impl CompletedMap {
    /// Returns the recognized parameter-set identity used to render the map.
    pub const fn params_id(&self) -> ParamsId {
        self.params_id
    }

    /// Returns the absolute filter-map index.
    pub const fn map_index(&self) -> u32 {
        self.map_index
    }

    /// Returns nonempty rows ordered by ascending row index.
    pub fn rows(&self) -> &[RenderedRow] {
        &self.rows
    }

    /// Returns block pointers associated with this map in stream order.
    pub fn block_pointers(&self) -> &[BlockPointer] {
        &self.block_pointers
    }

    /// Returns the boundary that made this map complete.
    pub const fn boundary(&self) -> MapBoundary {
        self.boundary
    }

    /// Returns the epoch containing this map.
    pub const fn epoch(&self) -> u32 {
        self.params_id.params().map_epoch(self.map_index)
    }

    /// Returns the total number of searchable marks in the map.
    pub fn mark_count(&self) -> usize {
        let mut count = 0;
        let mut index = 0;
        while index < self.rows.len() {
            count += self.rows[index].columns.len();
            index += 1;
        }
        count
    }

    /// Returns the boundary's canonical resume-block identity.
    pub const fn last_block(&self) -> BlockNumHash {
        BlockNumHash::new(self.boundary.resume_block_number, self.boundary.resume_block_hash)
    }
}

/// A completed logical map paired with its validated durable resume anchor.
#[derive(Debug, PartialEq, Eq)]
pub struct AnchoredCompletedMap {
    pub(super) map: CompletedMap,
    pub(super) resume_anchor: MapResumeAnchor,
}

impl AnchoredCompletedMap {
    /// Returns the completed logical map.
    pub const fn map(&self) -> &CompletedMap {
        &self.map
    }

    /// Returns the map's validated resume anchor.
    pub const fn resume_anchor(&self) -> MapResumeAnchor {
        self.resume_anchor
    }
}
