//! Storage-independent indexed-coverage rules.
//!
//! Covered ranges are complete and canonical, and `FilterMaps` candidates contain every exact
//! match. Uncovered or uncertain ranges fall back to the existing bloom path.
//!
//! This module models logical validity but does not persist rows. `reth-filter-maps-storage`
//! applies coverage expansion in the same transaction as supporting rows, pointers, anchors,
//! identity references, and integrity metadata; contraction commits the visibility change before
//! later physical cleanup.

mod checkpoint;
mod eligibility;
mod identity;
mod persisted;
mod plan;
mod publication;
mod segment;
mod set;
#[cfg(test)]
mod test_utils;

pub use checkpoint::{CheckpointProvenance, ValueSpaceCheckpoint, VerifiedCheckpoint};
pub use eligibility::{BlockReceiptEvidence, IneligibilityReason, IneligibleBlock};
pub use identity::{
    IdentityMismatch, IndexIdentity, StorageFormatVersion, UnknownStorageFormatVersion,
    STORAGE_FORMAT_V1,
};
pub use persisted::{
    CanonicalActivationError, CheckpointVerifier, PersistedCoverageError, QueryableCoverage,
    RejectUnrecognizedCheckpoints, StoredCoverageRecord, StoredSegmentOrigin, StoredSegmentRecord,
    StructurallyRestoredCoverage,
};
pub use plan::{
    CandidateSource, CanonicalityChanged, LogQueryTarget, PlanError, PlannedSubrange, QueryPlan,
};
pub use publication::{PointerEvidence, PublicationProposal, PublicationStart};
pub use segment::{RetainedAnchor, SegmentError, SegmentOrigin, ValidatedSegment};
pub(crate) use set::CoverageSet;
pub use set::{ContractionError, PublishError, ReorgContraction};
