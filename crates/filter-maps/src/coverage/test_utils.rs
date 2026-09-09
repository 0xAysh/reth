use crate::{
    coverage::{IndexIdentity, MapResumeAnchor, SegmentOrigin, VerifiedCheckpoint},
    BlockPointer, MapBoundary, ParamsId, DEFAULT_PARAMS, GETH_V1,
};
use alloy_primitives::B256;

pub(super) const VPM: u64 = DEFAULT_PARAMS.values_per_map();

pub(super) fn hash(byte: u64) -> B256 {
    B256::repeat_byte(byte as u8)
}

pub(super) fn identity() -> IndexIdentity {
    IndexIdentity::new(1, hash(0), GETH_V1, ParamsId::Default)
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
    SegmentOrigin::Checkpoint(VerifiedCheckpoint::derived(identity(), anchor))
}
