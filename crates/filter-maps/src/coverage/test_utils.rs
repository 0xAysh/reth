use crate::{
    coverage::{
        IndexIdentity, MapResumeAnchor, SegmentOrigin, ValidatedSegment, VerifiedCheckpoint,
        STORAGE_FORMAT_V1,
    },
    BlockPointer, MapBoundary, ParamsId, DEFAULT_PARAMS, GETH_V1,
};
use alloy_primitives::B256;
use std::collections::BTreeMap;

pub(super) const VPM: u64 = DEFAULT_PARAMS.values_per_map();

pub(super) fn hash(byte: u64) -> B256 {
    B256::repeat_byte(byte as u8)
}

pub(super) fn identity() -> IndexIdentity {
    IndexIdentity::new(STORAGE_FORMAT_V1, 1, hash(0), GETH_V1, ParamsId::Default)
}

pub(super) fn anchor(map: u32, block: u64, index: u64) -> MapResumeAnchor {
    MapResumeAnchor::new(
        MapBoundary::new(map, block, hash(block)),
        BlockPointer::new(block, hash(block), index),
    )
    .unwrap()
}

pub(super) fn aligned(map: u32, block: u64) -> MapResumeAnchor {
    anchor(map, block, (map as u64 + 1) * VPM)
}

pub(super) fn checkpoint(anchor: MapResumeAnchor) -> SegmentOrigin {
    SegmentOrigin::Checkpoint(VerifiedCheckpoint::recognized(
        identity(),
        anchor,
        u64::from(anchor.completed_map_index),
    ))
}

pub(super) fn anchors_through(first_map: u32, terminal: MapResumeAnchor) -> Vec<MapResumeAnchor> {
    if first_map > terminal.completed_map_index {
        return vec![terminal]
    }
    (first_map..=terminal.completed_map_index)
        .map(|map| {
            if map == terminal.completed_map_index {
                terminal
            } else {
                let maps_remaining = u64::from(terminal.completed_map_index - map);
                let block = u64::from(map + 1)
                    .saturating_mul(10)
                    .min(terminal.pointer.block_number.saturating_sub(maps_remaining));
                aligned(map, block)
            }
        })
        .collect()
}

/// Stored pointers a publication of `segments` would have written: the start and anchor pointers,
/// with every block between them one slot after its predecessor.
pub(super) fn stored_pointers<'a>(
    segments: impl IntoIterator<Item = &'a ValidatedSegment>,
) -> BTreeMap<u64, BlockPointer> {
    let mut pointers = BTreeMap::new();
    for segment in segments {
        let start = segment.start();
        let mut previous =
            BlockPointer::new(start.block_number, start.block_hash, start.first_log_value_index);
        pointers.insert(previous.block_number, previous);
        for anchor in segment.anchors() {
            for number in previous.block_number + 1..anchor.pointer.block_number {
                previous =
                    BlockPointer::new(number, hash(number), previous.first_log_value_index + 1);
                pointers.insert(number, previous);
            }
            previous = anchor.pointer;
            pointers.insert(previous.block_number, previous);
        }
    }
    pointers
}
