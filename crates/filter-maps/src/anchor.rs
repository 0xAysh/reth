//! Durable restart metadata for completed filter maps.

use crate::{BlockPointer, MapBoundary, Params, ValueSpaceVersion, GETH_V1};

/// Durable restart metadata for one completed filter map.
///
/// The renderer forms it by pairing a [`MapBoundary`] with its resume block's separately emitted
/// [`BlockPointer`]; coverage and storage only carry it. Its pointer is the value-space anchor from
/// which a stream reproduces the maps after the completed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MapResumeAnchor {
    /// Absolute index of the completed filter map.
    pub completed_map_index: u32,
    /// Canonical block from which construction resumes.
    pub pointer: BlockPointer,
    /// Semantic rules under which the numerical pointer was derived.
    pub value_space_version: ValueSpaceVersion,
}

impl MapResumeAnchor {
    /// Pairs a boundary with the pointer of its resume block.
    pub const fn new(
        boundary: MapBoundary,
        pointer: BlockPointer,
    ) -> Result<Self, ResumeAnchorMismatch> {
        Self::new_versioned(boundary, pointer, GETH_V1)
    }

    /// Pairs a boundary and pointer under an explicit value-space version.
    pub const fn new_versioned(
        boundary: MapBoundary,
        pointer: BlockPointer,
        value_space_version: ValueSpaceVersion,
    ) -> Result<Self, ResumeAnchorMismatch> {
        if boundary.resume_block_number != pointer.block_number ||
            !boundary.resume_block_hash.const_eq(&pointer.block_hash)
        {
            return Err(ResumeAnchorMismatch { boundary, pointer })
        }
        Ok(Self { completed_map_index: boundary.completed_map_index, pointer, value_space_version })
    }

    /// Returns the last block closed by the completed map.
    pub const fn covered_through(&self) -> Option<u64> {
        self.pointer.block_number.checked_sub(1)
    }

    /// Returns the first value index after the completed map.
    pub const fn next_map_start(&self, params: &Params) -> u64 {
        (self.completed_map_index as u64 + 1) * params.values_per_map()
    }

    /// Returns whether this map contains no values from `block_number` or its successors.
    pub const fn excludes_block(&self, block_number: u64, params: &Params) -> bool {
        if self.pointer.block_number < block_number {
            return true
        }
        self.pointer.block_number == block_number &&
            self.pointer.first_log_value_index >= self.next_map_start(params)
    }
}

/// A boundary paired with a pointer for another block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "map {} resumes at block {} ({}), but the supplied pointer belongs to block {} ({})",
    boundary.completed_map_index,
    boundary.resume_block_number,
    boundary.resume_block_hash,
    pointer.block_number,
    pointer.block_hash
)]
pub struct ResumeAnchorMismatch {
    /// Completed-map boundary.
    pub boundary: MapBoundary,
    /// Mismatched block pointer.
    pub pointer: BlockPointer,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_PARAMS, RANGE_TEST_PARAMS};
    use alloy_primitives::B256;

    fn hash(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    #[test]
    fn anchor_pairs_a_boundary_with_its_own_pointer() {
        let boundary = MapBoundary::new(7, 42, hash(42));
        let pointer = BlockPointer::new(42, hash(42), 500_000);
        let anchor = MapResumeAnchor::new(boundary, pointer).unwrap();
        assert_eq!(anchor.completed_map_index, 7);
        assert_eq!(anchor.pointer, pointer);
        assert_eq!(anchor.covered_through(), Some(41));
    }

    #[test]
    fn anchor_rejects_a_pointer_for_another_block() {
        let boundary = MapBoundary::new(7, 42, hash(42));
        for pointer in
            [BlockPointer::new(43, hash(43), 500_000), BlockPointer::new(42, hash(0xff), 500_000)]
        {
            assert_eq!(
                MapResumeAnchor::new(boundary, pointer),
                Err(ResumeAnchorMismatch { boundary, pointer })
            );
        }
    }

    #[test]
    fn genesis_resume_covers_nothing() {
        let anchor =
            MapResumeAnchor::new(MapBoundary::new(0, 0, hash(0)), BlockPointer::new(0, hash(0), 0))
                .unwrap();
        assert_eq!(anchor.covered_through(), None);
    }

    #[test]
    fn excludes_block_depends_on_where_the_resume_block_starts() {
        let params = DEFAULT_PARAMS;
        let map_start = params.values_per_map() * 8;
        let boundary = MapBoundary::new(7, 42, hash(42));

        // Block 42 starts in map 8: maps through 7 hold only earlier blocks.
        let clean =
            MapResumeAnchor::new(boundary, BlockPointer::new(42, hash(42), map_start)).unwrap();
        assert!(clean.excludes_block(42, &params));
        assert!(clean.excludes_block(43, &params));
        assert!(!clean.excludes_block(41, &params));

        // Block 42 started inside map 7 and spans the boundary: map 7 holds its values.
        let spanning =
            MapResumeAnchor::new(boundary, BlockPointer::new(42, hash(42), map_start - 1)).unwrap();
        assert!(!spanning.excludes_block(42, &params));
        assert!(spanning.excludes_block(43, &params));
    }

    #[test]
    fn range_test_params_make_every_map_one_slot() {
        let anchor =
            MapResumeAnchor::new(MapBoundary::new(4, 3, hash(3)), BlockPointer::new(3, hash(3), 5))
                .unwrap();
        assert_eq!(anchor.next_map_start(&RANGE_TEST_PARAMS), 5);
        assert!(anchor.excludes_block(3, &RANGE_TEST_PARAMS));
    }
}
