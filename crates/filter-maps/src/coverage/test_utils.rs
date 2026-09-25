//! Unit-test helpers that reach coverage internals.
//!
//! These hand-build anchors, segments, and checkpoints and round-trip the crate-private
//! [`CoverageSet`], which the public [`crate::test_utils`] cannot do. The synthetic chain itself,
//! [`block_hash`] and [`identity`], comes from the public module and is re-exported here, so no
//! helper is defined in both. Anchors here assume [`PARAMS`]: one map spans [`VPM`] values.

pub(super) use crate::test_utils::{block_hash, identity, AcceptAllCheckpoints};
use crate::{
    coverage::{
        CoverageSet, QueryableCoverage, SegmentOrigin, StructurallyRestoredCoverage,
        ValidatedSegment, VerifiedCheckpoint,
    },
    BlockPointer, MapBoundary, MapResumeAnchor, ParamsId, DEFAULT_PARAMS,
};
use alloy_primitives::B256;
use std::{collections::BTreeMap, convert::Infallible};

/// Parameter set every hand-built anchor assumes.
pub(super) const PARAMS: ParamsId = ParamsId::Default;
pub(super) const VPM: u64 = DEFAULT_PARAMS.values_per_map();

pub(super) fn anchor(map: u32, block: u64, index: u64) -> MapResumeAnchor {
    MapResumeAnchor::new(
        MapBoundary::new(map, block, block_hash(block)),
        BlockPointer::new(block, block_hash(block), index),
    )
    .unwrap()
}

pub(super) fn aligned(map: u32, block: u64) -> MapResumeAnchor {
    anchor(map, block, (map as u64 + 1) * VPM)
}

pub(super) fn checkpoint(anchor: MapResumeAnchor) -> SegmentOrigin {
    SegmentOrigin::Checkpoint(VerifiedCheckpoint::recognized(
        identity(PARAMS),
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
                previous = BlockPointer::new(
                    number,
                    block_hash(number),
                    previous.first_log_value_index + 1,
                );
                pointers.insert(number, previous);
            }
            previous = anchor.pointer;
            pointers.insert(previous.block_number, previous);
        }
    }
    pointers
}

/// Round-trips `set` through its stored catalog and anchors.
pub(super) fn restored(set: &CoverageSet) -> StructurallyRestoredCoverage {
    let anchors = set.segments().iter().flat_map(|segment| segment.anchors().iter().copied());
    StructurallyRestoredCoverage::restore(
        set.identity(),
        set.stored_record(),
        anchors.collect::<Vec<_>>(),
        &mut AcceptAllCheckpoints,
    )
    .unwrap()
}

/// Activates against a chain where block `number` has `canonical(number)` as its hash, with the
/// pointers a real publication would have stored.
pub(super) fn activate(
    restored: &StructurallyRestoredCoverage,
    canonical: impl Fn(u64) -> Option<B256>,
) -> QueryableCoverage {
    let pointers = stored_pointers(restored.segments());
    restored
        .activate(
            |number| Ok::<_, Infallible>(canonical(number)),
            |number| Ok::<_, Infallible>(pointers.get(&number).copied()),
        )
        .unwrap()
}

/// Restores and activates `set` on a chain that agrees with every one of its anchors.
pub(super) fn queryable(set: &CoverageSet) -> QueryableCoverage {
    activate(&restored(set), |number| Some(block_hash(number)))
}
