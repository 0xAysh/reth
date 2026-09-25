//! Value-space checkpoints and the provenance that makes their pointers trusted.

use crate::{coverage::IndexIdentity, MapResumeAnchor};

/// A checkpoint record whose pointer still requires a trusted attestation or derivation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ValueSpaceCheckpoint {
    identity: IndexIdentity,
    anchor: MapResumeAnchor,
    provenance: CheckpointProvenance,
}

impl ValueSpaceCheckpoint {
    pub(super) const fn new(
        identity: IndexIdentity,
        anchor: MapResumeAnchor,
        provenance: CheckpointProvenance,
    ) -> Self {
        Self { identity, anchor, provenance }
    }

    /// Returns the identity under which the checkpoint was derived.
    pub const fn identity(&self) -> &IndexIdentity {
        &self.identity
    }

    /// Returns the checkpoint's map resume anchor.
    pub const fn anchor(&self) -> MapResumeAnchor {
        self.anchor
    }

    /// Returns how the checkpoint's numerical pointer acquired trust.
    pub const fn provenance(&self) -> CheckpointProvenance {
        self.provenance
    }
}

/// Durable provenance for a checkpoint's numerical pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CheckpointProvenance {
    /// Produced from complete history already published by this node.
    PublishedCoverage,
    /// Shipped or otherwise recognized by a checkpoint registry.
    Recognized {
        /// Stable identifier interpreted by the registry that supplied the checkpoint.
        id: u64,
    },
    /// Independently derived by counting forward from an already trusted predecessor.
    DerivedFrom {
        /// Trusted predecessor used for the derivation.
        predecessor: MapResumeAnchor,
    },
}

/// A trusted, canonical checkpoint that may originate a validated segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VerifiedCheckpoint(ValueSpaceCheckpoint);

impl VerifiedCheckpoint {
    pub(super) const fn restore(checkpoint: ValueSpaceCheckpoint) -> Self {
        Self(checkpoint)
    }

    pub(super) const fn derived(identity: IndexIdentity, anchor: MapResumeAnchor) -> Self {
        Self(ValueSpaceCheckpoint::new(identity, anchor, CheckpointProvenance::PublishedCoverage))
    }

    #[cfg(test)]
    pub(super) const fn recognized(
        identity: IndexIdentity,
        anchor: MapResumeAnchor,
        id: u64,
    ) -> Self {
        Self(ValueSpaceCheckpoint::new(identity, anchor, CheckpointProvenance::Recognized { id }))
    }

    /// Returns the identity under which this checkpoint was verified.
    pub const fn identity(&self) -> &IndexIdentity {
        self.0.identity()
    }

    /// Returns the checkpoint record.
    pub const fn checkpoint(&self) -> &ValueSpaceCheckpoint {
        &self.0
    }

    /// Returns the map resume anchor at which construction begins.
    pub const fn anchor(&self) -> MapResumeAnchor {
        self.0.anchor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{coverage::STORAGE_FORMAT_V1, BlockPointer, MapBoundary, ParamsId, GETH_V1};
    use alloy_primitives::B256;

    fn hash(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    fn identity() -> IndexIdentity {
        IndexIdentity::new(STORAGE_FORMAT_V1, 1, hash(0xd4), GETH_V1, ParamsId::Default)
    }

    fn checkpoint() -> ValueSpaceCheckpoint {
        let anchor = MapResumeAnchor::new(
            MapBoundary::new(9, 1000, hash(0x10)),
            BlockPointer::new(1000, hash(0x10), 123_456),
        )
        .unwrap();
        ValueSpaceCheckpoint::new(identity(), anchor, CheckpointProvenance::Recognized { id: 7 })
    }

    #[test]
    fn checkpoint_binds_identity_and_anchor() {
        let checkpoint = checkpoint();
        assert_eq!(checkpoint.identity(), &identity());
        assert_eq!(checkpoint.provenance(), CheckpointProvenance::Recognized { id: 7 });
        assert_eq!(checkpoint.anchor().pointer, BlockPointer::new(1000, hash(0x10), 123_456));
    }
}
