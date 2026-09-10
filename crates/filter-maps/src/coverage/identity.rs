//! Index identity under which published coverage is meaningful.

use crate::{ParamsId, ValueSpaceVersion};
use alloy_primitives::B256;

/// Identity under which persisted rows and coverage are interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IndexIdentity {
    /// Encoding used for persisted rows and metadata.
    pub storage_format_version: StorageFormatVersion,
    /// EIP-155 chain id of the indexed chain.
    pub chain_id: u64,
    /// Hash of the chain's genesis block.
    pub genesis_hash: B256,
    /// Semantic rules that assigned absolute log value indices.
    pub value_space_version: ValueSpaceVersion,
    /// Recognized parameter set that placed values into rows and columns.
    pub params: ParamsId,
}

impl IndexIdentity {
    /// Creates an index identity.
    pub const fn new(
        storage_format_version: StorageFormatVersion,
        chain_id: u64,
        genesis_hash: B256,
        value_space_version: ValueSpaceVersion,
        params: ParamsId,
    ) -> Self {
        Self { storage_format_version, chain_id, genesis_hash, value_space_version, params }
    }

    /// Checks that `stored` coverage is compatible with `self`.
    pub fn check_compatible(&self, stored: &Self) -> Result<(), IdentityMismatch> {
        if self.storage_format_version != stored.storage_format_version {
            return Err(IdentityMismatch::StorageFormat {
                expected: self.storage_format_version,
                stored: stored.storage_format_version,
            })
        }
        if self.chain_id != stored.chain_id {
            return Err(IdentityMismatch::Chain { expected: self.chain_id, stored: stored.chain_id })
        }
        if self.genesis_hash != stored.genesis_hash {
            return Err(IdentityMismatch::Genesis {
                expected: self.genesis_hash,
                stored: stored.genesis_hash,
            })
        }
        if self.value_space_version != stored.value_space_version {
            return Err(IdentityMismatch::ValueSpaceVersion {
                expected: self.value_space_version,
                stored: stored.value_space_version,
            })
        }
        if self.params != stored.params {
            return Err(IdentityMismatch::Params { expected: self.params, stored: stored.params })
        }
        Ok(())
    }
}

/// Storage encoding used for rows and coverage metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StorageFormatVersion(u16);

impl StorageFormatVersion {
    /// Returns the stable persisted encoding.
    pub const fn encoded(self) -> u16 {
        self.0
    }
}

impl TryFrom<u16> for StorageFormatVersion {
    type Error = UnknownStorageFormatVersion;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(STORAGE_FORMAT_V1),
            value => Err(UnknownStorageFormatVersion(value)),
        }
    }
}

/// Unknown or unsupported persisted storage encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unknown storage format version {0}")]
pub struct UnknownStorageFormatVersion(u16);

impl UnknownStorageFormatVersion {
    /// Returns the rejected persisted encoding.
    pub const fn value(self) -> u16 {
        self.0
    }
}

/// Initial `FilterMaps` storage encoding.
pub const STORAGE_FORMAT_V1: StorageFormatVersion = StorageFormatVersion(1);

/// Reason a stored index identity is incompatible with the running implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdentityMismatch {
    /// The index uses a different storage encoding.
    #[error("index storage format {stored:?} does not match the running format {expected:?}")]
    StorageFormat {
        /// Format implemented by the running node.
        expected: StorageFormatVersion,
        /// Format recorded by the index.
        stored: StorageFormatVersion,
    },
    /// The index was built for a different chain.
    #[error("index chain id {stored} does not match the running chain id {expected}")]
    Chain {
        /// Chain id of the running node.
        expected: u64,
        /// Chain id recorded by the index.
        stored: u64,
    },
    /// The index was built from a different genesis block.
    #[error("index genesis {stored} does not match the running genesis {expected}")]
    Genesis {
        /// Genesis hash of the running node.
        expected: B256,
        /// Genesis hash recorded by the index.
        stored: B256,
    },
    /// The index assigned absolute indices under different semantic rules.
    #[error(
        "index value-space version {stored:?} does not match the running version {expected:?}"
    )]
    ValueSpaceVersion {
        /// Version implemented by the running node.
        expected: ValueSpaceVersion,
        /// Version recorded by the index.
        stored: ValueSpaceVersion,
    },
    /// The index placed values under a different recognized parameter set.
    #[error("index parameter set {stored:?} does not match the configured set {expected:?}")]
    Params {
        /// Parameter set configured on the running node.
        expected: ParamsId,
        /// Parameter set recorded by the index.
        stored: ParamsId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GETH_V1;

    const fn identity() -> IndexIdentity {
        IndexIdentity::new(
            STORAGE_FORMAT_V1,
            1,
            B256::repeat_byte(0xd4),
            GETH_V1,
            ParamsId::Default,
        )
    }

    #[test]
    fn identical_identities_are_compatible() {
        assert_eq!(identity().check_compatible(&identity()), Ok(()));
    }

    #[test]
    fn storage_format_version_has_a_stable_rejecting_encoding() {
        assert_eq!(StorageFormatVersion::try_from(1), Ok(STORAGE_FORMAT_V1));
        assert_eq!(StorageFormatVersion::try_from(2).unwrap_err().value(), 2);
    }

    #[test]
    fn any_differing_field_is_incompatible() {
        let running = identity();

        let mut stored = running;
        stored.storage_format_version = StorageFormatVersion(2);
        assert_eq!(
            running.check_compatible(&stored),
            Err(IdentityMismatch::StorageFormat {
                expected: STORAGE_FORMAT_V1,
                stored: StorageFormatVersion(2)
            })
        );

        let mut stored = running;
        stored.chain_id = 10;
        assert_eq!(
            running.check_compatible(&stored),
            Err(IdentityMismatch::Chain { expected: 1, stored: 10 })
        );

        let mut stored = running;
        stored.genesis_hash = B256::ZERO;
        assert_eq!(
            running.check_compatible(&stored),
            Err(IdentityMismatch::Genesis { expected: running.genesis_hash, stored: B256::ZERO })
        );

        let mut stored = running;
        stored.params = ParamsId::RangeTest;
        assert_eq!(
            running.check_compatible(&stored),
            Err(IdentityMismatch::Params {
                expected: ParamsId::Default,
                stored: ParamsId::RangeTest
            })
        );
    }
}
