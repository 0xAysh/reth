//! Storage-independent indexed-coverage rules.
//!
//! Covered ranges are complete and canonical, and `FilterMaps` candidates contain every exact
//! match. Uncovered or uncertain ranges fall back to the existing bloom path.

mod anchor;
mod eligibility;
mod identity;
mod plan;
mod segment;
mod set;
#[cfg(test)]
mod test_utils;

pub use anchor::{MapResumeAnchor, ResumeAnchorMismatch, ValueSpaceCheckpoint, VerifiedCheckpoint};
pub use eligibility::{BlockReceiptEvidence, IneligibilityReason, IneligibleBlock};
pub use identity::{IdentityMismatch, IndexIdentity};
pub use plan::{
    CandidateSource, CanonicalityChanged, LogQueryTarget, PlanError, PlannedSubrange, QueryPlan,
};
pub use segment::{RetainedAnchor, SegmentError, SegmentOrigin, ValidatedSegment};
pub use set::{
    ContractionError, CoverageSet, CoveredRange, PublishError, ReorgContraction, RestoreError,
};
