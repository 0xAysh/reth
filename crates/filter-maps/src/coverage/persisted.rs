//! Storage-neutral records and the restoration boundary for durable coverage.

use crate::coverage::{
    CheckpointProvenance, CoverageSet, IndexIdentity, MapResumeAnchor, PublishError, RestoreError,
    RetainedAnchor, SegmentOrigin, ValidatedSegment, ValueSpaceCheckpoint, VerifiedCheckpoint,
};
use alloy_primitives::B256;
use std::{collections::BTreeMap, convert::Infallible};

/// Compact durable representation of all coverage under one index identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCoverageRecord {
    /// Identity under which the catalog must be interpreted.
    pub identity: IndexIdentity,
    /// Ordered segment descriptors. Per-map anchors are stored separately.
    pub segments: Vec<StoredSegmentRecord>,
}

/// Compact durable descriptor for one segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSegmentRecord {
    /// Trusted-origin record to reconstruct and verify.
    pub origin: StoredSegmentOrigin,
    /// First completed map owned by this segment.
    pub first_map: u32,
    /// Terminal completed map owned by this segment.
    pub terminal_map: u32,
}

/// Durable representation of a segment origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredSegmentOrigin {
    /// The genesis value-space origin.
    Genesis,
    /// A checkpoint and the provenance of its numerical pointer.
    Checkpoint {
        /// Origin anchor.
        origin_anchor: MapResumeAnchor,
        /// Durable pointer provenance.
        provenance: CheckpointProvenance,
    },
    /// A node-published anchor retained after coverage contraction.
    Retained {
        /// Origin anchor.
        origin_anchor: MapResumeAnchor,
    },
}

/// Authority that re-establishes trust in persisted externally supplied checkpoints.
///
/// Restoration consults it only for [`CheckpointProvenance::Recognized`] and
/// [`CheckpointProvenance::DerivedFrom`] origins, whose numerical pointers came from outside the
/// node's own published coverage. Implementations must bind recognized identifiers to an exact
/// identity and anchor and prove derived checkpoints against a trusted predecessor. Returning
/// `false` fails closed.
///
/// Self-minted origins restore without it: a [`SegmentOrigin::Retained`] origin or a
/// [`CheckpointProvenance::PublishedCoverage`] checkpoint can only be constructed from anchors of
/// coverage that was already validated, and it is persisted in the same transaction as the
/// coverage it replaces. Whoever can forge such a record can equally forge the anchors and rows it
/// describes, so a verifier could add no evidence the store does not already hold. Canonical
/// activation still checks every origin hash against the current chain.
pub trait StoredOriginVerifier {
    /// Verifies the exact checkpoint record, including its numerical pointer.
    fn verify_checkpoint(
        &mut self,
        identity: &IndexIdentity,
        anchor: MapResumeAnchor,
        provenance: CheckpointProvenance,
    ) -> bool;
}

/// Verifier used when no checkpoint registry is present: every external checkpoint is rejected.
#[derive(Debug, Default, Clone, Copy)]
pub struct RejectUntrustedOrigins;

impl StoredOriginVerifier for RejectUntrustedOrigins {
    fn verify_checkpoint(
        &mut self,
        _identity: &IndexIdentity,
        _anchor: MapResumeAnchor,
        _provenance: CheckpointProvenance,
    ) -> bool {
        false
    }
}

/// Coverage whose persisted structure and origins are valid but not yet current-chain verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructurallyRestoredCoverage(CoverageSet);

impl StructurallyRestoredCoverage {
    /// Restores a catalog using ordered map anchors and an explicit origin trust resolver.
    pub fn restore(
        running: &IndexIdentity,
        record: StoredCoverageRecord,
        anchors: impl IntoIterator<Item = MapResumeAnchor>,
        verifier: &mut impl StoredOriginVerifier,
    ) -> Result<Self, PersistedCoverageError> {
        running.check_compatible(&record.identity)?;
        let mut anchors_by_map = BTreeMap::new();
        for anchor in anchors {
            if anchors_by_map.insert(anchor.completed_map_index, anchor).is_some() {
                return Err(PersistedCoverageError::DuplicateAnchor {
                    map_index: anchor.completed_map_index,
                })
            }
        }
        let mut anchors = anchors_by_map;
        let mut coverage = CoverageSet::new(record.identity);
        let mut previous_terminal = None;

        for descriptor in record.segments {
            if descriptor.first_map > descriptor.terminal_map {
                return Err(PersistedCoverageError::InvalidMapRange {
                    first_map: descriptor.first_map,
                    terminal_map: descriptor.terminal_map,
                })
            }
            if previous_terminal.is_some_and(|terminal| descriptor.first_map <= terminal) {
                return Err(PersistedCoverageError::UnorderedSegments {
                    previous_terminal: previous_terminal.expect("checked as some"),
                    first_map: descriptor.first_map,
                })
            }

            let origin = descriptor.origin.restore(record.identity, verifier)?;
            let expected_first = origin.anchor().map_or(Ok(0), |anchor| {
                anchor
                    .completed_map_index
                    .checked_add(1)
                    .ok_or(PersistedCoverageError::MapIndexOverflow)
            })?;
            if expected_first != descriptor.first_map {
                return Err(PersistedCoverageError::OriginMapMismatch {
                    expected: expected_first,
                    actual: descriptor.first_map,
                })
            }

            let mut segment_anchors = Vec::new();
            for map_index in descriptor.first_map..=descriptor.terminal_map {
                segment_anchors.push(
                    anchors
                        .remove(&map_index)
                        .ok_or(PersistedCoverageError::MissingAnchor { map_index })?,
                );
            }
            let segment = ValidatedSegment::new_batch(&record.identity, origin, segment_anchors)?;
            coverage.insert_restored(segment)?;
            previous_terminal = Some(descriptor.terminal_map);
        }

        if let Some((&map_index, _)) = anchors.first_key_value() {
            return Err(PersistedCoverageError::UnexpectedAnchor { map_index })
        }
        Ok(Self(coverage))
    }

    /// Returns the compatible identity of the restored catalog.
    pub const fn identity(&self) -> &IndexIdentity {
        self.0.identity()
    }

    /// Returns the structurally valid segments for integrity validation.
    pub fn segments(&self) -> &[ValidatedSegment] {
        self.0.segments()
    }

    /// Verifies segment canonical evidence and retains only currently canonical segments.
    ///
    /// A missing or mismatched canonical hash disables that segment. Lookup failures abort
    /// activation because they do not prove either canonicality or non-canonicality.
    pub fn activate<E>(
        &self,
        mut canonical_hash: impl FnMut(u64) -> Result<Option<B256>, E>,
    ) -> Result<QueryableCoverage, CanonicalActivationError<E>> {
        let mut queryable = CoverageSet::new(*self.identity());
        for segment in self.segments() {
            let mut canonical = true;
            if let Some(origin) = segment.origin().anchor() {
                canonical &= canonical_hash(origin.pointer.block_number)
                    .map_err(CanonicalActivationError::Lookup)? ==
                    Some(origin.pointer.block_hash);
                if let SegmentOrigin::Checkpoint(checkpoint) = segment.origin() &&
                    let CheckpointProvenance::DerivedFrom { predecessor } =
                        checkpoint.checkpoint().provenance()
                {
                    canonical &= canonical_hash(predecessor.pointer.block_number)
                        .map_err(CanonicalActivationError::Lookup)? ==
                        Some(predecessor.pointer.block_hash);
                }
            } else {
                canonical &= canonical_hash(0).map_err(CanonicalActivationError::Lookup)? ==
                    Some(self.identity().genesis_hash);
            }
            for anchor in segment.anchors() {
                canonical &= canonical_hash(anchor.pointer.block_number)
                    .map_err(CanonicalActivationError::Lookup)? ==
                    Some(anchor.pointer.block_hash);
            }
            if canonical {
                queryable
                    .insert_restored(segment.clone())
                    .map_err(CanonicalActivationError::Corrupt)?;
            }
        }
        Ok(QueryableCoverage(queryable))
    }

    /// Applies an open-segment proposal while retaining the structurally-restored type state.
    pub fn open_segment_batch(
        &mut self,
        origin: SegmentOrigin,
        anchors: impl IntoIterator<Item = MapResumeAnchor>,
    ) -> Result<(), PublishError> {
        self.0.open_segment_batch(origin, anchors)
    }

    /// Applies an extension proposal while retaining the structurally-restored type state.
    pub fn extend_batch(
        &mut self,
        from: MapResumeAnchor,
        anchors: impl IntoIterator<Item = MapResumeAnchor>,
    ) -> Result<(), PublishError> {
        self.0.extend_batch(from, anchors)
    }

    /// Contracts structurally-restored coverage for a reorg without granting queryability.
    pub fn contract_for_reorg(
        &mut self,
        earliest_changed: u64,
        safe_anchor: Option<MapResumeAnchor>,
    ) -> Result<crate::coverage::ReorgContraction, crate::coverage::ContractionError> {
        self.0.contract_for_reorg(earliest_changed, safe_anchor)
    }

    /// Applies retention contraction without granting queryability.
    pub fn retain_after(
        &mut self,
        tail: MapResumeAnchor,
    ) -> Result<(), crate::coverage::ContractionError> {
        self.0.retain_after(tail)
    }

    /// Produces the compact catalog after a validated structural transition.
    pub fn stored_record(&self) -> StoredCoverageRecord {
        self.0.stored_record()
    }
}

/// Coverage that passed current canonical-hash verification and may back matcher reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryableCoverage(CoverageSet);

impl QueryableCoverage {
    /// Returns current queryable coverage.
    pub const fn coverage(&self) -> &CoverageSet {
        &self.0
    }

    /// Consumes the wrapper after canonical verification.
    pub fn into_coverage(self) -> CoverageSet {
        self.0
    }
}

impl CoverageSet {
    /// Produces the compact storage-neutral catalog. Per-map anchors remain separate records.
    pub fn stored_record(&self) -> StoredCoverageRecord {
        let segments = self
            .segments()
            .iter()
            .map(|segment| StoredSegmentRecord {
                origin: StoredSegmentOrigin::from_origin(segment.origin()),
                first_map: segment.first_map(),
                terminal_map: segment.terminal().completed_map_index,
            })
            .collect();
        StoredCoverageRecord { identity: *self.identity(), segments }
    }

    pub(super) fn insert_restored(
        &mut self,
        segment: ValidatedSegment,
    ) -> Result<(), PublishError> {
        self.insert(segment)
    }
}

impl StoredSegmentOrigin {
    const fn from_origin(origin: &SegmentOrigin) -> Self {
        match origin {
            SegmentOrigin::Genesis => Self::Genesis,
            SegmentOrigin::Checkpoint(checkpoint) => Self::Checkpoint {
                origin_anchor: checkpoint.anchor(),
                provenance: checkpoint.checkpoint().provenance(),
            },
            SegmentOrigin::Retained(anchor) => Self::Retained { origin_anchor: anchor.anchor() },
        }
    }

    fn restore(
        self,
        identity: IndexIdentity,
        verifier: &mut impl StoredOriginVerifier,
    ) -> Result<SegmentOrigin, PersistedCoverageError> {
        let anchor = match &self {
            Self::Genesis => return Ok(SegmentOrigin::Genesis),
            Self::Checkpoint { origin_anchor, .. } | Self::Retained { origin_anchor } => {
                *origin_anchor
            }
        };
        if anchor.value_space_version != identity.value_space_version {
            return Err(PersistedCoverageError::InvalidOrigin)
        }
        Ok(match self {
            Self::Genesis => unreachable!("returned above"),
            Self::Checkpoint { origin_anchor, provenance } => {
                if let CheckpointProvenance::DerivedFrom { predecessor } = provenance &&
                    (predecessor.value_space_version != identity.value_space_version ||
                        predecessor.completed_map_index >= origin_anchor.completed_map_index ||
                        predecessor.pointer.block_number > origin_anchor.pointer.block_number ||
                        predecessor.pointer.first_log_value_index >
                            origin_anchor.pointer.first_log_value_index ||
                        (predecessor.pointer.block_number ==
                            origin_anchor.pointer.block_number &&
                            predecessor.pointer.block_hash !=
                                origin_anchor.pointer.block_hash))
                {
                    return Err(PersistedCoverageError::InvalidOrigin)
                }
                if provenance != CheckpointProvenance::PublishedCoverage &&
                    !verifier.verify_checkpoint(&identity, origin_anchor, provenance)
                {
                    return Err(PersistedCoverageError::UnverifiedOrigin)
                }
                SegmentOrigin::Checkpoint(VerifiedCheckpoint::restore(ValueSpaceCheckpoint::new(
                    identity,
                    origin_anchor,
                    provenance,
                )))
            }
            Self::Retained { origin_anchor } => {
                SegmentOrigin::Retained(RetainedAnchor::restore(origin_anchor))
            }
        })
    }
}

/// Reason persisted descriptors and their independently stored anchors could not be restored.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PersistedCoverageError {
    /// The catalog identity is incompatible with the running identity.
    #[error(transparent)]
    Identity(#[from] crate::coverage::IdentityMismatch),
    /// A segment descriptor contains an empty or reversed map interval.
    #[error("invalid stored map interval {first_map}..={terminal_map}")]
    InvalidMapRange {
        /// First map.
        first_map: u32,
        /// Terminal map.
        terminal_map: u32,
    },
    /// Segment descriptors are not strictly ordered and disjoint.
    #[error("segment starting at map {first_map} follows terminal map {previous_terminal}")]
    UnorderedSegments {
        /// Previous terminal map.
        previous_terminal: u32,
        /// Next first map.
        first_map: u32,
    },
    /// An origin implies a different first map.
    #[error("segment origin requires first map {expected}, descriptor stores {actual}")]
    OriginMapMismatch {
        /// Derived first map.
        expected: u32,
        /// Stored first map.
        actual: u32,
    },
    /// An inline origin or its provenance is internally inconsistent.
    #[error("invalid stored segment origin")]
    InvalidOrigin,
    /// Persisted origin trust could not be re-established by the supplied verifier.
    #[error("stored segment origin is not trusted")]
    UnverifiedOrigin,
    /// A covered map has no durable resume anchor.
    #[error("missing durable anchor for map {map_index}")]
    MissingAnchor {
        /// Missing map.
        map_index: u32,
    },
    /// More than one anchor record names the same map.
    #[error("duplicate durable anchor for map {map_index}")]
    DuplicateAnchor {
        /// Duplicated map.
        map_index: u32,
    },
    /// An anchor is not named by any segment descriptor.
    #[error("unexpected durable anchor for uncovered map {map_index}")]
    UnexpectedAnchor {
        /// Unexpected map.
        map_index: u32,
    },
    /// A restored segment failed its logical invariants.
    #[error(transparent)]
    Segment(#[from] crate::coverage::SegmentError),
    /// Ordered descriptors nevertheless overlap in block or map space.
    #[error(transparent)]
    Coverage(#[from] PublishError),
    /// An origin map index cannot be advanced.
    #[error("filter map index overflow")]
    MapIndexOverflow,
}

/// Failure while checking restored canonical identity evidence.
#[derive(Debug, thiserror::Error)]
pub enum CanonicalActivationError<E> {
    /// Canonical hash lookup failed.
    #[error("canonical hash lookup failed")]
    Lookup(#[source] E),
    /// Structurally valid segments became inconsistent while being selected.
    #[error("restored coverage became inconsistent: {0}")]
    Corrupt(PublishError),
}

impl StructurallyRestoredCoverage {
    /// Convenience activation for infallible canonical-hash lookup callbacks.
    pub fn activate_infallible(
        &self,
        mut canonical_hash: impl FnMut(u64) -> Option<B256>,
    ) -> QueryableCoverage {
        match self.activate(|number| Ok::<_, Infallible>(canonical_hash(number))) {
            Ok(coverage) => coverage,
            Err(error) => match error {
                CanonicalActivationError::Lookup(never) => match never {},
                CanonicalActivationError::Corrupt(corrupt) => {
                    unreachable!("restored coverage selection is valid: {corrupt}")
                }
            },
        }
    }
}

impl From<RestoreError> for PersistedCoverageError {
    fn from(error: RestoreError) -> Self {
        match error {
            RestoreError::Identity(error) => Self::Identity(error),
            RestoreError::Corrupt(error) => Self::Coverage(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coverage::test_utils::*;

    #[derive(Default)]
    struct TestOriginVerifier;

    impl StoredOriginVerifier for TestOriginVerifier {
        fn verify_checkpoint(
            &mut self,
            identity: &IndexIdentity,
            anchor: MapResumeAnchor,
            _provenance: CheckpointProvenance,
        ) -> bool {
            *identity == crate::coverage::test_utils::identity() &&
                anchor.value_space_version == identity.value_space_version
        }
    }

    #[test]
    fn stored_catalog_restores_without_granting_queryability() {
        let mut coverage = CoverageSet::new(identity());
        let anchors = anchors_through(0, aligned(2, 30));
        coverage.open_segment_batch(SegmentOrigin::Genesis, anchors.clone()).unwrap();

        let restored = StructurallyRestoredCoverage::restore(
            &identity(),
            coverage.stored_record(),
            anchors,
            &mut TestOriginVerifier,
        )
        .unwrap();
        assert_eq!(restored.segments().len(), 1);
        let queryable = restored.activate_infallible(|number| {
            Some(if number == 0 { identity().genesis_hash } else { hash(number) })
        });
        assert!(queryable.coverage().covers(29));
    }

    #[test]
    fn canonical_mismatch_disables_only_affected_segment() {
        let mut coverage = CoverageSet::new(identity());
        let first = anchors_through(0, aligned(1, 20));
        let second_origin = aligned(9, 100);
        let second = anchors_through(10, aligned(11, 120));
        coverage.open_segment_batch(SegmentOrigin::Genesis, first.clone()).unwrap();
        coverage.open_segment_batch(checkpoint(second_origin), second.clone()).unwrap();
        let all_anchors = first.into_iter().chain(second).collect::<Vec<_>>();
        let restored = StructurallyRestoredCoverage::restore(
            &identity(),
            coverage.stored_record(),
            all_anchors,
            &mut TestOriginVerifier,
        )
        .unwrap();

        let queryable = restored.activate_infallible(|number| {
            (number < 100).then(|| if number == 0 { identity().genesis_hash } else { hash(number) })
        });
        assert_eq!(queryable.coverage().segments().len(), 1);
        assert!(queryable.coverage().covers(19));
        assert!(!queryable.coverage().covers(100));
    }

    #[test]
    fn derived_checkpoint_activation_verifies_its_predecessor() {
        let origin = aligned(9, 100);
        let predecessor = aligned(5, 60);
        let anchors = anchors_through(10, aligned(12, 130));
        let record = StoredCoverageRecord {
            identity: identity(),
            segments: vec![StoredSegmentRecord {
                origin: StoredSegmentOrigin::Checkpoint {
                    origin_anchor: origin,
                    provenance: CheckpointProvenance::DerivedFrom { predecessor },
                },
                first_map: 10,
                terminal_map: 12,
            }],
        };
        let restored = StructurallyRestoredCoverage::restore(
            &identity(),
            record,
            anchors,
            &mut TestOriginVerifier,
        )
        .unwrap();
        let queryable = restored.activate_infallible(|number| {
            Some(if number == 60 { B256::ZERO } else { hash(number) })
        });
        assert!(queryable.coverage().segments().is_empty());
    }

    struct ExactCheckpointVerifier {
        id: u64,
        anchor: MapResumeAnchor,
    }

    impl StoredOriginVerifier for ExactCheckpointVerifier {
        fn verify_checkpoint(
            &mut self,
            identity: &IndexIdentity,
            anchor: MapResumeAnchor,
            provenance: CheckpointProvenance,
        ) -> bool {
            identity == &crate::coverage::test_utils::identity() &&
                provenance == CheckpointProvenance::Recognized { id: self.id } &&
                anchor == self.anchor
        }
    }

    #[test]
    fn persisted_checkpoint_requires_explicit_anchor_verification() {
        let origin = aligned(9, 100);
        let terminal = aligned(10, 110);
        let record = StoredCoverageRecord {
            identity: identity(),
            segments: vec![StoredSegmentRecord {
                origin: StoredSegmentOrigin::Checkpoint {
                    origin_anchor: origin,
                    provenance: CheckpointProvenance::Recognized { id: 7 },
                },
                first_map: 10,
                terminal_map: 10,
            }],
        };
        assert!(matches!(
            StructurallyRestoredCoverage::restore(
                &identity(),
                record.clone(),
                [terminal],
                &mut RejectUntrustedOrigins,
            ),
            Err(PersistedCoverageError::UnverifiedOrigin)
        ));
        assert!(StructurallyRestoredCoverage::restore(
            &identity(),
            record.clone(),
            [terminal],
            &mut ExactCheckpointVerifier { id: 7, anchor: origin },
        )
        .is_ok());
        assert!(matches!(
            StructurallyRestoredCoverage::restore(
                &identity(),
                record,
                [terminal],
                &mut ExactCheckpointVerifier { id: 8, anchor: origin },
            ),
            Err(PersistedCoverageError::UnverifiedOrigin)
        ));
    }

    #[test]
    fn self_minted_origins_restore_without_a_checkpoint_verifier() {
        let mut coverage = CoverageSet::new(identity());
        let anchors = anchors_through(0, aligned(4, 50));
        coverage.open_segment_batch(SegmentOrigin::Genesis, anchors.clone()).unwrap();
        let checkpoint = coverage.derived_checkpoint(anchors[1]).unwrap();
        coverage.retain_after(anchors[1]).unwrap();
        let retained = coverage.clone();
        assert!(matches!(retained.segments()[0].origin(), SegmentOrigin::Retained(_)));
        let restored = StructurallyRestoredCoverage::restore(
            &identity(),
            retained.stored_record(),
            anchors[2..].iter().copied(),
            &mut RejectUntrustedOrigins,
        )
        .unwrap();
        assert_eq!(restored.segments(), retained.segments());

        let mut published = CoverageSet::new(identity());
        published
            .open_segment_batch(SegmentOrigin::Checkpoint(checkpoint), anchors[2..].to_vec())
            .unwrap();
        let restored = StructurallyRestoredCoverage::restore(
            &identity(),
            published.stored_record(),
            anchors[2..].iter().copied(),
            &mut RejectUntrustedOrigins,
        )
        .unwrap();
        assert_eq!(restored.segments(), published.segments());
    }

    #[test]
    fn derived_checkpoint_requires_explicit_verification() {
        let record = StoredCoverageRecord {
            identity: identity(),
            segments: vec![StoredSegmentRecord {
                origin: StoredSegmentOrigin::Checkpoint {
                    origin_anchor: aligned(9, 100),
                    provenance: CheckpointProvenance::DerivedFrom { predecessor: aligned(5, 60) },
                },
                first_map: 10,
                terminal_map: 10,
            }],
        };
        assert!(matches!(
            StructurallyRestoredCoverage::restore(
                &identity(),
                record,
                [aligned(10, 110)],
                &mut RejectUntrustedOrigins,
            ),
            Err(PersistedCoverageError::UnverifiedOrigin)
        ));
    }

    #[test]
    fn restoration_rejects_missing_and_unexpected_anchors() {
        let record = StoredCoverageRecord {
            identity: identity(),
            segments: vec![StoredSegmentRecord {
                origin: StoredSegmentOrigin::Genesis,
                first_map: 0,
                terminal_map: 1,
            }],
        };
        assert!(matches!(
            StructurallyRestoredCoverage::restore(
                &identity(),
                record,
                [aligned(0, 10)],
                &mut TestOriginVerifier,
            ),
            Err(PersistedCoverageError::MissingAnchor { map_index: 1 })
        ));
        let empty = StoredCoverageRecord { identity: identity(), segments: Vec::new() };
        assert!(matches!(
            StructurallyRestoredCoverage::restore(
                &identity(),
                empty.clone(),
                [aligned(0, 10), aligned(0, 10)],
                &mut TestOriginVerifier,
            ),
            Err(PersistedCoverageError::DuplicateAnchor { map_index: 0 })
        ));
        assert!(matches!(
            StructurallyRestoredCoverage::restore(
                &identity(),
                empty,
                [aligned(0, 10)],
                &mut TestOriginVerifier,
            ),
            Err(PersistedCoverageError::UnexpectedAnchor { map_index: 0 })
        ));
    }
}
