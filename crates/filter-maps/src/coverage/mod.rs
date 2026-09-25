//! Storage-independent indexed-coverage rules.
//!
//! Covered ranges are complete and canonical, and `FilterMaps` candidates contain every exact
//! match. Uncovered or uncertain ranges fall back to the existing bloom path.
//!
//! This module models logical validity but does not persist rows. `reth-filter-maps-storage`
//! applies coverage expansion in the same transaction as supporting rows, pointers, anchors,
//! identity references, and integrity metadata; contraction commits the visibility change before
//! later physical cleanup.

mod anchor;
mod eligibility;
mod identity;
mod persisted;
mod plan;
mod segment;
mod set;
#[cfg(test)]
mod test_utils;

pub use anchor::{
    CheckpointProvenance, MapResumeAnchor, ResumeAnchorMismatch, ValueSpaceCheckpoint,
    VerifiedCheckpoint,
};
pub use eligibility::{BlockReceiptEvidence, IneligibilityReason, IneligibleBlock};
pub use identity::{
    IdentityMismatch, IndexIdentity, StorageFormatVersion, UnknownStorageFormatVersion,
    STORAGE_FORMAT_V1,
};
pub use persisted::{
    CanonicalActivationError, PersistedCoverageError, QueryableCoverage, RejectUntrustedOrigins,
    StoredCoverageRecord, StoredOriginVerifier, StoredSegmentOrigin, StoredSegmentRecord,
    StructurallyRestoredCoverage,
};
pub use plan::{
    CandidateSource, CanonicalityChanged, LogQueryTarget, PlanError, PlannedSubrange, QueryPlan,
};
pub use segment::{RetainedAnchor, SegmentError, SegmentOrigin, ValidatedSegment};
pub use set::{ContractionError, CoverageSet, PublishError, ReorgContraction, RestoreError};
