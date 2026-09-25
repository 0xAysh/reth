//! Typed `FilterMaps` storage failures.

use reth_db_api::{models::FilterMapModelError, DatabaseError};
use reth_filter_maps::coverage::{
    ContractionError, IdentityMismatch, PersistedCoverageError, PublishError,
};

/// Durable `FilterMaps` storage error.
#[derive(Debug, thiserror::Error)]
pub enum FilterMapStorageError {
    /// The store has no initialized identity.
    #[error("FilterMaps identity is missing")]
    MissingIdentity,
    /// Data exists without its authoritative identity.
    #[error("FilterMaps records exist without an identity")]
    IncompleteStore,
    /// Stored identity differs from the running identity.
    #[error(transparent)]
    IdentityMismatch(#[from] IdentityMismatch),
    /// A primitive persisted identity uses an unknown encoding.
    #[error("unknown persisted FilterMaps identity: {0}")]
    UnknownIdentity(&'static str),
    /// A database operation failed.
    #[error("FilterMaps database operation failed: {0}")]
    Database(#[from] DatabaseError),
    /// Coverage metadata or anchor ranges are malformed.
    #[error(transparent)]
    Coverage(#[from] PersistedCoverageError),
    /// The requested coverage transition is invalid.
    #[error(transparent)]
    Publish(#[from] PublishError),
    /// The requested contraction is invalid.
    #[error(transparent)]
    Contraction(#[from] ContractionError),
    /// Physical model construction failed.
    #[error(transparent)]
    Model(#[from] FilterMapModelError),
    /// Publication holds more maps than one batch may stage.
    #[error("FilterMaps publication contains {0} maps; maximum is 32")]
    OversizedPublication(usize),
    /// Publication maps do not share one base-row group.
    #[error("FilterMaps publication maps {first_map}..={last_map} span several base-row groups")]
    MultipleBaseRowGroups {
        /// First published map.
        first_map: u32,
        /// Last published map.
        last_map: u32,
    },
    /// Sparse renderer rows are malformed.
    #[error("FilterMaps rows are malformed at map {map_index}, row {row_index}")]
    MalformedRow {
        /// Map index.
        map_index: u32,
        /// Row index.
        row_index: u32,
    },
    /// Existing protected content disagrees with the publication.
    #[error("FilterMaps publication conflicts with protected {kind} at {key}")]
    ProtectedConflict {
        /// Record kind.
        kind: &'static str,
        /// Numeric key.
        key: u64,
    },
    /// A covered map has no row directory.
    #[error("covered FilterMaps map {0} has no row directory")]
    MissingDirectory(u32),
    /// Required grouped base data is absent.
    #[error("required FilterMaps base row is missing for map {map_index}, row {row_index}")]
    MissingBaseRow {
        /// Map index.
        map_index: u32,
        /// Row index.
        row_index: u32,
    },
    /// Required extension data is absent.
    #[error("required FilterMaps extension is missing for map {map_index}, row {row_index}")]
    MissingExtension {
        /// Map index.
        map_index: u32,
        /// Row index.
        row_index: u32,
    },
    /// A record repeats a key and disagrees with it.
    #[error("FilterMaps key/value mismatch in {kind} at {key}")]
    KeyValueMismatch {
        /// Record kind.
        kind: &'static str,
        /// Numeric key.
        key: u64,
    },
    /// A request lies outside the activated segment.
    #[error("FilterMaps {kind} {key} lies outside the source domain")]
    OutsideSource {
        /// Requested kind.
        kind: &'static str,
        /// Requested numeric key.
        key: u64,
    },
    /// Map requests are not strictly ascending and unique.
    #[error("FilterMaps map requests must be strictly ascending and unique")]
    UnorderedMapRequest,
    /// Row request is outside the recognized map height.
    #[error("FilterMaps row {0} is outside the map")]
    RowOutOfRange(u32),
    /// A required block pointer is absent.
    #[error("required FilterMaps block pointer {0} is missing")]
    MissingPointer(u64),
    /// Row payload cardinality disagrees with its authoritative map directory.
    #[error("FilterMaps payload count disagrees with directory for map {0}")]
    PayloadCountMismatch(u32),
    /// Physical payload exists where the authoritative directory declares an empty row.
    #[error("FilterMaps stale payload contradicts map {map_index}, row {row_index}")]
    ContradictedPayload {
        /// Map index.
        map_index: u32,
        /// Row index.
        row_index: u32,
    },
    /// Existing records only partially match an idempotent retry.
    #[error("FilterMaps store contains incomplete prior publication state")]
    IncompletePriorState,
    /// Checked cleanup or interval arithmetic overflowed.
    #[error("FilterMaps range arithmetic overflow")]
    Arithmetic,
    /// Test-only failure injected between durable publication phases.
    #[cfg(test)]
    #[error("injected FilterMaps publication failure")]
    InjectedPublicationFailure,
}

/// Result returned by durable `FilterMaps` operations.
pub type Result<T> = std::result::Result<T, FilterMapStorageError>;
